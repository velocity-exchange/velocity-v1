//! The protocol's single relay crank treasury.
//!
//! One account funds every market's crank reservoir. An admin creates it once
//! and prices it. A plain lamport transfer tops it up. There is no deposit
//! instruction, because crediting lamports to an account needs no program.
//! Reservoirs draw from it through the permissionless refill crank, so the
//! treasury is the only balance an operator watches.
//!
//! See [`crate::state::crank_treasury`] for why each market's reservoir still
//! pays its own cranks, and this account does not pay them directly.

mod initialize_crank_treasury;
mod sweep_crank_reservoir;
mod update_crank_treasury;
mod withdraw_crank_treasury;

pub use {
    initialize_crank_treasury::*, sweep_crank_reservoir::*, update_crank_treasury::*,
    withdraw_crank_treasury::*,
};
