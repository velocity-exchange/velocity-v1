//! Router instructions: one file per instruction, `#[derive(Accounts)]` on top
//! and the handler below.

pub mod distribute;
pub mod initialize;
pub mod update_config;

pub use {distribute::*, initialize::*, update_config::*};
