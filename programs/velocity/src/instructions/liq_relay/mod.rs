//! Relay plumbing for liquidations: the per-user condition block
//! (`state::liq_conditions`), its self-maintaining sync, and the resolver
//! that stages `liquidate_perp_with_fill` with the protocol `User` as the
//! (inventory-free) liquidator.

pub mod resolve_liquidate_perp_with_fill;
pub mod resolve_sync_liq_conditions;
pub mod sync_liq_conditions;

pub use {
    resolve_liquidate_perp_with_fill::*, resolve_sync_liq_conditions::*, sync_liq_conditions::*,
};
