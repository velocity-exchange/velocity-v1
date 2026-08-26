pub mod deposit_into_isolated_perp_position {
    use {
        crate::{
            controller::isolated_position::deposit_into_isolated_perp_position,
            create_anchor_account_info,
            error::ErrorCode,
            math::constants::{
                AMM_RESERVE_PRECISION, BASE_PRECISION_I128, LIQUIDATION_FEE_PRECISION,
                PEG_PRECISION, QUOTE_PRECISION_I128, QUOTE_PRECISION_U64,
                SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
            },
            state::{
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market::{MarketStats, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::SpotMarket,
                spot_market_map::SpotMarketMap,
                state::State,
                user::{PerpPosition, PositionFlag, User},
            },
            test_utils::get_pyth_price,
            PRICE_PRECISION_I64, SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    #[test]
    pub fn successful_deposit_into_isolated_perp_position() {
        let now = 0_i64;
        let slot = 0_u64;

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
            slot,
            crate::math::time::SlotClock::baseline(),
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
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            status: MarketStatus::Active,
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User::default();

        let user_key = Pubkey::default();

        let state = State::default();
        deposit_into_isolated_perp_position(
            user_key,
            &mut user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
            0,
            0,
            QUOTE_PRECISION_U64,
        )
        .unwrap();

        assert_eq!(
            user.perp_positions[0].isolated_position_scaled_balance,
            1000000000
        );
        assert_eq!(
            user.perp_positions[0].position_flag,
            PositionFlag::IsolatedPosition as u8
        );
    }

    #[test]
    pub fn fail_to_deposit_into_existing_perp_position() {
        let now = 0_i64;
        let slot = 0_u64;

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
            slot,
            crate::math::time::SlotClock::baseline(),
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
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            status: MarketStatus::Active,
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User::default();
        user.perp_positions[0] = PerpPosition {
            market_index: 0,
            open_orders: 1,
            ..PerpPosition::default()
        };

        let user_key = Pubkey::default();

        let state = State::default();
        let result = deposit_into_isolated_perp_position(
            user_key,
            &mut user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
            0,
            0,
            QUOTE_PRECISION_U64,
        );

        assert_eq!(result, Err(ErrorCode::InvalidPerpPosition));
    }

    #[test]
    pub fn deposit_into_isolated_perp_position_respects_daily_deposit_cap() {
        // The isolated deposit credits the same spot market vault as a
        // cross-margin deposit, so the daily deposit cap must bind on it. Before
        // this check existed this instruction was a free route around the cap.
        let now = 0_i64;
        let slot = 0_u64;

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
            slot,
            crate::math::time::SlotClock::baseline(),
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
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        // A 100 token deposit TWAP with a 1000 bps daily cap allows resulting
        // deposits up to 110 tokens. The market already holds 110 tokens, so it
        // sits on the cap.
        let market_deposit_balance = 110 * SPOT_BALANCE_PRECISION;
        let mut spot_market = SpotMarket {
            status: MarketStatus::Active,
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: market_deposit_balance,
            deposit_token_twap: 100 * QUOTE_PRECISION_U64,
            max_deposit_bps_per_day: 1_000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User::default();
        let user_key = Pubkey::default();
        let state = State::default();

        // One more token takes resulting deposits to 111 tokens, above the cap.
        let result = deposit_into_isolated_perp_position(
            user_key,
            &mut user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
            0,
            0,
            QUOTE_PRECISION_U64,
        );
        assert_eq!(result, Err(ErrorCode::DailyDepositLimit));

        // On chain the failed instruction reverts every write above. This test
        // calls the controller directly, so restore the pre-deposit state by hand
        // before the second run.
        user.perp_positions[0].isolated_position_scaled_balance = 0;
        user.total_deposits = 0;
        {
            let mut spot_market = spot_market_map.get_ref_mut(&0).unwrap();
            spot_market.deposit_balance = market_deposit_balance;
            // A cap of 0 disables the daily deposit limit. The same deposit is
            // then allowed, which proves the cap is what rejected it.
            spot_market.max_deposit_bps_per_day = 0;
        }

        deposit_into_isolated_perp_position(
            user_key,
            &mut user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
            0,
            0,
            QUOTE_PRECISION_U64,
        )
        .unwrap();

        assert_eq!(
            user.perp_positions[0].isolated_position_scaled_balance,
            SPOT_BALANCE_PRECISION_U64
        );
        assert_eq!(
            spot_market_map.get_ref(&0).unwrap().deposit_balance,
            market_deposit_balance + SPOT_BALANCE_PRECISION
        );
    }
}

pub mod transfer_isolated_perp_position_deposit {
    use {
        crate::{
            controller::isolated_position::transfer_isolated_perp_position_deposit,
            create_anchor_account_info,
            error::ErrorCode,
            math::constants::{
                AMM_RESERVE_PRECISION, BASE_PRECISION_I128, LIQUIDATION_FEE_PRECISION,
                PEG_PRECISION, QUOTE_PRECISION_I128, SPOT_CUMULATIVE_INTEREST_PRECISION,
                SPOT_WEIGHT_PRECISION,
            },
            state::{
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market::{MarketStats, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::SpotMarket,
                spot_market_map::SpotMarketMap,
                user::{PerpPosition, PositionFlag, SpotPosition, User, UserStats},
            },
            test_utils::get_pyth_price,
            PRICE_PRECISION_I64, QUOTE_PRECISION_I64, SPOT_BALANCE_PRECISION,
            SPOT_BALANCE_PRECISION_U64,
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    #[test]
    pub fn successful_transfer_to_isolated_perp_position() {
        let now = 0_i64;
        let slot = 0_u64;

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
            slot,
            crate::math::time::SlotClock::baseline(),
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
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            status: MarketStatus::Active,
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User::default();
        user.spot_positions[0] = SpotPosition {
            market_index: 0,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        let mut user_stats = UserStats::default();

        transfer_isolated_perp_position_deposit(
            &mut user,
            Some(&mut user_stats),
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            0,
            0,
            QUOTE_PRECISION_I64,
            false,
        )
        .unwrap();

        assert_eq!(
            user.perp_positions[0].isolated_position_scaled_balance,
            1000000000
        );
        assert_eq!(
            user.perp_positions[0].position_flag,
            PositionFlag::IsolatedPosition as u8
        );

        assert_eq!(user.spot_positions[0].scaled_balance, 0);
    }

    #[test]
    pub fn fail_to_transfer_to_existing_perp_position() {
        let now = 0_i64;
        let slot = 0_u64;

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
            slot,
            crate::math::time::SlotClock::baseline(),
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
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            status: MarketStatus::Active,
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User::default();
        user.spot_positions[0] = SpotPosition {
            market_index: 0,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        user.perp_positions[0] = PerpPosition {
            market_index: 0,
            open_orders: 1,
            ..PerpPosition::default()
        };

        let mut user_stats = UserStats::default();

        let result = transfer_isolated_perp_position_deposit(
            &mut user,
            Some(&mut user_stats),
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            0,
            0,
            QUOTE_PRECISION_I64,
            false,
        );

        assert_eq!(result, Err(ErrorCode::InvalidPerpPosition));
    }

    #[test]
    pub fn fail_to_transfer_due_to_insufficient_collateral() {
        let now = 0_i64;
        let slot = 0_u64;

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
            slot,
            crate::math::time::SlotClock::baseline(),
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
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            status: MarketStatus::Active,
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 2 * SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User::default();
        user.spot_positions[0] = SpotPosition {
            market_index: 0,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        let mut user_stats = UserStats::default();

        let result = transfer_isolated_perp_position_deposit(
            &mut user,
            Some(&mut user_stats),
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            0,
            0,
            2 * QUOTE_PRECISION_I64,
            false,
        );

        assert_eq!(result, Err(ErrorCode::InsufficientCollateral));
    }

    #[test]
    pub fn successful_transfer_from_isolated_perp_position() {
        let now = 0_i64;
        let slot = 0_u64;

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
            slot,
            crate::math::time::SlotClock::baseline(),
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
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            status: MarketStatus::Active,
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User::default();
        user.perp_positions[0] = PerpPosition {
            market_index: 0,
            isolated_position_scaled_balance: SPOT_BALANCE_PRECISION_U64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            ..PerpPosition::default()
        };

        let mut user_stats = UserStats::default();

        transfer_isolated_perp_position_deposit(
            &mut user,
            Some(&mut user_stats),
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            0,
            0,
            -QUOTE_PRECISION_I64,
            false,
        )
        .unwrap();

        assert_eq!(user.perp_positions[0].isolated_position_scaled_balance, 0);
        assert_eq!(
            user.perp_positions[0].position_flag,
            PositionFlag::IsolatedPosition as u8
        );

        assert_eq!(
            user.spot_positions[0].scaled_balance,
            SPOT_BALANCE_PRECISION_U64
        );
    }

    #[test]
    pub fn fail_transfer_from_non_isolated_perp_position() {
        let now = 0_i64;
        let slot = 0_u64;

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
            slot,
            crate::math::time::SlotClock::baseline(),
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
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            status: MarketStatus::Active,
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User::default();
        user.perp_positions[0] = PerpPosition {
            market_index: 0,
            open_orders: 1,
            ..PerpPosition::default()
        };

        let mut user_stats = UserStats::default();

        let result = transfer_isolated_perp_position_deposit(
            &mut user,
            Some(&mut user_stats),
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            0,
            0,
            -QUOTE_PRECISION_I64,
            false,
        );

        assert_eq!(result, Err(ErrorCode::InvalidPerpPosition));
    }

    #[test]
    pub fn fail_transfer_from_isolated_perp_position_due_to_insufficient_collateral() {
        let now = 0_i64;
        let slot = 0_u64;

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
            slot,
            crate::math::time::SlotClock::baseline(),
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
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            status: MarketStatus::Active,
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User::default();
        user.perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 100000,
            isolated_position_scaled_balance: SPOT_BALANCE_PRECISION_U64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            ..PerpPosition::default()
        };

        let mut user_stats = UserStats::default();

        let result = transfer_isolated_perp_position_deposit(
            &mut user,
            Some(&mut user_stats),
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            0,
            0,
            -QUOTE_PRECISION_I64,
            false,
        );

        assert_eq!(result, Err(ErrorCode::InsufficientCollateral));
    }
}

pub mod withdraw_from_isolated_perp_position {
    use {
        crate::{
            controller::isolated_position::withdraw_from_isolated_perp_position,
            create_anchor_account_info,
            error::ErrorCode,
            math::constants::{
                AMM_RESERVE_PRECISION, BASE_PRECISION_I128, LIQUIDATION_FEE_PRECISION,
                PEG_PRECISION, QUOTE_PRECISION_I128, QUOTE_PRECISION_U64,
                SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
            },
            state::{
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                paused_operations::SpotOperation,
                perp_market::{MarketStats, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::SpotMarket,
                spot_market_map::SpotMarketMap,
                user::{PerpPosition, PositionFlag, User, UserStats},
            },
            test_utils::get_pyth_price,
            PRICE_PRECISION_I64, SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    #[test]
    pub fn successful_withdraw_from_isolated_perp_position() {
        let now = 0_i64;
        let slot = 0_u64;

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
            slot,
            crate::math::time::SlotClock::baseline(),
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
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            status: MarketStatus::Active,
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User::default();
        user.perp_positions[0] = PerpPosition {
            market_index: 0,
            isolated_position_scaled_balance: SPOT_BALANCE_PRECISION_U64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            ..PerpPosition::default()
        };

        let user_key = Pubkey::default();

        let mut user_stats = UserStats::default();

        withdraw_from_isolated_perp_position(
            user_key,
            &mut user,
            &mut user_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            0,
            0,
            QUOTE_PRECISION_U64,
            false,
        )
        .unwrap();

        assert_eq!(user.perp_positions[0].isolated_position_scaled_balance, 0);
        assert_eq!(
            user.perp_positions[0].position_flag,
            PositionFlag::IsolatedPosition as u8
        );
    }

    #[test]
    pub fn withdraw_from_isolated_perp_position_fail_not_isolated_perp_position() {
        let now = 0_i64;
        let slot = 0_u64;

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
            slot,
            crate::math::time::SlotClock::baseline(),
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
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            status: MarketStatus::Active,
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User::default();
        user.perp_positions[0] = PerpPosition {
            market_index: 0,
            open_orders: 1,
            ..PerpPosition::default()
        };

        let user_key = Pubkey::default();

        let mut user_stats = UserStats::default();

        let result = withdraw_from_isolated_perp_position(
            user_key,
            &mut user,
            &mut user_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            0,
            0,
            QUOTE_PRECISION_U64,
            false,
        );

        assert_eq!(result, Err(ErrorCode::InvalidPerpPosition));
    }

    #[test]
    pub fn fail_withdraw_from_isolated_perp_position_due_to_insufficient_collateral() {
        let now = 0_i64;
        let slot = 0_u64;

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
            slot,
            crate::math::time::SlotClock::baseline(),
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
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            status: MarketStatus::Active,
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User::default();
        user.perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 100000,
            isolated_position_scaled_balance: SPOT_BALANCE_PRECISION_U64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            ..PerpPosition::default()
        };

        let user_key = Pubkey::default();

        let mut user_stats = UserStats::default();

        let result = withdraw_from_isolated_perp_position(
            user_key,
            &mut user,
            &mut user_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            0,
            0,
            QUOTE_PRECISION_U64,
            false,
        );

        assert_eq!(result, Err(ErrorCode::InsufficientCollateral));
    }
    #[test]
    pub fn withdraw_from_isolated_perp_position_respects_withdraw_circuit_breaker() {
        // The isolated withdraw path sends real tokens out of the spot market
        // vault, so the market withdraw circuit breaker must bind on it. Before
        // this check existed, one account of any size drained a market past a
        // tripped breaker, with no need to split the position.
        let now = 0_i64;
        let slot = 0_u64;

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
            slot,
            crate::math::time::SlotClock::baseline(),
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
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        // A 75 USDC market against a 100 USDC deposit TWAP. The default 2500 bps
        // breaker floors deposits at 75 USDC, so the market sits on its floor.
        let market_deposit_balance = 75 * SPOT_BALANCE_PRECISION;
        let mut spot_market = SpotMarket {
            status: MarketStatus::Active,
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: market_deposit_balance,
            deposit_token_twap: 100 * QUOTE_PRECISION_U64,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User::default();
        user.perp_positions[0] = PerpPosition {
            market_index: 0,
            isolated_position_scaled_balance: SPOT_BALANCE_PRECISION_U64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            ..PerpPosition::default()
        };

        let user_key = Pubkey::default();
        let mut user_stats = UserStats::default();

        // Taking 1 USDC out leaves 74 USDC, below the 75 USDC floor.
        let result = withdraw_from_isolated_perp_position(
            user_key,
            &mut user,
            &mut user_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            0,
            0,
            QUOTE_PRECISION_U64,
            false,
        );
        assert_eq!(result, Err(ErrorCode::DailyWithdrawLimit));

        // On chain the failed instruction reverts every write above. This test
        // calls the controller directly, so restore the pre-withdraw state by
        // hand before the second run.
        user.perp_positions[0].isolated_position_scaled_balance = SPOT_BALANCE_PRECISION_U64;
        user.total_withdraws = 0;
        {
            let mut spot_market = spot_market_map.get_ref_mut(&0).unwrap();
            spot_market.deposit_balance = market_deposit_balance;
            // A 90 USDC TWAP floors deposits at 67.5 USDC. The same withdrawal
            // now leaves 74 USDC, which is above the floor.
            spot_market.deposit_token_twap = 90 * QUOTE_PRECISION_U64;
        }

        withdraw_from_isolated_perp_position(
            user_key,
            &mut user,
            &mut user_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            0,
            0,
            QUOTE_PRECISION_U64,
            false,
        )
        .unwrap();

        assert_eq!(user.perp_positions[0].isolated_position_scaled_balance, 0);
        assert_eq!(
            spot_market_map.get_ref(&0).unwrap().deposit_balance,
            market_deposit_balance - SPOT_BALANCE_PRECISION
        );
    }
    #[test]
    pub fn withdraw_from_isolated_perp_position_respects_market_status_and_pause() {
        // The admin can stop withdrawals per market. Both gates must bind on the
        // isolated route out of the vault, not only on the cross-margin route.
        // The market TWAP is zero here, so the circuit breaker admits everything
        // and only the status and pause gates can reject.
        let now = 0_i64;
        let slot = 0_u64;

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
            slot,
            crate::math::time::SlotClock::baseline(),
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
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let market_deposit_balance = 75 * SPOT_BALANCE_PRECISION;
        let mut spot_market = SpotMarket {
            status: MarketStatus::Delisted,
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: market_deposit_balance,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User::default();
        user.perp_positions[0] = PerpPosition {
            market_index: 0,
            isolated_position_scaled_balance: SPOT_BALANCE_PRECISION_U64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            ..PerpPosition::default()
        };

        let user_key = Pubkey::default();
        let mut user_stats = UserStats::default();

        // A failed run still writes before it rejects, because this test calls
        // the controller directly and no transaction reverts it. Restore the
        // pre-withdraw state between runs.
        let mut restore = |user: &mut User| {
            user.perp_positions[0].isolated_position_scaled_balance = SPOT_BALANCE_PRECISION_U64;
            user.total_withdraws = 0;
            spot_market_map.get_ref_mut(&0).unwrap().deposit_balance = market_deposit_balance;
        };

        // Delisted is outside the admitted status set.
        let result = withdraw_from_isolated_perp_position(
            user_key,
            &mut user,
            &mut user_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            0,
            0,
            QUOTE_PRECISION_U64,
            false,
        );
        assert_eq!(result, Err(ErrorCode::MarketWithdrawPaused));
        restore(&mut user);

        // An active market with the Withdraw operation paused also rejects.
        {
            let mut spot_market = spot_market_map.get_ref_mut(&0).unwrap();
            spot_market.status = MarketStatus::Active;
            spot_market.paused_operations = SpotOperation::Withdraw as u8;
        }
        let result = withdraw_from_isolated_perp_position(
            user_key,
            &mut user,
            &mut user_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            0,
            0,
            QUOTE_PRECISION_U64,
            false,
        );
        assert_eq!(result, Err(ErrorCode::MarketWithdrawPaused));
        restore(&mut user);

        // Settlement is inside the admitted set, the same as on the cross-margin
        // path. A wound-down market stays exitable, so the status gate traps no
        // isolated collateral.
        {
            let mut spot_market = spot_market_map.get_ref_mut(&0).unwrap();
            spot_market.status = MarketStatus::Settlement;
            spot_market.paused_operations = 0;
        }
        withdraw_from_isolated_perp_position(
            user_key,
            &mut user,
            &mut user_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            0,
            0,
            QUOTE_PRECISION_U64,
            false,
        )
        .unwrap();

        assert_eq!(user.perp_positions[0].isolated_position_scaled_balance, 0);
        assert_eq!(
            spot_market_map.get_ref(&0).unwrap().deposit_balance,
            market_deposit_balance - SPOT_BALANCE_PRECISION
        );
    }
}
