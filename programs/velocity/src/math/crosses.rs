//! Which crossed orders on a book match each other, at whose price, and who is
//! entitled to the difference.
//!
//! A book does not resolve its own crosses. Every order fills at its own stored
//! price. The choice between the two crossing prices is an economic question
//! the book has no answer for. So a crossed book stays crossed until somebody
//! reads both sides and says what happens. This module is that reading.
//!
//! The flags on the two orders decide between three outcomes.
//!
//! - One side is a taker remainder. It rests at the worst price its signer
//!   agreed to tolerate, and it demands liquidity rather than offering it. So
//!   it aggresses, the maker's price stands, and the difference is the taker's.
//! - Both sides are taker remainders. Price and time decide. The one that
//!   rested later arrived into a book that already showed the other, so the
//!   earlier price stands and the later order aggresses.
//! - Neither side is a taker remainder. Two makers that cross are unclaimed
//!   arbitrage. Nobody demanded anything, so nobody is owed the spread. The
//!   protocol stands between them as a pass-through taker and keeps the spread.
//!   A floor on the surplus keeps the trade worth making.
//!
//! Taker-origin crosses resolve first. Middling one of those would hand a
//! taker's own improvement to the protocol. Resolving both kinds here in that
//! order removes the need for the arb path to refuse to run while a taker cross
//! is pending.
//!
//! [`crossing_prefix`] answers a second question: how much two ladders cross
//! in price order. The protocol's two-legged cross consumes exactly that.
//!
//! The input is rows and the output is crosses. This module touches no account
//! and settles nothing, so the cases that matter can be tested without a book.
//! Those cases are several remainders that cross at once, a chain where
//! resolving one pair frees the next, and a maker cross behind a taker one.

use crate::state::prop_amm::{
    ClobOrderRefV0, SideV0, UserRefV0, L3_ROW_FLAG_REDUCE_ONLY, L3_ROW_FLAG_TAKER_ORIGIN,
};

#[cfg(test)]
mod tests;

/// Slots an order must rest before a crank may mark its flow as having served a protection window.
/// The flag is `taker_served_window` on the quoter wire. On a book whose default activation delay
/// is zero, resting through placement is a zero-length window. A caller could then place a crossing
/// order, crank the cross in the next transaction, and carry the protected flag on fresh informed
/// flow. Two slots is above the swift hold, so the crank path never vouches for less protection
/// than the attested path does.
///
/// allow-verbose: the two-slot figure is a derivation from the activation delay and the swift hold.
pub const SERVED_WINDOW_MIN_SLOTS: u64 = 2;

/// Whether an order placed at `placed_slot` has rested long enough that a
/// crank may mark its flow as protected.
pub fn served_window(placed_slot: u64, slot: u64) -> bool {
    slot.saturating_sub(placed_slot) >= SERVED_WINDOW_MIN_SLOTS
}

/// One resting order, reduced to what matching needs.
///
/// `order_id` also states rest time. A book hands out ids from a counter that
/// only increases and never reuses an id, so a lower id rested earlier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestingOrder {
    pub order_ref: ClobOrderRefV0,
    pub user: UserRefV0,
    pub price: u64,
    pub base_asset_amount: u64,
    pub taker_origin: bool,
    /// The order fills only up to its owner's position in the reduce direction.
    /// A cross that settles it must bind the fill to the owner's cover and stop
    /// tracking the owner's reduce-only exposure once it leaves the book.
    pub reduce_only: bool,
    /// The slot the order was placed in. The order id decides rest order, not
    /// this field. This field decides whether a crank may mark the order's flow
    /// as protected.
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

/// How a crossed pair settles. Only one thing decides it: which side, if
/// either, demanded liquidity.
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
    pub fn aggressor_side(&self) -> Option<SideV0> {
        match self {
            Self::BidAggresses => Some(SideV0::Bid),
            Self::AskAggresses => Some(SideV0::Ask),
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
    /// The price a cross with an aggressor settles at. A protocol-middled cross
    /// has no single price, because each leg fills at its own.
    #[cfg(test)]
    pub fn settlement_price(&self) -> Option<u64> {
        match self.kind {
            CrossKind::BidAggresses => Some(self.ask.price),
            CrossKind::AskAggresses => Some(self.bid.price),
            CrossKind::ProtocolMiddles => None,
        }
    }
}

/// Every cross on one book, in the order they must be settled.
///
/// The walk reads both sides as a whole rather than pair by pair, because the
/// pairs are not independent. Resolving the two best-priced orders can leave a
/// third still crossing a fourth, and stopping at the first pair would need a
/// fresh crank for each pair. Each cross consumes size, so an order that fills
/// two counterparties appears twice and never over-fills.
///
/// A self-cross is skipped on the owning authority rather than the full user
/// ref. See [`same_authority`].
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
            // take. Among taker crosses the latest to rest goes first. Its
            // improvement is the one at stake, and settling it can free a pair
            // behind it.
            let rank = (
                kind.is_taker_origin(),
                match kind {
                    CrossKind::BidAggresses => bid.order_ref.order_id,
                    CrossKind::AskAggresses => ask.order_ref.order_id,
                    // The flag above orders these against the taker crosses.
                    // Among maker pairs the deepest cross goes first.
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

/// Whether a bid and an ask belong to one authority. Every cross walk refuses
/// such a pair. The cranker is paid out of what a cross produces, and one
/// authority that rests both sides across two sub-accounts could otherwise
/// manufacture a cross and collect for it.
pub fn same_authority(bid: &UserRefV0, ask: &UserRefV0) -> bool {
    bid.authority == ask.authority
}

/// Whether these two cross at all, and if so how they settle.
fn classify(bid: &RestingOrder, ask: &RestingOrder) -> Option<CrossKind> {
    if bid.base_asset_amount == 0
        || ask.base_asset_amount == 0
        || bid.price < ask.price
        || same_authority(&bid.user, &ask.user)
    {
        return None;
    }

    Some(match (bid.taker_origin, ask.taker_origin) {
        // The later order to rest arrived into a book that already showed the
        // other, so the earlier price stands.
        (true, true) => {
            if bid.order_ref.order_id > ask.order_ref.order_id {
                CrossKind::BidAggresses
            } else {
                CrossKind::AskAggresses
            }
        }

        // One side demanded liquidity. Rest time does not arbitrate here,
        // because a maker quote is passive by construction.
        (true, false) => CrossKind::BidAggresses,
        (false, true) => CrossKind::AskAggresses,
        (false, false) => CrossKind::ProtocolMiddles,
    })
}

/// One price level a crossing-prefix walk consumes. A book row names its
/// owner. A quoter level names the quoter's user, which owns its whole ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CrossLevel {
    pub price: u64,
    pub size: u64,
    pub owner: UserRefV0,
}

impl CrossLevel {
    pub fn from_row(row: &crate::state::prop_amm::L3RowV0) -> Self {
        Self {
            price: row.price,
            size: row.size,
            owner: row.user,
        }
    }
}

/// The crossing prefix of two ladders: the size both legs match, the gross
/// quote of each leg, and the owners it touches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CrossPrefix {
    pub size: u64,
    pub buy_quote: u128,
    pub sell_quote: u128,
    pub makers: Vec<UserRefV0>,
}

impl CrossPrefix {
    /// The surplus after both legs pay the taker fee at `fee_tier`. It is zero
    /// inside the fee gulf. The executor measures the real figure, so this
    /// estimate only keeps obvious losers from being staged.
    pub fn estimated_surplus(&self, fee_tier: &crate::state::state::FeeTier) -> u128 {
        let numerator = u128::from(fee_tier.fee_numerator);
        let denominator = u128::from(fee_tier.fee_denominator).max(1);
        let fees = (self.buy_quote * numerator).div_ceil(denominator)
            + (self.sell_quote * numerator).div_ceil(denominator);
        self.sell_quote
            .saturating_sub(self.buy_quote.saturating_add(fees))
    }

    /// Add `owner` to the owners, or report that the cap leaves no room.
    fn admit(&mut self, owner: UserRefV0, max_makers: usize) -> bool {
        if self.makers.contains(&owner) {
            return true;
        }

        if self.makers.len() == max_makers {
            return false;
        }

        self.makers.push(owner);
        true
    }
}

/// Walk two ladders, best first, for as long as the bid crosses the ask.
///
/// The walk stops before it admits an owner past `max_makers`, so the size
/// covers staged owners only. It also stops at a pair of one authority, see
/// [`same_authority`]. It stops rather than skips, because the executor's legs
/// consume the ladders in price order and would take the skipped depth too.
pub fn crossing_prefix(bids: &[CrossLevel], asks: &[CrossLevel], max_makers: usize) -> CrossPrefix {
    let mut prefix = CrossPrefix::default();
    // Price times base per leg, converted to quote once after the walk. A
    // division per level would understate each leg by up to one quote unit.
    let (mut scaled_buy, mut scaled_sell) = (0u128, 0u128);
    let (mut bid_index, mut ask_index) = (0usize, 0usize);
    let mut bid_remaining = bids.first().map_or(0, |level| level.size);
    let mut ask_remaining = asks.first().map_or(0, |level| level.size);
    while let (Some(bid), Some(ask)) = (bids.get(bid_index), asks.get(ask_index)) {
        if bid.price < ask.price || same_authority(&bid.owner, &ask.owner) {
            break;
        }

        if !prefix.admit(bid.owner, max_makers) || !prefix.admit(ask.owner, max_makers) {
            break;
        }

        let take = bid_remaining.min(ask_remaining);
        prefix.size = prefix.size.saturating_add(take);
        scaled_buy = scaled_buy.saturating_add(u128::from(ask.price) * u128::from(take));
        scaled_sell = scaled_sell.saturating_add(u128::from(bid.price) * u128::from(take));

        bid_remaining -= take;
        ask_remaining -= take;
        if bid_remaining == 0 {
            bid_index += 1;
            bid_remaining = bids.get(bid_index).map_or(0, |level| level.size);
        }

        if ask_remaining == 0 {
            ask_index += 1;
            ask_remaining = asks.get(ask_index).map_or(0, |level| level.size);
        }
    }

    let base_precision = u128::from(crate::math::constants::BASE_PRECISION_U64);
    prefix.buy_quote = scaled_buy / base_precision;
    prefix.sell_quote = scaled_sell / base_precision;
    prefix
}
