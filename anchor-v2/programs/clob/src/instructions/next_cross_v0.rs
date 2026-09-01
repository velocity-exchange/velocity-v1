//! Whether the book crosses itself, answered by the book.
//!
//! A book can cross itself: makers post both sides, and an unfilled taker
//! remainder the caller migrated on rests like any other order. Neither the
//! book nor the caller matches on its own — the book holds no funds and the
//! caller holds no arena — so the caller has to see the top of both sides to
//! decide what to send.
//!
//! It used to see it by reading the market account: the side heads, then a
//! walk down the linked list to skip whatever was not matchable yet. That put
//! the node layout and the activation and expiry rules in a program that never
//! called into the book. Both are questions the book can answer, and answering
//! them here is what lets the arena stay the book's own.
//!
//! Read-only, and meant to be simulated: a caller runs this to find work, then
//! sends the match it names.

use {
    crate::{
        book::{ClobBook, NodeArena, NIL},
        state::{ClobMarketV0, OrderNodeV0, Side},
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct NextCrossV0Accounts {
    pub market: ClobMarketV0,
}

/// Declared by `clob-wire`.
pub use clob_wire::{NextCrossV0, OrderViewV0};

/// The best matchable order on each side.
pub fn handle_next_cross_v0(ctx: &mut Context<NextCrossV0Accounts>) -> Result<NextCrossV0> {
    let clock = Clock::get()?;
    let market = &ctx.accounts.market;
    Ok(NextCrossV0 {
        bid: head(market, Side::Bid, clock.slot, clock.unix_timestamp),
        ask: head(market, Side::Ask, clock.slot, clock.unix_timestamp),
    })
}

/// Walk one side from its best price to the first order a match may consume.
///
/// The walk skips orders that are not matchable yet rather than stopping on
/// them: an order still inside its speed bump sits at a price better than the
/// depth behind it, and that depth is matchable now.
fn head(market: &ClobMarketV0, side: Side, slot: u64, now: i64) -> OrderViewV0 {
    let mut cursor = market.best(side);
    while cursor != NIL {
        let Ok(node) = market.read_node(cursor) else {
            return OrderViewV0::NONE;
        };
        if is_matchable(&node, slot, now) {
            return crate::state::order_view(&node, cursor);
        }
        cursor = node.next;
    }
    OrderViewV0::NONE
}

/// The same predicate quote and execute apply, minus the taker: this call has
/// no taker to exclude, because the caller supplies one only when it sends the
/// match.
fn is_matchable(node: &OrderNodeV0, slot: u64, now: i64) -> bool {
    !node.is_expired(now) && node.is_active(slot)
}
