//! The per-account settings an owner or delegate writes.
//!
//! Each handler writes one field of `User` or `UserStats`. The two that touch
//! margin load the market and oracle maps, because changing them can only be
//! allowed while the account still meets its requirement.

use super::*;

pub fn handle_update_user_name(
    ctx: Context<UpdateUser>,
    _sub_account_id: u16,
    name: [u8; 32],
) -> Result<()> {
    let mut user = load_mut!(ctx.accounts.user)?;
    user.name = name;
    Ok(())
}

pub fn handle_update_user_custom_margin_ratio(
    ctx: Context<UpdateUser>,
    _sub_account_id: u16,
    margin_ratio: u32,
) -> Result<()> {
    let mut user = load_mut!(ctx.accounts.user)?;
    user.max_margin_ratio = margin_ratio;
    Ok(())
}

pub fn handle_update_user_perp_position_custom_margin_ratio(
    ctx: Context<UpdateUserPerpPositionCustomMarginRatio>,
    _sub_account_id: u16,
    perp_market_index: u16,
    margin_ratio: u16,
) -> Result<()> {
    let mut user = load_mut!(ctx.accounts.user)?;

    user.update_perp_position_max_margin_ratio(perp_market_index, margin_ratio)?;

    Ok(())
}

pub fn handle_update_user_margin_trading_enabled<'c: 'info, 'info>(
    ctx: Context<'info, UpdateUserWithMarkets<'info>>,
    _sub_account_id: u16,
    margin_trading_enabled: bool,
) -> Result<()> {
    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        &MarketSet::new(),
        &MarketSet::new(),
        Clock::get()?.slot,
        ctx.accounts.state.load()?.slot_clock(),
        None,
    )?;

    let mut user = load_mut!(ctx.accounts.user)?;
    user.is_margin_trading_enabled = margin_trading_enabled;

    validate_spot_margin_trading(&user, &mut maps).map_err(|_| ErrorCode::MarginOrdersOpen)?;

    Ok(())
}

pub fn handle_update_user_pool_id<'c: 'info, 'info>(
    ctx: Context<'info, UpdateUserWithMarkets<'info>>,
    _sub_account_id: u16,
    pool_id: u8,
) -> Result<()> {
    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        &MarketSet::new(),
        &MarketSet::new(),
        Clock::get()?.slot,
        ctx.accounts.state.load()?.slot_clock(),
        None,
    )?;

    let mut user = load_mut!(ctx.accounts.user)?;
    user.pool_id = pool_id;

    // will throw if user has deposits/positions in other pools
    meets_initial_margin_requirement(&user, &mut maps)?;

    Ok(())
}

pub fn handle_update_user_delegate(
    ctx: Context<UpdateUser>,
    _sub_account_id: u16,
    delegate: Pubkey,
) -> Result<()> {
    let mut user = load_mut!(ctx.accounts.user)?;
    user.delegate = delegate;
    Ok(())
}

pub fn handle_update_user_allow_delegate_transfer(
    ctx: Context<UpdateUserStats>,
    allow_delegate_transfer: bool,
) -> Result<()> {
    let mut user_stats = load_mut!(ctx.accounts.user_stats)?;
    user_stats.update_allow_delegate_transfer(allow_delegate_transfer)?;
    Ok(())
}

pub fn handle_update_user_reduce_only(
    ctx: Context<UpdateUser>,
    _sub_account_id: u16,
    reduce_only: bool,
) -> Result<()> {
    let mut user = load_mut!(ctx.accounts.user)?;

    validate!(!user.is_being_liquidated(), ErrorCode::LiquidationsOngoing)?;

    user.update_reduce_only_status(reduce_only)?;
    Ok(())
}

pub fn handle_update_user_advanced_lp(
    ctx: Context<UpdateUser>,
    _sub_account_id: u16,
    advanced_lp: bool,
) -> Result<()> {
    let mut user = load_mut!(ctx.accounts.user)?;

    validate!(!user.is_being_liquidated(), ErrorCode::LiquidationsOngoing)?;

    user.update_advanced_lp_status(advanced_lp)?;
    Ok(())
}

pub fn handle_update_user_vault_owned(
    ctx: Context<UpdateUser>,
    _sub_account_id: u16,
) -> Result<()> {
    let mut user = load_mut!(ctx.accounts.user)?;

    // Set-only: a vault-owned User must never be un-flagged, or the
    // revenue-share sweep would resume crediting it and re-open the NAV-capture
    // vectors (OtterSec #91/#92/#93). Idempotent — re-marking is a no-op.
    user.add_user_status(crate::state::user::UserStatus::VaultOwned);
    Ok(())
}

#[derive(Accounts)]
pub struct UpdateUserStats<'info> {
    #[account(
        mut,
        seeds = [b"user_stats", authority.key.as_ref()],
        bump,
        has_one = authority
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(
    sub_account_id: u16,
)]
pub struct UpdateUser<'info> {
    #[account(
        mut,
        seeds = [b"user", authority.key.as_ref(), sub_account_id.to_le_bytes().as_ref()],
        bump,
    )]
    pub user: AccountLoader<'info, User>,
    pub authority: Signer<'info>,
}

/// `UpdateUser` plus `State`, for the two handlers that load market/oracle maps
/// and therefore need the live slot duration. Kept separate so the other
/// `UpdateUser` handlers, which touch no oracle, keep their account list.
#[derive(Accounts)]
#[instruction(sub_account_id: u16)]
pub struct UpdateUserWithMarkets<'info> {
    #[account(
        mut,
        seeds = [b"user", authority.key.as_ref(), sub_account_id.to_le_bytes().as_ref()],
        bump,
    )]
    pub user: AccountLoader<'info, User>,
    pub authority: Signer<'info>,
    /// Read only for the live slot duration. The seed constraint both locks
    /// the account to the singleton `State` and lets clients resolve it from
    /// the IDL, so callers that built this instruction before the account
    /// existed keep working.
    #[account(
        seeds = [b"velocity_state".as_ref()],
        bump,
    )]
    pub state: AccountLoader<'info, State>,
}

#[derive(Accounts)]
pub struct UpdateUserPerpPositionCustomMarginRatio<'info> {
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    pub authority: Signer<'info>,
}
