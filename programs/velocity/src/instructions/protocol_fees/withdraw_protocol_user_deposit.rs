//! Withdraws settled crank rewards from the protocol-owned `User` to the
//! associated token account of the protocol fee recipient.
//!
//! The CLOB cranks accrue the maker's flat removal reward to a `User` whose
//! authority is the velocity signer PDA. Any other crank paid this way does
//! the same. Nobody can sign for that authority, so the normal withdraw path
//! is unreachable and this hot-role instruction is the only exit. The hot role
//! first settles the accrued perp quote into deposits, which `settle_pnl`
//! allows permissionlessly. It then withdraws here and swaps the quote to SOL
//! off-chain to refill the crank reservoirs. That loop makes makers fund crank
//! gas instead of the protocol.
//!
//! This is narrower than a user withdrawal. The existing deposit caps the
//! amount, so the withdrawal can never open a borrow and needs no margin
//! machinery. The protocol `User` only holds reward quote, never base
//! exposure.

use {
    crate::{
        auth::check_hot,
        controller,
        error::ErrorCode,
        instructions::constraints::is_protocol_user,
        load_mut,
        math::{
            casting::Cast, safe_math::SafeMath, spot_withdraw::validate_spot_market_vault_amount,
        },
        state::{
            events::ProtocolUserWithdrawRecordV0,
            spot_market::{SpotBalanceType, SpotMarket},
            state::{HotRole, State},
            user::User,
        },
        validate,
    },
    anchor_lang::prelude::*,
    anchor_spl::{
        associated_token::AssociatedToken,
        token_interface::{Mint, TokenAccount, TokenInterface},
    },
};

#[derive(Accounts)]
#[instruction(args: WithdrawProtocolUserDepositArgs)]
pub struct WithdrawProtocolUserDeposit<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(constraint = check_hot(&authority.key(), &state, HotRole::FeeWithdraw)?)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = is_protocol_user(&protocol_user, &state)?
    )]
    pub protocol_user: AccountLoader<'info, User>,
    #[account(
        mut,
        seeds = [b"spot_market", args.market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), args.market_index.to_le_bytes().as_ref()],
        has_one = mint,
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub mint: InterfaceAccount<'info, Mint>,
    /// CHECK: locked to the cold-admin-set treasury; only used as the ATA wallet
    #[account(
        constraint = recipient.key() != Pubkey::default(),
        address = state.load()?.protocol_fee_recipient_perp @ ErrorCode::InvalidProtocolFeeRecipient
    )]
    pub recipient: UncheckedAccount<'info>,
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = mint,
        associated_token::authority = recipient,
        associated_token::token_program = token_program,
    )]
    pub recipient_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
    #[account(
        address = state.load()?.signer
    )]
    /// CHECK: forced velocity_signer
    pub velocity_signer: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct WithdrawProtocolUserDepositArgs {
    pub market_index: u16,
    pub amount: u64,
}

pub fn handle_withdraw_protocol_user_deposit<'c: 'info, 'info>(
    ctx: Context<'info, WithdrawProtocolUserDeposit<'info>>,
    args: WithdrawProtocolUserDepositArgs,
) -> Result<()> {
    let WithdrawProtocolUserDepositArgs {
        market_index,
        amount,
    } = args;
    let state = ctx.accounts.state.load()?;
    let now = Clock::get()?.unix_timestamp;
    let user = &mut load_mut!(ctx.accounts.protocol_user)?;
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mint = Some(ctx.accounts.mint.clone());

    controller::spot_balance::update_spot_market_cumulative_interest(
        spot_market,
        None,
        now,
        state.funding_paused()?,
    )?;

    // The existing deposit position is the ceiling, so this withdrawal can
    // never make the protocol `User` a borrower.
    let position_index = user.get_spot_position_index(market_index)?;
    validate!(
        user.spot_positions[position_index].balance_type == SpotBalanceType::Deposit,
        ErrorCode::DefaultError,
        "protocol user holds no deposit in spot market {}",
        market_index
    )?;
    let available = user.spot_positions[position_index].get_token_amount(spot_market)?;
    let withdraw_amount = amount.min(available.cast()?);
    validate!(
        withdraw_amount > 0,
        ErrorCode::InsufficientDeposit,
        "no protocol user deposit available (requested={}, available={})",
        amount,
        available
    )?;

    controller::spot_position::update_spot_balances_and_cumulative_deposits_with_limits(
        withdraw_amount.cast()?,
        &SpotBalanceType::Borrow,
        spot_market,
        user,
    )?;

    // the vault must still fully cover depositors after the withdrawal
    let vault_after = ctx
        .accounts
        .spot_market_vault
        .amount
        .safe_sub(withdraw_amount)?;
    validate_spot_market_vault_amount(spot_market, vault_after)?;

    controller::token::send_from_program_vault(
        &ctx.accounts.token_program,
        &ctx.accounts.spot_market_vault,
        &ctx.accounts.recipient_token_account,
        &ctx.accounts.velocity_signer,
        state.signer_nonce,
        withdraw_amount,
        &mint,
        if spot_market.has_transfer_hook() {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;

    emit!(ProtocolUserWithdrawRecordV0 {
        ts: now,
        spot_market_index: market_index,
        amount: withdraw_amount,
        protocol_user: ctx.accounts.protocol_user.key(),
        recipient_token_account: ctx.accounts.recipient_token_account.key(),
    });

    Ok(())
}
