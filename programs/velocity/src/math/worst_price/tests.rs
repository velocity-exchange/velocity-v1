//! Tests for the worst-price bound.

use crate::{
    controller::position::PositionDirection,
    math::{constants::PRICE_PRECISION_I64, worst_price::derive_worst_price},
    state::oracle::OraclePriceData,
};

fn oracle(price: i64) -> OraclePriceData {
    OraclePriceData {
        price,
        ..OraclePriceData::default()
    }
}

#[test]
fn a_named_price_is_the_cap_however_far_from_the_oracle() {
    let oracle = oracle(100 * PRICE_PRECISION_I64);
    let long = derive_worst_price(&oracle, PositionDirection::Long, 105_000_000).unwrap();
    let short = derive_worst_price(&oracle, PositionDirection::Short, 90_000_000).unwrap();

    assert_eq!(long, 105_000_000);
    assert_eq!(short, 90_000_000);
}

#[test]
fn an_unnamed_price_takes_the_default_slippage_from_the_oracle() {
    let oracle = oracle(100 * PRICE_PRECISION_I64);
    let long = derive_worst_price(&oracle, PositionDirection::Long, 0).unwrap();
    let short = derive_worst_price(&oracle, PositionDirection::Short, 0).unwrap();

    assert_eq!(long, 100_500_000);
    assert_eq!(short, 99_500_000);
}
