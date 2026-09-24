//! Velocity-mediated CLOB order lifecycle. Plain resting limits live on a
//! registered CLOB program, not in `User.orders`. Every path that changes a
//! maker's worst-case exposure still runs through velocity, so the open-order
//! aggregates that back the margin model stay exact: `open_bids`, `open_asks`,
//! and the open-order counters. Fills and culls unwind through the router
//! fill's execute response. Every other path unwinds here.
//!
//! The user routes. A v1 live order is detached. The handler builds it on the
//! stack, checks margin, and never writes it into `User.orders`.
//! - [`place_and_make_v1`]: the maker route. A post-only limit rests straight
//!   on the book as a maker quote. It names no taker and matches nothing on
//!   placement.
//! - [`place_and_take_v1`]: the taker route. The router fills across the vAMM
//!   and the quoter books. The restable remainder then rests on the book
//!   taker-origin.
//! - [`cancel_order_v1`]: cancel by CPI, then unwind the removed order's
//!   remaining size from the aggregates.
//! - [`cancel_orders_v1`]: the same for a maker's whole side, or for both
//!   sides, in one CPI. It unwinds from per-side totals, so the cost does not
//!   grow with the ladder. The book caps a single sweep and reports the cap.
//!   The handler unwinds what the book removed, so a repeat call converges.
//! - [`modify_order_v1`]: cancel and replace in one instruction. One margin
//!   gate covers the net change. The CLOB has no in-place mutation.
//!
//! The keeper endpoints:
//! - [`trigger_limit_order_v1`]: crank an armed trigger-limit onto the CLOB.
//!   The `User.orders` slot becomes a shadow that holds the trigger parameters
//!   and the CLOB `OrderRef`. A fill, a cancel or an expiry frees the slot. An
//!   eviction re-arms it.
//! - [`trigger_market_order_v1`]: fire an armed trigger-market. The router
//!   fills it in the same instruction, the remainder rests on the book
//!   taker-origin, and the slot is freed. The limit and market cranks differ on
//!   fire semantics. A fired limit rests whole. A fired market fills first.
//! - [`force_cancel_clob_orders`]: the CLOB arm of the force-cancel keeper
//!   flow. It reclaims a failing account's risk-increasing book orders, and
//!   their placed-trigger shadows, for the flat fee.
//!
//! The cranks, which relay conditions wake:
//! - [`crank_clob_evict`] and [`crank_clob_remove_expired`]: permissionless
//!   keeper wrappers over the CLOB's crank instructions. The maker's `User` is
//!   passed with them, so the returned removal unwinds its aggregates. Both
//!   have two modes. A signed keeper earns the flat reward from the maker. In
//!   program-keeper mode the protocol-owned `User` is the filler and the caller
//!   takes reservoir lamports instead.
//! - [`crank_cross_match`]: fill two crossed resting sources against each
//!   other, with the protocol `User` as the pass-through taker. It fires only
//!   when the spread nets positive after fees.
//! - [`crank_taker_origin_cross`]: give a migrated taker remainder the
//!   improvement its activation window earned. The crank consumes the crossing
//!   counterparty, lifts the remainder off the book, and settles the pair at
//!   the counterparty's price. The difference pays the cranker. There is no
//!   protocol pass-through here. One side is the aggressor and the improvement
//!   is its own. The cross conditions' resolver discovers this case, and stages
//!   this crank ahead of the arb crank when the top of the book is a crossed
//!   remainder.
//! - [`refill_crank_reservoir`]: refill a market's keeper-payment reservoir
//!   from the protocol crank treasury when its mirrored balance falls to the
//!   watermark.
//! - [`resolve_clob_crank`]: the one simulation-only resolver for every
//!   condition a market's CLOB cranks wake on. Relay passes the condition that
//!   fired, and the resolver stages whichever crank answers it.
//! - [`crank_conditions_setup`]: writes the market's relay condition block.
//!   `update_perp_market_clob_quoter` calls it, so attaching a CLOB also
//!   creates its cranks in the same instruction.
//! - [`initialize_quoter_cross_conditions`]: create or re-price a Custom
//!   quoter's cross-discovery conditions. The instruction is permissionless.
//!
//! The admin endpoints live in `admin`. The market's quoter slab is the
//! book's config authority, so `update_perp_market_clob_book_config` and
//! `resize_perp_market_clob_book` are the only paths that change the book's
//! rules or grow its arena. The config path rewrites the slab's copy of the
//! rules in the same instruction.
//!
//! Shared plumbing lives in [`helpers`]. A file directly in this directory is
//! an endpoint. A file in `helpers/` is not.
//! - [`helpers::placement`]: the placement helpers. They run the margin gate
//!   and the aggregate reserve, then CPI to the CLOB as its `place_authority`,
//!   which is the CLOB place authority PDA. Every route that rests a remainder
//!   calls [`try_place_remainder_on_clob`]. The trigger-limit crank runs its
//!   own placement CPI.
//! - [`helpers::crank_common`]: the cranks' shared dual-mode plumbing. Each
//!   crank lives in its own file with its simulation-only relay resolver, named
//!   `Resolve<EndpointName>`.
//! - Every CPI to the book goes through `ClobMarket` in `state::prop_amm`. One
//!   place in the program speaks the CLOB's wire format: its discriminators,
//!   borsh arguments, `invoke_signed` account pair, and return-data decode.
//!
//! A book order has no `User.orders` slot, so nothing in the order-history
//! stream would name it unless velocity says so. [`helpers::records`] emits the
//! two records that stream already carries. `OrderRecord` marks an order that
//! starts resting, and `OrderActionRecord` marks one that stops. Every path
//! here that places or removes an order emits them. `cancel_orders_v1` is the
//! exception. A sweep takes up to 128 orders and a record is 480 bytes, which
//! no transaction's log budget holds, so its per-order detail goes in the
//! book's own compact cancel record instead.
//!
//! A book order keeps one margin regime for its whole life. The book stores no
//! isolated flag, so every fill, cull and cancel reads the regime off the
//! owner's live [`crate::state::user::PerpPosition`]. That read cannot change
//! its answer while the order rests. A resting order holds `open_orders` and
//! `open_bids`/`open_asks` on the position, so nothing recycles the slot into
//! another market and nothing clears the isolated flag in place.
//!
//! The other direction is refused as well. A cross position cannot turn
//! isolated while its orders rest. The isolated-scope margin gate keeps the
//! isolated collateral above the worst-case requirement, and that requirement
//! counts resting size even at zero base. An order therefore settles against
//! the collateral pool it rested under. No order and no book node has to carry
//! a copy of the flag.

mod admin;
mod cancel_order_v1;
mod cancel_orders_v1;
mod crank_clob_evict;
mod crank_clob_remove_expired;
mod crank_conditions_setup;
mod crank_cross_match;
mod crank_taker_origin_cross;
mod force_cancel_clob_orders;
pub mod helpers;
mod initialize_quoter_cross_conditions;
mod modify_order_v1;
mod place_and_make_v1;
mod place_and_take_v1;
pub mod refill_crank_reservoir;
pub mod resolve_clob_crank;
mod trigger_limit_order_v1;
mod trigger_market_order_v1;

pub use {
    admin::*, cancel_order_v1::*, cancel_orders_v1::*, crank_clob_evict::*,
    crank_clob_remove_expired::*, crank_conditions_setup::*, crank_cross_match::*,
    crank_taker_origin_cross::*, force_cancel_clob_orders::*, helpers::*,
    initialize_quoter_cross_conditions::*, modify_order_v1::*, place_and_make_v1::*,
    place_and_take_v1::*, refill_crank_reservoir::*, resolve_clob_crank::*,
    trigger_limit_order_v1::*, trigger_market_order_v1::*,
};
