//! The worst price a taker accepts.
//!
//! An order names one price it will not fill past, and every fill is held to
//! it. `Order::price` holds it. An order that states the price as an offset
//! from the oracle holds the offset in `Order::oracle_price_offset` instead,
//! and resolves it at fill time.
//!
//! The price does not move while the order waits. A remainder that waits for a
//! counterparty waits on the book at one price, and `activation_delay_slots`
//! sets how long.

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
/// oracle it sits. A sender that names no price gets the oracle moved away from
/// the taker by `oracle / DEFAULT_MARKET_ORDER_SLIPPAGE_FRACTION`.
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
