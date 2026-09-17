use std::cmp::min;

use crate::{
    is_one_of_variant,
    types::{Order, OrderType, PositionDirection},
};
use program::math::time::{Millis, SlotClock};

/// Auction interpolation progress, as elapsed wall clock milliseconds over the
/// auction's wall clock length. `auction_duration` is stored in 400ms units.
/// Mirrors the program's `auction_progress`.
fn auction_progress(order: &Order, slot: u64, slot_clock: SlotClock) -> (i128, i128) {
    let duration_ms = Millis::from_stored_units(order.auction_duration as u64).as_ms();
    let elapsed_ms = slot_clock.elapsed(order.slot, slot).as_ms();
    (min(elapsed_ms, duration_ms) as i128, duration_ms as i128)
}

pub fn is_auction_complete(order: &Order, slot: u64, slot_clock: SlotClock) -> bool {
    if order.auction_duration == 0 {
        return true;
    }

    slot_clock.elapsed(order.slot, slot) > Millis::from_stored_units(order.auction_duration as u64)
}

#[track_caller]
pub fn get_auction_price(order: &Order, slot: u64, price: i64, slot_clock: SlotClock) -> i128 {
    if is_one_of_variant(
        &order.order_type,
        &[
            OrderType::Market,
            OrderType::TriggerMarket,
            OrderType::Limit,
            OrderType::TriggerLimit,
        ],
    ) {
        get_auction_price_for_fixed_auction(order, slot, slot_clock)
    } else if order.order_type == OrderType::Oracle {
        get_auction_price_for_oracle_offset_auction(order, slot, price, slot_clock)
    } else {
        panic!("Invalid order type")
    }
}

fn get_auction_price_for_fixed_auction(order: &Order, slot: u64, slot_clock: SlotClock) -> i128 {
    let auction_start_price = order.auction_start_price as i128;
    let auction_end_price = order.auction_end_price as i128;
    let (delta_numerator, delta_denominator) = auction_progress(order, slot, slot_clock);

    if delta_denominator == 0 {
        return auction_start_price;
    }

    match order.direction {
        PositionDirection::Long => {
            let price_delta =
                auction_end_price - auction_start_price * delta_numerator / delta_denominator;
            auction_start_price + price_delta
        }
        PositionDirection::Short => {
            let price_delta =
                auction_start_price - auction_end_price * delta_numerator / delta_denominator;
            auction_start_price - price_delta
        }
    }
}

fn get_auction_price_for_oracle_offset_auction(
    order: &Order,
    slot: u64,
    oracle_price: i64,
    slot_clock: SlotClock,
) -> i128 {
    let auction_start_price = order.auction_start_price as i128;
    let auction_end_price = order.auction_end_price as i128;
    let (delta_numerator, delta_denominator) = auction_progress(order, slot, slot_clock);

    if delta_denominator == 0 {
        return auction_start_price;
    }

    let price_offset = match order.direction {
        PositionDirection::Long => {
            let price_delta =
                auction_end_price - auction_start_price * delta_numerator / delta_denominator;
            auction_start_price + price_delta
        }
        PositionDirection::Short => {
            let price_delta =
                auction_start_price - auction_end_price * delta_numerator / delta_denominator;
            auction_start_price - price_delta
        }
    };

    oracle_price as i128 + price_offset
}
