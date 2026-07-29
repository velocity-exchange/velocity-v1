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
//! - evict/expire cranks (velocity wrappers over the CLOB's crank ixs) are
//!   still to come.

mod cancel_clob_order;
mod place_clob_order;

pub use cancel_clob_order::*;
pub use place_clob_order::*;
