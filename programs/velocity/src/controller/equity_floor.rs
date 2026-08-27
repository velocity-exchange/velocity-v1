//! Lazy equity-breaker trips. The authority-wide equity breaker is normally
//! armed by the permissionless `trip_equity_floor_breaker` instruction, which
//! requires a separate keeper transaction to land. This module lets the paths
//! that are allowed to run while a floored subaccount sits below its raw
//! floor (reducing fills, strictly reducing swaps, trigger cancels) arm the
//! breaker inline as a side effect of the interaction itself, shrinking the
//! window in which sibling subaccounts can keep taking risk to the next
//! touch instead of the next keeper transaction. A rejected instruction
//! reverts its own writes, so only succeeding paths can host a trip; the
//! gated paths (which reject at floor + buffer) never can, and never need to.

use crate::{
    error::VelocityResult,
    math::margin::calculate_user_equity_for_trip,
    msg,
    state::{
        oracle_map::OracleMap,
        perp_market_map::PerpMarketMap,
        spot_market_map::SpotMarketMap,
        user::{User, UserStats},
    },
};

/// Arms the authority-wide equity breaker if the subaccount's net equity is
/// provably below its raw floor. Decides with the same
/// `TripNetEquity::proves_breach` predicate as the permissionless trip:
/// invalid-oracle liabilities and shorts receive their sound zero upper
/// bound, while any invalid-oracle asset or long keeps the breach
/// unprovable. Where the permissionless trip
/// rejects on an unprovable breach so the keeper can retry, this skips
/// silently (it must not fail its host); a breach that rides out such an
/// outage is armed by
/// the next touch after the feed recovers. The gates cover the outage
/// itself, failing closed on the strict verdict, and match fills carry
/// their own `FillOrderMatch` validity rule. Skips all work when the
/// subaccount has no floor or the breaker is already set, and never fails
/// the host instruction on its own.
pub fn try_lazy_equity_breaker_trip(
    user: &User,
    user_stats: &mut UserStats,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
) -> VelocityResult {
    if user.equity_floor == 0 || user_stats.is_equity_breaker_tripped() {
        return Ok(());
    }

    let trip_equity =
        calculate_user_equity_for_trip(user, perp_market_map, spot_market_map, oracle_map)?;

    if trip_equity.proves_breach(user) {
        msg!(
            "equity floor breaker tripped for authority {:?}: subaccount {} net equity upper bound {} below floor {}",
            user.authority,
            user.sub_account_id,
            trip_equity.equity_upper_bound,
            user.equity_floor
        );
        user_stats.set_equity_breaker_tripped(true);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::try_lazy_equity_breaker_trip,
        crate::{
            create_anchor_account_info,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I64, PEG_PRECISION, PRICE_PRECISION,
                    PRICE_PRECISION_I64, QUOTE_PRECISION_I64, QUOTE_PRECISION_U64,
                    SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                time::SlotClock,
            },
            state::{
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market::{MarketStats, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                user::{Order, PerpPosition, SpotPosition, User, UserStats},
            },
            test_utils::{get_positions, get_pyth_price, *},
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    // 10 USDC deposit plus a perp position at oracle 100 (twap 100). With
    // the default position (1 base long entered at -90 quote) net equity is
    // 10 + (100 - 90) = 20.
    fn run_scenario_with_position(
        equity_floor: u64,
        already_tripped: bool,
        oracle_map_slot: u64,
        base_asset_amount: i64,
        quote_asset_amount: i64,
    ) -> UserStats {
        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            oracle_map_slot,
            SlotClock::baseline(),
            None,
        )
        .unwrap();

        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                ..AMM::default()
            },
            margin_ratio_initial: 2000,
            margin_ratio_maintenance: 1000,
            status: MarketStatus::Initialized,
            order_step_size: 1000,
            order_tick_size: 1,
            oracle: oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price: (100 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap: (100 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap_5min: (100 * PRICE_PRECISION) as i64,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default_test()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_map =
            SpotMarketMap::load_one(&usdc_spot_market_account_info, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 10 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount,
                quote_asset_amount,
                ..PerpPosition::default()
            }),
            spot_positions,
            equity_floor,
            ..User::default()
        };

        let mut user_stats = UserStats::default();
        user_stats.set_equity_breaker_tripped(already_tripped);

        try_lazy_equity_breaker_trip(
            &user,
            &mut user_stats,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
        )
        .unwrap();

        user_stats
    }

    fn run_scenario(equity_floor: u64, already_tripped: bool, oracle_map_slot: u64) -> UserStats {
        run_scenario_with_position(
            equity_floor,
            already_tripped,
            oracle_map_slot,
            BASE_PRECISION_I64,
            -90 * QUOTE_PRECISION_I64,
        )
    }

    #[test]
    fn trips_below_raw_floor() {
        let stats = run_scenario(30 * QUOTE_PRECISION_U64, false, 0);
        assert!(stats.is_equity_breaker_tripped());
    }

    #[test]
    fn no_trip_at_the_floor() {
        // trip threshold is strictly below: equity 20 == floor 20 stays clear
        let stats = run_scenario(20 * QUOTE_PRECISION_U64, false, 0);
        assert!(!stats.is_equity_breaker_tripped());
    }

    #[test]
    fn no_trip_without_floor() {
        let stats = run_scenario(0, false, 0);
        assert!(!stats.is_equity_breaker_tripped());
    }

    #[test]
    fn no_trip_on_stale_material_position() {
        // oracle map loaded far past the oracle's posted slot. The long has
        // no finite upper bound, so the breaker
        // must not arm. Equity would be 10 + (200 - 180) = 30 below the
        // 50 floor if the oracle were trusted.
        let stats = run_scenario_with_position(
            50 * QUOTE_PRECISION_U64,
            false,
            100_000,
            2 * BASE_PRECISION_I64,
            -180 * QUOTE_PRECISION_I64,
        );
        assert!(!stats.is_equity_breaker_tripped());
    }

    #[test]
    fn no_trip_on_stale_small_long_position() {
        // Size and stored twap do not bound an invalid-oracle long. Even this
        // 0.5 base position keeps the trip unprovable until price recovers.
        let stats = run_scenario_with_position(
            100 * QUOTE_PRECISION_U64,
            false,
            100_000,
            BASE_PRECISION_I64 / 2,
            -45 * QUOTE_PRECISION_I64,
        );
        assert!(!stats.is_equity_breaker_tripped());
    }

    #[test]
    fn noop_when_already_tripped() {
        let stats = run_scenario(30 * QUOTE_PRECISION_U64, true, 0);
        assert!(stats.is_equity_breaker_tripped());
    }
}
