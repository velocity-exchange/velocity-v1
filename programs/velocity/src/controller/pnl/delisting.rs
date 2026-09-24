use {
    crate::{
        math::time::{legacy_slot_duration_i64, legacy_slot_duration_u8},
        state::{
            oracle_map::OracleMap,
            perp_market::MarketStats,
            user::{Order, PerpPosition},
        },
    },
    anchor_lang::prelude::Pubkey,
};

fn get_user_keys() -> (Pubkey, Pubkey, Pubkey) {
    (Pubkey::default(), Pubkey::default(), Pubkey::default())
}

#[cfg(test)]
pub mod delisting_test {
    use crate::{
        instructions::optional_accounts::AccountMaps,
        math::time::{Millis, SlotClock},
    };
    // use crate::controller::orders::fill_order;
    use {
        super::*,
        crate::{
            controller::{
                liquidation::{
                    liquidate_perp, liquidate_perp_pnl_for_deposit, resolve_perp_bankruptcy,
                },
                orders::cancel_order,
                pnl::settle_expired_position,
                position::PositionDirection,
            },
            create_anchor_account_info,
            error::ErrorCode,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I64, BASE_PRECISION_U64,
                    LIQUIDATION_PCT_PRECISION, PEG_PRECISION, PERCENTAGE_PRECISION,
                    PRICE_PRECISION, PRICE_PRECISION_I64, PRICE_PRECISION_U64,
                    QUOTE_PRECISION_I128, QUOTE_PRECISION_I64, QUOTE_SPOT_MARKET_INDEX,
                    SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                funding::calculate_funding_payment,
                margin::{
                    calculate_margin_requirement_and_total_collateral_and_liability_info,
                    calculate_perp_position_value_and_pnl, MarginRequirementType,
                },
            },
            state::{
                events::OrderActionExplanation,
                margin_calculation::{MarginCalculation, MarginContext},
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource, StrictOraclePrice},
                paused_operations::PerpOperation,
                perp_market::{PerpMarket, PoolBalance, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                state::{OracleGuardRails, State, ValidityGuardRails},
                user::{OrderStatus, OrderType, SpotPosition, User, UserStats},
            },
            test_utils::{get_orders, get_positions, get_pyth_price, get_spot_positions},
            vlp::amm::{math::amm::calculate_net_user_pnl, refresh::settle_expired_market},
        },
        anchor_lang::prelude::Clock,
        std::str::FromStr,
    };

    #[test]
    fn failed_attempt_to_close_healthy_market() {
        let _now = 0_i64;
        let slot = 0_u64;
        let clock = Clock {
            slot: 6893025720,
            epoch_start_timestamp: 1662065595 - 1000,
            epoch: 2424,
            leader_schedule_epoch: 1662065595 - 1,
            unix_timestamp: 1662065595,
        };

        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map =
            OracleMap::load_one(&oracle_account_info, slot, SlotClock::baseline(), None).unwrap();

        // net users are short
        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                base_asset_amount_with_amm: (AMM_RESERVE_PRECISION / 2) as i128,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                amm_jit_intensity: 100,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            base_asset_amount_long: (AMM_RESERVE_PRECISION / 2) as i128,
            order_step_size: 10000000,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u128::MAX;
        market.amm.min_base_asset_reserve = 0;

        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);

        let state = State {
            oracle_guard_rails: OracleGuardRails {
                validity: ValidityGuardRails {
                    slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4s
                    slots_before_stale_for_margin: legacy_slot_duration_i64(120), // 48s
                    confidence_interval_max_size: 1000,
                    too_volatile_ratio: 5,
                },
                ..OracleGuardRails::default()
            },
            ..State::default()
        };

        // attempt to delist healthy market
        assert_eq!(market.expiry_ts, 0);
        assert!(settle_expired_market(0, &mut maps, &state, &clock,).is_err());
        assert_eq!(market.is_reduce_only().unwrap(), false);
        assert_eq!(market.is_in_settlement(clock.unix_timestamp), false);

        market.expiry_ts = clock.unix_timestamp + 100;
        assert_eq!(clock.unix_timestamp, 1662065595);

        assert_eq!(market.is_in_settlement(clock.unix_timestamp), false);
        assert_eq!(market.is_reduce_only().unwrap(), false); // isnt set like in update expiry ix

        market.status = MarketStatus::ReduceOnly;
        assert_eq!(market.is_reduce_only().unwrap(), true);

        // attempt to delist too early
        assert!(settle_expired_market(0, &mut maps, &state, &clock,).is_err());
    }

    #[test]
    fn delist_market_with_0_balance_long_at_target() {
        let _now = 0_i64;
        let slot = 0_u64;
        let clock = Clock {
            slot: 6893025720,
            epoch_start_timestamp: 1662065595 - 1000,
            epoch: 2424,
            leader_schedule_epoch: 1662065595 - 1,
            unix_timestamp: 1662065595,
        };

        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map =
            OracleMap::load_one(&oracle_account_info, slot, SlotClock::baseline(), None).unwrap();

        // net users are long
        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                base_asset_amount_with_amm: (AMM_RESERVE_PRECISION / 2) as i128,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                amm_jit_intensity: 100,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            expiry_ts: clock.unix_timestamp - 10, // past expiry time

            base_asset_amount_long: (AMM_RESERVE_PRECISION / 2) as i128,
            order_step_size: 10000000,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            quote_asset_amount: -(QUOTE_PRECISION_I128 * 50), //longs have $100 cost basis
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: (99 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap_5min: (99 * PRICE_PRECISION) as i64,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u128::MAX;
        market.amm.min_base_asset_reserve = 0;

        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);

        let state = State {
            oracle_guard_rails: OracleGuardRails {
                validity: ValidityGuardRails {
                    slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4s
                    slots_before_stale_for_margin: legacy_slot_duration_i64(120), // 48s
                    confidence_interval_max_size: 1000,
                    too_volatile_ratio: 5,
                },
                ..OracleGuardRails::default()
            },
            ..State::default()
        };

        // expiry time
        assert_eq!(market.expiry_ts < clock.unix_timestamp, true);
        assert_eq!(market.status, MarketStatus::Initialized);
        assert_eq!(market.expiry_price, 0);
        assert_eq!(market.is_in_settlement(clock.unix_timestamp), true);

        // put in settlement mode
        settle_expired_market(0, &mut maps, &state, &clock).unwrap();

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.expiry_price > 0, true);
        assert_eq!(market.expiry_price, 98999999);
        assert_eq!(market.status, MarketStatus::Settlement);
        drop(market);
    }

    #[test]
    fn delist_market_with_0_balance_long_at_best_effort() {
        let slot = 0_u64;
        let clock = Clock {
            slot: 6893025720,
            epoch_start_timestamp: 1662065595 - 1000,
            epoch: 2424,
            leader_schedule_epoch: 1662065595 - 1,
            unix_timestamp: 1662065595,
        };

        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map =
            OracleMap::load_one(&oracle_account_info, slot, SlotClock::baseline(), None).unwrap();

        // net users are short
        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                base_asset_amount_with_amm: (AMM_RESERVE_PRECISION / 2) as i128,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                amm_jit_intensity: 100,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            expiry_ts: clock.unix_timestamp - 10, // past expiry time

            base_asset_amount_long: (AMM_RESERVE_PRECISION / 2) as i128,
            order_step_size: 10000000,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            quote_asset_amount: -(QUOTE_PRECISION_I128 * 10), //longs have $20 cost basis
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: (99 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap_5min: (99 * PRICE_PRECISION) as i64,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u128::MAX;
        market.amm.min_base_asset_reserve = 0;

        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);

        let state = State {
            oracle_guard_rails: OracleGuardRails {
                validity: ValidityGuardRails {
                    slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4s
                    slots_before_stale_for_margin: legacy_slot_duration_i64(120), // 48s
                    confidence_interval_max_size: 1000,
                    too_volatile_ratio: 5,
                },
                ..OracleGuardRails::default()
            },
            ..State::default()
        };

        // expiry time
        assert_eq!(market.expiry_ts < clock.unix_timestamp, true);
        assert_eq!(market.status, MarketStatus::Initialized);
        assert_eq!(market.expiry_price, 0);

        // put in settlement mode
        settle_expired_market(0, &mut maps, &state, &clock).unwrap();

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.expiry_price > 0, true);
        assert_eq!(
            market.expiry_price
                < market
                    .market_stats
                    .historical_oracle_data
                    .last_oracle_price_twap,
            true
        );
        assert_eq!(market.expiry_price, 19999999); // best can do :/
        assert_eq!(market.status, MarketStatus::Settlement);
        drop(market);
    }

    #[test]
    fn delist_market_with_neg_balance_long_at_best_effort() {
        let slot = 0_u64;
        let clock = Clock {
            slot: 6893025720,
            epoch_start_timestamp: 1662065595 - 1000,
            epoch: 2424,
            leader_schedule_epoch: 1662065595 - 1,
            unix_timestamp: 1662065595,
        };

        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map =
            OracleMap::load_one(&oracle_account_info, slot, SlotClock::baseline(), None).unwrap();

        // net users are short
        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                base_asset_amount_with_amm: (AMM_RESERVE_PRECISION / 2) as i128,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                amm_jit_intensity: 100,
                total_fee_minus_distributions: -(100000 * QUOTE_PRECISION_I128), // down $100k
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            expiry_ts: clock.unix_timestamp - 10, // past expiry time

            base_asset_amount_long: (AMM_RESERVE_PRECISION / 2) as i128,
            order_step_size: 10000000,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            quote_asset_amount: -(QUOTE_PRECISION_I128 * 10), //longs have $20 cost basis
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: (99 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap_5min: (99 * PRICE_PRECISION) as i64,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u128::MAX;
        market.amm.min_base_asset_reserve = 0;

        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);

        let state = State {
            oracle_guard_rails: OracleGuardRails {
                validity: ValidityGuardRails {
                    slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4s
                    slots_before_stale_for_margin: legacy_slot_duration_i64(120), // 48s
                    confidence_interval_max_size: 1000,
                    too_volatile_ratio: 5,
                },
                ..OracleGuardRails::default()
            },
            ..State::default()
        };

        // expiry time
        assert_eq!(market.expiry_ts < clock.unix_timestamp, true);
        assert_eq!(market.status, MarketStatus::Initialized);
        assert_eq!(market.expiry_price, 0);

        // put in settlement mode
        settle_expired_market(0, &mut maps, &state, &clock).unwrap();

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.expiry_price > 0, true);
        assert_eq!(
            market.expiry_price
                < market
                    .market_stats
                    .historical_oracle_data
                    .last_oracle_price_twap,
            true
        );
        assert_eq!(market.expiry_price, 19999999); // best can do :/
        assert_eq!(market.status, MarketStatus::Settlement);
        drop(market);
    }

    #[test]
    fn delist_market_with_neg_balance_short_at_target() {
        let slot = 0_u64;
        let clock = Clock {
            slot: 6893025720,
            epoch_start_timestamp: 1662065595 - 1000,
            epoch: 2424,
            leader_schedule_epoch: 1662065595 - 1,
            unix_timestamp: 1662065595,
        };

        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map =
            OracleMap::load_one(&oracle_account_info, slot, SlotClock::baseline(), None).unwrap();

        // net users are short
        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                base_asset_amount_with_amm: -((AMM_RESERVE_PRECISION / 2) as i128),
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                amm_jit_intensity: 100,
                total_fee_minus_distributions: -(100000 * QUOTE_PRECISION_I128), // down $100k
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            expiry_ts: clock.unix_timestamp - 10, // past expiry time

            base_asset_amount_long: (AMM_RESERVE_PRECISION / 2) as i128,
            order_step_size: 10000000,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            quote_asset_amount: (QUOTE_PRECISION_I128 * 10), //shorts have $20 cost basis
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: (99 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap_5min: (99 * PRICE_PRECISION) as i64,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u128::MAX;
        market.amm.min_base_asset_reserve = 0;

        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);

        let state = State {
            oracle_guard_rails: OracleGuardRails {
                validity: ValidityGuardRails {
                    slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4s
                    slots_before_stale_for_margin: legacy_slot_duration_i64(120), // 48s
                    confidence_interval_max_size: 1000,
                    too_volatile_ratio: 5,
                },
                ..OracleGuardRails::default()
            },
            ..State::default()
        };

        // expiry time
        assert_eq!(market.expiry_ts < clock.unix_timestamp, true);
        assert_eq!(market.status, MarketStatus::Initialized);
        assert_eq!(market.expiry_price, 0);

        // put in settlement mode
        settle_expired_market(0, &mut maps, &state, &clock).unwrap();

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.expiry_price > 0, true);
        assert_eq!(market.expiry_price, 99000001); // target
        assert_eq!(market.status, MarketStatus::Settlement);
        drop(market);
    }

    #[test]
    fn delist_market_with_1000_balance_long_at_target() {
        let slot = 0_u64;
        let clock = Clock {
            slot: 6893025720,
            epoch_start_timestamp: 1662065595 - 1000,
            epoch: 2424,
            leader_schedule_epoch: 1662065595 - 1,
            unix_timestamp: 1662065595,
        };

        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map =
            OracleMap::load_one(&oracle_account_info, slot, SlotClock::baseline(), None).unwrap();

        // net users are short
        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                base_asset_amount_with_amm: (AMM_RESERVE_PRECISION / 2) as i128,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                amm_jit_intensity: 100,
                total_fee_minus_distributions: 0,
                ..AMM::default()
            },
            number_of_users_with_base: 1,
            number_of_users: 1,
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            pnl_pool: PoolBalance {
                scaled_balance: (1000 * SPOT_BALANCE_PRECISION),
                market_index: QUOTE_SPOT_MARKET_INDEX,
                ..PoolBalance::default()
            },
            expiry_ts: clock.unix_timestamp - 10, // past expiry time

            base_asset_amount_long: (AMM_RESERVE_PRECISION / 2) as i128,
            order_step_size: 10000000,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            quote_asset_amount: -(QUOTE_PRECISION_I128 * 10), //longs have $20 cost basis
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: (99 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap_5min: (99 * PRICE_PRECISION) as i64,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u128::MAX;
        market.amm.min_base_asset_reserve = 0;

        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            borrow_balance: 100 * SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);

        // taker wants to go long (would improve balance)
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 100 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                base_asset_amount: (BASE_PRECISION_I64 / 2),
                quote_asset_amount: -(QUOTE_PRECISION_I64 * 10),
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let (taker_key, _maker_key, _filler_key) = get_user_keys();

        let state = State {
            oracle_guard_rails: OracleGuardRails {
                validity: ValidityGuardRails {
                    slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4s
                    slots_before_stale_for_margin: legacy_slot_duration_i64(120), // 48s
                    confidence_interval_max_size: 1000,
                    too_volatile_ratio: 5,
                },
                ..OracleGuardRails::default()
            },
            ..State::default()
        };

        // expiry time
        assert_eq!(market.expiry_ts < clock.unix_timestamp, true);
        assert_eq!(market.status, MarketStatus::Initialized);
        assert_eq!(market.expiry_price, 0);

        let MarginCalculation {
            margin_requirement,
            total_collateral,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &taker,
            &mut maps,
            MarginContext::standard(MarginRequirementType::Maintenance),
        )
        .unwrap();

        assert_eq!(total_collateral, 100000000);
        assert_eq!(margin_requirement, 7510000);

        // put in settlement mode
        settle_expired_market(0, &mut maps, &state, &clock).unwrap();

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.expiry_price > 0, true);
        assert_eq!(market.expiry_price, 98999999);
        assert_eq!(market.status, MarketStatus::Settlement);
        drop(market);

        let MarginCalculation {
            margin_requirement,
            total_collateral,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &taker,
            &mut maps,
            MarginContext::standard(MarginRequirementType::Maintenance),
        )
        .unwrap();

        assert_eq!(total_collateral, 100000000);
        assert_eq!(margin_requirement, 10000);

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.pnl_pool.scaled_balance, 1000000000000);
        assert_eq!(taker.spot_positions[0].scaled_balance, 100000000000);
        assert_eq!(taker.perp_positions[0].quote_asset_amount, -10000000);
        drop(market);

        settle_expired_position(0, &mut taker, &taker_key, &mut maps, &clock, &state).unwrap();

        assert_eq!(taker.spot_positions[0].scaled_balance > 100000000000, true);
        assert_eq!(taker.spot_positions[0].scaled_balance, 139480199000);

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.pnl_pool.scaled_balance, 960519801000);
        // A permissionless expiry closeout charges a taker fee (OtterSec #44). The
        // fee accrues to the market fee ledger, split into IF and protocol, with
        // AMM provision zeroed since there is no AMM counterparty. It does not
        // linger in the pnl pool for delisting to sweep into the revenue pool.
        // The default fee structure zeros both numerators, so the fee lands as protocol residual.
        assert_eq!(market.fee_ledger.pending_protocol_fee, 19800);
        assert_eq!(market.fee_ledger.pending_if_fee, 0);
        assert_eq!(market.fee_ledger.pending_amm_provision, 0);
        assert_eq!(market.fee_ledger.total_exchange_fee, 19800);
        drop(market);

        assert_eq!(taker.perp_positions[0].open_orders, 0);
        assert_eq!(taker.perp_positions[0].base_asset_amount, 0);
        assert_eq!(taker.perp_positions[0].quote_asset_amount, 0);
        assert_eq!(taker.perp_positions[0].quote_entry_amount, 0);
        assert_eq!(taker.perp_positions[0].quote_break_even_amount, 0);
    }

    /// A caller with no position in an expired market settles nothing, and the no-op
    /// returns before the market's SettlePnl pause check. The handler gates
    /// `sweep_completed_revenue_share_for_market` on the returned flag, so the flag
    /// must be `false` here. A `true` would let anyone move builder and referrer fees
    /// out of the pnl pool of a market whose settlement an operator paused.
    #[test]
    fn settle_expired_position_with_no_position_reports_not_settled() {
        let slot = 0_u64;
        let clock = Clock {
            slot: 6893025720,
            epoch_start_timestamp: 1662065595 - 1000,
            epoch: 2424,
            leader_schedule_epoch: 1662065595 - 1,
            unix_timestamp: 1662065595,
        };

        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map =
            OracleMap::load_one(&oracle_account_info, slot, SlotClock::baseline(), None).unwrap();

        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                base_asset_amount_with_amm: (AMM_RESERVE_PRECISION / 2) as i128,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                amm_jit_intensity: 100,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Settlement,
            // The market operator paused settlement of this market.
            paused_operations: PerpOperation::SettlePnl as u8,
            pnl_pool: PoolBalance {
                scaled_balance: (1000 * SPOT_BALANCE_PRECISION),
                market_index: QUOTE_SPOT_MARKET_INDEX,
                ..PoolBalance::default()
            },
            expiry_ts: clock.unix_timestamp - 1000,
            expiry_price: 99 * PRICE_PRECISION_I64,
            base_asset_amount_long: (AMM_RESERVE_PRECISION / 2) as i128,
            order_step_size: 10000000,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: (99 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap_5min: (99 * PRICE_PRECISION) as i64,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u128::MAX;
        market.amm.min_base_asset_reserve = 0;

        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);

        let state = State {
            settlement_duration: 1,
            oracle_guard_rails: OracleGuardRails {
                validity: ValidityGuardRails {
                    slots_before_stale_for_amm: legacy_slot_duration_i64(10),
                    slots_before_stale_for_margin: legacy_slot_duration_i64(120),
                    confidence_interval_max_size: 1000,
                    too_volatile_ratio: 5,
                },
                ..OracleGuardRails::default()
            },
            ..State::default()
        };

        let (user_key, _maker_key, _filler_key) = get_user_keys();

        // A user who holds no position in the expired market.
        let mut user = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let settled =
            settle_expired_position(0, &mut user, &user_key, &mut maps, &clock, &state).unwrap();

        assert_eq!(settled, false);

        // A user who does hold a position hits the pause check and reverts.
        let mut holder = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: (BASE_PRECISION_I64 / 2),
                quote_asset_amount: -(QUOTE_PRECISION_I64 * 10),
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        assert_eq!(
            settle_expired_position(0, &mut holder, &user_key, &mut maps, &clock, &state,)
                .unwrap_err(),
            ErrorCode::InvalidMarketStatusToSettlePnl
        );
    }

    #[test]
    fn delist_market_with_1000_balance_long_at_target_price_w_positive_quote_long() {
        let slot = 0_u64;
        let clock = Clock {
            slot: 6893025720,
            epoch_start_timestamp: 1662065595 - 1000,
            epoch: 2424,
            leader_schedule_epoch: 1662065595 - 1,
            unix_timestamp: 1662065595,
        };

        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map =
            OracleMap::load_one(&oracle_account_info, slot, SlotClock::baseline(), None).unwrap();

        // net users are short
        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                base_asset_amount_with_amm: (AMM_RESERVE_PRECISION / 2) as i128,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                amm_jit_intensity: 100,
                total_fee_minus_distributions: 0,
                ..AMM::default()
            },
            number_of_users_with_base: 1,
            number_of_users: 1,
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            pnl_pool: PoolBalance {
                scaled_balance: (1000 * SPOT_BALANCE_PRECISION),
                market_index: QUOTE_SPOT_MARKET_INDEX,
                ..PoolBalance::default()
            },
            expiry_ts: clock.unix_timestamp - 10, // past expiry time

            base_asset_amount_long: (AMM_RESERVE_PRECISION / 2) as i128,
            order_step_size: 10000000,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            quote_asset_amount: (QUOTE_PRECISION_I128 * 10), //longs have -$20 cost basis
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: (99 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap_5min: (99 * PRICE_PRECISION) as i64,

                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u128::MAX;
        market.amm.min_base_asset_reserve = 0;

        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            borrow_balance: 100 * SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);

        // taker wants to go long (would improve balance)
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 100 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                base_asset_amount: (BASE_PRECISION_I64 / 2),
                quote_asset_amount: (QUOTE_PRECISION_I64 * 10),
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let (taker_key, _maker_key, _filler_key) = get_user_keys();

        let state = State {
            oracle_guard_rails: OracleGuardRails {
                validity: ValidityGuardRails {
                    slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4s
                    slots_before_stale_for_margin: legacy_slot_duration_i64(120), // 48s
                    confidence_interval_max_size: 1000,
                    too_volatile_ratio: 5,
                },
                ..OracleGuardRails::default()
            },
            ..State::default()
        };

        // expiry time
        assert_eq!(market.expiry_ts < clock.unix_timestamp, true);
        assert_eq!(market.status, MarketStatus::Initialized);
        assert_eq!(market.expiry_price, 0);

        let MarginCalculation {
            margin_requirement,
            total_collateral,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &taker,
            &mut maps,
            MarginContext::standard(MarginRequirementType::Maintenance),
        )
        .unwrap();

        assert_eq!(total_collateral, 100000000);
        assert_eq!(margin_requirement, 7510000);

        // put in settlement mode
        settle_expired_market(0, &mut maps, &state, &clock).unwrap();

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.expiry_price > 0, true);
        assert_eq!(market.expiry_price, 98999999);
        assert_eq!(market.status, MarketStatus::Settlement);
        drop(market);

        let MarginCalculation {
            margin_requirement,
            total_collateral,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &taker,
            &mut maps,
            MarginContext::standard(MarginRequirementType::Maintenance),
        )
        .unwrap();

        assert_eq!(total_collateral, 100000000);
        assert_eq!(margin_requirement, 10000); // settlement in margin now

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.pnl_pool.scaled_balance, 1000000000000);
        assert_eq!(taker.spot_positions[0].scaled_balance, 100000000000);
        assert_eq!(taker.perp_positions[0].quote_asset_amount, 10000000);
        drop(market);

        settle_expired_position(0, &mut taker, &taker_key, &mut maps, &clock, &state).unwrap();

        assert_eq!(taker.spot_positions[0].scaled_balance > 100000000000, true);
        assert_eq!(taker.spot_positions[0].scaled_balance, 159480199000);

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.pnl_pool.scaled_balance, 940519801000);
        drop(market);

        assert_eq!(taker.perp_positions[0].open_orders, 0);
        assert_eq!(taker.perp_positions[0].base_asset_amount, 0);
        assert_eq!(taker.perp_positions[0].quote_asset_amount, 0);
        assert_eq!(taker.perp_positions[0].quote_entry_amount, 0);
        assert_eq!(taker.perp_positions[0].quote_break_even_amount, 0);
    }

    #[test]
    fn delist_market_with_1000_balance_long_negative_expiry_price() {
        // longs have negative cost basis and are up big
        // so settlement price has to be negative

        let slot = 0_u64;
        let clock = Clock {
            slot: 6893025720,
            epoch_start_timestamp: 1662065595 - 1000,
            epoch: 2424,
            leader_schedule_epoch: 1662065595 - 1,
            unix_timestamp: 1662065595,
        };

        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map =
            OracleMap::load_one(&oracle_account_info, slot, SlotClock::baseline(), None).unwrap();

        // net users are short
        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                base_asset_amount_with_amm: (AMM_RESERVE_PRECISION * 2000) as i128,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                amm_jit_intensity: 100,
                total_fee_minus_distributions: 0,
                ..AMM::default()
            },
            number_of_users_with_base: 1,
            number_of_users: 1,
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            pnl_pool: PoolBalance {
                scaled_balance: (1000 * SPOT_BALANCE_PRECISION),
                market_index: QUOTE_SPOT_MARKET_INDEX,
                ..PoolBalance::default()
            },
            expiry_ts: clock.unix_timestamp - 10, // past expiry time

            base_asset_amount_long: (AMM_RESERVE_PRECISION * 2000) as i128,
            order_step_size: 10000000,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            quote_asset_amount: (QUOTE_PRECISION_I128 * 20 * 2000), //longs have -$20 cost basis
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: (99 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap_5min: (99 * PRICE_PRECISION) as i64,

                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u128::MAX;
        market.amm.min_base_asset_reserve = 0;

        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            borrow_balance: 100 * SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);

        // taker wants to go long (would improve balance)
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 100 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                base_asset_amount: (BASE_PRECISION_I64 * 2000),
                quote_asset_amount: (QUOTE_PRECISION_I64 * 20 * 2000), //longs have -$20 cost basis,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let (taker_key, _maker_key, _filler_key) = get_user_keys();

        let state = State {
            oracle_guard_rails: OracleGuardRails {
                validity: ValidityGuardRails {
                    slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4s
                    slots_before_stale_for_margin: legacy_slot_duration_i64(120), // 48s
                    confidence_interval_max_size: 1000,
                    too_volatile_ratio: 5,
                },
                ..OracleGuardRails::default()
            },
            ..State::default()
        };

        // expiry time
        assert_eq!(market.expiry_ts < clock.unix_timestamp, true);
        assert_eq!(market.status, MarketStatus::Initialized);
        assert_eq!(market.expiry_price, 0);

        // put in settlement mode
        settle_expired_market(0, &mut maps, &state, &clock).unwrap();

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.expiry_price != 0, true);
        assert_eq!(market.expiry_price, -19500001);
        assert_eq!(market.status, MarketStatus::Settlement);
        drop(market);

        let MarginCalculation {
            margin_requirement,
            total_collateral,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &taker,
            &mut maps,
            MarginContext::standard(MarginRequirementType::Maintenance),
        )
        .unwrap();

        assert_eq!(total_collateral, 100000000);
        assert_eq!(margin_requirement, 10000);

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.pnl_pool.scaled_balance, 1000000000000);
        assert_eq!(taker.spot_positions[0].scaled_balance, 100000000000);
        assert_eq!(taker.perp_positions[0].quote_asset_amount, 40000000000);
        drop(market);

        settle_expired_position(0, &mut taker, &taker_key, &mut maps, &clock, &state).unwrap();

        assert_eq!(taker.spot_positions[0].scaled_balance > 100000000000, true);

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        // A negative expiry price leaves a small closeout notional. The fee
        // rounds up, as it does on every other taker fill, so it is one quote
        // unit rather than zero.
        assert_eq!(market.pnl_pool.scaled_balance, 15602001000);
        assert_eq!(market.amm.fee_pool.scaled_balance, 0);
        drop(market);

        assert_eq!(taker.perp_positions[0].open_orders, 0);
        assert_eq!(taker.perp_positions[0].base_asset_amount, 0);
        assert_eq!(taker.perp_positions[0].quote_asset_amount, 0);
        assert_eq!(taker.perp_positions[0].quote_entry_amount, 0);
        assert_eq!(taker.perp_positions[0].quote_break_even_amount, 0);
    }

    #[test]
    fn delist_market_with_1000_balance_shorts_owe_longs_0() {
        let slot = 0_u64;
        let clock = Clock {
            slot: 6893025720,
            epoch_start_timestamp: 1662065595 - 1000,
            epoch: 2424,
            leader_schedule_epoch: 1662065595 - 1,
            unix_timestamp: 1662065595,
        };

        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map =
            OracleMap::load_one(&oracle_account_info, slot, SlotClock::baseline(), None).unwrap();

        // net users are short
        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                base_asset_amount_with_amm: (AMM_RESERVE_PRECISION * 1000) as i128,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                amm_jit_intensity: 100,
                total_fee_minus_distributions: 0,
                ..AMM::default()
            },
            number_of_users_with_base: 2,
            number_of_users: 3,
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            pnl_pool: PoolBalance {
                scaled_balance: (1000 * SPOT_BALANCE_PRECISION),
                market_index: QUOTE_SPOT_MARKET_INDEX,
                ..PoolBalance::default()
            },
            expiry_ts: clock.unix_timestamp - 10, // past expiry time

            base_asset_amount_long: (AMM_RESERVE_PRECISION * 2000) as i128,
            base_asset_amount_short: -((AMM_RESERVE_PRECISION * 1000) as i128),
            order_step_size: 10000000,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            quote_asset_amount: -(QUOTE_PRECISION_I128 * 20 * 1000 - QUOTE_PRECISION_I128),
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: (99 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap_5min: (99 * PRICE_PRECISION) as i64,

                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u128::MAX;
        market.amm.min_base_asset_reserve = 0;

        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 300000 * SPOT_BALANCE_PRECISION,
            borrow_balance: 100 * SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos =
            Vec::from([&spot_market_account_info, &sol_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);

        // taker wants to go long (would improve balance)
        let mut longer = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 100 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                base_asset_amount: (BASE_PRECISION_I64 * 2000),
                quote_entry_amount: -(QUOTE_PRECISION_I64 * 20 * 2000 + QUOTE_PRECISION_I64), //longs have $19 cost basis,
                quote_break_even_amount: -(QUOTE_PRECISION_I64 * 20 * 2000 + QUOTE_PRECISION_I64), //longs have $19 cost basis,
                quote_asset_amount: -(QUOTE_PRECISION_I64 * 20 * 2000 + QUOTE_PRECISION_I64), //longs have $19 cost basis,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 20000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let mut shorter = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64 / 2,
                price: 100 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64 / 2,
                base_asset_amount: -(BASE_PRECISION_I64 * 1000),
                quote_entry_amount: (QUOTE_PRECISION_I64 * 20 * 1000), //shorts have $20 cost basis,
                quote_break_even_amount: (QUOTE_PRECISION_I64 * 20 * 1000), //shorts have $20 cost basis,
                quote_asset_amount: (QUOTE_PRECISION_I64 * 20 * 1000), //shorts have $20 cost basis,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 200000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        shorter.spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        // just has unsettled quote
        let mut liq = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 0,
                open_bids: 0,
                base_asset_amount: 0,
                quote_asset_amount: QUOTE_PRECISION_I64 * 2,
                quote_break_even_amount: 0,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let (taker_key, maker_key, liq_key) = get_user_keys();

        let state = State {
            oracle_guard_rails: OracleGuardRails {
                validity: ValidityGuardRails {
                    slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4s
                    slots_before_stale_for_margin: legacy_slot_duration_i64(120), // 48s
                    confidence_interval_max_size: 1000,
                    too_volatile_ratio: 5,
                },
                ..OracleGuardRails::default()
            },
            ..State::default()
        };

        // expiry time
        assert_eq!(market.expiry_ts < clock.unix_timestamp, true);
        assert_eq!(market.status, MarketStatus::Initialized);
        assert_eq!(market.expiry_price, 0);

        let MarginCalculation {
            margin_requirement,
            total_collateral,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &longer,
            &mut maps,
            MarginContext::standard(MarginRequirementType::Maintenance),
        )
        .unwrap();

        assert_eq!(total_collateral, 20000000000);
        assert_eq!(margin_requirement, 10005010000);

        // put in settlement mode
        settle_expired_market(0, &mut maps, &state, &clock).unwrap();

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.expiry_price != 0, true);
        assert_eq!(market.expiry_price, 20998999);
        assert_eq!(market.status, MarketStatus::Settlement);
        drop(market);

        // do short close
        {
            assert_eq!(shorter.orders[0].order_id, 0);
            assert_eq!(shorter.orders[0].status, OrderStatus::Open);
            assert_eq!(shorter.orders[0].base_asset_amount, 500000000);

            cancel_order(
                0,
                &mut shorter,
                &maker_key,
                &mut maps,
                clock.unix_timestamp,
                clock.slot,
                OrderActionExplanation::None,
                None,
                0,
                true,
            )
            .unwrap();

            let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
            assert_eq!(market.pnl_pool.scaled_balance, 1000000000000);

            let orig_short_balance = shorter.spot_positions[0].scaled_balance;

            assert_eq!(orig_short_balance, 200000000000000);
            assert_eq!(shorter.perp_positions[0].base_asset_amount, -1000000000000);
            assert_eq!(shorter.perp_positions[0].quote_asset_amount, 20000000000);
            drop(market);

            let MarginCalculation {
                margin_requirement,
                total_collateral,
                ..
            } = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &shorter,
                &mut maps,
                MarginContext::standard(MarginRequirementType::Maintenance),
            )
            .unwrap();

            assert_eq!(total_collateral, 199001001000);
            assert_eq!(margin_requirement, 11000000000);

            settle_expired_position(0, &mut shorter, &maker_key, &mut maps, &clock, &state)
                .unwrap();

            // shorts lose
            assert_eq!(orig_short_balance, 200000000000000);
            assert_eq!(shorter.spot_positions[0].scaled_balance, 198992601400000);

            assert_eq!(
                shorter.spot_positions[0].scaled_balance < orig_short_balance,
                true
            );

            let shorter_loss = orig_short_balance - shorter.spot_positions[0].scaled_balance;
            assert_eq!(shorter_loss, 1007398600000); //$1020 loss

            let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
            assert_eq!(market.pnl_pool.scaled_balance, 2007398600000); //$2020
            assert_eq!(market.amm.fee_pool.scaled_balance, 0);
            drop(market);

            assert_eq!(shorter.perp_positions[0].open_orders, 0);
            assert_eq!(shorter.perp_positions[0].base_asset_amount, 0);
            assert_eq!(shorter.perp_positions[0].quote_asset_amount, 0);
            assert_eq!(shorter.perp_positions[0].quote_entry_amount, 0);
            assert_eq!(shorter.perp_positions[0].quote_break_even_amount, 0);
        }

        let MarginCalculation {
            margin_requirement,
            total_collateral,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &longer,
            &mut maps,
            MarginContext::standard(MarginRequirementType::Maintenance),
        )
        .unwrap();

        assert_eq!(total_collateral, 20000000000);
        assert_eq!(margin_requirement, 10000);

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.pnl_pool.scaled_balance, 2007398600000);
        assert_eq!(longer.spot_positions[0].scaled_balance, 20000000000000);
        assert_eq!(longer.perp_positions[0].quote_asset_amount, -40001000000);
        drop(market);

        settle_expired_position(0, &mut longer, &taker_key, &mut maps, &clock, &state).unwrap();

        assert_eq!(longer.spot_positions[0].scaled_balance > 100000000000, true);
        assert_eq!(longer.spot_positions[0].scaled_balance, 21980198800000);

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.pnl_pool.scaled_balance, 27199800000); //fee from settling
        assert_eq!(market.amm.fee_pool.scaled_balance, 0);
        drop(market);

        assert_eq!(longer.perp_positions[0].open_orders, 0);
        assert_eq!(longer.perp_positions[0].base_asset_amount, 0);
        assert_eq!(longer.perp_positions[0].quote_asset_amount, 0);
        assert_eq!(longer.perp_positions[0].quote_entry_amount, 0);
        assert_eq!(longer.perp_positions[0].quote_break_even_amount, 0);

        assert_eq!(liq.spot_positions[0].scaled_balance, 0);
        assert_eq!(liq.perp_positions[0].base_asset_amount, 0);
        assert_eq!(
            liq.perp_positions[0].quote_asset_amount,
            QUOTE_PRECISION_I64 * 2
        );
        assert_eq!(liq.perp_positions[0].quote_break_even_amount, 0);

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.number_of_users_with_base, 0);
        assert_eq!(market.quote_asset_amount, 2000000);
        drop(market);
        settle_expired_position(0, &mut liq, &liq_key, &mut maps, &clock, &state).unwrap();
        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.number_of_users_with_base, 0);
        drop(market);

        assert_eq!(liq.perp_positions[0].base_asset_amount, 0);
        assert_eq!(liq.perp_positions[0].quote_asset_amount, 0);
        assert_eq!(liq.perp_positions[0].quote_entry_amount, 0);
        assert_eq!(liq.perp_positions[0].quote_break_even_amount, 0);
        assert_eq!(liq.spot_positions[0].scaled_balance > 0, true);

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.base_asset_amount_long, 0);
        assert_eq!(market.base_asset_amount_short, 0);
        assert_eq!(market.number_of_users_with_base, 0);
        assert_eq!(market.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market.quote_asset_amount, 0);
        assert_eq!(market.total_social_loss, 0);
        drop(market);
    }

    #[test]
    fn delist_market_with_1000_balance_shorts_owe_longs_long_close_first() {
        let slot = 0_u64;
        let clock = Clock {
            slot: 6893025720,
            epoch_start_timestamp: 1662065595 - 1000,
            epoch: 2424,
            leader_schedule_epoch: 1662065595 - 1,
            unix_timestamp: 1662065595,
        };

        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map =
            OracleMap::load_one(&oracle_account_info, slot, SlotClock::baseline(), None).unwrap();

        // net users are short
        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                base_asset_amount_with_amm: -((AMM_RESERVE_PRECISION * 800) as i128),
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                amm_jit_intensity: 100,
                total_fee_minus_distributions: 0,
                ..AMM::default()
            },
            number_of_users_with_base: 2,
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            pnl_pool: PoolBalance {
                scaled_balance: (1000 * SPOT_BALANCE_PRECISION),
                market_index: QUOTE_SPOT_MARKET_INDEX,
                ..PoolBalance::default()
            },
            expiry_ts: clock.unix_timestamp - 10, // past expiry time

            base_asset_amount_long: (AMM_RESERVE_PRECISION * 200) as i128,
            base_asset_amount_short: -((AMM_RESERVE_PRECISION * 1000) as i128),
            order_step_size: 10000000,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            quote_asset_amount: (QUOTE_PRECISION_I128 * 200) + (QUOTE_PRECISION_I128 * 97 * 1000),
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: (99 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap_5min: (99 * PRICE_PRECISION) as i64,

                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u128::MAX;
        market.amm.min_base_asset_reserve = 0;

        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            borrow_balance: 100 * SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos =
            Vec::from([&spot_market_account_info, &sol_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);

        // taker wants to go long (would improve balance)
        let mut longer = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 100 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                base_asset_amount: (BASE_PRECISION_I64 * 200),
                quote_entry_amount: (QUOTE_PRECISION_I64 * 2000), //longs have -$1 cost basis,
                quote_break_even_amount: (QUOTE_PRECISION_I64 * 2000), //longs have -$1 cost basis,
                quote_asset_amount: (QUOTE_PRECISION_I64 * 2000), //longs have -$1 cost basis,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 20000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let mut shorter = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64 / 2,
                price: 100 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64 / 2,
                base_asset_amount: -(BASE_PRECISION_I64 * 1000),
                quote_entry_amount: (QUOTE_PRECISION_I64 * 97 * 1000), //shorts have $20 cost basis,
                quote_break_even_amount: (QUOTE_PRECISION_I64 * 97 * 1000), //shorts have $20 cost basis,
                quote_asset_amount: (QUOTE_PRECISION_I64 * 97 * 1000), //shorts have $20 cost basis,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 20000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        shorter.spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        let (taker_key, _maker_key, _filler_key) = get_user_keys();

        let state = State {
            oracle_guard_rails: OracleGuardRails {
                validity: ValidityGuardRails {
                    slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4s
                    slots_before_stale_for_margin: legacy_slot_duration_i64(120), // 48s
                    confidence_interval_max_size: 1000,
                    too_volatile_ratio: 5,
                },
                ..OracleGuardRails::default()
            },
            ..State::default()
        };

        // expiry time
        assert_eq!(market.expiry_ts < clock.unix_timestamp, true);
        assert_eq!(market.status, MarketStatus::Initialized);
        assert_eq!(market.expiry_price, 0);

        let MarginCalculation {
            margin_requirement,
            total_collateral,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &longer,
            &mut maps,
            MarginContext::standard(MarginRequirementType::Maintenance),
        )
        .unwrap();

        assert_eq!(total_collateral, 20000000000);
        assert_eq!(margin_requirement, 1005010000);

        let MarginCalculation {
            margin_requirement: margin_requirement_short,
            total_collateral: total_collateral_short,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &shorter,
            &mut maps,
            MarginContext::standard(MarginRequirementType::Maintenance),
        )
        .unwrap();

        assert_eq!(total_collateral_short, 17_000_000_000);
        assert_eq!(margin_requirement_short, 16002510000);
        assert_eq!(market.is_in_settlement(clock.unix_timestamp), true);
        assert_eq!(market.is_reduce_only().unwrap(), false);

        // put in settlement mode
        settle_expired_market(0, &mut maps, &state, &clock).unwrap();
        assert_eq!(market.is_reduce_only().unwrap(), false);
        assert_eq!(market.is_in_settlement(clock.unix_timestamp), true);

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.expiry_price != 0, true);
        assert_eq!(market.expiry_price, 120250001); //$120.25 (vs $100)
        assert_eq!(market.status, MarketStatus::Settlement);
        drop(market);

        // try long close
        {
            let MarginCalculation {
                margin_requirement,
                total_collateral,
                ..
            } = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &longer,
                &mut maps,
                MarginContext::standard(MarginRequirementType::Maintenance),
            )
            .unwrap();

            assert_eq!(total_collateral, 20000000000);
            assert_eq!(margin_requirement, 10000);

            let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
            assert_eq!(market.pnl_pool.scaled_balance, 1000000000000);
            assert_eq!(longer.spot_positions[0].scaled_balance, 20000000000000);
            assert_eq!(longer.perp_positions[0].quote_asset_amount, 2000000000);
            let longer_balance_before = longer.spot_positions[0].scaled_balance;
            drop(market);

            // not enough pnl pool
            assert_eq!(
                settle_expired_position(0, &mut longer, &taker_key, &mut maps, &clock, &state)
                    .is_err(),
                true
            );

            assert_eq!(
                longer.spot_positions[0].scaled_balance == longer_balance_before,
                true
            );

            let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
            assert_eq!(market.pnl_pool.scaled_balance, 1000000000000);
            assert_eq!(market.amm.fee_pool.scaled_balance, 0);
            drop(market);
        }

        // do short close
        // {
        //     assert_eq!(shorter.orders[0].order_id, 0);
        //     assert_eq!(shorter.orders[0].status, OrderStatus::Open);
        //     assert_eq!(shorter.orders[0].base_asset_amount, 5000000000000);

        //     cancel_order(
        //         0,
        //         &mut shorter,
        //         &maker_key,
        //         &maps.perp_market_map,
        //         &mut maps.oracle_map,
        //         clock.unix_timestamp,
        //         clock.slot,
        //         OrderActionExplanation::None,
        //         None,
        //         0,
        //         true,
        //     )
        //     .unwrap();

        //     let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        //     assert_eq!(market.pnl_pool.scaled_balance, 0);

        //     let orig_short_balance = shorter.spot_positions[0].scaled_balance;

        //     assert_eq!(orig_short_balance, 20000000000);
        //     assert_eq!(shorter.perp_positions[0].base_asset_amount, -10000000000000000);
        //     assert_eq!(shorter.perp_positions[0].quote_asset_amount, 97000000000);

        //     let oracle_price_data = maps.oracle_map.get_price_data(&market.oracle).unwrap();

        //     let (perp_margin_requirement, weighted_pnl) = calculate_perp_position_value_and_pnl(
        //         &shorter.perp_positions[0],
        //         &market,
        //         oracle_price_data,
        //         MarginRequirementType::Initial,
        //     ).unwrap();

        //     // short cant pay without bankruptcy
        //     assert_eq!(oracle_price_data.price, 1000000000000);
        //     assert_eq!(perp_margin_requirement, 12025000000);
        //     assert_eq!(weighted_pnl,           -23250000000);
        //     drop(market);

        //     settle_expired_position(
        //         0,
        //         &mut shorter,
        //         &maker_key,
        //         &maps.perp_market_map,
        //         &maps.spot_market_map,
        //         &mut maps.oracle_map,
        //         clock.unix_timestamp,
        //         &state.fee_structure,
        //     )
        //     .unwrap();

        //     assert_eq!(shorter.spot_positions[0].scaled_balance, 3370250001);
        //     assert_eq!(shorter.spot_positions[0].balance_type, SpotBalanceType::Borrow); // bad news

        //     let shorter_loss = orig_short_balance - shorter.spot_positions[0].scaled_balance;
        //     assert_eq!(shorter_loss, 16_629_749_999); //$16629 loss

        //     let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        //     assert_eq!(market.pnl_pool.scaled_balance, 23370250000); //$23370
        //     assert_eq!(market.amm.fee_pool.scaled_balance, 0);
        //     drop(market);

        //     assert_eq!(shorter.perp_positions[0].open_orders, 0);
        //     assert_eq!(shorter.perp_positions[0].base_asset_amount, 0);
        //     assert_eq!(shorter.perp_positions[0].quote_asset_amount, 0);
        //     assert_eq!(shorter.perp_positions[0].quote_break_even_amount, 0);
        // }
    }

    #[test]
    fn delist_market_with_1000_balance_shorts_owe_longs_short_close_first() {
        let slot = 0_u64;
        let clock = Clock {
            slot: 6893025720,
            epoch_start_timestamp: 1662065595 - 1000,
            epoch: 2424,
            leader_schedule_epoch: 1662065595 - 1,
            unix_timestamp: 1662065595,
        };

        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map =
            OracleMap::load_one(&oracle_account_info, slot, SlotClock::baseline(), None).unwrap();

        // net users are short
        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                base_asset_amount_with_amm: -((AMM_RESERVE_PRECISION * 800) as i128),
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                amm_jit_intensity: 100,
                total_fee_minus_distributions: 0,
                ..AMM::default()
            },
            number_of_users_with_base: 2,
            number_of_users: 2,
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            pnl_pool: PoolBalance {
                scaled_balance: (1000 * SPOT_BALANCE_PRECISION),
                market_index: QUOTE_SPOT_MARKET_INDEX,
                ..PoolBalance::default()
            },
            expiry_ts: clock.unix_timestamp - 10, // past expiry time

            base_asset_amount_long: (AMM_RESERVE_PRECISION * 200) as i128,
            base_asset_amount_short: -((AMM_RESERVE_PRECISION * 1000) as i128),
            order_step_size: 10000000,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            quote_asset_amount: QUOTE_PRECISION_I128 * (97 * 1000 + 200),
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: (99 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap_5min: (99 * PRICE_PRECISION) as i64,

                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u128::MAX;
        market.amm.min_base_asset_reserve = 0;

        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 40000 * SPOT_BALANCE_PRECISION,
            borrow_balance: 100 * SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (oracle_price.price * 99 / 100),

                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos =
            Vec::from([&spot_market_account_info, &sol_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);

        // taker wants to go long (would improve balance)
        let mut longer = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 100 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                base_asset_amount: (BASE_PRECISION_I64 * 200),
                quote_entry_amount: (QUOTE_PRECISION_I64 * 200), //longs have -$1 cost basis,
                quote_break_even_amount: (QUOTE_PRECISION_I64 * 200), //longs have -$1 cost basis,
                quote_asset_amount: (QUOTE_PRECISION_I64 * 200), //longs have -$1 cost basis,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 20000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let mut shorter = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64 / 2,
                price: 100 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64 / 2,
                base_asset_amount: -(BASE_PRECISION_I64 * 1000),
                quote_entry_amount: (QUOTE_PRECISION_I64 * 97 * 1000), //shorts have $20 cost basis,
                quote_break_even_amount: (QUOTE_PRECISION_I64 * 97 * 1000), //shorts have $20 cost basis,
                quote_asset_amount: (QUOTE_PRECISION_I64 * 97 * 1000), //shorts have $20 cost basis,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 20000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };
        shorter.spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 20000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        // let mut filler = User::default();

        let (taker_key, maker_key, liq_key) = get_user_keys();

        let state = State {
            oracle_guard_rails: OracleGuardRails {
                validity: ValidityGuardRails {
                    slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4s
                    slots_before_stale_for_margin: legacy_slot_duration_i64(120), // 48s
                    confidence_interval_max_size: 1000,
                    too_volatile_ratio: 5,
                },
                ..OracleGuardRails::default()
            },
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: legacy_slot_duration_u8(150),
            ..State::default()
        };

        let MarginCalculation {
            margin_requirement,
            total_collateral,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &longer,
            &mut maps,
            MarginContext::standard(MarginRequirementType::Maintenance),
        )
        .unwrap();

        assert_eq!(total_collateral, 20000000000);
        assert_eq!(margin_requirement, 1005010000);

        // put in settlement mode
        settle_expired_market(0, &mut maps, &state, &clock).unwrap();

        let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
        assert_eq!(market.expiry_price != 0, true);
        assert_eq!(market.expiry_price, 120250001); //$120.25 (vs $100)
        assert_eq!(market.status, MarketStatus::Settlement);
        drop(market);

        // do short liquidation
        {
            assert_eq!(shorter.orders[0].order_id, 0);
            assert_eq!(shorter.orders[0].status, OrderStatus::Open);
            assert_eq!(shorter.orders[0].base_asset_amount, 500000000);

            cancel_order(
                0,
                &mut shorter,
                &maker_key,
                &mut maps,
                clock.unix_timestamp,
                clock.slot,
                OrderActionExplanation::None,
                None,
                0,
                true,
            )
            .unwrap();

            let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
            assert_eq!(market.pnl_pool.scaled_balance, 1000000000000);

            let orig_short_balance = shorter.spot_positions[0].scaled_balance;

            assert_eq!(orig_short_balance, 20000000000000);
            assert_eq!(shorter.perp_positions[0].base_asset_amount, -1000000000000);
            assert_eq!(shorter.perp_positions[0].quote_asset_amount, 97000000000);

            let oracle_price_data = maps.oracle_map.get_price_data(&market.oracle_id()).unwrap();

            let strict_quote_price = StrictOraclePrice::test(QUOTE_PRECISION_I64);
            let (perp_margin_requirement, weighted_pnl, _, _) =
                calculate_perp_position_value_and_pnl(
                    &shorter.perp_positions[0],
                    &market,
                    oracle_price_data,
                    &strict_quote_price,
                    MarginRequirementType::Initial,
                    0,
                )
                .unwrap();

            // short cant pay without bankruptcy
            assert_eq!(oracle_price_data.price, 100000000);
            assert_eq!(perp_margin_requirement, 0);
            assert_eq!(weighted_pnl, -23250001000);
            drop(market);

            assert!(settle_expired_position(
                0,
                &mut shorter,
                &maker_key,
                &mut maps,
                &clock,
                &state,
            )
            .is_err());

            let market = maps.perp_market_map.get_ref_mut(&0).unwrap();

            assert_eq!(longer.spot_positions[0].scaled_balance, 20000000000000);
            assert_eq!(longer.perp_positions[0].quote_asset_amount, 200000000);

            assert_eq!(
                market.base_asset_amount_long + market.base_asset_amount_short,
                -800000000000
            );
            assert_eq!(market.quote_asset_amount, 97200000000);

            drop(market);

            let mut shorter_user_stats = UserStats::default();
            let mut liq_user_stats = UserStats::default();

            assert_eq!(shorter.is_cross_margin_being_liquidated(), false);
            assert_eq!(shorter.is_cross_margin_bankrupt(), false);
            let state = State {
                liquidation_margin_buffer_ratio: 10,
                ..Default::default()
            };
            liquidate_perp(
                0,
                shorter.perp_positions[0].base_asset_amount.unsigned_abs(),
                None,
                &mut shorter,
                &maker_key,
                &mut shorter_user_stats,
                &mut liquidator,
                &liq_key,
                &mut liq_user_stats,
                &mut maps,
                clock.slot,
                clock.unix_timestamp,
                &state,
            )
            .unwrap();

            assert_eq!(shorter.is_cross_margin_being_liquidated(), true);
            assert_eq!(shorter.is_cross_margin_bankrupt(), false);

            {
                let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
                let oracle_price_data =
                    maps.oracle_map.get_price_data(&market.oracle_id()).unwrap();

                let strict_quote_price = StrictOraclePrice::test(QUOTE_PRECISION_I64);
                let (perp_margin_requirement, weighted_pnl, _, _) =
                    calculate_perp_position_value_and_pnl(
                        &shorter.perp_positions[0],
                        &market,
                        oracle_price_data,
                        &strict_quote_price,
                        MarginRequirementType::Initial,
                        0,
                    )
                    .unwrap();

                // short cant pay without bankruptcy
                assert_eq!(shorter.spot_positions[0].scaled_balance, 20000000000000);
                assert_eq!(
                    shorter.spot_positions[0].balance_type,
                    SpotBalanceType::Deposit
                );
                assert_eq!(oracle_price_data.price, 100000000);
                assert_eq!(perp_margin_requirement, 0);
                assert_eq!(weighted_pnl, -23250001000);

                assert_eq!(longer.spot_positions[0].scaled_balance, 20000000000000);
                assert_eq!(longer.perp_positions[0].quote_asset_amount, 200000000);

                assert_eq!(
                    market.base_asset_amount_long + market.base_asset_amount_short,
                    -800000000000
                );
                assert_eq!(market.quote_asset_amount, 97200000000);

                assert_eq!(shorter.perp_positions[0].base_asset_amount, 0);
                assert_eq!(shorter.perp_positions[0].quote_asset_amount, -23250001000);

                assert_eq!(
                    liquidator.perp_positions[0].base_asset_amount as i128,
                    market.base_asset_amount_short
                );
                assert_eq!(
                    liquidator.perp_positions[0].quote_asset_amount,
                    // market.amm.quote_asset_amount_short
                    97000000000 + 23250001000
                );

                assert_eq!(
                    longer.perp_positions[0].base_asset_amount as i128,
                    market.base_asset_amount_long
                );

                assert_eq!(market.quote_asset_amount, 97200000000);

                drop(market);
            }

            liquidate_perp_pnl_for_deposit(
                0,
                0,
                QUOTE_PRECISION_I128 as u128,
                None,
                &mut shorter,
                &maker_key,
                &mut liquidator,
                &liq_key,
                &mut maps,
                clock.unix_timestamp,
                clock.slot,
                10,
                PERCENTAGE_PRECISION,
                Millis::from_stored_units(150),
                false,
            )
            .unwrap();

            assert_eq!(shorter.is_cross_margin_being_liquidated(), true);
            assert_eq!(shorter.is_cross_margin_bankrupt(), false);

            {
                let mut market = maps.perp_market_map.get_ref_mut(&0).unwrap();
                let oracle_price_data =
                    maps.oracle_map.get_price_data(&market.oracle_id()).unwrap();

                assert_eq!(market.quote_asset_amount, 97200000000);

                assert_eq!(market.cumulative_funding_rate_long, 0);
                assert_eq!(market.cumulative_funding_rate_short, 0);

                let strict_quote_price = StrictOraclePrice::test(QUOTE_PRECISION_I64);
                let (perp_margin_requirement, weighted_pnl, _, _) =
                    calculate_perp_position_value_and_pnl(
                        &shorter.perp_positions[0],
                        &market,
                        oracle_price_data,
                        &strict_quote_price,
                        MarginRequirementType::Initial,
                        0,
                    )
                    .unwrap();

                // short cant pay without bankruptcy
                assert_eq!(shorter.spot_positions[0].scaled_balance, 19999000000000);
                assert_eq!(
                    shorter.spot_positions[0].balance_type,
                    SpotBalanceType::Deposit
                );
                assert_eq!(oracle_price_data.price, 100000000);
                assert_eq!(perp_margin_requirement, 0);
                assert_eq!(weighted_pnl, -23249001000);

                assert_eq!(longer.spot_positions[0].scaled_balance, 20000000000000);
                assert_eq!(longer.perp_positions[0].quote_asset_amount, 200000000);

                assert_eq!(
                    market.base_asset_amount_long + market.base_asset_amount_short,
                    -800000000000
                );
                assert_eq!(market.quote_asset_amount, 97200000000);

                assert_eq!(shorter.perp_positions[0].base_asset_amount, 0);
                assert_eq!(shorter.perp_positions[0].quote_asset_amount, -23249001000);

                assert_eq!(
                    liquidator.perp_positions[0].base_asset_amount as i128,
                    market.base_asset_amount_short
                );

                assert_eq!(
                    liquidator.perp_positions[0].quote_asset_amount,
                    120249001000
                );

                assert_eq!(
                    longer.perp_positions[0].base_asset_amount as i128,
                    market.base_asset_amount_long
                );
                assert_eq!(longer.perp_positions[0].quote_asset_amount, 200000000);

                assert_eq!(market.quote_asset_amount, 201000000 + 96999000000);

                // add a liq fee now
                market.liquidator_fee = 10000;

                drop(market);
            }

            liquidate_perp_pnl_for_deposit(
                0,
                0,
                (QUOTE_PRECISION_I128 * 1000000000) as u128, // give all
                None,
                &mut shorter,
                &maker_key,
                &mut liquidator,
                &liq_key,
                &mut maps,
                clock.unix_timestamp,
                clock.slot,
                10,
                PERCENTAGE_PRECISION,
                Millis::from_stored_units(150),
                false,
            )
            .unwrap();

            assert_eq!(shorter.is_cross_margin_being_liquidated(), true);
            assert_eq!(shorter.is_cross_margin_bankrupt(), true);

            {
                let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
                let oracle_price_data =
                    maps.oracle_map.get_price_data(&market.oracle_id()).unwrap();

                assert_eq!(market.quote_asset_amount, 20000010000 + 77199990000);

                assert_eq!(market.cumulative_funding_rate_long, 0);
                assert_eq!(market.cumulative_funding_rate_short, 0);

                let strict_quote_price = StrictOraclePrice::test(QUOTE_PRECISION_I64);
                let (perp_margin_requirement, weighted_pnl, _, _) =
                    calculate_perp_position_value_and_pnl(
                        &shorter.perp_positions[0],
                        &market,
                        oracle_price_data,
                        &strict_quote_price,
                        MarginRequirementType::Initial,
                        0,
                    )
                    .unwrap();

                // short cant pay without bankruptcy
                assert_eq!(shorter.spot_positions[0].scaled_balance, 0);
                assert_eq!(
                    shorter.spot_positions[0].balance_type,
                    SpotBalanceType::Deposit
                );
                assert_eq!(oracle_price_data.price, 100000000);
                assert_eq!(perp_margin_requirement, 0);
                assert_eq!(weighted_pnl, -3449991000);

                assert_eq!(longer.spot_positions[0].scaled_balance, 20000000000000);
                assert_eq!(longer.perp_positions[0].quote_asset_amount, 200000000);

                assert_eq!(
                    market.base_asset_amount_long + market.base_asset_amount_short,
                    -800000000000
                );
                assert_eq!(market.quote_asset_amount, 97200000000);

                assert_eq!(shorter.perp_positions[0].base_asset_amount, 0);
                assert_eq!(shorter.perp_positions[0].quote_asset_amount, -3449991000);

                assert_eq!(
                    liquidator.perp_positions[0].base_asset_amount as i128,
                    market.base_asset_amount_short
                );
                assert_eq!(
                    liquidator.perp_positions[0].quote_asset_amount,
                    100449991000
                );

                assert_eq!(
                    longer.perp_positions[0].base_asset_amount as i128,
                    market.base_asset_amount_long
                );
                assert_eq!(longer.perp_positions[0].quote_asset_amount, 200000000,);

                assert_eq!(market.quote_asset_amount, 20000010000 + 77199990000);
                assert_eq!(market.total_social_loss, 0);

                drop(market);
            }

            assert_eq!(liquidator.spot_positions[0].scaled_balance, 40000000000000);
            assert_eq!(
                liquidator.spot_positions[0].balance_type,
                SpotBalanceType::Deposit
            );
            assert_eq!(
                liquidator.perp_positions[0].base_asset_amount,
                -1000000000000
            );
            assert_eq!(
                liquidator.perp_positions[0].quote_asset_amount,
                100449991000
            );
            assert_eq!(
                liquidator.perp_positions[0].quote_entry_amount,
                120250001000
            );
            assert_eq!(
                liquidator.perp_positions[0].quote_break_even_amount,
                120250001000
            );
            assert_eq!(liquidator.perp_positions[0].open_orders, 0);

            settle_expired_position(0, &mut liquidator, &liq_key, &mut maps, &clock, &state)
                .unwrap();

            assert_eq!(liquidator.spot_positions[0].scaled_balance, 20151889999000);
            // avoid the social loss :p
            // made 79 bucks

            assert_eq!(
                liquidator.spot_positions[0].balance_type,
                SpotBalanceType::Deposit
            );

            resolve_perp_bankruptcy(
                0,
                &mut shorter,
                &maker_key,
                &mut liquidator,
                &liq_key,
                &mut maps,
                clock.unix_timestamp,
                0,
                false,
            )
            .unwrap();

            assert_eq!(shorter.perp_positions[0].base_asset_amount, 0);
            assert_eq!(
                shorter.spot_positions[0].scaled_balance < orig_short_balance,
                true
            );
            assert_eq!(shorter.spot_positions[0].scaled_balance, 0);

            let shorter_loss = orig_short_balance - shorter.spot_positions[0].scaled_balance;
            assert_eq!(shorter_loss, 20000000000000); //$16629 loss

            let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
            assert_eq!(market.total_social_loss, 3449991000);
            assert_eq!(market.base_asset_amount_long, 200000000000);
            assert_eq!(market.base_asset_amount_short, 0);
            assert_eq!(market.amm.base_asset_amount_with_amm, 200000000000);

            assert_eq!(market.cumulative_funding_rate_long, 17249955000);
            assert_eq!(market.cumulative_funding_rate_short, -17249955000);

            assert_eq!(market.pnl_pool.scaled_balance, 20848110001000); //$20920
            assert_eq!(market.amm.fee_pool.scaled_balance, 0);
            drop(market);

            assert_eq!(shorter.perp_positions[0].open_orders, 0);
            assert_eq!(shorter.perp_positions[0].base_asset_amount, 0);
            assert_eq!(shorter.perp_positions[0].quote_entry_amount, 0);
            assert_eq!(shorter.perp_positions[0].quote_asset_amount, 0);
            assert_eq!(shorter.perp_positions[0].quote_break_even_amount, 0);

            assert_eq!(longer.perp_positions[0].quote_asset_amount, 200000000);
        }

        // do long close
        {
            let MarginCalculation {
                margin_requirement,
                total_collateral,
                ..
            } = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &longer,
                &mut maps,
                MarginContext::standard(MarginRequirementType::Maintenance),
            )
            .unwrap();

            assert_eq!(total_collateral, 20000000000);
            assert_eq!(margin_requirement, 10000);
            assert_eq!(longer.spot_positions[0].scaled_balance, 20000000000000);
            assert_eq!(longer.perp_positions[0].last_cumulative_funding_rate, 0);
            assert_eq!(longer.perp_positions[0].quote_asset_amount, 200000000);

            // with open orders fails
            cancel_order(
                0,
                &mut longer,
                &taker_key,
                &mut maps,
                clock.unix_timestamp,
                clock.slot,
                OrderActionExplanation::None,
                None,
                0,
                true,
            )
            .unwrap();

            let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
            assert_eq!(market.pnl_pool.scaled_balance, 20848110001000);
            assert_eq!(longer.spot_positions[0].scaled_balance, 20000000000000);
            assert_eq!(longer.perp_positions[0].quote_asset_amount, 200000000);
            assert_eq!(longer.perp_positions[0].quote_asset_amount, 200000000);
            assert_eq!(longer.perp_positions[0].last_cumulative_funding_rate, 0);

            assert_eq!(market.cumulative_funding_rate_long, 17249955000);
            let longer_funding_payment = calculate_funding_payment(
                market.cumulative_funding_rate_long,
                &longer.perp_positions[0],
            )
            .unwrap();
            assert_eq!(longer_funding_payment, -3449991000);

            assert_eq!(market.quote_asset_amount, 200000000);
            assert_eq!(market.total_social_loss, 3449991000);

            drop(market);

            settle_expired_position(0, &mut longer, &taker_key, &mut maps, &clock, &state).unwrap();
            assert_eq!(longer.perp_positions[0].quote_asset_amount, 0);
            assert_eq!(longer.perp_positions[0].base_asset_amount, 0);
            assert_eq!(longer.perp_positions[0].last_cumulative_funding_rate, 0);

            assert_eq!(longer.spot_positions[0].scaled_balance > 100000000000, true);
            assert_eq!(longer.spot_positions[0].scaled_balance, 40790389199000); //$40775

            let market = maps.perp_market_map.get_ref_mut(&0).unwrap();
            assert_eq!(market.pnl_pool.scaled_balance, 57720802000); // fees collected
            assert_eq!(market.amm.fee_pool.scaled_balance, 0);

            assert_eq!(market.number_of_users_with_base, 0);
            assert_eq!(market.base_asset_amount_long, 0);
            assert_eq!(market.base_asset_amount_short, 0);

            assert_eq!(market.amm.base_asset_amount_with_amm, 0);

            assert_eq!(market.quote_asset_amount, 0);

            assert_eq!(market.total_social_loss, 3449991000);

            let oracle_price_data = maps.oracle_map.get_price_data(&market.oracle_id()).unwrap();
            assert_eq!(oracle_price_data.price, 100 * PRICE_PRECISION_I64);
            let net_pnl = calculate_net_user_pnl(
                &market.amm,
                oracle_price_data.price,
                market.quote_asset_amount,
                market.net_unsettled_funding_pnl,
            )
            .unwrap();
            // market fully closed and settled: the socialized loss was
            // recorded into net_unsettled at bankruptcy and drained back to
            // zero as the long settled it, leaving no residual user claim.
            assert_eq!(net_pnl, 0);
            assert_eq!(market.net_unsettled_funding_pnl, 0);

            drop(market);

            assert_eq!(longer.perp_positions[0].open_orders, 0);
            assert_eq!(longer.perp_positions[0].base_asset_amount, 0);
            assert_eq!(longer.perp_positions[0].quote_asset_amount, 0);
            assert_eq!(longer.perp_positions[0].quote_entry_amount, 0);
            assert_eq!(longer.perp_positions[0].quote_break_even_amount, 0);
            assert_eq!(longer.perp_positions[0].last_cumulative_funding_rate, 0);
        }
    }
}
