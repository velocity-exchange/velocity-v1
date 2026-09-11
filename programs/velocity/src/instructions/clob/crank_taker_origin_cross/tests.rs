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
