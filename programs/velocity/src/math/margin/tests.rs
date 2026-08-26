#[cfg(test)]
mod test {
    use {
        crate::{
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, PRICE_PRECISION, PRICE_PRECISION_U64, QUOTE_PRECISION,
                    QUOTE_PRECISION_I64, SPOT_IMF_PRECISION,
                },
                margin::{calculate_perp_position_value_and_pnl, MarginRequirementType},
                position::calculate_base_asset_value_and_pnl_with_oracle_price,
            },
            state::{
                oracle::{OraclePriceData, StrictOraclePrice},
                perp_market::{ContractTier, PerpMarket, AMM},
                spot_market::{AssetTier, SpotMarket},
                user::PerpPosition,
            },
            PRICE_PRECISION_I64, QUOTE_PRECISION_U64, SPOT_BALANCE_PRECISION,
            SPOT_CUMULATIVE_INTEREST_PRECISION,
        },
        num_integer::Roots,
    };

    #[test]
    fn asset_tier_checks() {
        // first is as safe or safer
        assert!(ContractTier::A.is_as_safe_as(&ContractTier::A, &AssetTier::default()));
        assert!(ContractTier::A.is_as_safe_as(&ContractTier::A, &AssetTier::Cross));
        assert!(ContractTier::B.is_as_safe_as(&ContractTier::default(), &AssetTier::default()));
        assert!(ContractTier::C.is_as_safe_as(&ContractTier::Speculative, &AssetTier::Unlisted));
        assert!(ContractTier::C.is_as_safe_as(&ContractTier::C, &AssetTier::Cross));
        assert!(ContractTier::Speculative
            .is_as_safe_as(&ContractTier::Speculative, &AssetTier::Unlisted));
        assert!(
            ContractTier::Speculative.is_as_safe_as(&ContractTier::Isolated, &AssetTier::Unlisted)
        );
        assert!(ContractTier::Speculative
            .is_as_safe_as(&ContractTier::default(), &AssetTier::default()));
        assert!(ContractTier::Isolated.is_as_safe_as(&ContractTier::Isolated, &AssetTier::Unlisted));

        // one (or more) of the candidates are safer
        assert!(!ContractTier::A.is_as_safe_as(&ContractTier::A, &AssetTier::Collateral));
        assert!(!ContractTier::A.is_as_safe_as(&ContractTier::B, &AssetTier::Collateral));
        assert!(!ContractTier::B.is_as_safe_as(&ContractTier::A, &AssetTier::Collateral));
        assert!(!ContractTier::B.is_as_safe_as(&ContractTier::A, &AssetTier::default()));
        assert!(!ContractTier::C.is_as_safe_as(&ContractTier::B, &AssetTier::Cross));
        assert!(!ContractTier::C.is_as_safe_as(&ContractTier::B, &AssetTier::Isolated));
        assert!(!ContractTier::C.is_as_safe_as(&ContractTier::A, &AssetTier::default()));
        assert!(!ContractTier::Speculative.is_as_safe_as(&ContractTier::A, &AssetTier::default()));
        assert!(!ContractTier::Speculative.is_as_safe_as(&ContractTier::A, &AssetTier::Collateral));
        assert!(!ContractTier::Speculative.is_as_safe_as(&ContractTier::B, &AssetTier::Collateral));
        assert!(!ContractTier::Speculative.is_as_safe_as(&ContractTier::B, &AssetTier::Cross));
        assert!(!ContractTier::Speculative.is_as_safe_as(&ContractTier::C, &AssetTier::Collateral));

        assert!(!ContractTier::HighlySpeculative
            .is_as_safe_as(&ContractTier::C, &AssetTier::Collateral));
        assert!(!ContractTier::HighlySpeculative
            .is_as_safe_as(&ContractTier::Speculative, &AssetTier::Isolated));

        assert!(!ContractTier::Speculative
            .is_as_safe_as(&ContractTier::Speculative, &AssetTier::Collateral));
        assert!(
            !ContractTier::Speculative.is_as_safe_as(&ContractTier::Speculative, &AssetTier::Cross)
        );
        assert!(!ContractTier::Speculative
            .is_as_safe_as(&ContractTier::Isolated, &AssetTier::Collateral));
        assert!(
            !ContractTier::Speculative.is_as_safe_as(&ContractTier::Isolated, &AssetTier::Cross)
        );
        assert!(
            !ContractTier::Speculative.is_as_safe_as(&ContractTier::Isolated, &AssetTier::Isolated)
        );
        assert!(ContractTier::HighlySpeculative
            .is_as_safe_as(&ContractTier::Isolated, &AssetTier::default()));

        assert!(!ContractTier::Isolated.is_as_safe_as(&ContractTier::A, &AssetTier::default()));
        assert!(!ContractTier::Isolated
            .is_as_safe_as(&ContractTier::HighlySpeculative, &AssetTier::default()));

        assert!(
            !ContractTier::Isolated.is_as_safe_as(&ContractTier::Isolated, &AssetTier::Isolated)
        );
        assert!(
            !ContractTier::Isolated.is_as_safe_as(&ContractTier::default(), &AssetTier::default())
        );
    }

    #[test]
    fn spot_market_asset_weight() {
        let mut spot_market = SpotMarket {
            initial_asset_weight: 9000,
            initial_liability_weight: 11000,
            decimals: 6,
            imf_factor: 0,
            ..SpotMarket::default()
        };

        let size = 1000 * QUOTE_PRECISION;
        let price = QUOTE_PRECISION_I64;
        let asset_weight = spot_market
            .get_asset_weight(size, price, &MarginRequirementType::Initial)
            .unwrap();
        assert_eq!(asset_weight, 9000);

        let lib_weight = spot_market
            .get_liability_weight(size, &MarginRequirementType::Initial)
            .unwrap();
        assert_eq!(lib_weight, 11000);

        spot_market.imf_factor = 10;
        let asset_weight = spot_market
            .get_asset_weight(size, price, &MarginRequirementType::Initial)
            .unwrap();
        assert_eq!(asset_weight, 9000);

        let lib_weight = spot_market
            .get_liability_weight(size, &MarginRequirementType::Initial)
            .unwrap();
        assert_eq!(lib_weight, 11000);

        let same_asset_weight_diff_imf_factor = 8357;
        let asset_weight = spot_market
            .get_asset_weight(size * 1_000_000, price, &MarginRequirementType::Initial)
            .unwrap();
        assert_eq!(asset_weight, same_asset_weight_diff_imf_factor);

        spot_market.imf_factor = 10000;
        let asset_weight = spot_market
            .get_asset_weight(size, price, &MarginRequirementType::Initial)
            .unwrap();
        assert_eq!(asset_weight, same_asset_weight_diff_imf_factor);

        let lib_weight = spot_market
            .get_liability_weight(size, &MarginRequirementType::Initial)
            .unwrap();
        assert_eq!(lib_weight, 11962);

        spot_market.imf_factor = SPOT_IMF_PRECISION / 10;
        let asset_weight = spot_market
            .get_asset_weight(size, price, &MarginRequirementType::Initial)
            .unwrap();
        assert_eq!(asset_weight, 2642);

        let lib_weight = spot_market
            .get_liability_weight(size, &MarginRequirementType::Initial)
            .unwrap();
        assert_eq!(lib_weight, 40422);

        let maint_lib_weight = spot_market
            .get_liability_weight(size, &MarginRequirementType::Maintenance)
            .unwrap();
        assert_eq!(maint_lib_weight, 31622);

        let mut spot_market2 = SpotMarket {
            initial_asset_weight: 1500,
            maintenance_asset_weight: 7500,
            initial_liability_weight: 15000,
            maintenance_liability_weight: 12500,
            decimals: 6,
            imf_factor: 0,
            ..SpotMarket::default()
        };

        let size = 100000 * QUOTE_PRECISION;
        let price = QUOTE_PRECISION_I64 / 2;
        let asset_weight = spot_market2
            .get_asset_weight(size, price, &MarginRequirementType::Initial)
            .unwrap();
        assert_eq!(asset_weight, 1500);
        let asset_weight = spot_market2
            .get_asset_weight(size, price, &MarginRequirementType::Fill)
            .unwrap();
        assert_eq!(asset_weight, 4500);
        let asset_weight = spot_market2
            .get_asset_weight(size, price, &MarginRequirementType::Maintenance)
            .unwrap();
        assert_eq!(asset_weight, 7500);

        spot_market2.imf_factor = SPOT_IMF_PRECISION / 10;

        let asset_weight = spot_market2
            .get_asset_weight(size, price, &MarginRequirementType::Initial)
            .unwrap();
        assert_eq!(asset_weight, 337);
        let asset_weight = spot_market2
            .get_asset_weight(size, price, &MarginRequirementType::Fill)
            .unwrap();
        assert_eq!(asset_weight, 337);
        let asset_weight = spot_market2
            .get_asset_weight(size, price, &MarginRequirementType::Maintenance)
            .unwrap();
        assert_eq!(asset_weight, 337);
    }

    #[test]
    fn spot_market_scale_initial_asset_weight() {
        let mut sol_spot_market = SpotMarket {
            initial_asset_weight: 9000,
            initial_liability_weight: 11000,
            decimals: 9,
            imf_factor: 0,
            scale_initial_asset_weight_start: 500_000 * QUOTE_PRECISION_U64,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            ..SpotMarket::default()
        };

        let oracle_price = 25 * PRICE_PRECISION_I64;

        sol_spot_market.deposit_balance = SPOT_BALANCE_PRECISION;
        let asset_weight = sol_spot_market
            .get_scaled_initial_asset_weight(oracle_price)
            .unwrap();

        assert_eq!(asset_weight, 9000);

        sol_spot_market.deposit_balance = 20000 * SPOT_BALANCE_PRECISION;
        let asset_weight = sol_spot_market
            .get_scaled_initial_asset_weight(oracle_price)
            .unwrap();

        assert_eq!(asset_weight, 9000);

        sol_spot_market.deposit_balance = 40000 * SPOT_BALANCE_PRECISION;
        let asset_weight = sol_spot_market
            .get_scaled_initial_asset_weight(oracle_price)
            .unwrap();

        assert_eq!(asset_weight, 4500);

        sol_spot_market.deposit_balance = 60000 * SPOT_BALANCE_PRECISION;
        let asset_weight = sol_spot_market
            .get_scaled_initial_asset_weight(oracle_price)
            .unwrap();

        assert_eq!(asset_weight, 3000);
    }

    #[test]
    fn calculate_user_equity_value_tests() {
        let mut market = PerpMarket {
            market_index: 0,
            amm: AMM {
                base_asset_reserve: 512295081967,
                quote_asset_reserve: 488 * AMM_RESERVE_PRECISION,
                sqrt_k: 500 * AMM_RESERVE_PRECISION,
                peg_multiplier: 22_100_000_000,
                base_asset_amount_with_amm: -(12295081967_i128),
                max_spread: 1000,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            imf_factor: 1000, // 1_000/1_000_000 = .001
            unrealized_pnl_initial_asset_weight: 10000,
            unrealized_pnl_maintenance_asset_weight: 10000,
            ..PerpMarket::default()
        };

        let current_price = market.amm.reserve_price().unwrap();
        assert_eq!(current_price, 21051929600);

        market.imf_factor = 1000; // 1_000/1_000_000 = .001

        // btc
        let mut oracle_price_data = OraclePriceData {
            price: (22050 * PRICE_PRECISION) as i64,
            confidence: 0,
            delay: 2,
            has_sufficient_number_of_data_points: true,
            sequence_id: None,
        };

        let market_position = PerpPosition {
            market_index: 0,
            base_asset_amount: -(12295081967 / 2_i64),
            quote_asset_amount: 153688524588, // $25,000 entry price
            ..PerpPosition::default()
        };

        let margin_requirement_type = MarginRequirementType::Initial;

        // sqrt of oracle price = 149
        market.unrealized_pnl_imf_factor = market.imf_factor;

        let uaw = market
            .get_unrealized_asset_weight(22699050905, MarginRequirementType::Initial)
            .unwrap();
        assert_eq!(uaw, 9559);

        let strict_oracle_price = StrictOraclePrice::test(QUOTE_PRECISION_I64);
        let (pmr, upnl, _, _) = calculate_perp_position_value_and_pnl(
            &market_position,
            &market,
            &oracle_price_data,
            &strict_oracle_price,
            MarginRequirementType::Initial,
            0,
        )
        .unwrap();

        assert_eq!(upnl, 100000000);

        assert!(pmr > 0);
        assert_eq!(pmr, 13555327867);

        oracle_price_data.price = (21050 * PRICE_PRECISION) as i64; // lower by $1000 (in favor of user)
        oracle_price_data.confidence = PRICE_PRECISION_U64;

        let (_, position_unrealized_pnl) = calculate_base_asset_value_and_pnl_with_oracle_price(
            &market_position,
            oracle_price_data.price,
        )
        .unwrap();

        assert_eq!(position_unrealized_pnl, 24282786896); // $24.282k

        assert_eq!(
            market
                .get_unrealized_asset_weight(position_unrealized_pnl, margin_requirement_type)
                .unwrap(),
            9516
        );
        assert_eq!(
            market
                .get_unrealized_asset_weight(position_unrealized_pnl * 10, margin_requirement_type)
                .unwrap(),
            7368
        );
        assert_eq!(
            market
                .get_unrealized_asset_weight(position_unrealized_pnl * 100, margin_requirement_type)
                .unwrap(),
            4299
        );
        assert_eq!(
            market
                .get_unrealized_asset_weight(
                    position_unrealized_pnl * 1000,
                    margin_requirement_type
                )
                .unwrap(),
            1855
        );
        assert_eq!(
            market
                .get_unrealized_asset_weight(
                    position_unrealized_pnl * 10000,
                    margin_requirement_type
                )
                .unwrap(),
            663
        );
        //nice that 18000 < 60000

        assert_eq!(
            market
                .get_unrealized_asset_weight(
                    position_unrealized_pnl * 800000,
                    margin_requirement_type
                )
                .unwrap(),
            78
        );
        assert_eq!(position_unrealized_pnl * 800000, 19426229516800000); // 1.9 billion

        let strict_oracle_price = StrictOraclePrice::test(QUOTE_PRECISION_I64);
        let (pmr_2, upnl_2, _, _) = calculate_perp_position_value_and_pnl(
            &market_position,
            &market,
            &oracle_price_data,
            &strict_oracle_price,
            MarginRequirementType::Initial,
            0,
        )
        .unwrap();

        let uaw_2 = market
            .get_unrealized_asset_weight(upnl_2, MarginRequirementType::Initial)
            .unwrap();
        assert_eq!(uaw_2, 10000);

        assert_eq!(upnl_2, 100000000);
        assert!(upnl_2 == upnl);
        assert!(pmr_2 > 0);
        assert_eq!(pmr_2, 12940573769); //$12940.5737702000
        assert!(pmr > pmr_2);
        assert_eq!(pmr - pmr_2, 614754098);
        //-6.1475409835 * 1000 / 10 = 614.75
    }

    #[test]
    fn test_nroot() {
        let ans = (0).nth_root(2);
        assert_eq!(ans, 0);
    }
}

#[cfg(test)]
mod calculate_margin_requirement_and_total_collateral {
    use {
        crate::{
            create_anchor_account_info,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I64, LIQUIDATION_FEE_PRECISION,
                    MARGIN_PRECISION, PEG_PRECISION, SPOT_BALANCE_PRECISION,
                    SPOT_BALANCE_PRECISION_U64, SPOT_CUMULATIVE_INTEREST_PRECISION,
                    SPOT_WEIGHT_PRECISION,
                },
                margin::{
                    calculate_margin_requirement_and_total_collateral_and_liability_info,
                    MarginRequirementType,
                },
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
            test_utils::{get_positions, get_pyth_price},
            PRICE_PRECISION_I64,
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    #[test]
    pub fn usdc_deposit_and_5x_sol_bid() {
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            balance_type: SpotBalanceType::Deposit,
            open_orders: 1,
            open_bids: 500 * 10_i64.pow(9),
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let MarginCalculation {
            margin_requirement,
            total_collateral,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )
        .unwrap();

        assert_eq!(margin_requirement, 50000010000);
        assert_eq!(total_collateral, 50000000000);
    }

    #[test]
    pub fn usdc_deposit_and_5x_sol_ask() {
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            balance_type: SpotBalanceType::Deposit,
            open_orders: 1,
            open_asks: -500 * 10_i64.pow(9),
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let MarginCalculation {
            margin_requirement,
            total_collateral,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )
        .unwrap();

        assert_eq!(margin_requirement, 60000010000);
        assert_eq!(total_collateral, 60000000000);
    }

    #[test]
    pub fn sol_deposit_and_5x_sol_ask() {
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 500 * SPOT_BALANCE_PRECISION_U64,
            open_orders: 1,
            open_asks: -3000 * 10_i64.pow(9),
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let MarginCalculation {
            margin_requirement,
            total_collateral,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )
        .unwrap();

        assert_eq!(margin_requirement, 300000010000);
        assert_eq!(total_collateral, 300000000000);
    }

    #[test]
    pub fn user_custom_margin_ratio() {
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
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 100 * BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            max_margin_ratio: 2 * MARGIN_PRECISION, // .5x leverage
            ..User::default()
        };

        let MarginCalculation {
            margin_requirement, ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )
        .unwrap();

        assert_eq!(margin_requirement, 50000000000); // 100 * $100 * 3 + 100 * $100 * 2

        let user = User {
            max_margin_ratio: MARGIN_PRECISION, // 1x leverage
            ..user
        };

        let MarginCalculation {
            margin_requirement, ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )
        .unwrap();

        assert_eq!(margin_requirement, 30000000000); // 100 * 100 * 1 + 100 * $100 * 2

        let user = User {
            max_margin_ratio: MARGIN_PRECISION / 2, // 2x leverage
            ..user
        };

        let MarginCalculation {
            margin_requirement, ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )
        .unwrap();

        assert_eq!(margin_requirement, 20000000000); // 100 * 100 * .5 + 100 * $100 * 1.5

        let user = User {
            max_margin_ratio: 10 * MARGIN_PRECISION, // .1x leverage
            ..user
        };

        let MarginCalculation {
            margin_requirement: maintenance_margin_requirement,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Maintenance),
        )
        .unwrap();

        // doesnt affect maintenance margin requirement
        assert_eq!(maintenance_margin_requirement, 11500000000); // 100 * 100 * .05 + 100 * $100 * 1.1

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            spot_positions,
            max_margin_ratio: MARGIN_PRECISION / 2, // 2x leverage
            ..User::default()
        };

        let MarginCalculation {
            total_collateral, ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )
        .unwrap();

        assert_eq!(total_collateral, 5000000000); // 100 * $100 * .5
    }

    #[test]
    pub fn margin_ratio_override() {
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
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 100 * BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let MarginCalculation {
            margin_requirement, ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial)
                .margin_ratio_override(2 * MARGIN_PRECISION),
        )
        .unwrap();

        assert_eq!(margin_requirement, 50000000000); // 100 * $100 * 3 + 100 * $100 * 2

        let MarginCalculation {
            margin_requirement, ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial)
                .margin_ratio_override(MARGIN_PRECISION),
        )
        .unwrap();

        assert_eq!(margin_requirement, 30000000000); // 100 * 100 * 1 + 100 * $100 * 2

        let MarginCalculation {
            margin_requirement, ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial)
                .margin_ratio_override(MARGIN_PRECISION / 2),
        )
        .unwrap();

        assert_eq!(margin_requirement, 20000000000); // 100 * 100 * .5 + 100 * $100 * 1.5

        let MarginCalculation {
            margin_requirement: maintenance_margin_requirement,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Maintenance)
                .margin_ratio_override(10 * MARGIN_PRECISION),
        )
        .unwrap();

        // doesnt affect maintenance margin requirement
        assert_eq!(maintenance_margin_requirement, 210000000000);

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            spot_positions,
            ..User::default()
        };

        let MarginCalculation {
            total_collateral, ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial)
                .margin_ratio_override(MARGIN_PRECISION / 2),
        )
        .unwrap();

        assert_eq!(total_collateral, 5000000000); // 100 * $100 * .5
    }

    #[test]
    pub fn user_perp_positions_custom_margin_ratio() {
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
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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

        let user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 100 * BASE_PRECISION_I64,
                max_margin_ratio: 2 * MARGIN_PRECISION as u16, // .5x leverage
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let MarginCalculation {
            margin_requirement, ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )
        .unwrap();

        assert_eq!(margin_requirement, 20000000000);

        let user = User {
            max_margin_ratio: 4 * MARGIN_PRECISION, // 1x leverage
            ..user
        };

        let MarginCalculation {
            margin_requirement, ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )
        .unwrap();

        // user custom margin ratio should override perp position custom margin ratio
        assert_eq!(margin_requirement, 40000000000);
    }

    #[test]
    pub fn user_and_position_max_margin_ratio_initial_vs_maintenance() {
        // Four scenarios: user vs perp_position max_margin_ratio for Initial vs Maintenance.
        // Maintenance always uses market-only (custom = 0). Initial uses max(user, position).
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
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::Pyth,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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

        // Baseline: no custom ratios → maintenance margin = market-only (100 * $100 * 0.05 = 500 in quote → 500000000)
        let user_baseline = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 100 * BASE_PRECISION_I64,
                max_margin_ratio: 0,
                ..PerpPosition::default()
            }),
            spot_positions,
            max_margin_ratio: 0,
            ..User::default()
        };
        let MarginCalculation {
            margin_requirement: maintenance_baseline,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user_baseline,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Maintenance),
        )
        .unwrap();
        assert_eq!(maintenance_baseline, 500000000); // market maintenance only: 10000 * 500 / MARGIN_PRECISION

        // Scenario 1: User max_margin_ratio higher than perp position — Maintenance → market-only (custom = 0)
        let user_high = User {
            max_margin_ratio: 4 * MARGIN_PRECISION,
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 100 * BASE_PRECISION_I64,
                max_margin_ratio: 2 * MARGIN_PRECISION as u16,
                ..PerpPosition::default()
            }),
            ..user_baseline
        };
        let MarginCalculation {
            margin_requirement: maintenance_user_higher,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user_high,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Maintenance),
        )
        .unwrap();
        assert_eq!(
            maintenance_user_higher, maintenance_baseline,
            "Maintenance must use market-only when user ratio is higher than position"
        );

        // Scenario 2: User max_margin_ratio higher than perp position — Initial → use user ratio (4 * MARGIN_PRECISION)
        let MarginCalculation {
            margin_requirement: initial_user_higher,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user_high,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )
        .unwrap();
        assert_eq!(
            initial_user_higher, 40000000000,
            "Initial must use user.max_margin_ratio when user > position"
        );

        // Scenario 3: User max_margin_ratio lower than perp position — Maintenance → market-only
        let user_low = User {
            max_margin_ratio: MARGIN_PRECISION / 2,
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 100 * BASE_PRECISION_I64,
                max_margin_ratio: 4 * MARGIN_PRECISION as u16,
                ..PerpPosition::default()
            }),
            ..user_baseline
        };
        let MarginCalculation {
            margin_requirement: maintenance_user_lower,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user_low,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Maintenance),
        )
        .unwrap();
        assert_eq!(
            maintenance_user_lower, maintenance_baseline,
            "Maintenance must use market-only when position ratio is higher than user"
        );

        // Scenario 4: User max_margin_ratio lower than perp position — Initial → use position ratio (4 * MARGIN_PRECISION)
        let MarginCalculation {
            margin_requirement: initial_user_lower,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user_low,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )
        .unwrap();
        assert_eq!(
            initial_user_lower, 40000000000,
            "Initial must use perp_position.max_margin_ratio when position > user"
        );
    }

    #[test]
    pub fn user_dust_deposit() {
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
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION / 99, // big loss
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 1040,
            ..SpotPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            spot_positions,
            max_margin_ratio: 2 * MARGIN_PRECISION, // .5x leverage
            ..User::default()
        };

        let MarginCalculation {
            total_collateral,
            margin_requirement,
            all_deposit_oracles_valid: deposit_oracles_valid,
            all_liability_oracles_valid: liability_oracles_valid,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )
        .unwrap();

        assert_eq!(
            user.spot_positions[0]
                .get_token_amount(&usdc_spot_market)
                .unwrap(),
            0
        );

        assert_eq!(
            user.spot_positions[1]
                .get_token_amount(&sol_spot_market)
                .unwrap(),
            10
        );

        assert_eq!(deposit_oracles_valid, false);
        assert_eq!(liability_oracles_valid, true);
        assert_eq!(total_collateral, 0); // todo not 0
        assert_eq!(margin_requirement, 0);
    }

    #[test]
    pub fn user_dust_borrow() {
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
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: 1,
            ..SpotPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            spot_positions,
            max_margin_ratio: 2 * MARGIN_PRECISION, // .5x leverage
            ..User::default()
        };

        let MarginCalculation {
            total_collateral,
            margin_requirement,
            all_deposit_oracles_valid: deposit_oracles_valid,
            all_liability_oracles_valid: liability_oracles_valid,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )
        .unwrap();

        assert_eq!(
            user.spot_positions[0]
                .get_token_amount(&usdc_spot_market)
                .unwrap(),
            0
        );

        assert_eq!(
            user.spot_positions[1]
                .get_token_amount(&sol_spot_market)
                .unwrap(),
            1
        );

        assert_eq!(deposit_oracles_valid, true);
        assert_eq!(liability_oracles_valid, false);
        assert_eq!(total_collateral, 0); // todo not 0
        assert_eq!(margin_requirement, 3);

        let mut sol_oracle_price = get_pyth_price(1, 6);
        sol_oracle_price.price /= 10000; // < 1 penny

        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
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
        let MarginCalculation {
            total_collateral,
            margin_requirement,
            all_deposit_oracles_valid: deposit_oracles_valid,
            all_liability_oracles_valid: liability_oracles_valid,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )
        .unwrap();

        assert_eq!(
            user.spot_positions[0]
                .get_token_amount(&usdc_spot_market)
                .unwrap(),
            0
        );

        assert_eq!(
            user.spot_positions[1]
                .get_token_amount(&sol_spot_market)
                .unwrap(),
            1
        );

        assert_eq!(deposit_oracles_valid, true);
        assert_eq!(liability_oracles_valid, false);
        assert_eq!(total_collateral, 0); // todo not 0
        assert_eq!(margin_requirement, 3);
    }

    #[test]
    pub fn strict_maintenance() {
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap_5min: PRICE_PRECISION_I64 * 9 / 10,
                ..HistoricalOracleData::default_quote_oracle()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap_5min: 110 * PRICE_PRECISION_I64,
                ..HistoricalOracleData::default_price(100 * PRICE_PRECISION_I64)
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let MarginCalculation {
            margin_requirement,
            total_collateral,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Maintenance).strict(true),
        )
        .unwrap();

        assert_eq!(total_collateral, 9000000000);
        assert_eq!(margin_requirement, 12100000000);
    }

    #[test]
    pub fn invalid_oracle_for_deposit() {
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
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            spot_positions,
            max_margin_ratio: 2 * MARGIN_PRECISION, // .5x leverage
            ..User::default()
        };

        let MarginCalculation {
            total_collateral,
            margin_requirement,
            all_deposit_oracles_valid: deposit_oracles_valid,
            all_liability_oracles_valid: liability_oracles_valid,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial)
                .ignore_invalid_deposit_oracles(true),
        )
        .unwrap();

        assert_eq!(deposit_oracles_valid, false);
        assert_eq!(liability_oracles_valid, true);
        assert_eq!(total_collateral, 0);
        assert_ne!(margin_requirement, 0);
    }

    #[test]
    pub fn invalid_oracle_for_bid() {
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
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            open_orders: 1,
            open_bids: 100 * BASE_PRECISION_I64,
            ..SpotPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            spot_positions,
            max_margin_ratio: 2 * MARGIN_PRECISION, // .5x leverage
            ..User::default()
        };

        let MarginCalculation {
            total_collateral,
            margin_requirement,
            all_deposit_oracles_valid: deposit_oracles_valid,
            all_liability_oracles_valid: liability_oracles_valid,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial)
                .ignore_invalid_deposit_oracles(true),
        )
        .unwrap();

        assert_eq!(deposit_oracles_valid, false);
        assert_eq!(liability_oracles_valid, true);
        assert_eq!(total_collateral, 0);
        assert_ne!(margin_requirement, 0);
    }

    #[test]
    pub fn invalid_oracle_for_ask() {
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
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            open_orders: 1,
            open_asks: -100 * BASE_PRECISION_I64,
            ..SpotPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            spot_positions,
            max_margin_ratio: 2 * MARGIN_PRECISION, // .5x leverage
            ..User::default()
        };

        let MarginCalculation {
            total_collateral,
            margin_requirement,
            all_deposit_oracles_valid: deposit_oracles_valid,
            all_liability_oracles_valid: liability_oracles_valid,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial)
                .ignore_invalid_deposit_oracles(true),
        )
        .unwrap();

        assert_eq!(deposit_oracles_valid, true);
        assert_eq!(liability_oracles_valid, false);
        assert_eq!(total_collateral, 0);
        assert_ne!(margin_requirement, 0);
    }
}

#[cfg(test)]
mod calculate_margin_requirement_and_total_collateral_and_liability_info {
    use {
        crate::{
            controller::position::PositionDirection,
            create_anchor_account_info,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, LIQUIDATION_FEE_PRECISION, MARGIN_PRECISION,
                    PEG_PRECISION, QUOTE_PRECISION, SPOT_BALANCE_PRECISION,
                    SPOT_BALANCE_PRECISION_U64, SPOT_CUMULATIVE_INTEREST_PRECISION,
                    SPOT_WEIGHT_PRECISION,
                },
                margin::{
                    calculate_margin_requirement_and_total_collateral_and_liability_info,
                    MarginRequirementType,
                },
            },
            state::{
                margin_calculation::{MarginCalculation, MarginContext},
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market::{ContractTier, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                user::{Order, OrderType, PerpPosition, SpotPosition, User},
            },
            test_utils::{get_positions, get_pyth_price, get_pyth_price_mantissa, *},
            BASE_PRECISION_I64, PRICE_PRECISION_I64, QUOTE_PRECISION_I128, QUOTE_PRECISION_I64,
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    #[test]
    fn no_perp_position_but_trigger_order() {
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
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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

        let user = User {
            orders: get_orders(Order {
                order_type: OrderType::TriggerMarket,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 0,
                open_orders: 1,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )
        .unwrap();

        assert_eq!(calculation.margin_requirement, QUOTE_PRECISION / 100);
        assert_eq!(calculation.get_num_of_liabilities().unwrap(), 1);
        assert_eq!(calculation.with_perp_isolated_liability, false);
        assert_eq!(calculation.with_spot_isolated_liability, false);
    }

    #[test]
    fn isolated_contract_tier_count_and_check() {
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
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            contract_tier: ContractTier::Isolated,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        // just usdc and resting limit order
        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        let user = User {
            orders: get_orders(Order {
                order_type: OrderType::Limit,
                base_asset_amount: 1,
                base_asset_amount_filled: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 0,
                open_orders: 1,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let calculation: MarginCalculation =
            calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                &perp_market_map,
                &spot_market_map,
                &mut oracle_map,
                MarginContext::standard(MarginRequirementType::Initial),
            )
            .unwrap();

        assert_eq!(calculation.margin_requirement, QUOTE_PRECISION / 100);
        assert_eq!(calculation.get_num_of_liabilities().unwrap(), 1);
        assert_eq!(calculation.with_spot_isolated_liability, false);
        assert_eq!(calculation.with_perp_isolated_liability, true);

        // just usdc, long iso perp, resting limit order to close
        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        let user = User {
            orders: get_orders(Order {
                order_type: OrderType::Limit,
                base_asset_amount: market.order_step_size,
                base_asset_amount_filled: 0,
                direction: PositionDirection::Short,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: market.order_step_size as i64,
                open_orders: 1,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let calculation: MarginCalculation =
            calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                &perp_market_map,
                &spot_market_map,
                &mut oracle_map,
                MarginContext::standard(MarginRequirementType::Initial),
            )
            .unwrap();

        assert_eq!(calculation.margin_requirement, 110000);
        assert_eq!(calculation.get_num_of_liabilities().unwrap(), 1);
        assert_eq!(calculation.with_perp_isolated_liability, true);
    }

    #[test]
    fn no_spot_position_but_trigger_order() {
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
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 0,
            open_orders: 1,
            ..SpotPosition::default()
        };

        let user = User {
            orders: get_orders(Order {
                order_type: OrderType::TriggerMarket,
                ..Order::default()
            }),
            spot_positions,
            ..User::default()
        };

        let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )
        .unwrap();

        assert_eq!(calculation.margin_requirement, QUOTE_PRECISION / 100);
        assert_eq!(calculation.get_num_of_liabilities().unwrap(), 1);
    }

    #[test]
    pub fn usdc_less_than_1_with_deposit() {
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            sol_oracle_account_info
        );

        let mut usdc_oracle_price = get_pyth_price_mantissa(99 * 10000, 6); // $.99
        let usdc_oracle_price_key =
            Pubkey::from_str("Gnt27xtC473ZT2Mw5u8wZ68Z3gULkSTb5DuxJy7eJotD").unwrap();
        create_anchor_account_info!(
            usdc_oracle_price,
            &usdc_oracle_price_key,
            PythLazerOracle,
            usdc_oracle_account_info
        );
        let oracle_account_infos = Vec::from([sol_oracle_account_info, usdc_oracle_account_info]);
        let mut oracle_map = OracleMap::load(
            &mut oracle_account_infos.iter().peekable(),
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::PythLazerStableCoin,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            oracle: usdc_oracle_price_key,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: usdc_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let MarginCalculation {
            total_collateral,
            margin_requirement,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial).strict(true),
        )
        .unwrap();

        let usdc_price = oracle_map
            .get_price_data(&(usdc_oracle_price_key, OracleSource::QuoteAsset))
            .unwrap()
            .price;
        println!("usdc_price: {}", usdc_price);

        assert_eq!(margin_requirement, 0);
        assert_eq!(total_collateral, 990000);

        let mut spot_market = spot_market_map.get_ref_mut(&0).unwrap();
        spot_market.historical_oracle_data = HistoricalOracleData {
            last_oracle_price_twap_5min: 95 * PRICE_PRECISION_I64 / 100,
            ..HistoricalOracleData::default()
        };
        drop(spot_market);

        let MarginCalculation {
            total_collateral,
            margin_requirement,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial).strict(true),
        )
        .unwrap();

        assert_eq!(margin_requirement, 0);
        assert_eq!(total_collateral, 950000);

        let mut spot_market = spot_market_map.get_ref_mut(&0).unwrap();
        spot_market.historical_oracle_data = HistoricalOracleData {
            last_oracle_price_twap_5min: 101 * PRICE_PRECISION_I64 / 100,
            ..HistoricalOracleData::default()
        };
        drop(spot_market);

        let MarginCalculation {
            total_collateral,
            margin_requirement,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial).strict(true),
        )
        .unwrap();

        assert_eq!(margin_requirement, 0);
        // twap is 1.01, but oracle is .99, so we use oracle
        assert_eq!(total_collateral, 990000);
    }

    #[test]
    pub fn usdc_more_than_1_with_borrow() {
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            sol_oracle_account_info
        );

        let mut usdc_oracle_price = get_pyth_price_mantissa(101 * 10000, 6); // $1.01
        let usdc_oracle_price_key =
            Pubkey::from_str("Gnt27xtC473ZT2Mw5u8wZ68Z3gULkSTb5DuxJy7eJotD").unwrap();
        create_anchor_account_info!(
            usdc_oracle_price,
            &usdc_oracle_price_key,
            PythLazerOracle,
            usdc_oracle_account_info
        );
        let oracle_account_infos = Vec::from([sol_oracle_account_info, usdc_oracle_account_info]);
        let mut oracle_map = OracleMap::load(
            &mut oracle_account_infos.iter().peekable(),
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::PythLazerStableCoin,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            borrow_balance: 1000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            oracle: usdc_oracle_price_key,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: usdc_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let MarginCalculation {
            total_collateral,
            margin_requirement,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial).strict(true),
        )
        .unwrap();

        assert_eq!(margin_requirement, 1010000);
        assert_eq!(total_collateral, 0);

        let mut spot_market = spot_market_map.get_ref_mut(&0).unwrap();
        spot_market.historical_oracle_data = HistoricalOracleData {
            last_oracle_price_twap_5min: 102 * PRICE_PRECISION_I64 / 100,
            ..HistoricalOracleData::default()
        };
        drop(spot_market);

        let MarginCalculation {
            total_collateral,
            margin_requirement,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial).strict(true),
        )
        .unwrap();

        assert_eq!(margin_requirement, 1020000);
        assert_eq!(total_collateral, 0);

        let mut spot_market = spot_market_map.get_ref_mut(&0).unwrap();
        spot_market.historical_oracle_data = HistoricalOracleData {
            last_oracle_price_twap_5min: 99 * PRICE_PRECISION_I64 / 100,
            ..HistoricalOracleData::default()
        };
        drop(spot_market);

        let MarginCalculation {
            total_collateral,
            margin_requirement,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial).strict(true),
        )
        .unwrap();

        assert_eq!(total_collateral, 0);
        // twap is .99, but oracle is 1.01, so we use oracle
        assert_eq!(margin_requirement, 1010000);
    }

    #[test]
    pub fn usdc_not_1_with_perp_position() {
        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            sol_oracle_account_info
        );

        let usdc_price = 101 * 10000; // $1.01
        let mut usdc_oracle_price = get_pyth_price_mantissa(usdc_price, 6);
        let usdc_oracle_price_key =
            Pubkey::from_str("Gnt27xtC473ZT2Mw5u8wZ68Z3gULkSTb5DuxJy7eJotD").unwrap();
        create_anchor_account_info!(
            usdc_oracle_price,
            &usdc_oracle_price_key,
            PythLazerOracle,
            usdc_oracle_account_info
        );
        let oracle_account_infos = Vec::from([sol_oracle_account_info, usdc_oracle_account_info]);
        let mut oracle_map = OracleMap::load(
            &mut oracle_account_infos.iter().peekable(),
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::PythLazerStableCoin,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_price(usdc_price),
            oracle: usdc_oracle_price_key,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: usdc_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_account_infos = Vec::from([
            &usdc_spot_market_account_info,
            &sol_spot_market_account_info,
        ]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

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
                base_asset_amount: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let MarginCalculation {
            total_collateral,
            margin_requirement,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial).strict(true),
        )
        .unwrap();

        assert_eq!(margin_requirement, 10100000);
        assert_eq!(total_collateral, 10100000);

        let mut spot_market = spot_market_map.get_ref_mut(&0).unwrap();
        spot_market.historical_oracle_data = HistoricalOracleData {
            last_oracle_price_twap_5min: 105 * PRICE_PRECISION_I64 / 100,
            ..HistoricalOracleData::default()
        };
        drop(spot_market);

        let MarginCalculation {
            total_collateral,
            margin_requirement,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial).strict(true),
        )
        .unwrap();

        assert_eq!(margin_requirement, 10500000);
        assert_eq!(total_collateral, 10100000);

        let mut spot_market = spot_market_map.get_ref_mut(&0).unwrap();
        spot_market.historical_oracle_data = HistoricalOracleData {
            last_oracle_price_twap_5min: 95 * PRICE_PRECISION_I64 / 100,
            ..HistoricalOracleData::default()
        };
        drop(spot_market);

        let MarginCalculation {
            total_collateral,
            margin_requirement,
            ..
        } = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial).strict(true),
        )
        .unwrap();

        assert_eq!(margin_requirement, 10100000);
        assert_eq!(total_collateral, 9500000);
    }

    #[test]
    fn negative_perp_pnl_liquidation_buffer() {
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
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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

        let user = User {
            orders: get_orders(Order {
                order_type: OrderType::TriggerMarket,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 0,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::liquidation(MARGIN_PRECISION / 100),
        )
        .unwrap();

        assert_eq!(calculation.total_collateral, 0);

        assert_eq!(
            calculation.get_cross_total_collateral_plus_buffer(),
            -QUOTE_PRECISION_I128
        );
    }
}

#[cfg(test)]
mod calculate_max_withdrawable_amount {
    use {
        crate::{
            create_anchor_account_info,
            math::{
                constants::{
                    LIQUIDATION_FEE_PRECISION, SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                margin::calculate_max_withdrawable_amount,
            },
            state::{
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                user::{Order, PerpPosition, SpotPosition, User},
            },
            test_utils::get_pyth_price,
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    #[test]
    pub fn usdc_withdraw() {
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            balance_type: SpotBalanceType::Deposit,
            open_orders: 1,
            open_bids: 100 * 10_i64.pow(9),
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let amount = calculate_max_withdrawable_amount(
            0,
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
        )
        .unwrap();

        assert_eq!(amount, 10000000000);
    }

    #[test]
    pub fn sol_withdraw() {
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            scaled_balance: 10000 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 200 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let amount = calculate_max_withdrawable_amount(
            1,
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
        )
        .unwrap();

        assert_eq!(amount, 74999999000);
    }

    #[test]
    pub fn sol_dust_withdraw() {
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            scaled_balance: 1,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 8008,
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let amount = calculate_max_withdrawable_amount(
            1,
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
        )
        .unwrap();

        assert_eq!(amount, 7000);
    }
}

#[cfg(test)]
mod validate_spot_margin_trading {
    use {
        crate::{
            create_anchor_account_info,
            error::ErrorCode,
            math::{
                constants::{
                    LIQUIDATION_FEE_PRECISION, SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                margin::validate_spot_margin_trading,
            },
            state::{
                market_status::MarketStatus,
                oracle::OracleSource,
                oracle_map::OracleMap,
                perp_market::{ContractTier, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                user::{Order, PerpPosition, SpotPosition, User},
            },
            test_utils::{get_pyth_price, *},
            AMM_RESERVE_PRECISION, BASE_PRECISION_I64, PEG_PRECISION, QUOTE_PRECISION_I64,
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    #[test]
    pub fn sol_ask_larger_than_deposit() {
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let perp_market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            open_asks: -100 * 10_i64.pow(9),
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let result = validate_spot_margin_trading(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
        );

        assert_eq!(result, Err(ErrorCode::MarginTradingDisabled));
    }

    #[test]
    pub fn sol_ask_smaller_than_deposit() {
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let perp_market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 2 * SPOT_BALANCE_PRECISION_U64,
            open_asks: -(10_i64.pow(9)),
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let result = validate_spot_margin_trading(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
        );

        assert_eq!(result, Ok(()));
    }

    #[test]
    pub fn sol_ask_with_borrow() {
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let perp_market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            scaled_balance: 2 * SPOT_BALANCE_PRECISION_U64,
            open_asks: -(10_i64.pow(9)),
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let result = validate_spot_margin_trading(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
        );

        assert_eq!(result, Err(ErrorCode::MarginTradingDisabled));
    }

    #[test]
    pub fn sol_bids_value_larger_than_usdc_deposit() {
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let perp_market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 2 * SPOT_BALANCE_PRECISION_U64,
            open_bids: 2 * 10_i64.pow(9),
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let result = validate_spot_margin_trading(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
        );

        assert_eq!(result, Err(ErrorCode::MarginTradingDisabled));
    }

    #[test]
    pub fn sol_bids_value_smaller_than_usdc_deposit() {
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let perp_market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 2 * SPOT_BALANCE_PRECISION_U64,
            open_bids: 2 * 10_i64.pow(9),
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let result = validate_spot_margin_trading(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
        );

        assert_eq!(result, Ok(()));
    }

    #[test]
    pub fn sol_bids_with_usdc_borrow() {
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let perp_market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 2 * SPOT_BALANCE_PRECISION_U64,
            open_bids: 2 * 10_i64.pow(9),
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let result = validate_spot_margin_trading(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
        );

        assert_eq!(result, Err(ErrorCode::MarginTradingDisabled));
    }

    #[test]
    pub fn attempt_enable_margin_trading_with_isolated_perp() {
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
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            contract_tier: ContractTier::Isolated,

            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 2 * SPOT_BALANCE_PRECISION_U64,
            open_bids: 2 * 10_i64.pow(9),
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
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
            spot_positions,
            is_margin_trading_enabled: true,
            ..User::default()
        };

        let result = validate_spot_margin_trading(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
        );

        assert_eq!(result, Err(ErrorCode::IsolatedAssetTierViolation));
    }
}

#[cfg(test)]
mod calculate_user_equity {
    use {
        crate::{
            create_anchor_account_info,
            math::{
                constants::{
                    SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                margin::calculate_user_equity,
            },
            state::{
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market::{MarketStats, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                user::{Order, PerpPosition, SpotPosition, User},
            },
            test_utils::{get_pyth_price, *},
            MarketStatus, AMM_RESERVE_PRECISION, BASE_PRECISION_I64, LIQUIDATION_FEE_PRECISION,
            PEG_PRECISION, PRICE_PRECISION, PRICE_PRECISION_I64, QUOTE_PRECISION_I64,
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    #[test]
    pub fn usdc_deposit_positive_perp_pnl() {
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
                base_spread: 0, // 1 basis point
                ..AMM::default()
            },
            margin_ratio_initial: 2000,
            margin_ratio_maintenance: 1000,
            status: MarketStatus::Initialized,
            order_step_size: 1000,
            order_tick_size: 1,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
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
        market.amm.max_base_asset_reserve = u128::MAX;
        market.amm.min_base_asset_reserve = 0;

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
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

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
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -90 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let (net_usd_value, _) =
            calculate_user_equity(&user, &market_map, &spot_market_map, &mut oracle_map).unwrap();

        assert_eq!(net_usd_value, 20000000);
    }

    #[test]
    pub fn settled_market_dead_oracle_does_not_poison_the_verdict() {
        // A settled market is valued at its expiry price; its oracle does not
        // enter the number, so its verdict must not enter the flag. Settled
        // markets are exactly where feeds die, and every consumer that
        // requires validity (breaker trip, reset, cure transfers) would
        // otherwise be permanently blocked for any account still holding the
        // settled position.
        let slot = 100_000_u64; // far past the posted slot: oracle stale

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
                base_spread: 0,
                ..AMM::default()
            },
            margin_ratio_initial: 2000,
            margin_ratio_maintenance: 1000,
            status: MarketStatus::Settlement,
            expiry_price: 95 * PRICE_PRECISION_I64,
            order_step_size: 1000,
            order_tick_size: 1,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
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
        market.amm.max_base_asset_reserve = u128::MAX;
        market.amm.min_base_asset_reserve = 0;

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
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

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
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -90 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let (net_usd_value, all_oracles_valid) =
            calculate_user_equity(&user, &market_map, &spot_market_map, &mut oracle_map).unwrap();

        // valued at expiry 95: pnl 95 - 90 = 5, plus the 10 deposit
        assert_eq!(net_usd_value, 15_000_000);
        // the dead oracle on the settled market does not poison the verdict
        assert!(all_oracles_valid);
    }

    #[test]
    pub fn usdc_deposit_negative_perp_pnl() {
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
                base_spread: 0, // 1 basis point
                ..AMM::default()
            },
            margin_ratio_initial: 2000,
            margin_ratio_maintenance: 1000,
            status: MarketStatus::Initialized,
            order_step_size: 1000,
            order_tick_size: 1,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
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
        market.amm.max_base_asset_reserve = u128::MAX;
        market.amm.min_base_asset_reserve = 0;

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
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

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
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -105 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let (net_usd_value, _) =
            calculate_user_equity(&user, &market_map, &spot_market_map, &mut oracle_map).unwrap();

        assert_eq!(net_usd_value, 5000000);
    }

    #[test]
    pub fn usdc_deposit_sol_borrow() {
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap_5min: 110 * PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            ..User::default()
        };

        let (net_usd_value, _) =
            calculate_user_equity(&user, &market_map, &spot_market_map, &mut oracle_map).unwrap();

        assert_eq!(net_usd_value, 1000000000);
    }
}

#[cfg(test)]
mod floor_net_equity {
    use {
        crate::{
            create_anchor_account_info,
            math::{
                constants::{
                    QUOTE_PRECISION_U64, SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                margin::{calculate_net_equity_for_floor, calculate_user_equity, FloorNetEquity},
            },
            state::{
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                user::{Order, PerpPosition, SpotPosition, User},
            },
            test_utils::get_pyth_price,
            LIQUIDATION_FEE_PRECISION, PRICE_PRECISION_I64,
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    /// 10,000 USDC deposit plus a 90 SOL spot position at oracle 100. The
    /// oracle goes stale when `slot` runs far past the posted slot.
    fn run_scenario(
        balance_type: SpotBalanceType,
        slot: u64,
        equity_floor: u64,
    ) -> (Option<FloorNetEquity>, User, i128, bool) {
        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
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

        let market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);

        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: 100 * PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: 100 * PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);

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
            balance_type,
            scaled_balance: 90 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            equity_floor,
            ..User::default()
        };

        let net_equity =
            calculate_net_equity_for_floor(&user, &market_map, &spot_market_map, &mut oracle_map)
                .unwrap();
        let (live_equity, live_valid) =
            calculate_user_equity(&user, &market_map, &spot_market_map, &mut oracle_map).unwrap();

        (net_equity, user, live_equity, live_valid)
    }

    #[test]
    fn no_walk_without_a_floor() {
        let (net_equity, _, _, _) = run_scenario(SpotBalanceType::Borrow, 0, 0);
        assert!(net_equity.is_none());
    }

    #[test]
    fn matches_live_equity_and_verdict() {
        // 10,000 USDC - 90 SOL at 100 = 1,000 USDC
        let (net_equity, _, live_equity, live_valid) =
            run_scenario(SpotBalanceType::Borrow, 0, QUOTE_PRECISION_U64);
        let net_equity = net_equity.unwrap();

        assert!(live_valid);
        assert!(net_equity.all_oracles_valid);
        assert_eq!(live_equity, 1_000_000_000);
        assert_eq!(net_equity.value, 1_000_000_000);
    }

    #[test]
    fn valid_oracles_decide_both_predicates_exactly() {
        // equity 1,000 vs floor 100: clears, not below
        let (net_equity, user, _, _) =
            run_scenario(SpotBalanceType::Borrow, 0, 100 * QUOTE_PRECISION_U64);
        let net_equity = net_equity.unwrap();
        assert!(net_equity.clears_buffered_floor(&user));
        assert!(!net_equity.proves_below_floor(&user));
        assert!(net_equity.validate_clears_buffered_floor(&user).is_ok());

        // equity 1,000 vs floor 2,000: below, does not clear
        let (net_equity, user, _, _) =
            run_scenario(SpotBalanceType::Borrow, 0, 2000 * QUOTE_PRECISION_U64);
        let net_equity = net_equity.unwrap();
        assert!(!net_equity.clears_buffered_floor(&user));
        assert!(net_equity.proves_below_floor(&user));
        assert!(net_equity.validate_clears_buffered_floor(&user).is_err());
    }

    #[test]
    fn invalid_oracle_fails_both_predicates_closed() {
        // A stale oracle must neither authorize an action (clears) nor
        // authorize someone against the user (proves below), regardless of
        // which side of the floor the untrusted value lands on.
        for floor in [100 * QUOTE_PRECISION_U64, 2000 * QUOTE_PRECISION_U64] {
            let (net_equity, user, _, live_valid) =
                run_scenario(SpotBalanceType::Borrow, 100_000, floor);
            let net_equity = net_equity.unwrap();

            assert!(!live_valid);
            assert!(!net_equity.all_oracles_valid);
            assert!(!net_equity.clears_buffered_floor(&user));
            assert!(!net_equity.proves_below_floor(&user));
            assert!(net_equity.validate_clears_buffered_floor(&user).is_err());
        }
    }
}

mod trip_net_equity {
    use {
        crate::{
            create_anchor_account_info,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I64, EQUITY_FLOOR_TRIP_DUST_ALLOWANCE,
                    PEG_PRECISION, PRICE_PRECISION, QUOTE_PRECISION_I64, QUOTE_PRECISION_U64,
                    SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                margin::{calculate_user_equity, calculate_user_equity_for_trip, TripNetEquity},
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
                user::{Order, PerpPosition, SpotPosition, User},
            },
            test_utils::{get_positions, get_pyth_price},
            LIQUIDATION_FEE_PRECISION, PRICE_PRECISION_I64,
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    /// 10,000 USDC deposit plus a SOL spot position priced by a Pyth Lazer
    /// oracle at 100 (twap `sol_twap`). `slot` far past the posted slot
    /// makes the SOL oracle invalid; the USDC market has a quote-asset
    /// oracle, which is always valid.
    fn run_spot_scenario(
        balance_type: SpotBalanceType,
        sol_scaled_balance: u64,
        sol_twap: i64,
        slot: u64,
        equity_floor: u64,
    ) -> (TripNetEquity, User, i128, bool) {
        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
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

        let market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);

        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: sol_twap,
                last_oracle_price_twap_5min: sol_twap,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);

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
            balance_type,
            scaled_balance: sol_scaled_balance,
            ..SpotPosition::default()
        };
        let user = User {
            orders: [Order::default(); 32],
            perp_positions: [PerpPosition::default(); 8],
            spot_positions,
            equity_floor,
            ..User::default()
        };

        let trip_equity =
            calculate_user_equity_for_trip(&user, &market_map, &spot_market_map, &mut oracle_map)
                .unwrap();
        let (live_equity, live_valid) =
            calculate_user_equity(&user, &market_map, &spot_market_map, &mut oracle_map).unwrap();

        (trip_equity, user, live_equity, live_valid)
    }

    /// 10 USDC deposit plus a perp position priced by a Pyth Lazer oracle at
    /// 100 (twap `perp_twap`). `slot` far past the posted slot makes the
    /// perp oracle invalid; the quote market oracle is always valid.
    fn run_perp_scenario(
        base_asset_amount: i64,
        quote_asset_amount: i64,
        perp_twap: i64,
        slot: u64,
        equity_floor: u64,
    ) -> (TripNetEquity, User) {
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
                    last_oracle_price_twap: perp_twap,
                    last_oracle_price_twap_5min: perp_twap,
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

        let trip_equity =
            calculate_user_equity_for_trip(&user, &market_map, &spot_market_map, &mut oracle_map)
                .unwrap();

        (trip_equity, user)
    }

    const SOL_TWAP: i64 = 100 * PRICE_PRECISION_I64;

    #[test]
    fn valid_oracles_match_the_trusted_walk() {
        // 10,000 USDC - 90 SOL at 100 = 1,000 USDC, no concession needed
        let (trip_equity, _, live_equity, live_valid) = run_spot_scenario(
            SpotBalanceType::Borrow,
            90 * SPOT_BALANCE_PRECISION_U64,
            SOL_TWAP,
            0,
            QUOTE_PRECISION_U64,
        );

        assert!(live_valid);
        assert!(trip_equity.provable);
        assert_eq!(trip_equity.equity_upper_bound, live_equity);
        assert_eq!(trip_equity.equity_upper_bound, 1_000_000_000);
    }

    #[test]
    fn stale_dust_asset_is_conceded_the_allowance() {
        // 0.5 SOL is worth 50 at its twap, within the allowance, so the
        // asset counts as the full allowance instead of blocking the proof
        let (trip_equity, _, _, live_valid) = run_spot_scenario(
            SpotBalanceType::Deposit,
            SPOT_BALANCE_PRECISION_U64 / 2,
            SOL_TWAP,
            100_000,
            QUOTE_PRECISION_U64,
        );

        assert!(!live_valid);
        assert!(trip_equity.provable);
        assert_eq!(
            trip_equity.equity_upper_bound,
            10_000 * QUOTE_PRECISION_I64 as i128 + EQUITY_FLOOR_TRIP_DUST_ALLOWANCE
        );
    }

    #[test]
    fn stale_dust_liability_is_conceded_zero() {
        // a liability can only lower equity, so its most favorable value
        // adds nothing to the upper bound
        let (trip_equity, _, _, _) = run_spot_scenario(
            SpotBalanceType::Borrow,
            SPOT_BALANCE_PRECISION_U64 / 2,
            SOL_TWAP,
            100_000,
            QUOTE_PRECISION_U64,
        );

        assert!(trip_equity.provable);
        assert_eq!(
            trip_equity.equity_upper_bound,
            10_000 * QUOTE_PRECISION_I64 as i128
        );
    }

    #[test]
    fn stale_material_asset_keeps_the_breach_unprovable() {
        // 90 SOL at twap 100 is far past the allowance: real exposure the
        // trip cannot value, so no floor makes the breach provable
        let (trip_equity, user, _, _) = run_spot_scenario(
            SpotBalanceType::Deposit,
            90 * SPOT_BALANCE_PRECISION_U64,
            SOL_TWAP,
            100_000,
            u64::MAX,
        );

        assert!(!trip_equity.provable);
        assert!(!trip_equity.proves_breach(&user));
    }

    #[test]
    fn stale_material_borrow_is_conceded_zero() {
        // a borrow can only lower equity at any price, so it is conceded
        // zero at any size instead of vetoing the proof: the valid 10,000
        // USDC alone proves the breach of an 11,000 floor. Gating the
        // borrow on the dust test would hand the veto back to whoever can
        // hold a large borrow in a dead-oracle market.
        let (trip_equity, user, _, _) = run_spot_scenario(
            SpotBalanceType::Borrow,
            90 * SPOT_BALANCE_PRECISION_U64,
            SOL_TWAP,
            100_000,
            11_000 * QUOTE_PRECISION_U64,
        );

        assert!(trip_equity.provable);
        assert_eq!(
            trip_equity.equity_upper_bound,
            10_000 * QUOTE_PRECISION_I64 as i128
        );
        assert!(trip_equity.proves_breach(&user));
    }

    #[test]
    fn non_positive_twap_asset_keeps_the_breach_unprovable() {
        // a zero twap sizes every balance at zero, so the dust test cannot
        // bound the asset; conceding the allowance to an unsizeable asset
        // could understate equity and trip a solvent account
        let (trip_equity, user, _, _) = run_spot_scenario(
            SpotBalanceType::Deposit,
            SPOT_BALANCE_PRECISION_U64 / 2,
            0,
            100_000,
            u64::MAX,
        );

        assert!(!trip_equity.provable);
        assert!(!trip_equity.proves_breach(&user));
    }

    #[test]
    fn dust_cannot_veto_a_material_breach() {
        // 10,000 USDC of valid equity against an 11,000 floor is a real
        // breach; the stale dust position is conceded the allowance and the
        // trip still proves it
        let (trip_equity, user, _, _) = run_spot_scenario(
            SpotBalanceType::Deposit,
            SPOT_BALANCE_PRECISION_U64 / 2,
            SOL_TWAP,
            100_000,
            11_000 * QUOTE_PRECISION_U64,
        );

        assert!(trip_equity.proves_breach(&user));
    }

    #[test]
    fn the_concession_covers_a_breach_smaller_than_the_allowance() {
        // valid equity 10,000 against a 10,050 floor: the dust's allowance
        // lifts the upper bound to 10,100, so the breach is not provable at
        // every true dust price and the trip must not fire
        let (trip_equity, user, _, _) = run_spot_scenario(
            SpotBalanceType::Deposit,
            SPOT_BALANCE_PRECISION_U64 / 2,
            SOL_TWAP,
            100_000,
            10_050 * QUOTE_PRECISION_U64,
        );

        assert!(trip_equity.provable);
        assert!(!trip_equity.proves_breach(&user));
    }

    #[test]
    fn stale_long_dust_perp_bounds_the_base_leg_at_the_allowance() {
        // long 0.5 base entered at -45 quote: entry quote counts exactly,
        // the base leg is conceded the allowance. 10 + (-45 + 100) = 65
        let (trip_equity, _) = run_perp_scenario(
            BASE_PRECISION_I64 / 2,
            -45 * QUOTE_PRECISION_I64,
            SOL_TWAP,
            100_000,
            QUOTE_PRECISION_U64,
        );

        assert!(trip_equity.provable);
        assert_eq!(
            trip_equity.equity_upper_bound,
            10 * QUOTE_PRECISION_I64 as i128 - 45 * QUOTE_PRECISION_I64 as i128
                + EQUITY_FLOOR_TRIP_DUST_ALLOWANCE
        );
    }

    #[test]
    fn stale_short_dust_perp_bounds_the_base_leg_at_zero() {
        // short 0.5 base entered at +55 quote: the base leg only subtracts
        // at any positive price, so the bound is entry quote alone.
        // 10 + 55 = 65
        let (trip_equity, _) = run_perp_scenario(
            -BASE_PRECISION_I64 / 2,
            55 * QUOTE_PRECISION_I64,
            SOL_TWAP,
            100_000,
            QUOTE_PRECISION_U64,
        );

        assert!(trip_equity.provable);
        assert_eq!(
            trip_equity.equity_upper_bound,
            10 * QUOTE_PRECISION_I64 as i128 + 55 * QUOTE_PRECISION_I64 as i128
        );
    }

    #[test]
    fn stale_material_short_perp_is_conceded_zero() {
        // short 90 base entered at +9,000 quote, far past the allowance at
        // its twap: the base leg still only subtracts at any positive
        // price, so the size does not matter and the bound is entry quote
        // alone. 10 + 9,000 = 9,010 proves the breach of a 100,000 floor.
        let (trip_equity, user) = run_perp_scenario(
            -90 * BASE_PRECISION_I64,
            9_000 * QUOTE_PRECISION_I64,
            SOL_TWAP,
            100_000,
            100_000 * QUOTE_PRECISION_U64,
        );

        assert!(trip_equity.provable);
        assert_eq!(
            trip_equity.equity_upper_bound,
            10 * QUOTE_PRECISION_I64 as i128 + 9_000 * QUOTE_PRECISION_I64 as i128
        );
        assert!(trip_equity.proves_breach(&user));
    }

    #[test]
    fn zero_base_position_is_priced_exactly_despite_the_stale_oracle() {
        // unsettled quote pnl does not depend on the perp oracle at all, so
        // a stale oracle neither blocks the proof nor shrinks the value to
        // the allowance. Crediting only the allowance here would understate
        // equity and trip an account that is above its floor.
        let (trip_equity, user) = run_perp_scenario(
            0,
            50_000 * QUOTE_PRECISION_I64,
            SOL_TWAP,
            100_000,
            25_000 * QUOTE_PRECISION_U64,
        );

        assert!(trip_equity.provable);
        assert_eq!(
            trip_equity.equity_upper_bound,
            10 * QUOTE_PRECISION_I64 as i128 + 50_000 * QUOTE_PRECISION_I64 as i128
        );
        assert!(!trip_equity.proves_breach(&user));
    }

    #[test]
    fn stale_material_long_perp_keeps_the_breach_unprovable() {
        // 2 base long at twap 100 is a 200 notional, past the allowance
        let (trip_equity, user) = run_perp_scenario(
            2 * BASE_PRECISION_I64,
            -180 * QUOTE_PRECISION_I64,
            SOL_TWAP,
            100_000,
            u64::MAX,
        );

        assert!(!trip_equity.provable);
        assert!(!trip_equity.proves_breach(&user));
    }

    #[test]
    fn non_positive_twap_long_perp_keeps_the_breach_unprovable() {
        // a zero twap sizes any long at zero notional, so the dust test
        // cannot bound it. Conceding the allowance to a 10,000 base long
        // with -1,000,000 stored quote would collapse the upper bound and
        // trip a solvent account.
        let (trip_equity, user) = run_perp_scenario(
            10_000 * BASE_PRECISION_I64,
            -1_000_000 * QUOTE_PRECISION_I64,
            0,
            100_000,
            100_000 * QUOTE_PRECISION_U64,
        );

        assert!(!trip_equity.provable);
        assert!(!trip_equity.proves_breach(&user));
    }

    #[test]
    fn non_positive_twap_short_perp_is_still_conceded_zero() {
        // the short's zero concession does not depend on the twap at all,
        // so a broken twap does not block the proof. 10 + 180 = 190.
        let (trip_equity, _) = run_perp_scenario(
            -2 * BASE_PRECISION_I64,
            180 * QUOTE_PRECISION_I64,
            -1,
            100_000,
            QUOTE_PRECISION_U64,
        );

        assert!(trip_equity.provable);
        assert_eq!(
            trip_equity.equity_upper_bound,
            10 * QUOTE_PRECISION_I64 as i128 + 180 * QUOTE_PRECISION_I64 as i128
        );
    }
}

#[cfg(test)]
mod pools {
    use {
        crate::{
            create_anchor_account_info,
            error::ErrorCode,
            math::{
                constants::{
                    BASE_PRECISION_I64, SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                margin::{
                    calculate_margin_requirement_and_total_collateral_and_liability_info,
                    MarginRequirementType,
                },
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
                user::{PerpPosition, SpotPosition, User},
            },
            test_utils::{get_positions, get_pyth_price},
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    #[test]
    pub fn spot() {
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let market_map = PerpMarketMap::empty();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            pool_id: 1,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 10000 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let user = User {
            spot_positions,
            ..User::default()
        };

        let result = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        );

        assert_eq!(result.unwrap_err(), ErrorCode::InvalidPoolId)
    }

    #[test]
    fn perp_market_invalid_pool_id() {
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let mut market = PerpMarket {
            amm: AMM { ..AMM::default() },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            pool_id: 1,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let spot_market_map = SpotMarketMap::empty();

        let user = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 100 * BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let result = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        );

        assert_eq!(result.unwrap_err(), ErrorCode::InvalidPoolId)
    }
}

#[cfg(test)]
mod isolated_position {
    use {
        crate::{
            create_anchor_account_info,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I64, LIQUIDATION_FEE_PRECISION,
                    PEG_PRECISION, SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                margin::{
                    calculate_margin_requirement_and_total_collateral_and_liability_info,
                    MarginRequirementType,
                },
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
            test_utils::{get_positions, get_pyth_price},
            QUOTE_PRECISION_I64,
        },
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    #[test]
    pub fn isolated_position_margin_requirement() {
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
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle_source: OracleSource::PythLazer,
            oracle: sol_oracle_price_key,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 9,
            initial_asset_weight: 8 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_asset_weight: 9 * SPOT_WEIGHT_PRECISION / 10,
            initial_liability_weight: 12 * SPOT_WEIGHT_PRECISION / 10,
            maintenance_liability_weight: 11 * SPOT_WEIGHT_PRECISION / 10,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 1000,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
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
            scaled_balance: 20000 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        spot_positions[1] = SpotPosition {
            market_index: 1,
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: 100 * BASE_PRECISION_I64,
                quote_asset_amount: -11000 * QUOTE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                isolated_position_scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..PerpPosition::default()
            }),
            spot_positions,
            ..User::default()
        };

        let margin_calculation =
            calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                &perp_market_map,
                &spot_market_map,
                &mut oracle_map,
                MarginContext::standard(MarginRequirementType::Initial),
            )
            .unwrap();

        let cross_margin_margin_requirement = margin_calculation.margin_requirement;
        let cross_total_collateral = margin_calculation.total_collateral;

        let isolated_margin_calculation = margin_calculation
            .get_isolated_margin_calculation(0)
            .unwrap();
        let isolated_margin_requirement = isolated_margin_calculation.margin_requirement;
        let isolated_total_collateral = isolated_margin_calculation.total_collateral;

        assert_eq!(cross_margin_margin_requirement, 12000000000);
        assert_eq!(cross_total_collateral, 20000000000);
        assert_eq!(isolated_margin_requirement, 1000000000);
        assert_eq!(isolated_total_collateral, -900000000);
        assert_eq!(margin_calculation.meets_margin_requirement(), false);
        assert_eq!(margin_calculation.meets_cross_margin_requirement(), true);
        assert_eq!(
            isolated_margin_calculation.meets_margin_requirement(),
            false
        );
        assert_eq!(
            margin_calculation
                .meets_isolated_margin_requirement(0)
                .unwrap(),
            false
        );

        let margin_calculation =
            calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                &perp_market_map,
                &spot_market_map,
                &mut oracle_map,
                MarginContext::standard(MarginRequirementType::Initial).margin_buffer(1000),
            )
            .unwrap();

        let cross_margin_margin_requirement = margin_calculation.margin_requirement_plus_buffer;
        let cross_total_collateral = margin_calculation.get_cross_total_collateral_plus_buffer();

        let isolated_margin_calculation = margin_calculation
            .get_isolated_margin_calculation(0)
            .unwrap();
        let isolated_margin_requirement =
            isolated_margin_calculation.margin_requirement_plus_buffer;
        let isolated_total_collateral =
            isolated_margin_calculation.get_total_collateral_plus_buffer();

        assert_eq!(cross_margin_margin_requirement, 13000000000);
        assert_eq!(cross_total_collateral, 20000000000);
        assert_eq!(isolated_margin_requirement, 2000000000);
        assert_eq!(isolated_total_collateral, -1000000000);
    }
}

#[cfg(test)]
mod meets_place_order_margin_requirement_with_isolated {
    use {
        crate::{
            controller::position::PositionDirection,
            create_anchor_account_info,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I64, BASE_PRECISION_U64, PEG_PRECISION,
                    SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                margin::meets_place_order_margin_requirement,
            },
            state::{
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market::{PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                user::{
                    MarketType, Order, OrderStatus, OrderType, PerpPosition, PositionFlag,
                    SpotPosition, User,
                },
            },
            test_utils::get_pyth_price,
            QUOTE_PRECISION_I64,
        },
        anchor_lang::prelude::Pubkey,
        std::str::FromStr,
    };

    #[test]
    fn cross_order_passes_when_isolated_fails_initial_but_passes_maintenance() {
        // Scenario:
        // - User has a cross account USDC deposit (collateral)
        // - User has an isolated SOL-PERP position that:
        //   - FAILS initial margin check
        //   - PASSES maintenance margin check
        // - User submits a cross account order (risk increasing)
        // - Expected: Order should PASS because isolated only needs maintenance when
        //   the order is for a cross position

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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        // SOL-PERP market with 10% initial margin, 5% maintenance margin
        let mut sol_perp_market = PerpMarket {
            market_index: 0,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,    // 10%
            margin_ratio_maintenance: 500, // 5%
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(sol_perp_market, PerpMarket, sol_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&sol_perp_market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        // User has:
        // - 1000 USDC cross collateral
        // - Isolated SOL-PERP position: 10 SOL long @ $100 = $1000 notional
        //   - With $70 isolated collateral
        //   - Initial margin required: $1000 * 10% = $100 (FAILS - only has $70)
        //   - Maintenance margin required: $1000 * 5% = $50 (PASSES - has $70)
        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64, // 1000 USDC cross collateral
            ..SpotPosition::default()
        };

        let mut perp_positions = [PerpPosition::default(); 8];
        perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 10 * BASE_PRECISION_I64, // 10 SOL long
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64, // Entry at $100
            position_flag: PositionFlag::IsolatedPosition as u8,
            isolated_position_scaled_balance: 70 * SPOT_BALANCE_PRECISION_U64, // $70 isolated collateral
            ..PerpPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        // Test: Place a cross order (risk_increasing = true, isolated_market_index = None)
        // This should use CrossMarginOverride: cross=Initial, isolated=Maintenance
        // The isolated position should pass with maintenance check
        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            true, // risk_increasing
            None, // isolated_market_index = None means this is a cross order
        );

        // Should pass because:
        // - Cross margin: 1000 USDC collateral, no cross positions = passes Initial
        // - Isolated SOL-PERP: $70 collateral >= $50 maintenance margin = passes Maintenance
        assert!(
            result.is_ok(),
            "Cross order should pass when isolated position passes maintenance margin. Error: {:?}",
            result
        );
    }

    #[test]
    fn cross_order_passes_when_cross_passes_initial_no_other_isolated() {
        // Scenario: Cross PI, no isolated positions. Place cross order (risk increasing).
        // Expected: PASS (cross must pass Initial when risk increasing; no isolated to check).
        let slot = 0_u64;
        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            sol_oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(
            &sol_oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let mut sol_perp_market = PerpMarket {
            market_index: 0,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(sol_perp_market, PerpMarket, sol_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&sol_perp_market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let perp_positions = [PerpPosition::default(); 8];

        let user = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            true,
            None,
        );
        assert!(
            result.is_ok(),
            "Cross order should pass when cross passes initial and no isolated. Error: {:?}",
            result
        );
    }

    #[test]
    fn cross_order_fails_when_other_isolated_fails_maintenance() {
        // Scenario: Cross PI, one isolated with collateral < MM ($40 for $50 MM). Place cross order.
        // Expected: FAIL (other isolated must pass Maintenance).
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let mut sol_perp_market = PerpMarket {
            market_index: 0,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(sol_perp_market, PerpMarket, sol_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&sol_perp_market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut perp_positions = [PerpPosition::default(); 8];
        perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 10 * BASE_PRECISION_I64,
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            isolated_position_scaled_balance: 40 * SPOT_BALANCE_PRECISION_U64, // $40 < $50 MM
            ..PerpPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            true,
            None,
        );
        assert!(
            result.is_err(),
            "Cross order should fail when other isolated fails maintenance margin"
        );
    }

    #[test]
    fn cross_order_fails_when_cross_only_passes_maintenance() {
        // Scenario: Cross PM (collateral $70, IM $100, MM $50 for $1000 notional), no isolated.
        // Place cross order (risk increasing). Expected: FAIL (cross must pass Initial).
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let mut sol_perp_market = PerpMarket {
            market_index: 0,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(sol_perp_market, PerpMarket, sol_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&sol_perp_market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 70 * SPOT_BALANCE_PRECISION_U64, // $70: >= MM $50, < IM $100
            ..SpotPosition::default()
        };
        let mut perp_positions = [PerpPosition::default(); 8];
        perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 10 * BASE_PRECISION_I64,
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
            // Cross position (no isolated flag)
            ..PerpPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            true,
            None,
        );
        assert!(
            result.is_err(),
            "Cross order should fail when cross only passes maintenance"
        );
    }

    #[test]
    fn cross_order_fails_when_cross_fails_maintenance() {
        // Scenario: Cross FM (collateral $40 < MM $50 for $1000 notional). Place cross order.
        // Expected: FAIL.
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let mut sol_perp_market = PerpMarket {
            market_index: 0,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(sol_perp_market, PerpMarket, sol_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&sol_perp_market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 40 * SPOT_BALANCE_PRECISION_U64, // $40 < MM $50
            ..SpotPosition::default()
        };
        let mut perp_positions = [PerpPosition::default(); 8];
        perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 10 * BASE_PRECISION_I64,
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
            ..PerpPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            true,
            None,
        );
        assert!(
            result.is_err(),
            "Cross order should fail when cross fails maintenance"
        );
    }

    #[test]
    fn cross_order_not_risk_increasing_passes_when_all_pass_maintenance() {
        // Scenario: Cross PM (or PI), no other isolated. risk_increasing: false -> all Maintenance.
        // Expected: PASS.
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let mut sol_perp_market = PerpMarket {
            market_index: 0,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(sol_perp_market, PerpMarket, sol_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&sol_perp_market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 70 * SPOT_BALANCE_PRECISION_U64, // PM: >= MM $50
            ..SpotPosition::default()
        };
        let mut perp_positions = [PerpPosition::default(); 8];
        perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 10 * BASE_PRECISION_I64,
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
            ..PerpPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            false, // not risk increasing -> Maintenance only
            None,
        );
        assert!(
            result.is_ok(),
            "Cross order not risk increasing should pass when all pass maintenance. Error: {:?}",
            result
        );
    }

    #[test]
    fn cross_order_not_risk_increasing_fails_when_other_isolated_fails_maintenance() {
        // Scenario: Cross PI, other isolated FM. risk_increasing: false. Expected: FAIL.
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let mut sol_perp_market = PerpMarket {
            market_index: 0,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(sol_perp_market, PerpMarket, sol_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&sol_perp_market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut perp_positions = [PerpPosition::default(); 8];
        perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 10 * BASE_PRECISION_I64,
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            isolated_position_scaled_balance: 40 * SPOT_BALANCE_PRECISION_U64, // FM: < $50 MM
            ..PerpPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            false,
            None,
        );
        assert!(
            result.is_err(),
            "Cross order not risk increasing should fail when other isolated fails maintenance"
        );
    }

    #[test]
    fn cross_order_not_risk_increasing_fails_when_cross_fails_maintenance() {
        // Scenario: Cross FM. risk_increasing: false. Expected: FAIL.
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
        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let mut sol_perp_market = PerpMarket {
            market_index: 0,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(sol_perp_market, PerpMarket, sol_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&sol_perp_market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 40 * SPOT_BALANCE_PRECISION_U64, // < MM $50
            ..SpotPosition::default()
        };
        let mut perp_positions = [PerpPosition::default(); 8];
        perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 10 * BASE_PRECISION_I64,
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
            ..PerpPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            false,
            None,
        );
        assert!(
            result.is_err(),
            "Cross order not risk increasing should fail when cross fails maintenance"
        );
    }

    #[test]
    fn isolated_order_passes_when_other_isolated_fails_initial_but_passes_maintenance() {
        // Scenario:
        // - User has a cross account USDC deposit (collateral)
        // - User has an isolated SOL-PERP position that:
        //   - FAILS initial margin check
        //   - PASSES maintenance margin check
        // - User submits an ETH-PERP order on an isolated position which increases risk
        // - Expected: Order should PASS because separate isolated position should only
        //   need maintenance margin requirement

        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            sol_oracle_account_info
        );
        let mut eth_oracle_price = get_pyth_price(1000, 6);
        let eth_oracle_price_key =
            Pubkey::from_str("AHRAk64kPiGwkbkisDvjVYzq6Ho5Q2wQSj28vAaAt7Tq").unwrap();
        create_anchor_account_info!(
            eth_oracle_price,
            &eth_oracle_price_key,
            PythLazerOracle,
            eth_oracle_account_info
        );

        let oracle_account_infos = [sol_oracle_account_info, eth_oracle_account_info];
        let mut oracle_map = OracleMap::load(
            &mut oracle_account_infos.iter().peekable(),
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        // SOL-PERP market with 10% initial margin, 5% maintenance margin
        let mut sol_perp_market = PerpMarket {
            market_index: 0,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,    // 10%
            margin_ratio_maintenance: 500, // 5%
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };

        let mut eth_perp_market = PerpMarket {
            market_index: 2,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,    // 10%
            margin_ratio_maintenance: 500, // 5%
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: eth_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(sol_perp_market, PerpMarket, sol_perp_market_account_info);
        create_anchor_account_info!(eth_perp_market, PerpMarket, eth_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_multiple(
            vec![&sol_perp_market_account_info, &eth_perp_market_account_info],
            true,
        )
        .unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        // User has:
        // - 1000 USDC cross collateral
        // - Isolated SOL-PERP position: 10 SOL long @ $100 = $1000 notional
        //   - With $70 isolated collateral
        //   - Initial margin required: $1000 * 10% = $100 (PASSES - has $70)
        //   - Maintenance margin required: $1000 * 5% = $50 (FAILS - has $50)
        // - Isolated ETH-PERP position: 1 ETH long @ $1000 = $1000 notional
        //   - With $200 isolated collateral
        //   - Initial margin required: $1000 * 10% = $100 (PASSES - has $200)
        //   - Maintenance margin required: $1000 * 5% = $50 (PASSES - has $70)
        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64, // 1000 USDC cross collateral
            ..SpotPosition::default()
        };

        let mut perp_positions = [PerpPosition::default(); 8];
        perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 10 * BASE_PRECISION_I64, // 10 SOL long
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64, // Entry at $100
            position_flag: PositionFlag::IsolatedPosition as u8,
            isolated_position_scaled_balance: 70 * SPOT_BALANCE_PRECISION_U64, // $70 isolated collateral
            ..PerpPosition::default()
        };
        perp_positions[1] = PerpPosition {
            market_index: 2,
            base_asset_amount: BASE_PRECISION_I64, // 1 ETH long
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64, // Entry at $1000
            position_flag: PositionFlag::IsolatedPosition as u8,
            isolated_position_scaled_balance: 200 * SPOT_BALANCE_PRECISION_U64, // $1000 isolated collateral
            ..PerpPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        // Test: Place a cross order (risk_increasing = true, isolated_market_index = None)
        // This should use CrossMarginOverride: cross=Initial, isolated=Maintenance
        // The isolated position should pass with maintenance check
        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            true,    // risk_increasing
            Some(2), // isolated_market_index = 2 means this is an ETH-PERP order
        );

        // Should pass because:
        // - Cross margin: 1000 USDC collateral, no cross positions = passes Initial
        // - Isolated ETH-PERP: $1000 collateral >= $500 maintenance margin = passes Maintenance
        // - Isolated SOL-PERP: $70 collateral >= $50 maintenance margin = passes Maintenance
        assert!(
            result.is_ok(),
            "Isolated ETH-PERP order should pass when other isolated position passes maintenance margin. Error: {:?}",
            result
        );
    }

    #[test]
    fn isolated_order_fails_when_cross_account_fails_initial_margin() {
        // Scenario:
        // - User has a cross account that is FAILING initial margin (but passing maintenance)
        //   because they have a cross perp position that requires more margin than available
        // - User tries to increase an isolated position
        // - Expected: Order should SUCCEED because collateral for isolated positions comes from
        //   the cross account, but we already ran initial check at the transfer ix previous to this IRL
        //   so we only check maintenance at time of placing order
        //
        // This ensures users can't escape cross margin requirements by moving to isolated positions

        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            sol_oracle_account_info
        );
        let mut eth_oracle_price = get_pyth_price(1000, 6);
        let eth_oracle_price_key =
            Pubkey::from_str("AHRAk64kPiGwkbkisDvjVYzq6Ho5Q2wQSj28vAaAt7Tq").unwrap();
        create_anchor_account_info!(
            eth_oracle_price,
            &eth_oracle_price_key,
            PythLazerOracle,
            eth_oracle_account_info
        );

        let oracle_account_infos = [sol_oracle_account_info, eth_oracle_account_info];
        let mut oracle_map = OracleMap::load(
            &mut oracle_account_infos.iter().peekable(),
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        // SOL-PERP market (cross position) with 10% initial margin, 5% maintenance margin
        let mut sol_perp_market = PerpMarket {
            market_index: 0,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,    // 10%
            margin_ratio_maintenance: 500, // 5%
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };

        // ETH-PERP market (isolated position) with 10% initial margin, 5% maintenance margin
        let mut eth_perp_market = PerpMarket {
            market_index: 2,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 1000 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,    // 10%
            margin_ratio_maintenance: 500, // 5%
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: eth_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(sol_perp_market, PerpMarket, sol_perp_market_account_info);
        create_anchor_account_info!(eth_perp_market, PerpMarket, eth_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_multiple(
            vec![&sol_perp_market_account_info, &eth_perp_market_account_info],
            true,
        )
        .unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 100000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        // User has:
        // - 80 USDC cross collateral
        // - Cross SOL-PERP position: 10 SOL long @ $100 = $1000 notional
        //   - Initial margin required: $1000 * 10% = $100 (doesn't matter, we check maintenance - only $80 cross collateral)
        //   - Maintenance margin required: $1000 * 5% = $50 (PASSES - $80 > $50)
        // - Isolated ETH-PERP position: 1 ETH long @ $1000 = $1000 notional
        //   - With $200 isolated collateral
        //   - Initial margin required: $1000 * 10% = $100 (PASSES - has $200)
        //   - Maintenance margin required: $1000 * 5% = $50 (PASSES - has $200)
        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 80 * SPOT_BALANCE_PRECISION_U64, // Only 80 USDC cross collateral
            ..SpotPosition::default()
        };

        let mut perp_positions = [PerpPosition::default(); 8];
        // Cross SOL-PERP position (not isolated)
        perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 10 * BASE_PRECISION_I64, // 10 SOL long
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64, // Entry at $100
            // No isolated flag - this is a cross position
            ..PerpPosition::default()
        };
        // Isolated ETH-PERP position
        perp_positions[1] = PerpPosition {
            market_index: 2,
            base_asset_amount: BASE_PRECISION_I64, // 1 ETH long
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64, // Entry at $1000
            position_flag: PositionFlag::IsolatedPosition as u8,
            isolated_position_scaled_balance: 200 * SPOT_BALANCE_PRECISION_U64, // $200 isolated collateral
            ..PerpPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        // Test: Try to place an isolated ETH-PERP order (risk_increasing = true)
        // This should use IsolatedPositionOverride: ETH-PERP=Initial, cross+others=Maintenance
        // But since cross account is failing initial, we shouldn't allow this
        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            true,    // risk_increasing
            Some(2), // isolated_market_index = 2 means this is an ETH-PERP order
        );

        // Should SUCCEED because:
        // - Cross margin with SOL-PERP: $80 collateral < $100 initial margin required
        // - But it's more than $50, which is the maintenance margin required for the ETH-PERP position
        assert!(
            result.is_ok(),
            "Isolated place order should succeed when cross account fails initial margin but has enough maintenance collateral"
        );
    }

    #[test]
    fn isolated_order_fails_when_isolated_deposit_would_make_cross_fail_initial_margin() {
        // Scenario:
        // - User has a cross account that is PASSING initial margin
        // - User has an isolated position that is currently PASSING initial margin
        // - User places an order to increase the isolated position
        // - The order increases the worst-case position size, increasing IM required
        // - The new IM required exceeds isolated collateral
        // - The deposit required to cover the shortfall would make cross fail IM
        // - Expected: Order should FAIL
        //
        // This ensures users can't increase isolated positions when the required
        // deposit would make their cross account undercollateralized
        //
        // Key numbers:
        // - Cross: $110 collateral, $100 IM required -> PASSES ($110 > $100)
        // - Isolated position: 1 ETH = $1000 notional, $100 IM required
        // - Isolated collateral: $110 -> PASSES current IM ($110 > $100)
        // - Order: Buy 0.5 ETH more (open_bids = 0.5 ETH)
        // - Worst case position: 1.5 ETH = $1500 notional = $150 IM required
        // - Isolated collateral: $110 < $150 IM required -> FAILS
        // - Shortfall: $40
        // - If cross deposits $40 to isolated: cross has $70 vs $100 IM -> FAILS

        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            sol_oracle_account_info
        );
        let mut eth_oracle_price = get_pyth_price(1000, 6);
        let eth_oracle_price_key =
            Pubkey::from_str("AHRAk64kPiGwkbkisDvjVYzq6Ho5Q2wQSj28vAaAt7Tq").unwrap();
        create_anchor_account_info!(
            eth_oracle_price,
            &eth_oracle_price_key,
            PythLazerOracle,
            eth_oracle_account_info
        );

        let oracle_account_infos = [sol_oracle_account_info, eth_oracle_account_info];
        let mut oracle_map = OracleMap::load(
            &mut oracle_account_infos.iter().peekable(),
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        // SOL-PERP market (cross position) with 10% initial margin, 5% maintenance margin
        let mut sol_perp_market = PerpMarket {
            market_index: 0,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,    // 10%
            margin_ratio_maintenance: 500, // 5%
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };

        // ETH-PERP market (isolated position) with 10% initial margin, 5% maintenance margin
        let mut eth_perp_market = PerpMarket {
            market_index: 2,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 1000 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,    // 10%
            margin_ratio_maintenance: 500, // 5%
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: eth_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(sol_perp_market, PerpMarket, sol_perp_market_account_info);
        create_anchor_account_info!(eth_perp_market, PerpMarket, eth_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_multiple(
            vec![&sol_perp_market_account_info, &eth_perp_market_account_info],
            true,
        )
        .unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 100000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        // User has:
        // - 110 USDC cross collateral (PASSES initial margin for cross position)
        // - Cross SOL-PERP position: 10 SOL long @ $100 = $1000 notional
        //   - Initial margin required: $1000 * 10% = $100 (PASSES - $110 > $100)
        // - Isolated ETH-PERP position: 1 ETH long @ $1000 = $1000 notional
        //   - With $110 isolated collateral
        //   - Current IM required: $1000 * 10% = $100 (PASSES - $110 > $100)
        //   - User places order to buy 0.5 ETH more (open_bids = 0.5 ETH)
        //   - Worst case position: 1.5 ETH = $1500 notional
        //   - New IM required: $1500 * 10% = $150 (FAILS - $110 < $150)
        //   - Shortfall: $40
        //   - If cross deposits $40: cross has $70 vs $100 IM -> FAILS
        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 110 * SPOT_BALANCE_PRECISION_U64, // 110 USDC cross collateral (passes IM)
            ..SpotPosition::default()
        };

        let mut perp_positions = [PerpPosition::default(); 8];
        // Cross SOL-PERP position (not isolated)
        perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 10 * BASE_PRECISION_I64, // 10 SOL long
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64, // Entry at $100
            // No isolated flag - this is a cross position
            ..PerpPosition::default()
        };
        // Isolated ETH-PERP position with sufficient collateral for current position,
        // but with an open order that increases the worst-case position size
        perp_positions[1] = PerpPosition {
            market_index: 2,
            base_asset_amount: BASE_PRECISION_I64, // 1 ETH long
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64, // Entry at $1000
            position_flag: PositionFlag::IsolatedPosition as u8,
            isolated_position_scaled_balance: 110 * SPOT_BALANCE_PRECISION_U64, // $110 isolated collateral
            open_orders: 1,                                                     // Has an open order
            open_bids: BASE_PRECISION_I64 / 2, // Order to buy 0.5 ETH more
            ..PerpPosition::default()
        };

        // Create an order for the isolated position
        let mut orders = [Order::default(); 32];
        orders[0] = Order {
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            market_type: MarketType::Perp,
            market_index: 2,
            direction: PositionDirection::Long,
            base_asset_amount: BASE_PRECISION_U64 / 2, // 0.5 ETH
            ..Order::default()
        };

        let user = User {
            orders,
            perp_positions,
            spot_positions,
            ..User::default()
        };

        // Test: Check margin after placing an isolated ETH-PERP order (risk_increasing = true)
        // The order has already been "placed" by setting open_bids on the position
        // This uses IsolatedPositionOverride: ETH-PERP=Initial, cross=Initial
        //
        // Margin calculation:
        // - Worst case position = base_asset_amount + open_bids = 1 + 0.5 = 1.5 ETH
        // - Worst case notional = 1.5 * $1000 = $1500
        // - IM required = $1500 * 10% = $150
        // - Isolated collateral = $110 < $150 -> FAILS isolated IM
        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            true,    // risk_increasing
            Some(2), // isolated_market_index = 2 means this is an ETH-PERP order
        );

        // Should FAIL because:
        // - Worst case isolated ETH-PERP: 1.5 ETH = $1500 notional, $150 IM required
        // - Isolated collateral: $110 < $150 -> FAILS
        // - Cross passes IM ($110 > $100), but can't spare $40 without failing
        // - If cross deposited $40 to isolated: cross would have $70 vs $100 IM -> fails
        //
        // This is different from the previous test where cross was already failing IM.
        // Here, cross is passing IM, but the deposit required to fund the isolated
        // position increase would make cross fail.
        assert!(
            result.is_err(),
            "Isolated order should fail when deposit would make cross fail IM"
        );
    }

    #[test]
    fn isolated_order_passes_when_cross_has_plenty_of_collateral() {
        // Scenario:
        // - User has a cross account with plenty of USDC collateral
        // - User has no existing positions
        // - User opens a new isolated position
        // - Expected: Order should PASS because cross has plenty of collateral
        //
        // Key numbers:
        // - Cross: $1000 USDC collateral, no positions -> $0 IM required
        // - New isolated position: 1 ETH = $1000 notional = $100 IM required
        // - Isolated collateral provided: $150 (from cross)
        // - After transfer: Cross has $850, still $0 IM required -> PASSES

        let slot = 0_u64;

        let mut eth_oracle_price = get_pyth_price(1000, 6);
        let eth_oracle_price_key =
            Pubkey::from_str("AHRAk64kPiGwkbkisDvjVYzq6Ho5Q2wQSj28vAaAt7Tq").unwrap();
        create_anchor_account_info!(
            eth_oracle_price,
            &eth_oracle_price_key,
            PythLazerOracle,
            eth_oracle_account_info
        );

        let oracle_account_infos = [eth_oracle_account_info];
        let mut oracle_map = OracleMap::load(
            &mut oracle_account_infos.iter().peekable(),
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        // ETH-PERP market (isolated position) with 10% initial margin, 5% maintenance margin
        let mut eth_perp_market = PerpMarket {
            market_index: 2,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 1000 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,    // 10%
            margin_ratio_maintenance: 500, // 5%
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: eth_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(eth_perp_market, PerpMarket, eth_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&eth_perp_market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 100000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        // User has:
        // - 1000 USDC cross collateral (plenty of buffer)
        // - New isolated ETH-PERP position: 1 ETH long @ $1000 = $1000 notional
        //   - With $150 isolated collateral
        //   - IM required: $1000 * 10% = $100 (PASSES - $150 > $100)
        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64, // 1000 USDC cross collateral
            ..SpotPosition::default()
        };

        let mut perp_positions = [PerpPosition::default(); 8];
        // New isolated ETH-PERP position with sufficient collateral
        perp_positions[0] = PerpPosition {
            market_index: 2,
            base_asset_amount: BASE_PRECISION_I64, // 1 ETH long
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64, // Entry at $1000
            position_flag: PositionFlag::IsolatedPosition as u8,
            isolated_position_scaled_balance: 150 * SPOT_BALANCE_PRECISION_U64, // $150 isolated collateral
            ..PerpPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            true,
            Some(2),
        );

        // Should PASS because:
        // - Cross has $1000, no positions, $0 IM required -> PASSES
        // - Isolated ETH-PERP: $150 collateral >= $100 IM required -> PASSES
        assert!(
            result.is_ok(),
            "Isolated order should pass when cross has plenty of collateral. Error: {:?}",
            result
        );
    }

    #[test]
    fn isolated_order_fails_when_other_isolated_fails_maintenance_margin() {
        // Scenario:
        // - User has a cross account with USDC collateral
        // - User has an existing isolated SOL-PERP position that FAILS maintenance margin
        // - User tries to open a new isolated ETH-PERP position
        // - Expected: Order should FAIL because existing isolated position fails MM
        //
        // Key numbers:
        // - Cross: $1000 USDC collateral
        // - Existing isolated SOL-PERP: 10 SOL @ $100 = $1000 notional
        //   - MM required: $1000 * 5% = $50
        //   - Isolated collateral: $40 < $50 -> FAILS MM
        // - New isolated ETH-PERP order: would pass on its own
        // - But since existing isolated fails MM, can't open new isolated

        let slot = 0_u64;

        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            sol_oracle_account_info
        );
        let mut eth_oracle_price = get_pyth_price(1000, 6);
        let eth_oracle_price_key =
            Pubkey::from_str("AHRAk64kPiGwkbkisDvjVYzq6Ho5Q2wQSj28vAaAt7Tq").unwrap();
        create_anchor_account_info!(
            eth_oracle_price,
            &eth_oracle_price_key,
            PythLazerOracle,
            eth_oracle_account_info
        );

        let oracle_account_infos = [sol_oracle_account_info, eth_oracle_account_info];
        let mut oracle_map = OracleMap::load(
            &mut oracle_account_infos.iter().peekable(),
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        // SOL-PERP market with 10% initial margin, 5% maintenance margin
        let mut sol_perp_market = PerpMarket {
            market_index: 0,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,    // 10%
            margin_ratio_maintenance: 500, // 5%
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };

        // ETH-PERP market (new isolated position) with 10% initial margin, 5% maintenance margin
        let mut eth_perp_market = PerpMarket {
            market_index: 2,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 1000 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,    // 10%
            margin_ratio_maintenance: 500, // 5%
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: eth_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(sol_perp_market, PerpMarket, sol_perp_market_account_info);
        create_anchor_account_info!(eth_perp_market, PerpMarket, eth_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_multiple(
            vec![&sol_perp_market_account_info, &eth_perp_market_account_info],
            true,
        )
        .unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 100000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        // User has:
        // - 1000 USDC cross collateral
        // - Existing isolated SOL-PERP position: 10 SOL long @ $100 = $1000 notional
        //   - With only $40 isolated collateral
        //   - MM required: $1000 * 5% = $50 (FAILS - $40 < $50)
        // - New isolated ETH-PERP position: 1 ETH long @ $1000 = $1000 notional
        //   - With $200 isolated collateral
        //   - IM required: $1000 * 10% = $100 (PASSES - $200 > $100)
        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64, // 1000 USDC cross collateral
            ..SpotPosition::default()
        };

        let mut perp_positions = [PerpPosition::default(); 8];
        // Existing isolated SOL-PERP position that FAILS maintenance margin
        perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 10 * BASE_PRECISION_I64, // 10 SOL long
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64, // Entry at $100
            position_flag: PositionFlag::IsolatedPosition as u8,
            isolated_position_scaled_balance: 40 * SPOT_BALANCE_PRECISION_U64, // Only $40 - fails MM
            ..PerpPosition::default()
        };
        // New isolated ETH-PERP position with sufficient collateral
        perp_positions[1] = PerpPosition {
            market_index: 2,
            base_asset_amount: BASE_PRECISION_I64, // 1 ETH long
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64, // Entry at $1000
            position_flag: PositionFlag::IsolatedPosition as u8,
            isolated_position_scaled_balance: 200 * SPOT_BALANCE_PRECISION_U64, // $200 isolated collateral
            ..PerpPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        // Test: Try to place an order on the new ETH-PERP isolated position
        // Even though ETH-PERP itself passes IM, the existing SOL-PERP fails MM
        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            true,
            Some(2), // isolated_market_index = 2 (ETH-PERP)
        );

        // Should FAIL because:
        // - Existing isolated SOL-PERP: $40 collateral < $50 MM required -> FAILS MM
        // - When opening new isolated position, all other isolated positions must pass MM
        assert!(
            result.is_err(),
            "Isolated order should fail when other isolated position fails maintenance margin"
        );
    }

    #[test]
    fn isolated_order_passes_when_cross_only_passes_maintenance() {
        // Scenario: Current isolated PI, cross PM (no other isolated). Place isolated order (risk increasing).
        // Cross has no perp position so cross margin req 0; cross $70 is PM-level. Isolated ETH $150 >= IM $100.
        // Expected: PASS.
        let slot = 0_u64;
        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            sol_oracle_account_info
        );
        let mut eth_oracle_price = get_pyth_price(1000, 6);
        let eth_oracle_price_key =
            Pubkey::from_str("AHRAk64kPiGwkbkisDvjVYzq6Ho5Q2wQSj28vAaAt7Tq").unwrap();
        create_anchor_account_info!(
            eth_oracle_price,
            &eth_oracle_price_key,
            PythLazerOracle,
            eth_oracle_account_info
        );
        let oracle_account_infos = [sol_oracle_account_info, eth_oracle_account_info];
        let mut oracle_map = OracleMap::load(
            &mut oracle_account_infos.iter().peekable(),
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let mut sol_perp_market = PerpMarket {
            market_index: 0,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        let mut eth_perp_market = PerpMarket {
            market_index: 2,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 1000 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: eth_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(sol_perp_market, PerpMarket, sol_perp_market_account_info);
        create_anchor_account_info!(eth_perp_market, PerpMarket, eth_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_multiple(
            vec![&sol_perp_market_account_info, &eth_perp_market_account_info],
            true,
        )
        .unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 70 * SPOT_BALANCE_PRECISION_U64, // cross PM (no cross position)
            ..SpotPosition::default()
        };
        let mut perp_positions = [PerpPosition::default(); 8];
        perp_positions[0] = PerpPosition {
            market_index: 2,
            base_asset_amount: BASE_PRECISION_I64,
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            isolated_position_scaled_balance: 150 * SPOT_BALANCE_PRECISION_U64, // PI: >= $100 IM
            ..PerpPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            true,
            Some(2),
        );
        assert!(
            result.is_ok(),
            "Isolated order should pass when cross only passes maintenance. Error: {:?}",
            result
        );
    }

    #[test]
    fn isolated_order_fails_when_current_isolated_only_passes_maintenance() {
        // Scenario: Current isolated PM (collateral $70 < IM $100), cross/other ok. Place isolated order (risk increasing).
        // Expected: FAIL (current isolated must pass Initial).
        let slot = 0_u64;
        let mut eth_oracle_price = get_pyth_price(1000, 6);
        let eth_oracle_price_key =
            Pubkey::from_str("AHRAk64kPiGwkbkisDvjVYzq6Ho5Q2wQSj28vAaAt7Tq").unwrap();
        create_anchor_account_info!(
            eth_oracle_price,
            &eth_oracle_price_key,
            PythLazerOracle,
            eth_oracle_account_info
        );
        let oracle_account_infos = [eth_oracle_account_info];
        let mut oracle_map = OracleMap::load(
            &mut oracle_account_infos.iter().peekable(),
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let mut eth_perp_market = PerpMarket {
            market_index: 2,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 1000 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: eth_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(eth_perp_market, PerpMarket, eth_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&eth_perp_market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut perp_positions = [PerpPosition::default(); 8];
        perp_positions[0] = PerpPosition {
            market_index: 2,
            base_asset_amount: BASE_PRECISION_I64,
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            isolated_position_scaled_balance: 70 * SPOT_BALANCE_PRECISION_U64, // PM: $70 < IM $100
            ..PerpPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            true,
            Some(2),
        );
        assert!(
            result.is_err(),
            "Isolated order should fail when current isolated only passes maintenance"
        );
    }

    #[test]
    fn isolated_order_fails_when_current_isolated_fails_maintenance() {
        // Scenario: Current isolated FM (collateral $40 < MM $50). Place isolated order. Expected: FAIL.
        let slot = 0_u64;
        let mut eth_oracle_price = get_pyth_price(1000, 6);
        let eth_oracle_price_key =
            Pubkey::from_str("AHRAk64kPiGwkbkisDvjVYzq6Ho5Q2wQSj28vAaAt7Tq").unwrap();
        create_anchor_account_info!(
            eth_oracle_price,
            &eth_oracle_price_key,
            PythLazerOracle,
            eth_oracle_account_info
        );
        let oracle_account_infos = [eth_oracle_account_info];
        let mut oracle_map = OracleMap::load(
            &mut oracle_account_infos.iter().peekable(),
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let mut eth_perp_market = PerpMarket {
            market_index: 2,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 1000 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: eth_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(eth_perp_market, PerpMarket, eth_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&eth_perp_market_account_info, true).unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut perp_positions = [PerpPosition::default(); 8];
        perp_positions[0] = PerpPosition {
            market_index: 2,
            base_asset_amount: BASE_PRECISION_I64,
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            isolated_position_scaled_balance: 40 * SPOT_BALANCE_PRECISION_U64, // FM: < $50 MM
            ..PerpPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            true,
            Some(2),
        );
        assert!(
            result.is_err(),
            "Isolated order should fail when current isolated fails maintenance"
        );
    }

    #[test]
    fn isolated_order_not_risk_increasing_passes_when_all_pass_maintenance() {
        // Scenario: Current isolated PM, cross PM. risk_increasing: false -> all Maintenance. Expected: PASS.
        let slot = 0_u64;
        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            sol_oracle_account_info
        );
        let mut eth_oracle_price = get_pyth_price(1000, 6);
        let eth_oracle_price_key =
            Pubkey::from_str("AHRAk64kPiGwkbkisDvjVYzq6Ho5Q2wQSj28vAaAt7Tq").unwrap();
        create_anchor_account_info!(
            eth_oracle_price,
            &eth_oracle_price_key,
            PythLazerOracle,
            eth_oracle_account_info
        );
        let oracle_account_infos = [sol_oracle_account_info, eth_oracle_account_info];
        let mut oracle_map = OracleMap::load(
            &mut oracle_account_infos.iter().peekable(),
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let mut sol_perp_market = PerpMarket {
            market_index: 0,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        let mut eth_perp_market = PerpMarket {
            market_index: 2,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 1000 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: eth_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(sol_perp_market, PerpMarket, sol_perp_market_account_info);
        create_anchor_account_info!(eth_perp_market, PerpMarket, eth_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_multiple(
            vec![&sol_perp_market_account_info, &eth_perp_market_account_info],
            true,
        )
        .unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 70 * SPOT_BALANCE_PRECISION_U64, // cross PM
            ..SpotPosition::default()
        };
        let mut perp_positions = [PerpPosition::default(); 8];
        perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 10 * BASE_PRECISION_I64,
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
            ..PerpPosition::default()
        };
        perp_positions[1] = PerpPosition {
            market_index: 2,
            base_asset_amount: BASE_PRECISION_I64,
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            isolated_position_scaled_balance: 70 * SPOT_BALANCE_PRECISION_U64, // isolated PM
            ..PerpPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            false,
            Some(2),
        );
        assert!(
            result.is_ok(),
            "Isolated order not risk increasing should pass when all pass maintenance. Error: {:?}",
            result
        );
    }

    #[test]
    fn isolated_order_not_risk_increasing_fails_when_other_isolated_fails_maintenance() {
        // Scenario: Current PI, cross PI, other isolated FM. risk_increasing: false. Expected: FAIL.
        let slot = 0_u64;
        let mut sol_oracle_price = get_pyth_price(100, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            sol_oracle_account_info
        );
        let mut eth_oracle_price = get_pyth_price(1000, 6);
        let eth_oracle_price_key =
            Pubkey::from_str("AHRAk64kPiGwkbkisDvjVYzq6Ho5Q2wQSj28vAaAt7Tq").unwrap();
        create_anchor_account_info!(
            eth_oracle_price,
            &eth_oracle_price_key,
            PythLazerOracle,
            eth_oracle_account_info
        );
        let oracle_account_infos = [sol_oracle_account_info, eth_oracle_account_info];
        let mut oracle_map = OracleMap::load(
            &mut oracle_account_infos.iter().peekable(),
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let mut sol_perp_market = PerpMarket {
            market_index: 0,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        let mut eth_perp_market = PerpMarket {
            market_index: 2,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 1000 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: eth_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(sol_perp_market, PerpMarket, sol_perp_market_account_info);
        create_anchor_account_info!(eth_perp_market, PerpMarket, eth_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_multiple(
            vec![&sol_perp_market_account_info, &eth_perp_market_account_info],
            true,
        )
        .unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_account_infos = Vec::from([&usdc_spot_market_account_info]);
        let spot_market_map =
            SpotMarketMap::load_multiple(spot_market_account_infos, true).unwrap();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };
        let mut perp_positions = [PerpPosition::default(); 8];
        perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 10 * BASE_PRECISION_I64,
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            isolated_position_scaled_balance: 40 * SPOT_BALANCE_PRECISION_U64, // other isolated FM
            ..PerpPosition::default()
        };
        perp_positions[1] = PerpPosition {
            market_index: 2,
            base_asset_amount: BASE_PRECISION_I64,
            quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            isolated_position_scaled_balance: 200 * SPOT_BALANCE_PRECISION_U64, // current PI
            ..PerpPosition::default()
        };

        let user = User {
            orders: [Order::default(); 32],
            perp_positions,
            spot_positions,
            ..User::default()
        };

        let result = meets_place_order_margin_requirement(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            false,
            Some(2),
        );
        assert!(
            result.is_err(),
            "Isolated order not risk increasing should fail when other isolated fails maintenance"
        );
    }
}

/// OtterSec #143 / #144 — the spot-only liability flag must not be polluted by
/// the perp oracle, and both spot flags must actually flip on a stale spot
/// oracle so the fill-path gate can read them.
///
/// The perp-fill path already handles a stale *perp* oracle deliberately
/// (`oracle_stale_for_margin` → 100% margin override for the taker, reject
/// unless someone reduces for the maker). Gating the new fill check on the
/// broad `all_liability_oracles_valid` would silently replace that design with a
/// hard reject, which is why `all_spot_liability_oracles_valid` exists.
#[test]
fn spot_liability_oracle_flag_is_independent_of_the_perp_oracle() {
    use crate::{
        math::margin::MarginRequirementType,
        state::margin_calculation::{MarginCalculation, MarginContext},
    };

    // A perp-oracle invalidation clears the broad flag but must leave the
    // spot-only flag alone.
    let mut calc = MarginCalculation::new(MarginContext::standard(MarginRequirementType::Initial));
    assert!(calc.all_liability_oracles_valid);
    assert!(calc.all_spot_liability_oracles_valid);
    assert!(calc.all_deposit_oracles_valid);

    calc.update_all_liability_oracles_valid(false);
    assert!(
        !calc.all_liability_oracles_valid,
        "the broad flag must record the perp oracle"
    );
    assert!(
        calc.all_spot_liability_oracles_valid,
        "a perp-oracle invalidation must NOT clear the spot-only flag — that is \
             what keeps the fill gate from overriding oracle_stale_for_margin"
    );

    // A spot-borrow invalidation clears both, so every pre-existing consumer of
    // the broad flag keeps its current meaning.
    let mut calc2 = MarginCalculation::new(MarginContext::standard(MarginRequirementType::Initial));
    calc2.update_all_spot_liability_oracles_valid(false);
    assert!(!calc2.all_spot_liability_oracles_valid);
    assert!(
        !calc2.all_liability_oracles_valid,
        "a spot-borrow invalidation must still fold into the broad flag"
    );
}

/// OtterSec #135 / #148 — a stale spot-interest index must block valuing that
/// market's **borrow** for margin, while a stale *deposit* index stays allowed.
///
/// Margin values a scaled borrow through the stored `cumulative_borrow_interest`,
/// so un-booked interest understates the debt. `handle_withdraw` cranks only the
/// market being withdrawn and the perp-fill handler cranks none, and the other
/// markets arrive read-only — hence a freshness precondition rather than a
/// refresh (which would need them writable) or an in-margin projection (which
/// would cost CU on every fill).
#[test]
fn stale_spot_interest_blocks_borrow_valuation_but_not_deposits() {
    use crate::{
        create_anchor_account_info,
        math::{
            constants::TWENTY_FOUR_HOUR,
            margin::{
                max_spot_interest_staleness_for_margin,
                validate_spot_borrow_interest_fresh_for_margin,
            },
        },
        state::{
            spot_market::{SpotBalanceType, SpotMarket},
            spot_market_map::SpotMarketMap,
            user::{SpotPosition, User},
        },
        SPOT_BALANCE_PRECISION, SPOT_CUMULATIVE_INTEREST_PRECISION,
    };

    let now = 1_700_000_000_i64;

    // 50% utilization under a 70%/6%/100% curve, so the market charges a real
    // borrow rate and an un-booked interval is measurable.
    let build = |last_interest_ts: u64| SpotMarket {
        market_index: 1,
        deposit_balance: 1_000 * SPOT_BALANCE_PRECISION,
        borrow_balance: 500 * SPOT_BALANCE_PRECISION,
        cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
        cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
        optimal_utilization: 700_000,
        optimal_borrow_rate: 60_000,
        max_borrow_rate: 1_000_000,
        decimals: 6,
        last_interest_ts,
        ..SpotMarket::default()
    };

    let position = |balance_type: SpotBalanceType, scaled_balance: u64| {
        let mut user = User::default();
        user.spot_positions[0] = SpotPosition {
            market_index: 1,
            scaled_balance,
            balance_type,
            ..SpotPosition::default()
        };
        user
    };

    let borrow_of = 100 * (SPOT_BALANCE_PRECISION as u64);

    // The window this market earns from its own rate ceiling. A 100% APR ceiling
    // holds one basis point of the debt for 3,153s, inside the one-hour cap.
    let window = max_spot_interest_staleness_for_margin(&build(now as u64)).unwrap();
    assert_eq!(window, 3_153);

    // Fresh market: a borrow values fine.
    {
        let mut market = build((now - 10) as u64);
        create_anchor_account_info!(market, SpotMarket, ai);
        let map = SpotMarketMap::load_one(&ai, true).unwrap();
        let user = position(SpotBalanceType::Borrow, borrow_of);
        assert!(validate_spot_borrow_interest_fresh_for_margin(&user, &map, now).is_ok());
    }

    // Stale beyond the bound: a borrow is refused.
    {
        let stale_by = window + 1;
        let mut market = build((now - stale_by) as u64);
        create_anchor_account_info!(market, SpotMarket, ai);
        let map = SpotMarketMap::load_one(&ai, true).unwrap();
        let user = position(SpotBalanceType::Borrow, borrow_of);
        assert_eq!(
            validate_spot_borrow_interest_fresh_for_margin(&user, &map, now),
            Err(crate::error::ErrorCode::SpotMarketInterestStaleForMargin),
            "an un-cranked market must not be used to value a borrow"
        );

        // ...but the identical staleness on a DEPOSIT position is allowed: a stale
        // deposit index understates collateral, which errs the protocol's way.
        let depositor = position(SpotBalanceType::Deposit, borrow_of);
        assert!(
            validate_spot_borrow_interest_fresh_for_margin(&depositor, &map, now).is_ok(),
            "a stale deposit index must not block — it understates collateral"
        );
    }

    // Exactly at the bound is still allowed (inclusive).
    {
        let mut market = build((now - window) as u64);
        create_anchor_account_info!(market, SpotMarket, ai);
        let map = SpotMarketMap::load_one(&ai, true).unwrap();
        let user = position(SpotBalanceType::Borrow, borrow_of);
        assert!(validate_spot_borrow_interest_fresh_for_margin(&user, &map, now).is_ok());
    }

    // A borrow whose un-booked interest is under one token unit passes at any
    // staleness. `update_spot_market_cumulative_interest` defers an interval it
    // cannot book in full and leaves `last_interest_ts` where it is, so on a market
    // this small the clock never advances however often the crank runs. Gating on
    // the clock alone would make every fill and withdrawal for this account fail
    // forever. The same market a day stale still refuses a borrow large enough for
    // the omission to clear a token, so the exemption is bounded by the omission
    // and not by the market.
    {
        let mut market = build((now - TWENTY_FOUR_HOUR) as u64);
        create_anchor_account_info!(market, SpotMarket, ai);
        let map = SpotMarketMap::load_one(&ai, true).unwrap();

        let dust = position(SpotBalanceType::Borrow, 1_000);
        assert!(
            validate_spot_borrow_interest_fresh_for_margin(&dust, &map, now).is_ok(),
            "a sub-token omission must not lock the account out"
        );

        let material = position(SpotBalanceType::Borrow, borrow_of);
        assert_eq!(
            validate_spot_borrow_interest_fresh_for_margin(&material, &map, now),
            Err(crate::error::ErrorCode::SpotMarketInterestStaleForMargin),
            "the exemption must not extend to a borrow that hides a whole token"
        );
    }
}

/// The staleness window must come from the market's rate ceiling, not from one
/// fixed span.
///
/// `validate_borrow_rate` bounds `max_borrow_rate` only against
/// `optimal_borrow_rate`, so the field is a `u32` a market may set arbitrarily
/// high. Under a fixed window the share of the debt that un-booked interest hides
/// then scales with that field without limit, which is the property the bound
/// exists to hold down.
#[test]
fn spot_interest_staleness_window_shrinks_as_the_rate_ceiling_rises() {
    use crate::{
        math::{
            constants::{
                MAX_SPOT_INTEREST_STALENESS_FOR_MARGIN,
                MAX_SPOT_INTEREST_UNDERSTATEMENT_FOR_MARGIN, ONE_YEAR, PERCENTAGE_PRECISION,
            },
            margin::max_spot_interest_staleness_for_margin,
        },
        state::spot_market::SpotMarket,
    };

    let with_ceiling = |max_borrow_rate: u32, min_borrow_rate: u8| SpotMarket {
        max_borrow_rate,
        min_borrow_rate,
        ..SpotMarket::default()
    };

    // A low rate earns more than an hour, and the cap keeps it at an hour.
    assert_eq!(
        max_spot_interest_staleness_for_margin(&with_ceiling(200_000, 0)).unwrap(),
        MAX_SPOT_INTEREST_STALENESS_FOR_MARGIN
    );

    // 100% APR: one basis point of the debt takes 3,153s to accrue.
    assert_eq!(
        max_spot_interest_staleness_for_margin(&with_ceiling(1_000_000, 0)).unwrap(),
        3_153
    );

    // 1,000% APR: the same one basis point takes a tenth of that. A fixed hour
    // would have hidden more than ten basis points here.
    assert_eq!(
        max_spot_interest_staleness_for_margin(&with_ceiling(10_000_000, 0)).unwrap(),
        315
    );

    // The largest rate the field can hold leaves no window at all, so the exact
    // measurement in `validate_spot_borrow_interest_fresh_for_margin` runs for any
    // staleness whatsoever.
    assert_eq!(
        max_spot_interest_staleness_for_margin(&with_ceiling(u32::MAX, 0)).unwrap(),
        0
    );

    // `calculate_borrow_rate` floors its result at `min_borrow_rate`, so the
    // ceiling is the larger of the two fields. `min_borrow_rate` counts in half
    // percent, so 40 is 20% APR — here above a `max_borrow_rate` of 1%.
    assert_eq!(
        max_spot_interest_staleness_for_margin(&with_ceiling(10_000, 40)).unwrap(),
        max_spot_interest_staleness_for_margin(&with_ceiling(200_000, 0)).unwrap()
    );

    // A market that charges nothing cannot understate anything, so it takes the cap.
    assert_eq!(
        max_spot_interest_staleness_for_margin(&with_ceiling(0, 0)).unwrap(),
        MAX_SPOT_INTEREST_STALENESS_FOR_MARGIN
    );

    // The window is exactly the span in which the ceiling accrues the allowed
    // share, so the hidden share at the window can never exceed it.
    for max_borrow_rate in [1_000_000_u32, 10_000_000, 123_456_789] {
        let window = max_spot_interest_staleness_for_margin(&with_ceiling(max_borrow_rate, 0))
            .unwrap() as u128;
        let hidden_share = (max_borrow_rate as u128) * window / ONE_YEAR;
        assert!(
            hidden_share <= MAX_SPOT_INTEREST_UNDERSTATEMENT_FOR_MARGIN,
            "rate {} hides {} of {} at a {}s window",
            max_borrow_rate,
            hidden_share,
            PERCENTAGE_PRECISION,
            window
        );
    }
}

mod fill_perp_order_margin_requirement_with_isolated {
    use {
        crate::{
            create_anchor_account_info,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I64, PEG_PRECISION,
                    SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                margin::{
                    calculate_margin_requirement_and_total_collateral_and_liability_info,
                    MarginRequirementType,
                },
            },
            state::{
                margin_calculation::{MarginContext, MarginTypeConfig},
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
            test_utils::get_pyth_price,
            QUOTE_PRECISION_I64,
        },
        anchor_lang::prelude::Pubkey,
        std::str::FromStr,
    };

    fn with_sol_eth_setup<F, R>(slot: u64, f: F) -> R
    where
        F: FnOnce(&mut OracleMap, &PerpMarketMap, &SpotMarketMap) -> R,
    {
        let sol_oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        let eth_oracle_price_key =
            Pubkey::from_str("AHRAk64kPiGwkbkisDvjVYzq6Ho5Q2wQSj28vAaAt7Tq").unwrap();
        let mut sol_oracle_price = get_pyth_price(100, 6);
        let mut eth_oracle_price = get_pyth_price(1000, 6);
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            sol_oracle_account_info
        );
        create_anchor_account_info!(
            eth_oracle_price,
            &eth_oracle_price_key,
            PythLazerOracle,
            eth_oracle_account_info
        );
        let oracle_account_infos = [sol_oracle_account_info, eth_oracle_account_info];
        let mut oracle_map = OracleMap::load(
            &mut oracle_account_infos.iter().peekable(),
            slot,
            crate::math::time::SlotClock::baseline(),
            None,
        )
        .unwrap();

        let mut sol_perp_market = PerpMarket {
            market_index: 0,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: sol_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        let mut eth_perp_market = PerpMarket {
            market_index: 2,
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 1000 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            oracle: eth_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(sol_perp_market, PerpMarket, sol_perp_market_account_info);
        create_anchor_account_info!(eth_perp_market, PerpMarket, eth_perp_market_account_info);
        let perp_market_map = PerpMarketMap::load_multiple(
            vec![&sol_perp_market_account_info, &eth_perp_market_account_info],
            true,
        )
        .unwrap();

        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 10000 * SPOT_BALANCE_PRECISION,
            liquidator_fee: 0,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let spot_market_map =
            SpotMarketMap::load_multiple(vec![&usdc_spot_market_account_info], true).unwrap();

        f(&mut oracle_map, &perp_market_map, &spot_market_map)
    }

    // --- Scenario 1a: Isolated fill, position increasing (current isolated = Fill) ---

    #[test]
    fn isolated_fill_increasing_passes_when_current_isolated_passes_fill_others_maintenance() {
        with_sol_eth_setup(0, |oracle_map, perp_market_map, spot_market_map| {
            let mut spot_positions = [SpotPosition::default(); 8];
            spot_positions[0] = SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 70 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            };

            let mut perp_positions = [PerpPosition::default(); 8];
            perp_positions[0] = PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                isolated_position_scaled_balance: 150 * SPOT_BALANCE_PRECISION_U64,
                ..PerpPosition::default()
            };

            let user = User {
                orders: [Order::default(); 32],
                perp_positions,
                spot_positions,
                ..User::default()
            };

            let margin_type_config = MarginTypeConfig::IsolatedPositionOverride {
                market_index: 0,
                margin_requirement_type: MarginRequirementType::Fill,
                default_isolated_margin_requirement_type: MarginRequirementType::Maintenance,
                cross_margin_requirement_type: MarginRequirementType::Maintenance,
            };
            let context = MarginContext::standard_with_config(margin_type_config);

            let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                context,
            )
            .unwrap();

            assert!(
            calculation.meets_margin_requirement(),
            "Isolated fill increasing should pass when current isolated passes Fill and others pass Maintenance"
        );
        });
    }

    #[test]
    fn isolated_fill_increasing_fails_when_current_isolated_only_passes_maintenance() {
        with_sol_eth_setup(0, |oracle_map, perp_market_map, spot_market_map| {
            let mut spot_positions = [SpotPosition::default(); 8];
            spot_positions[0] = SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            };

            let mut perp_positions = [PerpPosition::default(); 8];
            perp_positions[0] = PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                isolated_position_scaled_balance: 7 * SPOT_BALANCE_PRECISION_U64,
                ..PerpPosition::default()
            };

            let user = User {
                orders: [Order::default(); 32],
                perp_positions,
                spot_positions,
                ..User::default()
            };

            let margin_type_config = MarginTypeConfig::IsolatedPositionOverride {
                market_index: 0,
                margin_requirement_type: MarginRequirementType::Fill,
                default_isolated_margin_requirement_type: MarginRequirementType::Maintenance,
                cross_margin_requirement_type: MarginRequirementType::Maintenance,
            };
            let context = MarginContext::standard_with_config(margin_type_config);

            let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                context,
            )
            .unwrap();

            assert!(
            !calculation.meets_margin_requirement(),
            "Isolated fill increasing should fail when current isolated only passes Maintenance (needs Fill after delta)"
        );
        });
    }

    #[test]
    fn isolated_fill_increasing_fails_when_current_isolated_fails_maintenance() {
        with_sol_eth_setup(0, |oracle_map, perp_market_map, spot_market_map| {
            let mut spot_positions = [SpotPosition::default(); 8];
            spot_positions[0] = SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            };

            let mut perp_positions = [PerpPosition::default(); 8];
            perp_positions[0] = PerpPosition {
                market_index: 0,
                base_asset_amount: 10 * BASE_PRECISION_I64,
                quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                isolated_position_scaled_balance: 40 * SPOT_BALANCE_PRECISION_U64,
                ..PerpPosition::default()
            };

            let user = User {
                orders: [Order::default(); 32],
                perp_positions,
                spot_positions,
                ..User::default()
            };

            let margin_type_config = MarginTypeConfig::IsolatedPositionOverride {
                market_index: 0,
                margin_requirement_type: MarginRequirementType::Fill,
                default_isolated_margin_requirement_type: MarginRequirementType::Maintenance,
                cross_margin_requirement_type: MarginRequirementType::Maintenance,
            };
            let context = MarginContext::standard_with_config(margin_type_config);

            let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                context,
            )
            .unwrap();

            assert!(
                !calculation.meets_margin_requirement(),
                "Isolated fill increasing should fail when current isolated fails Maintenance"
            );
        });
    }

    #[test]
    fn isolated_fill_increasing_fails_when_cross_fails_maintenance() {
        with_sol_eth_setup(0, |oracle_map, perp_market_map, spot_market_map| {
            let mut spot_positions = [SpotPosition::default(); 8];
            spot_positions[0] = SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 40 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            };

            let mut perp_positions = [PerpPosition::default(); 8];
            perp_positions[0] = PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                isolated_position_scaled_balance: 150 * SPOT_BALANCE_PRECISION_U64,
                ..PerpPosition::default()
            };
            perp_positions[1] = PerpPosition {
                market_index: 2,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            };

            let user = User {
                orders: [Order::default(); 32],
                perp_positions,
                spot_positions,
                ..User::default()
            };

            let margin_type_config = MarginTypeConfig::IsolatedPositionOverride {
                market_index: 0,
                margin_requirement_type: MarginRequirementType::Fill,
                default_isolated_margin_requirement_type: MarginRequirementType::Maintenance,
                cross_margin_requirement_type: MarginRequirementType::Maintenance,
            };
            let context = MarginContext::standard_with_config(margin_type_config);

            let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                context,
            )
            .unwrap();

            assert!(
                !calculation.meets_margin_requirement(),
                "Isolated fill increasing should fail when cross fails Maintenance"
            );
        });
    }

    #[test]
    fn isolated_fill_increasing_fails_when_other_isolated_fails_maintenance() {
        with_sol_eth_setup(0, |oracle_map, perp_market_map, spot_market_map| {
            let mut spot_positions = [SpotPosition::default(); 8];
            spot_positions[0] = SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 70 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            };

            let mut perp_positions = [PerpPosition::default(); 8];
            perp_positions[0] = PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                isolated_position_scaled_balance: 150 * SPOT_BALANCE_PRECISION_U64,
                ..PerpPosition::default()
            };
            perp_positions[1] = PerpPosition {
                market_index: 2,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                isolated_position_scaled_balance: 40 * SPOT_BALANCE_PRECISION_U64,
                ..PerpPosition::default()
            };

            let user = User {
                orders: [Order::default(); 32],
                perp_positions,
                spot_positions,
                ..User::default()
            };

            let margin_type_config = MarginTypeConfig::IsolatedPositionOverride {
                market_index: 0,
                margin_requirement_type: MarginRequirementType::Fill,
                default_isolated_margin_requirement_type: MarginRequirementType::Maintenance,
                cross_margin_requirement_type: MarginRequirementType::Maintenance,
            };
            let context = MarginContext::standard_with_config(margin_type_config);

            let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                context,
            )
            .unwrap();

            assert!(
                !calculation.meets_margin_requirement(),
                "Isolated fill increasing should fail when other isolated fails Maintenance"
            );
        });
    }

    // --- Scenario 1b: Isolated fill, position decreasing (all Maintenance) ---

    #[test]
    fn isolated_fill_decreasing_passes_when_all_pass_maintenance() {
        with_sol_eth_setup(0, |oracle_map, perp_market_map, spot_market_map| {
            let mut spot_positions = [SpotPosition::default(); 8];
            spot_positions[0] = SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 70 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            };

            let mut perp_positions = [PerpPosition::default(); 8];
            perp_positions[0] = PerpPosition {
                market_index: 0,
                base_asset_amount: 10 * BASE_PRECISION_I64,
                quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                isolated_position_scaled_balance: 70 * SPOT_BALANCE_PRECISION_U64,
                ..PerpPosition::default()
            };

            let user = User {
                orders: [Order::default(); 32],
                perp_positions,
                spot_positions,
                ..User::default()
            };

            let margin_type_config = MarginTypeConfig::IsolatedPositionOverride {
                market_index: 0,
                margin_requirement_type: MarginRequirementType::Maintenance,
                default_isolated_margin_requirement_type: MarginRequirementType::Maintenance,
                cross_margin_requirement_type: MarginRequirementType::Maintenance,
            };
            let context = MarginContext::standard_with_config(margin_type_config);

            let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                context,
            )
            .unwrap();

            assert!(
                calculation.meets_margin_requirement(),
                "Isolated fill decreasing should pass when all pass Maintenance"
            );
        });
    }

    #[test]
    fn isolated_fill_decreasing_fails_when_current_isolated_fails_maintenance() {
        with_sol_eth_setup(0, |oracle_map, perp_market_map, spot_market_map| {
            let mut spot_positions = [SpotPosition::default(); 8];
            spot_positions[0] = SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            };

            let mut perp_positions = [PerpPosition::default(); 8];
            perp_positions[0] = PerpPosition {
                market_index: 0,
                base_asset_amount: 10 * BASE_PRECISION_I64,
                quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                isolated_position_scaled_balance: 40 * SPOT_BALANCE_PRECISION_U64,
                ..PerpPosition::default()
            };

            let user = User {
                orders: [Order::default(); 32],
                perp_positions,
                spot_positions,
                ..User::default()
            };

            let margin_type_config = MarginTypeConfig::IsolatedPositionOverride {
                market_index: 0,
                margin_requirement_type: MarginRequirementType::Maintenance,
                default_isolated_margin_requirement_type: MarginRequirementType::Maintenance,
                cross_margin_requirement_type: MarginRequirementType::Maintenance,
            };
            let context = MarginContext::standard_with_config(margin_type_config);

            let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                context,
            )
            .unwrap();

            assert!(
                !calculation.meets_margin_requirement(),
                "Isolated fill decreasing should fail when current isolated fails Maintenance"
            );
        });
    }

    #[test]
    fn isolated_fill_decreasing_fails_when_other_isolated_fails_maintenance() {
        with_sol_eth_setup(0, |oracle_map, perp_market_map, spot_market_map| {
            let mut spot_positions = [SpotPosition::default(); 8];
            spot_positions[0] = SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 70 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            };

            let mut perp_positions = [PerpPosition::default(); 8];
            perp_positions[0] = PerpPosition {
                market_index: 0,
                base_asset_amount: 10 * BASE_PRECISION_I64,
                quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                isolated_position_scaled_balance: 70 * SPOT_BALANCE_PRECISION_U64,
                ..PerpPosition::default()
            };
            perp_positions[1] = PerpPosition {
                market_index: 2,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                isolated_position_scaled_balance: 40 * SPOT_BALANCE_PRECISION_U64,
                ..PerpPosition::default()
            };

            let user = User {
                orders: [Order::default(); 32],
                perp_positions,
                spot_positions,
                ..User::default()
            };

            let margin_type_config = MarginTypeConfig::IsolatedPositionOverride {
                market_index: 0,
                margin_requirement_type: MarginRequirementType::Maintenance,
                default_isolated_margin_requirement_type: MarginRequirementType::Maintenance,
                cross_margin_requirement_type: MarginRequirementType::Maintenance,
            };
            let context = MarginContext::standard_with_config(margin_type_config);

            let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                context,
            )
            .unwrap();

            assert!(
                !calculation.meets_margin_requirement(),
                "Isolated fill decreasing should fail when other isolated fails Maintenance"
            );
        });
    }

    // --- Scenario 2a: Cross fill, position increasing (cross = Fill) ---

    #[test]
    fn cross_fill_increasing_passes_when_cross_passes_fill_isolated_maintenance() {
        with_sol_eth_setup(0, |oracle_map, perp_market_map, spot_market_map| {
            let mut spot_positions = [SpotPosition::default(); 8];
            spot_positions[0] = SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 150 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            };

            let mut perp_positions = [PerpPosition::default(); 8];
            perp_positions[0] = PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            };

            let user = User {
                orders: [Order::default(); 32],
                perp_positions,
                spot_positions,
                ..User::default()
            };

            let margin_type_config = MarginTypeConfig::CrossMarginOverride {
                margin_requirement_type: MarginRequirementType::Fill,
                default_margin_requirement_type: MarginRequirementType::Maintenance,
            };
            let context = MarginContext::standard_with_config(margin_type_config);

            let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                context,
            )
            .unwrap();

            assert!(
            calculation.meets_margin_requirement(),
            "Cross fill increasing should pass when cross passes Fill and isolated pass Maintenance"
        );
        });
    }

    #[test]
    fn cross_fill_increasing_fails_when_cross_only_passes_maintenance() {
        with_sol_eth_setup(0, |oracle_map, perp_market_map, spot_market_map| {
            let mut spot_positions = [SpotPosition::default(); 8];
            spot_positions[0] = SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 70 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            };

            let mut perp_positions = [PerpPosition::default(); 8];
            perp_positions[0] = PerpPosition {
                market_index: 0,
                base_asset_amount: 10 * BASE_PRECISION_I64,
                quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            };

            let user = User {
                orders: [Order::default(); 32],
                perp_positions,
                spot_positions,
                ..User::default()
            };

            let margin_type_config = MarginTypeConfig::CrossMarginOverride {
                margin_requirement_type: MarginRequirementType::Fill,
                default_margin_requirement_type: MarginRequirementType::Maintenance,
            };
            let context = MarginContext::standard_with_config(margin_type_config);

            let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                context,
            )
            .unwrap();

            assert!(
                !calculation.meets_margin_requirement(),
                "Cross fill increasing should fail when cross only passes Maintenance"
            );
        });
    }

    #[test]
    fn cross_fill_increasing_fails_when_cross_fails_maintenance() {
        with_sol_eth_setup(0, |oracle_map, perp_market_map, spot_market_map| {
            let mut spot_positions = [SpotPosition::default(); 8];
            spot_positions[0] = SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 40 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            };

            let mut perp_positions = [PerpPosition::default(); 8];
            perp_positions[0] = PerpPosition {
                market_index: 0,
                base_asset_amount: 10 * BASE_PRECISION_I64,
                quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            };

            let user = User {
                orders: [Order::default(); 32],
                perp_positions,
                spot_positions,
                ..User::default()
            };

            let margin_type_config = MarginTypeConfig::CrossMarginOverride {
                margin_requirement_type: MarginRequirementType::Fill,
                default_margin_requirement_type: MarginRequirementType::Maintenance,
            };
            let context = MarginContext::standard_with_config(margin_type_config);

            let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                context,
            )
            .unwrap();

            assert!(
                !calculation.meets_margin_requirement(),
                "Cross fill increasing should fail when cross fails Maintenance"
            );
        });
    }

    #[test]
    fn cross_fill_increasing_fails_when_other_isolated_fails_maintenance() {
        with_sol_eth_setup(0, |oracle_map, perp_market_map, spot_market_map| {
            let mut spot_positions = [SpotPosition::default(); 8];
            spot_positions[0] = SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 150 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            };

            let mut perp_positions = [PerpPosition::default(); 8];
            perp_positions[0] = PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            };
            perp_positions[1] = PerpPosition {
                market_index: 2,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                isolated_position_scaled_balance: 40 * SPOT_BALANCE_PRECISION_U64,
                ..PerpPosition::default()
            };

            let user = User {
                orders: [Order::default(); 32],
                perp_positions,
                spot_positions,
                ..User::default()
            };

            let margin_type_config = MarginTypeConfig::CrossMarginOverride {
                margin_requirement_type: MarginRequirementType::Fill,
                default_margin_requirement_type: MarginRequirementType::Maintenance,
            };
            let context = MarginContext::standard_with_config(margin_type_config);

            let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                context,
            )
            .unwrap();

            assert!(
                !calculation.meets_margin_requirement(),
                "Cross fill increasing should fail when other isolated fails Maintenance"
            );
        });
    }

    // --- Scenario 2b: Cross fill, position decreasing (all Maintenance) ---

    #[test]
    fn cross_fill_decreasing_passes_when_all_pass_maintenance() {
        with_sol_eth_setup(0, |oracle_map, perp_market_map, spot_market_map| {
            let mut spot_positions = [SpotPosition::default(); 8];
            spot_positions[0] = SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 70 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            };

            let mut perp_positions = [PerpPosition::default(); 8];
            perp_positions[0] = PerpPosition {
                market_index: 0,
                base_asset_amount: 10 * BASE_PRECISION_I64,
                quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            };

            let user = User {
                orders: [Order::default(); 32],
                perp_positions,
                spot_positions,
                ..User::default()
            };

            let margin_type_config = MarginTypeConfig::CrossMarginOverride {
                margin_requirement_type: MarginRequirementType::Maintenance,
                default_margin_requirement_type: MarginRequirementType::Maintenance,
            };
            let context = MarginContext::standard_with_config(margin_type_config);

            let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                context,
            )
            .unwrap();

            assert!(
                calculation.meets_margin_requirement(),
                "Cross fill decreasing should pass when all pass Maintenance"
            );
        });
    }

    #[test]
    fn cross_fill_decreasing_fails_when_cross_fails_maintenance() {
        with_sol_eth_setup(0, |oracle_map, perp_market_map, spot_market_map| {
            let mut spot_positions = [SpotPosition::default(); 8];
            spot_positions[0] = SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 40 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            };

            let mut perp_positions = [PerpPosition::default(); 8];
            perp_positions[0] = PerpPosition {
                market_index: 0,
                base_asset_amount: 10 * BASE_PRECISION_I64,
                quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            };

            let user = User {
                orders: [Order::default(); 32],
                perp_positions,
                spot_positions,
                ..User::default()
            };

            let margin_type_config = MarginTypeConfig::CrossMarginOverride {
                margin_requirement_type: MarginRequirementType::Maintenance,
                default_margin_requirement_type: MarginRequirementType::Maintenance,
            };
            let context = MarginContext::standard_with_config(margin_type_config);

            let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                context,
            )
            .unwrap();

            assert!(
                !calculation.meets_margin_requirement(),
                "Cross fill decreasing should fail when cross fails Maintenance"
            );
        });
    }

    #[test]
    fn cross_fill_decreasing_fails_when_other_isolated_fails_maintenance() {
        with_sol_eth_setup(0, |oracle_map, perp_market_map, spot_market_map| {
            let mut spot_positions = [SpotPosition::default(); 8];
            spot_positions[0] = SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 70 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            };

            let mut perp_positions = [PerpPosition::default(); 8];
            perp_positions[0] = PerpPosition {
                market_index: 0,
                base_asset_amount: 10 * BASE_PRECISION_I64,
                quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            };
            perp_positions[1] = PerpPosition {
                market_index: 2,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -1000 * QUOTE_PRECISION_I64,
                position_flag: PositionFlag::IsolatedPosition as u8,
                isolated_position_scaled_balance: 40 * SPOT_BALANCE_PRECISION_U64,
                ..PerpPosition::default()
            };

            let user = User {
                orders: [Order::default(); 32],
                perp_positions,
                spot_positions,
                ..User::default()
            };

            let margin_type_config = MarginTypeConfig::CrossMarginOverride {
                margin_requirement_type: MarginRequirementType::Maintenance,
                default_margin_requirement_type: MarginRequirementType::Maintenance,
            };
            let context = MarginContext::standard_with_config(margin_type_config);

            let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                context,
            )
            .unwrap();

            assert!(
                !calculation.meets_margin_requirement(),
                "Cross fill decreasing should fail when other isolated fails Maintenance"
            );
        });
    }
}
