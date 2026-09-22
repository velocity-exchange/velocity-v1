//! The worst price a taker accepts.
//!
//! An order names one price it will not fill past, and every fill is held to
//! it. `Order::price` holds it. An order that states the price as an offset
//! from the oracle holds the offset in `Order::oracle_price_offset` instead,
//! and resolves it at fill time.
//!
//! This replaced a price that ramped from a start to an end over a duration.
//! The ramp existed because an order rested and competing fillers watched it
//! cross their price. An order routes to the book now, so the only thing the
//! ramp still did was make the fill price depend on how long the sender took
//! to land the transaction. A remainder that waits for a counterparty waits on
//! the book at one price, and `activation_delay_slots` sets how long it waits.

use crate::{
    controller::position::PositionDirection,
    error::VelocityResult,
    math::{casting::Cast, constants::DEFAULT_MARKET_ORDER_SLIPPAGE_FRACTION, safe_math::SafeMath},
    state::oracle::OraclePriceData,
};

#[cfg(test)]
mod tests;

/// The worst price to stamp on an order, from the price its sender named.
///
/// A named price is the order's cap, and the sender chooses how far from the
/// oracle it sits. A sender that names no price takes
/// [`DEFAULT_MARKET_ORDER_SLIPPAGE_FRACTION`] of the oracle as the cap.
pub fn derive_worst_price(
    oracle_price_data: &OraclePriceData,
    direction: PositionDirection,
    named_price: u64,
) -> VelocityResult<u64> {
    if named_price > 0 {
        return Ok(named_price);
    }

    let oracle_price = oracle_price_data.price;
    let slippage = oracle_price.safe_div(DEFAULT_MARKET_ORDER_SLIPPAGE_FRACTION)?;
    let bound = match direction {
        PositionDirection::Long => oracle_price.safe_add(slippage)?,
        PositionDirection::Short => oracle_price.safe_sub(slippage)?,
    };

    bound.max(0).cast::<u64>()
}
