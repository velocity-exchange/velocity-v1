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
    quoter_spec::L3_ROW_FLAG_RESERVED,
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

/// A remainder the book withholds: it crosses a counterparty and its claim
/// holds, so the book reports it reserved and with no matchable size.
fn withheld_remainder(authority: u8, price: u64) -> L3RowV0 {
    L3RowV0 {
        flags: L3_ROW_FLAG_TAKER_ORIGIN | L3_ROW_FLAG_RESERVED,
        ..maker(authority, price, 0)
    }
}

/// A maker order that a remainder's claim covers in part. The book reports its
/// free size, and a fill passes over the whole order.
fn partly_claimed(authority: u8, price: u64, free: u64) -> L3RowV0 {
    L3RowV0 {
        flags: L3_ROW_FLAG_RESERVED,
        ..maker(authority, price, free)
    }
}

fn find(bids: &[L3RowV0], asks: &[L3RowV0]) -> CrossPrefix {
    clob_cross_prefix(&BookSides {
        bids: bids.to_vec(),
        asks: asks.to_vec(),
    })
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

/// One authority on both sides of a cross ends the prefix. The executor's legs
/// take depth in price order, so the walk cannot skip the pair and keep going.
#[test]
fn a_self_cross_ends_the_prefix() {
    let cross = find(
        &[maker(1, 102 * PRICE, UNIT / 2), maker(2, 101 * PRICE, UNIT)],
        &[maker(3, 99 * PRICE, UNIT / 2), maker(2, 100 * PRICE, UNIT)],
    );

    assert_eq!(cross.size, UNIT / 2);
    assert_eq!(cross.makers, vec![user(1), user(3)]);
    assert_eq!(
        find(
            &[maker(4, 101 * PRICE, UNIT)],
            &[maker(4, 99 * PRICE, UNIT)]
        )
        .size,
        0
    );
}

/// A taker remainder is not depth this crank may cross. The book reports a
/// crossed remainder whose claim holds with no matchable size, and the maker
/// cross behind it stays. A remainder the book reports matchable ends its side,
/// because the executor refuses a leg that can reach it.
#[test]
fn a_crossed_taker_remainder_is_not_offered_to_the_arb_crank() {
    let cross = find(
        &[
            withheld_remainder(1, 101 * PRICE),
            maker(2, 100 * PRICE, UNIT / 2),
        ],
        &[maker(3, 99 * PRICE, UNIT)],
    );

    assert_eq!(cross.size, UNIT / 2);
    assert_eq!(cross.makers, vec![user(2), user(3)]);

    // A lapsed claim leaves the whole remainder matchable in front.
    let cross = find(
        &[
            remainder(1, 101 * PRICE, UNIT),
            maker(2, 100 * PRICE, UNIT / 2),
        ],
        &[maker(3, 99 * PRICE, UNIT)],
    );

    assert_eq!(cross.size, 0);

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

/// A maker order that a claim covers in part is no depth, because a fill
/// passes over all of it. The prefix crosses the depth behind it instead.
#[test]
fn a_partly_claimed_maker_is_not_offered_to_the_arb_crank() {
    let cross = find(
        &[maker(1, 101 * PRICE, UNIT)],
        &[
            partly_claimed(2, 99 * PRICE, UNIT / 2),
            maker(3, 100 * PRICE, UNIT / 4),
        ],
    );

    assert_eq!(cross.size, UNIT / 4);
    assert_eq!(cross.makers, vec![user(1), user(3)]);
}

/// The executor refuses a leg that fills the owner of a remainder it can take,
/// so a side ends at that owner's first row, even an ordinary one.
#[test]
fn a_maker_that_owns_a_takeable_remainder_ends_its_side() {
    let cross = find(
        &[
            maker(2, 102 * PRICE, UNIT / 2),
            maker(1, 101 * PRICE, UNIT),
            remainder(1, 98 * PRICE, UNIT),
        ],
        &[maker(3, 99 * PRICE, UNIT)],
    );

    assert_eq!(cross.size, UNIT / 2);
    assert_eq!(cross.makers, vec![user(2), user(3)]);

    // A withheld remainder cannot be taken, so its owner's maker order stays.
    let cross = find(
        &[
            maker(1, 101 * PRICE, UNIT),
            withheld_remainder(1, 100 * PRICE),
        ],
        &[maker(3, 99 * PRICE, UNIT)],
    );

    assert_eq!(cross.size, UNIT);
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
/// A leg brings no price of its own, so it bounds itself at the last price
/// the maker band accepts. That is the widest price the fill settles a maker
/// at.
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

    /// The bound is the last price the band accepts, on both sides. A level
    /// at the bound fills, and the next price out is refused.
    #[test]
    fn a_leg_is_bounded_at_the_last_price_the_band_accepts() {
        let buy = leg_limit_price(PositionDirection::Long, ORACLE, BAND).unwrap();
        assert_eq!(buy, 110 * PRICE - 1);
        assert!(!breaches(buy, PositionDirection::Long));
        assert!(breaches(buy + 1, PositionDirection::Long));

        let sell = leg_limit_price(PositionDirection::Short, ORACLE, BAND).unwrap();
        assert_eq!(sell, 90 * PRICE + 1);
        assert!(!breaches(sell, PositionDirection::Short));
        assert!(breaches(sell - 1, PositionDirection::Short));
    }

    /// A band whose edge falls between two prices keeps the price below the
    /// edge, which the band accepts.
    #[test]
    fn an_edge_between_two_prices_keeps_the_price_inside_it() {
        let oracle = 1_000_003;
        let band = 1_000;
        let buy = leg_limit_price(PositionDirection::Long, oracle, band).unwrap();
        let refuses = |price, direction| {
            crate::math::orders::limit_price_breaches_maker_oracle_price_bands(
                price, direction, oracle, band,
            )
            .unwrap()
        };

        assert!(!refuses(buy, PositionDirection::Long));
        assert!(refuses(buy + 1, PositionDirection::Long));

        let sell = leg_limit_price(PositionDirection::Short, oracle, band).unwrap();
        assert!(!refuses(sell, PositionDirection::Short));
        assert!(refuses(sell - 1, PositionDirection::Short));
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

/// The floor a cross must clear before the reservoir pays for it.
mod surplus_floor {
    use {
        super::{super::*, PRICE},
        crate::{
            create_anchor_account_info,
            state::{
                clob_crank::CrankPaymentsV0,
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                spot_market::SpotMarket,
                spot_market_map::SpotMarketMap,
            },
        },
    };

    const SOL_MARKET: u16 = 1;
    const MIN_CROSS_SURPLUS: u64 = 1_000;

    /// A keeper payment of 0.01 SOL. At a SOL price of 100 it is worth one
    /// unit of quote.
    fn payment(sol_spot_market_index: u16, sol_twap: Option<i64>) -> CrossPayment {
        let mut conditions = ClobCrankConditionsV0 {
            min_cross_surplus: MIN_CROSS_SURPLUS,
            crank_payments: CrankPaymentsV0 {
                cross: 10_000_000,
                ..CrankPaymentsV0::default()
            },
            ..ClobCrankConditionsV0::default()
        };

        create_anchor_account_info!(conditions, ClobCrankConditionsV0, conditions_info);
        let conditions = AccountLoader::try_from(&conditions_info).unwrap();

        let mut sol_market = SpotMarket {
            market_index: SOL_MARKET,
            oracle: Pubkey::new_unique(),
            oracle_source: OracleSource::PythLazer,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap_5min: sol_twap.unwrap_or(0),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };

        create_anchor_account_info!(sol_market, SpotMarket, sol_market_info);
        let spot_market_map = match sol_twap {
            Some(_) => SpotMarketMap::load_one(&sol_market_info, true).unwrap(),
            None => SpotMarketMap::empty(),
        };

        let state = State {
            sol_spot_market_index,
            ..State::default()
        };

        cross_payment(
            &conditions,
            &state,
            &spot_market_map,
            &mut OracleMap::empty(),
        )
        .unwrap()
    }

    const UNPAID: CrossPayment = CrossPayment {
        min_surplus: MIN_CROSS_SURPLUS,
        lamports: 0,
    };

    /// Market index zero is the quote market, so its TWAP must not price the
    /// payment. The reservoir then pays nothing.
    #[test]
    fn a_state_with_no_sol_market_pays_nothing() {
        assert_eq!(payment(0, None), UNPAID);
        assert_eq!(payment(0, Some(100 * PRICE as i64)), UNPAID);
    }

    /// A relay resolver can stage the SOL spot market but not its oracle. The
    /// market's own TWAP then prices the payment.
    #[test]
    fn the_sol_market_twap_prices_the_payment_when_no_oracle_rides() {
        assert_eq!(
            payment(SOL_MARKET, Some(100 * PRICE as i64)),
            CrossPayment {
                min_surplus: crate::math::constants::QUOTE_PRECISION_U64,
                lamports: 10_000_000,
            }
        );
    }

    /// A crank that carries no SOL market, or one whose TWAP is cold, cannot
    /// value the payment, so the reservoir pays nothing for it.
    #[test]
    fn a_crank_without_a_sol_price_pays_nothing() {
        assert_eq!(payment(SOL_MARKET, None), UNPAID);
        assert_eq!(payment(SOL_MARKET, Some(0)), UNPAID);
    }
}

/// A funding period that rolls between the two legs.
///
/// The first leg's own funding update rolls the rate after the protocol
/// `User` opened `UNIT` long. The second leg's fill then settles one period on
/// that long. The fills are modelled by writing the position directly, because
/// only the order of the settle and the baseline read is under test.
mod funding_between_legs {
    use {
        super::{super::*, PRICE, UNIT},
        crate::{
            math::funding::calculate_funding_payment,
            state::{perp_market::PerpMarket, user::PerpPosition},
            test_utils::get_positions,
        },
    };

    const TAKER_FEE: i64 = 40_000;
    /// One period of funding worth 0.5 quote on `UNIT` long.
    const RATE_ROLL: i128 = 500_000_000;

    fn market_position(user: &mut User) -> &mut PerpPosition {
        user.get_perp_position_mut(0).unwrap()
    }

    /// Leg 1 buys `UNIT` at 100 and leg 2 sells it at 100.3. The spread nets
    /// 0.22 after both taker fees, which the 0.5 funding payment outweighs.
    #[test]
    fn funding_the_first_leg_rolls_counts_against_the_surplus() {
        let key = Pubkey::new_unique();
        let mut market = PerpMarket::default();
        let mut user = User {
            // An open order keeps the flat position addressable.
            perp_positions: get_positions(PerpPosition {
                open_orders: 1,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let opening = open_leg(&mut user, 0);
        let position = market_position(&mut user);
        position.base_asset_amount = UNIT as i64;
        position.quote_asset_amount = opening.quote_before - 100 * PRICE as i64 - TAKER_FEE;
        position.last_cumulative_funding_rate = market.cumulative_funding_rate_long as i64;
        let buy = CrossLegFill {
            base_filled: UNIT,
            quote_delta: position.quote_asset_amount - opening.quote_before,
            worst_price: 100 * PRICE,
        };

        market.cumulative_funding_rate_long += RATE_ROLL;
        let funding = calculate_funding_payment(
            market.cumulative_funding_rate_long,
            market_position(&mut user),
        )
        .unwrap();
        assert!(funding < 0, "the long pays the period");

        let opening = open_leg(&mut user, 0);
        settle_funding_payment(&mut user, &key, &mut market, 0).unwrap();
        let position = market_position(&mut user);
        position.base_asset_amount = 0;
        position.quote_asset_amount += 100_300_000 - TAKER_FEE;
        let quote_after = position.quote_asset_amount;
        let sell = CrossLegFill {
            base_filled: UNIT,
            quote_delta: quote_after - opening.quote_before,
            worst_price: 100_300_000,
        };

        // Accepted, this cross leaves the protocol `User` short of quote.
        assert!(quote_after < 0);
        assert_eq!(
            validate_cross_legs(&buy, &sell, (0, 0), 1).unwrap_err(),
            ErrorCode::CrossMatchUnprofitable.into()
        );

        // A baseline read after the settle hides the payment and admits it.
        let hidden = CrossLegFill {
            quote_delta: sell.quote_delta - funding,
            ..sell
        };
        assert!(validate_cross_legs(&buy, &hidden, (0, 0), 1).is_ok());
    }
}

/// Which taker-origin orders a cross leg may not fill. The sell leg takes bids
/// best price first, down to its limit.
mod taker_origin_reach {
    use super::{
        super::*, maker, partly_claimed, remainder, user, withheld_remainder, PRICE, UNIT,
    };

    const SELL_LIMIT: u64 = 98 * PRICE;

    fn sell_reach(bids: &[L3RowV0]) -> TakerOriginReach {
        let rows: Vec<ReachRow> = bids.iter().map(ReachRow::from_row).collect();
        let mut reach = TakerOriginReach::default();
        reach.add_side(&rows, PositionDirection::Short, SELL_LIMIT);
        reach
    }

    fn taken(reach: &TakerOriginReach, makers: &[UserRefV0], worst_price: u64) -> bool {
        reach.taken(makers, Some(worst_price), PositionDirection::Short)
    }

    /// A remainder that no ask crosses rests as a bid at its worst price. The
    /// book reports it matchable, so the leg may not fill its owner.
    #[test]
    fn a_leg_that_fills_a_takeable_remainders_owner_is_refused() {
        let reach = sell_reach(&[
            maker(2, 101 * PRICE, UNIT / 2),
            remainder(1, 100 * PRICE, UNIT),
        ]);

        assert!(taken(&reach, &[user(2), user(1)], 100 * PRICE));
        assert!(!taken(&reach, &[user(2)], 101 * PRICE));
    }

    /// The book reports a partly claimed maker with its free size, but a fill
    /// passes over the whole order. Depth in front of a remainder therefore
    /// says nothing about whether the leg reaches it.
    #[test]
    fn a_remainder_behind_a_partly_claimed_maker_is_watched() {
        let reach = sell_reach(&[
            partly_claimed(2, 101 * PRICE, 7 * UNIT),
            remainder(1, 100 * PRICE, UNIT),
        ]);

        assert_eq!(reach.owners, vec![user(1)]);
        assert!(taken(&reach, &[user(1)], 100 * PRICE));
    }

    #[test]
    fn a_remainder_past_the_leg_limit_is_not_watched() {
        let reach = sell_reach(&[remainder(1, SELL_LIMIT - 1, UNIT)]);
        assert!(reach.owners.is_empty());
    }

    /// The book withholds a crossed remainder whose claim holds, so no leg can
    /// fill it. Its owner's other orders stay open to the leg.
    #[test]
    fn a_withheld_remainder_is_not_watched() {
        let reach = sell_reach(&[
            withheld_remainder(1, 102 * PRICE),
            maker(1, 101 * PRICE, UNIT),
        ]);

        assert!(reach.owners.is_empty());
        assert!(!taken(&reach, &[user(1)], 101 * PRICE));
    }

    /// A full read inside the limit leaves rows unread. A leg that reaches the
    /// last price read can have taken one of them. A short read is the whole
    /// side.
    #[test]
    fn a_leg_that_reaches_past_a_full_read_is_refused() {
        let full_window = vec![maker(2, 101 * PRICE, 1); CROSS_ROWS_PER_SIDE as usize];
        let reach = sell_reach(&full_window);
        assert!(taken(&reach, &[user(2)], 101 * PRICE));
        assert!(!taken(&reach, &[], 102 * PRICE));
        assert!(!reach.taken(&[], None, PositionDirection::Short));

        let reach = sell_reach(&full_window[1..]);
        assert!(!taken(&reach, &[user(2)], 101 * PRICE));
    }
}
