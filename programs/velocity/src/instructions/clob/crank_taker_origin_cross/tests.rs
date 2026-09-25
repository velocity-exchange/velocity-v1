//! What a settled pair of remainders is held to after the match.
//!
//! This branch settles both legs itself, so no router pass stands behind it to
//! apply the shared post-fill checks. These cases pin that the branch applies
//! them: the same size and price settle or revert on the aggressor's collateral
//! state, which no placement reservation records.

use {
    super::*,
    crate::{
        create_anchor_account_info,
        math::{
            constants::{
                BASE_PRECISION_I64, BASE_PRECISION_U64, PRICE_PRECISION_I64, QUOTE_PRECISION_I64,
                QUOTE_PRECISION_U64, SPOT_BALANCE_PRECISION_U64,
                SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
            },
            time::SlotClock,
        },
        state::{
            market_status::MarketStatus,
            oracle::{HistoricalOracleData, OracleSource},
            oracle_map::OracleMap,
            perp_market::{MarketStats, PerpMarket},
            pyth_lazer_oracle::PythLazerOracle,
            spot_market::{SpotBalanceType, SpotMarket},
            spot_market_map::SpotMarketMap,
            user::{PerpPosition, SpotPosition},
        },
        test_utils::{get_positions, get_pyth_price, get_spot_positions},
    },
    std::str::FromStr,
};

const NOW: i64 = 0;
const SLOT: u64 = 0;

/// One market at 100, one aggressor that just bought a whole unit from flat,
/// and the counterparty that sold it.
///
/// The state is the state after the match, which is where the post-fill checks
/// read it. `breaker_tripped` is the only input that varies: the aggressor's
/// equity breaker.
/// Which side of the pair holds an isolated position, and what it holds
/// against it. `None` leaves both sides cross-margined.
#[derive(Default, Clone, Copy)]
struct Isolated {
    aggressor: Option<u64>,
    counterparty: Option<u64>,
}

fn pair_post_checks(breaker_tripped: bool) -> VelocityResult {
    pair_post_checks_with(breaker_tripped, Isolated::default())
}

fn pair_post_checks_with(breaker_tripped: bool, isolated: Isolated) -> VelocityResult {
    let mut oracle_price = get_pyth_price(100, 6);
    let oracle_key = Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
    create_anchor_account_info!(oracle_price, &oracle_key, PythLazerOracle, oracle_info);
    let oracle_map = OracleMap::load_one(&oracle_info, SLOT, SlotClock::baseline(), None).unwrap();

    let mut market = PerpMarket {
        market_index: 0,
        oracle: oracle_key,
        oracle_source: OracleSource::PythLazer,
        margin_ratio_initial: 1000,
        margin_ratio_maintenance: 500,
        status: MarketStatus::Initialized,
        market_stats: MarketStats {
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price: 100 * PRICE_PRECISION_I64,
                last_oracle_price_twap: 100 * PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: 100 * PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..MarketStats::default()
        },
        ..PerpMarket::default_test()
    };

    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut quote_market = SpotMarket {
        market_index: 0,
        oracle_source: OracleSource::QuoteAsset,
        cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
        cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
        decimals: 6,
        initial_asset_weight: SPOT_WEIGHT_PRECISION,
        maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
        historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
        ..SpotMarket::default()
    };

    create_anchor_account_info!(quote_market, SpotMarket, quote_market_info);
    let spot_market_map = SpotMarketMap::load_one(&quote_market_info, true).unwrap();
    let mut maps = AccountMaps::new(perp_market_map, spot_market_map, oracle_map);

    let taker_key = Pubkey::from_str("My11111111111111111111111111111111111111113").unwrap();
    let mut taker = User {
        perp_positions: get_positions(PerpPosition {
            market_index: 0,
            base_asset_amount: BASE_PRECISION_I64,
            quote_asset_amount: -100 * QUOTE_PRECISION_I64,
            position_flag: isolated
                .aggressor
                .map(|_| crate::state::user::PositionFlag::IsolatedPosition as u8)
                .unwrap_or(0),
            isolated_position_scaled_balance: isolated
                .aggressor
                .map(|quote| quote * SPOT_BALANCE_PRECISION_U64)
                .unwrap_or(0),
            ..PerpPosition::default()
        }),

        spot_positions: get_spot_positions(SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 10_000 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        }),
        ..User::default()
    };

    create_anchor_account_info!(taker, &taker_key, User, taker_info);
    let taker_loader = AccountLoader::try_from(&taker_info).unwrap();

    let mut taker_stats = UserStats {
        equity_breaker_tripped: u8::from(breaker_tripped),
        ..UserStats::default()
    };

    create_anchor_account_info!(taker_stats, UserStats, taker_stats_info);
    let taker_stats_loader = AccountLoader::try_from(&taker_stats_info).unwrap();

    let counterparty_key = Pubkey::from_str("CLoB111111111111111111111111111111111111111").unwrap();
    let counterparty_authority =
        Pubkey::from_str("6ncQ5nmiZjHJK8QPGevKJnnLKtSXjZ4Q2r8bTHTNiFEf").unwrap();
    let mut counterparty = User {
        authority: counterparty_authority,
        perp_positions: get_positions(PerpPosition {
            market_index: 0,
            base_asset_amount: -BASE_PRECISION_I64,
            quote_asset_amount: 100 * QUOTE_PRECISION_I64,
            position_flag: isolated
                .counterparty
                .map(|_| crate::state::user::PositionFlag::IsolatedPosition as u8)
                .unwrap_or(0),
            isolated_position_scaled_balance: isolated
                .counterparty
                .map(|quote| quote * SPOT_BALANCE_PRECISION_U64)
                .unwrap_or(0),
            ..PerpPosition::default()
        }),

        spot_positions: get_spot_positions(SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 10_000 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        }),
        ..User::default()
    };

    create_anchor_account_info!(counterparty, &counterparty_key, User, counterparty_info);
    let makers_and_referrer = UserMap::load_one(&counterparty_info).unwrap();

    let mut counterparty_stats = UserStats {
        authority: counterparty_authority,
        ..UserStats::default()
    };

    create_anchor_account_info!(counterparty_stats, UserStats, counterparty_stats_info);
    let makers_and_referrer_stats = UserStatsMap::load_one(&counterparty_stats_info).unwrap();

    super::post_checks::check_pair_fill(
        &taker_loader,
        &taker_stats_loader,
        &makers_and_referrer,
        &makers_and_referrer_stats,
        &mut maps,
        0,
        &PairFill {
            counterparty_key,
            counterparty_direction: PositionDirection::Short,
            base_filled: BASE_PRECISION_U64,
            quote_filled: 100 * QUOTE_PRECISION_U64,
        },
        &PairFillFacts {
            // The aggressor bought from flat, so the match opened a position.
            aggressor_order_decreasing: false,
            aggressor_is_isolated: isolated.aggressor.is_some(),
            perp_market_oi_before: 0,
            oracle_stale_for_margin: false,
        },
        NOW,
    )
    .map(|_| ())
}

#[test]
fn a_pair_both_sides_can_afford_settles() {
    assert_eq!(pair_post_checks(false), Ok(()));
}

#[test]
fn a_tripped_aggressor_equity_breaker_refuses_the_pair() {
    assert_eq!(
        pair_post_checks(true),
        Err(ErrorCode::EquityBelowFloor),
        "a risk-increasing aggressor whose equity breaker is tripped must not \
         settle against a counterparty remainder"
    );
}

/// Nothing on an order records its margin regime: both sides of a settled pair
/// are read off their live positions. These pin that the read reaches the
/// checks, on each side independently — a fill that lands in an isolated
/// position is backed by that position's own collateral, and a flush cross
/// balance does not stand in for it.
#[test]
fn an_isolated_aggressor_is_checked_against_its_own_collateral() {
    assert_eq!(
        pair_post_checks_with(
            false,
            Isolated {
                aggressor: Some(0),
                ..Isolated::default()
            }
        ),
        Err(ErrorCode::InsufficientCollateral),
        "the aggressor's 10,000 of cross collateral does not back a fill that \
         lands in its isolated position"
    );
    assert_eq!(
        pair_post_checks_with(
            false,
            Isolated {
                aggressor: Some(10_000),
                ..Isolated::default()
            }
        ),
        Ok(()),
        "the same fill settles once the isolated position holds the collateral"
    );
}

#[test]
fn an_isolated_counterparty_is_checked_against_its_own_collateral() {
    assert_eq!(
        pair_post_checks_with(
            false,
            Isolated {
                counterparty: Some(0),
                ..Isolated::default()
            }
        ),
        Err(ErrorCode::InsufficientCollateral),
        "the maker half of the pair is scoped by its own live position too"
    );
    assert_eq!(
        pair_post_checks_with(
            false,
            Isolated {
                counterparty: Some(10_000),
                ..Isolated::default()
            }
        ),
        Ok(())
    );
}

/// The size a pair settles, before either position moves.
mod pair_size {
    use super::*;

    const UNIT: u64 = BASE_PRECISION_U64;

    /// A position holding `base` with `reserved_bids` of resting bids behind it.
    fn position(base: i64, reserved_bids: u64) -> PerpPosition {
        PerpPosition {
            market_index: 0,
            base_asset_amount: base,
            open_bids: reserved_bids as i64,
            ..PerpPosition::default()
        }
    }

    fn size(
        aggressor: PerpPosition,
        counterparty: PerpPosition,
        reduce_only: PairReduceOnly,
    ) -> Result<u64> {
        super::super::pair_fill_size(
            UNIT,
            PositionDirection::Long,
            &aggressor,
            &counterparty,
            reduce_only,
        )
    }

    const NEITHER: PairReduceOnly = PairReduceOnly {
        aggressor: false,
        counterparty: false,
    };

    #[test]
    fn an_open_pair_settles_the_whole_cross() {
        assert_eq!(size(position(0, UNIT), position(0, 0), NEITHER), Ok(UNIT));
    }

    #[test]
    fn a_report_above_the_aggressor_reservation_fails() {
        assert_eq!(
            size(position(0, UNIT / 2), position(0, 0), NEITHER),
            Err(ErrorCode::QuoterReportExceedsReservation.into())
        );
    }

    /// A reduce-only bid covers only the short it closes, so the pair shrinks
    /// to that short instead of failing.
    #[test]
    fn a_reduce_only_aggressor_shrinks_the_pair_to_its_cover() {
        let reduce_only = PairReduceOnly {
            aggressor: true,
            ..NEITHER
        };

        assert_eq!(
            size(
                position(-(UNIT as i64) / 4, UNIT),
                position(0, 0),
                reduce_only
            ),
            Ok(UNIT / 4)
        );
    }

    /// The counterparty sells, so its cover is the long it closes.
    #[test]
    fn a_reduce_only_counterparty_shrinks_the_pair_to_its_cover() {
        let reduce_only = PairReduceOnly {
            counterparty: true,
            ..NEITHER
        };

        assert_eq!(
            size(position(0, UNIT), position(UNIT as i64 / 2, 0), reduce_only),
            Ok(UNIT / 2)
        );
    }

    #[test]
    fn a_reduce_only_side_with_nothing_to_reduce_settles_nothing() {
        let reduce_only = PairReduceOnly {
            aggressor: true,
            ..NEITHER
        };

        assert_eq!(
            size(position(0, UNIT), position(0, 0), reduce_only),
            Err(ErrorCode::NoTakerOriginCross.into())
        );
    }
}

/// Who the crank pays, and what a referred taker must carry.
mod cranker_rules {
    use {super::*, crate::state::user::ReferrerStatus};

    fn filler(authority: u8, pool_id: u8) -> User {
        User {
            authority: Pubkey::new_from_array([authority; 32]),
            pool_id,
            ..User::default()
        }
    }

    const TAKER: Pubkey = Pubkey::new_from_array([7; 32]);

    #[test]
    fn a_third_party_cranker_earns_the_reward() {
        assert_eq!(cranker_earns_reward(&filler(1, 0), &TAKER), Ok(true));
    }

    /// Another sub-account of the taker earns no reward and no filler volume.
    #[test]
    fn a_cranker_of_the_taker_authority_earns_nothing() {
        assert_eq!(cranker_earns_reward(&filler(7, 0), &TAKER), Ok(false));
    }

    #[test]
    fn a_cranker_outside_pool_zero_is_refused() {
        assert_eq!(
            cranker_earns_reward(&filler(1, 1), &TAKER),
            Err(ErrorCode::InvalidPoolId.into())
        );
    }

    #[test]
    fn a_referred_taker_must_carry_its_escrow() {
        let referred = UserStats {
            referrer_status: ReferrerStatus::BuilderReferral as u8,
            ..UserStats::default()
        };

        assert_eq!(
            require_referral_escrow(false, &referred),
            Err(ErrorCode::UnableToLoadRevenueShareAccount.into())
        );
        assert_eq!(require_referral_escrow(true, &referred), Ok(()));
        assert_eq!(
            require_referral_escrow(false, &UserStats::default()),
            Ok(())
        );
    }
}

/// When a resolver lets a maker cross go ahead of a taker-origin cross.
mod stalled_cross {
    use {super::*, crate::state::prop_amm::ClobOrderRefV0};

    fn cross(bid_slot: u64, ask_slot: u64) -> Cross {
        let row = |placed_slot| RestingOrder {
            order_ref: ClobOrderRefV0 {
                node_index: 0,
                order_id: 0,
            },
            user: UserRefV0::default(),
            price: 100,
            base_asset_amount: 1,
            taker_origin: true,
            reduce_only: false,
            placed_slot,
        };

        Cross {
            bid: row(bid_slot),
            ask: row(ask_slot),
            base_asset_amount: 1,
            kind: CrossKind::BidAggresses,
        }
    }

    #[test]
    fn a_cross_stalls_only_once_its_later_row_is_old() {
        let limit = super::super::super::crank_cross_match::STALLED_TAKER_ORIGIN_CROSS_SLOTS;
        assert!(!cross_stalled(&cross(0, 10), 10 + limit));
        assert!(cross_stalled(&cross(0, 10), 11 + limit));
        assert!(!cross_stalled(&cross(10, 0), 10 + limit));
    }
}

/// The order-layer steps a settled pair shares with every routed fill.
mod settled_match {
    use {
        super::*,
        crate::{controller::orders::SettledMatch, state::user::Order},
    };

    struct Case {
        oracle_twap_5min: i64,
        open_interest: i128,
        max_open_interest: u128,
        fill_price: u64,
    }

    const ORDINARY: Case = Case {
        oracle_twap_5min: 100 * PRICE_PRECISION_I64,
        open_interest: BASE_PRECISION_I64 as i128,
        max_open_interest: 0,
        fill_price: 100 * PRICE_PRECISION_I64 as u64,
    };

    fn market_at(case: &Case, oracle_key: Pubkey) -> PerpMarket {
        PerpMarket {
            market_index: 0,
            oracle: oracle_key,
            oracle_source: OracleSource::PythLazer,
            status: MarketStatus::Active,
            base_asset_amount_long: case.open_interest,
            base_asset_amount_short: -case.open_interest,
            max_open_interest: case.max_open_interest,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price: 100 * PRICE_PRECISION_I64,
                    last_oracle_price_twap: 100 * PRICE_PRECISION_I64,
                    last_oracle_price_twap_5min: case.oracle_twap_5min,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default_test()
        }
    }

    /// What one run observed.
    #[derive(Debug, PartialEq)]
    struct Observed {
        too_divergent: bool,
        /// `None` when the run stopped before the bookkeeping.
        last_fill_price: Option<u64>,
    }

    /// Read the conditions for a one-unit match with an oracle at 100. With
    /// `apply`, run the bookkeeping at `case.fill_price` too.
    fn run(case: Case, apply: bool) -> VelocityResult<Observed> {
        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_key = Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(oracle_price, &oracle_key, PythLazerOracle, oracle_info);
        let oracle_map =
            OracleMap::load_one(&oracle_info, SLOT, SlotClock::baseline(), None).unwrap();

        let mut market = market_at(&case, oracle_key);
        create_anchor_account_info!(market, PerpMarket, market_info);
        let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();
        let mut maps = AccountMaps::new(perp_market_map, SpotMarketMap::empty(), oracle_map);

        let mut taker = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        create_anchor_account_info!(taker, User, taker_info);
        let taker_loader = AccountLoader::try_from(&taker_info).unwrap();
        let mut taker_stats = UserStats::default();
        create_anchor_account_info!(taker_stats, UserStats, taker_stats_info);
        let taker_stats_loader = AccountLoader::try_from(&taker_stats_info).unwrap();

        let state = State::default();
        let clock = Clock {
            slot: SLOT,
            unix_timestamp: NOW,
            ..Clock::default()
        };
        let mut order = Order {
            market_index: 0,
            status: OrderStatus::Open,
            market_type: crate::state::user::MarketType::Perp,
            direction: PositionDirection::Long,
            base_asset_amount: BASE_PRECISION_U64,
            ..Order::default()
        };

        let settled = SettledMatch::read(
            &state,
            &mut maps,
            &taker_loader,
            &taker_stats_loader,
            &mut order,
            &clock,
        )?;
        let too_divergent = settled.oracle_too_divergent_with_twap(&state)?;
        if !apply {
            return Ok(Observed {
                too_divergent,
                last_fill_price: None,
            });
        }

        settled.apply_bookkeeping(
            &state,
            &mut order,
            &taker_loader,
            &taker_stats_loader,
            &mut controller::orders::FillParties {
                maps: &mut maps,
                makers_and_referrer: &UserMap::empty(),
                makers_and_referrer_stats: &UserStatsMap::empty(),
            },
            controller::orders::FillAmounts {
                base: BASE_PRECISION_U64,
                quote: case.fill_price,
            },
        )?;

        let last_fill_price = maps.perp_market_map.get_ref(&0)?.last_fill_price;
        Ok(Observed {
            too_divergent,
            last_fill_price: Some(last_fill_price),
        })
    }

    #[test]
    fn an_ordinary_match_records_its_price() {
        assert_eq!(
            run(ORDINARY, true),
            Ok(Observed {
                too_divergent: false,
                last_fill_price: Some(100 * PRICE_PRECISION_I64 as u64),
            })
        );
    }

    #[test]
    fn a_match_past_the_open_interest_cap_fails() {
        let over_cap = Case {
            max_open_interest: BASE_PRECISION_I64 as u128 / 2,
            ..ORDINARY
        };

        assert_eq!(run(over_cap, true), Err(ErrorCode::MaxOpenInterest));
    }

    #[test]
    fn a_match_outside_the_fill_price_band_fails() {
        let off_band = Case {
            fill_price: 150 * PRICE_PRECISION_I64 as u64,
            ..ORDINARY
        };

        assert_eq!(run(off_band, true), Err(ErrorCode::PriceBandsBreached));
    }

    /// The verdict reads the TWAP as it stood before the read refreshed it.
    #[test]
    fn an_oracle_far_from_its_twap_is_reported() {
        let divergent = Case {
            oracle_twap_5min: 40 * PRICE_PRECISION_I64,
            ..ORDINARY
        };

        assert_eq!(
            run(divergent, false),
            Ok(Observed {
                too_divergent: true,
                last_fill_price: None,
            })
        );
    }
}

/// A pair takes nothing from the vAMM. It still samples the mark TWAP at the
/// price it traded, as a routed fill does, so funding reads both branches
/// alike.
mod pair_mark_twap {
    use {
        super::*,
        crate::{
            controller::orders::{record_fill_in_mark_twap_and_volume, AmmMarkQuote, FillAmounts},
            math::constants::PRICE_PRECISION_U64,
        },
    };

    /// The ask TWAP after one pair buys a unit at `price`, a minute after the
    /// last sample.
    fn ask_twap_after_pair_at(price: u64) -> u64 {
        let mut market = PerpMarket {
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price: 100 * PRICE_PRECISION_I64,
                    last_oracle_price_twap: 100 * PRICE_PRECISION_I64,
                    last_oracle_price_twap_5min: 100 * PRICE_PRECISION_I64,
                    ..HistoricalOracleData::default()
                },
                last_bid_price_twap: 100 * PRICE_PRECISION_U64,
                last_ask_price_twap: 100 * PRICE_PRECISION_U64,
                last_mark_price_twap: 100 * PRICE_PRECISION_U64,
                last_mark_price_twap_5min: 100 * PRICE_PRECISION_U64,
                ..MarketStats::default()
            },
            ..PerpMarket::default_test()
        };
        let amm_mark_quote = AmmMarkQuote::of_amm(&market.amm).unwrap();

        record_fill_in_mark_twap_and_volume(
            &mut market,
            &amm_mark_quote,
            FillAmounts {
                base: BASE_PRECISION_U64,
                // Price and quote share one precision.
                quote: price,
            },
            PositionDirection::Long,
            60,
        )
        .unwrap();

        assert_eq!(market.market_stats.last_trade_ts, 60);
        market.market_stats.last_ask_price_twap
    }

    #[test]
    fn a_pair_samples_the_mark_twap_at_its_price() {
        assert!(
            ask_twap_after_pair_at(100 * PRICE_PRECISION_U64)
                < ask_twap_after_pair_at(101 * PRICE_PRECISION_U64)
        );
    }
}
