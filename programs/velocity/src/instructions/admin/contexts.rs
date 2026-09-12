//! The accounts structs that several admin subjects share.
//!
//! Each one names an authority tier and the account it writes. A handler picks
//! the struct whose tier gates it, so the gate is visible where the handler
//! names its accounts. Structs that only one subject uses live with that
//! subject.

use super::*;

#[derive(Accounts)]
pub struct AdminUpdatePerpMarket<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
}

#[derive(Accounts)]
pub struct HotAdminUpdatePerpMarket<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
}

#[derive(Accounts)]
pub struct AdminUpdateState<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(mut)]
    pub state: AccountLoader<'info, State>,
}

#[derive(Accounts)]
pub struct HotAdminUpdateState<'info> {
    #[account(constraint = check_hot(&admin.key(), &state, HotRole::FeatureFlag)?)]
    pub admin: Signer<'info>,
    #[account(mut)]
    pub state: AccountLoader<'info, State>,
}

#[derive(Accounts)]
pub struct AdminUpdateSpotMarket<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub spot_market: AccountLoader<'info, SpotMarket>,
}

#[derive(Accounts)]
pub struct AdminDisableBidAskTwapUpdate<'info> {
    #[account(constraint = check_hot(&admin.key(), &state, HotRole::UserFlag)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user_stats: AccountLoader<'info, UserStats>,
}

// ----- Pause-admin gated contexts -----
//
// Pause flags can be flipped by cold, warm, or the dedicated `pause_admin`
// (which has no on-chain timelock). pause_admin is restricted *inside* the
// handlers to bit-additions only — it can never clear a pause bit.

#[derive(Accounts)]
pub struct PauseAdminUpdateState<'info> {
    #[account(constraint = check_pause(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(mut)]
    pub state: AccountLoader<'info, State>,
}

#[derive(Accounts)]
pub struct PauseAdminUpdateSpotMarket<'info> {
    #[account(constraint = check_pause(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub spot_market: AccountLoader<'info, SpotMarket>,
}

#[derive(Accounts)]
pub struct PauseAdminUpdatePerpMarket<'info> {
    #[account(constraint = check_pause(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
}
