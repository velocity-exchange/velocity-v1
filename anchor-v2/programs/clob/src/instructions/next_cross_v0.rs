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

/// Declared by `clob-wire`.
pub use clob_wire::{NextCrossV0, OrderViewV0};
use {
    crate::{
        book::{is_live, walk_side_ref, CrossReservation, Walk},
        instructions::MarketViewV0,
        state::{ClobMarketV0, SideV0},
    },
    anchor_lang::prelude::*,
};

/// The best matchable order on each side.
pub fn handle_next_cross_v0(ctx: &mut Context<MarketViewV0>) -> Result<NextCrossV0> {
    let clock = Clock::get()?;
    let market = &ctx.accounts.market;
    Ok(NextCrossV0 {
        bid: head(market, SideV0::Bid, clock.slot, clock.unix_timestamp),
        ask: head(market, SideV0::Ask, clock.slot, clock.unix_timestamp),
    })
}

/// Walk one side from its best price to the first order a match may consume.
///
/// A speed-bumped order is skipped rather than treated as a stop. The depth
/// behind it is matchable now, and skipping it excludes no taker.
///
/// An order a crossing taker remainder claims whole is skipped too, because
/// only the crank settling that remainder may take it. The walk still returns
/// the crossing order itself, since hiding it would hide the cross.
fn head(market: &ClobMarketV0, side: SideV0, slot: u64, now: i64) -> OrderViewV0 {
    let mut reservation = CrossReservation::new(market, side, slot, now, false);
    let mut found = OrderViewV0::NONE;
    let walk = walk_side_ref(market, side, |index, node| {
        if !is_live(node, slot, now) || reservation.claimed(market, node)? >= node.base_asset_amount
        {
            return Ok(Walk::Continue);
        }

        found = crate::state::order_view(node, index);
        Ok(Walk::Stop)
    });

    if walk.is_err() {
        return OrderViewV0::NONE;
    }

    found
}
