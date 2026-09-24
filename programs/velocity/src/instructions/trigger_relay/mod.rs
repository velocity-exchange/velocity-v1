//! Relay plumbing for user trigger orders: the per-user condition block
//! (`state::user_conditions`) and its permissionless sync. Each trigger resolver
//! lives beside its executor, so `ResolveTriggerMarketOrderV1` is with
//! `trigger_market_order_v1` and `ResolveTriggerLimitOrderV1` is with
//! `trigger_limit_order_v1`.

pub mod sync_trigger_conditions;

pub use sync_trigger_conditions::*;
