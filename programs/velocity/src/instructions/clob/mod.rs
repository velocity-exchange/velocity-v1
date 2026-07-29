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
//!   along so the returned removal unwinds its aggregates, and the keeper
//!   earns the flat reward from the maker.

mod cancel_clob_order;
mod crank_clob_order_removal;
mod place_clob_order;

pub use cancel_clob_order::*;
pub use crank_clob_order_removal::*;
pub use place_clob_order::*;
