//! Router instructions: one file per instruction, `#[derive(Accounts)]` on top
//! and the handler below.

pub mod distribute;
pub mod initialize;
pub mod set_admin;
pub mod set_cranker;
pub mod set_tiers;
pub mod set_treasury;

pub use {
    distribute::*, initialize::*, set_admin::*, set_cranker::*, set_tiers::*, set_treasury::*,
};
