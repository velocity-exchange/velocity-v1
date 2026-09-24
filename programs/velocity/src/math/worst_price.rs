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
//!
//! A market order that names no price, and a fired stop-market, take the
//! widest bound the market's contract tier gave a market order auction's end
//! price. The tier sets how far a price can gap on that market.

use crate::{
    controller::position::PositionDirection,
    error::VelocityResult,
    math::{casting::Cast, safe_math::SafeMath},
    state::{oracle::OraclePriceData, perp_market::ContractTier},
};

#[cfg(test)]
mod tests;

/// The worst price to stamp on an order, from the price its sender named.
///
/// A named price is the order's cap, and the sender chooses how far from the
/// oracle it sits. A sender that names no price gets the oracle moved away from
/// the taker by `oracle / unnamed_price_slippage_divisor(contract_tier)`.
pub fn derive_worst_price(
    oracle_price_data: &OraclePriceData,
    contract_tier: ContractTier,
    direction: PositionDirection,
    named_price: u64,
) -> VelocityResult<u64> {
    if named_price > 0 {
        return Ok(named_price);
    }

    let oracle_price = oracle_price_data.price;
    let slippage = oracle_price.safe_div(unnamed_price_slippage_divisor(contract_tier))?;
    let bound = match direction {
        PositionDirection::Long => oracle_price.safe_add(slippage)?,
        PositionDirection::Short => oracle_price.safe_sub(slippage)?,
    };

    bound.max(0).cast::<u64>()
}

/// The oracle over the slippage an unnamed price takes on a tier: 2 percent for
/// A, 5 for B and C, 10 for Speculative, 20 for HighlySpeculative and Isolated.
/// These are the maximum divisors of `PerpMarket::get_auction_end_min_max_divisors`.
pub fn unnamed_price_slippage_divisor(contract_tier: ContractTier) -> i64 {
    match contract_tier {
        ContractTier::A => 50,
        ContractTier::B | ContractTier::C => 20,
        ContractTier::Speculative => 10,
        ContractTier::HighlySpeculative | ContractTier::Isolated => 5,
    }
}
