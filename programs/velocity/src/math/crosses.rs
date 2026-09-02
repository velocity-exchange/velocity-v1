//! Which crossed orders on a book match each other, at whose price, and who
//! is entitled to the difference.
//!
//! A book declines to resolve its own crosses: every order fills at its own
//! stored price, and deciding *which* of two crossing prices a match settles
//! at is an economic question the book has no answer for. So a crossed book
//! sits there until somebody reads both sides and says what should happen.
//! This is that reading.
//!
//! Three outcomes, and the flags on the two orders decide between them:
//!
//! - **One side is a taker remainder.** It rests at the worst price its signer
//!   agreed to tolerate, and it demands liquidity rather than offering it, so
//!   it aggresses and the maker's price stands. The difference is the taker's.
//! - **Both sides are taker remainders.** Price-time decides: the one that
//!   rested later arrived into a book already showing the other, so the earlier
//!   one's price stands and the later one aggresses.
//! - **Neither is.** Two makers crossing is unclaimed arbitrage — nobody
//!   demanded anything, so nobody is owed the spread. The protocol stands
//!   between them as a pass-through taker and keeps it, floored so the trade is
//!   worth making.
//!
//! Taker-origin crosses resolve first. Middling one of those would hand a
//! taker's own improvement to the protocol, which is why the arb path used to
//! refuse to run while one was pending; resolving both here in the right order
//! removes the need to refuse.
//!
//! Rows in, crosses out. No accounts and no settlement, so the cases that
//! matter — several remainders crossing at once, a chain where resolving one
//! pair frees the next, a maker cross hiding behind a taker one — can be tested
//! without a book.

use crate::state::prop_amm::{
    ClobOrderRefV0, ClobSide, ClobUserRefV0, L3_ROW_FLAG_REDUCE_ONLY, L3_ROW_FLAG_TAKER_ORIGIN,
};

#[cfg(test)]
mod tests;

/// Slots an order must have rested before a crank may mark its flow as
/// having served a protection window (`taker_served_window` on the quoter
/// wire). The cranks cannot vouch by construction alone: on a book whose
/// default activation delay is zero, "rested through placement" is a
/// zero-length window, and a caller could place a crossing order and crank
/// the cross in the next transaction — fresh informed flow wearing the
/// protected flag. Measured age closes that: two slots (~800ms) is above
/// the swift hold, so the crank path never vouches for less protection
/// than the attested path does.
pub const SERVED_WINDOW_MIN_SLOTS: u64 = 2;

/// Whether an order placed at `placed_slot` has rested long enough that a
/// crank may mark its flow as protected.
pub fn served_window(placed_slot: u64, slot: u64) -> bool {
    slot.saturating_sub(placed_slot) >= SERVED_WINDOW_MIN_SLOTS
}

/// One resting order, reduced to what matching needs.
///
/// `order_id` doubles as rest time: a book hands out ids from a counter that
/// only increases and never reuses one, so a lower id rested earlier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestingOrder {
    pub order_ref: ClobOrderRefV0,
    pub user: ClobUserRefV0,
    pub price: u64,
    pub base_asset_amount: u64,
    pub taker_origin: bool,
    /// The order fills only up to its owner's position in the reduce direction.
    /// A cross that settles it must bind the fill to the owner's cover and stop
    /// tracking the owner's reduce-only exposure once it leaves the book.
    pub reduce_only: bool,
    /// Slot it was placed in. Not what orders it — the id does that — but what
    /// prices the work of resolving it.
    pub placed_slot: u64,
}

impl RestingOrder {
    /// Read one out of a `quote_l3_v0` row.
    pub fn from_row(row: &crate::state::prop_amm::L3RowV0) -> Self {
        Self {
            order_ref: ClobOrderRefV0 {
                node_index: row.node_index,
                order_id: row.order_id,
            },
            user: row.user,
            price: row.price,
            base_asset_amount: row.size,
            taker_origin: row.flags & L3_ROW_FLAG_TAKER_ORIGIN != 0,
            reduce_only: row.flags & L3_ROW_FLAG_REDUCE_ONLY != 0,
            placed_slot: row.placed_slot,
        }
    }
}

/// How a crossed pair settles, which is entirely a question of which side (if
/// either) demanded liquidity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrossKind {
    /// The bid demanded liquidity. It aggresses, and settles at the ask's
    /// price.
    BidAggresses,
    /// The ask demanded liquidity. It aggresses, and settles at the bid's
    /// price.
    AskAggresses,
    /// Neither did. The protocol stands between them, each leg fills at its own
    /// price, and the spread is the protocol's for a floored surplus.
    ProtocolMiddles,
}

impl CrossKind {
    /// The side that demanded liquidity, if either did.
    pub fn aggressor_side(&self) -> Option<ClobSide> {
        match self {
            Self::BidAggresses => Some(ClobSide::Bid),
            Self::AskAggresses => Some(ClobSide::Ask),
            Self::ProtocolMiddles => None,
        }
    }

    /// A taker's improvement is not the protocol's to middle, so these resolve
    /// first.
    fn is_taker_origin(&self) -> bool {
        self.aggressor_side().is_some()
    }
}

/// One crossed pair and what settling it comes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cross {
    pub bid: RestingOrder,
    pub ask: RestingOrder,
    pub base_asset_amount: u64,
    pub kind: CrossKind,
}

impl Cross {
    /// The price this settles at, for a cross that has an aggressor. A
    /// protocol-middled cross has no single price — each leg fills at its own.
    pub fn settlement_price(&self) -> Option<u64> {
        match self.kind {
            CrossKind::BidAggresses => Some(self.ask.price),
            CrossKind::AskAggresses => Some(self.bid.price),
            CrossKind::ProtocolMiddles => None,
        }
    }
}

/// Every cross on one book, in the order they should be settled.
///
/// Both sides are walked as a whole rather than pair by pair, because the pairs
/// are not independent: resolving the two best-priced orders can leave a third
/// still crossing a fourth, and stopping at the first pair would need a fresh
/// crank for each. Sizes are consumed as crosses are made, so an order that
/// fills two counterparties appears twice and never over-fills.
///
/// A self-cross is skipped on the owning authority rather than the full user
/// ref: the cranker is paid out of what a cross produces, so one authority
/// resting both sides across two sub-accounts could otherwise manufacture one
/// and collect for it.
pub fn resolve_crosses(
    bids: &[RestingOrder],
    asks: &[RestingOrder],
    max_crosses: usize,
) -> Vec<Cross> {
    let mut bids = bids.to_vec();
    let mut asks = asks.to_vec();
    let mut crosses = Vec::new();

    while crosses.len() < max_crosses {
        let Some((bid_index, ask_index, kind)) = next_cross(&bids, &asks) else {
            break;
        };
        let bid = bids[bid_index];
        let ask = asks[ask_index];
        let base_asset_amount = bid.base_asset_amount.min(ask.base_asset_amount);
        bids[bid_index].base_asset_amount -= base_asset_amount;
        asks[ask_index].base_asset_amount -= base_asset_amount;
        crosses.push(Cross {
            bid,
            ask,
            base_asset_amount,
            kind,
        });
    }
    crosses
}

/// The next pair to settle: a taker-origin cross if there is one, and the best
/// maker pair otherwise.
fn next_cross(bids: &[RestingOrder], asks: &[RestingOrder]) -> Option<(usize, usize, CrossKind)> {
    let mut best: Option<(usize, usize, CrossKind, u64)> = None;
    for (bid_index, bid) in bids.iter().enumerate() {
        for (ask_index, ask) in asks.iter().enumerate() {
            let Some(kind) = classify(bid, ask) else {
                continue;
            };
            // A taker's own improvement outranks arbitrage the protocol would
            // take, and within taker crosses the latest to rest goes first: it
            // is the one whose improvement is at stake, and settling it can
            // free a pair behind it.
            let rank = (
                kind.is_taker_origin(),
                match kind {
                    CrossKind::BidAggresses => bid.order_ref.order_id,
                    CrossKind::AskAggresses => ask.order_ref.order_id,
                    // Ordered against the others only by the flag above; among
                    // maker pairs the heads cross the most, so price decides.
                    CrossKind::ProtocolMiddles => bid.price.saturating_sub(ask.price),
                },
            );
            let score = u64::from(rank.0) << 63 | (rank.1 & (u64::MAX >> 1));
            if best.is_none_or(|(_, _, _, seen)| score > seen) {
                best = Some((bid_index, ask_index, kind, score));
            }
        }
    }
    best.map(|(bid, ask, kind, _)| (bid, ask, kind))
}

/// Whether these two cross at all, and if so how they settle.
fn classify(bid: &RestingOrder, ask: &RestingOrder) -> Option<CrossKind> {
    if bid.base_asset_amount == 0
        || ask.base_asset_amount == 0
        || bid.price < ask.price
        || bid.user.authority == ask.user.authority
    {
        return None;
    }
    Some(match (bid.taker_origin, ask.taker_origin) {
        // Price-time: the later to rest arrived into a book already showing the
        // other, so the earlier one's price stands.
        (true, true) => {
            if bid.order_ref.order_id > ask.order_ref.order_id {
                CrossKind::BidAggresses
            } else {
                CrossKind::AskAggresses
            }
        }
        // One side demanded liquidity; rest time does not arbitrate, because a
        // maker quote is passive by construction.
        (true, false) => CrossKind::BidAggresses,
        (false, true) => CrossKind::AskAggresses,
        (false, false) => CrossKind::ProtocolMiddles,
    })
}
