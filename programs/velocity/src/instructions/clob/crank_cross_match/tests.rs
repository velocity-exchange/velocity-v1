//! What the cross resolver offers `crank_cross_match`, and the rules the
//! crank holds its two legs to.
//!
//! The prefix cases take the book's own `quote_l3_v0` answer as their input,
//! so they say nothing about how the book stores an order; the litesvm crank
//! tests pin the reporting against the real CLOB program. The leg rules take
//! what the router pass reports, so they say nothing about how a leg reached
//! a price.

use {
    super::*,
    crate::state::prop_amm::{L3RowV0, UserRefV0, L3_ROW_FLAG_TAKER_ORIGIN},
};

const UNIT: u64 = crate::math::constants::BASE_PRECISION_U64;
const PRICE: u64 = crate::math::constants::PRICE_PRECISION_U64;

fn user(authority: u8) -> UserRefV0 {
    UserRefV0 {
        authority: Pubkey::new_from_array([authority; 32]),
        sub_account_id: 0,
    }
}

/// One resting order, best-first within its side.
fn maker(authority: u8, price: u64, size: u64) -> L3RowV0 {
    L3RowV0 {
        price,
        size,
        order_id: 1,
        node_index: 1,
        user: user(authority),
        flags: 0,
        _pad: [0; 1],
        placed_slot: 1,
    }
}

/// A migrated taker remainder: the same row, flagged.
fn remainder(authority: u8, price: u64, size: u64) -> L3RowV0 {
    L3RowV0 {
        flags: L3_ROW_FLAG_TAKER_ORIGIN,
        ..maker(authority, price, size)
    }
}

fn find(bids: &[L3RowV0], asks: &[L3RowV0]) -> ClobCross {
    cross_prefix(bids, asks)
}

#[test]
fn the_prefix_is_the_crossed_depth_and_the_makers_it_touches() {
    let cross = find(
        &[maker(1, 101 * PRICE, UNIT), maker(2, 98 * PRICE, UNIT)],
        &[maker(3, 99 * PRICE, UNIT / 2)],
    );

    // Only the 101 bid crosses the 99 ask, and only for the ask's half unit.
    assert_eq!(cross.size, UNIT / 2);
    assert_eq!(cross.buy_quote, 49_500_000);
    assert_eq!(cross.sell_quote, 50_500_000);
    assert_eq!(cross.makers, vec![user(1), user(3)]);
    // Nothing crossed at all is no work.
    assert_eq!(find(&[maker(1, 99 * PRICE, UNIT)], &[]).size, 0);
    assert_eq!(
        find(
            &[maker(1, 99 * PRICE, UNIT)],
            &[maker(2, 101 * PRICE, UNIT)]
        )
        .size,
        0
    );
}

/// A crossed taker remainder is not depth this crank may cross: the book
/// withholds it from `execute_v0` while a counterparty crosses it, and the one
/// case where it does not — a first leg that consumed the whole opposite side —
/// hands it over at its own resting price. What sits behind it is still an
/// ordinary cross, and stays in.
#[test]
fn a_crossed_taker_remainder_is_not_offered_to_the_arb_crank() {
    // The remainder is the best bid: the maker×maker cross behind it is what
    // remains, sized to the 98 bid rather than to the 101 remainder.
    let cross = find(
        &[
            remainder(1, 101 * PRICE, UNIT),
            maker(2, 100 * PRICE, UNIT / 2),
        ],
        &[maker(3, 99 * PRICE, UNIT)],
    );

    assert_eq!(cross.size, UNIT / 2);
    assert_eq!(cross.makers, vec![user(2), user(3)]);

    // Behind the best on its own side, with the maker in front too small to
    // absorb the whole crossing ask: the prefix stops at the remainder instead
    // of counting its base.
    let cross = find(
        &[
            maker(2, 102 * PRICE, UNIT / 2),
            remainder(1, 101 * PRICE, UNIT),
        ],
        &[maker(3, 99 * PRICE, UNIT)],
    );

    assert_eq!(cross.size, UNIT / 2);
    assert_eq!(cross.makers, vec![user(2), user(3)]);

    // A cross made only of remainders is entirely this crank's non-business.
    assert_eq!(
        find(
            &[remainder(1, 101 * PRICE, UNIT)],
            &[remainder(2, 99 * PRICE, UNIT)]
        )
        .size,
        0
    );
    assert_eq!(
        find(
            &[remainder(1, 101 * PRICE, UNIT)],
            &[maker(2, 99 * PRICE, UNIT)]
        )
        .size,
        0
    );
}

/// The three rules that turn a pair of router fills into a cross.
///
/// Every figure here is what the fill reports back: the base each leg took,
/// the worst price any one source of it reached, and what the leg did to the
/// protocol `User`'s quote net of the taker fee it paid.
mod cross_rules {
    use super::{super::*, PRICE, UNIT};

    fn leg(base_filled: u64, worst_price: u64, quote_delta: i64) -> CrossLegFill {
        CrossLegFill {
            base_filled,
            quote_delta,
            worst_price,
        }
    }

    /// Bought no worse than it sold, flat afterwards, and the protocol kept
    /// more than the floor asks for.
    #[test]
    fn a_balanced_fully_crossed_pair_clears() {
        let surplus = validate_cross_legs(
            &leg(UNIT, 99 * PRICE, -99_500_000),
            &leg(UNIT, 101 * PRICE, 101_000_000),
            (0, 0),
            1_000_000,
        )
        .unwrap();
        assert_eq!(surplus.base_matched, UNIT);
        assert_eq!(surplus.surplus, 1_500_000);
    }

    /// A size past the crossing depth. The tail of each leg runs through
    /// levels that do not cross — the buy pays up to 100.5 and the sell
    /// receives down to 99.5 — and the totals still show a surplus, because
    /// the crossed front of the cross paid for the uncrossed tail. The
    /// marginal rule is what refuses it; the floor cannot.
    #[test]
    fn a_size_past_the_crossing_depth_is_refused_even_when_the_totals_clear() {
        let err = validate_cross_legs(
            &leg(2 * UNIT, 100_500_000, -199_500_000),
            &leg(2 * UNIT, 99_500_000, 201_000_000),
            (0, 0),
            1_000_000,
        )
        .expect_err("part of the size did not cross");
        assert_eq!(err, ErrorCode::CrossMatchLegsDoNotCross.into());
    }

    /// The boundary case: every unit crossed at exactly one price. It is
    /// admitted by the marginal rule, and the floor is what decides it.
    #[test]
    fn legs_that_meet_at_one_price_still_cross() {
        assert!(validate_cross_legs(
            &leg(UNIT, 100 * PRICE, -99_500_000),
            &leg(UNIT, 100 * PRICE, 100_500_000),
            (0, 0),
            1_000_000,
        )
        .is_ok());
    }

    /// The sell leg must return exactly what the buy leg took.
    #[test]
    fn legs_that_matched_different_base_are_refused() {
        let err = validate_cross_legs(
            &leg(UNIT, 99 * PRICE, -99_500_000),
            &leg(UNIT / 2, 101 * PRICE, 50_500_000),
            (0, 0),
            0,
        )
        .expect_err("the legs are imbalanced");
        assert_eq!(err, ErrorCode::CrossMatchImbalanced.into());
    }

    /// And the protocol must end the crank holding what it started with, so
    /// the crank never leaves a position behind.
    #[test]
    fn a_taker_that_did_not_return_to_flat_is_refused() {
        let err = validate_cross_legs(
            &leg(UNIT, 99 * PRICE, -99_500_000),
            &leg(UNIT, 101 * PRICE, 101_000_000),
            (0, UNIT as i64),
            0,
        )
        .expect_err("the protocol user kept base");
        assert_eq!(err, ErrorCode::CrossMatchImbalanced.into());
    }

    /// A cross the protocol barely clears is not worth the lamports the
    /// reservoir pays to land it.
    #[test]
    fn a_surplus_under_the_floor_is_refused() {
        let err = validate_cross_legs(
            &leg(UNIT, 99 * PRICE, -99_500_000),
            &leg(UNIT, 101 * PRICE, 100_499_999),
            (0, 0),
            1_000_000,
        )
        .expect_err("the surplus is under the floor");
        assert_eq!(err, ErrorCode::CrossMatchUnprofitable.into());
    }

    /// Nothing crossed at all, which is what a leg the book had no depth for
    /// comes back as.
    #[test]
    fn a_cross_that_filled_nothing_is_refused() {
        let err = validate_cross_legs(&leg(0, 0, 0), &leg(0, 0, 0), (0, 0), 0)
            .expect_err("nothing crossed");
        assert_eq!(err, ErrorCode::CrossMatchUnprofitable.into());
    }
}

/// Where a cross leg bounds itself.
///
/// A leg brings no price of its own — the crossed prices are what it exists
/// to reach — so it bounds itself at the edge of the market's maker band,
/// which is the widest price the fill would settle a maker at anyway.
mod leg_bound {
    use super::{super::*, PRICE};

    const ORACLE: i64 = 100 * PRICE as i64;
    /// Ten percent, in MARGIN_PRECISION units.
    const BAND: u32 = 1_000;

    fn breaches(price: u64, direction: PositionDirection) -> bool {
        crate::math::orders::limit_price_breaches_maker_oracle_price_bands(
            price, direction, ORACLE, BAND,
        )
        .unwrap()
    }

    /// The bound sits exactly where the band starts refusing, on both sides,
    /// so it discards no price the fill would have taken.
    #[test]
    fn a_leg_is_bounded_at_the_first_price_the_band_refuses() {
        let buy = leg_limit_price(PositionDirection::Long, ORACLE, BAND).unwrap();
        assert_eq!(buy, 110 * PRICE);
        assert!(breaches(buy, PositionDirection::Long));
        assert!(!breaches(buy - 1, PositionDirection::Long));

        let sell = leg_limit_price(PositionDirection::Short, ORACLE, BAND).unwrap();
        assert_eq!(sell, 90 * PRICE);
        assert!(breaches(sell, PositionDirection::Short));
        assert!(!breaches(sell + 1, PositionDirection::Short));
    }

    /// A market with no band of its own bounds a leg at the oracle price,
    /// which is the tightest the rule can be rather than an absent bound.
    #[test]
    fn a_market_with_no_band_bounds_both_legs_at_oracle() {
        assert_eq!(
            leg_limit_price(PositionDirection::Long, ORACLE, 0).unwrap(),
            ORACLE as u64
        );
        assert_eq!(
            leg_limit_price(PositionDirection::Short, ORACLE, 0).unwrap(),
            ORACLE as u64
        );
    }
}
