//! What removal work the book has, answered by the book.
//!
//! Expiry and eviction are the book's own business — a quoter with no resting
//! orders has neither — but a removal's *consequences* belong to the caller:
//! a maker's margin reservation, the reward that removal pays, a trigger slot
//! that follows the order. So the caller does the removing, and asks here
//! which order to remove.
//!
//! Before this existed the caller read the answer out of the market account's
//! bytes, which meant knowing where a node keeps its expiry and how the free
//! list is threaded — a book that changed its data structures broke a program
//! that never called into it. Reading is a question the book can answer, and
//! answering it here is what lets the arena stay the book's own.
//!
//! Read-only, and meant to be simulated: a caller runs this to find work, then
//! sends the removal it names.

use {
    crate::{
        book::{ClobBook, NodeArena},
        state::{ClobMarketV0, OrderBitFlag, Side},
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct NextRemovalV0Accounts {
    pub market: ClobMarketV0,
}

/// Declared by `clob-wire`.
pub use clob_wire::{ClobRemovalKindV0, NextRemovalArgsV0, OrderViewV0};

/// The next order of `kind` this book would let a caller remove, or
/// [`OrderViewV0::NONE`].
pub fn handle_next_removal_v0(
    ctx: &mut Context<NextRemovalV0Accounts>,
    args: NextRemovalArgsV0,
) -> Result<OrderViewV0> {
    let clock = Clock::get()?;
    let market = &ctx.accounts.market;
    match args.kind {
        ClobRemovalKindV0::Expired => expired(market, clock.unix_timestamp),
        ClobRemovalKindV0::Evictable => Ok(evictable(market)),
    }
}

/// The first live order past its expiry.
///
/// A walk of the arena rather than of a side: expiry has no ordering on the
/// book, and the alternative — keeping one — would cost every placement to
/// serve a crank. The walk is the book's own memory and this call is
/// simulated, so it is paid in a place nothing lands.
fn expired(market: &ClobMarketV0, now: i64) -> Result<OrderViewV0> {
    for index in 0..market.len() as u32 {
        let node = market.read_node(index)?;
        if !node.is_bit_flag_set(OrderBitFlag::Open) || node.max_ts == 0 || node.max_ts > now {
            continue;
        }
        return Ok(crate::state::order_view(&node, index));
    }
    Ok(OrderViewV0::NONE)
}

/// The worst-priced order on the side that has reached the eviction threshold,
/// relieving the fuller side first when both have.
///
/// The threshold and the choice of side are the book's policy and stay here: a
/// caller that had to know them would be re-deciding, against numbers it read
/// out of the header, what the book already decides for itself.
fn evictable(market: &ClobMarketV0) -> OrderViewV0 {
    let bids = market.node_count(Side::Bid);
    let asks = market.node_count(Side::Ask);
    let threshold = market.evict_threshold_per_side;
    let side = match (bids >= threshold, asks >= threshold) {
        (true, true) if asks > bids => Side::Ask,
        (true, _) => Side::Bid,
        (_, true) => Side::Ask,
        _ => return OrderViewV0::NONE,
    };
    let worst = market.worst(side);
    market
        .read_node(worst)
        .ok()
        .filter(|node| node.is_bit_flag_set(OrderBitFlag::Open))
        .map_or(OrderViewV0::NONE, |node| {
            crate::state::order_view(&node, worst)
        })
}
