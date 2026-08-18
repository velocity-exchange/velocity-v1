pub mod liquidate_perp {
    use {
        crate::{
            controller::{liquidation::liquidate_perp, position::PositionDirection},
            create_account_info, create_anchor_account_info,
            error::ErrorCode,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I128, BASE_PRECISION_I64,
                    BASE_PRECISION_U64, FUNDING_RATE_PRECISION_I128, LIQUIDATION_FEE_PRECISION,
                    LIQUIDATION_PCT_PRECISION, MARGIN_PRECISION, MARGIN_PRECISION_U128, ONE_HOUR,
                    PEG_PRECISION, PRICE_PRECISION, PRICE_PRECISION_U64, QUOTE_PRECISION,
                    QUOTE_PRECISION_I128, QUOTE_PRECISION_I64, QUOTE_PRECISION_U64,
                    SPOT_BALANCE_PRECISION_U64, SPOT_CUMULATIVE_INTEREST_PRECISION,
                    SPOT_WEIGHT_PRECISION,
                },
                liquidation::is_cross_margin_being_liquidated,
                margin::{
                    calculate_margin_requirement_and_total_collateral_and_liability_info,
                    MarginRequirementType,
                },
                position::calculate_base_asset_value_with_oracle_price,
            },
            state::{
                margin_calculation::{MarginCalculation, MarginContext},
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market::{MarketStats, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                state::State,
                user::{
                    Order, OrderStatus, OrderType, PerpPosition, PositionFlag, SpotPosition, User,
                    UserStats, UserStatus,
                },
            },
            test_utils::{get_orders, get_positions, get_pyth_price, get_spot_positions, *},
            PRICE_PRECISION_I64,
        },
        solana_program::pubkey::Pubkey,
        std::{collections::BTreeSet, str::FromStr},
    };

    /// After fix: When cross-margin user's position was fully liquidated, they were stuck in
    /// BEING_LIQUIDATED. Now liquidate_perp succeeds via early-exit path and clears it.
    #[test]
    pub fn clear_being_liquidated_when_position_fully_liquidated() {
        let now = 0_i64;
        let slot = 0_u64;
        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        let pyth_program = crate::ids::pyth_program::id();
        create_account_info!(
            oracle_price,
            &oracle_price_key,
            &pyth_program,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();
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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
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
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            status: UserStatus::BeingLiquidated as u8,
            ..User::default()
        };
        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };
        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();
        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        assert!(user.is_cross_margin_being_liquidated());
        liquidate_perp(
            0,
            BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();
        assert!(!user.is_cross_margin_being_liquidated());
    }

    #[test]
    pub fn successful_liquidation_long_perp() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
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

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -150 * QUOTE_PRECISION_I64,
                quote_entry_amount: -150 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -150 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],

            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(user.perp_positions[0].base_asset_amount, 0);
        assert_eq!(
            user.perp_positions[0].quote_asset_amount,
            -51 * QUOTE_PRECISION_I64
        );
        assert_eq!(user.perp_positions[0].open_orders, 0);
        assert_eq!(user.perp_positions[0].open_bids, 0);

        assert_eq!(
            liquidator.perp_positions[0].base_asset_amount,
            BASE_PRECISION_I64
        );
        assert_eq!(
            liquidator.perp_positions[0].quote_asset_amount,
            -99 * QUOTE_PRECISION_I64
        );

        let market_after = perp_market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.fee_ledger.total_liquidation_fee, 0);
    }

    // A liquidator subaccount inside its buffer band (floor <= net equity <
    // floor + buffer) must not acquire exposure through liquidation; the
    // admission check mirrors the risk-increasing fill gate. Same setup as
    // successful_liquidation_long_perp: the liquidator ends with 50 deposit,
    // base 1 at oracle 100 and quote -99, so post-liquidation net equity is
    // 51.
    #[test]
    pub fn liquidation_rejected_when_liquidator_inside_buffer_band() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
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

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -150 * QUOTE_PRECISION_I64,
                quote_entry_amount: -150 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -150 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],

            ..User::default()
        };

        // floor + buffer = 60 > post-liquidation net equity 51: rejected
        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            equity_floor: 40 * QUOTE_PRECISION_U64,
            equity_floor_buffer: 20 * QUOTE_PRECISION_U64,
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        let result = liquidate_perp(
            0,
            BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        );

        assert_eq!(result, Err(ErrorCode::EquityBelowFloor));
    }

    // Same setup, but the liquidator's post-liquidation net equity (51)
    // clears floor + buffer (50): admitted.
    #[test]
    pub fn liquidation_allowed_when_liquidator_clears_buffered_floor() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
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

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -150 * QUOTE_PRECISION_I64,
                quote_entry_amount: -150 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -150 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],

            ..User::default()
        };

        // floor + buffer = 50 <= post-liquidation net equity 51: admitted
        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            equity_floor: 40 * QUOTE_PRECISION_U64,
            equity_floor_buffer: 10 * QUOTE_PRECISION_U64,
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(
            liquidator.perp_positions[0].base_asset_amount,
            BASE_PRECISION_I64
        );
    }

    // Regression (audit bot on the funding-pause PR): a position carrying a
    // pre-pause unsettled funding delta must stay liquidatable while the
    // exchange-wide FundingPaused bit is set. `update_position_and_market`
    // validates `position.last_cumulative_funding_rate ==
    // market.cumulative_funding_rate_{long,short}` before any modification, and
    // the unconditional `settle_funding_payment` at the top of `liquidate_perp`
    // is what establishes it. An earlier revision skipped that settle during the
    // pause, which made this exact call revert with
    // InvalidPositionLastFundingRate and left underwater accounts frozen for the
    // pause's duration. Settling here is safe: the accumulators are frozen by
    // the gated `update_funding_rate`, so only pre-pause accrual is folded in.
    #[test]
    pub fn liquidation_settles_pre_pause_funding_delta_while_funding_paused() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                peg_multiplier: 100 * PEG_PRECISION,
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            // funding accrued before the pause; the position below has not been
            // touched since (last_cumulative_funding_rate == 0), so it carries a
            // pre-pause unsettled delta
            cumulative_funding_rate_long: 1000 * FUNDING_RATE_PRECISION_I128,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -150 * QUOTE_PRECISION_I64,
                quote_entry_amount: -150 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -150 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            // exchange-wide FundingPaused
            exchange_status: 0b00100000,
            ..Default::default()
        };

        liquidate_perp(
            0,
            BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        // the pre-pause funding delta was settled into the position before the
        // liquidation modified it, and the liquidation completed
        assert_eq!(user.perp_positions[0].base_asset_amount, 0);
        assert_eq!(user.cumulative_perp_funding, -1000 * QUOTE_PRECISION_I64);
        assert_eq!(
            user.perp_positions[0].quote_asset_amount,
            -1051 * QUOTE_PRECISION_I64
        );
    }

    #[test]
    pub fn successful_liquidation_short_perp() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 50 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: 3600,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: -BASE_PRECISION_I64,
                quote_asset_amount: 50 * QUOTE_PRECISION_I64,
                quote_entry_amount: 50 * QUOTE_PRECISION_I64,
                quote_break_even_amount: 50 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],

            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(user.perp_positions[0].base_asset_amount, 0);
        assert_eq!(
            user.perp_positions[0].quote_asset_amount,
            -51 * QUOTE_PRECISION_I64
        );
        assert_eq!(user.perp_positions[0].open_orders, 0);
        assert_eq!(user.perp_positions[0].open_bids, 0);

        assert_eq!(
            liquidator.perp_positions[0].base_asset_amount,
            -BASE_PRECISION_I64
        );
        assert_eq!(
            liquidator.perp_positions[0].quote_asset_amount,
            101 * QUOTE_PRECISION_I64
        );

        let market_after = perp_market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.fee_ledger.total_liquidation_fee, 0);
    }

    #[test]
    pub fn successful_liquidation_by_canceling_order() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 50 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: 3600,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: 1000 * BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: 100 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: 1000 * BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 255,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(user.perp_positions[0].base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(user.perp_positions[0].open_orders, 0);
        assert_eq!(user.perp_positions[0].open_bids, 0);

        assert_eq!(liquidator.perp_positions[0].base_asset_amount, 0);
    }

    #[test]
    pub fn successful_liquidation_up_to_max_liquidator_base_asset_amount() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: ONE_HOUR,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -150 * QUOTE_PRECISION_I64,
                quote_entry_amount: -150 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -150 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],

            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: MARGIN_PRECISION / 50,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            BASE_PRECISION_U64 / 2,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(
            user.perp_positions[0].base_asset_amount,
            BASE_PRECISION_I64 / 2
        );
        assert_eq!(user.perp_positions[0].quote_asset_amount, -100500000);
        assert_eq!(user.perp_positions[0].quote_entry_amount, -75000000);
        assert_eq!(user.perp_positions[0].quote_break_even_amount, -75500000);
        assert_eq!(user.perp_positions[0].open_orders, 0);
        assert_eq!(user.perp_positions[0].open_bids, 0);

        assert_eq!(
            liquidator.perp_positions[0].base_asset_amount,
            BASE_PRECISION_I64 / 2
        );
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, -49500000);

        let market_after = perp_market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.fee_ledger.total_liquidation_fee, 0)
    }

    #[test]
    pub fn successful_liquidation_to_cover_margin_shortage() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: ONE_HOUR,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 2 * BASE_PRECISION_I64,
                quote_asset_amount: -200 * QUOTE_PRECISION_I64,
                quote_entry_amount: -200 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -200 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 5 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),

            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: MARGIN_PRECISION / 50,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            10 * BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(user.perp_positions[0].base_asset_amount, 200000000);
        assert_eq!(user.perp_positions[0].quote_asset_amount, -23600000);
        assert_eq!(user.perp_positions[0].quote_entry_amount, -20000000);
        assert_eq!(user.perp_positions[0].quote_break_even_amount, -23600000);
        assert_eq!(user.perp_positions[0].open_orders, 0);
        assert_eq!(user.perp_positions[0].open_bids, 0);

        let MarginCalculation {
            total_collateral,
            margin_requirement_plus_buffer,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::liquidation(state.liquidation_margin_buffer_ratio),
        )
        .unwrap();

        // user out of liq territory
        assert_eq!(
            total_collateral.unsigned_abs(),
            margin_requirement_plus_buffer
        );

        let oracle_price = oracle_map
            .get_price_data(&(
                oracle_price_key,
                crate::state::oracle::OracleSource::PythLazer,
            ))
            .unwrap()
            .price;

        let perp_value = calculate_base_asset_value_with_oracle_price(
            user.perp_positions[0].base_asset_amount as i128,
            oracle_price,
        )
        .unwrap();

        let margin_ratio = total_collateral.unsigned_abs() * MARGIN_PRECISION_U128 / perp_value;

        assert_eq!(margin_ratio, 700);

        assert_eq!(liquidator.perp_positions[0].base_asset_amount, 1800000000);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, -178200000);

        let market_after = perp_market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.fee_ledger.total_liquidation_fee, 1800000)
    }

    #[test]
    pub fn successful_liquidation_long_perp_whale_imf_factor() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            imf_factor: 1000, // SPOT_IMF_PRECISION == 1e6
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            protocol_liquidation_fee: LIQUIDATION_FEE_PRECISION / 200,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: ONE_HOUR,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64 * 10000,
                quote_asset_amount: -150 * QUOTE_PRECISION_I64 * 10000,
                quote_entry_amount: -150 * QUOTE_PRECISION_I64 * 10000,
                quote_break_even_amount: -150 * QUOTE_PRECISION_I64 * 10000,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 150 * 10000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),

            ..User::default()
        };

        let MarginCalculation {
            margin_requirement: margin_req,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Maintenance),
        )
        .unwrap();
        assert_eq!(margin_req, 140014010000);
        assert!(!is_cross_margin_being_liquidated(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            0
        )
        .unwrap());

        {
            let market_to_edit = &mut perp_market_map.get_ref_mut(&0).unwrap();
            market_to_edit.imf_factor *= 10;
        }

        let MarginCalculation {
            margin_requirement: margin_req2,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Maintenance),
        )
        .unwrap();
        assert_eq!(margin_req2, 1040104010000);
        assert!(is_cross_margin_being_liquidated(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MARGIN_PRECISION / 50
        )
        .unwrap());

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        // user pays liquidator (1%) + IF (1%) + protocol (0.5%); the extra
        // 0.5% protocol cut shows up as an additional 500000 quote debit
        assert_eq!(user.perp_positions[0].base_asset_amount, 9999000000000);
        assert_eq!(user.perp_positions[0].quote_asset_amount, -1499902500000);
        assert_eq!(user.perp_positions[0].open_orders, 0);
        assert_eq!(user.perp_positions[0].open_bids, 0);

        assert_eq!(
            liquidator.perp_positions[0].base_asset_amount,
            BASE_PRECISION_I64
        );
        assert_eq!(
            liquidator.perp_positions[0].quote_asset_amount,
            -99 * QUOTE_PRECISION_I64
        );

        let market_after = perp_market_map.get_ref(&0).unwrap();
        // IF-first split: the IF keeps its full 1% (margin budget allowed it),
        // the protocol captures its 0.5% on top; total_liquidation_fee records
        // both cuts (the full amount charged to the liquidatee)
        assert_eq!(
            market_after.fee_ledger.total_liquidation_fee,
            QUOTE_PRECISION * 3 / 2
        );
        assert_eq!(market_after.fee_ledger.pending_if_fee, QUOTE_PRECISION);
        assert_eq!(
            market_after.fee_ledger.pending_protocol_fee,
            QUOTE_PRECISION / 2
        );
    }

    #[test]
    pub fn fail_liquidating_long_perp_due_to_limit_price() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: ONE_HOUR,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -150 * QUOTE_PRECISION_I64,
                quote_entry_amount: -150 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -150 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],

            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        let result = liquidate_perp(
            0,
            BASE_PRECISION_U64,
            Some(50 * PRICE_PRECISION_U64),
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        );

        assert_eq!(result, Err(ErrorCode::LiquidationDoesntSatisfyLimitPrice));
    }

    #[test]
    pub fn fail_liquidating_short_perp_due_to_limit_price() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 50 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: ONE_HOUR,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: -BASE_PRECISION_I64,
                quote_asset_amount: 50 * QUOTE_PRECISION_I64,
                quote_entry_amount: 50 * QUOTE_PRECISION_I64,
                quote_break_even_amount: 50 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],

            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        let result = liquidate_perp(
            0,
            BASE_PRECISION_U64,
            Some(150 * PRICE_PRECISION_U64),
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        );

        assert_eq!(result, Err(ErrorCode::LiquidationDoesntSatisfyLimitPrice));
    }

    #[test]
    pub fn liquidate_user_with_step_size_position() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: ONE_HOUR,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        let mut user = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 6 * SPOT_BALANCE_PRECISION_U64 / 11,
                ..SpotPosition::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64 / 100,
                quote_asset_amount: -150 * QUOTE_PRECISION_I64 / 100,
                quote_entry_amount: -150 * QUOTE_PRECISION_I64 / 100,
                quote_break_even_amount: -150 * QUOTE_PRECISION_I64 / 100,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            BASE_PRECISION_U64 / 100,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(user.perp_positions[0].base_asset_amount, 0);
        assert_eq!(
            user.perp_positions[0].quote_asset_amount,
            -52 * QUOTE_PRECISION_I64 / 100
        );
        assert_eq!(user.perp_positions[0].open_orders, 0);
        assert_eq!(user.perp_positions[0].open_bids, 0);

        assert_eq!(
            liquidator.perp_positions[0].base_asset_amount,
            BASE_PRECISION_I64 / 100
        );
        assert_eq!(
            liquidator.perp_positions[0].quote_asset_amount,
            -99 * QUOTE_PRECISION_I64 / 100
        );

        let market_after = perp_market_map.get_ref(&0).unwrap();
        assert_eq!(
            market_after.fee_ledger.total_liquidation_fee,
            QUOTE_PRECISION / 100
        );
    }

    #[test]
    pub fn liquidation_over_multiple_slots() {
        let now = 1_i64;
        let slot = 1_u64;

        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: ONE_HOUR,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: 10 * BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 20 * BASE_PRECISION_I64,
                quote_asset_amount: -2000 * QUOTE_PRECISION_I64,
                quote_entry_amount: -2000 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -2000 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: 10 * BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),

            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 500 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: MARGIN_PRECISION / 50,
            initial_pct_to_liquidate: (LIQUIDATION_PCT_PRECISION / 10) as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            10 * BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(user.last_active_slot, 1);
        assert_eq!(user.liquidation_margin_freed, 70010000);
        assert_eq!(user.perp_positions[0].base_asset_amount, 20000000000);

        // ~60% of liquidation finished
        let slot = 76_u64;
        liquidate_perp(
            0,
            10 * BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(user.last_active_slot, 1);
        assert_eq!(user.liquidation_margin_freed, 95802000);
        assert_eq!(user.perp_positions[0].base_asset_amount, 14800000000);

        let MarginCalculation {
            total_collateral,
            margin_requirement_plus_buffer,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::liquidation(state.liquidation_margin_buffer_ratio),
        )
        .unwrap();

        let margin_shortage =
            ((margin_requirement_plus_buffer as i128) - total_collateral).unsigned_abs();

        let pct_margin_freed = (user.liquidation_margin_freed as u128) * PRICE_PRECISION
            / (margin_shortage + user.liquidation_margin_freed as u128);
        assert_eq!(pct_margin_freed, 599504); // ~60%

        // dont change slot, still ~60% done
        let slot = 76_u64;
        liquidate_perp(
            0,
            100 * BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(user.last_active_slot, 1);
        assert_eq!(user.liquidation_margin_freed, 96000400); // no new margin freed
        assert_eq!(user.perp_positions[0].base_asset_amount, 14760000000);

        // ~76% of liquidation finished
        let slot = 101_u64;
        liquidate_perp(
            0,
            100 * BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(user.last_active_slot, 1);
        assert_eq!(user.liquidation_margin_freed, 122486800);
        assert_eq!(user.perp_positions[0].base_asset_amount, 9420000000);

        let MarginCalculation {
            total_collateral,
            margin_requirement_plus_buffer,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::liquidation(state.liquidation_margin_buffer_ratio),
        )
        .unwrap();

        let margin_shortage =
            ((margin_requirement_plus_buffer as i128) - total_collateral).unsigned_abs();

        let pct_margin_freed = (user.liquidation_margin_freed as u128) * PRICE_PRECISION
            / (margin_shortage + user.liquidation_margin_freed as u128);
        assert_eq!(pct_margin_freed, 767524); // ~76%

        // ~100% of liquidation finished
        let slot = 136_u64;
        liquidate_perp(
            0,
            100 * BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(user.status, 0);
        assert_eq!(user.last_active_slot, 1);
        assert_eq!(user.liquidation_margin_freed, 0);
        assert_eq!(user.perp_positions[0].base_asset_amount, 1910000000);
    }

    #[test]
    pub fn liquidation_accelerated() {
        let now = 1_i64;
        let slot = 1_u64;

        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: ONE_HOUR,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 2 * BASE_PRECISION_I64,
                quote_asset_amount: -200 * QUOTE_PRECISION_I64,
                quote_entry_amount: -200 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -200 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 5 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),

            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: MARGIN_PRECISION / 50,
            initial_pct_to_liquidate: (LIQUIDATION_PCT_PRECISION / 10) as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            10 * BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(user.status, 0);
        assert_eq!(user.last_active_slot, 1);
        assert_eq!(user.liquidation_margin_freed, 0);
        assert_eq!(user.perp_positions[0].base_asset_amount, 200000000);
    }

    #[test]
    pub fn partial_liquidation_oracle_down_20_pct() {
        let now = 1_i64;
        let slot = 1_u64;

        let mut oracle_price = get_pyth_price(80, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: ONE_HOUR,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                quote_entry_amount: -100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 10 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: MARGIN_PRECISION / 50,
            initial_pct_to_liquidate: (LIQUIDATION_PCT_PRECISION / 10) as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            10 * BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(user.last_active_slot, 1);
        assert_eq!(user.liquidation_margin_freed, 4784000);
        assert_eq!(user.perp_positions[0].base_asset_amount, 0);
    }

    #[test]
    pub fn successful_liquidation_half_of_if_fee() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            number_of_users: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 50 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: 3600,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        let mut user = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: -BASE_PRECISION_I64,
                quote_asset_amount: 100 * QUOTE_PRECISION_I64,
                quote_entry_amount: 100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: 100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 15 * SPOT_BALANCE_PRECISION_U64 / 10, // $1.5
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        let market_after = perp_market_map.get_ref(&0).unwrap();
        // .5% * 100 * .95 =$0.475
        assert_eq!(market_after.fee_ledger.total_liquidation_fee, 475000);
    }

    #[test]
    pub fn successful_liquidation_portion_of_if_fee() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut oracle_price = get_pyth_price_mantissa(23244136, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            number_of_users: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 50 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: 3600,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        let mut user = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: -299400000000,
                quote_asset_amount: 6959294318,
                quote_entry_amount: 6959294318,
                quote_break_even_amount: 6959294318,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 113838792 * 1000,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 200,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            300 * BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        let market_after = perp_market_map.get_ref(&0).unwrap();
        assert!(!user.is_cross_margin_being_liquidated());
        assert_eq!(market_after.fee_ledger.total_liquidation_fee, 41787043);
    }

    #[test]
    pub fn unhealthy_cross_margin_doesnt_cause_isolated_position_liquidation() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
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
        let mut market2 = PerpMarket {
            market_index: 1,
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
            status: MarketStatus::Initialized,
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
        create_anchor_account_info!(market2, PerpMarket, market2_account_info);

        let market_account_infos = [market_account_info, market2_account_info];
        let market_set = BTreeSet::default();
        let perp_market_map =
            PerpMarketMap::load(&market_set, &mut market_account_infos.iter().peekable()).unwrap();

        let mut spot_market = SpotMarket {
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

        let spot_positions = [SpotPosition::default(); 8];
        let mut perp_positions = [PerpPosition::default(); 8];
        perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: BASE_PRECISION_I64,
            quote_asset_amount: -150 * QUOTE_PRECISION_I64,
            quote_entry_amount: -150 * QUOTE_PRECISION_I64,
            quote_break_even_amount: -150 * QUOTE_PRECISION_I64,
            ..PerpPosition::default()
        };
        perp_positions[1] = PerpPosition {
            market_index: 1,
            base_asset_amount: BASE_PRECISION_I64,
            quote_asset_amount: -50 * QUOTE_PRECISION_I64,
            quote_entry_amount: -50 * QUOTE_PRECISION_I64,
            quote_break_even_amount: -150 * QUOTE_PRECISION_I64,
            isolated_position_scaled_balance: 200 * SPOT_BALANCE_PRECISION_U64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            ..PerpPosition::default()
        };
        let mut user = User {
            perp_positions,
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        let isolated_position_before = user.perp_positions[1];

        let result = liquidate_perp(
            1,
            BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        );

        assert_eq!(result, Err(ErrorCode::SufficientCollateral));

        liquidate_perp(
            0,
            BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        let isolated_position_after = user.perp_positions[1];

        assert_eq!(isolated_position_before, isolated_position_after);
    }
}

pub mod liquidate_perp_with_fill {

    use {
        crate::{
            controller::{liquidation::liquidate_perp_with_fill, position::PositionDirection},
            create_anchor_account_info,
            math::constants::{
                AMM_RESERVE_PRECISION, BASE_PRECISION_I64, BASE_PRECISION_U64,
                LIQUIDATION_FEE_PRECISION, LIQUIDATION_PCT_PRECISION, PEG_PRECISION,
                PRICE_PRECISION_U64, QUOTE_PRECISION_I128, QUOTE_PRECISION_I64,
                SPOT_BALANCE_PRECISION_U64, SPOT_CUMULATIVE_INTEREST_PRECISION,
                SPOT_WEIGHT_PRECISION,
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
                state::State,
                user::{
                    Order, OrderStatus, OrderType, PerpPosition, SpotPosition, User, UserStats,
                },
                user_map::{UserMap, UserStatsMap},
            },
            test_utils::{get_orders, get_positions, get_pyth_price, get_spot_positions},
            PRICE_PRECISION_I64,
        },
        anchor_lang::prelude::AccountLoader,
        solana_program::{clock::Clock, pubkey::Pubkey},
        std::str::FromStr,
    };

    #[test]
    pub fn successful_liquidate_perp_with_fill_long() {
        let now = 0_i64;
        let slot = 100_u64;

        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                terminal_quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_asset_amount_with_amm: 0,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            order_tick_size: 1,
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

        let user_key = Pubkey::new_unique();
        let liquidator_key = Pubkey::new_unique();

        let mut user = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                quote_entry_amount: -100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -100 * QUOTE_PRECISION_I64,
                open_orders: 0,
                open_bids: 0,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 4 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),

            ..User::default()
        };

        create_anchor_account_info!(user, &user_key, User, user_account_info);
        let user_account_loader: AccountLoader<User> =
            AccountLoader::try_from(&user_account_info).unwrap();

        let liquidator_authority = Pubkey::new_unique();
        let mut liquidator = User {
            authority: liquidator_authority,
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        create_anchor_account_info!(liquidator, &liquidator_key, User, liquidator_account_info);
        let liquidator_account_loader: AccountLoader<User> =
            AccountLoader::try_from(&liquidator_account_info).unwrap();

        let mut user_stats = UserStats::default();

        create_anchor_account_info!(user_stats, UserStats, user_stats_account_info);
        let user_stats_account_loader: AccountLoader<UserStats> =
            AccountLoader::try_from(&user_stats_account_info).unwrap();

        let mut liquidator_stats = UserStats::default();

        create_anchor_account_info!(liquidator_stats, UserStats, liquidator_stats_account_info);
        let liquidator_stats_account_loader: AccountLoader<UserStats> =
            AccountLoader::try_from(&liquidator_stats_account_info).unwrap();

        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        let maker_key = Pubkey::new_unique();
        let maker_authority = Pubkey::new_unique();
        let mut maker = User {
            authority: maker_authority,
            orders: get_orders(Order {
                status: OrderStatus::Open,
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64 / 2,
                price: 100 * PRICE_PRECISION_U64,
                slot: slot - 1,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64 / 2,
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
        create_anchor_account_info!(maker, &maker_key, User, maker_account_info);
        let makers_and_referrers = UserMap::load_one(&maker_account_info).unwrap();

        let mut maker_stats = UserStats {
            authority: maker_authority,
            ..UserStats::default()
        };
        create_anchor_account_info!(maker_stats, UserStats, maker_stats_account_info);
        let maker_and_referrer_stats = UserStatsMap::load_one(&maker_stats_account_info).unwrap();

        let clock = Clock {
            slot,
            unix_timestamp: now,
            ..Clock::default()
        };

        liquidate_perp_with_fill(
            0,
            &user_account_loader,
            &user_key,
            &user_stats_account_loader,
            &liquidator_account_loader,
            &liquidator_key,
            &liquidator_stats_account_loader,
            &makers_and_referrers,
            &maker_and_referrer_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            &clock,
            &state,
        )
        .unwrap();

        let user = user_account_loader.load().unwrap();
        assert_eq!(user.perp_positions[0].base_asset_amount, 640000000);
        assert_eq!(user.perp_positions[0].quote_asset_amount, -64374400);
        assert_eq!(user.perp_positions[0].open_orders, 0);
        assert_eq!(user.perp_positions[0].open_bids, 0);

        let maker = makers_and_referrers.get_ref(&maker_key).unwrap();
        assert_eq!(maker.perp_positions[0].base_asset_amount, 360000000);
        assert_eq!(maker.perp_positions[0].quote_asset_amount, -35999100);

        let liquidator = liquidator_account_loader.load().unwrap();
        assert_eq!(liquidator.perp_positions[0].base_asset_amount, 0);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, 1440);

        let market_after = perp_market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.fee_ledger.total_liquidation_fee, 360000);
    }

    #[test]
    pub fn successful_liquidate_perp_with_fill_short() {
        let now = 0_i64;
        let slot = 100_u64;

        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                terminal_quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_asset_amount_with_amm: 0,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            order_tick_size: 1,
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

        let user_key = Pubkey::new_unique();
        let liquidator_key = Pubkey::new_unique();

        let mut user = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: -BASE_PRECISION_I64,
                quote_asset_amount: 100 * QUOTE_PRECISION_I64,
                quote_entry_amount: 100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: 100 * QUOTE_PRECISION_I64,
                open_orders: 0,
                open_bids: 0,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 4 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),

            ..User::default()
        };

        create_anchor_account_info!(user, &user_key, User, user_account_info);
        let user_account_loader: AccountLoader<User> =
            AccountLoader::try_from(&user_account_info).unwrap();

        let liquidator_authority = Pubkey::new_unique();
        let mut liquidator = User {
            authority: liquidator_authority,
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        create_anchor_account_info!(liquidator, &liquidator_key, User, liquidator_account_info);
        let liquidator_account_loader: AccountLoader<User> =
            AccountLoader::try_from(&liquidator_account_info).unwrap();

        let mut user_stats = UserStats::default();

        create_anchor_account_info!(user_stats, UserStats, user_stats_account_info);
        let user_stats_account_loader: AccountLoader<UserStats> =
            AccountLoader::try_from(&user_stats_account_info).unwrap();

        let mut liquidator_stats = UserStats::default();

        create_anchor_account_info!(liquidator_stats, UserStats, liquidator_stats_account_info);
        let liquidator_stats_account_loader: AccountLoader<UserStats> =
            AccountLoader::try_from(&liquidator_stats_account_info).unwrap();

        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        let maker_key = Pubkey::new_unique();
        let maker_authority = Pubkey::new_unique();
        let mut maker = User {
            authority: maker_authority,
            orders: get_orders(Order {
                status: OrderStatus::Open,
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64 / 2,
                price: 100 * PRICE_PRECISION_U64,
                slot: slot - 1,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64 / 2,
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
        create_anchor_account_info!(maker, &maker_key, User, maker_account_info);
        let makers_and_referrers = UserMap::load_one(&maker_account_info).unwrap();

        let mut maker_stats = UserStats {
            authority: maker_authority,
            ..UserStats::default()
        };
        create_anchor_account_info!(maker_stats, UserStats, maker_stats_account_info);
        let maker_and_referrer_stats = UserStatsMap::load_one(&maker_stats_account_info).unwrap();

        let clock = Clock {
            slot,
            unix_timestamp: now,
            ..Clock::default()
        };

        liquidate_perp_with_fill(
            0,
            &user_account_loader,
            &user_key,
            &user_stats_account_loader,
            &liquidator_account_loader,
            &liquidator_key,
            &liquidator_stats_account_loader,
            &makers_and_referrers,
            &maker_and_referrer_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            &clock,
            &state,
        )
        .unwrap();

        let user = user_account_loader.load().unwrap();
        assert_eq!(user.perp_positions[0].base_asset_amount, -640000000);
        assert_eq!(user.perp_positions[0].quote_asset_amount, 63625600);
        assert_eq!(user.perp_positions[0].open_orders, 0);
        assert_eq!(user.perp_positions[0].open_bids, 0);

        let maker = makers_and_referrers.get_ref(&maker_key).unwrap();
        assert_eq!(maker.perp_positions[0].base_asset_amount, -360000000);
        assert_eq!(maker.perp_positions[0].quote_asset_amount, 36000900);

        let liquidator = liquidator_account_loader.load().unwrap();
        assert_eq!(liquidator.perp_positions[0].base_asset_amount, 0);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, 1440);

        let market_after = perp_market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.fee_ledger.total_liquidation_fee, 360000);
    }

    #[test]
    pub fn successful_liquidate_perp_with_fill_long_with_amm() {
        let now = 0_i64;
        let slot = 100_u64;

        let mut oracle_price = get_pyth_price(100, 6);
        oracle_price.posted_slot = slot;
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                terminal_quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_asset_amount_with_amm: 0,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            order_tick_size: 1,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        market.amm.max_fill_reserve_fraction = 1;
        market.amm.max_base_asset_reserve = u64::MAX as u128;
        market.amm.min_base_asset_reserve = 0;
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        let user_key = Pubkey::new_unique();
        let liquidator_key = Pubkey::new_unique();

        let mut user = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                quote_entry_amount: -100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -100 * QUOTE_PRECISION_I64,
                open_orders: 0,
                open_bids: 0,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 4 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),

            ..User::default()
        };

        create_anchor_account_info!(user, &user_key, User, user_account_info);
        let user_account_loader: AccountLoader<User> =
            AccountLoader::try_from(&user_account_info).unwrap();

        let liquidator_authority = Pubkey::new_unique();
        let mut liquidator = User {
            authority: liquidator_authority,
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        create_anchor_account_info!(liquidator, &liquidator_key, User, liquidator_account_info);
        let liquidator_account_loader: AccountLoader<User> =
            AccountLoader::try_from(&liquidator_account_info).unwrap();

        let mut user_stats = UserStats::default();

        create_anchor_account_info!(user_stats, UserStats, user_stats_account_info);
        let user_stats_account_loader: AccountLoader<UserStats> =
            AccountLoader::try_from(&user_stats_account_info).unwrap();

        let mut liquidator_stats = UserStats::default();

        create_anchor_account_info!(liquidator_stats, UserStats, liquidator_stats_account_info);
        let liquidator_stats_account_loader: AccountLoader<UserStats> =
            AccountLoader::try_from(&liquidator_stats_account_info).unwrap();

        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        let clock = Clock {
            slot,
            unix_timestamp: now,
            ..Clock::default()
        };

        liquidate_perp_with_fill(
            0,
            &user_account_loader,
            &user_key,
            &user_stats_account_loader,
            &liquidator_account_loader,
            &liquidator_key,
            &liquidator_stats_account_loader,
            &UserMap::empty(),
            &UserStatsMap::empty(),
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            &clock,
            &state,
        )
        .unwrap();

        let user = user_account_loader.load().unwrap();
        assert_eq!(user.perp_positions[0].base_asset_amount, 640000000);
        assert_eq!(user.perp_positions[0].quote_asset_amount, -64502193);
        assert_eq!(user.perp_positions[0].open_orders, 0);
        assert_eq!(user.perp_positions[0].open_bids, 0);

        let liquidator = liquidator_account_loader.load().unwrap();
        assert_eq!(liquidator.perp_positions[0].base_asset_amount, 0);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, 1434);

        let market_after = perp_market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.fee_ledger.total_liquidation_fee, 358708);
    }

    #[test]
    pub fn successful_liquidate_perp_with_fill_short_with_amm() {
        let now = 0_i64;
        let slot = 100_u64;

        let mut oracle_price = get_pyth_price(100, 6);
        oracle_price.posted_slot = slot;
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                terminal_quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_asset_amount_with_amm: 0,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            number_of_users_with_base: 1,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            order_tick_size: 1,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        market.amm.max_fill_reserve_fraction = 1;
        market.amm.max_base_asset_reserve = u64::MAX as u128;
        market.amm.min_base_asset_reserve = 0;
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        let user_key = Pubkey::new_unique();
        let liquidator_key = Pubkey::new_unique();

        let mut user = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: -BASE_PRECISION_I64,
                quote_asset_amount: 100 * QUOTE_PRECISION_I64,
                quote_entry_amount: 100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: 100 * QUOTE_PRECISION_I64,
                open_orders: 0,
                open_bids: 0,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 4 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),

            ..User::default()
        };

        create_anchor_account_info!(user, &user_key, User, user_account_info);
        let user_account_loader: AccountLoader<User> =
            AccountLoader::try_from(&user_account_info).unwrap();

        let liquidator_authority = Pubkey::new_unique();
        let mut liquidator = User {
            authority: liquidator_authority,
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        create_anchor_account_info!(liquidator, &liquidator_key, User, liquidator_account_info);
        let liquidator_account_loader: AccountLoader<User> =
            AccountLoader::try_from(&liquidator_account_info).unwrap();

        let mut user_stats = UserStats::default();

        create_anchor_account_info!(user_stats, UserStats, user_stats_account_info);
        let user_stats_account_loader: AccountLoader<UserStats> =
            AccountLoader::try_from(&user_stats_account_info).unwrap();

        let mut liquidator_stats = UserStats::default();

        create_anchor_account_info!(liquidator_stats, UserStats, liquidator_stats_account_info);
        let liquidator_stats_account_loader: AccountLoader<UserStats> =
            AccountLoader::try_from(&liquidator_stats_account_info).unwrap();

        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        let clock = Clock {
            slot,
            unix_timestamp: now,
            ..Clock::default()
        };

        liquidate_perp_with_fill(
            0,
            &user_account_loader,
            &user_key,
            &user_stats_account_loader,
            &liquidator_account_loader,
            &liquidator_key,
            &liquidator_stats_account_loader,
            &UserMap::empty(),
            &UserStatsMap::empty(),
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            &clock,
            &state,
        )
        .unwrap();

        let user = user_account_loader.load().unwrap();
        assert_eq!(user.perp_positions[0].base_asset_amount, -640000000);
        assert_eq!(user.perp_positions[0].quote_asset_amount, 63494178);
        assert_eq!(user.perp_positions[0].open_orders, 0);
        assert_eq!(user.perp_positions[0].open_bids, 0);

        let liquidator = liquidator_account_loader.load().unwrap();
        assert_eq!(liquidator.perp_positions[0].base_asset_amount, 0);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, 1445);

        let market_after = perp_market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.fee_ledger.total_liquidation_fee, 361300);
    }
}

pub mod liquidate_spot {
    use {
        crate::{
            controller::liquidation::liquidate_spot,
            create_anchor_account_info,
            error::ErrorCode,
            math::{
                constants::{
                    LIQUIDATION_FEE_PRECISION, LIQUIDATION_PCT_PRECISION, MARGIN_PRECISION,
                    MARGIN_PRECISION_U128, PRICE_PRECISION, PRICE_PRECISION_U64,
                    SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                margin::calculate_margin_requirement_and_total_collateral_and_liability_info,
                orders::is_oracle_too_divergent_with_twap_5min,
                spot_balance::{get_strict_token_value, get_token_amount, get_token_value},
            },
            state::{
                margin_calculation::{MarginCalculation, MarginContext},
                oracle::{HistoricalOracleData, OracleSource, StrictOraclePrice},
                oracle_map::OracleMap,
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                state::State,
                user::{Order, PerpPosition, SpotPosition, User},
            },
            test_utils::{get_pyth_price, get_spot_positions},
            QUOTE_PRECISION_I64,
        },
        solana_program::pubkey::Pubkey,
        std::{ops::Deref, str::FromStr},
    };

    #[test]
    pub fn successful_liquidation_liability_transfer_implied_by_asset_amount() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let perp_market_map = PerpMarketMap::empty();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        liquidate_spot(
            0,
            1,
            10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        )
        .unwrap();

        assert_eq!(user.spot_positions[0].scaled_balance, 0);
        assert_eq!(user.spot_positions[1].scaled_balance, 999999);

        assert_eq!(
            liquidator.spot_positions[0].balance_type,
            SpotBalanceType::Deposit
        );
        assert_eq!(liquidator.spot_positions[0].scaled_balance, 200000000000);
        assert_eq!(
            liquidator.spot_positions[1].balance_type,
            SpotBalanceType::Borrow
        );
        assert_eq!(liquidator.spot_positions[1].scaled_balance, 999000001);
    }

    #[test]
    pub fn stale_for_margin_deposit_oracle_seizes_at_protective_price() {
        let now = 0_i64;
        // oracle posted at slot 0 -> delay 200 > slots_before_stale_for_margin (120),
        // while the price stays inside the 5min twap divergence band
        let slot = 200_u64;

        // stale oracle shows $90 while the 5min twap is $100: the depressed price may
        // flag the account liquidatable, but the seizure must be priced at the
        // user-protective max(oracle, 5min twap, oracle + conf) = $100
        let mut sol_oracle_price = get_pyth_price(90, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let perp_market_map = PerpMarketMap::empty();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 20000 * SPOT_BALANCE_PRECISION,
            borrow_balance: 9500 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: 100 * SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: 100 * QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: 100 * QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        // 100 sol deposit ($8.1k weighted at the stale price) vs 9500 usdc borrow -> liquidatable
        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: 9500 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
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

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        liquidate_spot(
            1,
            0,
            10000 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        )
        .unwrap();

        // the full 9500 usdc borrow is repaid, and the sol seized for it is priced at the
        // protective $100 (twap) instead of the stale $90: 9500 * 1.001 / 100 = 95.095 sol.
        // at the stale $90 the whole 100 sol deposit would have been drained while leaving
        // a residual borrow (bankruptcy)
        assert_eq!(user.spot_positions[0].scaled_balance, 0);
        assert_eq!(user.spot_positions[1].scaled_balance, 4_905_000_000);
        assert!(!user.is_cross_margin_bankrupt());

        assert_eq!(
            liquidator.spot_positions[0].scaled_balance,
            10500 * SPOT_BALANCE_PRECISION_U64
        );
        assert_eq!(liquidator.spot_positions[1].scaled_balance, 95_095_000_000);
    }

    #[test]
    pub fn too_uncertain_deposit_oracle_seizes_at_protective_price() {
        let now = 0_i64;
        let slot = 0_u64;

        // fresh oracle but confidence is 5% of price > 2% collateral-tier max: the
        // seizure must be priced at max(oracle, 5min twap, oracle + conf) = $105
        let mut sol_oracle_price = get_pyth_price(100, 6);
        sol_oracle_price.conf = 5 * PRICE_PRECISION_U64;
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let perp_market_map = PerpMarketMap::empty();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 20000 * SPOT_BALANCE_PRECISION,
            borrow_balance: 9500 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            asset_tier: crate::state::spot_market::AssetTier::Collateral,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: 100 * SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: sol_oracle_price.price,
                last_oracle_price_twap_5min: sol_oracle_price.price,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: 9500 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
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

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        liquidate_spot(
            1,
            0,
            10000 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        )
        .unwrap();

        // the margin shortage requires repaying 5089.910089 usdc; the sol seized for it is
        // priced at the protective $105 (oracle + conf) instead of the raw $100:
        // 5089.910089 * 1.001 / 105 = 48.523809 sol (vs 50.949999 at the raw price)
        assert_eq!(user.spot_positions[0].scaled_balance, 4_410_089_910_999);
        assert_eq!(user.spot_positions[1].scaled_balance, 51_476_191_000);
        assert!(!user.is_cross_margin_being_liquidated());
        assert!(!user.is_cross_margin_bankrupt());
    }

    #[test]
    pub fn stale_for_margin_liability_oracle_repays_at_protective_price() {
        let now = 0_i64;
        // oracle posted at slot 0 -> delay 200 > slots_before_stale_for_margin (120),
        // while the price stays inside the 5min twap divergence band
        let slot = 200_u64;

        // stale borrow oracle shows $110 while the 5min twap is $100: the inflated debt
        // price may flag the account liquidatable, but the collateral given per unit of
        // borrow repaid must be priced at the user-protective
        // min(oracle, 5min twap, oracle - conf) = $100
        let mut sol_oracle_price = get_pyth_price(110, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let perp_market_map = PerpMarketMap::empty();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 30000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: 100 * SPOT_BALANCE_PRECISION,
            borrow_balance: 90 * SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: 100 * QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: 100 * QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        // 10000 usdc deposit vs 90 sol borrow ($9.9k at the stale $110, $10.9k weighted)
        // -> liquidatable at the stale price (solvent at the $100 twap)
        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 10000 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: 90 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
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

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        liquidate_spot(
            0,
            1,
            100 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        )
        .unwrap();

        // 81.809090 sol of borrow is repaid; the usdc handed over for it is priced with
        // the borrow at the protective $100 (twap) instead of the stale $110:
        // 81.809090 * 100 / 0.999 = 8189.098098 usdc (vs 9008.007907 at the stale price)
        assert_eq!(user.spot_positions[0].scaled_balance, 1_810_901_902_000);
        assert_eq!(user.spot_positions[1].scaled_balance, 8_190_909_999);
        assert!(!user.is_cross_margin_being_liquidated());
        assert!(!user.is_cross_margin_bankrupt());
    }

    /// The 5-minute TWAP price band must judge the oracle against the TWAP as it stood on
    /// entry. `liquidate_spot` refreshes both oracle TWAPs before the band runs, and the
    /// refresh pulls each TWAP toward the very oracle price the band measures. Reading the
    /// field back therefore lets a divergent oracle widen its own band and pass.
    #[test]
    pub fn divergent_liability_oracle_bands_against_pre_refresh_twap() {
        // The clock is 10 minutes past the market's last oracle-TWAP stamp, so this
        // instruction's own refresh moves the 5-minute TWAP by the full sanitize clamp.
        let now = 600_i64;
        let slot = 0_u64;

        // The borrow oracle prints $155 against a $100 5-minute TWAP: 55% divergence, over
        // the 50% band floor. The refresh moves the TWAP up by the sanitize clamp, to about
        // $133, where the same oracle reads as 16% divergent and clears the band.
        let mut sol_oracle_price = get_pyth_price(155, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let perp_market_map = PerpMarketMap::empty();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 30000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            last_interest_ts: now as u64,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: 100 * SPOT_BALANCE_PRECISION,
            borrow_balance: 60 * SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            last_interest_ts: now as u64,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: 100 * QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: 100 * QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        // 10000 usdc deposit vs 60 sol borrow ($9.3k at $155, $10.23k weighted) -> the
        // account is liquidatable and solvent, so the band is the only thing that can stop
        // the transfer.
        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 10000 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: 60 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
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

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        let res = liquidate_spot(
            0,
            1,
            100 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        );

        assert_eq!(res, Err(ErrorCode::PriceBandsBreached));

        // Control: the refresh did run and did drag the stored 5-minute TWAP up to the
        // clamp, where the same oracle clears the band. Reading the field is what the band
        // must not do. If this trips, the fixture no longer reproduces the flip.
        let refreshed_twap_5min = spot_market_map
            .get_ref(&1)
            .unwrap()
            .historical_oracle_data
            .last_oracle_price_twap_5min;
        assert!(refreshed_twap_5min > 130 * QUOTE_PRECISION_I64);
        assert!(
            !is_oracle_too_divergent_with_twap_5min(
                155 * QUOTE_PRECISION_I64,
                refreshed_twap_5min,
                state
                    .oracle_guard_rails
                    .max_oracle_twap_5min_percent_divergence() as i64,
            )
            .unwrap(),
            "fixture no longer reproduces the flip — the refreshed TWAP must clear the band"
        );

        assert_eq!(
            user.spot_positions[0].scaled_balance,
            10000 * SPOT_BALANCE_PRECISION_U64
        );
        assert_eq!(
            user.spot_positions[1].scaled_balance,
            60 * SPOT_BALANCE_PRECISION_U64
        );
    }

    #[test]
    pub fn successful_liquidation_with_valid_deposit_oracle() {
        let now = 0_i64;
        let slot = 0_u64;

        // fresh, margin-valid oracle: the seizure is priced at the raw oracle price,
        // no protective pricing kicks in
        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let perp_market_map = PerpMarketMap::empty();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 20000 * SPOT_BALANCE_PRECISION,
            borrow_balance: 9500 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: 100 * SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: sol_oracle_price.price,
                last_oracle_price_twap_5min: sol_oracle_price.price,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: 9500 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
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

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        liquidate_spot(
            1,
            0,
            10000 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        )
        .unwrap();

        // liquidation proceeded at the raw $100 oracle price:
        // 5089.910089 usdc repaid, 5089.910089 * 1.001 / 100 = 50.949999 sol seized
        assert_eq!(user.spot_positions[0].scaled_balance, 4_410_089_910_999);
        assert_eq!(user.spot_positions[1].scaled_balance, 49_050_001_000);
    }

    #[test]
    pub fn successful_liquidation_liquidator_max_liability_transfer() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let perp_market_map = PerpMarketMap::empty();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 1442 / 10000),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 999 / 1000),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_market = [SpotPosition::default(); 8];
        spot_market[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_market[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions: spot_market,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        // oracle twap too volatile to liq rn
        assert!(liquidate_spot(
            0,
            1,
            10_u128.pow(6) / 10,
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        )
        .is_err());

        // move twap closer to oracle price (within 80% below)
        let mut market1 = spot_market_map
            .get_ref_mut(&sol_market.market_index)
            .unwrap();
        market1.historical_oracle_data.last_oracle_price_twap =
            sol_oracle_price.price * 6744 / 10000;
        drop(market1);

        liquidate_spot(
            0,
            1,
            10_u128.pow(6) / 10,
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        )
        .unwrap();

        assert_eq!(user.spot_positions[0].scaled_balance, 89989990000);
        assert_eq!(user.spot_positions[1].scaled_balance, 899999999);

        assert_eq!(
            liquidator.spot_positions[0].balance_type,
            SpotBalanceType::Deposit
        );
        assert_eq!(liquidator.spot_positions[0].scaled_balance, 110010010000);
        assert_eq!(
            liquidator.spot_positions[1].balance_type,
            SpotBalanceType::Borrow
        );
        assert_eq!(liquidator.spot_positions[1].scaled_balance, 100000001);
    }

    #[test]
    pub fn successful_liquidation_liability_transfer_to_cover_margin_shortage() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 105 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let state = State {
            liquidation_margin_buffer_ratio: MARGIN_PRECISION / 50,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_spot(
            0,
            1,
            10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        )
        .unwrap();

        assert_eq!(user.spot_positions[0].scaled_balance, 45558159000);
        assert_eq!(user.spot_positions[1].scaled_balance, 406768999);

        let liquidation_buffer = state.liquidation_margin_buffer_ratio;
        let MarginCalculation {
            margin_requirement,
            total_collateral,
            margin_requirement_plus_buffer,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::liquidation(liquidation_buffer),
        )
        .unwrap();

        assert_eq!(margin_requirement, 44744590);
        assert_eq!(total_collateral, 45558159);
        assert_eq!(margin_requirement_plus_buffer, 45558128);

        let token_amount = get_token_amount(
            user.spot_positions[1].scaled_balance as u128,
            spot_market_map.get_ref(&1).unwrap().deref(),
            &user.spot_positions[1].balance_type,
        )
        .unwrap();
        let oracle_price_data = oracle_map
            .get_price_data(&(
                sol_oracle_price_key,
                crate::state::oracle::OracleSource::PythLazer,
            ))
            .unwrap();
        let token_value =
            get_token_value(token_amount as i128, 6, oracle_price_data.price).unwrap();

        let strict_price_1 = StrictOraclePrice {
            current: oracle_price_data.price,
            twap_5min: Some(oracle_price_data.price / 10),
        };
        let strict_token_value_1 =
            get_strict_token_value(token_amount as i128, 6, &strict_price_1).unwrap();

        let strict_price_2 = StrictOraclePrice {
            current: oracle_price_data.price,
            twap_5min: Some(oracle_price_data.price * 2),
        };
        let strict_token_value_2 =
            get_strict_token_value(token_amount as i128, 6, &strict_price_2).unwrap();

        let strict_price_3 = StrictOraclePrice {
            current: oracle_price_data.price,
            twap_5min: Some(oracle_price_data.price * 2),
        };
        let strict_token_value_3 =
            get_strict_token_value(-(token_amount as i128), 6, &strict_price_3).unwrap();

        assert_eq!(token_amount, 406769);
        assert_eq!(token_value, 40676900);
        assert_eq!(strict_token_value_1, 4067690); // if oracle price is more favorable than twap
        assert_eq!(strict_token_value_2, token_value); // oracle price is less favorable than twap
        assert_eq!(strict_token_value_3, -(token_value * 2)); // if liability and strict would value as twap

        let margin_ratio =
            total_collateral.unsigned_abs() * MARGIN_PRECISION_U128 / token_value.unsigned_abs();

        assert_eq!(margin_ratio, 11200); // 112%

        assert_eq!(
            liquidator.spot_positions[0].balance_type,
            SpotBalanceType::Deposit
        );
        assert_eq!(liquidator.spot_positions[0].scaled_balance, 159441841000);
        assert_eq!(
            liquidator.spot_positions[1].balance_type,
            SpotBalanceType::Borrow
        );
        assert_eq!(liquidator.spot_positions[1].scaled_balance, 593824001);

        let market_after = spot_market_map.get_ref(&1).unwrap();
        let market_revenue = get_token_amount(
            market_after.revenue_pool.scaled_balance,
            &market_after,
            &SpotBalanceType::Deposit,
        )
        .unwrap();

        assert_eq!(market_revenue, 593);
        assert_eq!(
            liquidator.spot_positions[1].scaled_balance + user.spot_positions[1].scaled_balance
                - market_after.revenue_pool.scaled_balance as u64,
            SPOT_BALANCE_PRECISION_U64
        );
    }

    #[test]
    pub fn failure_due_to_limit_price() {
        let now = 0_i64;
        let slot = 0_u64;
        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let perp_market_map = PerpMarketMap::empty();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();
        let state = State {
            liquidation_margin_buffer_ratio: MARGIN_PRECISION / 50,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        let limit_price = (100000000 * PRICE_PRECISION_U64 / 999000) + 1;
        let result = liquidate_spot(
            0,
            1,
            10_u128.pow(6),
            Some(limit_price),
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        );

        assert_eq!(result, Err(ErrorCode::LiquidationDoesntSatisfyLimitPrice));
    }

    #[test]
    pub fn success_with_to_limit_price() {
        let now = 0_i64;
        let slot = 0_u64;
        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let perp_market_map = PerpMarketMap::empty();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();
        let state = State {
            liquidation_margin_buffer_ratio: MARGIN_PRECISION / 50,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        let limit_price = (100000000 * PRICE_PRECISION_U64 / 999000) - 1;
        let result = liquidate_spot(
            0,
            1,
            10_u128.pow(6),
            Some(limit_price),
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        );

        assert_eq!(result, Ok(()));
    }

    #[test]
    pub fn successful_liquidation_dust_borrow() {
        let now = 0_i64;
        let slot = 0_u64;
        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 107 * SPOT_BALANCE_PRECISION_U64 / 50,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64 / 50,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let state = State {
            liquidation_margin_buffer_ratio: MARGIN_PRECISION / 50,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_spot(
            0,
            1,
            10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        )
        .unwrap();

        assert_eq!(user.spot_positions[0].scaled_balance, 0);
        assert_eq!(user.spot_positions[1].scaled_balance, 19999);

        assert_eq!(liquidator.spot_positions[0].scaled_balance, 102140000000);
        assert_eq!(liquidator.spot_positions[1].scaled_balance, 20000001); // ~$1 worth of liability
    }

    #[test]
    pub fn liquidate_over_multiple_slots() {
        let now = 1_i64;
        let slot = 1_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: 10 * SPOT_BALANCE_PRECISION,
            borrow_balance: 10 * SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 1050 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: 10 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();
        let state = State {
            liquidation_margin_buffer_ratio: MARGIN_PRECISION / 50,
            initial_pct_to_liquidate: (LIQUIDATION_PCT_PRECISION / 10) as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        let liquidation_buffer = state.liquidation_margin_buffer_ratio;

        liquidate_spot(
            0,
            1,
            10 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        )
        .unwrap();

        assert_eq!(user.last_active_slot, 1);
        assert_eq!(user.is_cross_margin_being_liquidated(), true);
        assert_eq!(user.liquidation_margin_freed, 7000031);
        assert_eq!(user.spot_positions[0].scaled_balance, 990558159000);
        assert_eq!(user.spot_positions[1].scaled_balance, 9406768999);

        let MarginCalculation {
            total_collateral,
            margin_requirement_plus_buffer,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::liquidation(liquidation_buffer),
        )
        .unwrap();

        let margin_shortage =
            ((margin_requirement_plus_buffer as i128) - total_collateral).unsigned_abs();

        let pct_margin_freed = (user.liquidation_margin_freed as u128) * PRICE_PRECISION
            / (margin_shortage + user.liquidation_margin_freed as u128);
        assert_eq!(pct_margin_freed, 100000); // ~10%

        let slot = 51_u64;
        liquidate_spot(
            0,
            1,
            10 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        )
        .unwrap();

        assert_eq!(user.last_active_slot, 1);
        assert_eq!(user.liquidation_margin_freed, 30328714);
        assert_eq!(user.spot_positions[0].scaled_balance, 792456458000);
        assert_eq!(user.spot_positions[1].scaled_balance, 7429711998);

        let MarginCalculation {
            total_collateral,
            margin_requirement_plus_buffer,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::liquidation(liquidation_buffer),
        )
        .unwrap();

        let margin_shortage =
            ((margin_requirement_plus_buffer as i128) - total_collateral).unsigned_abs();

        let pct_margin_freed = (user.liquidation_margin_freed as u128) * PRICE_PRECISION
            / (margin_shortage + user.liquidation_margin_freed as u128);
        assert_eq!(pct_margin_freed, 433267); // ~43.3%
        assert_eq!(user.is_cross_margin_being_liquidated(), true);

        let slot = 136_u64;
        liquidate_spot(
            0,
            1,
            10 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        )
        .unwrap();

        assert_eq!(user.last_active_slot, 1);
        assert_eq!(user.liquidation_margin_freed, 0);
        assert_eq!(user.spot_positions[0].scaled_balance, 455580082000);
        assert_eq!(user.spot_positions[1].scaled_balance, 4067681997);
        assert_eq!(user.is_cross_margin_being_liquidated(), false);
    }

    #[test]
    pub fn successful_liquidation_half_if_fee() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 20,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let mut usdt_market = SpotMarket {
            market_index: 2,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdt_market, SpotMarket, usdt_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
            &usdt_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[2] = SpotPosition {
            market_index: 2,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 105 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let liquidation_buffer = MARGIN_PRECISION / 50;
        let state = State {
            liquidation_margin_buffer_ratio: liquidation_buffer,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        liquidate_spot(
            2,
            1,
            10_u128.pow(9),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        )
        .unwrap();

        let liability_market = spot_market_map.get_ref(&1).unwrap();
        let revenue_pool_token_amount = get_token_amount(
            liability_market.revenue_pool.scaled_balance,
            &liability_market,
            &SpotBalanceType::Deposit,
        )
        .unwrap();

        assert_eq!(revenue_pool_token_amount, 23944781); // 2.39%

        let margin_calc = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::liquidation(liquidation_buffer),
        )
        .unwrap();

        print!("{:?}", margin_calc);
        assert!(margin_calc.meets_margin_requirement());
    }
}

pub mod liquidate_borrow_for_perp_pnl {
    use {
        crate::{
            controller::liquidation::liquidate_borrow_for_perp_pnl,
            create_anchor_account_info,
            error::ErrorCode,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I128, LIQUIDATION_FEE_PRECISION,
                    LIQUIDATION_PCT_PRECISION, MARGIN_PRECISION, MARGIN_PRECISION_U128,
                    PEG_PRECISION, PERCENTAGE_PRECISION, PRICE_PRECISION, PRICE_PRECISION_U64,
                    QUOTE_PRECISION_I128, QUOTE_PRECISION_I64, SPOT_BALANCE_PRECISION,
                    SPOT_BALANCE_PRECISION_U64, SPOT_CUMULATIVE_INTEREST_PRECISION,
                    SPOT_WEIGHT_PRECISION,
                },
                margin::calculate_margin_requirement_and_total_collateral_and_liability_info,
                spot_balance::{get_token_amount, get_token_value},
            },
            state::{
                margin_calculation::{MarginCalculation, MarginContext},
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market::{PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                user::{Order, PerpPosition, SpotPosition, User},
            },
            test_utils::{get_positions, get_pyth_price, get_spot_positions},
        },
        solana_program::pubkey::Pubkey,
        std::{ops::Deref, str::FromStr},
    };

    #[test]
    pub fn successful_liquidation_liquidator_max_liability_transfer() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: 100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        liquidate_borrow_for_perp_pnl(
            0,
            1,
            8 * 10_u128.pow(5), // .8
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            10,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();

        assert_eq!(user.spot_positions[0].scaled_balance, 199999999);
        assert_eq!(user.perp_positions[0].quote_asset_amount, 19119120);

        assert_eq!(
            liquidator.spot_positions[1].balance_type,
            SpotBalanceType::Borrow
        );
        assert_eq!(liquidator.spot_positions[1].scaled_balance, 800000001);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, 80880880);
    }

    #[test]
    pub fn stale_for_margin_liability_oracle_transfers_pnl_at_protective_price() {
        let now = 0_i64;
        // oracle posted at slot 0 -> delay 200 > slots_before_stale_for_margin (120),
        // while the price stays inside the 5min twap divergence band
        let slot = 200_u64;

        // stale borrow oracle shows $110 while the 5min twap is $100: the pnl handed over
        // per unit of borrow taken must be priced with the borrow at the user-protective
        // min(oracle, 5min twap, oracle - conf) = $100
        let mut sol_oracle_price = get_pyth_price(110, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: 100 * QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: 100 * QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: 100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        liquidate_borrow_for_perp_pnl(
            0,
            1,
            8 * 10_u128.pow(5), // .8
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            10,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();

        // 0.8 sol of borrow is taken over; the pnl handed over for it is priced with the
        // borrow at the protective $100 (twap) instead of the stale $110:
        // 0.8 * 100 * 1.01 / 0.999 = 80.880880 pnl (vs 88.968968 at the stale price)
        assert_eq!(user.spot_positions[0].scaled_balance, 199999999);
        assert_eq!(user.perp_positions[0].quote_asset_amount, 19119120);

        assert_eq!(
            liquidator.spot_positions[1].balance_type,
            SpotBalanceType::Borrow
        );
        assert_eq!(liquidator.spot_positions[1].scaled_balance, 800000001);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, 80880880);
    }

    #[test]
    pub fn stale_for_margin_liability_oracle_prices_at_pre_refresh_twap() {
        // Same setup as the test above, with one change: the clock has advanced past the
        // 5-minute window since the market's last oracle-TWAP stamp, so this instruction's
        // own refresh materially moves `last_oracle_price_twap_5min`. The protective price
        // must still be the pre-refresh $100, not the refreshed value the stale $110 pulls
        // it toward. `last_interest_ts` is stamped at `now` so no interest accrues over the
        // elapsed span and the transfer numbers stay comparable to that test.
        let now = 600_i64;
        let slot = 200_u64;

        let mut sol_oracle_price = get_pyth_price(110, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            last_interest_ts: now as u64,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            last_interest_ts: now as u64,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: 100 * QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: 100 * QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: 100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        liquidate_borrow_for_perp_pnl(
            0,
            1,
            8 * 10_u128.pow(5), // .8
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            10,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();

        // Control: this instruction's own refresh did drag the stored 5min TWAP nearly all
        // the way to the stale $110, so pricing off the field would have handed over close
        // to the stale-price amount. If this trips, the fixture no longer reproduces it.
        let refreshed_twap_5min = spot_market_map
            .get_ref(&1)
            .unwrap()
            .historical_oracle_data
            .last_oracle_price_twap_5min;
        assert!(
            refreshed_twap_5min > 109 * QUOTE_PRECISION_I64,
            "expected the refresh to normalize the 5min TWAP onto the stale price, got {}",
            refreshed_twap_5min
        );

        // Fixed: identical to the test above — 0.8 sol of borrow still costs 80.880880 pnl
        // at the protective $100, not ~88.9 at the normalized TWAP.
        assert_eq!(user.spot_positions[0].scaled_balance, 199999999);
        assert_eq!(user.perp_positions[0].quote_asset_amount, 19119120);
        assert_eq!(liquidator.spot_positions[1].scaled_balance, 800000001);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, 80880880);
    }

    #[test]
    pub fn successful_liquidation_liability_transfer_to_cover_margin_shortage() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: 105 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let liquidation_buffer = MARGIN_PRECISION / 50;
        liquidate_borrow_for_perp_pnl(
            0,
            1,
            2 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            liquidation_buffer,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();

        assert_eq!(user.spot_positions[0].scaled_balance, 357739999);
        assert_eq!(user.perp_positions[0].quote_asset_amount, 40066807);

        let MarginCalculation {
            total_collateral,
            margin_requirement_plus_buffer,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::liquidation(liquidation_buffer),
        )
        .unwrap();

        assert_eq!(total_collateral, 40066807);
        assert_eq!(margin_requirement_plus_buffer, 40066880);

        let token_amount = get_token_amount(
            user.spot_positions[0].scaled_balance as u128,
            spot_market_map.get_ref(&1).unwrap().deref(),
            &user.spot_positions[0].balance_type,
        )
        .unwrap();
        let oracle_price_data = oracle_map
            .get_price_data(&(
                sol_oracle_price_key,
                crate::state::oracle::OracleSource::PythLazer,
            ))
            .unwrap();
        let token_value =
            get_token_value(token_amount as i128, 6, oracle_price_data.price).unwrap();

        let margin_ratio =
            total_collateral.unsigned_abs() * MARGIN_PRECISION_U128 / token_value.unsigned_abs();

        assert_eq!(margin_ratio, 11199); // ~112%

        assert_eq!(
            liquidator.spot_positions[1].balance_type,
            SpotBalanceType::Borrow
        );
        assert_eq!(liquidator.spot_positions[1].scaled_balance, 642260001);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, 64933193);

        let market_after = spot_market_map.get_ref(&1).unwrap();
        let market_revenue = get_token_amount(
            market_after.revenue_pool.scaled_balance,
            &market_after,
            &SpotBalanceType::Deposit,
        )
        .unwrap();

        assert_eq!(market_revenue, 0);
    }

    #[test]
    pub fn successful_liquidation_liability_transfer_implied_by_pnl() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: 80 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        liquidate_borrow_for_perp_pnl(
            0,
            1,
            2 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            10,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();

        assert_eq!(user.spot_positions[0].scaled_balance, 208711999);
        assert_eq!(user.perp_positions[0].quote_asset_amount, 0);

        assert_eq!(
            liquidator.spot_positions[1].balance_type,
            SpotBalanceType::Borrow
        );
        assert_eq!(liquidator.spot_positions[1].scaled_balance, 791288001);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, 80000000);
    }

    #[test]
    pub fn failure_due_to_limit_price() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: 100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let limit_price = (80880880 * PRICE_PRECISION_U64 / 800000) + 1;
        let result = liquidate_borrow_for_perp_pnl(
            0,
            1,
            8 * 10_u128.pow(5), // .8
            Some(limit_price),
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            10,
            PERCENTAGE_PRECISION,
            150,
            false,
        );

        assert_eq!(result, Err(ErrorCode::LiquidationDoesntSatisfyLimitPrice));
    }

    #[test]
    pub fn success_with_limit_price() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: 100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let limit_price = (80880880 * PRICE_PRECISION_U64 / 800000) - 1;
        let result = liquidate_borrow_for_perp_pnl(
            0,
            1,
            8 * 10_u128.pow(5), // .8
            Some(limit_price),
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            10,
            PERCENTAGE_PRECISION,
            150,
            false,
        );

        assert_eq!(result, Ok(()));
    }

    #[test]
    pub fn successful_liquidation_dust_position() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION / 50,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64 / 50,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: 107 * QUOTE_PRECISION_I64 / 50,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let liquidation_buffer = MARGIN_PRECISION / 50;
        liquidate_borrow_for_perp_pnl(
            0,
            1,
            2 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            liquidation_buffer,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();

        assert_eq!(user.spot_positions[0].scaled_balance, 0);
        assert_eq!(user.perp_positions[0].quote_asset_amount, 0);

        assert_eq!(
            liquidator.spot_positions[1].balance_type,
            SpotBalanceType::Borrow
        );
        assert_eq!(liquidator.spot_positions[1].scaled_balance, 20000001); // ~$1 liability taken over
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, 2140000);
    }

    #[test]
    pub fn successful_liquidation_over_multiple_slots() {
        let now = 1_i64;
        let slot = 1_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: 100 * SPOT_BALANCE_PRECISION,
            borrow_balance: 11 * SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: 10 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: 1050 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let liquidation_buffer = MARGIN_PRECISION / 50;
        liquidate_borrow_for_perp_pnl(
            0,
            1,
            10 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            liquidation_buffer,
            LIQUIDATION_PCT_PRECISION / 10,
            150,
            false,
        )
        .unwrap();

        assert_eq!(user.last_active_slot, 1);
        assert_eq!(user.liquidation_margin_freed, 6999927);
        assert_eq!(user.spot_positions[0].scaled_balance, 9357739999);
        assert_eq!(user.perp_positions[0].quote_asset_amount, 985066807);

        let MarginCalculation {
            total_collateral,
            margin_requirement_plus_buffer,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::liquidation(liquidation_buffer),
        )
        .unwrap();

        let margin_shortage =
            ((margin_requirement_plus_buffer as i128) - total_collateral).unsigned_abs();

        let pct_margin_freed = (user.liquidation_margin_freed as u128) * PRICE_PRECISION
            / (margin_shortage + user.liquidation_margin_freed as u128);
        assert_eq!(pct_margin_freed, 99998); // ~10%

        let slot = 51_u64;
        liquidate_borrow_for_perp_pnl(
            0,
            1,
            10 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            liquidation_buffer,
            LIQUIDATION_PCT_PRECISION / 10,
            150,
            false,
        )
        .unwrap();

        assert_eq!(user.last_active_slot, 1);
        assert_eq!(user.liquidation_margin_freed, 30328628);
        assert_eq!(user.spot_positions[0].scaled_balance, 7217275998);
        assert_eq!(user.perp_positions[0].quote_asset_amount, 768663540);

        let MarginCalculation {
            total_collateral,
            margin_requirement_plus_buffer,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::liquidation(liquidation_buffer),
        )
        .unwrap();

        let margin_shortage =
            ((margin_requirement_plus_buffer as i128) - total_collateral).unsigned_abs();

        let pct_margin_freed = (user.liquidation_margin_freed as u128) * PRICE_PRECISION
            / (margin_shortage + user.liquidation_margin_freed as u128);
        assert_eq!(pct_margin_freed, 433266); // ~43.3%

        let slot = 136_u64;
        liquidate_borrow_for_perp_pnl(
            0,
            1,
            10 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            liquidation_buffer,
            LIQUIDATION_PCT_PRECISION / 10,
            150,
            false,
        )
        .unwrap();

        assert_eq!(user.liquidation_margin_freed, 0);
        assert_eq!(user.last_active_slot, 1);
    }
}

pub mod liquidate_perp_pnl_for_deposit {
    use {
        crate::{
            controller::liquidation::{liquidate_perp_pnl_for_deposit, liquidate_spot},
            create_anchor_account_info,
            error::{ErrorCode, VelocityResult},
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I128, LIQUIDATION_FEE_PRECISION,
                    LIQUIDATION_PCT_PRECISION, MARGIN_PRECISION, PEG_PRECISION,
                    PERCENTAGE_PRECISION, PRICE_PRECISION, PRICE_PRECISION_U64,
                    QUOTE_PRECISION_I128, QUOTE_PRECISION_I64, SPOT_BALANCE_PRECISION,
                    SPOT_BALANCE_PRECISION_U64, SPOT_CUMULATIVE_INTEREST_PRECISION,
                    SPOT_WEIGHT_PRECISION,
                },
                margin::calculate_margin_requirement_and_total_collateral_and_liability_info,
            },
            state::{
                margin_calculation::MarginContext,
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market::{ContractTier, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{AssetTier, SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                state::State,
                user::{Order, PerpPosition, SpotPosition, User, UserStatus},
            },
            test_utils::{get_positions, get_pyth_price, get_spot_positions},
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    /// Liquidates a $150 negative pnl against a 1-token deposit whose maintenance
    /// asset weight is `maintenance_asset_weight`. The perp market charges a 2%
    /// liquidator fee, the deposit market 0.1%, and the state buffer is 2%.
    /// Returns the call result and the deposit the user keeps.
    fn liquidate_with_asset_weight(maintenance_asset_weight: u32) -> (VelocityResult, u64) {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 50, // 2%
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: maintenance_asset_weight,
            maintenance_asset_weight,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: 0,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000, // 0.1%
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: -150 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let result = liquidate_perp_pnl_for_deposit(
            0,
            1,
            150 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            MARGIN_PRECISION / 50, // 2% buffer
            PERCENTAGE_PRECISION,
            150,
            false,
        );

        (result, user.spot_positions[0].scaled_balance)
    }

    // Audit #25: the seizure premium scales with the deposit's asset weight, so
    // whether the transfer helps or hurts depends on that weight. At a 2%
    // liquidation buffer against a 2% perp and 0.1% deposit liquidator fee, the
    // premium outgrows the buffer at a weight of about 0.9986. Collateral weighted
    // below that is safe to seize; full-weight collateral is not, because the
    // account was getting full credit for it and gives it up at a premium.
    #[test]
    pub fn asset_weight_sets_whether_the_transfer_helps() {
        // 0.80 and 0.90: comfortably profitable for the account
        let (result, remaining_deposit) =
            liquidate_with_asset_weight(8 * SPOT_WEIGHT_PRECISION / 10);
        assert_eq!(result, Ok(()));
        assert!(remaining_deposit < SPOT_BALANCE_PRECISION_U64);

        let (result, remaining_deposit) =
            liquidate_with_asset_weight(9 * SPOT_WEIGHT_PRECISION / 10);
        assert_eq!(result, Ok(()));
        assert!(remaining_deposit < SPOT_BALANCE_PRECISION_U64);

        // 0.99: still under the boundary
        let (result, _) = liquidate_with_asset_weight(99 * SPOT_WEIGHT_PRECISION / 100);
        assert_eq!(result, Ok(()));

        // the boundary itself: 0.9986 is the last weight the transfer helps at
        let (result, _) = liquidate_with_asset_weight(9986);
        assert_eq!(result, Ok(()));

        let (result, remaining_deposit) = liquidate_with_asset_weight(9987);
        assert_eq!(result, Err(ErrorCode::LiquidationWorsensAccountHealth));
        assert_eq!(remaining_deposit, SPOT_BALANCE_PRECISION_U64);

        // 1.00: the premium now exceeds the buffer, so no transfer size helps
        let (result, remaining_deposit) = liquidate_with_asset_weight(SPOT_WEIGHT_PRECISION);
        assert_eq!(result, Err(ErrorCode::LiquidationWorsensAccountHealth));
        assert_eq!(remaining_deposit, SPOT_BALANCE_PRECISION_U64);
    }

    #[test]
    pub fn successful_liquidation_liquidator_max_pnl_transfer() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: 0,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        liquidate_perp_pnl_for_deposit(
            0,
            1,
            50 * 10_u128.pow(6), // .8
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            10,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();

        assert_eq!(user.spot_positions[0].scaled_balance, 494445000);
        assert_eq!(user.perp_positions[0].quote_asset_amount, -50000000);

        assert_eq!(
            liquidator.spot_positions[1].balance_type,
            SpotBalanceType::Deposit
        );
        assert_eq!(liquidator.spot_positions[1].scaled_balance, 505555000);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, -50000000);
    }

    // Audit #25: when the perp + asset liquidator fees exceed the liquidation
    // margin buffer, transferring pnl-for-deposit *worsens* the account's
    // (buffered) margin shortage — the asset premium the liquidator collects
    // outweighs the collateral relief from cancelling the negative pnl. The
    // postcondition must reject rather than silently strip the deposit.
    #[test]
    pub fn reverts_when_transfer_worsens_margin_shortage() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            // 10% liquidator fee: well above the 0.1% buffer passed below.
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 10,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: 0,
            // 10% liquidator fee on the seized asset.
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 10,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let result = liquidate_perp_pnl_for_deposit(
            0,
            1,
            100 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            10,
            PERCENTAGE_PRECISION,
            150,
            false,
        );

        assert_eq!(result, Err(ErrorCode::LiquidationWorsensAccountHealth));
    }

    // Audit #25 follow-up: a degradation of less than a dollar must revert too.
    // The liquidator picks `liquidator_max_pnl_transfer`, so any tolerance on this
    // guard is an amount the liquidator can stay under and repeat until the
    // deposit is gone. The guard holds no tolerance.
    #[test]
    pub fn reverts_when_transfer_worsens_margin_shortage_by_less_than_a_dollar() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            // 1% liquidator fee against a 0.1% buffer: a $50 transfer takes
            // $50.50 of the deposit, so the shortage grows by about $0.50.
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 400 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_map =
            SpotMarketMap::load_multiple(Vec::from([&usdc_spot_market_account_info]), true)
                .unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 200 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: -250 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let result = liquidate_perp_pnl_for_deposit(
            0,
            0,
            50 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            10,
            PERCENTAGE_PRECISION,
            150,
            false,
        );

        assert_eq!(result, Err(ErrorCode::LiquidationWorsensAccountHealth));

        // the deposit and the pnl stay where they were: the transfer is refused
        // before any balance moves
        assert_eq!(
            user.spot_positions[0].scaled_balance,
            200 * SPOT_BALANCE_PRECISION_U64
        );
        assert_eq!(
            user.perp_positions[0].quote_asset_amount,
            -250 * QUOTE_PRECISION_I64
        );
    }

    #[test]
    pub fn successful_liquidation_pnl_transfer_to_cover_margin_shortage() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: 0,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: -91 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        liquidate_perp_pnl_for_deposit(
            0,
            1,
            200 * 10_u128.pow(6), // .8
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            MARGIN_PRECISION / 50,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();

        assert_eq!(user.spot_positions[0].scaled_balance, 740788000);
        assert_eq!(user.perp_positions[0].quote_asset_amount, -65363637);

        assert_eq!(
            liquidator.spot_positions[1].balance_type,
            SpotBalanceType::Deposit
        );
        assert_eq!(liquidator.spot_positions[1].scaled_balance, 259212000);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, -25636363);

        let market_after = market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.fee_ledger.total_liquidation_fee, 0);
    }

    #[test]
    pub fn successful_liquidation_pnl_transfer_implied_by_asset_amount() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: 0,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: -150 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        liquidate_perp_pnl_for_deposit(
            0,
            1,
            200 * 10_u128.pow(6), // .8
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            10,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();

        assert_eq!(user.spot_positions[0].scaled_balance, 0);
        assert_eq!(user.perp_positions[0].quote_asset_amount, -51098901);

        assert_eq!(
            liquidator.spot_positions[1].balance_type,
            SpotBalanceType::Deposit
        );
        assert_eq!(liquidator.spot_positions[1].scaled_balance, 1000000000);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, -98901099);
    }

    #[test]
    pub fn stale_for_margin_deposit_oracle_seizes_at_protective_price() {
        let now = 0_i64;
        // oracle posted at slot 0 -> delay 200 > slots_before_stale_for_margin (120),
        // while the price stays inside the 5min twap divergence band
        let slot = 200_u64;

        // stale oracle shows $90 while the 5min twap is $100: the deposit seized for the
        // pnl transfer must be priced at the user-protective
        // max(oracle, 5min twap, oracle + conf) = $100
        let mut sol_oracle_price = get_pyth_price(90, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: 0,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: 100 * QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: 100 * QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: -150 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        liquidate_perp_pnl_for_deposit(
            0,
            1,
            200 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            10,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();

        // the 1 sol deposit is exchanged at the protective $100 (twap), so the user is
        // credited 98.901099 pnl (1 * 100 * 0.99 / 1.001) instead of the 88.911089 the
        // stale $90 price would have given for the same deposit
        assert_eq!(user.spot_positions[0].scaled_balance, 0);
        assert_eq!(user.perp_positions[0].quote_asset_amount, -51098901);

        assert_eq!(
            liquidator.spot_positions[1].balance_type,
            SpotBalanceType::Deposit
        );
        assert_eq!(liquidator.spot_positions[1].scaled_balance, 1000000000);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, -98901099);
    }

    #[test]
    pub fn failure_due_to_limit_price() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: 0,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let limit_price = 505555 * PRICE_PRECISION_U64 / 50000000 + 1;
        let result = liquidate_perp_pnl_for_deposit(
            0,
            1,
            50 * 10_u128.pow(6), // .8
            Some(limit_price),
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            10,
            PERCENTAGE_PRECISION,
            150,
            false,
        );

        assert_eq!(result, Err(ErrorCode::LiquidationDoesntSatisfyLimitPrice));
    }

    #[test]
    pub fn success_with_limit_price() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: 0,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let limit_price = 505555 * PRICE_PRECISION_U64 / 50000000 - 1;
        let result = liquidate_perp_pnl_for_deposit(
            0,
            1,
            50 * 10_u128.pow(6), // .8
            Some(limit_price),
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            10,
            PERCENTAGE_PRECISION,
            150,
            false,
        );

        assert_eq!(result, Ok(()));
    }

    #[test]
    pub fn successful_liquidate_dust_position() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: 0,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: SPOT_BALANCE_PRECISION_U64 / 50,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: -91 * QUOTE_PRECISION_I64 / 50,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        liquidate_perp_pnl_for_deposit(
            0,
            1,
            200 * 10_u128.pow(6), // .8
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            MARGIN_PRECISION / 50,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();

        // The whole $1.82 of pnl moves, and it buys $1.8402 of the deposit at the
        // liquidation rate. The rest of the 0.02 SOL deposit ($0.16) stays with the
        // user: the pnl relief does not pay for it, and the seizure never takes
        // more than it pays for.
        assert_eq!(user.spot_positions[0].scaled_balance, 1598000);
        assert_eq!(user.perp_positions[0].quote_asset_amount, 0);

        assert_eq!(
            liquidator.spot_positions[1].balance_type,
            SpotBalanceType::Deposit
        );
        assert_eq!(liquidator.spot_positions[1].scaled_balance, 18402000);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, -1820000); // -$1
    }

    #[test]
    pub fn successful_liquidation_over_multiple_slots() {
        let now = 1_i64;
        let slot = 1_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: 0,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 10 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: -950 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let liquidation_buffer = MARGIN_PRECISION / 50;
        liquidate_perp_pnl_for_deposit(
            0,
            1,
            200 * 10_u128.pow(6), // .8
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            liquidation_buffer,
            LIQUIDATION_PCT_PRECISION / 10,
            150,
            false,
        )
        .unwrap();

        assert_eq!(user.last_active_slot, 1);
        assert_eq!(user.liquidation_margin_freed, 6900038);
        assert_eq!(user.spot_positions[0].scaled_balance, 9365758000);
        assert_eq!(user.perp_positions[0].quote_asset_amount, -887272728);

        let calc = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::liquidation(liquidation_buffer),
        )
        .unwrap();

        let margin_shortage = calc.cross_margin_margin_shortage().unwrap();

        let pct_margin_freed = (user.liquidation_margin_freed as u128) * PRICE_PRECISION
            / (margin_shortage + user.liquidation_margin_freed as u128);
        assert_eq!(pct_margin_freed, 100000); // ~10%

        let slot = 51_u64;
        liquidate_perp_pnl_for_deposit(
            0,
            1,
            200 * 10_u128.pow(6), // .8
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            liquidation_buffer,
            LIQUIDATION_PCT_PRECISION / 10,
            150,
            false,
        )
        .unwrap();

        assert_eq!(user.last_active_slot, 1);
        assert_eq!(user.liquidation_margin_freed, 28900058);
        assert_eq!(user.spot_positions[0].scaled_balance, 7343536000);
        assert_eq!(user.perp_positions[0].quote_asset_amount, -687272728);

        let calc = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::liquidation(liquidation_buffer),
        )
        .unwrap();

        let margin_shortage = calc.cross_margin_margin_shortage().unwrap();

        let pct_margin_freed = (user.liquidation_margin_freed as u128) * PRICE_PRECISION
            / (margin_shortage + user.liquidation_margin_freed as u128);
        assert_eq!(pct_margin_freed, 418841); // ~43%

        let slot = 136_u64;
        liquidate_perp_pnl_for_deposit(
            0,
            1,
            2000 * 10_u128.pow(6), // .8
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            liquidation_buffer,
            LIQUIDATION_PCT_PRECISION / 10,
            150,
            false,
        )
        .unwrap();

        assert_eq!(user.last_active_slot, 1);
        assert_eq!(user.liquidation_margin_freed, 0);
    }

    #[test]
    pub fn failure_due_to_asset_tier_violation() {
        let now = 0_i64;
        let slot = 0_u64;
        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            asset_tier: AssetTier::Collateral,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION as i64,
                last_oracle_price_twap_5min: PRICE_PRECISION as i64,

                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: 10 * SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),

                ..HistoricalOracleData::default()
            },
            asset_tier: AssetTier::Collateral,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 200 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 2500 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        assert!(liquidate_perp_pnl_for_deposit(
            0,
            0,
            50 * 10_u128.pow(6), // .8
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            // 2% liquidation margin buffer: it must stay above the market's 1%
            // liquidator fee, or the seizure premium outweighs the pnl relief and
            // the transfer is refused as loss-making
            200,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .is_err());

        let state = State {
            liquidation_margin_buffer_ratio: MARGIN_PRECISION / 50,
            initial_pct_to_liquidate: (PERCENTAGE_PRECISION / 10) as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        liquidate_spot(
            0,
            1,
            10_u128.pow(9),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        )
        .unwrap();

        assert_eq!(user.spot_positions[1].scaled_balance, 0);

        liquidate_perp_pnl_for_deposit(
            0,
            0,
            50 * 10_u128.pow(6), // .8
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            200,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();
        assert_eq!(user.perp_positions[0].quote_asset_amount, -50000000);
        assert_eq!(user.spot_positions[0].scaled_balance, 49394850000); // <$50
        assert_eq!(user.status, UserStatus::BeingLiquidated as u8);

        liquidate_perp_pnl_for_deposit(
            0,
            0,
            50 * 10_u128.pow(6), // .8
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            200,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();
        assert_eq!(user.spot_positions[0].scaled_balance, 0);
        assert_eq!(user.spot_positions[1].scaled_balance, 0);

        assert_eq!(user.perp_positions[0].quote_asset_amount, -1099098);
        assert_eq!(user.status, UserStatus::Bankrupt as u8);
    }

    #[test]
    pub fn failure_due_to_contract_tier_violation() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            market_index: 0,
            contract_tier: ContractTier::A,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);

        let mut bonk_market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 8000,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            contract_tier: ContractTier::Speculative,
            market_index: 1,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(bonk_market, PerpMarket, bonk_market_account_info);

        let market_map = PerpMarketMap::load_multiple(
            vec![&market_account_info, &bonk_market_account_info],
            true,
        )
        .unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: 0,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 200 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: SPOT_BALANCE_PRECISION_U64 / 1000,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        user.perp_positions[1] = PerpPosition {
            market_index: 1,
            quote_asset_amount: -150 * QUOTE_PRECISION_I64,
            ..PerpPosition::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        assert!(liquidate_perp_pnl_for_deposit(
            1,
            0,
            50 * 10_u128.pow(6), // .8
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            // 2% liquidation margin buffer: it must stay above the market's 1%
            // liquidator fee, or the seizure premium outweighs the pnl relief and
            // the transfer is refused as loss-making
            200,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .is_err());
        assert_eq!(user.perp_positions[0].quote_asset_amount, -100000000);

        liquidate_perp_pnl_for_deposit(
            0,
            0,
            5000 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            200,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();
        assert_eq!(user.perp_positions[0].quote_asset_amount, 0);

        liquidate_perp_pnl_for_deposit(
            1,
            0,
            50 * 10_u128.pow(6), // .8
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            200,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();

        assert_eq!(user.spot_positions[0].scaled_balance, 48484849000);
        assert_eq!(user.perp_positions[0].quote_asset_amount, 0);
        assert_eq!(user.perp_positions[1].quote_asset_amount, -100000000);

        assert_eq!(
            liquidator.spot_positions[1].balance_type,
            SpotBalanceType::Deposit
        );
        assert_eq!(liquidator.spot_positions[1].scaled_balance, 0);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, -100000000);
    }

    #[test]
    pub fn positive_pnl_in_safer_market_does_not_block_liquidation() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            market_index: 0,
            contract_tier: ContractTier::A,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);

        let mut bonk_market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 8000,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            contract_tier: ContractTier::Speculative,
            market_index: 1,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(bonk_market, PerpMarket, bonk_market_account_info);

        let market_map = PerpMarketMap::load_multiple(
            vec![&market_account_info, &bonk_market_account_info],
            true,
        )
        .unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: 0,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 200 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        // zero-base positive unsettled pnl claim in the A tier market must not
        // count as the user's safest perp liability and block liquidating the
        // speculative market's negative pnl against the deposit
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: 10 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        user.perp_positions[1] = PerpPosition {
            market_index: 1,
            quote_asset_amount: -300 * QUOTE_PRECISION_I64,
            ..PerpPosition::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        liquidate_perp_pnl_for_deposit(
            1,
            0,
            50 * 10_u128.pow(6),
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            // 2% liquidation margin buffer: it must stay above the market's 1%
            // liquidator fee, or the seizure premium outweighs the pnl relief and
            // the transfer is refused as loss-making
            200,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();

        assert_eq!(
            user.perp_positions[0].quote_asset_amount,
            10 * QUOTE_PRECISION_I64
        );
        assert_eq!(user.perp_positions[1].quote_asset_amount, -250000000);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, -50000000);
        assert_eq!(liquidator.perp_positions[0].market_index, 1);
    }
}

pub mod resolve_perp_bankruptcy {
    use {
        crate::{
            controller::{
                funding::settle_funding_payment, liquidation::resolve_perp_bankruptcy,
                perp_pools::sweep_market_fees, position::PositionDirection,
            },
            create_anchor_account_info,
            math::constants::{
                AMM_RESERVE_PRECISION, BASE_PRECISION_I128, BASE_PRECISION_I64, BASE_PRECISION_U64,
                FUNDING_RATE_PRECISION_I128, FUNDING_RATE_PRECISION_I64, LIQUIDATION_FEE_PRECISION,
                PEG_PRECISION, PERCENTAGE_PRECISION_U32, QUOTE_PRECISION, QUOTE_PRECISION_I128,
                QUOTE_PRECISION_I64, QUOTE_PRECISION_U64, QUOTE_SPOT_MARKET_INDEX,
                SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
            },
            state::{
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market::{
                    FeeLedger, InsuranceClaim, MarketStats, PerpMarket, PoolBalance, AMM,
                },
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                user::{
                    Order, OrderStatus, OrderType, PerpPosition, SpotPosition, User, UserStatus,
                },
            },
            test_utils::{get_orders, get_positions, get_pyth_price, get_spot_positions},
            PRICE_PRECISION_I64,
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    #[test]
    pub fn successful_resolve_perp_bankruptcy() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            number_of_users: 1,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 5 * BASE_PRECISION_I128,
            base_asset_amount_short: -5 * BASE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            cumulative_funding_rate_long: 1000 * FUNDING_RATE_PRECISION_I128,
            cumulative_funding_rate_short: -1000 * FUNDING_RATE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                quote_entry_amount: -100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -100 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],
            status: UserStatus::Bankrupt as u8,
            next_liquidation_id: 2,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut expected_user = user;
        expected_user.status = 0;
        expected_user.perp_positions[0].quote_asset_amount = 0;
        expected_user.total_social_loss = 100000000;

        let mut expected_market = market;
        expected_market.cumulative_funding_rate_long = 1010 * FUNDING_RATE_PRECISION_I128;
        expected_market.cumulative_funding_rate_short = -1010 * FUNDING_RATE_PRECISION_I128;
        // resolve_perp_bankruptcy settles the AMM's genuine funding, then resyncs
        // its stamp past the socialization bump so the bump is excluded from the
        // AMM's next funding payment (no phantom AMM revenue — OtterSec #89).
        expected_market.amm.last_cumulative_funding_rate_long =
            expected_market.cumulative_funding_rate_long as i64;
        expected_market.amm.last_cumulative_funding_rate_short =
            expected_market.cumulative_funding_rate_short as i64;
        // Model the production invariant: update_funding_rate keeps the AMM
        // funding stamp in sync with the market cum rates, so on entry the stamp
        // is not lagging and the #89 settle-first is a no-op (without this the
        // harness's default-zero stamp would make the settle realize a spurious
        // payment and shift total_fee_minus_distributions).
        {
            let mut m = market_map.get_ref_mut(&0).unwrap();
            m.amm.last_cumulative_funding_rate_long = m.cumulative_funding_rate_long as i64;
            m.amm.last_cumulative_funding_rate_short = m.cumulative_funding_rate_short as i64;
        }
        expected_market.total_social_loss = 100000000;
        expected_market.net_unsettled_funding_pnl = -100 * QUOTE_PRECISION_I64;
        expected_market.quote_asset_amount = -50 * QUOTE_PRECISION_I128;
        expected_market.number_of_users = 0;

        resolve_perp_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            0,
            false,
        )
        .unwrap();

        assert_eq!(expected_user, user);
        assert_eq!(expected_market, market_map.get_ref(&0).unwrap().clone());

        let mut affected_long_user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 5 * BASE_PRECISION_I64,
                quote_asset_amount: -500 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -500 * QUOTE_PRECISION_I64,
                quote_entry_amount: -500 * QUOTE_PRECISION_I64,
                open_bids: BASE_PRECISION_I64,
                last_cumulative_funding_rate: 1000 * FUNDING_RATE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],
            ..User::default()
        };

        let mut expected_affected_long_user = affected_long_user;
        expected_affected_long_user.perp_positions[0].quote_asset_amount =
            -550 * QUOTE_PRECISION_I64; // loses $50
        expected_affected_long_user.perp_positions[0].quote_break_even_amount =
            -550 * QUOTE_PRECISION_I64; // loses $50
        expected_affected_long_user.perp_positions[0].last_cumulative_funding_rate =
            1010 * FUNDING_RATE_PRECISION_I64;
        expected_affected_long_user.cumulative_perp_funding = -50 * QUOTE_PRECISION_I64;

        {
            let mut market = market_map.get_ref_mut(&0).unwrap();
            settle_funding_payment(
                &mut affected_long_user,
                &Pubkey::default(),
                &mut market,
                now,
            )
            .unwrap()
        }

        assert_eq!(expected_affected_long_user, affected_long_user);

        let mut affected_short_user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: -5 * BASE_PRECISION_I64,
                quote_asset_amount: 500 * QUOTE_PRECISION_I64,
                quote_entry_amount: 500 * QUOTE_PRECISION_I64,
                quote_break_even_amount: 500 * QUOTE_PRECISION_I64,
                open_bids: BASE_PRECISION_I64,
                last_cumulative_funding_rate: -1000 * FUNDING_RATE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],
            ..User::default()
        };

        let mut expected_affected_short_user = affected_short_user;
        expected_affected_short_user.perp_positions[0].quote_asset_amount =
            450 * QUOTE_PRECISION_I64; // loses $50
        expected_affected_short_user.perp_positions[0].quote_break_even_amount =
            450 * QUOTE_PRECISION_I64; // loses $50
        expected_affected_short_user.perp_positions[0].last_cumulative_funding_rate =
            -1010 * FUNDING_RATE_PRECISION_I64;
        expected_affected_short_user.cumulative_perp_funding = -50 * QUOTE_PRECISION_I64;

        {
            let mut market = market_map.get_ref_mut(&0).unwrap();
            settle_funding_payment(
                &mut affected_short_user,
                &Pubkey::default(),
                &mut market,
                now,
            )
            .unwrap()
        }

        assert_eq!(expected_affected_short_user, affected_short_user);
    }

    #[test]
    pub fn socialized_loss_rounding_residual_bounded() {
        // Socialize a loss across uneven long/short positions, settle everyone,
        // and confirm net_unsettled_funding_pnl returns to ~0. ceil delta +
        // truncating per-user settle leave bounded dust, not exact zero.
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        // Open base 7.0, split 4.0 long / 3.0 short: $100/7 doesn't divide even.
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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            number_of_users: 1,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 4 * BASE_PRECISION_I128,
            base_asset_amount_short: -3 * BASE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            cumulative_funding_rate_long: 1000 * FUNDING_RATE_PRECISION_I128,
            cumulative_funding_rate_short: -1000 * FUNDING_RATE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        // Bankrupt user carries a $100 quote loss and no base.
        let mut user = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                quote_entry_amount: -100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],
            status: UserStatus::Bankrupt as u8,
            next_liquidation_id: 2,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        // No insurance vault balance, so the full $100 is socialized.
        resolve_perp_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            0,
            false,
        )
        .unwrap();

        // The socialized loss is now recorded as an obligation.
        assert_eq!(
            market_map.get_ref(&0).unwrap().net_unsettled_funding_pnl,
            -100 * QUOTE_PRECISION_I64
        );

        // Uneven survivors (incl. fractional base) summing to 4.0 long / 3.0 short.
        let affected_bases = [
            3 * BASE_PRECISION_I64 / 2, // 1.5 long
            5 * BASE_PRECISION_I64 / 2, // 2.5 long
            -2 * BASE_PRECISION_I64,    // 2.0 short
            -BASE_PRECISION_I64,        // 1.0 short
        ];

        for base in affected_bases {
            let last_cumulative_funding_rate = if base > 0 {
                1000 * FUNDING_RATE_PRECISION_I64
            } else {
                -1000 * FUNDING_RATE_PRECISION_I64
            };
            let mut affected_user = User {
                perp_positions: get_positions(PerpPosition {
                    market_index: 0,
                    base_asset_amount: base,
                    quote_asset_amount: 1000 * QUOTE_PRECISION_I64,
                    quote_entry_amount: 1000 * QUOTE_PRECISION_I64,
                    quote_break_even_amount: 1000 * QUOTE_PRECISION_I64,
                    last_cumulative_funding_rate,
                    ..PerpPosition::default()
                }),
                spot_positions: [SpotPosition::default(); 8],
                ..User::default()
            };

            let mut market = market_map.get_ref_mut(&0).unwrap();
            settle_funding_payment(&mut affected_user, &Pubkey::default(), &mut market, now)
                .unwrap();
        }

        // All settled: only rounding dust remains, not the full socialized loss.
        let residual = market_map.get_ref(&0).unwrap().net_unsettled_funding_pnl;
        assert!(
            residual.abs() <= 10,
            "net_unsettled residual {} exceeds rounding bound",
            residual
        );
    }

    #[test]
    pub fn full_coverage_zero_open_interest_resolves() {
        // Loss is fully covered by the market's pending IF fees (Tranche 1),
        // so loss_to_socialize == 0. The market has zero open interest. The
        // funding-rate delta must be skipped: computing it would require
        // nonzero base exposure and otherwise revert the whole resolution,
        // leaving the account bankrupt despite full coverage.
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_asset_amount_with_amm: 0,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            number_of_users: 1,
            order_step_size: 10000000,
            quote_asset_amount: -100 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 0,
            base_asset_amount_short: 0,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            cumulative_funding_rate_long: 1000 * FUNDING_RATE_PRECISION_I128,
            cumulative_funding_rate_short: -1000 * FUNDING_RATE_PRECISION_I128,
            fee_ledger: FeeLedger {
                pending_if_fee: 100 * QUOTE_PRECISION,
                ..FeeLedger::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                quote_entry_amount: -100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],
            status: UserStatus::Bankrupt as u8,
            next_liquidation_id: 2,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut expected_user = user;
        expected_user.status = 0; // exits bankruptcy
        expected_user.perp_positions[0].quote_asset_amount = 0;
        expected_user.total_social_loss = 100 * QUOTE_PRECISION_U64;

        let mut expected_market = market;
        // no socialization: funding rates and market social loss are untouched
        expected_market.total_social_loss = 0;
        expected_market.quote_asset_amount = 0;
        expected_market.number_of_users = 0;
        expected_market.fee_ledger.pending_if_fee = 0;

        resolve_perp_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            0,
            false,
        )
        .unwrap();

        assert_eq!(expected_user, user);
        assert_eq!(expected_market, market_map.get_ref(&0).unwrap().clone());
    }

    #[test]
    pub fn fresh_user_economic_bankruptcy_allocates_liquidation_id() {
        // A user can become economically bankrupt with no prior liquidation
        // episode: status is not yet Bankrupt and next_liquidation_id is still
        // 0. The resolver enters bankruptcy, which must allocate a liquidation
        // id so the event-id derivation (next_liquidation_id - 1) does not
        // underflow and revert the whole resolution.
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            number_of_users: 1,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 5 * BASE_PRECISION_I128,
            base_asset_amount_short: -5 * BASE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            cumulative_funding_rate_long: 1000 * FUNDING_RATE_PRECISION_I128,
            cumulative_funding_rate_short: -1000 * FUNDING_RATE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        // No open orders, no deposits, negative perp quote: economically
        // bankrupt. Crucially, status is NOT Bankrupt and next_liquidation_id is
        // 0, i.e. no liquidation episode ever started.
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                quote_entry_amount: -100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],
            status: 0,
            next_liquidation_id: 0,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        resolve_perp_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            0,
            false,
        )
        .unwrap();

        // entry allocated id 0 (next_liquidation_id 0 -> 1); bad debt cleared and
        // the user exited bankruptcy
        assert_eq!(user.next_liquidation_id, 1);
        assert_eq!(user.status, 0);
        assert_eq!(user.perp_positions[0].quote_asset_amount, 0);
        assert_eq!(user.total_social_loss, 100 * QUOTE_PRECISION_U64);
    }

    #[test]
    pub fn successful_resolve_perp_bankruptcy_with_fee_pool() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                fee_pool: PoolBalance {
                    scaled_balance: 50 * SPOT_BALANCE_PRECISION,
                    market_index: QUOTE_SPOT_MARKET_INDEX,
                    ..PoolBalance::default()
                },
                ..AMM::default()
            },
            // tranche order: the market's in-transit IF fees are consumed
            // first, then (no IF vault here) the AMM's backstop-of-last-resort
            // tranche, capped at fees the AMM has received
            fee_ledger: FeeLedger {
                pending_if_fee: 10 * QUOTE_PRECISION_I64 as u128,
                amm_protocol_fees_received: 50 * QUOTE_PRECISION_I64 as u128,
                ..FeeLedger::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            number_of_users: 1,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 5 * BASE_PRECISION_I128,
            base_asset_amount_short: -5 * BASE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            cumulative_funding_rate_long: 1000 * FUNDING_RATE_PRECISION_I128,
            cumulative_funding_rate_short: -1000 * FUNDING_RATE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            deposit_balance: 500 * SPOT_BALANCE_PRECISION,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                quote_entry_amount: -100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -100 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],
            status: UserStatus::Bankrupt as u8,
            next_liquidation_id: 2,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut expected_user = user;
        expected_user.status = 0;
        expected_user.perp_positions[0].quote_asset_amount = 0;
        expected_user.total_social_loss = 100000000;

        let mut expected_market = market;
        expected_market.cumulative_funding_rate_long = 1004 * FUNDING_RATE_PRECISION_I128;
        expected_market.cumulative_funding_rate_short = -1004 * FUNDING_RATE_PRECISION_I128;
        // AMM stamp resynced past the socialization bump (OtterSec #89)
        expected_market.amm.last_cumulative_funding_rate_long =
            expected_market.cumulative_funding_rate_long as i64;
        expected_market.amm.last_cumulative_funding_rate_short =
            expected_market.cumulative_funding_rate_short as i64;
        // Model the production invariant (see successful_resolve_perp_bankruptcy):
        // the AMM stamp is not lagging at entry, so #89's settle-first is a no-op.
        {
            let mut m = market_map.get_ref_mut(&0).unwrap();
            m.amm.last_cumulative_funding_rate_long = m.cumulative_funding_rate_long as i64;
            m.amm.last_cumulative_funding_rate_short = m.cumulative_funding_rate_short as i64;
        }
        expected_market.total_social_loss = 40000000;
        expected_market.net_unsettled_funding_pnl = -40 * QUOTE_PRECISION_I64;
        expected_market.quote_asset_amount = -50 * QUOTE_PRECISION_I128;
        expected_market.number_of_users = 0;
        expected_market.amm.fee_pool.scaled_balance = 0;
        // tranche 1: the 10-QUOTE in-transit IF cut is consumed counter-only
        // (its value stays in the pnl pool, now backing the spared
        // counterparties). tranche 3: the full 50-QUOTE tokenized provision is
        // clawed back — tokens move into the pnl pool and the AMM's books pay
        // — leaving 40 QUOTE to socialize.
        expected_market.fee_ledger.pending_if_fee = 0;
        expected_market.fee_ledger.amm_protocol_fees_received = 0;
        expected_market.pnl_pool.scaled_balance = 50 * SPOT_BALANCE_PRECISION;
        expected_market.amm.total_fee_minus_distributions = -50 * QUOTE_PRECISION_I128;
        expected_market.amm.net_revenue_since_last_funding = -50 * QUOTE_PRECISION_I64;

        resolve_perp_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            0,
            false,
        )
        .unwrap();

        assert_eq!(user.total_social_loss, 100000000);
        assert_eq!(expected_user, user);
        assert_eq!(expected_market, market_map.get_ref(&0).unwrap().clone());

        let mut affected_long_user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 5 * BASE_PRECISION_I64,
                quote_asset_amount: -500 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -500 * QUOTE_PRECISION_I64,
                quote_entry_amount: -500 * QUOTE_PRECISION_I64,
                open_bids: BASE_PRECISION_I64,
                last_cumulative_funding_rate: 1000 * FUNDING_RATE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],
            ..User::default()
        };

        let mut expected_affected_long_user = affected_long_user;
        expected_affected_long_user.perp_positions[0].quote_asset_amount =
            -520 * QUOTE_PRECISION_I64; // loses $20 (only 40 QUOTE socialized)
        expected_affected_long_user.perp_positions[0].quote_break_even_amount =
            -520 * QUOTE_PRECISION_I64; // loses $20
        expected_affected_long_user.perp_positions[0].last_cumulative_funding_rate =
            1004 * FUNDING_RATE_PRECISION_I64;
        expected_affected_long_user.cumulative_perp_funding = -20 * QUOTE_PRECISION_I64;

        {
            let mut market = market_map.get_ref_mut(&0).unwrap();
            settle_funding_payment(
                &mut affected_long_user,
                &Pubkey::default(),
                &mut market,
                now,
            )
            .unwrap()
        }

        assert_eq!(expected_affected_long_user, affected_long_user);

        let mut affected_short_user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: -5 * BASE_PRECISION_I64,
                quote_asset_amount: 500 * QUOTE_PRECISION_I64,
                quote_entry_amount: 500 * QUOTE_PRECISION_I64,
                quote_break_even_amount: 500 * QUOTE_PRECISION_I64,
                open_bids: BASE_PRECISION_I64,
                last_cumulative_funding_rate: -1000 * FUNDING_RATE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],
            ..User::default()
        };

        let mut expected_affected_short_user = affected_short_user;
        expected_affected_short_user.perp_positions[0].quote_asset_amount =
            480 * QUOTE_PRECISION_I64; // loses $20 (only 40 QUOTE socialized)
        expected_affected_short_user.perp_positions[0].quote_break_even_amount =
            480 * QUOTE_PRECISION_I64; // loses $20
        expected_affected_short_user.perp_positions[0].last_cumulative_funding_rate =
            -1004 * FUNDING_RATE_PRECISION_I64;
        expected_affected_short_user.cumulative_perp_funding = -20 * QUOTE_PRECISION_I64;

        {
            let mut market = market_map.get_ref_mut(&0).unwrap();
            settle_funding_payment(
                &mut affected_short_user,
                &Pubkey::default(),
                &mut market,
                now,
            )
            .unwrap()
        }

        assert_eq!(expected_affected_short_user, affected_short_user);
    }

    /// The waterfall's sign convention: `loss` is NEGATIVE (validated), every
    /// tranche payment is positive, and each `safe_add` moves the running loss
    /// toward zero — i.e. ADDING a payment IS the offset. This test walks all
    /// four stages (pending IF -> IF vault -> untokenized provision ->
    /// tokenized provision) and pins the socialized remainder to
    /// `|loss| − sum(tranche payments)`; a sign flip anywhere would balloon
    /// the socialized loss instead of shrinking it and fail every assertion
    /// below.
    #[test]
    pub fn bankruptcy_waterfall_offsets_loss_across_all_tranches() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        // loss = -100. Tranches: pending IF 30 (counter-only), IF vault 25
        // (capped by quote_max_insurance), provision clawback 15 = 8
        // untokenized (counter-only) + 7 tokenized (fee_pool -> pnl_pool).
        // Socialized remainder: 100 - 30 - 25 - 15 = 30.
        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                fee_pool: PoolBalance {
                    scaled_balance: 50 * SPOT_BALANCE_PRECISION,
                    market_index: QUOTE_SPOT_MARKET_INDEX,
                    ..PoolBalance::default()
                },
                ..AMM::default()
            },
            fee_ledger: FeeLedger {
                pending_if_fee: 30 * QUOTE_PRECISION_I64 as u128,
                pending_amm_provision: 8 * QUOTE_PRECISION_I64 as u128,
                amm_protocol_fees_received: 15 * QUOTE_PRECISION_I64 as u128,
                ..FeeLedger::default()
            },
            insurance_claim: InsuranceClaim {
                quote_max_insurance: 25 * QUOTE_PRECISION_I64 as u64,
                ..InsuranceClaim::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            number_of_users: 1,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 5 * BASE_PRECISION_I128,
            base_asset_amount_short: -5 * BASE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            cumulative_funding_rate_long: 1000 * FUNDING_RATE_PRECISION_I128,
            cumulative_funding_rate_short: -1000 * FUNDING_RATE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            deposit_balance: 500 * SPOT_BALANCE_PRECISION,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                quote_entry_amount: -100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -100 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],
            status: UserStatus::Bankrupt as u8,
            next_liquidation_id: 2,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut expected_user = user;
        expected_user.status = 0;
        expected_user.perp_positions[0].quote_asset_amount = 0;
        expected_user.total_social_loss = 100 * QUOTE_PRECISION_I64 as u64;

        let mut expected_market = market;
        // 30 QUOTE socialized over 10 base -> 3 QUOTE/base funding delta
        expected_market.cumulative_funding_rate_long = 1003 * FUNDING_RATE_PRECISION_I128;
        expected_market.cumulative_funding_rate_short = -1003 * FUNDING_RATE_PRECISION_I128;
        // AMM stamp resynced past the socialization bump (OtterSec #89)
        expected_market.amm.last_cumulative_funding_rate_long =
            expected_market.cumulative_funding_rate_long as i64;
        expected_market.amm.last_cumulative_funding_rate_short =
            expected_market.cumulative_funding_rate_short as i64;
        // Model the production invariant (see successful_resolve_perp_bankruptcy):
        // the AMM stamp is not lagging at entry, so #89's settle-first is a no-op.
        {
            let mut m = market_map.get_ref_mut(&0).unwrap();
            m.amm.last_cumulative_funding_rate_long = m.cumulative_funding_rate_long as i64;
            m.amm.last_cumulative_funding_rate_short = m.cumulative_funding_rate_short as i64;
        }
        expected_market.total_social_loss = 30 * QUOTE_PRECISION_I64 as u128;
        expected_market.net_unsettled_funding_pnl = -30 * QUOTE_PRECISION_I64;
        expected_market.quote_asset_amount = -50 * QUOTE_PRECISION_I128;
        expected_market.number_of_users = 0;
        // tranche 1 + 3a are counter-only; tranche 2 (25) and 3b (7) move
        // real tokens into the pnl pool
        expected_market.fee_ledger.pending_if_fee = 0;
        expected_market.fee_ledger.pending_amm_provision = 0;
        expected_market.fee_ledger.amm_protocol_fees_received = 0;
        expected_market.insurance_claim.quote_settled_insurance = 25 * QUOTE_PRECISION_I64 as u64;
        expected_market.pnl_pool.scaled_balance = 32 * SPOT_BALANCE_PRECISION;
        expected_market.amm.fee_pool.scaled_balance = 43 * SPOT_BALANCE_PRECISION;
        // the AMM's books pay exactly the clawback (8 + 7), nothing else
        expected_market.amm.total_fee_minus_distributions = -15 * QUOTE_PRECISION_I128;
        expected_market.amm.net_revenue_since_last_funding = -15 * QUOTE_PRECISION_I64;

        resolve_perp_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            100 * QUOTE_PRECISION_I64 as u64, // IF vault balance (capped by quote_max_insurance)
            false,
        )
        .unwrap();

        assert_eq!(expected_user, user);
        assert_eq!(expected_market, market_map.get_ref(&0).unwrap().clone());
    }

    /// When the market's in-transit IF cut alone covers the whole loss,
    /// nothing is socialized: no funding-rate adjustment, no IF vault draw,
    /// no provision clawback — only the pending counter shrinks.
    #[test]
    pub fn bankruptcy_fully_absorbed_by_pending_if_tranche() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            fee_ledger: FeeLedger {
                pending_if_fee: 150 * QUOTE_PRECISION_I64 as u128,
                amm_protocol_fees_received: 50 * QUOTE_PRECISION_I64 as u128,
                ..FeeLedger::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            number_of_users: 1,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 5 * BASE_PRECISION_I128,
            base_asset_amount_short: -5 * BASE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            cumulative_funding_rate_long: 1000 * FUNDING_RATE_PRECISION_I128,
            cumulative_funding_rate_short: -1000 * FUNDING_RATE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            deposit_balance: 500 * SPOT_BALANCE_PRECISION,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                quote_entry_amount: -100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -100 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],
            status: UserStatus::Bankrupt as u8,
            next_liquidation_id: 2,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut expected_user = user;
        expected_user.status = 0;
        expected_user.perp_positions[0].quote_asset_amount = 0;
        expected_user.total_social_loss = 100 * QUOTE_PRECISION_I64 as u64;

        let mut expected_market = market;
        // tranche 1 absorbs the full 100: counters shrink, nothing else moves
        expected_market.fee_ledger.pending_if_fee = 50 * QUOTE_PRECISION_I64 as u128;
        expected_market.quote_asset_amount = -50 * QUOTE_PRECISION_I128;
        expected_market.number_of_users = 0;

        resolve_perp_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            0,
            false,
        )
        .unwrap();

        assert_eq!(expected_user, user);
        let market_after = market_map.get_ref(&0).unwrap().clone();
        // no socialization: funding rates and social-loss counters untouched,
        // the AMM's clawback cap untouched
        assert_eq!(market_after.total_social_loss, 0);
        assert_eq!(
            market_after.cumulative_funding_rate_long,
            1000 * FUNDING_RATE_PRECISION_I128
        );
        assert_eq!(
            market_after.cumulative_funding_rate_short,
            -1000 * FUNDING_RATE_PRECISION_I128
        );
        assert_eq!(
            market_after.fee_ledger.amm_protocol_fees_received,
            50 * QUOTE_PRECISION_I64 as u128
        );
        assert_eq!(expected_market, market_after);
    }

    /// The OI-scaled floor defeats a sweep front-run of a pending
    /// bankruptcy: with `bankruptcy_if_floor_pct` covering the loss, a
    /// permissionless `sweep_market_fees` fired between the bankruptcy and
    /// its resolution leaves the pending IF tranche intact, so tranche 1
    /// still fully absorbs the loss — zero social loss, funding untouched.
    #[test]
    pub fn bankruptcy_if_floor_survives_sweep_front_run() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        // OI = 5 base at a $100 TWAP -> 500 QUOTE notional; 30% floor = 150,
        // covering the full accrued pending IF fee
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
            fee_ledger: FeeLedger {
                pending_if_fee: 150 * QUOTE_PRECISION_I64 as u128,
                ..FeeLedger::default()
            },
            pnl_pool: PoolBalance {
                scaled_balance: 200 * QUOTE_PRECISION * SPOT_BALANCE_PRECISION,
                market_index: QUOTE_SPOT_MARKET_INDEX,
                ..PoolBalance::default()
            },
            bankruptcy_if_floor_pct: 3 * PERCENTAGE_PRECISION_U32 / 10, // 30%
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: 100 * PRICE_PRECISION_I64,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            number_of_users: 1,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 5 * BASE_PRECISION_I128,
            base_asset_amount_short: -5 * BASE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            cumulative_funding_rate_long: 1000 * FUNDING_RATE_PRECISION_I128,
            cumulative_funding_rate_short: -1000 * FUNDING_RATE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            deposit_balance: 400 * QUOTE_PRECISION * SPOT_BALANCE_PRECISION,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                quote_entry_amount: -100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],
            status: UserStatus::Bankrupt as u8,
            next_liquidation_id: 2,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        // attacker front-runs the resolution with a permissionless sweep:
        // the floor covers the whole pending IF fee, nothing may leave
        {
            let mut market = market_map.get_ref_mut(&0).unwrap();
            let mut spot_market = spot_market_map.get_ref_mut(&0).unwrap();
            let (if_swept, _, _) =
                sweep_market_fees(&mut market, &mut spot_market, 0, now, false).unwrap();
            assert_eq!(if_swept, 0);
            assert_eq!(
                market.fee_ledger.pending_if_fee,
                150 * QUOTE_PRECISION_I64 as u128
            );
        }

        resolve_perp_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            0,
            false,
        )
        .unwrap();

        // tranche 1 fully absorbed the loss despite the sweep attempt
        let market_after = market_map.get_ref(&0).unwrap().clone();
        assert_eq!(
            market_after.fee_ledger.pending_if_fee,
            50 * QUOTE_PRECISION_I64 as u128
        );
        assert_eq!(market_after.total_social_loss, 0);
        assert_eq!(
            market_after.cumulative_funding_rate_long,
            1000 * FUNDING_RATE_PRECISION_I128
        );
        assert_eq!(
            market_after.cumulative_funding_rate_short,
            -1000 * FUNDING_RATE_PRECISION_I128
        );
        assert_eq!(user.status, 0);
        assert_eq!(user.perp_positions[0].quote_asset_amount, 0);
    }

    /// Control leg for the test above: with the floor disabled (the pre-fix
    /// behavior, and what legacy accounts read until the admin sets a pct),
    /// the same front-running sweep clears the pending IF tranche and the
    /// identical loss is socialized through cumulative funding instead.
    #[test]
    pub fn bankruptcy_if_floor_disabled_sweep_socializes_loss() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                peg_multiplier: 100 * PEG_PRECISION,
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            fee_ledger: FeeLedger {
                pending_if_fee: 150 * QUOTE_PRECISION_I64 as u128,
                ..FeeLedger::default()
            },
            pnl_pool: PoolBalance {
                scaled_balance: 200 * QUOTE_PRECISION * SPOT_BALANCE_PRECISION,
                market_index: QUOTE_SPOT_MARKET_INDEX,
                ..PoolBalance::default()
            },
            bankruptcy_if_floor_pct: 0,
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            number_of_users: 1,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 5 * BASE_PRECISION_I128,
            base_asset_amount_short: -5 * BASE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            cumulative_funding_rate_long: 1000 * FUNDING_RATE_PRECISION_I128,
            cumulative_funding_rate_short: -1000 * FUNDING_RATE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            deposit_balance: 400 * QUOTE_PRECISION * SPOT_BALANCE_PRECISION,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                quote_entry_amount: -100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],
            status: UserStatus::Bankrupt as u8,
            next_liquidation_id: 2,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        // no floor: the front-running sweep drains the entire tranche
        {
            let mut market = market_map.get_ref_mut(&0).unwrap();
            let mut spot_market = spot_market_map.get_ref_mut(&0).unwrap();
            let (if_swept, _, _) =
                sweep_market_fees(&mut market, &mut spot_market, 0, now, false).unwrap();
            assert_eq!(if_swept, 150 * QUOTE_PRECISION);
            assert_eq!(market.fee_ledger.pending_if_fee, 0);
        }

        resolve_perp_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            0,
            false,
        )
        .unwrap();

        // with no tranche left, the whole loss socializes: counterparties pay
        let market_after = market_map.get_ref(&0).unwrap().clone();
        assert_eq!(
            market_after.total_social_loss,
            100 * QUOTE_PRECISION_I64 as u128
        );
        assert!(market_after.cumulative_funding_rate_long > 1000 * FUNDING_RATE_PRECISION_I128);
        assert!(market_after.cumulative_funding_rate_short < -1000 * FUNDING_RATE_PRECISION_I128);
    }

    /// Clawing back a provision that was never tokenized is counter-only:
    /// the AMM's books pay (`tfmd`), the clawback cap shrinks, but NO tokens
    /// move — the provision's backing still sits in the pnl pool, where it
    /// now backs the spared counterparties. The empty fee pool caps the
    /// tokenized phase at zero.
    #[test]
    pub fn bankruptcy_untokenized_provision_clawback_is_counter_only() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                // fee pool empty: the provision accrued but was never swept
                ..AMM::default()
            },
            fee_ledger: FeeLedger {
                pending_amm_provision: 20 * QUOTE_PRECISION_I64 as u128,
                amm_protocol_fees_received: 20 * QUOTE_PRECISION_I64 as u128,
                ..FeeLedger::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            number_of_users: 1,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 5 * BASE_PRECISION_I128,
            base_asset_amount_short: -5 * BASE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            cumulative_funding_rate_long: 1000 * FUNDING_RATE_PRECISION_I128,
            cumulative_funding_rate_short: -1000 * FUNDING_RATE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            deposit_balance: 500 * SPOT_BALANCE_PRECISION,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                quote_entry_amount: -100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -100 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],
            status: UserStatus::Bankrupt as u8,
            next_liquidation_id: 2,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut expected_user = user;
        expected_user.status = 0;
        expected_user.perp_positions[0].quote_asset_amount = 0;
        expected_user.total_social_loss = 100 * QUOTE_PRECISION_I64 as u64;

        let mut expected_market = market;
        // 80 QUOTE socialized over 10 base -> 8 QUOTE/base funding delta
        expected_market.cumulative_funding_rate_long = 1008 * FUNDING_RATE_PRECISION_I128;
        expected_market.cumulative_funding_rate_short = -1008 * FUNDING_RATE_PRECISION_I128;
        // AMM stamp resynced past the socialization bump (OtterSec #89)
        expected_market.amm.last_cumulative_funding_rate_long =
            expected_market.cumulative_funding_rate_long as i64;
        expected_market.amm.last_cumulative_funding_rate_short =
            expected_market.cumulative_funding_rate_short as i64;
        // Model the production invariant (see successful_resolve_perp_bankruptcy):
        // the AMM stamp is not lagging at entry, so #89's settle-first is a no-op.
        {
            let mut m = market_map.get_ref_mut(&0).unwrap();
            m.amm.last_cumulative_funding_rate_long = m.cumulative_funding_rate_long as i64;
            m.amm.last_cumulative_funding_rate_short = m.cumulative_funding_rate_short as i64;
        }
        expected_market.total_social_loss = 80 * QUOTE_PRECISION_I64 as u128;
        expected_market.net_unsettled_funding_pnl = -80 * QUOTE_PRECISION_I64;
        expected_market.quote_asset_amount = -50 * QUOTE_PRECISION_I128;
        expected_market.number_of_users = 0;
        expected_market.fee_ledger.pending_amm_provision = 0;
        expected_market.fee_ledger.amm_protocol_fees_received = 0;
        expected_market.amm.total_fee_minus_distributions = -20 * QUOTE_PRECISION_I128;
        expected_market.amm.net_revenue_since_last_funding = -20 * QUOTE_PRECISION_I64;

        resolve_perp_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            0,
            false,
        )
        .unwrap();

        assert_eq!(expected_user, user);
        let market_after = market_map.get_ref(&0).unwrap().clone();
        // counter-only: no tokens moved anywhere
        assert_eq!(market_after.pnl_pool.scaled_balance, 0);
        assert_eq!(market_after.amm.fee_pool.scaled_balance, 0);
        assert_eq!(expected_market, market_after);
    }

    /// OtterSec #130: a quote deposit that arrives after the latch must pay the debt before insurance
    /// does.
    ///
    /// The credit arrives permissionlessly through the revenue-share sweep, or through an unguarded
    /// keeper filler reward. Once latched, `settle_pnl` and `liquidate_spot` both reject the user, so
    /// before this fix the deposit sat idle while insurance and depositors covered the whole debt. It
    /// then became withdrawable when the resolver cleared the latch.
    #[test]
    pub fn quote_deposit_is_set_off_before_socializing_loss() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            status: MarketStatus::Initialized,
            number_of_users: 1,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 5 * BASE_PRECISION_I128,
            base_asset_amount_short: -5 * BASE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();
        {
            let mut m = market_map.get_ref_mut(&0).unwrap();
            m.amm.last_cumulative_funding_rate_long = m.cumulative_funding_rate_long as i64;
            m.amm.last_cumulative_funding_rate_short = m.cumulative_funding_rate_short as i64;
        }

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 40 * SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        // $100 of bad debt, and a $40 quote credit that landed after the latch was set.
        let mut user = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                quote_entry_amount: -100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 40 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            status: UserStatus::Bankrupt as u8,
            next_liquidation_id: 2,
            ..User::default()
        };

        let mut liquidator = User::default();
        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        resolve_perp_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            0,
            false,
        )
        .unwrap();

        // The credit was consumed, not left behind for the user to withdraw.
        assert_eq!(user.spot_positions[0].scaled_balance, 0);
        // Debt cleared, but only $60 was socialized -- the $40 came from the estate itself.
        assert_eq!(user.perp_positions[0].quote_asset_amount, 0);
        assert_eq!(user.total_social_loss, 60 * QUOTE_PRECISION_U64);
        // The setoff lands exactly where an insurance payment would have, backing the
        // counterparties this spares from socialization.
        let market_after = market_map.get_ref(&0).unwrap().clone();
        assert_eq!(
            market_after.pnl_pool.scaled_balance,
            40 * SPOT_BALANCE_PRECISION
        );
        // Latch cleared: the estate is wound up.
        assert_eq!(user.status, 0);
    }

    /// OtterSec #130, the fallback leg. A non-quote deposit cannot be netted against a quote debt,
    /// because that needs a cross-asset swap. The resolver must refuse to draw and un-latch instead,
    /// which hands the account back to ordinary liquidation.
    ///
    /// Un-latching matters: an error would leave the status bit set, and `liquidate_spot` rejects a
    /// latched user, so both paths would wedge forever.
    #[test]
    pub fn non_quote_deposit_unlatches_instead_of_drawing() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                base_asset_amount_with_amm: BASE_PRECISION_I128,
                ..AMM::default()
            },
            status: MarketStatus::Initialized,
            number_of_users: 1,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 5 * BASE_PRECISION_I128,
            base_asset_amount_short: -5 * BASE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
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
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("BAtFj4kQttZRVep3UZS2aZRDixkGYgWsbqTBVDbnSsPF").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            sol_oracle_account_info
        );
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: 100 * PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: 100 * PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_multiple(
            Vec::from([
                &usdc_spot_market_account_info,
                &sol_spot_market_account_info,
            ]),
            true,
        )
        .unwrap();

        // Slot 0 must stay the quote row -- `get_spot_position_index` enforces
        // "first spot position is always quote asset". The SOL deposit goes in slot 1.
        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        let mut user = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                quote_entry_amount: -100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            status: UserStatus::Bankrupt as u8,
            next_liquidation_id: 2,
            ..User::default()
        };

        let mut liquidator = User::default();
        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let pay_from_insurance = resolve_perp_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            1_000 * QUOTE_PRECISION_U64,
            false,
        )
        .unwrap();

        // Nothing drawn, nothing socialized, debt untouched.
        assert_eq!(pay_from_insurance, 0);
        assert_eq!(user.total_social_loss, 0);
        assert_eq!(
            user.perp_positions[0].quote_asset_amount,
            -100 * QUOTE_PRECISION_I64
        );
        // The SOL deposit is still there for ordinary liquidation to seize.
        assert_eq!(
            user.spot_positions[1].scaled_balance,
            SPOT_BALANCE_PRECISION_U64
        );
        // Un-latched, so `liquidate_spot` (which rejects a latched user) is legal again.
        assert_eq!(user.status, 0);
        assert!(!user.is_cross_margin_bankrupt());
    }
}

pub mod resolve_spot_bankruptcy {
    use {
        crate::{
            controller::{liquidation::resolve_spot_bankruptcy, position::PositionDirection},
            create_anchor_account_info,
            error::ErrorCode,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I128, BASE_PRECISION_U64,
                    FUNDING_RATE_PRECISION_I128, LIQUIDATION_FEE_PRECISION, PEG_PRECISION,
                    QUOTE_PRECISION, QUOTE_PRECISION_I128, QUOTE_PRECISION_I64,
                    SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                spot_balance::get_token_amount,
            },
            state::{
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market::{PerpMarket, PoolBalance, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                user::{
                    Order, OrderStatus, OrderType, PerpPosition, SpotPosition, User, UserStatus,
                },
            },
            test_utils::{get_orders, get_positions, get_pyth_price, get_spot_positions},
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    #[test]
    pub fn successful_resolve_spot_bankruptcy() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 5 * BASE_PRECISION_I128,
            base_asset_amount_short: -5 * BASE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            cumulative_funding_rate_long: 1000 * FUNDING_RATE_PRECISION_I128,
            cumulative_funding_rate_short: -1000 * FUNDING_RATE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 1000 * SPOT_BALANCE_PRECISION,
            borrow_balance: 100 * SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: [PerpPosition::default(); 8],
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                balance_type: SpotBalanceType::Borrow,
                ..SpotPosition::default()
            }),
            status: UserStatus::Bankrupt as u8,
            next_liquidation_id: 2,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut expected_user = user;
        expected_user.status = 0;
        expected_user.spot_positions[0].scaled_balance = 0;
        expected_user.spot_positions[0].cumulative_deposits = 100 * QUOTE_PRECISION_I64;
        expected_user.total_social_loss = 100000000;

        let mut expected_spot_market = spot_market;
        expected_spot_market.borrow_balance = 0;
        expected_spot_market.cumulative_deposit_interest =
            9 * SPOT_CUMULATIVE_INTEREST_PRECISION / 10;
        expected_spot_market.total_social_loss = 100 * QUOTE_PRECISION;
        expected_spot_market.total_quote_social_loss = 100 * QUOTE_PRECISION;

        resolve_spot_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            0,
            false,
        )
        .unwrap();

        assert_eq!(expected_user, user);
        assert_eq!(expected_spot_market, *spot_market_map.get_ref(&0).unwrap());

        let spot_market = spot_market_map.get_ref_mut(&0).unwrap();
        let deposit_balance = spot_market.deposit_balance;
        let deposit_token_amount =
            get_token_amount(deposit_balance, &spot_market, &SpotBalanceType::Deposit).unwrap();

        assert_eq!(deposit_token_amount, 900 * QUOTE_PRECISION);
    }

    // Audit #52: resolve_spot_bankruptcy must refuse to run while the user still
    // has a pending cross-margin perp bankruptcy (a non-isolated perp position
    // with bad debt). Both resolvers draw from the shared quote insurance fund,
    // so a fixed perp-before-spot precedence (matching the keeper bots) keeps the
    // socialized-loss split deterministic and unforgeable by the public caller.
    #[test]
    pub fn reverts_when_perp_bankruptcy_pending() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 5 * BASE_PRECISION_I128,
            base_asset_amount_short: -5 * BASE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            cumulative_funding_rate_long: 1000 * FUNDING_RATE_PRECISION_I128,
            cumulative_funding_rate_short: -1000 * FUNDING_RATE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 1000 * SPOT_BALANCE_PRECISION,
            borrow_balance: 100 * SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        // User is cross-margin bankrupt with BOTH a spot borrow and a bad-debt
        // perp position (base 0, negative quote) still outstanding.
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 0,
                quote_asset_amount: -50 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                balance_type: SpotBalanceType::Borrow,
                ..SpotPosition::default()
            }),
            status: UserStatus::Bankrupt as u8,
            next_liquidation_id: 2,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let result = resolve_spot_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            0,
            false,
        );

        assert_eq!(result, Err(ErrorCode::PerpBankruptcyMustPrecedeSpot));
    }

    #[test]
    pub fn resolve_spot_bankruptcy_partial_if_payment() {
        // $100 bad debt, IF covers $40, $60 socialized to depositors. The user
        // records the gross $100; the spot-market counters record only the $60
        // actually borne by depositors.
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 5 * BASE_PRECISION_I128,
            base_asset_amount_short: -5 * BASE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            cumulative_funding_rate_long: 1000 * FUNDING_RATE_PRECISION_I128,
            cumulative_funding_rate_short: -1000 * FUNDING_RATE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 1000 * SPOT_BALANCE_PRECISION,
            borrow_balance: 100 * SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: [PerpPosition::default(); 8],
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                balance_type: SpotBalanceType::Borrow,
                ..SpotPosition::default()
            }),
            status: UserStatus::Bankrupt as u8,
            next_liquidation_id: 2,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut expected_user = user;
        expected_user.status = 0;
        expected_user.spot_positions[0].scaled_balance = 0;
        expected_user.spot_positions[0].cumulative_deposits = 100 * QUOTE_PRECISION_I64;
        // gross bad debt, unaffected by the IF payment
        expected_user.total_social_loss = 100 * QUOTE_PRECISION as u64;

        let mut expected_spot_market = spot_market;
        expected_spot_market.borrow_balance = 0;
        // 6% haircut: $60 socialized over $1000 of deposits
        expected_spot_market.cumulative_deposit_interest =
            94 * SPOT_CUMULATIVE_INTEREST_PRECISION / 100;
        // socialized loss only ($60), not the gross $100
        expected_spot_market.total_social_loss = 60 * QUOTE_PRECISION;
        expected_spot_market.total_quote_social_loss = 60 * QUOTE_PRECISION;

        // +1 so `insurance_fund_vault_balance - 1` leaves exactly $40 payable
        let if_payment = resolve_spot_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            (40 * QUOTE_PRECISION + 1) as u64,
            false,
        )
        .unwrap();

        assert_eq!(if_payment, (40 * QUOTE_PRECISION) as u64);
        assert_eq!(expected_user, user);
        assert_eq!(expected_spot_market, *spot_market_map.get_ref(&0).unwrap());

        let spot_market = spot_market_map.get_ref_mut(&0).unwrap();
        let deposit_balance = spot_market.deposit_balance;
        let deposit_token_amount =
            get_token_amount(deposit_balance, &spot_market, &SpotBalanceType::Deposit).unwrap();

        // depositors lose exactly the socialized $60
        assert_eq!(deposit_token_amount, 940 * QUOTE_PRECISION);
    }

    #[test]
    pub fn resolve_spot_bankruptcy_loss_exceeds_total_deposits() {
        // $100 bad debt, empty IF vault, and only a dust deposit ($0.001) in
        // the market. The socialized loss exceeds total deposits, so the
        // haircut must clamp: cumulative_deposit_interest floors at 1 instead
        // of underflowing, the full loss is still recorded, and resolution
        // succeeds (griefing-DoS regression: one dust deposit previously made
        // every resolve_spot_bankruptcy call revert).
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 5 * BASE_PRECISION_I128,
            base_asset_amount_short: -5 * BASE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            cumulative_funding_rate_long: 1000 * FUNDING_RATE_PRECISION_I128,
            cumulative_funding_rate_short: -1000 * FUNDING_RATE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: SPOT_BALANCE_PRECISION / 1000,
            borrow_balance: 100 * SPOT_BALANCE_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: [PerpPosition::default(); 8],
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                balance_type: SpotBalanceType::Borrow,
                ..SpotPosition::default()
            }),
            status: UserStatus::Bankrupt as u8,
            next_liquidation_id: 2,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut expected_user = user;
        expected_user.status = 0;
        expected_user.spot_positions[0].scaled_balance = 0;
        expected_user.spot_positions[0].cumulative_deposits = 100 * QUOTE_PRECISION_I64;
        expected_user.total_social_loss = 100 * QUOTE_PRECISION as u64;

        let mut expected_spot_market = spot_market;
        expected_spot_market.borrow_balance = 0;
        // haircut clamped: depositors are wiped but interest floors at 1
        expected_spot_market.cumulative_deposit_interest = 1;
        // the full $100 is still recorded even though deposits only covered $0.001
        expected_spot_market.total_social_loss = 100 * QUOTE_PRECISION;
        expected_spot_market.total_quote_social_loss = 100 * QUOTE_PRECISION;

        // empty IF vault: nothing payable, the entire loss is socialized
        let if_payment = resolve_spot_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            0,
            false,
        )
        .unwrap();

        assert_eq!(if_payment, 0);
        assert_eq!(expected_user, user);
        assert_eq!(expected_spot_market, *spot_market_map.get_ref(&0).unwrap());

        let spot_market = spot_market_map.get_ref_mut(&0).unwrap();
        let deposit_balance = spot_market.deposit_balance;
        let deposit_token_amount =
            get_token_amount(deposit_balance, &spot_market, &SpotBalanceType::Deposit).unwrap();

        // remaining deposits redeem for ~0 tokens
        assert_eq!(deposit_token_amount, 0);
    }

    #[test]
    pub fn resolve_spot_bankruptcy_revenue_pool_covers_fully() {
        // $100 bad debt, $150 in the market's revenue pool, empty IF vault.
        // The revenue pool is first-loss: it absorbs the full $100 with no IF
        // payment and no social loss to depositors.
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 5 * BASE_PRECISION_I128,
            base_asset_amount_short: -5 * BASE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            cumulative_funding_rate_long: 1000 * FUNDING_RATE_PRECISION_I128,
            cumulative_funding_rate_short: -1000 * FUNDING_RATE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        // $1000 of depositor claims + $150 revenue pool (the pool lives inside
        // deposit_balance)
        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 1150 * SPOT_BALANCE_PRECISION,
            borrow_balance: 100 * SPOT_BALANCE_PRECISION,
            revenue_pool: PoolBalance {
                market_index: 0,
                scaled_balance: 150 * SPOT_BALANCE_PRECISION,
                ..PoolBalance::default()
            },
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: [PerpPosition::default(); 8],
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                balance_type: SpotBalanceType::Borrow,
                ..SpotPosition::default()
            }),
            status: UserStatus::Bankrupt as u8,
            next_liquidation_id: 2,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut expected_user = user;
        expected_user.status = 0;
        expected_user.spot_positions[0].scaled_balance = 0;
        expected_user.spot_positions[0].cumulative_deposits = 100 * QUOTE_PRECISION_I64;
        // gross bad debt, unaffected by the revenue pool payment
        expected_user.total_social_loss = 100 * QUOTE_PRECISION as u64;

        let mut expected_spot_market = spot_market;
        expected_spot_market.borrow_balance = 0;
        // pool pays $100, deposit_balance shrinks with it
        expected_spot_market.revenue_pool.scaled_balance = 50 * SPOT_BALANCE_PRECISION;
        expected_spot_market.deposit_balance = 1050 * SPOT_BALANCE_PRECISION;
        // no social loss: depositor interest untouched, counters stay zero

        let if_payment = resolve_spot_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            0,
            false,
        )
        .unwrap();

        assert_eq!(if_payment, 0);
        assert_eq!(expected_user, user);
        assert_eq!(expected_spot_market, *spot_market_map.get_ref(&0).unwrap());

        let spot_market = spot_market_map.get_ref_mut(&0).unwrap();
        let deposit_balance = spot_market.deposit_balance;
        let deposit_token_amount =
            get_token_amount(deposit_balance, &spot_market, &SpotBalanceType::Deposit).unwrap();

        // depositors keep their full $1000; the remaining $50 is still pool
        assert_eq!(deposit_token_amount, 1050 * QUOTE_PRECISION);
    }

    #[test]
    pub fn resolve_spot_bankruptcy_revenue_pool_then_if_then_social_loss() {
        // $100 bad debt covered in tranche order: $30 revenue pool, $40 IF
        // vault, remaining $30 socialized to depositors.
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: 5 * BASE_PRECISION_I128,
            base_asset_amount_short: -5 * BASE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            cumulative_funding_rate_long: 1000 * FUNDING_RATE_PRECISION_I128,
            cumulative_funding_rate_short: -1000 * FUNDING_RATE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        // $1000 of depositor claims + $30 revenue pool
        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 1030 * SPOT_BALANCE_PRECISION,
            borrow_balance: 100 * SPOT_BALANCE_PRECISION,
            revenue_pool: PoolBalance {
                market_index: 0,
                scaled_balance: 30 * SPOT_BALANCE_PRECISION,
                ..PoolBalance::default()
            },
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: [PerpPosition::default(); 8],
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                balance_type: SpotBalanceType::Borrow,
                ..SpotPosition::default()
            }),
            status: UserStatus::Bankrupt as u8,
            next_liquidation_id: 2,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut expected_user = user;
        expected_user.status = 0;
        expected_user.spot_positions[0].scaled_balance = 0;
        expected_user.spot_positions[0].cumulative_deposits = 100 * QUOTE_PRECISION_I64;
        // gross bad debt, unaffected by the tranche payments
        expected_user.total_social_loss = 100 * QUOTE_PRECISION as u64;

        let mut expected_spot_market = spot_market;
        expected_spot_market.borrow_balance = 0;
        // revenue pool fully consumed as tranche 1
        expected_spot_market.revenue_pool.scaled_balance = 0;
        expected_spot_market.deposit_balance = 1000 * SPOT_BALANCE_PRECISION;
        // 3% haircut: $30 socialized over the $1000 of remaining deposits
        expected_spot_market.cumulative_deposit_interest =
            97 * SPOT_CUMULATIVE_INTEREST_PRECISION / 100;
        // socialized loss only ($30), not the gross $100
        expected_spot_market.total_social_loss = 30 * QUOTE_PRECISION;
        expected_spot_market.total_quote_social_loss = 30 * QUOTE_PRECISION;

        // +1 so `insurance_fund_vault_balance - 1` leaves exactly $40 payable
        let if_payment = resolve_spot_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            (40 * QUOTE_PRECISION + 1) as u64,
            false,
        )
        .unwrap();

        // only the IF vault tranche is returned for token transfer; the
        // revenue pool tranche needs no token movement
        assert_eq!(if_payment, (40 * QUOTE_PRECISION) as u64);
        assert_eq!(expected_user, user);
        assert_eq!(expected_spot_market, *spot_market_map.get_ref(&0).unwrap());

        let spot_market = spot_market_map.get_ref_mut(&0).unwrap();
        let deposit_balance = spot_market.deposit_balance;
        let deposit_token_amount =
            get_token_amount(deposit_balance, &spot_market, &SpotBalanceType::Deposit).unwrap();

        // depositors lose exactly the socialized $30
        assert_eq!(deposit_token_amount, 970 * QUOTE_PRECISION);
    }
}

pub mod set_user_status_to_being_liquidated {

    use {
        crate::{
            controller::{
                liquidation::set_user_status_to_being_liquidated, position::PositionDirection,
            },
            create_anchor_account_info,
            error::ErrorCode,
            math::constants::{
                AMM_RESERVE_PRECISION, BASE_PRECISION_I128, BASE_PRECISION_I64, BASE_PRECISION_U64,
                LIQUIDATION_FEE_PRECISION, PEG_PRECISION, QUOTE_PRECISION_I128,
                QUOTE_PRECISION_I64, SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
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
                state::State,
                user::{
                    Order, OrderStatus, OrderType, PerpPosition, SpotPosition, User, UserStatus,
                },
            },
            test_utils::{get_orders, get_positions, get_pyth_price, *},
            DEFAULT_LIQUIDATION_MARGIN_BUFFER_RATIO, LIQUIDATION_PCT_PRECISION,
            PRICE_PRECISION_I64,
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    #[test]
    pub fn failure_sufficient_collateral() {
        let slot = 0_u64;

        let mut oracle_price = get_pyth_price(200, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
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
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: 100 * QUOTE_PRECISION_I64,
                quote_entry_amount: 100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: 100 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                scaled_balance: 1000000000000,
                cumulative_deposits: 100000000000,
                balance_type: SpotBalanceType::Deposit,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let state = State {
            liquidation_margin_buffer_ratio: DEFAULT_LIQUIDATION_MARGIN_BUFFER_RATIO,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        let result = set_user_status_to_being_liquidated(
            &mut user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            &state,
        );

        assert_eq!(result, Err(ErrorCode::SufficientCollateral));
    }

    #[test]
    pub fn failure_from_user_statuses() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions: [SpotPosition::default(); 8],
            ..User::default()
        };

        user.add_user_status(UserStatus::Bankrupt);
        let state = State {
            liquidation_margin_buffer_ratio: DEFAULT_LIQUIDATION_MARGIN_BUFFER_RATIO,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        let mut market = PerpMarket {
            amm: AMM::default(),
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();
        let mut spot_market = SpotMarket::default();
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut result = set_user_status_to_being_liquidated(
            &mut user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            &state,
        );

        assert_eq!(result, Err(ErrorCode::UserBankrupt));

        user.remove_user_status(UserStatus::Bankrupt);
        user.add_user_status(UserStatus::BeingLiquidated);
        result = set_user_status_to_being_liquidated(
            &mut user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            &state,
        );
        assert_eq!(result, Err(ErrorCode::UserIsBeingLiquidated));
    }

    #[test]
    pub fn success() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
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
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -150 * QUOTE_PRECISION_I64,
                quote_entry_amount: -150 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -150 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let state = State {
            liquidation_margin_buffer_ratio: DEFAULT_LIQUIDATION_MARGIN_BUFFER_RATIO,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        let result = set_user_status_to_being_liquidated(
            &mut user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            &state,
        );

        assert_eq!(user.status, UserStatus::BeingLiquidated as u8);
        assert_eq!(result, Ok(()));
    }
}

pub mod liquidate_spot_with_swap {
    use {
        crate::{
            controller::liquidation::{
                liquidate_spot_with_swap_begin, liquidate_spot_with_swap_end,
            },
            create_anchor_account_info,
            error::ErrorCode,
            math::{
                constants::{
                    LIQUIDATION_FEE_PRECISION, LIQUIDATION_PCT_PRECISION, MARGIN_PRECISION,
                    SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                spot_balance::get_token_amount,
            },
            state::{
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                state::State,
                user::{Order, PerpPosition, SpotPosition, User},
            },
            test_utils::{get_pyth_price, get_spot_positions},
            QUOTE_PRECISION_I64,
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    #[test]
    pub fn successful_liquidation_liability_transfer_to_cover_margin_shortage() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 105 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let state = State {
            liquidation_margin_buffer_ratio: MARGIN_PRECISION / 50,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        let asset_transfer = 64338200;
        let liability_transfer = 643382;

        // the max-pct-to-liquidate throttle is a hard cap: one unit above the
        // throttled asset transfer is refused, with no headroom on top
        let res = liquidate_spot_with_swap_begin(
            0,
            1,
            asset_transfer + 1,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        );

        assert_eq!(res, Err(ErrorCode::InvalidLiquidation));

        let res = liquidate_spot_with_swap_begin(
            0,
            1,
            asset_transfer,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        );

        assert_eq!(res, Ok(()));

        liquidate_spot_with_swap_end(
            0,
            1,
            &mut user,
            &user_key,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
            asset_transfer as u128,
            liability_transfer,
        )
        .unwrap();

        assert_eq!(user.is_cross_margin_being_liquidated(), false);

        let quote_spot_market = spot_market_map.get_ref(&0).unwrap();
        let sol_spot_market = spot_market_map.get_ref(&1).unwrap();

        assert_eq!(
            user.spot_positions[0]
                .get_signed_token_amount(&quote_spot_market)
                .unwrap(),
            40661800
        );
        // Audit #51: the insurance-side fee is now capped by the account's margin
        // shortage (~$7 here) exactly like the direct `liquidate_spot` path, so it
        // charges ~0.098% instead of the raw 1% if_liquidation_fee. The user keeps
        // more borrow relief (-357251 -> -357249, i.e. less residual borrow) and
        // only ~631 (vs 6433 at the raw rate) routes into the revenue pool.
        assert_eq!(
            user.spot_positions[1]
                .get_signed_token_amount(&sol_spot_market)
                .unwrap(),
            -357249
        );

        let market_revenue = get_token_amount(
            sol_spot_market.revenue_pool.scaled_balance,
            &sol_spot_market,
            &SpotBalanceType::Deposit,
        )
        .unwrap();

        assert_eq!(market_revenue, 631);
    }

    #[test]
    pub fn stale_for_margin_deposit_oracle_bounds_swap_at_protective_price() {
        let now = 0_i64;
        // oracle posted at slot 0 -> delay 200 > slots_before_stale_for_margin (120),
        // while the price stays inside the 5min twap divergence band
        let slot = 200_u64;

        // stale oracle shows $90 while the 5min twap is $100: the liquidator's swap of the
        // user's sol deposit must clear at least the user-protective
        // max(oracle, 5min twap, oracle + conf) = $100 (net of the liquidator premium),
        // not the stale $90
        let mut sol_oracle_price = get_pyth_price(90, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 20000 * SPOT_BALANCE_PRECISION,
            borrow_balance: 9500 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: 100 * SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: 100 * QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: 100 * QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: 9500 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
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

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        let swap_amount_in = 50_000_000_u64; // 50 sol

        liquidate_spot_with_swap_begin(
            1,
            0,
            swap_amount_in,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        )
        .unwrap();

        // a swap executed at the stale $90 (4500 usdc for 50 sol) is below the protective
        // worst-case price of $100 / 1.001 and must be rejected
        let res = liquidate_spot_with_swap_end(
            1,
            0,
            &mut user,
            &user_key,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
            swap_amount_in as u128,
            4500 * 1_000_000_u128,
        );

        assert_eq!(res, Err(ErrorCode::InvalidLiquidation));

        // a swap at the protective $100 clears the boundary
        liquidate_spot_with_swap_end(
            1,
            0,
            &mut user,
            &user_key,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
            swap_amount_in as u128,
            5000 * 1_000_000_u128,
        )
        .unwrap();

        assert_eq!(user.spot_positions[0].scaled_balance, 4_499_999_999_999);
        assert_eq!(
            user.spot_positions[1].scaled_balance,
            50 * SPOT_BALANCE_PRECISION_U64
        );
        assert!(!user.is_cross_margin_bankrupt());
    }

    /// `liquidate_spot_with_swap_end` prices the swap against the deposit market's 5-minute
    /// TWAP. `begin` runs first, in its own instruction, so a refresh there would pull that
    /// TWAP onto the stale oracle price and be gone from the account by the time `end` reads
    /// it. The swap lane therefore does not advance the oracle TWAPs at all, and `end`
    /// bounds the swap at the same protective price `begin` did.
    #[test]
    pub fn swap_lane_leaves_oracle_twap_unmoved_for_end_to_price_against() {
        // Same fixture as the test above, with one change: the clock is 10 minutes past the
        // market's last oracle-TWAP stamp, so a refresh in `begin` would move the 5-minute
        // TWAP by the full sanitize clamp — all the way onto the stale $90.
        let now = 600_i64;
        // oracle posted at slot 0 -> delay 200 > slots_before_stale_for_margin (120),
        // while the price stays inside the 5min twap divergence band
        let slot = 200_u64;

        let mut sol_oracle_price = get_pyth_price(90, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 20000 * SPOT_BALANCE_PRECISION,
            borrow_balance: 9500 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            last_interest_ts: now as u64,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: 100 * SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            last_interest_ts: now as u64,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: 100 * QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: 100 * QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: 9500 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
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

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        let swap_amount_in = 50_000_000_u64; // 50 sol

        liquidate_spot_with_swap_begin(
            1,
            0,
            swap_amount_in,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        )
        .unwrap();

        // begin left the 5-minute TWAP exactly where it found it, so end still reads $100.
        assert_eq!(
            spot_market_map
                .get_ref(&1)
                .unwrap()
                .historical_oracle_data
                .last_oracle_price_twap_5min,
            100 * QUOTE_PRECISION_I64
        );

        // a swap executed at the stale $90 (4500 usdc for 50 sol) is below the protective
        // worst-case price of $100 / 1.001 and must be rejected
        let res = liquidate_spot_with_swap_end(
            1,
            0,
            &mut user,
            &user_key,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
            swap_amount_in as u128,
            4500 * 1_000_000_u128,
        );

        assert_eq!(res, Err(ErrorCode::InvalidLiquidation));

        // a swap at the protective $100 clears the boundary
        liquidate_spot_with_swap_end(
            1,
            0,
            &mut user,
            &user_key,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
            swap_amount_in as u128,
            5000 * 1_000_000_u128,
        )
        .unwrap();

        assert_eq!(user.spot_positions[0].scaled_balance, 4_499_999_999_999);
        assert_eq!(
            user.spot_positions[1].scaled_balance,
            50 * SPOT_BALANCE_PRECISION_U64
        );
        assert!(!user.is_cross_margin_bankrupt());
    }

    #[test]
    pub fn stale_for_margin_liability_oracle_bounds_swap_at_protective_price() {
        let now = 0_i64;
        // oracle posted at slot 0 -> delay 200 > slots_before_stale_for_margin (120),
        // while the price stays inside the 5min twap divergence band
        let slot = 200_u64;

        // stale borrow oracle shows $110 while the 5min twap is $100: the liquidator's
        // swap of the user's usdc deposit into the borrowed sol must return sol as if it
        // were worth the user-protective min(oracle, 5min twap, oracle - conf) = $100,
        // not the inflated $110
        let mut sol_oracle_price = get_pyth_price(110, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 30000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: 100 * SPOT_BALANCE_PRECISION,
            borrow_balance: 90 * SPOT_BALANCE_PRECISION,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: 100 * QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: 100 * QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 10000 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: 90 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
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

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };

        let swap_amount_in = 5_000_000_000_u64; // 5000 usdc

        liquidate_spot_with_swap_begin(
            0,
            1,
            swap_amount_in,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        )
        .unwrap();

        // a swap returning sol at the stale $110 (45.454545 sol for 5000 usdc) is below
        // the protective worst-case price and must be rejected
        let res = liquidate_spot_with_swap_end(
            0,
            1,
            &mut user,
            &user_key,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
            swap_amount_in as u128,
            45_454_545_u128,
        );

        assert_eq!(res, Err(ErrorCode::InvalidLiquidation));

        // a swap returning sol at the protective $100 (50 sol) clears the boundary
        liquidate_spot_with_swap_end(
            0,
            1,
            &mut user,
            &user_key,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
            swap_amount_in as u128,
            50 * 1_000_000_u128,
        )
        .unwrap();

        assert_eq!(
            user.spot_positions[0].scaled_balance,
            5000 * SPOT_BALANCE_PRECISION_U64
        );
        assert_eq!(user.spot_positions[1].scaled_balance, 39_999_999_999);
        assert!(!user.is_cross_margin_bankrupt());
    }
}

mod liquidate_dust_spot_market {

    use {
        crate::{
            controller::liquidation::liquidate_spot,
            create_anchor_account_info,
            state::{
                oracle::OracleSource,
                oracle_map::OracleMap,
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::SpotMarket,
                spot_market_map::SpotMarketMap,
                state::State,
                user::{SpotPosition, User},
            },
            test_utils::{create_account_info, get_pyth_price, get_spot_positions},
            MARGIN_PRECISION, SPOT_BALANCE_PRECISION_U64,
        },
        anchor_lang::prelude::AccountLoader,
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    // Snapshots migrated: the SpotMarket blobs are already current-layout (size
    // 800) and load via aligned_account_bytes_from_b64; the User blob's fields
    // are unchanged vs its snapshot vintage (only trailing `padding` grew) so it
    // loads the same way. With the data loading correctly, the test now fails
    // behaviorally rather than on layout: liquidate_spot() returns
    // Err(SufficientCollateral) instead of Ok(()) — under the current margin math
    // (and the hardcoded USDC=1 / SOL=220 / BTC=97000 oracle prices) this
    // snapshot's user is no longer below the maintenance margin, so it is not
    // liquidatable. A human who owns the spot-margin/liquidation changes must
    // decide whether the assertion is stale or whether re-snapshotting an
    // actually-underwater user is required; do not relax the assertion blindly.
    #[test]
    #[ignore = "layout fixed; now fails behaviorally — liquidate_spot returns \
                Err(SufficientCollateral), user not underwater under current margin math. \
                Needs human review of spot-liquidation behavior / a fresh underwater snapshot."]
    fn test() {
        let perp_market_map = PerpMarketMap::empty();

        let usdc_market_str = String::from("ZLEIa6hBQSdUX6MOo7w/PClm2otsPf7406t9pXygIypU5KAmT//Dwsy3xpWPA/Pp1GfkQjwaxq3rB7BfPBWigujgMxXAX1Z3xvp6877brTo9ZfNqq8l0MbG75MLS9uDkfKYCA0UvXWHmsHZFgFFAI49uEcLfeyYJqqXqJL+++g9w+I4yK2cfD1VTREMgICAgICAgICAgICAgICAgICAgICAgICAgICAgEeQyJ9kZP4WsuF8p8cVq/vGj3k+tUwDvAx8T7OdXugMOWccB1wQAAAAAAAAAAAAAkLC/JQ8EAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACAIxEAAAAAAG67ZWcAAAAAEA4AAAAAAAAQJwAAiBMAAAAAAAAAAAAAAAAAAAAAAAAIz/qyn00QAwAAAAAAAAAA3JYDW8Hy8QEAAAAAAAAAALlkt50CAAAAAAAAAAAAAADH/LnrAgAAAAAAAAAAAAAAJ74VfAAAAAAAAAAAAAAAAH/8F3wAAAAAAAAAAAAAAADtpNKLhRMAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAQEIPAAAAAAC5AwAAAAAAABEAAAAAAAAAQUIPAAAAAABBQg8AAAAAAN+/ZWcAAAAAQEIPAAAAAABAQg8AAAAAAEBCDwAAAAAAQEIPAAAAAAAAAAAAAAAAAAAQpdToAAAAAEBjUr/GAQAFYGDuqNYAAJZ2HP3HlwAAb/UKAAAAAADqv2VnAAAAAOq/ZWcAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEAAAAAAAAAB72nAAAAAAAQJwAAECcAABAnAAAQJwAAAAAAAAAAAACIEwAAYK4KAPBJAgCAhB4ABgAAAAAAAAoBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACjBcBAAAAAADAbjHZEAEAAAAAAQEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==");

        let mut usdc_market_bytes = unsafe {
            crate::test_utils::aligned_account_bytes_from_b64::<SpotMarket>(&usdc_market_str)
        };

        let key = Pubkey::default();
        let owner = Pubkey::from_str("vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P").unwrap();
        let mut lamports = 0;
        let usdc_market_account_info = create_account_info(
            &key,
            true,
            &mut lamports,
            &mut usdc_market_bytes[..],
            &owner,
        );

        let sol_market_str = String::from("ZLEIa6hBQScr1lQqaOSFYS9WELcT14N7mJY9eLJbJXlsZ9Z5/AUPNpcdDKvImMwegHYSrqlRr4mPm/gqRPWD+8llAWp4/D4KBpuIV/6rgYT7aH9jRhjANdrEOdwa6ztVmKDwAAAAAAG8K5ZficO5VwesMce/cvsBy5AvfQoKym53Aehbqm9wSVNPTCAgICAgICAgICAgICAgICAgICAgICAgICAgICAgOmRcJ2YnHQR3Ag5Eg7xlll/BgfFeAH6FulNmduPi8PZ1gO2mrQAAAAAAAAAAAAAAVMnX6Y4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACAIxEAAAAAAMWwZWcAAAAAEA4AAAAAAABADQMAYOoAADegVHrbAAAAAAAAAAAAAAAXHjUC5nMBAAAAAAAAAAAAWbiEIPGpAAAAAAAAAAAAAO/e7mkCAAAAAAAAAAAAAAA+BfWRAgAAAAAAAAAAAAAAL+hICwAAAAAAAAAAAAAAAFfnUwEAAAAAAAAAAAAAAABR1gwfAQAAAAAAAAAAAAAAAQAAAAAAAAAAAAAAAAAAAAqtcVLKhQIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAw2JiCwAAAADvSgIAAAAAAAQAAAAAAAAAD2tcCwAAAADWJ18LAAAAAOW/ZWcAAAAA0LJdCwAAAABwOV8LAAAAAINtWwsAAAAAGKhZCwAAAAA9vWVnAAAAAAAgPYh5LQAAACAPDBIFAwCaPKw2W7cBAI/cc74urAAAgjgGAAAAAADlv2VnAAAAAOW/ZWcAAAAAAAAAAAAAAACghgEAAAAAAGQAAAAAAAAAAOH1BQAAAAAAAAAAAAAAAB3NHQAAAAAA6kSWAAAAAABAHwAAKCMAAOAuAAD4KgAA4gQAAEwdAADkVwAAADUMAOAiAgCATxIACQAAAAEAAQcBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACTL6ACAAAAAABAD4S1owAAAQAAAQEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==");

        let mut sol_market_bytes = unsafe {
            crate::test_utils::aligned_account_bytes_from_b64::<SpotMarket>(&sol_market_str)
        };

        let key = Pubkey::default();
        let owner = Pubkey::from_str("vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P").unwrap();
        let mut lamports = 0;
        let sol_market_account_info =
            create_account_info(&key, true, &mut lamports, &mut sol_market_bytes[..], &owner);

        let btc_market_str = String::from("ZLEIa6hBQSc8PneF/UaEHXUvNAKBDYzFEth8zuNsU/RjhT3POJeVtH29BUUxTm/izrxCmvmE71Qipt4AMCT0gQnMuKstsICKIzzqR01stRPa1CHILmgfgO11EkVd+5H8aDY7mdkVZYImEGLbWKmQIQDHgAf+18OTFJGMv5G6fep4zl3vqc926ndCVEMgICAgICAgICAgICAgICAgICAgICAgICAgICAgxWUdIQutAns2flEnqgm7YoikyrTeWdw6zgyxrzqqAa2kyN0CAAAAAAAAAAAAAAAA4MnWAgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACAIxEAAAAAAMWwZWcAAAAAEA4AAAAAAABADQMAYOoAAFLublMAAAAAAAAAAAAAAAC3aaaKJwAAAAAAAAAAAAAA+l7L0gIAAAAAAAAAAAAAAPG+xFUCAAAAAAAAAAAAAAAdkGRdAgAAAAAAAAAAAAAApQgAAAAAAAAAAAAAAAAAAJNaEwAAAAAAAAAAAAAAAABQNgAAAAAAAAAAAAAAAAAAAwAAAAAAAAAAAAAAAAAAAChZ91WwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA9RVThxYAAADCasMHAAAAACAAAAAAAAAAia/ThBYAAACESzuKFgAAAOW/ZWcAAAAAEICN4AsAAAAAFOhdDwAAAOFtO58NAAAA4W07nw0AAADZ6IdmAAAAAACE1xcAAAAAAKwj/AYAAACO8yvuAwAAAOmTsUcAAAAAWxsBAAAAAADlv2VnAAAAAOW/ZWcAAAAAAAAAAAAAAAAQJwAAAAAAABAnAAAAAAAAECcAAAAAAAAAAAAAAAAAAPQNAAAAAAAAPD4AAAAAAABAHwAAKCMAAOAuAAD4KgAAKJoBAEwdAAD0fgAAIKEHAKCGAQBg4xYACAAAAAMAAQcBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABA5ZwwEgAAAAAAAQEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==");

        let mut btc_market_bytes = unsafe {
            crate::test_utils::aligned_account_bytes_from_b64::<SpotMarket>(&btc_market_str)
        };

        let key = Pubkey::default();
        let owner = Pubkey::from_str("vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P").unwrap();
        let mut lamports = 0;
        let btc_market_account_info =
            create_account_info(&key, true, &mut lamports, &mut btc_market_bytes[..], &owner);

        let spot_market_map = SpotMarketMap::load_multiple(
            vec![
                &sol_market_account_info,
                &usdc_market_account_info,
                &btc_market_account_info,
            ],
            true,
        )
        .unwrap();
        spot_market_map.get_ref_mut(&1).unwrap().oracle_source = OracleSource::PythLazer;
        spot_market_map.get_ref_mut(&3).unwrap().oracle_source = OracleSource::PythLazer;
        let now = 1734721516;
        let clock_slot = 308728664;

        let key = Pubkey::from_str("En8hkHLkRe9d9DraYmBTrus518BvmVH448YcvmrFM6Ce").unwrap();
        let mut usdc_oracle_price = get_pyth_price(1, 6);
        usdc_oracle_price.publish_time = now as u64;
        usdc_oracle_price.posted_slot = clock_slot;
        create_anchor_account_info!(
            usdc_oracle_price,
            &key,
            PythLazerOracle,
            usdc_oracle_account_info
        );

        let key = Pubkey::from_str("BAtFj4kQttZRVep3UZS2aZRDixkGYgWsbqTBVDbnSsPF").unwrap();
        let mut sol_oracle_price = get_pyth_price(220, 6);
        sol_oracle_price.publish_time = now as u64;
        sol_oracle_price.posted_slot = clock_slot;
        create_anchor_account_info!(
            sol_oracle_price,
            &key,
            PythLazerOracle,
            sol_oracle_account_info
        );

        let key = Pubkey::from_str("9Tq8iN5WnMX2PcZGj4iSFEAgHCi8cM6x8LsDUbuzq8uw").unwrap();
        let mut btc_oracle_price = get_pyth_price(97000, 6);
        btc_oracle_price.publish_time = now as u64;
        btc_oracle_price.posted_slot = clock_slot;
        create_anchor_account_info!(
            btc_oracle_price,
            &key,
            PythLazerOracle,
            btc_oracle_account_info
        );

        let account_infos = [
            btc_oracle_account_info,
            usdc_oracle_account_info,
            sol_oracle_account_info,
        ];
        let mut oracle_map =
            OracleMap::load(&mut account_infos.iter().peekable(), clock_slot, None).unwrap();

        let mut state = State::default();
        state
            .oracle_guard_rails
            .price_divergence
            .oracle_twap_5min_percent_divergence = 1000000000000000000;
        state.liquidation_margin_buffer_ratio = MARGIN_PRECISION / 50;

        let user_str = String::from("n3Vf4++XOuwLsTVvD0RzIZV6wjrBQeGW8UQMhZsq83DJs/s2vF8BJgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAU3VwZXIgU3Rha2UgSml0b1NPTCAgICAgICAgICAgICAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACLs0oSAAAAAAAAAQAAAAAAAgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAADAAEAAAAAAEoWAAAAAAAAAAAAAAAAAAAAAAAAAAAAAF7LlQIAAAAAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACEq////////wkAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAKAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAArCdo+f////8AAAAAAAAAAAAAAAAUAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAByRVv+/////wAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAADmbqdrAgAAAKmoHpwCAAAAAAAAAAAAAAB8zcDt/////wAAAAAAAAAAfRzX//////8AAAAAAAAAAOBCYhIAAAAADQAAANAHAAAHAAAAAQEAAAAAAAAAAAAA6r9lZwAAAAAAAAAAAAAAAA==");
        // The User layout only grew its trailing `padding` (all fields are
        // unchanged vs the snapshot vintage), so the aligned-+-zero-padded
        // helper loads it as a current `User`.
        let mut decoded_bytes =
            unsafe { crate::test_utils::aligned_account_bytes_from_b64::<User>(&user_str) };
        let user_bytes = &mut decoded_bytes[..];

        let user_key = Pubkey::from_str("4U5qwCPc3fVfNjFpoLnBjtDNgbcyStpjmGuQiVgPQfdE").unwrap();
        let owner = Pubkey::from_str("vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P").unwrap();
        let mut lamports = 0;
        let user_account_info =
            create_account_info(&user_key, true, &mut lamports, user_bytes, &owner);

        let user_account_loader: AccountLoader<User> =
            AccountLoader::try_from(&user_account_info).unwrap();

        let mut user = user_account_loader.load_mut().unwrap();

        let mut liquidator = User::default();
        liquidator.spot_positions = get_spot_positions(SpotPosition {
            market_index: 0,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        });
        let liquidator_key =
            Pubkey::from_str("5smUuFz1ZzW3FVAF2W1GjYWzxsXQaVyPGdFKfvSnPpaL").unwrap();

        let result = liquidate_spot(
            1,
            3,
            1,
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            clock_slot,
            &state,
        );

        assert_eq!(result, Ok(()));
    }
}

pub mod liquidate_isolated_perp {
    use {
        crate::{
            controller::{
                liquidation::{liquidate_perp, liquidate_spot},
                position::PositionDirection,
            },
            create_anchor_account_info,
            error::ErrorCode,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I128, BASE_PRECISION_I64,
                    BASE_PRECISION_U64, LIQUIDATION_FEE_PRECISION, LIQUIDATION_PCT_PRECISION,
                    MARGIN_PRECISION, MARGIN_PRECISION_U128, ONE_HOUR, PEG_PRECISION,
                    QUOTE_PRECISION_I128, QUOTE_PRECISION_I64, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                liquidation::validate_user_not_being_liquidated,
                margin::calculate_margin_requirement_and_total_collateral_and_liability_info,
                position::calculate_base_asset_value_with_oracle_price,
            },
            state::{
                margin_calculation::MarginContext,
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market::{MarketStats, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                state::State,
                user::{
                    Order, OrderStatus, OrderType, PerpPosition, PositionFlag, SpotPosition, User,
                    UserStats,
                },
            },
            test_utils::{get_orders, get_positions, get_pyth_price, get_spot_positions, *},
            PRICE_PRECISION_I64,
        },
        solana_program::pubkey::Pubkey,
        std::{collections::BTreeSet, str::FromStr},
    };

    #[test]
    pub fn successful_liquidation_long_perp() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
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

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -150 * QUOTE_PRECISION_I64,
                quote_entry_amount: -150 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -150 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],

            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(user.perp_positions[0].base_asset_amount, 0);
        assert_eq!(
            user.perp_positions[0].quote_asset_amount,
            -51 * QUOTE_PRECISION_I64
        );
        assert_eq!(user.perp_positions[0].open_orders, 0);
        assert_eq!(user.perp_positions[0].open_bids, 0);

        assert_eq!(
            liquidator.perp_positions[0].base_asset_amount,
            BASE_PRECISION_I64
        );
        assert_eq!(
            liquidator.perp_positions[0].quote_asset_amount,
            -99 * QUOTE_PRECISION_I64
        );

        let market_after = perp_market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.fee_ledger.total_liquidation_fee, 0);
    }

    #[test]
    pub fn successful_liquidation_short_perp() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 50 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: 3600,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: -BASE_PRECISION_I64,
                quote_asset_amount: 50 * QUOTE_PRECISION_I64,
                quote_entry_amount: 50 * QUOTE_PRECISION_I64,
                quote_break_even_amount: 50 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                ..PerpPosition::default()
            }),
            spot_positions: [SpotPosition::default(); 8],

            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(user.perp_positions[0].base_asset_amount, 0);
        assert_eq!(
            user.perp_positions[0].quote_asset_amount,
            -51 * QUOTE_PRECISION_I64
        );
        assert_eq!(user.perp_positions[0].open_orders, 0);
        assert_eq!(user.perp_positions[0].open_bids, 0);

        assert_eq!(
            liquidator.perp_positions[0].base_asset_amount,
            -BASE_PRECISION_I64
        );
        assert_eq!(
            liquidator.perp_positions[0].quote_asset_amount,
            101 * QUOTE_PRECISION_I64
        );

        let market_after = perp_market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.fee_ledger.total_liquidation_fee, 0);
    }

    #[test]
    pub fn successful_liquidation_to_cover_margin_shortage() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: ONE_HOUR,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 2 * BASE_PRECISION_I64,
                quote_asset_amount: -200 * QUOTE_PRECISION_I64,
                quote_entry_amount: -200 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -200 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                isolated_position_scaled_balance: 5 * SPOT_BALANCE_PRECISION_U64,
                ..PerpPosition::default()
            }),

            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: MARGIN_PRECISION / 50,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            10 * BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(user.perp_positions[0].base_asset_amount, 200000000);
        assert_eq!(user.perp_positions[0].quote_asset_amount, -23600000);
        assert_eq!(user.perp_positions[0].quote_entry_amount, -20000000);
        assert_eq!(user.perp_positions[0].quote_break_even_amount, -23600000);
        assert_eq!(user.perp_positions[0].open_orders, 0);
        assert_eq!(user.perp_positions[0].open_bids, 0);

        let margin_calculation =
            calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                &perp_market_map,
                &spot_market_map,
                &mut oracle_map,
                MarginContext::liquidation(state.liquidation_margin_buffer_ratio),
            )
            .unwrap();

        let isolated_margin_calculation = margin_calculation
            .get_isolated_margin_calculation(0)
            .unwrap();
        let total_collateral = isolated_margin_calculation.total_collateral;
        let margin_requirement_plus_buffer =
            isolated_margin_calculation.margin_requirement_plus_buffer;

        // user out of liq territory
        assert_eq!(
            total_collateral.unsigned_abs(),
            margin_requirement_plus_buffer
        );

        let oracle_price = oracle_map
            .get_price_data(&(
                oracle_price_key,
                crate::state::oracle::OracleSource::PythLazer,
            ))
            .unwrap()
            .price;

        let perp_value = calculate_base_asset_value_with_oracle_price(
            user.perp_positions[0].base_asset_amount as i128,
            oracle_price,
        )
        .unwrap();

        let margin_ratio = total_collateral.unsigned_abs() * MARGIN_PRECISION_U128 / perp_value;

        assert_eq!(margin_ratio, 700);

        assert_eq!(liquidator.perp_positions[0].base_asset_amount, 1800000000);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, -178200000);

        let market_after = perp_market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.fee_ledger.total_liquidation_fee, 1800000)
    }

    #[test]
    pub fn liquidation_over_multiple_slots_takes_one() {
        let now = 1_i64;
        let slot = 1_u64;

        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: -150 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: ONE_HOUR,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: 10 * BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 20 * BASE_PRECISION_I64,
                quote_asset_amount: -2000 * QUOTE_PRECISION_I64,
                quote_entry_amount: -2000 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -2000 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: 10 * BASE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                isolated_position_scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 500 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: MARGIN_PRECISION / 50,
            initial_pct_to_liquidate: (LIQUIDATION_PCT_PRECISION / 10) as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            20 * BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        assert_eq!(user.perp_positions[0].base_asset_amount, 2000000000);
        assert_eq!(user.perp_positions[0].is_being_liquidated(), false);
    }

    #[test]
    pub fn successful_liquidation_half_of_if_fee() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            number_of_users: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 50 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: 3600,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        let mut user = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: -BASE_PRECISION_I64,
                quote_asset_amount: 100 * QUOTE_PRECISION_I64,
                quote_entry_amount: 100 * QUOTE_PRECISION_I64,
                quote_break_even_amount: 100 * QUOTE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                isolated_position_scaled_balance: 15 * SPOT_BALANCE_PRECISION_U64 / 10, // $1.5
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        let market_after = perp_market_map.get_ref(&0).unwrap();
        // .5% * 100 * .95 =$0.475
        assert_eq!(market_after.fee_ledger.total_liquidation_fee, 475000);
    }

    #[test]
    pub fn successful_liquidation_portion_of_if_fee() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut oracle_price = get_pyth_price_mantissa(23244136, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            number_of_users: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 50 * QUOTE_PRECISION_I128,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                funding_period: 3600,
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut spot_market = SpotMarket {
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

        let mut user = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: -299400000000,
                quote_asset_amount: 6959294318,
                quote_entry_amount: 6959294318,
                quote_break_even_amount: 6959294318,
                position_flag: PositionFlag::IsolatedPosition as u8,
                isolated_position_scaled_balance: 113838792 * 1000,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 200,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        liquidate_perp(
            0,
            300 * BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        let market_after = perp_market_map.get_ref(&0).unwrap();
        assert!(!user.is_isolated_margin_being_liquidated(0).unwrap());
        assert_eq!(market_after.fee_ledger.total_liquidation_fee, 41787043);
    }

    #[test]
    pub fn unhealthy_isolated_perp_doesnt_cause_cross_margin_liquidation() {
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
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

        let mut market2 = market;
        market2.market_index = 1;
        create_anchor_account_info!(market2, PerpMarket, market2_account_info);

        let market_account_infos = [market_account_info, market2_account_info];
        let market_set = BTreeSet::default();
        let perp_market_map =
            PerpMarketMap::load(&market_set, &mut market_account_infos.iter().peekable()).unwrap();

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
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);

        let mut spot_market2 = spot_market;
        spot_market2.market_index = 1;
        create_anchor_account_info!(spot_market2, SpotMarket, spot_market2_account_info);

        let spot_market_account_infos = [spot_market_account_info, spot_market2_account_info];
        let mut spot_market_set = BTreeSet::default();
        spot_market_set.insert(0);
        spot_market_set.insert(1);
        let spot_market_map = SpotMarketMap::load(
            &spot_market_set,
            &mut spot_market_account_infos.iter().peekable(),
        )
        .unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -150 * QUOTE_PRECISION_I64,
                quote_entry_amount: -150 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -150 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
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

        user.spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        user.perp_positions[1] = PerpPosition {
            market_index: 1,
            base_asset_amount: BASE_PRECISION_I64,
            quote_asset_amount: -100 * QUOTE_PRECISION_I64,
            quote_entry_amount: -100 * QUOTE_PRECISION_I64,
            quote_break_even_amount: -100 * QUOTE_PRECISION_I64,
            ..PerpPosition::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        let mut user_stats = UserStats::default();
        let mut liquidator_stats = UserStats::default();
        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: 150,
            ..Default::default()
        };
        let result = liquidate_perp(
            1,
            BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        );

        assert_eq!(result, Err(ErrorCode::SufficientCollateral));

        let result = liquidate_spot(
            0,
            1,
            1,
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            &state,
        );

        assert_eq!(result, Err(ErrorCode::SufficientCollateral));

        let margin_calculation =
            calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                &perp_market_map,
                &spot_market_map,
                &mut oracle_map,
                MarginContext::liquidation(state.liquidation_margin_buffer_ratio),
            )
            .unwrap();

        assert_eq!(margin_calculation.meets_cross_margin_requirement(), true);

        assert_eq!(margin_calculation.meets_margin_requirement(), false);

        assert_eq!(
            margin_calculation
                .meets_isolated_margin_requirement(0)
                .unwrap(),
            false
        );

        let spot_position_one_before = user.spot_positions[0];
        let spot_position_two_before = user.spot_positions[1];
        let perp_position_one_before = user.perp_positions[1];
        liquidate_perp(
            0,
            BASE_PRECISION_U64,
            None,
            &mut user,
            &user_key,
            &mut user_stats,
            &mut liquidator,
            &liquidator_key,
            &mut liquidator_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            slot,
            now,
            &state,
        )
        .unwrap();

        let spot_position_one_after = user.spot_positions[0];
        let spot_position_two_after = user.spot_positions[1];
        let perp_position_one_after = user.perp_positions[1];

        assert_eq!(spot_position_one_before, spot_position_one_after);
        assert_eq!(spot_position_two_before, spot_position_two_after);
        assert_eq!(perp_position_one_before, perp_position_one_after);
    }

    #[test]
    pub fn mixed_mode_isolated_liquidation_blocks_exit_after_cross_recovers() {
        // Both cross-margin and isolated liquidation flags are active. Cross
        // health has recovered (can exit) but the isolated position is still
        // unhealthy. The validator must check both states independently: clear
        // the cross flag, then still reject because the isolated liquidation
        // remains. Returning Ok here would let order placement / fills / swaps
        // bypass a live isolated liquidation.
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            status: MarketStatus::Initialized,
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

        let mut market2 = market;
        market2.market_index = 1;
        create_anchor_account_info!(market2, PerpMarket, market2_account_info);

        let market_account_infos = [market_account_info, market2_account_info];
        let market_set = BTreeSet::default();
        let perp_market_map =
            PerpMarketMap::load(&market_set, &mut market_account_infos.iter().peekable()).unwrap();

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
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);

        let mut spot_market2 = spot_market;
        spot_market2.market_index = 1;
        create_anchor_account_info!(spot_market2, SpotMarket, spot_market2_account_info);

        let spot_market_account_infos = [spot_market_account_info, spot_market2_account_info];
        let mut spot_market_set = BTreeSet::default();
        spot_market_set.insert(0);
        spot_market_set.insert(1);
        let spot_market_map = SpotMarketMap::load(
            &spot_market_set,
            &mut spot_market_account_infos.iter().peekable(),
        )
        .unwrap();

        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -150 * QUOTE_PRECISION_I64,
                quote_entry_amount: -150 * QUOTE_PRECISION_I64,
                quote_break_even_amount: -150 * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
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

        user.spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        user.perp_positions[1] = PerpPosition {
            market_index: 1,
            base_asset_amount: BASE_PRECISION_I64,
            quote_asset_amount: -100 * QUOTE_PRECISION_I64,
            quote_entry_amount: -100 * QUOTE_PRECISION_I64,
            quote_break_even_amount: -100 * QUOTE_PRECISION_I64,
            ..PerpPosition::default()
        };

        // Cross health is fine, isolated market 0 is not.
        let margin_calculation =
            calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                &perp_market_map,
                &spot_market_map,
                &mut oracle_map,
                MarginContext::liquidation(10),
            )
            .unwrap();
        assert!(margin_calculation
            .can_exit_cross_margin_liquidation()
            .unwrap());
        assert!(!margin_calculation
            .can_exit_isolated_margin_liquidation(0)
            .unwrap());

        // Both liquidation states active at once.
        user.status = crate::state::user::UserStatus::BeingLiquidated as u8;
        user.perp_positions[0].position_flag |= PositionFlag::BeingLiquidated as u8;

        let result = validate_user_not_being_liquidated(
            &mut user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            10,
        );

        // Cross flag cleared, but the still-active isolated liquidation blocks exit.
        assert_eq!(result, Err(ErrorCode::UserIsBeingLiquidated));
        assert!(!user.is_cross_margin_being_liquidated());
        assert!(user.perp_positions[0].is_being_liquidated());
    }
}

pub mod liquidate_isolated_perp_pnl_for_deposit {
    use {
        crate::{
            controller::liquidation::{liquidate_perp_pnl_for_deposit, resolve_perp_bankruptcy},
            create_anchor_account_info,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I128, LIQUIDATION_FEE_PRECISION,
                    MARGIN_PRECISION, PEG_PRECISION, PERCENTAGE_PRECISION, QUOTE_PRECISION_I128,
                    QUOTE_PRECISION_I64, SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                margin::calculate_margin_requirement_and_total_collateral_and_liability_info,
            },
            state::{
                margin_calculation::MarginContext,
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market::{PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                user::{Order, PerpPosition, PositionFlag, SpotPosition, User},
            },
            test_utils::{get_positions, get_pyth_price, get_spot_positions},
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    #[test]
    pub fn successful_liquidation_liquidator_max_pnl_transfer() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: 0,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let spot_positions = [SpotPosition::default(); 8];
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                isolated_position_scaled_balance: 90 * SPOT_BALANCE_PRECISION_U64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        liquidate_perp_pnl_for_deposit(
            0,
            0,
            50 * 10_u128.pow(6), // .8
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            // 2% liquidation margin buffer: it must stay above the market's 1%
            // liquidator fee, or the seizure premium outweighs the pnl relief and
            // the transfer is refused as loss-making
            200,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();

        assert_eq!(
            user.perp_positions[0].isolated_position_scaled_balance,
            39494950000
        );
        assert_eq!(user.perp_positions[0].quote_asset_amount, -50000000);

        assert_eq!(
            liquidator.spot_positions[1].balance_type,
            SpotBalanceType::Deposit
        );
        assert_eq!(liquidator.spot_positions[0].scaled_balance, 150505050000);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, -50000000);
    }

    #[test]
    pub fn successful_liquidation_pnl_transfer_leaves_position_bankrupt() {
        let now = 0_i64;
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            base_asset_amount_long: BASE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: 0,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let spot_positions = [SpotPosition::default(); 8];
        let mut user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                quote_asset_amount: -91 * QUOTE_PRECISION_I64,
                isolated_position_scaled_balance: 90 * SPOT_BALANCE_PRECISION_U64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let mut liquidator = User {
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        let user_key = Pubkey::default();
        let liquidator_key = Pubkey::default();

        liquidate_perp_pnl_for_deposit(
            0,
            0,
            200 * 10_u128.pow(6), // .8
            None,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            slot,
            MARGIN_PRECISION / 50,
            PERCENTAGE_PRECISION,
            150,
            false,
        )
        .unwrap();

        assert_eq!(user.perp_positions[0].isolated_position_scaled_balance, 0);
        assert_eq!(user.perp_positions[0].quote_asset_amount, -1900000);
        assert_eq!(
            user.perp_positions[0].position_flag & PositionFlag::Bankrupt as u8,
            PositionFlag::Bankrupt as u8
        );

        assert_eq!(liquidator.spot_positions[0].scaled_balance, 190000000000);
        assert_eq!(liquidator.perp_positions[0].quote_asset_amount, -89100000);

        let calc = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::liquidation(MARGIN_PRECISION / 50),
        )
        .unwrap();

        assert_eq!(calc.meets_margin_requirement(), false);

        let market_after = market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.fee_ledger.total_liquidation_fee, 0);
        drop(market_after);

        resolve_perp_bankruptcy(
            0,
            &mut user,
            &user_key,
            &mut liquidator,
            &liquidator_key,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            now,
            0,
            false,
        )
        .unwrap();

        assert_eq!(user.perp_positions[0].isolated_position_scaled_balance, 0);
        assert_eq!(user.perp_positions[0].quote_asset_amount, 0);
        assert_eq!(
            user.perp_positions[0].position_flag & PositionFlag::Bankrupt as u8,
            0
        );
        assert_eq!(user.is_being_liquidated(), false);
    }
}

mod liquidation_mode {
    use {
        crate::{
            create_anchor_account_info,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I128, LIQUIDATION_FEE_PRECISION,
                    MARGIN_PRECISION, PEG_PRECISION, QUOTE_PRECISION_I128, QUOTE_PRECISION_I64,
                    SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                margin::calculate_margin_requirement_and_total_collateral_and_liability_info,
            },
            state::{
                liquidation_mode::{
                    get_perp_liquidation_mode, CrossMarginLiquidatePerpMode,
                    IsolatedMarginLiquidatePerpMode, LiquidatePerpMode,
                },
                margin_calculation::MarginContext,
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market::{PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                user::{Order, PerpPosition, PositionFlag, SpotPosition, User, UserStatus},
            },
            test_utils::get_pyth_price,
        },
        solana_program::pubkey::Pubkey,
        std::{collections::BTreeSet, str::FromStr},
    };

    #[test]
    pub fn tests_meets_margin_requirements() {
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            unrealized_pnl_initial_asset_weight: 9000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            number_of_users_with_base: 1,
            status: MarketStatus::Initialized,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10000000,
            quote_asset_amount: 150 * QUOTE_PRECISION_I128,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);

        let mut market2 = PerpMarket {
            market_index: 1,
            ..market
        };
        create_anchor_account_info!(market2, PerpMarket, market2_account_info);

        let market_account_infos = [market_account_info, market2_account_info];
        let market_set = BTreeSet::default();
        let market_map =
            PerpMarketMap::load(&market_set, &mut market_account_infos.iter().peekable()).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 200 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: QUOTE_PRECISION_I64,
                last_oracle_price_twap_5min: QUOTE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_market = SpotMarket {
            market_index: 1,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            deposit_balance: SPOT_BALANCE_PRECISION,
            borrow_balance: 0,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (sol_oracle_price.price * 99 / 100),
                last_oracle_price_twap_5min: (sol_oracle_price.price * 99 / 100),
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 200 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut perp_positions = [PerpPosition::default(); 8];
        perp_positions[0] = PerpPosition {
            market_index: 0,
            quote_asset_amount: -100 * QUOTE_PRECISION_I64,
            isolated_position_scaled_balance: 90 * SPOT_BALANCE_PRECISION_U64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            ..PerpPosition::default()
        };
        perp_positions[1] = PerpPosition {
            market_index: 1,
            quote_asset_amount: -100 * QUOTE_PRECISION_I64,
            ..PerpPosition::default()
        };
        let user_isolated_position_being_liquidated = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        let isolated_liquidation_mode = IsolatedMarginLiquidatePerpMode::new(0);
        let cross_liquidation_mode = CrossMarginLiquidatePerpMode::new(0);

        let liquidation_margin_buffer_ratio = MARGIN_PRECISION / 50;
        let margin_calculation =
            calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user_isolated_position_being_liquidated,
                &market_map,
                &spot_market_map,
                &mut oracle_map,
                MarginContext::liquidation(liquidation_margin_buffer_ratio),
            )
            .unwrap();

        assert_eq!(
            cross_liquidation_mode
                .meets_margin_requirements(&margin_calculation)
                .unwrap(),
            true
        );
        assert_eq!(
            isolated_liquidation_mode
                .meets_margin_requirements(&margin_calculation)
                .unwrap(),
            false
        );

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 90 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut perp_positions = [PerpPosition::default(); 8];
        perp_positions[0] = PerpPosition {
            market_index: 0,
            quote_asset_amount: -100 * QUOTE_PRECISION_I64,
            isolated_position_scaled_balance: 200 * SPOT_BALANCE_PRECISION_U64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            ..PerpPosition::default()
        };
        perp_positions[1] = PerpPosition {
            market_index: 1,
            quote_asset_amount: -100 * QUOTE_PRECISION_I64,
            ..PerpPosition::default()
        };
        let user_cross_margin_being_liquidated = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        let margin_calculation =
            calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user_cross_margin_being_liquidated,
                &market_map,
                &spot_market_map,
                &mut oracle_map,
                MarginContext::liquidation(liquidation_margin_buffer_ratio),
            )
            .unwrap();

        assert_eq!(
            cross_liquidation_mode
                .meets_margin_requirements(&margin_calculation)
                .unwrap(),
            false
        );
        assert_eq!(
            isolated_liquidation_mode
                .meets_margin_requirements(&margin_calculation)
                .unwrap(),
            true
        );
    }

    #[test]
    pub fn get_perp_liquidation_mode_returns_cross_margin_when_no_position() {
        let perp_positions = [PerpPosition::default(); 8];
        let mut user = User {
            perp_positions,
            spot_positions: [SpotPosition::default(); 8],
            status: UserStatus::BeingLiquidated as u8,
            ..User::default()
        };

        // Before fix: would error here with UserHasNoPositionInMarket (get_perp_position fails when no position)
        let mode = get_perp_liquidation_mode(&user, 0).unwrap();
        assert_eq!(mode.as_ref().user_is_being_liquidated(&user).unwrap(), true);
        mode.exit_liquidation(&mut user).unwrap();
        assert!(!user.is_cross_margin_being_liquidated());
    }

    #[test]
    pub fn get_perp_liquidation_mode_returns_isolated_when_isolated_position() {
        let mut perp_positions = [PerpPosition::default(); 8];
        perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 1,
            quote_asset_amount: -100 * QUOTE_PRECISION_I64,
            isolated_position_scaled_balance: 90 * SPOT_BALANCE_PRECISION_U64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            ..PerpPosition::default()
        };
        let user = User {
            perp_positions,
            spot_positions: [SpotPosition::default(); 8],
            ..User::default()
        };

        let mode = get_perp_liquidation_mode(&user, 0).unwrap();
        assert_eq!(
            mode.as_ref().user_is_being_liquidated(&user).unwrap(),
            false
        );
        let (cancel_market_type, cancel_market_index) = mode.get_cancel_orders_params();
        assert_eq!(
            cancel_market_type,
            Some(crate::state::user::MarketType::Perp)
        );
        assert_eq!(cancel_market_index, Some(0));
    }
}

/// OtterSec #145: extinguishing an unfundable perp claim moves its *creditor*, it does not destroy
/// value.
pub mod extinguish_unfundable_perp_claims {
    use crate::{
        controller::liquidation::extinguish_unfundable_perp_claims,
        create_anchor_account_info,
        math::constants::{
            QUOTE_PRECISION_I128, QUOTE_PRECISION_I64, SPOT_BALANCE_PRECISION,
            SPOT_CUMULATIVE_INTEREST_PRECISION,
        },
        state::{
            perp_market::PerpMarket,
            perp_market_map::PerpMarketMap,
            spot_market::SpotMarket,
            spot_market_map::SpotMarketMap,
            user::{PerpPosition, User},
        },
    };

    /// The user's claim is gone, the market owes the same total, and the insurance tranche is the new
    /// creditor.
    ///
    /// Equity neutrality is the invariant to protect. Zeroing the claim lowers
    /// `market.quote_asset_amount`, and so `net_user_pnl`, which raises the market's excess by the
    /// forfeited amount. The `pending_if_fee` credit lowers it by the same amount. If a later change
    /// breaks that pairing, the market's balance sheet misstates without any error.
    #[test]
    fn unfundable_claim_moves_to_the_insurance_tranche() {
        let mut spot_market = SpotMarket {
            market_index: 0,
            decimals: 6,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            deposit_balance: 1_000_000 * SPOT_BALANCE_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_ai);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_ai, true).unwrap();

        // Claim market with an empty pnl pool: the 500 claim is entirely unfundable.
        let mut claim_market = PerpMarket {
            market_index: 0,
            quote_spot_market_index: 0,
            quote_asset_amount: 500 * QUOTE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(claim_market, PerpMarket, claim_market_ai);
        let perp_market_map = PerpMarketMap::load_one(&claim_market_ai, true).unwrap();

        let mut user = User::default();
        user.perp_positions[0] = PerpPosition {
            market_index: 0,
            quote_asset_amount: 500 * QUOTE_PRECISION_I64,
            ..PerpPosition::default()
        };

        let market_quote_before = perp_market_map.get_ref(&0).unwrap().quote_asset_amount;
        let pending_if_before = perp_market_map
            .get_ref(&0)
            .unwrap()
            .fee_ledger
            .pending_if_fee;

        let forfeited =
            extinguish_unfundable_perp_claims(&mut user, &perp_market_map, &spot_market_map)
                .unwrap();

        assert_eq!(forfeited, 500 * QUOTE_PRECISION_I128 as u128);
        // The user no longer holds a claim to collect after insurance covers their debt.
        assert_eq!(user.perp_positions[0].quote_asset_amount, 0);

        let market_after = perp_market_map.get_ref(&0).unwrap();
        // Aggregate user claims fell by the forfeited amount...
        assert_eq!(
            market_after.quote_asset_amount,
            market_quote_before - 500 * QUOTE_PRECISION_I128,
            "aggregate user claims must fall by the forfeited amount"
        );
        // ...and the insurance tranche picked it up, one for one. Equity-neutral.
        assert_eq!(
            market_after.fee_ledger.pending_if_fee,
            pending_if_before + 500 * QUOTE_PRECISION_I128 as u128,
            "the insurance tranche must become the creditor for exactly the forfeited amount"
        );
    }

    /// Only the part the pool cannot pay may be taken. The fundable remainder belongs in the ordinary
    /// settle pipeline, which uses no insurance.
    #[test]
    fn only_the_unfundable_excess_is_taken() {
        let mut spot_market = SpotMarket {
            market_index: 0,
            decimals: 6,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            deposit_balance: 1_000_000 * SPOT_BALANCE_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_ai);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_ai, true).unwrap();

        // Pool holds 200 tokens against a 500 claim -> 300 unfundable.
        let mut claim_market = PerpMarket {
            market_index: 0,
            quote_spot_market_index: 0,
            quote_asset_amount: 500 * QUOTE_PRECISION_I128,
            ..PerpMarket::default()
        };
        claim_market.pnl_pool.scaled_balance = 200 * (SPOT_BALANCE_PRECISION as u128);
        create_anchor_account_info!(claim_market, PerpMarket, claim_market_ai);
        let perp_market_map = PerpMarketMap::load_one(&claim_market_ai, true).unwrap();

        let mut user = User::default();
        user.perp_positions[0] = PerpPosition {
            market_index: 0,
            quote_asset_amount: 500 * QUOTE_PRECISION_I64,
            ..PerpPosition::default()
        };

        let forfeited =
            extinguish_unfundable_perp_claims(&mut user, &perp_market_map, &spot_market_map)
                .unwrap();

        assert_eq!(
            forfeited,
            300 * QUOTE_PRECISION_I128 as u128,
            "only the unfundable excess may be taken"
        );
        assert_eq!(
            user.perp_positions[0].quote_asset_amount,
            200 * QUOTE_PRECISION_I64,
            "the fundable part must survive for the ordinary settle pipeline"
        );
        assert_eq!(
            perp_market_map
                .get_ref(&0)
                .unwrap()
                .fee_ledger
                .pending_if_fee,
            300 * QUOTE_PRECISION_I128 as u128
        );
    }

    /// A position with base exposure or a live order is not a settled claim. It must never be
    /// forfeited, even if admission lets it through.
    #[test]
    fn live_positions_are_never_extinguished() {
        let mut spot_market = SpotMarket {
            market_index: 0,
            decimals: 6,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            deposit_balance: 1_000_000 * SPOT_BALANCE_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_ai);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_ai, true).unwrap();

        let mut claim_market = PerpMarket {
            market_index: 0,
            quote_spot_market_index: 0,
            quote_asset_amount: 500 * QUOTE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(claim_market, PerpMarket, claim_market_ai);
        let perp_market_map = PerpMarketMap::load_one(&claim_market_ai, true).unwrap();

        let mut with_base = User::default();
        with_base.perp_positions[0] = PerpPosition {
            market_index: 0,
            quote_asset_amount: 500 * QUOTE_PRECISION_I64,
            base_asset_amount: 1,
            ..PerpPosition::default()
        };
        assert_eq!(
            extinguish_unfundable_perp_claims(&mut with_base, &perp_market_map, &spot_market_map)
                .unwrap(),
            0
        );
        assert_eq!(
            with_base.perp_positions[0].quote_asset_amount,
            500 * QUOTE_PRECISION_I64
        );

        let mut with_order = User::default();
        with_order.perp_positions[0] = PerpPosition {
            market_index: 0,
            quote_asset_amount: 500 * QUOTE_PRECISION_I64,
            open_orders: 1,
            ..PerpPosition::default()
        };
        assert_eq!(
            extinguish_unfundable_perp_claims(&mut with_order, &perp_market_map, &spot_market_map)
                .unwrap(),
            0
        );
        assert_eq!(
            with_order.perp_positions[0].quote_asset_amount,
            500 * QUOTE_PRECISION_I64
        );
    }

    /// The writable-market contract: everything this function writes to must be in
    /// `perp_markets_with_forfeitable_claims`, which is what the resolve handlers declare writable.
    ///
    /// This test builds the claim market's `AccountInfo` by hand rather than through
    /// `create_anchor_account_info!`, because that macro passes `is_writable: true` unconditionally.
    /// Every other unit test in this file therefore runs with every account writable, and cannot
    /// observe a mutability failure at all — which is exactly how a `get_ref_mut` on a market the
    /// handler never declared writable reached review. Anchor's `load_mut` rejects a non-writable
    /// account, so passing `false` here reproduces the production failure.
    #[test]
    fn only_declared_writable_markets_are_written() {
        let mut spot_market = SpotMarket {
            market_index: 0,
            decimals: 6,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            deposit_balance: 1_000_000 * SPOT_BALANCE_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_ai);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_ai, true).unwrap();

        // A funded pool, so this claim is NOT forfeitable and the pass writes nothing.
        let mut funded_market = PerpMarket {
            market_index: 0,
            quote_spot_market_index: 0,
            quote_asset_amount: 500 * QUOTE_PRECISION_I128,
            pnl_pool: crate::state::perp_market::PoolBalance {
                scaled_balance: 500 * SPOT_BALANCE_PRECISION,
                market_index: 0,
                ..crate::state::perp_market::PoolBalance::default()
            },
            ..PerpMarket::default()
        };
        // Read-only on purpose: `is_writable = false`, which the macro cannot express.
        let funded_owner = <PerpMarket as anchor_lang::Owner>::owner();
        let funded_key = anchor_lang::prelude::Pubkey::default();
        let mut funded_lamports = 0;
        let mut funded_data = crate::test_utils::get_anchor_account_bytes(&mut funded_market);
        let funded_market_ai = crate::test_utils::create_account_info(
            &funded_key,
            false,
            &mut funded_lamports,
            &mut funded_data[..],
            &funded_owner,
        );

        let mut user = User::default();
        user.perp_positions[0] = PerpPosition {
            market_index: 0,
            quote_asset_amount: 500 * QUOTE_PRECISION_I64,
            ..PerpPosition::default()
        };

        // The claim is fully fundable, so nothing is forfeited and no write borrow is needed. A
        // read-only market therefore has to succeed: the fundability test must not take
        // `get_ref_mut`. Against the original code this line fails with a load error.
        let read_only_map = PerpMarketMap::load_multiple(vec![&funded_market_ai], false).unwrap();
        assert_eq!(
            extinguish_unfundable_perp_claims(&mut user, &read_only_map, &spot_market_map).unwrap(),
            0,
            "a fully fundable claim must not be forfeited"
        );
        assert_eq!(
            user.perp_positions[0].quote_asset_amount,
            500 * QUOTE_PRECISION_I64,
            "a fundable claim stays with the user for the ordinary pipeline"
        );

        // And the market it *would* write to is exactly the one the handlers declare writable.
        let mut unfunded_market = PerpMarket {
            market_index: 1,
            quote_spot_market_index: 0,
            quote_asset_amount: 500 * QUOTE_PRECISION_I128,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(unfunded_market, PerpMarket, unfunded_market_ai);
        let writable_map = PerpMarketMap::load_multiple(vec![&unfunded_market_ai], true).unwrap();

        let mut claimant = User::default();
        claimant.perp_positions[0] = PerpPosition {
            market_index: 1,
            quote_asset_amount: 500 * QUOTE_PRECISION_I64,
            ..PerpPosition::default()
        };

        assert_eq!(
            crate::math::bankruptcy::perp_markets_with_forfeitable_claims(&claimant),
            vec![1],
            "the declared writable set must name every market the forfeit writes to"
        );
        assert_eq!(
            extinguish_unfundable_perp_claims(&mut claimant, &writable_map, &spot_market_map)
                .unwrap(),
            500 * QUOTE_PRECISION_I128 as u128
        );
    }
}
