//! Order records for orders that rest on a CLOB.
//!
//! A book order has no `User.orders` slot, so nothing in the order-history
//! stream would name it unless velocity says so. These helpers emit the two
//! records that stream already carries — `OrderRecord` when an order starts
//! resting, `OrderActionRecord` with `OrderAction::Cancel` when it stops —
//! against an `Order` value built from the placement.
//!
//! The `Order` is synthesized, not stored. Every field in it is a fact about
//! the placement velocity already holds, and the record is the only thing that
//! reads it. Three of them carry meaning worth stating:
//!
//! - `order_id` is the id velocity minted from `User.next_order_id`, the same
//!   counter its DLOB orders draw from. That is what makes an order's records
//!   name it the same way wherever the order rests.
//! - `post_only` is true for an ordinary book order, because that is what it
//!   is: a resting CLOB order settles at its own price on the maker fee
//!   schedule in every path that can consume it. A migrated taker remainder is
//!   the exception — it is the aggressor in a cross — and so reports false.
//! - `bit_flags` carries `OrderBitFlag::PlacedOnClob`, which is how a reader
//!   tells a book order from a DLOB order carrying the same id space.

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
/// `base_asset_amount` is the order's size as placed and
/// `base_asset_amount_filled` what it has given up since — the pair a reader
/// needs to tell a cancelled order from a completed one.
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
    fn to_order(self, status: OrderStatus) -> Order {
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
            bit_flags: OrderBitFlag::PlacedOnClob as u8,
            ..Order::default()
        }
    }
}

/// Record an order that started resting on a book.
///
/// Emitted by every path that places one — the direct placement, a modify's
/// replacement, a triggered stop, and a taker remainder migrating onto the
/// book — because to a reader they are all the same event: this order is now
/// open at this price for this size.
pub fn emit_clob_place_record(
    now: i64,
    user_key: &Pubkey,
    facts: ClobOrderFacts,
) -> VelocityResult {
    emit_stack::<_, { OrderRecord::SIZE }>(OrderRecord {
        ts: now,
        user: *user_key,
        order: facts.to_order(OrderStatus::Open),
    })
}

/// Record an order that stopped resting on a book.
///
/// `explanation` is what took it off: the owner asking, an eviction, an
/// expiry, a force-cancel, or a fill leaving a remainder too small to rest.
/// The order's `status` is `Canceled` for all of them — a book order that
/// filled to nothing is reported by the fill, not here.
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
    let order = facts.to_order(OrderStatus::Canceled);
    let bit_flags = set_order_bit_flag(0, is_isolated_position, OrderBitFlag::IsIsolatedPosition);
    // A book order is the maker half of the record for the reason it reports
    // `post_only`: it is standing liquidity, and a reader that filed it as a
    // taker would count it against the taker-side volume of a market it never
    // took from. A migrated remainder is the taker half, being the aggressor.
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
