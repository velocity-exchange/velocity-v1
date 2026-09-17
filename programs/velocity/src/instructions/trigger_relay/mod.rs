//! Relay plumbing for user trigger orders: the per-user condition block
//! (`state::user_conditions`) and its permissionless sync. Each trigger resolver
//! lives beside its executor. `ResolveTriggerOrder` is in the keeper tree with
//! `trigger_order`, and `ResolveTriggerLimitOrderV1` is with
//! `trigger_limit_order_v1`.

pub mod sync_trigger_conditions;

pub use sync_trigger_conditions::*;
