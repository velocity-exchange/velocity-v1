//! Per-user flags an admin sets.
//!
//! The handlers mark a user as exempt or accelerated, pause an authority's
//! operations, set the equity floor a subaccount must hold, and clear the
//! breaker that a floor breach trips.

use super::*;

pub fn handle_update_user_accelerated_referral_status(
    ctx: Context<AdminUpdateUserStats>,
    accelerated: bool,
) -> Result<()> {
    let mut user_stats = ctx.accounts.user_stats.load_mut()?;
    let previous_status = user_stats.accelerated_referral_status;

    if user_stats.set_accelerated_referral_by_admin(accelerated) {
        emit_accelerated_referral_status_changed(
            Clock::get()?.unix_timestamp,
            user_stats.authority,
            previous_status,
            user_stats.accelerated_referral_status,
            if accelerated {
                AcceleratedReferralStatusChange::AdminGrant
            } else {
                AcceleratedReferralStatusChange::AdminRevoke
            },
        );
    }

    Ok(())
}

pub fn handle_admin_update_user_stats_paused_operations(
    ctx: Context<PauseAdminUpdateUserStats>,
    paused_operations: u8,
) -> Result<()> {
    let mut user_stats = load_mut!(ctx.accounts.user_stats)?;

    // Authority matrix for user_stats.paused_operations:
    //   * cold / warm / hot_user_flag — full control (pause + unpause)
    //   * pause_admin                 — pause-only (may not clear bits)
    //
    // `is_hot(.., UserFlag)` already returns true for cold/warm; the negation
    // therefore isolates pause_admin specifically.
    let signer = ctx.accounts.admin.key();
    let state = ctx.accounts.state.load()?;
    if !state.is_hot(&signer, HotRole::UserFlag) {
        validate!(
            (user_stats.paused_operations & paused_operations) == user_stats.paused_operations,
            ErrorCode::Unauthorized,
            "pause_admin may not clear pause bits",
        )?;
    }
    drop(state);

    msg!(
        "user_stats.paused_operations: {:?} -> {:?}",
        user_stats.paused_operations,
        paused_operations
    );

    user_stats.paused_operations = paused_operations;
    Ok(())
}

pub fn handle_update_special_user_status(
    ctx: Context<UpdateSpecialUserStatus>,
    status: u8,
) -> Result<()> {
    let allowed_bits = SpecialUserStatus::VammHedger as u8;

    validate!(
        status & !allowed_bits == 0,
        ErrorCode::DefaultError,
        "unknown bits set in user's special_user_status: {:?}",
        status
    )?;

    let user = &mut load_mut!(ctx.accounts.user)?;

    if *ctx.accounts.admin.key != ctx.accounts.state.load()?.cold_admin {
        validate!(
            status == 0,
            ErrorCode::DefaultError,
            "signer must be state admin to enable special user status flags",
        )?;
    }

    msg!(
        "special_user_status for {:?}: {:?} -> {:?}",
        user.authority,
        user.special_user_status,
        status
    );

    user.special_user_status = status;

    Ok(())
}

/// Clears the authority-wide equity breaker set by the permissionless
/// `trip_equity_floor_breaker`. Warm admin only; intended to be called after
/// a human has reviewed why the breaker fired.
///
/// The clear is self-verifying at execution time: `remaining_accounts` must
/// carry every live subaccount of the authority (count pinned by
/// `UserStats.number_of_sub_accounts`, so none can be omitted or passed
/// twice) followed by the markets and oracles their positions reference, and
/// every floored subaccount must show net equity at or above its
/// floor + buffer with all oracles valid. An approval that has gone stale
/// (a subaccount drifted back into breach after review) therefore fails
/// instead of unfreezing a breached authority.
///
/// The validity requirement here stays all-or-nothing, deliberately not
/// sharing the trip's dust concession. The trip proves equity below the
/// floor, so unknowns are conceded upward and a trip that fires is sound at
/// any true dust price; the reset proves the opposite direction, where
/// conceding dust upward would unfreeze off values the program cannot
/// verify. A dead oracle on a dust position therefore blocks the reset
/// until the feed recovers. The escape hatch, here and whenever resumption
/// is the business decision anyway, is `update_user_equity_floor`: lower
/// the floors first, explicitly and auditably.
pub fn handle_reset_equity_floor_breaker<'c: 'info, 'info>(
    ctx: Context<'info, ResetEquityFloorBreaker<'info>>,
) -> Result<()> {
    let state = ctx.accounts.state.load()?;
    let mut user_stats = load_mut!(ctx.accounts.user_stats)?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let user_map = load_user_map(remaining_accounts_iter, false)?;
    let mut maps = load_maps(
        remaining_accounts_iter,
        &MarketSet::new(),
        &MarketSet::new(),
        Clock::get()?.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    // Completeness: exactly the authority's live subaccounts. The map is
    // keyed by pubkey (a duplicate collapses and fails the count), every
    // entry must belong to the authority, and distinct same-authority user
    // accounts are distinct subaccounts (PDA uniqueness), so no subaccount
    // can be omitted or counted twice.
    validate!(
        user_map.0.len() == user_stats.number_of_sub_accounts as usize,
        ErrorCode::InvalidEquityBreakerReset,
        "expected all {} subaccounts of the authority, got {}",
        user_stats.number_of_sub_accounts,
        user_map.0.len()
    )?;

    for user_account_loader in user_map.0.values() {
        let user = user_account_loader.load()?;

        validate_subaccount_above_floor(&user, user_stats.authority, &mut maps)?;
    }

    msg!(
        "equity floor breaker reset for authority {:?}",
        user_stats.authority
    );

    user_stats.set_equity_breaker_tripped(false);

    Ok(())
}

/// Proves one subaccount belongs to the authority and stands clear of its
/// equity floor.
///
/// A subaccount with no floor set has nothing to prove. Every other one must
/// show net equity at or above its floor plus its buffer, priced by valid
/// oracles.
fn validate_subaccount_above_floor(
    user: &User,
    authority: Pubkey,
    maps: &mut crate::instructions::optional_accounts::AccountMaps,
) -> Result<()> {
    validate!(
        user.authority == authority,
        ErrorCode::InvalidEquityBreakerReset,
        "subaccount {} does not belong to authority {}",
        user.sub_account_id,
        authority
    )?;

    if user.equity_floor == 0 {
        return Ok(());
    }

    let (net_equity, all_oracles_valid) = calculate_user_equity(user, maps)?;

    // An unfreeze must not be granted off an invalid price, mirroring
    // the trip's own oracle-validity requirement.
    validate!(
        all_oracles_valid,
        ErrorCode::InvalidOracle,
        "cannot reset equity floor breaker with an invalid oracle"
    )?;

    validate!(
        !user.is_below_buffered_equity_floor(net_equity),
        ErrorCode::InvalidEquityBreakerReset,
        "subaccount {} net equity {} below equity floor {} + buffer {}",
        user.sub_account_id,
        net_equity,
        user.equity_floor,
        user.equity_floor_buffer
    )?;

    Ok(())
}

pub fn handle_update_user_equity_floor(
    ctx: Context<AdminUpdateUserEquityFloor>,
    equity_floor: u64,
    equity_floor_buffer: u64,
) -> Result<()> {
    let user = &mut load_mut!(ctx.accounts.user)?;

    msg!(
        "equity_floor for {:?}: {:?} -> {:?}, buffer: {:?} -> {:?}",
        user.authority,
        user.equity_floor,
        equity_floor,
        user.equity_floor_buffer,
        equity_floor_buffer
    );

    user.equity_floor = equity_floor;
    user.equity_floor_buffer = equity_floor_buffer;

    Ok(())
}

#[derive(Accounts)]
pub struct AdminUpdateUserStats<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user_stats: AccountLoader<'info, UserStats>,
}

#[derive(Accounts)]
pub struct UpdateSpecialUserStatus<'info> {
    #[account(constraint = check_hot(&admin.key(), &state, HotRole::UserFlag)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
}

#[derive(Accounts)]
pub struct AdminUpdateUserEquityFloor<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
}

#[derive(Accounts)]
pub struct ResetEquityFloorBreaker<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user_stats: AccountLoader<'info, UserStats>,
}

/// Per-user pause flips are reachable by cold/warm, the existing
/// `HotRole::UserFlag` bot, or the pause_admin (pause-only — see handler).
#[derive(Accounts)]
pub struct PauseAdminUpdateUserStats<'info> {
    #[account(
        constraint =
            check_pause(&admin.key(), &state)?
                || check_hot(&admin.key(), &state, HotRole::UserFlag)?
    )]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user_stats: AccountLoader<'info, UserStats>,
}
