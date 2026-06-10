//! Direct protocol-fee withdrawal.
//!
//! Protocol fees accrue as an excess `protocol_fee_pool` claim on each market
//! (perp fees are quote/USDC-denominated and drawn from the quote spot vault;
//! spot/lending fees are drawn from the market's own vault). These ixs let the
//! `FeeWithdraw` hot key move those fees directly to `State.protocol_fee_recipient`
//! WITHOUT touching the insurance fund or depositor backing: the withdrawal is
//! capped to the pool balance and re-validates `vault >= depositors_claim`, and
//! the recipient is hard-locked to the cold-admin-set treasury.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{TokenAccount, TokenInterface};

use crate::auth::check_hot;
use crate::error::ErrorCode;
use crate::load_mut;
use crate::math::casting::Cast;
use crate::math::safe_math::SafeMath;
use crate::math::spot_balance::get_token_amount;
use crate::math::spot_withdraw::validate_spot_market_vault_amount;
use crate::optional_accounts::get_token_mint;
use crate::state::events::ProtocolFeeWithdrawRecord;
use crate::state::perp_market::PerpMarket;
use crate::state::spot_market::{SpotBalanceType, SpotMarket};
use crate::state::state::{HotRole, State};
use crate::validate;
use crate::{controller, msg};

/// Withdraw a spot market's accrued protocol fees (lending + spot-liquidation
/// carveouts) from its own vault to the protocol fee recipient.
pub fn handle_withdraw_protocol_fees_spot<'c: 'info, 'info>(
    ctx: Context<'info, WithdrawProtocolFeesSpot<'info>>,
    _market_index: u16,
    amount: u64,
) -> Result<()> {
    let state = ctx.accounts.state.load()?;
    let now = Clock::get()?.unix_timestamp;
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mint = get_token_mint(remaining_accounts_iter)?;

    controller::spot_balance::update_spot_market_cumulative_interest(spot_market, None, now)?;

    let available = get_token_amount(
        spot_market.protocol_fee_pool.scaled_balance,
        spot_market,
        &SpotBalanceType::Deposit,
    )?;
    let withdraw_amount = amount.min(available.cast()?);
    validate!(
        withdraw_amount > 0,
        ErrorCode::InsufficientProtocolFees,
        "no protocol fees available (requested={}, available={})",
        amount,
        available
    )?;

    // decrement the protocol-fee claim first (tokens are leaving the protocol)
    controller::spot_balance::update_protocol_fee_pool_balances(
        withdraw_amount.cast()?,
        &SpotBalanceType::Borrow,
        spot_market,
        true,
    )?;

    // the vault must still fully cover depositors after removing the fee — so a
    // protocol-fee withdrawal can never eat into depositor backing
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
        &ctx.accounts.drift_signer,
        state.signer_nonce,
        withdraw_amount,
        &mint,
        if spot_market.has_transfer_hook() {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;

    emit!(ProtocolFeeWithdrawRecord {
        ts: now,
        market_index: spot_market.market_index,
        is_perp: false,
        spot_market_index: spot_market.market_index,
        amount: withdraw_amount,
        recipient_token_account: ctx.accounts.recipient_token_account.key(),
    });

    Ok(())
}

/// Withdraw a perp market's accrued protocol fees (quote/USDC-denominated) from
/// the quote spot market's vault to the protocol fee recipient.
pub fn handle_withdraw_protocol_fees_perp<'c: 'info, 'info>(
    ctx: Context<'info, WithdrawProtocolFeesPerp<'info>>,
    _market_index: u16,
    amount: u64,
) -> Result<()> {
    let state = ctx.accounts.state.load()?;
    let now = Clock::get()?.unix_timestamp;
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    let spot_market = &mut load_mut!(ctx.accounts.quote_spot_market)?;
    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mint = get_token_mint(remaining_accounts_iter)?;

    validate!(
        perp_market.quote_spot_market_index == spot_market.market_index
            && perp_market.protocol_fee_pool.market_index == spot_market.market_index,
        ErrorCode::DefaultError,
        "quote spot market mismatch: perp.quote={} pool.market={} spot={}",
        perp_market.quote_spot_market_index,
        perp_market.protocol_fee_pool.market_index,
        spot_market.market_index
    )?;

    controller::spot_balance::update_spot_market_cumulative_interest(spot_market, None, now)?;

    let available = get_token_amount(
        perp_market.protocol_fee_pool.scaled_balance,
        spot_market,
        &SpotBalanceType::Deposit,
    )?;
    let withdraw_amount = amount.min(available.cast()?);
    validate!(
        withdraw_amount > 0,
        ErrorCode::InsufficientProtocolFees,
        "no protocol fees available (requested={}, available={})",
        amount,
        available
    )?;

    // decrement the perp market's quote-denominated claim against the quote
    // spot market (tokens are leaving the protocol)
    controller::spot_balance::update_spot_balances(
        withdraw_amount.cast()?,
        &SpotBalanceType::Borrow,
        spot_market,
        &mut perp_market.protocol_fee_pool,
        true,
    )?;

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
        &ctx.accounts.drift_signer,
        state.signer_nonce,
        withdraw_amount,
        &mint,
        if spot_market.has_transfer_hook() {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;

    emit!(ProtocolFeeWithdrawRecord {
        ts: now,
        market_index: perp_market.market_index,
        is_perp: true,
        spot_market_index: spot_market.market_index,
        amount: withdraw_amount,
        recipient_token_account: ctx.accounts.recipient_token_account.key(),
    });

    Ok(())
}

#[derive(Accounts)]
#[instruction(market_index: u16)]
pub struct WithdrawProtocolFeesSpot<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(constraint = check_hot(&authority.key(), &state, HotRole::FeeWithdraw)?)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"spot_market", market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        token::mint = spot_market_vault.mint,
        constraint = recipient_token_account.owner == state.load()?.protocol_fee_recipient @ ErrorCode::InvalidProtocolFeeRecipient,
    )]
    pub recipient_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
    #[account(
        constraint = state.load()?.signer.eq(&drift_signer.key())
    )]
    /// CHECK: forced drift_signer
    pub drift_signer: UncheckedAccount<'info>,
}

#[derive(Accounts)]
#[instruction(market_index: u16)]
pub struct WithdrawProtocolFeesPerp<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(constraint = check_hot(&authority.key(), &state, HotRole::FeeWithdraw)?)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"perp_market", market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    #[account(
        mut,
        seeds = [b"spot_market", quote_spot_market.load()?.market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub quote_spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), quote_spot_market.load()?.market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        token::mint = spot_market_vault.mint,
        constraint = recipient_token_account.owner == state.load()?.protocol_fee_recipient @ ErrorCode::InvalidProtocolFeeRecipient,
    )]
    pub recipient_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
    #[account(
        constraint = state.load()?.signer.eq(&drift_signer.key())
    )]
    /// CHECK: forced drift_signer
    pub drift_signer: UncheckedAccount<'info>,
}
