//! The protocol's single relay crank treasury.
//!
//! One account funds every market's crank reservoir. It is created once,
//! priced by an admin, and topped up by a plain lamport transfer — there is no
//! deposit instruction, because crediting lamports to an account needs no
//! program. Reservoirs draw from it through the permissionless refill crank,
//! so the treasury is the only balance an operator watches.
//!
//! See [`crate::state::crank_treasury`] for why the cranks are still paid by
//! their own market's reservoir rather than from here directly.

mod initialize_crank_treasury;
mod sweep_crank_reservoir;
mod update_crank_treasury;
mod withdraw_crank_treasury;

pub use {
    initialize_crank_treasury::*, sweep_crank_reservoir::*, update_crank_treasury::*,
    withdraw_crank_treasury::*,
};
