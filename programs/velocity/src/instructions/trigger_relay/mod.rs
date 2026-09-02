//! Relay plumbing for user trigger orders: the per-user condition block
//! (`state::trigger_conditions`) and its permissionless sync. The trigger
//! resolvers live beside their executors (`ResolveTriggerOrder` in the
//! keeper tree with `trigger_order`, `ResolveTriggerLimitOrderV1` with
//! `trigger_limit_order_v1`).

pub mod sync_trigger_conditions;

pub use sync_trigger_conditions::*;
