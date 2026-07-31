//! Relay plumbing for user trigger orders: the per-user condition block
//! (`state::trigger_conditions`) and its permissionless sync. The trigger
//! resolvers live beside their executors (`ResolveTriggerOrder` in the
//! keeper tree with `trigger_order`, `ResolveTriggerClobOrder` with
//! `trigger_clob_order`).

pub mod sync_trigger_conditions;

pub use sync_trigger_conditions::*;
