//! The collateral of an isolated perp position.
//!
//! An isolated position carries its own margin, so funding and draining that
//! margin are separate instructions from the cross-margin deposit and
//! withdrawal. Each one moves tokens through the spot vault the collateral
//! lives in, and defers the accounting to `controller::isolated_position`.

use super::*;

#[access_control(
    deposit_not_paused(&ctx.accounts.state)
)]
pub fn handle_deposit_into_isolated_perp_position<'c: 'info, 'info>(
    ctx: Context<'info, DepositIsolatedPerpPosition<'info>>,
    spot_market_index: u16,
    perp_market_index: u16,
    amount: u64,
) -> Result<()> {
    let user_key = ctx.accounts.user.key();
    let mut user = load_mut!(ctx.accounts.user)?;

    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let slot = clock.slot;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        &MarketSet::new(),
        &get_writable_spot_market_set(spot_market_index),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let mint = get_token_mint(remaining_accounts_iter)?;

    controller::isolated_position::deposit_into_isolated_perp_position(
        user_key,
        &mut user,
        &mut maps,
        slot,
        now,
        &state,
        spot_market_index,
        perp_market_index,
        amount,
    )?;

    let spot_market = maps.spot_market_map.get_ref(&spot_market_index)?;

    controller::token::receive(
        &ctx.accounts.token_program,
        &ctx.accounts.user_token_account,
        &ctx.accounts.spot_market_vault,
        &ctx.accounts.authority,
        amount,
        &mint,
        if spot_market.has_transfer_hook() {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;

    ctx.accounts.spot_market_vault.reload()?;

    math::spot_withdraw::validate_spot_market_vault_amount(
        &spot_market,
        ctx.accounts.spot_market_vault.amount,
    )?;

    spot_market.validate_max_token_deposits_and_borrows(false)?;

    Ok(())
}

#[access_control(
    deposit_not_paused(&ctx.accounts.state)
    withdraw_not_paused(&ctx.accounts.state)
)]
pub fn handle_transfer_isolated_perp_position_deposit<'c: 'info, 'info>(
    ctx: Context<'info, TransferIsolatedPerpPositionDeposit<'info>>,
    spot_market_index: u16,
    perp_market_index: u16,
    amount: i64,
) -> anchor_lang::Result<()> {
    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;
    let slot = clock.slot;

    let user = &mut load_mut!(ctx.accounts.user)?;
    let user_stats = &mut load_mut!(ctx.accounts.user_stats)?;

    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    validate!(
        !user.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "user bankrupt"
    )?;

    let mut maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &get_writable_spot_market_set(spot_market_index),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    controller::isolated_position::transfer_isolated_perp_position_deposit(
        user,
        Some(user_stats),
        &mut maps,
        slot,
        now,
        spot_market_index,
        perp_market_index,
        amount,
        state.funding_paused()?,
    )?;

    let spot_market = maps.spot_market_map.get_ref(&spot_market_index)?;
    math::spot_withdraw::validate_spot_market_vault_amount(
        &spot_market,
        ctx.accounts.spot_market_vault.amount,
    )?;

    Ok(())
}

#[access_control(
    withdraw_not_paused(&ctx.accounts.state)
)]
pub fn handle_withdraw_from_isolated_perp_position<'c: 'info, 'info>(
    ctx: Context<'info, WithdrawIsolatedPerpPosition<'info>>,
    spot_market_index: u16,
    perp_market_index: u16,
    amount: u64,
) -> anchor_lang::Result<()> {
    let user_key = ctx.accounts.user.key();
    let user = &mut load_mut!(ctx.accounts.user)?;
    let mut user_stats = load_mut!(ctx.accounts.user_stats)?;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let slot = clock.slot;
    let state = ctx.accounts.state.load()?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        &MarketSet::new(),
        &get_writable_spot_market_set(spot_market_index),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let mint = get_token_mint(remaining_accounts_iter)?;

    controller::isolated_position::withdraw_from_isolated_perp_position(
        user_key,
        user,
        &mut user_stats,
        &mut maps,
        slot,
        now,
        spot_market_index,
        perp_market_index,
        amount,
        state.funding_paused()?,
    )?;

    let spot_market = maps.spot_market_map.get_ref(&spot_market_index)?;

    controller::token::send_from_program_vault(
        &ctx.accounts.token_program,
        &ctx.accounts.spot_market_vault,
        &ctx.accounts.user_token_account,
        &ctx.accounts.velocity_signer,
        state.signer_nonce,
        amount,
        &mint,
        if spot_market.has_transfer_hook() {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;

    // reload the spot market vault balance so it's up-to-date
    ctx.accounts.spot_market_vault.reload()?;
    math::spot_withdraw::validate_spot_market_vault_amount(
        &spot_market,
        ctx.accounts.spot_market_vault.amount,
    )?;

    spot_market.validate_max_token_deposits_and_borrows(false)?;

    Ok(())
}

#[derive(Accounts)]
#[instruction(spot_market_index: u16,)]
pub struct DepositIsolatedPerpPosition<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), spot_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = &spot_market_vault.mint.eq(&user_token_account.mint),
        token::authority = authority
    )]
    pub user_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
#[instruction(spot_market_index: u16,)]
pub struct TransferIsolatedPerpPositionDeposit<'info> {
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(
        seeds = [b"spot_market_vault".as_ref(), spot_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
}

#[derive(Accounts)]
#[instruction(spot_market_index: u16)]
pub struct WithdrawIsolatedPerpPosition<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        has_one = authority,
    )]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        has_one = authority
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), spot_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        constraint = state.load()?.signer.eq(&velocity_signer.key())
    )]
    /// CHECK: forced velocity_signer
    pub velocity_signer: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = &spot_market_vault.mint.eq(&user_token_account.mint)
    )]
    pub user_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}
