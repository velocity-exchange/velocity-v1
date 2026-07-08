//! The hedge half of VLP: the LP pool that offsets vAMM inventory.
//!
//! Contains the pool accounts and their constituent loader, the perp-to-lp
//! settlement math, the admin-setup and user-facing instruction handlers, and the
//! settlement keeper handler.

// Instruction handlers compile out unless the vlp-hedge feature is on (mainnet
// builds exclude it until the hedge component is audited). State, settlement
// math, and the constituent loader stay compiled so account layouts and shared
// readers (amm_cache, keeper crank) are identical across builds.
pub mod admin;
pub mod constituent_map;
pub mod instructions;
pub mod math;
pub mod settle;
pub mod state;
