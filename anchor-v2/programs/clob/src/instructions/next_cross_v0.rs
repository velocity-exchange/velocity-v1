//! Whether the book crosses itself, answered by the book.
//!
//! A book can cross itself. Makers post both sides, and an unfilled taker
//! remainder that the caller migrated on rests like any other order. Neither
//! the book nor the caller matches on its own. The book holds no funds and the
//! caller holds no arena. The caller has to see the top of both sides to decide
//! what to send.
//!
//! The caller used to read that from the market account: the side heads, then a
//! walk down the linked list to skip whatever was not matchable yet. That put
//! the node layout and the activation and expiry rules in a program that never
//! called into the book. The book can answer both questions itself, which lets
//! the arena stay the book's own.
//!
//! Read-only. A caller simulates this to find work, then sends the match it
//! names.

use {
    crate::{
        book::{is_live, ClobBook, CrossReservation, NodeArena, NIL},
        state::{ClobMarketV0, Side},
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
/// A speed-bumped order is skipped rather than treated as a stop. The depth
/// behind it is matchable now, and skipping it excludes no taker. The caller
/// supplies a taker only when it sends the match.
///
/// An order that a crossing taker remainder claims whole is also skipped.
/// Only the crank settling that remainder may take it. This walk still
/// returns the crossing order itself, since hiding it would hide the cross.
fn head(market: &ClobMarketV0, side: Side, slot: u64, now: i64) -> OrderViewV0 {
    let mut reservation = CrossReservation::new(market, side, slot, now, false);
    let mut cursor = market.best(side);
    while cursor != NIL {
        let Ok(node) = market.read_node(cursor) else {
            return OrderViewV0::NONE;
        };

        if is_live(&node, slot, now) {
            let Ok(claimed) = reservation.claimed(market, &node) else {
                return OrderViewV0::NONE;
            };

            if claimed < node.base_asset_amount {
                return crate::state::order_view(&node, cursor);
            }
        }

        cursor = node.next;
    }

    OrderViewV0::NONE
}
