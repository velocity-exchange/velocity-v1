//! Order records for orders that rest on a CLOB.
//!
//! A book order has no `User.orders` slot, so nothing in the order-history
//! stream would name it unless velocity says so. These helpers emit the two
//! records that stream already carries. `OrderRecord` marks an order that
//! starts resting. `OrderActionRecord` with `OrderAction::Cancel` marks one
//! that stops. Both are built from an `Order` value made out of the placement.
//!
//! The `Order` is synthesized, not stored. Every field in it is a fact about
//! the placement that velocity already holds, and the record is the only reader
//! of it. Three fields carry meaning worth stating.
//!
//! - `order_id` is the id velocity minted from `User.next_order_id`, the same
//!   counter a slot order draws from. An order's records therefore name it the
//!   same way wherever the order rests.
//! - `post_only` is true for an ordinary book order. A resting CLOB order
//!   settles at its own price on the maker fee schedule in every path that can
//!   consume it. A migrated taker remainder is the exception, because it is the
//!   aggressor in a cross, so it reports false.
//! - `bit_flags` carries `OrderBitFlag::PlacedOnClob`, which is how a reader
//!   tells a book order from a slot order in the same id space. It also carries
//!   `OrderBitFlag::IsIsolatedPosition` when the order belongs to an isolated
//!   position, the same as a slot order does. A reader therefore learns an
//!   order's margin regime from the record that opens it rather than from the
//!   one that closes it.

use {
    crate::{
        controller::position::PositionDirection,
        error::VelocityResult,
        math::orders::set_order_bit_flag,
        state::{
            events::{
                emit_stack, get_order_action_record, OrderAction, OrderActionExplanation,
                OrderActionRecord, OrderRecord,
            },
            traits::Size,
            user::{MarketType, Order, OrderBitFlag, OrderStatus, OrderType},
        },
    },
    anchor_lang::prelude::*,
};

/// One resting book order, in the shape the records speak.
///
/// `base_asset_amount` is the order's size as placed.
/// `base_asset_amount_filled` is how much of it filled since. A reader needs
/// both to tell a cancelled order from a completed one.
#[derive(Clone, Copy, Debug)]
pub struct ClobOrderFacts {
    pub order_id: u32,
    pub market_index: u16,
    pub direction: PositionDirection,
    pub price: u64,
    pub base_asset_amount: u64,
    pub base_asset_amount_filled: u64,
    pub max_ts: i64,
    pub slot: u64,
    /// A migrated taker remainder rather than a quote its owner chose to post.
    pub taker_origin: bool,
}

impl ClobOrderFacts {
    /// The facts of an order the book removed, as the removal reported them.
    /// A removal report carries the order's remaining size, so
    /// `base_asset_amount_filled` is zero. The record describes what left the
    /// book rather than the order's fill history.
    pub fn from_removed(
        removed: &crate::state::prop_amm::ClobRemovedOrderV0,
        market_index: u16,
        slot: u64,
    ) -> Self {
        Self {
            order_id: removed.client_order_id,
            market_index,
            direction: PositionDirection::from(removed.side),
            price: removed.price,
            base_asset_amount: removed.base_asset_amount,
            base_asset_amount_filled: 0,
            max_ts: removed.max_ts,
            slot,
            taker_origin: removed.taker_origin,
        }
    }

    /// `is_isolated_position` is the owning position's margin regime. It is
    /// fixed for the order's whole life. A resting order holds `open_orders`
    /// and `open_bids`/`open_asks` on that position, so nothing recycles the
    /// slot into a cross position under it.
    fn to_order(self, status: OrderStatus, is_isolated_position: bool) -> Order {
        Order {
            slot: self.slot,
            price: self.price,
            base_asset_amount: self.base_asset_amount,
            base_asset_amount_filled: self.base_asset_amount_filled,
            max_ts: self.max_ts,
            order_id: self.order_id,
            market_index: self.market_index,
            status,
            order_type: OrderType::Limit,
            market_type: MarketType::Perp,
            direction: self.direction,
            post_only: !self.taker_origin,
            bit_flags: set_order_bit_flag(
                OrderBitFlag::PlacedOnClob as u8,
                is_isolated_position,
                OrderBitFlag::IsIsolatedPosition,
            ),
            ..Order::default()
        }
    }
}

/// Record an order that started resting on a book.
///
/// Every path that places an order emits this record. That covers the direct
/// placement, a modify's replacement, a triggered stop, and a taker remainder
/// that migrates onto the book. A reader sees one event in all four cases. The
/// order is now open at this price for this size.
pub fn emit_clob_place_record(
    now: i64,
    user_key: &Pubkey,
    facts: ClobOrderFacts,
    is_isolated_position: bool,
) -> VelocityResult {
    emit_stack::<_, { OrderRecord::SIZE }>(OrderRecord {
        ts: now,
        user: *user_key,
        order: facts.to_order(OrderStatus::Open, is_isolated_position),
    })
}

/// Record an order that stopped resting on a book.
///
/// `explanation` says what removed it. The causes are the owner asking, an
/// eviction, an expiry, a force-cancel, and a fill that leaves a remainder too
/// small to rest. The order's `status` is `Canceled` for all of them. The fill
/// reports a book order that filled to nothing, so that case does not reach
/// here.
pub fn emit_clob_cancel_record(
    now: i64,
    oracle_price: i64,
    user_key: &Pubkey,
    facts: ClobOrderFacts,
    explanation: OrderActionExplanation,
    filler: Option<Pubkey>,
    filler_reward: Option<u64>,
    is_isolated_position: bool,
) -> VelocityResult {
    let order = facts.to_order(OrderStatus::Canceled, is_isolated_position);
    let bit_flags = set_order_bit_flag(0, is_isolated_position, OrderBitFlag::IsIsolatedPosition);
    // A book order fills the maker half of the record, since it is standing
    // liquidity and a reader that filed it as a taker would count it against
    // taker-side volume it never took. A migrated remainder is the aggressor,
    // so it fills the taker half.
    let (taker, taker_order, maker, maker_order) = if order.post_only {
        (None, None, Some(*user_key), Some(order))
    } else {
        (Some(*user_key), Some(order), None, None)
    };
    let record = get_order_action_record(
        now,
        OrderAction::Cancel,
        explanation,
        facts.market_index,
        filler,
        None,
        filler_reward,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        taker,
        taker_order,
        maker,
        maker_order,
        oracle_price,
        bit_flags,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )?;

    emit_stack::<_, { OrderActionRecord::SIZE }>(record)
}
