//! Which crossing prefix the cross resolver offers `crank_cross_match`.
//!
//! The input is the book's own `quote_l3_v0` answer, so these cases say
//! nothing about how the book stores an order; the litesvm crank tests pin the
//! reporting against the real CLOB program.

use {
    super::*,
    crate::state::prop_amm::{ClobUserRefV0, L3RowV0, L3_ROW_FLAG_TAKER_ORIGIN},
};

const UNIT: u64 = crate::math::constants::BASE_PRECISION_U64;
const PRICE: u64 = crate::math::constants::PRICE_PRECISION_U64;

fn user(authority: u8) -> ClobUserRefV0 {
    ClobUserRefV0 {
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
        user: user(authority),
        flags: 0,
        _pad: [0; 5],
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
