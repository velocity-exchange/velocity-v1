//! Velocity-mediated CLOB order lifecycle. Plain resting limits live on a
//! registered CLOB program, not in `User.orders` — but every path that
//! changes a maker's worst-case exposure runs through velocity so the
//! open-order aggregates (`open_bids`/`open_asks`, the open-order counters)
//! that back the DLOB margin model stay exact:
//!
//! - [`place_clob_order`]: margin gate + aggregate reserve, then a CPI to the
//!   CLOB as its `place_authority` (the velocity signer PDA).
//! - [`cancel_clob_order`]: cancel CPI, then unwind the removed order's
//!   remaining size from the aggregates.
//! - fills/culls unwind through the router fill's execute response.
//! - [`crank_clob_evict`]/[`crank_clob_remove_expired`]: permissionless
//!   keeper wrappers over the CLOB's crank ixs — the maker's `User` rides
//!   along so the returned removal unwinds its aggregates. Dual-mode: a
//!   signed keeper earns the flat reward from the maker as before, or the
//!   protocol-owned `User` is passed as the filler (program-keeper mode) and
//!   the caller takes reservoir lamports instead.
//! - [`crank_conditions_setup`]: writes the market's relay condition block —
//!   called by `update_perp_market_clob_quoter`, so attaching a CLOB stands
//!   its cranks up in the same instruction.
//! - [`resolve_clob_crank`]: the simulation-only relay resolvers that stage
//!   the crank executor calls.

mod cancel_clob_order;
mod crank_clob_order_removal;
mod crank_conditions_setup;
mod place_clob_order;
mod resolve_clob_crank;

pub use {
    cancel_clob_order::*, crank_clob_order_removal::*, crank_conditions_setup::*,
    place_clob_order::*, resolve_clob_crank::*,
};
