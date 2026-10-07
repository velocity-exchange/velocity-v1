//! Relay plumbing for liquidations. It holds the per-user condition block in
//! `state::user_conditions`, the sync that maintains that block, and the
//! resolver that stages `liquidate_perp_with_fill`. The resolver names the
//! protocol `User` as the liquidator, which takes no inventory.
//!
//! Every staged liquidation and force cancel writes that one `User` and its
//! `UserStats`. Any transaction can lock them writable, so a party that fills
//! their per-block compute limit delays relay coverage of every user for that
//! block. Signed keepers fill with their own accounts and are not affected.

pub mod resolve_liquidate_perp_with_fill;
pub mod resync_liq_conditions;
pub mod sync_liq_conditions;

pub use {resolve_liquidate_perp_with_fill::*, resync_liq_conditions::*, sync_liq_conditions::*};
