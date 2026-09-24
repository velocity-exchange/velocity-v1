//! Tests for the worst-price bound.
//!
//! The SDK mirror `packages/sdk/tests/ci/worstPrice.ts` asserts the same
//! inputs and outputs.

use crate::{
    controller::position::PositionDirection,
    math::{constants::PRICE_PRECISION_I64, worst_price::derive_worst_price},
    state::{oracle::OraclePriceData, perp_market::ContractTier},
};

fn oracle(price: i64) -> OraclePriceData {
    OraclePriceData {
        price,
        ..OraclePriceData::default()
    }
}

fn unnamed_bounds(oracle_price: i64, tier: ContractTier) -> (u64, u64) {
    let oracle = oracle(oracle_price);
    let long = derive_worst_price(&oracle, tier, PositionDirection::Long, 0).unwrap();
    let short = derive_worst_price(&oracle, tier, PositionDirection::Short, 0).unwrap();
    (long, short)
}

#[test]
fn a_named_price_is_the_cap_however_far_from_the_oracle() {
    let oracle = oracle(100 * PRICE_PRECISION_I64);
    let tier = ContractTier::A;
    let long = derive_worst_price(&oracle, tier, PositionDirection::Long, 105_000_000).unwrap();
    let short = derive_worst_price(&oracle, tier, PositionDirection::Short, 90_000_000).unwrap();

    assert_eq!(long, 105_000_000);
    assert_eq!(short, 90_000_000);
}

#[test]
fn an_unnamed_price_takes_the_tier_bound_from_the_oracle() {
    let oracle_price = 100 * PRICE_PRECISION_I64;
    let expected = [
        (ContractTier::A, (102_000_000, 98_000_000)),
        (ContractTier::B, (105_000_000, 95_000_000)),
        (ContractTier::C, (105_000_000, 95_000_000)),
        (ContractTier::Speculative, (110_000_000, 90_000_000)),
        (ContractTier::HighlySpeculative, (120_000_000, 80_000_000)),
        (ContractTier::Isolated, (120_000_000, 80_000_000)),
    ];

    for (tier, bounds) in expected {
        assert_eq!(unnamed_bounds(oracle_price, tier), bounds, "{:?}", tier);
    }
}

/// The slippage rounds toward the oracle, so the bound never reaches past
/// the tier's fraction.
#[test]
fn an_unnamed_price_rounds_the_slippage_down() {
    assert_eq!(
        unnamed_bounds(123_456_789, ContractTier::A),
        (125_925_924, 120_987_654)
    );
    assert_eq!(
        unnamed_bounds(33_333, ContractTier::Speculative),
        (36_666, 30_000)
    );
}
