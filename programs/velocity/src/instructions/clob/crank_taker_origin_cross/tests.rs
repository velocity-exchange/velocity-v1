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
                BASE_PRECISION_I64, BASE_PRECISION_U64, PEG_PRECISION, PRICE_PRECISION_I64,
                QUOTE_PRECISION_I64, QUOTE_PRECISION_U64, SPOT_BALANCE_PRECISION_U64,
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
/// A zero buffer reads every account under liquidation as able to exit it.
const LIQUIDATION_MARGIN_BUFFER_RATIO: u32 = 200;

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
    pair_checks(breaker_tripped, isolated, Parties::default())
}

/// The liquidation state of each side, and the aggressor's cross deposit.
#[derive(Clone, Copy)]
struct Parties {
    aggressor_status: u8,
    counterparty_status: u8,
    aggressor_deposit_quote: u64,
}

impl Default for Parties {
    fn default() -> Self {
        Self {
            aggressor_status: 0,
            counterparty_status: 0,
            aggressor_deposit_quote: 10_000,
        }
    }
}

fn pair_checks(breaker_tripped: bool, isolated: Isolated, parties: Parties) -> VelocityResult {
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
            scaled_balance: parties.aggressor_deposit_quote * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        }),
        status: parties.aggressor_status,
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
        status: parties.counterparty_status,
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

    super::admit_pair_parties(
        &taker_loader,
        &makers_and_referrer,
        &counterparty_key,
        LIQUIDATION_MARGIN_BUFFER_RATIO,
        &mut maps,
    )?;

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

/// A pair is held to the liquidation gates a routed fill applies to its taker
/// and its maker.
mod pair_parties {
    use {super::*, crate::state::user::UserStatus};

    const LIQUIDATED: u8 = UserStatus::BeingLiquidated as u8;
    const BANKRUPT: u8 = UserStatus::Bankrupt as u8;

    fn checks(parties: Parties) -> VelocityResult {
        pair_checks(false, Isolated::default(), parties)
    }

    #[test]
    fn an_underwater_aggressor_under_liquidation_is_refused() {
        let parties = Parties {
            aggressor_status: LIQUIDATED,
            aggressor_deposit_quote: 0,
            ..Parties::default()
        };

        assert_eq!(checks(parties), Err(ErrorCode::UserIsBeingLiquidated));
    }

    /// A routed fill clears the flag of a taker that is back above the
    /// liquidation margin, and the pair does the same.
    #[test]
    fn a_recovered_aggressor_settles() {
        let parties = Parties {
            aggressor_status: LIQUIDATED,
            ..Parties::default()
        };

        assert_eq!(checks(parties), Ok(()));
    }

    #[test]
    fn a_bankrupt_aggressor_is_refused() {
        let parties = Parties {
            aggressor_status: BANKRUPT,
            ..Parties::default()
        };

        assert_eq!(checks(parties), Err(ErrorCode::UserBankrupt));
    }

    #[test]
    fn a_counterparty_under_liquidation_is_refused() {
        let liquidated = Parties {
            counterparty_status: LIQUIDATED,
            ..Parties::default()
        };
        let bankrupt = Parties {
            counterparty_status: BANKRUPT,
            ..Parties::default()
        };

        assert_eq!(checks(liquidated), Err(ErrorCode::UserIsBeingLiquidated));
        assert_eq!(checks(bankrupt), Err(ErrorCode::UserBankrupt));
    }
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

    /// What one run observed, and the market as the run left it.
    struct Ran {
        observed: Observed,
        market: PerpMarket,
    }

    fn oracle_key() -> Pubkey {
        Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap()
    }

    /// Read the conditions for a one-unit match with an oracle at 100. With
    /// `apply`, run the bookkeeping at `case.fill_price` too.
    fn run(case: Case, apply: bool) -> VelocityResult<Observed> {
        let market = market_at(&case, oracle_key());
        run_on(market, case.fill_price, apply).map(|ran| ran.observed)
    }

    fn run_on(mut market: PerpMarket, fill_price: u64, apply: bool) -> VelocityResult<Ran> {
        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_key = oracle_key();
        create_anchor_account_info!(oracle_price, &oracle_key, PythLazerOracle, oracle_info);
        let oracle_map =
            OracleMap::load_one(&oracle_info, SLOT, SlotClock::baseline(), None).unwrap();

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
        if apply {
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
                    quote: fill_price,
                },
            )?;
        }

        let market = *maps.perp_market_map.get_ref(&0)?;
        Ok(Ran {
            observed: Observed {
                too_divergent,
                last_fill_price: apply.then_some(market.last_fill_price),
            },
            market,
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

    const FUNDING_PERIOD: i64 = 3_600;

    /// Whether a match at 100, on a market whose funding period is due and
    /// whose AMM marks at 100, writes funding at fill exit.
    fn match_writes_funding(oracle_twap_5min: i64) -> bool {
        let case = Case {
            oracle_twap_5min,
            ..ORDINARY
        };
        let mut market = market_at(&case, oracle_key());
        market.amm.peg_multiplier = 100 * PEG_PRECISION;
        market.last_funding_rate_ts = NOW - FUNDING_PERIOD;
        let stats = &mut market.market_stats;
        stats.funding_period = FUNDING_PERIOD;
        stats.historical_oracle_data.last_oracle_price_twap_ts = NOW - 300;
        stats.last_mark_price_twap_ts = NOW - 300;
        stats.last_bid_price_twap = 100 * PRICE_PRECISION_I64 as u64;
        stats.last_ask_price_twap = 100 * PRICE_PRECISION_I64 as u64;
        stats.last_mark_price_twap = 100 * PRICE_PRECISION_I64 as u64;
        stats.last_mark_price_twap_5min = 100 * PRICE_PRECISION_I64 as u64;

        let ran = run_on(market, case.fill_price, true).unwrap();
        assert!(!ran.observed.too_divergent);
        ran.market.last_funding_rate_ts == NOW
    }

    #[test]
    fn a_match_at_the_funding_boundary_writes_funding() {
        assert!(match_writes_funding(100 * PRICE_PRECISION_I64));
    }

    /// The fill's own refresh moves a lagging 5-minute TWAP onto the live
    /// price. The funding gate judges the TWAP from before that refresh.
    #[test]
    fn a_match_with_a_lagging_twap_writes_no_funding() {
        assert!(!match_writes_funding(85 * PRICE_PRECISION_I64));
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

mod keeper_payment_floor {
    use super::*;

    const PAYMENT_QUOTE: u64 = 1_000;

    /// Two wallets can cross each other at one price. That crank earns no
    /// reward, and the fee remainder alone must cover the keeper payment.
    #[test]
    fn a_crank_that_collected_less_than_the_payment_is_not_paid() {
        assert!(!collected_covers_payment(0, Some(PAYMENT_QUOTE)));
        assert!(!collected_covers_payment(
            PAYMENT_QUOTE - 1,
            Some(PAYMENT_QUOTE)
        ));
    }

    #[test]
    fn a_crank_that_collected_the_payment_is_paid() {
        assert!(collected_covers_payment(PAYMENT_QUOTE, Some(PAYMENT_QUOTE)));
    }

    #[test]
    fn a_payment_with_no_price_is_not_paid() {
        assert!(!collected_covers_payment(u64::MAX, None));
    }
}

mod builder_binding {
    use {
        super::*,
        crate::state::user::{Order, OrderBitFlag},
    };

    /// The book names the remainder by its own handle. A builder row for the
    /// velocity id renames the order and flags it, so the fill charges the fee.
    #[test]
    fn a_remainder_with_a_builder_row_is_charged_the_builder_fee() {
        let mut order = Order {
            order_id: 41,
            ..Order::default()
        };

        apply_builder_row(&mut order, 7, true);
        assert_eq!(order.order_id, 7);
        assert!(order.is_bit_flag_set(OrderBitFlag::HasBuilder));
    }

    #[test]
    fn a_remainder_with_no_builder_row_is_left_as_the_book_named_it() {
        let mut order = Order {
            order_id: 41,
            ..Order::default()
        };

        apply_builder_row(&mut order, 7, false);
        assert_eq!(order.order_id, 41);
        assert!(!order.is_bit_flag_set(OrderBitFlag::HasBuilder));
    }
}

/// Which remainder a crank may settle when several on one side cross.
mod subject_claim_order {
    use {super::*, crate::state::prop_amm::ClobOrderRefV0};

    fn user(owner: u8) -> UserRefV0 {
        UserRefV0 {
            authority: Pubkey::new_from_array([owner; 32]),
            sub_account_id: 0,
        }
    }

    fn row(order_id: u64, price: u64, owner: u8, taker_origin: bool) -> RestingOrder {
        RestingOrder {
            order_ref: ClobOrderRefV0 {
                node_index: order_id as u32,
                order_id,
            },
            user: user(owner),
            price,
            base_asset_amount: 5,
            taker_origin,
            reduce_only: false,
            placed_slot: order_id,
        }
    }

    /// Two bid remainders cross one ask. The book reserves the ask for the
    /// older remainder, so a crank for the newer one must fail.
    #[test]
    fn only_the_oldest_remainder_on_a_side_is_settled() {
        let bids = [row(10, 101, 0xA, true), row(30, 101, 0xC, true)];
        let asks = [row(5, 99, 0xB, false)];
        let crosses = resolve_crosses(&bids, &asks, MAX_CROSSES_PER_CRANK);

        let older = subject_cross(&crosses, user(0xA)).unwrap();
        assert_eq!(older.order.order_ref.order_id, 10);

        assert_eq!(
            subject_cross(&crosses, user(0xC)).err(),
            Some(ErrorCode::NoTakerOriginCross)
        );
    }

    /// A counterparty with depth for both still settles the older first. The
    /// fill takes the whole remainder, so the newer one must wait.
    #[test]
    fn a_newer_remainder_waits_even_when_depth_covers_both() {
        let bids = [row(10, 101, 0xA, true), row(30, 101, 0xC, true)];
        let mut ask = row(5, 99, 0xB, false);
        ask.base_asset_amount = 10;
        let crosses = resolve_crosses(&bids, &[ask], MAX_CROSSES_PER_CRANK);

        assert!(subject_cross(&crosses, user(0xA)).is_ok());
        assert_eq!(
            subject_cross(&crosses, user(0xC)).err(),
            Some(ErrorCode::NoTakerOriginCross)
        );
    }
}

/// Which rows the crank removes instead of routing, and what their owner pays.
mod unfillable_rows {
    use {super::*, crate::state::prop_amm::ClobOrderRefV0};

    const UNIT: i64 = BASE_PRECISION_I64;

    fn row(reduce_only: bool) -> RestingOrder {
        RestingOrder {
            order_ref: ClobOrderRefV0 {
                node_index: 1,
                order_id: 1,
            },
            user: UserRefV0::ZERO,
            price: 100,
            base_asset_amount: 5,
            taker_origin: true,
            reduce_only,
            placed_slot: 0,
        }
    }

    /// A reduce-only buy whose short closed through another order. Every
    /// fill clamps it to zero, so the crank removes it.
    #[test]
    fn a_reduce_only_row_with_a_flat_position_cannot_fill() {
        assert!(has_nothing_to_reduce(
            &row(true),
            0,
            PositionDirection::Long
        ));
    }

    /// A reduce-only buy against a long would increase the position.
    #[test]
    fn a_reduce_only_row_on_the_side_of_its_position_cannot_fill() {
        assert!(has_nothing_to_reduce(
            &row(true),
            UNIT,
            PositionDirection::Long
        ));
    }

    #[test]
    fn a_reduce_only_row_with_a_position_to_reduce_fills() {
        assert!(!has_nothing_to_reduce(
            &row(true),
            -UNIT,
            PositionDirection::Long
        ));
        assert!(!has_nothing_to_reduce(
            &row(true),
            UNIT,
            PositionDirection::Short
        ));
    }

    #[test]
    fn a_row_that_is_not_reduce_only_is_routed() {
        assert!(!has_nothing_to_reduce(
            &row(false),
            0,
            PositionDirection::Long
        ));
    }

    /// The reservoir pays the keeper, so the owner pays at least that value.
    #[test]
    fn a_priced_keeper_payment_raises_the_fee() {
        assert_eq!(unfillable_row_fee(10_000, Some(14_000)), 14_000);
    }

    #[test]
    fn the_fee_never_falls_below_the_flat_fee() {
        assert_eq!(unfillable_row_fee(10_000, Some(1)), 10_000);
        assert_eq!(unfillable_row_fee(10_000, None), 10_000);
    }

    /// The raised fee always clears the payment floor, so relay is paid for
    /// the removal.
    #[test]
    fn the_raised_fee_covers_the_keeper_payment() {
        let payment_quote = Some(14_000);
        assert!(collected_covers_payment(
            unfillable_row_fee(10_000, payment_quote),
            payment_quote
        ));
    }
}
