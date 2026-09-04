//! Velocity-mediated CLOB order lifecycle. Plain resting limits live on a
//! registered CLOB program, not in `User.orders` — but every path that
//! changes a maker's worst-case exposure runs through velocity so the
//! open-order aggregates (`open_bids`/`open_asks`, the open-order counters)
//! that back the margin model stay exact. Fills and culls unwind through the
//! router fill's execute response; everything else unwinds here.
//!
//! The user routes. A v1 live order is ephemeral — built on the stack,
//! margin-checked, and never written into `User.orders`:
//! - [`place_and_make_v1`]: the maker route — a post-only limit rests
//!   straight on the book as a maker quote. It names no taker and matches
//!   nothing on placement.
//! - [`place_and_take_v1`]: the taker route — fill through the router across
//!   the vAMM, the quoter books, and the passed DLOB makers, then rest the
//!   restable remainder on the book taker-origin. The v0 instruction's
//!   account list is frozen, hence the new endpoint.
//! - [`cancel_order_v1`]: cancel CPI, then unwind the removed order's
//!   remaining size from the aggregates.
//! - [`cancel_orders_v1`]: the same thing for a maker's whole side (or
//!   both) in one CPI, unwinding from per-side totals so the cost does not grow
//!   with the ladder. The book caps a single sweep and says so; the handler
//!   unwinds what was actually removed, so repeating it converges.
//! - [`modify_order_v1`]: cancel-and-replace in one instruction, with one
//!   margin gate over the *net* change (the CLOB has no in-place mutation).
//!
//! The keeper endpoints:
//! - [`fill_legacy_dlob_order`]: the keeper fill for live orders in
//!   `User.orders`, which only the legacy endpoints still create — a restable
//!   remainder migrates to the book, so each fill drains the legacy DLOB.
//! - [`trigger_limit_order_v1`]: crank an armed trigger-limit onto the CLOB; the
//!   `User.orders` slot becomes a shadow keeping the trigger params + the
//!   CLOB `OrderRef` (freed on fill/cancel/expiry, re-armed on eviction).
//! - [`trigger_market_order_v1`]: fire an armed trigger-market — fill it
//!   through the router in the same instruction, rest the remainder on the
//!   book taker-origin, and free the slot. The limit and market cranks split
//!   on fire semantics: a fired limit rests whole, a fired market fills
//!   first.
//! - [`force_cancel_clob_orders`]: the CLOB arm of the force-cancel keeper
//!   flow — reclaim a failing account's risk-increasing book orders (and
//!   their placed-trigger shadows) for the flat fee.
//!
//! The cranks, woken by relay conditions:
//! - [`crank_clob_evict`]/[`crank_clob_remove_expired`]: permissionless
//!   keeper wrappers over the CLOB's crank ixs — the maker's `User` rides
//!   along so the returned removal unwinds its aggregates. Dual-mode: a
//!   signed keeper earns the flat reward from the maker as before, or the
//!   protocol-owned `User` is passed as the filler (program-keeper mode) and
//!   the caller takes reservoir lamports instead.
//! - [`crank_cross_match`]: fill two crossed resting sources against each
//!   other with the protocol `User` as the pass-through taker; fires only
//!   when the spread nets positive after fees.
//! - [`crank_taker_origin_cross`]: hand a migrated taker remainder the
//!   improvement its auction window earned it — consume the crossing
//!   counterparty, lift the remainder off the book, and settle the pair at the
//!   counterparty's price, paying the cranker out of the difference. No
//!   protocol pass-through: one side is the aggressor and the improvement is
//!   its own. Discovered by the cross conditions' resolver, which stages this
//!   crank ahead of the arb one when the top of the book is a crossed
//!   remainder.
//! - [`refill_crank_reservoir`]: top a market's keeper-payment reservoir back
//!   up out of the protocol crank treasury when its mirrored balance falls to
//!   the watermark.
//! - [`resolve_clob_crank`]: the one simulation-only resolver for every
//!   condition a market's CLOB cranks wake on — relay hands it the condition
//!   that fired and it stages whichever crank answers it.
//! - [`crank_conditions_setup`]: writes the market's relay condition block —
//!   called by `update_perp_market_clob_quoter`, so attaching a CLOB stands
//!   its cranks up in the same instruction.
//! - [`initialize_quoter_cross_conditions`]: stand up (or re-price) a Custom
//!   quoter's cross-discovery conditions, permissionlessly.
//!
//! Shared plumbing lives in [`helpers`] — a file directly in this directory
//! is an endpoint; a file in `helpers/` is not:
//! - [`helpers::placement`]: the placement helpers — margin gate + aggregate
//!   reserve, then a CPI to the CLOB as its `place_authority` (the CLOB place
//!   authority PDA). Every route that rests a remainder calls
//!   [`try_place_remainder_on_clob`]; the trigger-limit crank runs its own
//!   placement CPI.
//! - [`helpers::crank_common`]: the cranks' shared dual-mode plumbing. Each
//!   crank lives in its own file together with its simulation-only relay
//!   resolver (named `Resolve<EndpointName>`).
//! - Every CPI to the book goes through `ClobMarket` (`state::prop_amm`): one
//!   place in the program speaks the CLOB's wire — its discriminators, borsh
//!   args, `invoke_signed` account pair, and return-data decode.
//!
//! A book order has no `User.orders` slot, so nothing in the order-history
//! stream would name it unless velocity says so. [`helpers::records`] emits the two
//! records that stream already carries — `OrderRecord` when an order starts
//! resting, `OrderActionRecord` when it stops — from every path here that
//! places or removes one. `cancel_orders_v1` is the exception: a sweep
//! takes up to 128 orders and a record is 480 bytes, which no transaction's
//! log budget holds, so its per-order detail rides the book's own compact
//! cancel record instead.

mod admin;
mod cancel_order_v1;
mod cancel_orders_v1;
mod crank_clob_evict;
mod crank_clob_remove_expired;
mod crank_conditions_setup;
mod crank_cross_match;
mod crank_taker_origin_cross;
mod fill_legacy_dlob_order;
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
    crank_taker_origin_cross::*, fill_legacy_dlob_order::*, force_cancel_clob_orders::*,
    helpers::*, initialize_quoter_cross_conditions::*, modify_order_v1::*, place_and_make_v1::*,
    place_and_take_v1::*, refill_crank_reservoir::*, resolve_clob_crank::*,
    trigger_limit_order_v1::*, trigger_market_order_v1::*,
};
