//! Velocity-mediated CLOB order lifecycle. Plain resting limits live on a
//! registered CLOB program, not in `User.orders` — but every path that
//! changes a maker's worst-case exposure runs through velocity so the
//! open-order aggregates (`open_bids`/`open_asks`, the open-order counters)
//! that back the DLOB margin model stay exact:
//!
//! - [`place_clob_order`]: margin gate + aggregate reserve, then a CPI to the
//!   CLOB as its `place_authority` (the quoter CPI signer PDA).
//! - [`cancel_clob_order`]: cancel CPI, then unwind the removed order's
//!   remaining size from the aggregates.
//! - [`modify_clob_order`]: cancel-and-replace in one instruction, with one
//!   margin gate over the *net* change (the CLOB has no in-place mutation).
//! - [`fill_v1`]: the keeper fill with the CLOB accounts required — a restable
//!   remainder migrates to the book instead of resting in `User.orders`.
//! - [`place_and_make_v1`]: the maker route with the CLOB accounts required —
//!   an unmatched remainder rests on the book instead of being cancelled.
//! - [`place_and_take_v1`]: the taker route with the CLOB accounts required —
//!   an unfilled restable limit remainder rests on the book instead of the
//!   DLOB. The v0 instruction's account list is frozen, hence the new
//!   endpoint.
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
//!
//! Every CPI to the book goes through `ClobMarket` (`state::prop_amm`): one
//! place in the program speaks the CLOB's wire — its discriminators, borsh
//! args, `invoke_signed` account pair, and return-data decode.
//!
//! Each crank instruction lives in its own file together with its
//! simulation-only relay resolver (named `Resolve<EndpointName>`); the
//! shared dual-mode plumbing is [`crank_common`].
//! - [`trigger_clob_order`]: crank an armed trigger-limit onto the CLOB; the
//!   `User.orders` slot becomes a shadow keeping the trigger params + the
//!   CLOB `OrderRef` (freed on fill/cancel/expiry, re-armed on eviction).
//! - [`crank_cross_match`]: fill two crossed resting sources against each
//!   other with the protocol `User` as the pass-through taker; fires only
//!   when the spread nets positive after fees.
//! - [`crank_taker_origin_cross`]: hand a migrated taker remainder the
//!   improvement its auction window earned it — consume the crossing
//!   counterparty, lift the remainder off the book, and settle the pair at the
//!   counterparty's price, paying the cranker out of the difference. No
//!   protocol pass-through: one side is the aggressor and the improvement is
//!   its own.
//! - [`force_cancel_clob_orders`]: the CLOB arm of the force-cancel keeper
//!   flow — reclaim a failing account's risk-increasing book orders (and
//!   their placed-trigger shadows) for the flat fee.

mod cancel_clob_order;
mod crank_clob_evict;
mod crank_clob_remove_expired;
mod crank_common;
mod crank_conditions_setup;
mod crank_cross_match;
mod crank_taker_origin_cross;
mod fill_v1;
mod force_cancel_clob_orders;
mod initialize_quoter_cross_conditions;
mod modify_clob_order;
mod place_and_make_v1;
mod place_and_take_v1;
mod place_clob_order;
mod trigger_clob_order;

pub use {
    cancel_clob_order::*, crank_clob_evict::*, crank_clob_remove_expired::*, crank_common::*,
    crank_conditions_setup::*, crank_cross_match::*, crank_taker_origin_cross::*, fill_v1::*,
    force_cancel_clob_orders::*, initialize_quoter_cross_conditions::*, modify_clob_order::*,
    place_and_make_v1::*, place_and_take_v1::*, place_clob_order::*, trigger_clob_order::*,
};
