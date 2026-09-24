//! What removal work the book has, answered by the book.
//!
//! Expiry and eviction are the book's own business. A quoter with no resting
//! orders has neither. The consequences of a removal belong to the caller: a
//! maker's margin reservation, the reward the removal pays, and a trigger slot
//! that follows the order. The caller does the removing, and asks here which
//! order to remove.
//!
//! The caller used to read the answer out of the market account's bytes. That
//! meant knowing where a node keeps its expiry and how the free list is
//! threaded, so a book that changed its data structures broke a program that
//! never called into it. The book can answer the question itself, which lets
//! the arena stay the book's own.
//!
//! Read-only. A caller simulates this to find work, then sends the removal it
//! names.

/// Declared by `clob-wire`.
pub use clob_wire::{ClobRemovalKindV0, NextRemovalArgsV0, OrderViewV0};
use {
    crate::{
        book::{evictable_order, ClobBook, NodeArena},
        instructions::MarketViewV0,
        state::{ClobMarketV0, OrderBitFlag, SideV0, NIL},
    },
    anchor_lang::prelude::*,
};

/// The next order of `kind` this book would let a caller remove, or
/// [`OrderViewV0::NONE`].
pub fn handle_next_removal_v0(
    ctx: &mut Context<MarketViewV0>,
    args: NextRemovalArgsV0,
) -> Result<OrderViewV0> {
    let clock = Clock::get()?;
    let market = &ctx.accounts.market;
    match args.kind {
        ClobRemovalKindV0::Expired => expired(market, clock.unix_timestamp),
        ClobRemovalKindV0::Evictable => evictable(market, clock.slot),
    }
}

/// The first live order past its expiry.
///
/// This walks the arena rather than a side, since expiry has no book
/// ordering and keeping one would cost every placement. The walk reads the
/// book's own memory only in simulation, so the cost never lands on chain.
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

/// The order `evict_worst_v0` would take on a side that has reached the
/// eviction threshold. When both sides have reached it, the fuller side is
/// relieved first. A side whose every order is a bound remainder is passed
/// over, the same way [`evictable_order`] passes over each such order.
///
/// The threshold and the choice of side are the book's policy and stay here. A
/// caller that had to know them would re-decide, from numbers it read out of
/// the header, what the book already decides for itself.
fn evictable(market: &ClobMarketV0, slot: u64) -> Result<OrderViewV0> {
    let preference = if market.node_count(SideV0::Ask) > market.node_count(SideV0::Bid) {
        [SideV0::Ask, SideV0::Bid]
    } else {
        [SideV0::Bid, SideV0::Ask]
    };

    let over_threshold = preference.into_iter().filter(|side| {
        let count = market.node_count(*side);
        count > 0 && count >= market.evict_threshold_per_side
    });
    for side in over_threshold {
        let index = evictable_order(market, side, slot)?;
        if index != NIL {
            return Ok(crate::state::order_view(&market.read_node(index)?, index));
        }
    }

    Ok(OrderViewV0::NONE)
}
