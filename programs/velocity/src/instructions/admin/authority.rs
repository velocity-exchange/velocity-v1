//! Who may administer the exchange.
//!
//! The cold admin is the root key. It rotates the warm admin, the pause admin,
//! and itself, and it names the treasury that protocol fees reach. The warm
//! admin rotates an individual hot role. Each tier gets its own accounts
//! struct, so the constraint that gates a handler is visible where the handler
//! names its accounts.

use super::*;

pub fn handle_update_admin(ctx: Context<ColdAdminUpdateState>, admin: Pubkey) -> Result<()> {
    msg!(
        "admin: {:?} -> {:?}",
        ctx.accounts.state.load()?.cold_admin,
        admin
    );
    ctx.accounts.state.load_mut()?.cold_admin = admin;
    Ok(())
}

// ----- Tiered admin authority handlers -----
//
// cold/warm/hot pubkeys now live directly on `State`. `handle_initialize`
// seeds `cold_admin = warm_admin = signer` at deploy time; the handlers below
// rotate `warm_admin` (cold-only), `pause_admin` (cold-only), and individual
// hot-role keys (warm-only).

pub fn handle_update_warm_admin(
    ctx: Context<UpdateWarmAdmin>,
    new_warm_admin: Pubkey,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    msg!("warm_admin: {:?} -> {:?}", state.warm_admin, new_warm_admin);
    state.warm_admin = new_warm_admin;
    Ok(())
}

pub fn handle_update_pause_admin(
    ctx: Context<UpdatePauseAdmin>,
    new_pause_admin: Pubkey,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    msg!(
        "pause_admin: {:?} -> {:?}",
        state.pause_admin,
        new_pause_admin
    );
    state.pause_admin = new_pause_admin;
    Ok(())
}

pub fn handle_update_hot_admin(
    ctx: Context<UpdateHotAdmin>,
    role: HotRole,
    new_pubkey: Pubkey,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    let prev = state.hot_key(role);
    state.set_hot_key(role, new_pubkey);
    msg!("hot_admin[{:?}]: {:?} -> {:?}", role, prev, new_pubkey);
    Ok(())
}

/// Cold-only. Sets the treasury that protocol fees can be withdrawn to —
/// perp (quote-denominated) and spot (per-market tokens) recipients are
/// configured independently via `market_type`.
pub fn handle_update_protocol_fee_recipient(
    ctx: Context<ColdAdminUpdateState>,
    protocol_fee_recipient: Pubkey,
    market_type: MarketType,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    match market_type {
        MarketType::Perp => {
            msg!(
                "protocol_fee_recipient_perp: {:?} -> {:?}",
                state.protocol_fee_recipient_perp,
                protocol_fee_recipient
            );
            state.protocol_fee_recipient_perp = protocol_fee_recipient;
        }
        MarketType::Spot => {
            msg!(
                "protocol_fee_recipient_spot: {:?} -> {:?}",
                state.protocol_fee_recipient_spot,
                protocol_fee_recipient
            );
            state.protocol_fee_recipient_spot = protocol_fee_recipient;
        }
    }
    Ok(())
}

/// Cold-only state mutation. Constraint enforces `state.cold_admin == admin.key()`.
#[derive(Accounts)]
pub struct ColdAdminUpdateState<'info> {
    #[account(mut, constraint = state.load()?.cold_admin == admin.key() @ ErrorCode::Unauthorized)]
    pub state: AccountLoader<'info, State>,
    pub admin: Signer<'info>,
}

/// Cold-only mutation of `warm_admin`.
#[derive(Accounts)]
pub struct UpdateWarmAdmin<'info> {
    #[account(mut, constraint = state.load()?.cold_admin == admin.key() @ ErrorCode::Unauthorized)]
    pub state: AccountLoader<'info, State>,
    pub admin: Signer<'info>,
}

/// Cold-only mutation of `pause_admin`. The pause admin is the no-timelock
/// emergency-pause key; only the root (cold) authority can rotate it.
#[derive(Accounts)]
pub struct UpdatePauseAdmin<'info> {
    #[account(mut, constraint = state.load()?.cold_admin == admin.key() @ ErrorCode::Unauthorized)]
    pub state: AccountLoader<'info, State>,
    pub admin: Signer<'info>,
}

/// Warm-or-cold gated mutation of an individual hot-role key.
#[derive(Accounts)]
pub struct UpdateHotAdmin<'info> {
    #[account(
        mut,
        constraint = state.load()?.is_warm(&admin.key()) @ ErrorCode::Unauthorized
    )]
    pub state: AccountLoader<'info, State>,
    pub admin: Signer<'info>,
}
