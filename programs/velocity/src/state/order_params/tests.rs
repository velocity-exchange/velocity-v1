use crate::state::order_params::parse_optional_params;

mod get_auction_duration {
    use crate::{state::order_params::get_auction_duration, ContractTier, PRICE_PRECISION_U64};

    #[test]
    fn test() {
        let price_diff = 0;
        let price = 100 * PRICE_PRECISION_U64;
        let contract_tier = ContractTier::C;

        let duration = get_auction_duration(price_diff, price, contract_tier).unwrap();
        assert_eq!(duration, 1);

        let price_diff = PRICE_PRECISION_U64 / 10;
        let price = 100 * PRICE_PRECISION_U64;

        let duration = get_auction_duration(price_diff, price, contract_tier).unwrap();
        assert_eq!(duration, 6);

        let price_diff = PRICE_PRECISION_U64 / 2;
        let price = 100 * PRICE_PRECISION_U64;

        let duration = get_auction_duration(price_diff, price, contract_tier).unwrap();
        assert_eq!(duration, 30);

        let price_diff = PRICE_PRECISION_U64;
        let price = 100 * PRICE_PRECISION_U64;

        let duration = get_auction_duration(price_diff, price, contract_tier).unwrap();
        assert_eq!(duration, 60);

        let price_diff = 2 * PRICE_PRECISION_U64;
        let price = 100 * PRICE_PRECISION_U64;

        let duration = get_auction_duration(price_diff, price, contract_tier).unwrap();
        assert_eq!(duration, 120);
    }

    #[test]
    fn duration_is_wall_clock_units_independent_of_slot_duration() {
        let price = 100 * PRICE_PRECISION_U64;
        let tier = ContractTier::C;

        // a 2%-diff auction is 48s at tier C: 120 wall clock 400ms units,
        // whatever the live slot duration; progress converts elapsed slots to
        // wall clock at fill time instead of inflating the stored count
        let diff = 2 * PRICE_PRECISION_U64;
        assert_eq!(get_auction_duration(diff, price, tier).unwrap(), 120);

        // the 180 unit (72s) maximum fits the u8 with room to spare (255
        // units = 102s), so no wall clock compression exists at any gate
        let max_diff = 3 * PRICE_PRECISION_U64;
        assert_eq!(get_auction_duration(max_diff, price, tier).unwrap(), 180);
    }
}

mod update_perp_auction_params {
    use crate::{
        state::{
            order_params::PostOnlyParam,
            perp_market::{ContractTier, MarketStats, PerpMarket, AMM},
            user::OrderType,
        },
        OracleSource, OrderParams, PositionDirection, AMM_RESERVE_PRECISION, PEG_PRECISION,
        PRICE_PRECISION_I64, PRICE_PRECISION_U64, QUOTE_PRECISION_U64,
    };

    #[test]
    fn test_extreme_sanitize_oracle_order() {
        let oracle_price = 145 * PRICE_PRECISION_I64;
        let amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,

            ..AMM::default()
        };
        let mut market_stats = MarketStats::default();
        market_stats.last_bid_price_twap = (oracle_price - 192988) as u64;
        market_stats.last_mark_price_twap_5min = oracle_price as u64;
        market_stats.last_ask_price_twap = (oracle_price + 192988) as u64;
        market_stats.historical_oracle_data.last_oracle_price_twap = oracle_price;
        market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min = oracle_price;

        market_stats.historical_oracle_data.last_oracle_price = oracle_price;
        market_stats.volume_24h = 1_000_000 * QUOTE_PRECISION_U64;
        let perp_market = PerpMarket {
            market_stats,
            amm,
            ..PerpMarket::default()
        };
        let order_params_before = OrderParams {
            order_type: OrderType::Oracle,
            auction_start_price: Some(market_stats.last_bid_price_twap as i64),
            auction_end_price: Some((market_stats.last_bid_price_twap + 1000000) as i64),
            auction_duration: Some(30),
            direction: PositionDirection::Long,
            ..OrderParams::default()
        };

        assert_eq!(order_params_before.auction_start_price, Some(144_807_012));
        assert_eq!(order_params_before.auction_end_price, Some(145_807_012));

        let mut order_params_after = order_params_before;
        let sanitized = order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();

        // Spread reserves are no longer cached on AMM; the auction price
        // is now derived from on-demand quote state. Legacy: -144807 /
        // 3_092_988.
        assert_eq!(order_params_after.auction_start_price, Some(-192988));
        assert_eq!(order_params_after.auction_end_price, Some(3_092_988));
        // duration floor paces the requested spread (1e6 / 145 = 0.69% -> 42),
        // not the wider sanitized spread (2.27% -> 136)
        assert_eq!(order_params_after.auction_duration, Some(42));
        assert_eq!(sanitized, true);

        let order_params_before2 = OrderParams {
            order_type: OrderType::Oracle,
            auction_start_price: Some(market_stats.last_ask_price_twap as i64),
            auction_end_price: Some((market_stats.last_bid_price_twap - 1000000) as i64),
            auction_duration: Some(30),
            direction: PositionDirection::Short,
            ..OrderParams::default()
        };

        assert_eq!(order_params_before2.auction_start_price, Some(145192988));
        assert_eq!(order_params_before2.auction_end_price, Some(143807012));

        let mut order_params_after2 = order_params_before2;
        order_params_after2
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();

        assert_eq!(order_params_after2.auction_start_price, Some(145192988)); // will never fill kek
        assert_eq!(order_params_after2.auction_end_price, Some(143807012));
        assert_eq!(order_params_after2.auction_duration, Some(58));

        // huge negative for short
        let order_params_before3 = OrderParams {
            order_type: OrderType::Oracle,
            auction_start_price: Some(-(market_stats.last_ask_price_twap as i64)),
            auction_end_price: Some(-((market_stats.last_bid_price_twap - 1000000) as i64)),
            auction_duration: Some(30),
            direction: PositionDirection::Short,
            oracle_price_offset: Some(-((market_stats.last_bid_price_twap - 1000000) as i64)),
            ..OrderParams::default()
        };

        assert_eq!(order_params_before3.auction_start_price, Some(-145192988));
        assert_eq!(order_params_before3.auction_end_price, Some(-143807012));
        assert_eq!(order_params_before3.oracle_price_offset, Some(-143807012));

        let mut order_params_after3 = order_params_before3;
        order_params_after3
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();

        assert_eq!(order_params_after3.auction_start_price, Some(192988));
        assert_eq!(order_params_after3.auction_end_price, Some(-3092988));
        assert_eq!(order_params_after3.oracle_price_offset, Some(-143807012));

        // requested spread 1385976 / 145 = 0.96% -> 58, vs sanitized 2.27% -> 136
        assert_eq!(order_params_after3.auction_duration, Some(58));
    }

    #[test]
    fn test_signed_msg_orders_oracle() {
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,

            ..AMM::default()
        };
        let mut market_stats = MarketStats::default();
        market_stats.last_bid_price_twap = (oracle_price - 100000) as u64;
        market_stats.last_mark_price_twap_5min = oracle_price as u64;
        market_stats.last_ask_price_twap = (oracle_price + 100000) as u64;
        market_stats.historical_oracle_data.last_oracle_price_twap = oracle_price;
        market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min = oracle_price;

        market_stats.historical_oracle_data.last_oracle_price = oracle_price;
        market_stats.volume_24h = 1_000_000 * QUOTE_PRECISION_U64;
        let perp_market = PerpMarket {
            market_stats,
            amm,
            contract_tier: ContractTier::A,
            ..PerpMarket::default()
        };
        let order_params_long_before = OrderParams {
            order_type: OrderType::Oracle,
            auction_start_price: Some(0),
            auction_end_price: Some(200000),
            auction_duration: Some(30),
            direction: PositionDirection::Long,
            ..OrderParams::default()
        };

        let mut order_params_long_after = order_params_long_before;
        let sanitized = order_params_long_after
            .update_perp_auction_params(&perp_market, oracle_price, true)
            .unwrap();

        assert_eq!(order_params_long_after.auction_start_price, Some(0));
        assert_eq!(order_params_long_after.auction_end_price, Some(200000));
        assert_eq!(order_params_long_after.auction_duration, Some(30));
        assert_eq!(sanitized, false);

        let order_params_long_before_not_signed = OrderParams {
            order_type: OrderType::Oracle,
            auction_start_price: Some(0),
            auction_end_price: Some(200000),
            auction_duration: Some(30),
            direction: PositionDirection::Long,
            ..OrderParams::default()
        };

        let mut order_params_long_after_not_signed = order_params_long_before_not_signed;
        let sanitized = order_params_long_after_not_signed
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();

        assert_eq!(
            order_params_long_after_not_signed.auction_start_price,
            Some(-100000)
        );
        assert_eq!(
            order_params_long_after_not_signed.auction_end_price,
            Some(200000)
        );
        assert_eq!(
            order_params_long_after_not_signed.auction_duration,
            Some(30)
        );
        assert_eq!(sanitized, true);

        // now short
        let order_params_short_before = OrderParams {
            order_type: OrderType::Oracle,
            auction_start_price: Some(100),
            auction_end_price: Some(-200000),
            auction_duration: Some(30),
            direction: PositionDirection::Short,
            ..OrderParams::default()
        };

        let mut order_params_short_after = order_params_short_before;
        let sanitized = order_params_short_after
            .update_perp_auction_params(&perp_market, oracle_price, true)
            .unwrap();

        assert_eq!(order_params_short_after.auction_start_price, Some(100));
        assert_eq!(order_params_short_after.auction_end_price, Some(-200000));
        assert_eq!(order_params_short_after.auction_duration, Some(30));
        assert_eq!(sanitized, false);

        let order_params_long_before_not_signed = OrderParams {
            order_type: OrderType::Oracle,
            auction_start_price: Some(0),
            auction_end_price: Some(-200000),
            auction_duration: Some(30),
            direction: PositionDirection::Short,
            ..OrderParams::default()
        };

        let mut order_params_long_after_not_signed = order_params_long_before_not_signed;
        let sanitized = order_params_long_after_not_signed
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();

        assert_eq!(
            order_params_long_after_not_signed.auction_start_price,
            Some(100000)
        );
        assert_eq!(
            order_params_long_after_not_signed.auction_end_price,
            Some(-200000)
        );
        assert_eq!(
            order_params_long_after_not_signed.auction_duration,
            Some(30)
        );
        assert_eq!(sanitized, true);
    }

    #[test]
    fn test_signed_msg_non_tail_oracle_preserves_user_auction_params() {
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,

            ..AMM::default()
        };
        let mut market_stats = MarketStats::default();
        market_stats.last_bid_price_twap = (oracle_price - 100000) as u64;
        market_stats.last_mark_price_twap_5min = oracle_price as u64;
        market_stats.last_ask_price_twap = (oracle_price + 100000) as u64;
        market_stats.historical_oracle_data.last_oracle_price_twap = oracle_price;
        market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min = oracle_price;

        market_stats.historical_oracle_data.last_oracle_price = oracle_price;
        market_stats.volume_24h = 1_000_000 * QUOTE_PRECISION_U64;
        let perp_market = PerpMarket {
            market_stats,
            amm,
            contract_tier: ContractTier::A,
            ..PerpMarket::default()
        };

        let order_params_before = OrderParams {
            order_type: OrderType::Oracle,
            auction_start_price: Some(-300000),
            auction_end_price: Some(300000),
            auction_duration: Some(5),
            direction: PositionDirection::Long,
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        let sanitized = order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, true)
            .unwrap();

        assert_eq!(order_params_after.auction_start_price, Some(-300000));
        assert_eq!(order_params_after.auction_end_price, Some(300000));
        assert_eq!(order_params_after.auction_duration, Some(5));
        assert_eq!(sanitized, false);

        let order_params_before = OrderParams {
            order_type: OrderType::Oracle,
            auction_start_price: Some(300000),
            auction_end_price: Some(-300000),
            auction_duration: Some(5),
            direction: PositionDirection::Short,
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        let sanitized = order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, true)
            .unwrap();

        assert_eq!(order_params_after.auction_start_price, Some(300000));
        assert_eq!(order_params_after.auction_end_price, Some(-300000));
        assert_eq!(order_params_after.auction_duration, Some(5));
        assert_eq!(sanitized, false);
    }

    #[test]
    fn test_signed_msg_non_tail_market_preserves_user_auction_params() {
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,

            ..AMM::default()
        };
        let mut market_stats = MarketStats::default();
        market_stats.last_bid_price_twap = (oracle_price - 100000) as u64;
        market_stats.last_mark_price_twap_5min = oracle_price as u64;
        market_stats.last_ask_price_twap = (oracle_price + 100000) as u64;
        market_stats.historical_oracle_data.last_oracle_price_twap = oracle_price;
        market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min = oracle_price;

        market_stats.historical_oracle_data.last_oracle_price = oracle_price;
        market_stats.volume_24h = 1_000_000 * QUOTE_PRECISION_U64;
        let perp_market = PerpMarket {
            market_stats,
            amm,
            contract_tier: ContractTier::B,
            ..PerpMarket::default()
        };

        let order_params_before = OrderParams {
            order_type: OrderType::Market,
            auction_start_price: Some(99700000),
            auction_end_price: Some(100300000),
            auction_duration: Some(5),
            direction: PositionDirection::Long,
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        let sanitized = order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, true)
            .unwrap();

        assert_eq!(order_params_after.auction_start_price, Some(99700000));
        assert_eq!(order_params_after.auction_end_price, Some(100300000));
        assert_eq!(order_params_after.auction_duration, Some(5));
        assert_eq!(sanitized, false);

        let order_params_before = OrderParams {
            order_type: OrderType::Market,
            auction_start_price: Some(100300000),
            auction_end_price: Some(99700000),
            auction_duration: Some(5),
            direction: PositionDirection::Short,
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        let sanitized = order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, true)
            .unwrap();

        assert_eq!(order_params_after.auction_start_price, Some(100300000));
        assert_eq!(order_params_after.auction_end_price, Some(99700000));
        assert_eq!(order_params_after.auction_duration, Some(5));
        assert_eq!(sanitized, false);
    }

    #[test]
    fn test_signed_msg_non_tail_crossing_limit_preserves_user_auction_params() {
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,

            ..AMM::default()
        };
        let mut market_stats = MarketStats::default();
        market_stats.last_bid_price_twap = (oracle_price - 100000) as u64;
        market_stats.last_mark_price_twap_5min = oracle_price as u64;
        market_stats.last_ask_price_twap = (oracle_price + 100000) as u64;
        market_stats.historical_oracle_data.last_oracle_price_twap = oracle_price;
        market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min = oracle_price;

        market_stats.historical_oracle_data.last_oracle_price = oracle_price;
        market_stats.volume_24h = 1_000_000 * QUOTE_PRECISION_U64;
        let perp_market = PerpMarket {
            market_stats,
            amm,
            contract_tier: ContractTier::A,
            ..PerpMarket::default()
        };

        let order_params_before = OrderParams {
            order_type: OrderType::Limit,
            auction_start_price: Some(99700000),
            auction_end_price: Some(100300000),
            auction_duration: Some(5),
            price: 100300000,
            direction: PositionDirection::Long,
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        let sanitized = order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, true)
            .unwrap();

        assert_eq!(order_params_after.auction_start_price, Some(99700000));
        assert_eq!(order_params_after.auction_end_price, Some(100300000));
        assert_eq!(order_params_after.auction_duration, Some(5));
        assert_eq!(sanitized, false);

        let order_params_before = OrderParams {
            order_type: OrderType::Limit,
            auction_start_price: Some(100300000),
            auction_end_price: Some(99700000),
            auction_duration: Some(5),
            price: 99700000,
            direction: PositionDirection::Short,
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        let sanitized = order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, true)
            .unwrap();

        assert_eq!(order_params_after.auction_start_price, Some(100300000));
        assert_eq!(order_params_after.auction_end_price, Some(99700000));
        assert_eq!(order_params_after.auction_duration, Some(5));
        assert_eq!(sanitized, false);
    }

    #[test]
    fn test_signed_msg_orders_limit() {
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,

            ..AMM::default()
        };
        let mut market_stats = MarketStats::default();
        market_stats.last_bid_price_twap = (oracle_price - 100000) as u64;
        market_stats.last_mark_price_twap_5min = oracle_price as u64;
        market_stats.last_ask_price_twap = (oracle_price + 100000) as u64;
        market_stats.historical_oracle_data.last_oracle_price_twap = oracle_price;
        market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min = oracle_price;

        market_stats.historical_oracle_data.last_oracle_price = oracle_price;
        market_stats.volume_24h = 1_000_000 * QUOTE_PRECISION_U64;
        let perp_market = PerpMarket {
            market_stats,
            amm,
            contract_tier: ContractTier::A,
            ..PerpMarket::default()
        };
        let order_params_long_before = OrderParams {
            order_type: OrderType::Market,
            auction_start_price: Some(100000000),
            auction_end_price: Some(100200000),
            auction_duration: Some(30),
            direction: PositionDirection::Long,
            ..OrderParams::default()
        };

        let mut order_params_long_after = order_params_long_before;
        let sanitized = order_params_long_after
            .update_perp_auction_params(&perp_market, oracle_price, true)
            .unwrap();

        assert_eq!(order_params_long_after.auction_start_price, Some(100000000));
        assert_eq!(order_params_long_after.auction_end_price, Some(100200000));
        assert_eq!(order_params_long_after.auction_duration, Some(30));
        assert_eq!(sanitized, false);

        let order_params_long_before_not_signed = OrderParams {
            order_type: OrderType::Market,
            auction_start_price: Some(100000000),
            auction_end_price: Some(100200000),
            auction_duration: Some(30),
            direction: PositionDirection::Long,
            ..OrderParams::default()
        };

        let mut order_params_long_after_not_signed = order_params_long_before_not_signed;
        let sanitized = order_params_long_after_not_signed
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();

        assert_eq!(
            order_params_long_after_not_signed.auction_start_price,
            Some(99900000)
        );
        assert_eq!(
            order_params_long_after_not_signed.auction_end_price,
            Some(100200000)
        );
        assert_eq!(
            order_params_long_after_not_signed.auction_duration,
            Some(30)
        );
        assert_eq!(sanitized, true);

        // now short
        let order_params_short_before = OrderParams {
            order_type: OrderType::Market,
            auction_start_price: Some(100000100),
            auction_end_price: Some(99800000),
            auction_duration: Some(30),
            direction: PositionDirection::Short,
            ..OrderParams::default()
        };

        let mut order_params_short_after = order_params_short_before;
        let sanitized = order_params_short_after
            .update_perp_auction_params(&perp_market, oracle_price, true)
            .unwrap();

        assert_eq!(
            order_params_short_after.auction_start_price,
            Some(100000100)
        );
        assert_eq!(order_params_short_after.auction_end_price, Some(99800000));
        assert_eq!(order_params_short_after.auction_duration, Some(30));
        assert_eq!(sanitized, false);

        let order_params_long_before_not_signed = OrderParams {
            order_type: OrderType::Market,
            auction_start_price: Some(100000000),
            auction_end_price: Some(99800000),
            auction_duration: Some(30),
            direction: PositionDirection::Short,
            ..OrderParams::default()
        };

        let mut order_params_long_after_not_signed = order_params_long_before_not_signed;
        let sanitized = order_params_long_after_not_signed
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();

        assert_eq!(
            order_params_long_after_not_signed.auction_start_price,
            Some(100100000)
        );
        assert_eq!(
            order_params_long_after_not_signed.auction_end_price,
            Some(99800000)
        );
        assert_eq!(
            order_params_long_after_not_signed.auction_duration,
            Some(30)
        );
        assert_eq!(sanitized, true);
    }

    #[test]
    fn test_extreme_sanitize_oracle_order_huge_market_prem() {
        let oracle_price = 145 * PRICE_PRECISION_I64;
        let amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,

            ..AMM::default()
        };
        let mut market_stats = MarketStats::default();
        market_stats.last_bid_price_twap = (oracle_price * 15 / 10 - 192988) as u64;
        market_stats.last_mark_price_twap_5min = (oracle_price * 155 / 100) as u64;
        market_stats.last_ask_price_twap = (oracle_price * 16 / 10 + 192988) as u64;
        market_stats.historical_oracle_data.last_oracle_price_twap = oracle_price;
        market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min = oracle_price;
        market_stats.last_mark_price_twap_5min =
            (market_stats.last_ask_price_twap + market_stats.last_bid_price_twap) / 2;

        market_stats.historical_oracle_data.last_oracle_price = oracle_price;
        market_stats.volume_24h = 1_000_000 * QUOTE_PRECISION_U64;
        let perp_market = PerpMarket {
            market_stats,
            amm,
            ..PerpMarket::default()
        };
        let order_params_before = OrderParams {
            order_type: OrderType::Oracle,
            auction_start_price: Some(market_stats.last_bid_price_twap as i64),
            auction_end_price: Some((market_stats.last_bid_price_twap + 1000000) as i64),
            auction_duration: Some(30),
            ..OrderParams::default()
        };

        assert_eq!(order_params_before.auction_start_price, Some(217_307_012));
        assert_eq!(order_params_before.auction_end_price, Some(218_307_012));

        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();

        // The start offset is clamped to the tier auction-width band (OtterSec #146). The market is
        // the default HighlySpeculative tier, so the bound is oracle_twap / 5 = 20% = 29_000_000.
        // The raw fast-TWAP offset here is 79_750_000, or 55% above oracle, because the fixture puts
        // the mark TWAP 50-60% above the oracle. Clamping moves the start toward oracle, which is
        // less aggressive for the taker; the end offset is unchanged.
        assert_eq!(order_params_after.auction_start_price, Some(29_000_000));
        assert_eq!(order_params_after.auction_end_price, Some(90_092_988));
        // duration floor paces the requested spread (0.69% -> 42), not the
        // sanitized spread (42% -> clamped 180)
        assert_eq!(order_params_after.auction_duration, Some(42));
    }

    #[test]
    fn test_sanitize_limit() {
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,

            ..AMM::default()
        };
        let mut market_stats = MarketStats::default();
        market_stats.last_bid_price_twap = (oracle_price * 99 / 100) as u64;
        market_stats.last_ask_price_twap = (oracle_price * 101 / 100) as u64;
        market_stats.historical_oracle_data.last_oracle_price_twap = oracle_price;
        market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min = oracle_price;
        market_stats.last_mark_price_twap_5min =
            (market_stats.last_ask_price_twap + market_stats.last_bid_price_twap) / 2;

        market_stats.historical_oracle_data.last_oracle_price = oracle_price;
        market_stats.volume_24h = 1_000_000 * QUOTE_PRECISION_U64;
        let mut perp_market = PerpMarket {
            market_stats,
            amm,
            ..PerpMarket::default()
        };
        let order_params_before = OrderParams {
            order_type: OrderType::Limit,
            auction_duration: Some(0),
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(order_params_before, order_params_after);

        let order_params_before = OrderParams {
            order_type: OrderType::Limit,
            auction_duration: None,
            post_only: PostOnlyParam::MustPostOnly,
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(order_params_before, order_params_after);

        let order_params_before = OrderParams {
            order_type: OrderType::Limit,
            auction_duration: None,
            post_only: PostOnlyParam::None,
            bit_flags: 1,
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(order_params_before, order_params_after);

        let order_params_before = OrderParams {
            order_type: OrderType::Limit,
            auction_duration: None,
            post_only: PostOnlyParam::None,
            bit_flags: 0,
            oracle_price_offset: Some(0),
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(order_params_before, order_params_after);

        let order_params_before = OrderParams {
            order_type: OrderType::Limit,
            auction_duration: None,
            post_only: PostOnlyParam::None,
            bit_flags: 0,
            oracle_price_offset: None,
            price: 0,
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(order_params_before, order_params_after);

        let order_params_before = OrderParams {
            order_type: OrderType::Limit,
            auction_duration: None,
            post_only: PostOnlyParam::None,
            bit_flags: 0,
            oracle_price_offset: None,
            price: 100 * PRICE_PRECISION_U64,
            direction: PositionDirection::Long,
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(order_params_before, order_params_after);

        let order_params_before = OrderParams {
            order_type: OrderType::Limit,
            auction_duration: None,
            post_only: PostOnlyParam::None,
            bit_flags: 0,
            oracle_price_offset: None,
            price: 102 * PRICE_PRECISION_U64,
            direction: PositionDirection::Long,
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_ne!(order_params_before, order_params_after);
        assert_eq!(order_params_after.auction_duration, Some(120));
        assert_eq!(
            order_params_after.auction_start_price,
            Some(100 * PRICE_PRECISION_I64)
        );
        assert_eq!(
            order_params_after.auction_end_price,
            Some(102 * PRICE_PRECISION_I64)
        );

        let order_params_before = OrderParams {
            order_type: OrderType::Limit,
            auction_duration: None,
            post_only: PostOnlyParam::None,
            bit_flags: 0,
            oracle_price_offset: None,
            price: 100 * PRICE_PRECISION_U64,
            direction: PositionDirection::Short,
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(order_params_before, order_params_after);

        let order_params_before = OrderParams {
            order_type: OrderType::Limit,
            auction_duration: None,
            post_only: PostOnlyParam::None,
            bit_flags: 0,
            oracle_price_offset: None,
            price: 98 * PRICE_PRECISION_U64,
            direction: PositionDirection::Short,
            ..OrderParams::default()
        };

        // tighten bid/ask to mark twap 5min to activate buffer
        perp_market.market_stats.last_bid_price_twap =
            market_stats.last_mark_price_twap_5min - 100000;
        perp_market.market_stats.last_ask_price_twap =
            market_stats.last_mark_price_twap_5min + 100000;
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_ne!(order_params_before, order_params_after);
        assert_eq!(
            order_params_after.auction_start_price,
            Some(100 * PRICE_PRECISION_I64 + 100000) // a bit more passive than mid
        );
        assert_eq!(
            order_params_after.auction_end_price,
            Some(98 * PRICE_PRECISION_I64)
        );
        assert_eq!(order_params_after.auction_duration, Some(126));
    }

    #[test]
    fn sanitize_ignores_an_oracle_offset_limit() {
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,

            ..AMM::default()
        };
        let mut market_stats = MarketStats::default();
        market_stats.last_bid_price_twap = (oracle_price * 999 / 1000) as u64;
        market_stats.last_mark_price_twap_5min = oracle_price as u64;
        market_stats.last_ask_price_twap = (oracle_price * 1001 / 1000) as u64;
        market_stats.historical_oracle_data.last_oracle_price_twap = oracle_price;
        market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min = oracle_price;

        market_stats.historical_oracle_data.last_oracle_price = oracle_price;
        market_stats.volume_24h = 1_000_000 * QUOTE_PRECISION_U64;
        let perp_market = PerpMarket {
            market_stats,
            amm,
            ..PerpMarket::default()
        };
        // The sanitizer prices a limit auction off the fixed limit price only.
        // A limit order with an oracle offset has no fixed price, so the pass
        // leaves it untouched. Validation then refuses it
        // (InvalidOrderOracleOffset).
        for (direction, offset) in [
            (PositionDirection::Long, PRICE_PRECISION_I64 * 10),
            (PositionDirection::Short, -PRICE_PRECISION_I64 * 10),
        ] {
            let order_params_before = OrderParams {
                order_type: OrderType::Limit,
                auction_duration: None,
                post_only: PostOnlyParam::None,
                bit_flags: 0,
                oracle_price_offset: Some(offset),
                price: 0,
                direction,
                ..OrderParams::default()
            };
            let mut order_params_after = order_params_before;
            order_params_after
                .update_perp_auction_params(&perp_market, oracle_price, false)
                .unwrap();
            assert_eq!(order_params_before, order_params_after);
        }
    }

    #[test]
    fn test_market_sanitize() {
        let oracle_price = 99 * PRICE_PRECISION_I64;
        let amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 99 * PEG_PRECISION,

            ..AMM::default()
        };
        let mut market_stats = MarketStats::default();
        market_stats.historical_oracle_data.last_oracle_price = oracle_price;
        market_stats.historical_oracle_data.last_oracle_price_twap = oracle_price - 97238;
        market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min = oracle_price - 97238;
        market_stats.last_ask_price_twap =
            (market_stats.historical_oracle_data.last_oracle_price_twap as u64) + 217999;
        market_stats.last_bid_price_twap =
            (market_stats.historical_oracle_data.last_oracle_price_twap as u64) + 17238;
        market_stats.last_mark_price_twap_5min =
            (market_stats.last_ask_price_twap + market_stats.last_bid_price_twap) / 2;

        market_stats.volume_24h = 1_000_000 * QUOTE_PRECISION_U64;
        let mut perp_market = PerpMarket {
            market_stats,
            amm,
            contract_tier: ContractTier::B,
            ..PerpMarket::default()
        };
        let order_params_before = OrderParams {
            order_type: OrderType::Market,
            direction: PositionDirection::Long,
            auction_start_price: Some(103 * PRICE_PRECISION_I64),
            auction_end_price: Some(104 * PRICE_PRECISION_I64),
            price: 104 * PRICE_PRECISION_U64,
            auction_duration: Some(1),

            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_ne!(order_params_before, order_params_after);
        assert_eq!(order_params_after.auction_start_price.unwrap(), 99017238);
        let amm_bid_price = amm.bid_price(amm.reserve_price().unwrap(), 0, 0).unwrap();
        // Legacy: 98010000 (with cached spread); now bid = reserve_price
        // because no spread is provided.
        assert_eq!(amm_bid_price, 99000000);
        assert!(order_params_after.auction_start_price.unwrap() as u64 > amm_bid_price);

        let order_params_before = OrderParams {
            order_type: OrderType::Market,
            direction: PositionDirection::Short,
            auction_start_price: Some(98 * PRICE_PRECISION_I64),
            auction_end_price: Some(95 * PRICE_PRECISION_I64),
            price: 94 * PRICE_PRECISION_U64,
            auction_duration: Some(11),

            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_ne!(order_params_before, order_params_after);
        assert_eq!(order_params_after.auction_start_price.unwrap(), 99217999);

        // skip for prelaunch oracle
        perp_market.oracle_source = OracleSource::Prelaunch;
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(
            order_params_after.auction_start_price,
            order_params_before.auction_start_price
        );
        assert_eq!(
            order_params_after.auction_end_price,
            order_params_before.auction_end_price
        );

        perp_market.contract_tier = ContractTier::B; // switch back

        let order_params_before = OrderParams {
            order_type: OrderType::Market,
            direction: PositionDirection::Short,
            auction_start_price: Some(103 * PRICE_PRECISION_I64),
            auction_end_price: Some(104 * PRICE_PRECISION_I64),
            price: 104 * PRICE_PRECISION_U64,
            auction_duration: Some(1),

            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(
            order_params_before.auction_start_price,
            order_params_after.auction_start_price
        );
        assert_eq!(
            Some(order_params_before.price as i64),
            order_params_after.auction_end_price
        );
        assert_eq!(order_params_before.direction, order_params_after.direction);

        assert_eq!(order_params_after.auction_duration, Some(102));
    }

    #[test]
    fn test_oracle_market_sanitize() {
        let oracle_price = 99 * PRICE_PRECISION_I64;
        let amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,

            ..AMM::default()
        };
        let mut market_stats = MarketStats::default();
        market_stats.historical_oracle_data.last_oracle_price = oracle_price;
        market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min = oracle_price - 97238;
        market_stats.historical_oracle_data.last_oracle_price_twap = oracle_price - 97238;
        market_stats.last_ask_price_twap =
            (market_stats.historical_oracle_data.last_oracle_price_twap as u64) + 217999;
        market_stats.last_bid_price_twap =
            (market_stats.historical_oracle_data.last_oracle_price_twap as u64) + 17238;
        market_stats.last_mark_price_twap_5min =
            (market_stats.last_ask_price_twap + market_stats.last_bid_price_twap) / 2;
        market_stats.volume_24h = 1_000_000 * QUOTE_PRECISION_U64;
        let perp_market = PerpMarket {
            market_stats,
            amm,
            contract_tier: ContractTier::B,
            ..PerpMarket::default()
        };
        let order_params_before = OrderParams {
            order_type: OrderType::Oracle,
            direction: PositionDirection::Long,
            auction_start_price: Some(4 * PRICE_PRECISION_I64),
            auction_end_price: Some(5 * PRICE_PRECISION_I64),
            price: 5 * PRICE_PRECISION_U64,
            auction_duration: Some(8),

            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_ne!(order_params_before, order_params_after);
        assert_eq!(order_params_after.auction_start_price.unwrap(), 17238);
        // legacy: 2196053
        assert_eq!(order_params_after.auction_end_price.unwrap(), 316901);

        let order_params_before = OrderParams {
            order_type: OrderType::Oracle,
            direction: PositionDirection::Short,
            auction_start_price: Some(4 * PRICE_PRECISION_I64),
            auction_end_price: Some(5 * PRICE_PRECISION_I64),
            price: 5 * PRICE_PRECISION_U64,
            auction_duration: Some(8),

            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(
            order_params_before.auction_start_price,
            order_params_after.auction_start_price
        );
        assert_eq!(
            order_params_before.auction_end_price,
            order_params_after.auction_end_price
        );
        assert_eq!(order_params_before.direction, order_params_after.direction);

        assert_ne!(
            order_params_before.auction_duration,
            order_params_after.auction_duration
        );

        // Oracle market params are fine
        let order_params_before = OrderParams {
            order_type: OrderType::Oracle,
            direction: PositionDirection::Short,
            auction_start_price: Some(99 * PRICE_PRECISION_I64),
            auction_end_price: Some(100 * PRICE_PRECISION_I64),
            price: 100 * PRICE_PRECISION_U64,
            auction_duration: Some(102),

            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        let sanitized = order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(sanitized, false,);
    }

    #[test]
    fn test_market_sanatize_no_auction_params() {
        let oracle_price = 99 * PRICE_PRECISION_I64;
        let amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,

            ..AMM::default()
        };
        let mut market_stats = MarketStats::default();
        market_stats.historical_oracle_data.last_oracle_price = oracle_price;
        market_stats.historical_oracle_data.last_oracle_price_twap = oracle_price - 97238;
        market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min = oracle_price - 97238;
        market_stats.last_ask_price_twap =
            (market_stats.historical_oracle_data.last_oracle_price_twap as u64) + 217999;
        market_stats.last_bid_price_twap =
            (market_stats.historical_oracle_data.last_oracle_price_twap as u64) + 17238;
        market_stats.last_mark_price_twap_5min =
            (market_stats.last_ask_price_twap + market_stats.last_bid_price_twap) / 2;

        market_stats.volume_24h = 1_000_000 * QUOTE_PRECISION_U64;
        market_stats.min_order_size = 1;
        let perp_market = PerpMarket {
            market_stats,
            amm,
            contract_tier: ContractTier::Speculative,
            order_step_size: 1,
            order_tick_size: 1,
            ..PerpMarket::default()
        };
        let order_params_before = OrderParams {
            order_type: OrderType::Market,
            direction: PositionDirection::Long,
            auction_start_price: None,
            auction_end_price: None,
            price: 104 * PRICE_PRECISION_U64,
            auction_duration: None,

            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_ne!(order_params_before, order_params_after);
        assert_eq!(order_params_after.auction_start_price.unwrap(), 98769738);

        let order_params_before = OrderParams {
            order_type: OrderType::Market,
            direction: PositionDirection::Long,
            auction_start_price: None,
            auction_end_price: None,
            price: 99 * PRICE_PRECISION_U64,
            auction_duration: None,

            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_ne!(order_params_before, order_params_after);
        assert_eq!(
            order_params_after.auction_start_price.unwrap(),
            (99 * PRICE_PRECISION_I64 - oracle_price / 400 + 17238) // approx equal with some noise
        );

        let order_params_before = OrderParams {
            order_type: OrderType::Market,
            direction: PositionDirection::Short,
            auction_start_price: None,
            auction_end_price: None,
            price: 94 * PRICE_PRECISION_U64,
            auction_duration: None,

            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(
            order_params_after.auction_start_price.unwrap(),
            99118879 + oracle_price / 400 + 99120
        );

        let order_params_before = OrderParams {
            order_type: OrderType::Market,
            direction: PositionDirection::Short,
            auction_start_price: None,
            auction_end_price: None,
            price: 99 * PRICE_PRECISION_U64 + 100000,
            auction_duration: None,

            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_ne!(order_params_before, order_params_after);
        assert_eq!(
            order_params_after.auction_start_price.unwrap(),
            (99 * PRICE_PRECISION_U64 + 100000) as i64 + oracle_price / 400 + 117999 // use limit price and oracle buffer with some noise
        );

        let order_params_before = OrderParams {
            order_type: OrderType::Market,
            direction: PositionDirection::Short,
            auction_start_price: None,
            auction_end_price: None,
            price: 0,
            auction_duration: None,

            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(
            order_params_after.auction_start_price.unwrap(),
            99118879 + oracle_price / 400 + 99120
        );
        assert_eq!(order_params_after.auction_end_price.unwrap(), 98028211);

        assert_eq!(order_params_after.auction_duration, Some(88));

        let order_params_before = OrderParams {
            order_type: OrderType::Market,
            direction: PositionDirection::Long,
            auction_start_price: None,
            auction_end_price: None,
            price: 0,
            auction_duration: None,

            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(
            order_params_after.auction_start_price.unwrap(),
            98901080 - oracle_price / 400 + 116158
        );
        assert_eq!(order_params_after.auction_end_price.unwrap(), 100207026);

        assert_eq!(order_params_after.auction_duration, Some(88));
    }

    #[test]
    fn test_oracle_market_sanitize_no_auction_params() {
        let oracle_price = 99 * PRICE_PRECISION_I64;
        let amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,
            ..AMM::default()
        };
        let mut market_stats = MarketStats::default();
        market_stats.historical_oracle_data.last_oracle_price = oracle_price;
        market_stats.historical_oracle_data.last_oracle_price_twap = oracle_price - 97238;
        market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min =
            market_stats.historical_oracle_data.last_oracle_price_twap;

        let ask_twap_offset = 217999;
        market_stats.last_ask_price_twap =
            (market_stats.historical_oracle_data.last_oracle_price_twap as u64) + ask_twap_offset;

        let bid_twap_offset = 17238;
        market_stats.last_bid_price_twap =
            (market_stats.historical_oracle_data.last_oracle_price_twap as u64) + bid_twap_offset;

        market_stats.last_mark_price_twap_5min =
            (market_stats.historical_oracle_data.last_oracle_price_twap as u64)
                + (17238 + 217999) / 2;

        market_stats.volume_24h = 1_000_000 * QUOTE_PRECISION_U64;
        market_stats.min_order_size = 1;
        let perp_market = PerpMarket {
            market_stats,
            amm,
            contract_tier: ContractTier::Speculative,
            order_step_size: 1,
            order_tick_size: 1,
            ..PerpMarket::default()
        };
        let order_params_before = OrderParams {
            order_type: OrderType::Oracle,
            direction: PositionDirection::Long,
            auction_start_price: None,
            auction_end_price: None,
            oracle_price_offset: Some(5 * PRICE_PRECISION_I64),
            auction_duration: None,

            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_ne!(order_params_before, order_params_after);
        assert_eq!(order_params_after.auction_start_price.unwrap(), -230262);
        // 25 bps buffer; spread reserves are no longer cached, so the
        // computed `auction_start_price` now lands exactly at the buffer
        // boundary rather than slightly inside it. Relax `>` to `>=`.
        assert!(
            order_params_after.auction_start_price.unwrap()
                >= (bid_twap_offset as i64) - oracle_price / 400
        );
        assert_eq!(
            order_params_after.auction_end_price.unwrap(),
            order_params_before.oracle_price_offset.unwrap()
        );

        let order_params_before = OrderParams {
            order_type: OrderType::Oracle,
            direction: PositionDirection::Long,
            auction_start_price: None,
            auction_end_price: None,
            oracle_price_offset: None,
            auction_duration: None,

            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_ne!(order_params_before, order_params_after);
        assert_eq!(
            order_params_after.auction_start_price.unwrap(),
            17238 - oracle_price / 400
        );
        assert_eq!(order_params_after.auction_end_price.unwrap(), 1207026);
        assert_eq!(order_params_after.oracle_price_offset, None);

        // test sanitize laxing on stale/mismatched mark/oracle twap timestamps

        // not too late, should be the same
        market_stats
            .historical_oracle_data
            .last_oracle_price_twap_ts = 17000000;
        market_stats.last_mark_price_twap_ts = 17000000 - 55;
        let mut order_params_after_2 = order_params_before;
        order_params_after_2
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(
            order_params_after.auction_start_price.unwrap(),
            order_params_after_2.auction_start_price.unwrap()
        );
        assert_eq!(
            order_params_after.auction_end_price.unwrap(),
            order_params_after_2.auction_end_price.unwrap()
        );
        assert_eq!(
            order_params_after.auction_duration.unwrap(),
            order_params_after_2.auction_duration.unwrap()
        );

        // test sanitize skip on stale/mismatched mark/oracle twap timestamps
        market_stats
            .historical_oracle_data
            .last_oracle_price_twap_ts = 17000000;
        market_stats.last_mark_price_twap_ts = 17000000 - 65;
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(
            order_params_after.auction_start_price.unwrap(),
            17238 - oracle_price / 400
        );
        assert_eq!(order_params_after.auction_end_price.unwrap(), 1207026);

        // test sanitize skip on low volume
        market_stats
            .historical_oracle_data
            .last_oracle_price_twap_ts = 17000000;
        market_stats.last_mark_price_twap_ts = market_stats
            .historical_oracle_data
            .last_oracle_price_twap_ts;
        market_stats.volume_24h = 183953; // under $1
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(
            order_params_after.auction_start_price.unwrap(),
            17238 - oracle_price / 400
        );
        assert_eq!(order_params_after.auction_end_price.unwrap(), 1207026);

        // test empty
        let order_params_before = OrderParams {
            order_type: OrderType::Oracle,
            direction: PositionDirection::Short,
            auction_start_price: None,
            auction_end_price: None,
            oracle_price_offset: Some(-5 * PRICE_PRECISION_I64),
            auction_duration: None,

            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(
            order_params_after.auction_start_price.unwrap(),
            217999 + oracle_price / 400
        );
        // 25 bps buffer; auction_start_price now lands exactly at the
        // buffer boundary, so relax `<` to `<=`.
        assert!(
            order_params_after.auction_start_price.unwrap()
                <= (ask_twap_offset as i64) + oracle_price / 400
        );
        assert_eq!(
            order_params_after.auction_end_price.unwrap(),
            order_params_before.oracle_price_offset.unwrap()
        );
        assert_eq!(order_params_after.auction_duration.unwrap(), 180);

        let order_params_before = OrderParams {
            order_type: OrderType::Oracle,
            direction: PositionDirection::Short,
            auction_start_price: None,
            auction_end_price: None,
            oracle_price_offset: None,
            auction_duration: None,

            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(
            order_params_after.auction_start_price.unwrap(),
            217999 + oracle_price / 400
        );
        assert_eq!(order_params_after.auction_end_price.unwrap(), -971789);
        assert_eq!(order_params_after.auction_duration.unwrap(), 88);
    }

    /// Tail-tier market (wide baseline below oracle for longs): stats shaped so
    /// the baseline start offset for a long is ~-1.5% of oracle.
    fn tail_market_long_baseline_below_oracle(oracle_price: i64) -> PerpMarket {
        let amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,
            ..AMM::default()
        };
        let mut market_stats = MarketStats::default();
        market_stats.historical_oracle_data.last_oracle_price = oracle_price;
        market_stats.historical_oracle_data.last_oracle_price_twap = oracle_price;
        market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min = oracle_price;
        // long baseline start offset = min(bid_twap - oracle_twap, mark_5min - oracle_5min)
        market_stats.last_bid_price_twap = (oracle_price - 1_500_000) as u64;
        market_stats.last_ask_price_twap = (oracle_price + 1_500_000) as u64;
        market_stats.last_mark_price_twap_5min = (oracle_price - 1_400_000) as u64;
        market_stats.volume_24h = 1_000_000 * QUOTE_PRECISION_U64;
        PerpMarket {
            market_stats,
            amm,
            contract_tier: ContractTier::C,
            ..PerpMarket::default()
        }
    }

    /// Tail-tier market shaped so the baseline start offset for a short is
    /// ~+1.5% of oracle.
    fn tail_market_short_baseline_above_oracle(oracle_price: i64) -> PerpMarket {
        let mut perp_market = tail_market_long_baseline_below_oracle(oracle_price);
        perp_market.market_stats.last_mark_price_twap_5min = (oracle_price + 1_400_000) as u64;
        perp_market
    }

    #[test]
    fn test_signed_msg_tail_market_duration_floor_uses_requested_spread() {
        // HYPE scenario: signed-msg market order on a tail-tier market with a
        // tight requested auction. Sanitization improves the start toward
        // baseline; the duration floor must pace the requested spread, not the
        // widened one.
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let perp_market = tail_market_long_baseline_below_oracle(oracle_price);

        let order_params_before = OrderParams {
            order_type: OrderType::Market,
            direction: PositionDirection::Long,
            auction_start_price: Some(100_050_000), // +0.05%
            auction_end_price: Some(100_250_000),   // +0.25%
            price: 100_250_000,
            auction_duration: Some(20),
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        let sanitized = order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, true)
            .unwrap();
        assert_eq!(sanitized, true);

        // start improved to baseline, end untouched
        assert_eq!(order_params_after.auction_start_price, Some(98_500_000));
        assert_eq!(order_params_after.auction_end_price, Some(100_250_000));

        // requested spread 0.2% -> floor 12, grace |20 - 12| <= 10 holds:
        // client duration survives. Flooring on the sanitized spread (1.75%
        // -> 105) would have produced 105.
        assert_eq!(order_params_after.auction_duration, Some(20));
    }

    #[test]
    fn test_signed_msg_tail_market_no_mutation_is_noop() {
        // Prices inside thresholds and requested-spread floor within grace:
        // the order must pass through untouched.
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let perp_market = tail_market_long_baseline_below_oracle(oracle_price);

        let order_params_before = OrderParams {
            order_type: OrderType::Market,
            direction: PositionDirection::Long,
            auction_start_price: Some(98_550_000), // -1.45%, inside baseline + 0.1% grace
            auction_end_price: Some(99_000_000),   // -1.0%
            price: 99_000_000,
            auction_duration: Some(20),
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        let sanitized = order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, true)
            .unwrap();

        assert_eq!(sanitized, false);
        assert_eq!(order_params_before, order_params_after);
    }

    #[test]
    fn test_signed_msg_tail_market_short_end_mutation_floors_on_sanitized_spread() {
        // Short with a fat-finger end far below baseline: sanitization pulls
        // the end up (narrows the range). The floor paces the narrower
        // sanitized spread, same as before the requested-spread change.
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let perp_market = tail_market_short_baseline_above_oracle(oracle_price);

        let order_params_before = OrderParams {
            order_type: OrderType::Market,
            direction: PositionDirection::Short,
            auction_start_price: Some(101_500_000), // at baseline
            auction_end_price: Some(95_000_000),    // -5%, far past baseline end
            price: 95_000_000,
            auction_duration: Some(20),
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        let sanitized = order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, true)
            .unwrap();
        assert_eq!(sanitized, true);

        // start untouched, end pulled up toward baseline
        assert_eq!(order_params_after.auction_start_price, Some(101_500_000));
        assert!(order_params_after.auction_end_price.unwrap() > 95_000_000);

        // sanitized spread is narrower than the requested 6.5%; the floor must
        // come from the sanitized spread (i.e. unchanged legacy behavior)
        let sanitized_spread = (order_params_after.auction_start_price.unwrap()
            - order_params_after.auction_end_price.unwrap())
        .unsigned_abs();
        let expected_floor = crate::state::order_params::get_auction_duration(
            sanitized_spread,
            oracle_price.unsigned_abs(),
            ContractTier::C,
        )
        .unwrap();
        assert_eq!(
            order_params_after.auction_duration,
            Some(expected_floor.max(20))
        );
    }

    #[test]
    fn test_signed_msg_duration_grace_boundary() {
        // Grace: the floor only overrides a signed-msg duration when it
        // differs by more than 10 slots. Prices stay inside thresholds so the
        // requested spread is the floor input.
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let perp_market = tail_market_long_baseline_below_oracle(oracle_price);

        // spread 0.5% -> floor exactly 30; |20 - 30| = 10 -> kept
        let order_params_before = OrderParams {
            order_type: OrderType::Market,
            direction: PositionDirection::Long,
            auction_start_price: Some(98_550_000),
            auction_end_price: Some(99_050_000),
            price: 99_050_000,
            auction_duration: Some(20),
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, true)
            .unwrap();
        assert_eq!(order_params_after.auction_duration, Some(20));

        // spread 0.52% -> floor 32; |20 - 32| = 12 > 10 -> floored to 32
        let order_params_before = OrderParams {
            order_type: OrderType::Market,
            direction: PositionDirection::Long,
            auction_start_price: Some(98_550_000),
            auction_end_price: Some(99_070_000),
            price: 99_070_000,
            auction_duration: Some(20),
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, true)
            .unwrap();
        assert_eq!(order_params_after.auction_duration, Some(32));

        // non-signed orders get no grace: floor 30 applies at the same spread
        let order_params_before = OrderParams {
            order_type: OrderType::Market,
            direction: PositionDirection::Long,
            auction_start_price: Some(98_550_000),
            auction_end_price: Some(99_050_000),
            price: 99_050_000,
            auction_duration: Some(20),
            ..OrderParams::default()
        };
        let mut order_params_after = order_params_before;
        order_params_after
            .update_perp_auction_params(&perp_market, oracle_price, false)
            .unwrap();
        assert_eq!(order_params_after.auction_duration, Some(30));
    }
}

mod get_close_perp_params {
    use {
        crate::{
            math::{orders::get_posted_slot_from_clock_slot, time::SlotClock},
            state::{
                oracle::HistoricalOracleData,
                order_params::PostOnlyParam,
                perp_market::{MarketStats, PerpMarket, AMM},
                user::{Order, OrderStatus},
            },
            test_utils::create_account_info,
            validation::order::validate_order,
            ContractTier, OrderParams, PositionDirection, BASE_PRECISION_U64, PRICE_PRECISION_I64,
            PRICE_PRECISION_U64, QUOTE_PRECISION_U64,
        },
        anchor_lang::prelude::AccountLoader,
        solana_program::pubkey::Pubkey,
        std::str::FromStr,
    };

    #[test]
    fn bid() {
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let slot = 1;
        let amm = AMM {
            ..AMM::default_test()
        };
        let perp_market = PerpMarket {
            amm,
            market_stats: MarketStats {
                min_order_size: 1,
                mark_std: PRICE_PRECISION_U64,
                oracle_std: PRICE_PRECISION_U64,

                last_ask_price_twap: 101 * PRICE_PRECISION_U64,

                last_bid_price_twap: 99 * PRICE_PRECISION_U64,

                last_mark_price_twap_5min: 99 * PRICE_PRECISION_U64,

                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: 100 * PRICE_PRECISION_I64,
                    last_oracle_price_twap_5min: 100 * PRICE_PRECISION_I64,

                    ..HistoricalOracleData::default()
                },

                volume_24h: 1_000_000 * QUOTE_PRECISION_U64,
                ..MarketStats::default()
            },
            contract_tier: ContractTier::Speculative,
            order_step_size: 1,
            order_tick_size: 1,
            ..PerpMarket::default()
        };
        let direction_to_close = PositionDirection::Long;
        let base_asset_amount = BASE_PRECISION_U64;

        let params =
            OrderParams::get_close_perp_params(&perp_market, direction_to_close, base_asset_amount)
                .unwrap();

        let auction_start_price = params.auction_start_price.unwrap();
        let auction_end_price = params.auction_end_price.unwrap();
        let oracle_price_offset = params.oracle_price_offset.unwrap();
        assert_eq!(auction_start_price, -1000000);
        assert_eq!(auction_end_price, 2 * PRICE_PRECISION_I64);
        assert_eq!(oracle_price_offset, 2 * PRICE_PRECISION_I64);

        let order = get_order(&params, slot);

        validate_order(
            &order,
            &perp_market,
            Some(oracle_price),
            slot,
            SlotClock::baseline(),
        )
        .unwrap();

        let amm = AMM {
            ..AMM::default_test()
        };
        let perp_market = PerpMarket {
            amm,
            market_stats: MarketStats {
                min_order_size: 1,
                mark_std: PRICE_PRECISION_U64,
                oracle_std: PRICE_PRECISION_U64,

                last_ask_price_twap: 103 * PRICE_PRECISION_U64,

                last_bid_price_twap: 101 * PRICE_PRECISION_U64,

                last_mark_price_twap_5min: 102 * PRICE_PRECISION_U64,

                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: 100 * PRICE_PRECISION_I64,
                    last_oracle_price_twap_5min: 100 * PRICE_PRECISION_I64,
                    ..HistoricalOracleData::default()
                },

                volume_24h: 1_000_000 * QUOTE_PRECISION_U64,
                ..MarketStats::default()
            },
            contract_tier: ContractTier::Speculative,
            order_step_size: 1,
            order_tick_size: 1,
            ..PerpMarket::default()
        };
        let params =
            OrderParams::get_close_perp_params(&perp_market, direction_to_close, base_asset_amount)
                .unwrap();

        let auction_start_price = params.auction_start_price.unwrap();
        let auction_end_price = params.auction_end_price.unwrap();
        let oracle_price_offset = params.oracle_price_offset.unwrap();
        assert_eq!(auction_start_price, 2 * PRICE_PRECISION_I64);
        assert_eq!(auction_end_price, 4 * PRICE_PRECISION_I64);
        assert_eq!(oracle_price_offset, 4 * PRICE_PRECISION_I64);

        let order = get_order(&params, slot);

        validate_order(
            &order,
            &perp_market,
            Some(oracle_price),
            slot,
            SlotClock::baseline(),
        )
        .unwrap();

        let amm = AMM {
            ..AMM::default_test()
        };
        let perp_market = PerpMarket {
            amm,
            market_stats: MarketStats {
                min_order_size: 1,
                mark_std: PRICE_PRECISION_U64,
                oracle_std: PRICE_PRECISION_U64,

                last_ask_price_twap: 99 * PRICE_PRECISION_U64,

                last_bid_price_twap: 97 * PRICE_PRECISION_U64,

                last_mark_price_twap_5min: 98 * PRICE_PRECISION_U64,

                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: 100 * PRICE_PRECISION_I64,
                    last_oracle_price_twap_5min: 100 * PRICE_PRECISION_I64,
                    ..HistoricalOracleData::default()
                },

                volume_24h: 1_000_000 * QUOTE_PRECISION_U64,
                ..MarketStats::default()
            },
            contract_tier: ContractTier::Speculative,
            order_step_size: 1,
            order_tick_size: 1,
            ..PerpMarket::default()
        };
        let params =
            OrderParams::get_close_perp_params(&perp_market, direction_to_close, base_asset_amount)
                .unwrap();

        let auction_start_price = params.auction_start_price.unwrap();
        let auction_end_price = params.auction_end_price.unwrap();
        let oracle_price_offset = params.oracle_price_offset.unwrap();
        assert_eq!(auction_start_price, -2 * PRICE_PRECISION_I64);
        assert_eq!(auction_end_price, 0);
        assert_eq!(oracle_price_offset, 0);

        let order = get_order(&params, slot);

        validate_order(
            &order,
            &perp_market,
            Some(oracle_price),
            slot,
            SlotClock::baseline(),
        )
        .unwrap();
    }

    #[test]
    fn ask() {
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let slot = 1;
        let amm = AMM {
            ..AMM::default_test()
        };
        let perp_market = PerpMarket {
            amm,
            market_stats: MarketStats {
                min_order_size: 1,
                mark_std: PRICE_PRECISION_U64,
                oracle_std: PRICE_PRECISION_U64,

                last_ask_price_twap: 101 * PRICE_PRECISION_U64,

                last_bid_price_twap: 99 * PRICE_PRECISION_U64,

                last_mark_price_twap_5min: 100 * PRICE_PRECISION_U64,

                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: 100 * PRICE_PRECISION_I64,
                    last_oracle_price_twap_5min: 100 * PRICE_PRECISION_I64,
                    ..HistoricalOracleData::default()
                },

                volume_24h: 1_000_000 * QUOTE_PRECISION_U64,
                ..MarketStats::default()
            },
            contract_tier: ContractTier::Speculative,
            order_step_size: 1,
            order_tick_size: 1,
            ..PerpMarket::default()
        };
        let direction_to_close = PositionDirection::Short;
        let base_asset_amount = BASE_PRECISION_U64;

        let params =
            OrderParams::get_close_perp_params(&perp_market, direction_to_close, base_asset_amount)
                .unwrap();

        let auction_start_price = params.auction_start_price.unwrap();
        let auction_end_price = params.auction_end_price.unwrap();
        let oracle_price_offset = params.oracle_price_offset.unwrap();
        assert_eq!(auction_start_price, 0);
        assert_eq!(auction_end_price, -2 * PRICE_PRECISION_I64);
        assert_eq!(oracle_price_offset, -2 * PRICE_PRECISION_I64);

        let order = get_order(&params, slot);

        validate_order(
            &order,
            &perp_market,
            Some(oracle_price),
            slot,
            SlotClock::baseline(),
        )
        .unwrap();

        let amm = AMM {
            ..AMM::default_test()
        };
        let perp_market = PerpMarket {
            amm,
            market_stats: MarketStats {
                min_order_size: 1,
                mark_std: PRICE_PRECISION_U64,
                oracle_std: PRICE_PRECISION_U64,

                last_ask_price_twap: 103 * PRICE_PRECISION_U64,

                last_bid_price_twap: 101 * PRICE_PRECISION_U64,

                last_mark_price_twap_5min: 102 * PRICE_PRECISION_U64,

                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: 100 * PRICE_PRECISION_I64,
                    last_oracle_price_twap_5min: 100 * PRICE_PRECISION_I64,
                    ..HistoricalOracleData::default()
                },

                volume_24h: 1_000_000 * QUOTE_PRECISION_U64,
                ..MarketStats::default()
            },
            contract_tier: ContractTier::Speculative,
            order_step_size: 1,
            order_tick_size: 1,
            ..PerpMarket::default()
        };
        let params =
            OrderParams::get_close_perp_params(&perp_market, direction_to_close, base_asset_amount)
                .unwrap();

        let auction_start_price = params.auction_start_price.unwrap();
        let auction_end_price = params.auction_end_price.unwrap();
        let oracle_price_offset = params.oracle_price_offset.unwrap();
        assert_eq!(auction_start_price, 2 * PRICE_PRECISION_I64);
        assert_eq!(auction_end_price, 0);
        assert_eq!(oracle_price_offset, 0);

        let order = get_order(&params, slot);

        validate_order(
            &order,
            &perp_market,
            Some(oracle_price),
            slot,
            SlotClock::baseline(),
        )
        .unwrap();

        let amm = AMM {
            ..AMM::default_test()
        };
        let perp_market = PerpMarket {
            amm,
            market_stats: MarketStats {
                min_order_size: 1,
                mark_std: PRICE_PRECISION_U64,
                oracle_std: PRICE_PRECISION_U64,

                last_ask_price_twap: 99 * PRICE_PRECISION_U64,

                last_mark_price_twap_5min: 98 * PRICE_PRECISION_U64,

                last_bid_price_twap: 97 * PRICE_PRECISION_U64,

                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: 100 * PRICE_PRECISION_I64,
                    last_oracle_price_twap_5min: 100 * PRICE_PRECISION_I64,

                    ..HistoricalOracleData::default()
                },

                volume_24h: 1_000_000 * QUOTE_PRECISION_U64,
                ..MarketStats::default()
            },
            contract_tier: ContractTier::Speculative,
            order_step_size: 1,
            order_tick_size: 1,
            ..PerpMarket::default()
        };
        let params =
            OrderParams::get_close_perp_params(&perp_market, direction_to_close, base_asset_amount)
                .unwrap();

        let auction_start_price = params.auction_start_price.unwrap();
        let auction_end_price = params.auction_end_price.unwrap();
        let oracle_price_offset = params.oracle_price_offset.unwrap();
        assert_eq!(auction_start_price, -2 * PRICE_PRECISION_I64);
        assert_eq!(auction_end_price, -4 * PRICE_PRECISION_I64);
        assert_eq!(oracle_price_offset, -4 * PRICE_PRECISION_I64);

        let order = get_order(&params, slot);

        validate_order(
            &order,
            &perp_market,
            Some(oracle_price),
            slot,
            SlotClock::baseline(),
        )
        .unwrap();
    }

    #[test]
    fn btc() {
        let perp_market_str = String::from("Ct8MLGv1N/cV6vWLwJY+18dY2GsrmrNldgnISB7pmbcf7cn9S4FZ4KA0JMEnAAAAAAAAAAAAAADg/mJJ2f///////////////U3ihP3//////////////0p/wecT+f////////////8elGWXkwYAAAAAAAAAAAAAbccyGPz4/////////////+ZmycPDBgAAAAAAAAAAAAAARCk1OgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAANJkd49WBwAAAAAAAAAAAAD30wV0VgcAAAAAAAAAAAAAhqB0KRkAAAAAAAAAAAAAAATX1A4SAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA5i32yLSoX+GmfbRNwS3l2zMPesZrctxliv7fD0pBW0NYpbrsAycBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEJUQy1QRVJQICAgICAgICAgICAgICAgICAgICAgICAgWXIm/v////8AwusLAAAAAAB0O6QLAAAAvz8ZJAAAAACLqJ5lAAAAAKwtgO4AAAAArC2A7gAAAACsLYDuAAAAADKjnmUAAAAAAAAAAAAAAAAAAAAAAAAAAKCGAQAAAAAAoIYBAAAAAAAAypo7AAAAAAAAAAAAAAAAAAAAAAAAAACnDw0AAAAAAPEkAAAAAAAAQB8AAAAAAABMHQAA1DAAAPQBAAAsAQAAAAAAABAnAACnBQAAEQkAAAEAAQAAAAAAtf8AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABroFyHCgAAAIv2go0KAAAAJ6qeZQAAAACyQVqDCgAAACX/XosKAAAAa+0wEAAAAACeJZsPAAAAAAQCAAAAAAAAscrx5+8FAACIP1dQJgAAAEGRyqEnAAAAJ6qeZQAAAACnDEgrAQAAABAOAAAAAAAAIKEHAAAAAAAqGgAAAAAAAAAAAAAAAAAAKHVdAAAAAAB3f72RCgAAAAAAAAABAAAAAAAAAAAAAAB3f72RCgAAAAAAAAAAAAAAAQAAAAAAAADZWSKCCgAAAL91lIgKAAAAJ6qeZQAAAAAAAAAAAAAAAHv6tC3KSgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAZTv3q84BAAAAAAAAAAAAAFZz+LK2BAAAAAAAAAAAAACcYg8AAAAAAAAAAAAAAAAAMu5zessBAAAAAAAAAAAAAOYoXB/TAQAAAAAAAAAAAACu4s8y6wIAAAAAAAAAAAAA7NxuDQQAAAAAAAAAAAAAAGCISRq1BAAAAAAAAAAAAACAM4cKAQAAAAAAAAAAAAAAaxKKoSwAAAAAAAAAAAAAAH/p0cgTAAAAAAAAAAAAAADQH9cHJgAAAAAAAAAAAAAAc132XBgAAAAAAAAAAAAAAD0+XQ4AAAAAAVGB1v////8AAAAAAAAAAAAAAAAAAAAAFAAAACxMAADcBTIAZMgAAAAAAAAAAAAAAAAAAAAAAAA=");
        let mut perp_market_bytes = unsafe {
            crate::test_utils::aligned_account_bytes_from_b64::<PerpMarket>(&perp_market_str)
        };

        let key = Pubkey::default();
        let owner = Pubkey::from_str("vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P").unwrap();
        let mut lamports = 0;
        let perp_market_account_info = create_account_info(
            &key,
            true,
            &mut lamports,
            &mut perp_market_bytes[..],
            &owner,
        );

        let perp_market_loader: AccountLoader<PerpMarket> =
            AccountLoader::try_from(&perp_market_account_info).unwrap();
        let perp_market = perp_market_loader.load_mut().unwrap();

        let oracle_price = perp_market
            .market_stats
            .historical_oracle_data
            .last_oracle_price;
        let slot = 240991856_u64;

        let direction_to_close = PositionDirection::Short;
        let base_asset_amount = BASE_PRECISION_U64;

        let params =
            OrderParams::get_close_perp_params(&perp_market, direction_to_close, base_asset_amount)
                .unwrap();

        let auction_start_price = params.auction_start_price.unwrap();
        let auction_end_price = params.auction_end_price.unwrap();
        let oracle_price_offset = params.oracle_price_offset.unwrap();
        assert_eq!(auction_start_price, 154969420);
        assert_eq!(auction_end_price, -251200914);
        assert_eq!(oracle_price_offset, -251200914);
        assert_eq!(params.auction_duration.unwrap_or(0), 80);

        let order = get_order(&params, slot);

        validate_order(
            &order,
            &perp_market,
            Some(oracle_price),
            slot,
            SlotClock::baseline(),
        )
        .unwrap();
    }

    #[test]
    fn doge() {
        let perp_market_str = String::from("Ct8MLGv1N/cueW7q94VBpwLPordbGCeLrp/R8owsajNEG7L2nvhZ8ACcfFCu/wYAAAAAAAAAAAAAnFHtB0b6////////////BhCDPfz//////////////6bEBnzX//////////////95+qpnJAAAAAAAAAAAAAAAwQyrjdX//////////////33ohvUnAAAAAAAAAAAAAAAAAMFv8oYjAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAMu9tAEAAAAAAAAAAAAAAADLvbQBAAAAAAAAAAAAAAAAvCoJYQEAAAAAAAAAAAAAAApzcx8BAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA3O9Q3QpM0tzBfkXfFnbcszahGmHGnfegKZsBUMZy0lw81GX8Tk8AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAERPR0UtUEVSUCAgICAgICAgICAgICAgICAgICAgICAg5Nyg//////+AlpgAAAAAAAAvaFkAAAAAMZviAQAAAABXpJ5lAAAAAPIkAAAAAAAA8iQAAAAAAADyJAAAAAAAAOuinmUAAAAAAAAAAAAAAAAAAAAAAAAAAACUNXcAAAAACgAAAAAAAAAQJwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAblAAAAAAAABUaAAAAAAAAyAAAAMgAAAAQJwAAqGEAAOgDAAD0AQAAAAAAABAnAADYAAAASQEAAAcAAQACAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA/MAEAAAAAAGgwAQAAAAAAKaueZQAAAAAyLgEAAAAAAE0yAQAAAAAAJQAAAAAAAACVAAAAAAAAADcCAAAAAAAAc3fY9xsAAAD1rzWPAAAAABtgqEAAAAAAdKqeZQAAAADXBgAAAAAAABAOAAAAAAAAAHQ7pAsAAADVAQAAAAAAAAAAAAAAAAAANbUVAAAAAADzLgEAAAAAAAAAAAABAAAAAAAAAAAAAADzLgEAAAAAAAAAAAAAAAAAAQAAAAAAAACILwEAAAAAAEwvAQAAAAAAKaueZQAAAAAAAAAAAAAAAN3NcoxTCwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAz9R4XsYVwAIAAAAAAAAAAHQxpPBqtccCAAAAAAAAAAAM5A8AAAAAAAAAAAAAAAAAKZctsJ5HpQIAAAAAAAAAAOKkHsEtjN4CAAAAAAAAAACJ8xsT+OLDAgAAAAAAAAAAtCsBAAAAAAAAAAAAAAAAAAbWzs8ic8YCAAAAAAAAAAAAOM49tkUBAAAAAAAAAAAAYN95sAoAAAAAAAAAAAAAAAWNdlMJAAAAAAAAAAAAAACQMZk6EwAAAAAAAAAAAAAAnzjKCwEAAAAAAAAAAAAAAIBAXQ4AAAAAYJN7/v////8AAAAAAAAAAAAAAAAAAAAAHCUAAIA4AQD0ATIAZGQAAAAAAAAAAAAAAAAAAAAAAAA=");
        let mut perp_market_bytes = unsafe {
            crate::test_utils::aligned_account_bytes_from_b64::<PerpMarket>(&perp_market_str)
        };

        let key = Pubkey::default();
        let owner = Pubkey::from_str("vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P").unwrap();
        let mut lamports = 0;
        let perp_market_account_info = create_account_info(
            &key,
            true,
            &mut lamports,
            &mut perp_market_bytes[..],
            &owner,
        );

        let perp_market_loader: AccountLoader<PerpMarket> =
            AccountLoader::try_from(&perp_market_account_info).unwrap();
        let perp_market = perp_market_loader.load_mut().unwrap();

        let oracle_price = perp_market
            .market_stats
            .historical_oracle_data
            .last_oracle_price;
        let slot = 240991856_u64;

        let direction_to_close = PositionDirection::Short;
        let base_asset_amount = 100 * BASE_PRECISION_U64;

        let params =
            OrderParams::get_close_perp_params(&perp_market, direction_to_close, base_asset_amount)
                .unwrap();

        let auction_start_price = params.auction_start_price.unwrap();
        let auction_end_price = params.auction_end_price.unwrap();
        let oracle_price_offset = params.oracle_price_offset.unwrap();
        assert_eq!(auction_start_price, 284);
        // legacy: auction_end_price=-1021, oracle_price_offset=-1021
        assert_eq!(auction_end_price, -497);
        assert_eq!(oracle_price_offset, -497);

        let order = get_order(&params, slot);

        validate_order(
            &order,
            &perp_market,
            Some(oracle_price),
            slot,
            SlotClock::baseline(),
        )
        .unwrap();
    }

    fn get_order(params: &OrderParams, slot: u64) -> Order {
        Order {
            status: OrderStatus::Open,
            order_type: params.order_type,
            market_type: params.market_type,
            slot,
            order_id: 1,
            user_order_id: params.user_order_id,
            market_index: params.market_index,
            price: params.price,
            existing_position_direction: PositionDirection::Long,
            base_asset_amount: params.base_asset_amount,
            base_asset_amount_filled: 0,
            quote_asset_amount_filled: 0,
            direction: params.direction,
            reduce_only: params.reduce_only,
            trigger_price: params.trigger_price.unwrap_or(0),
            trigger_condition: params.trigger_condition,
            post_only: params.post_only != PostOnlyParam::None,
            oracle_price_offset: params.oracle_price_offset.unwrap_or(0),
            immediate_or_cancel: params.is_immediate_or_cancel(),
            auction_start_price: params.auction_start_price.unwrap_or(0),
            auction_end_price: params.auction_end_price.unwrap_or(0),
            auction_duration: params.auction_duration.unwrap_or(0),
            max_ts: 100,
            posted_slot_tail: get_posted_slot_from_clock_slot(slot),
            bit_flags: 0,
            padding: [0; 5],
        }
    }

    #[test]
    fn test_default_starts_on_perp_markets() {
        // BTC style market
        // ideally 60 above oracle is fill
        let perp_market_str = String::from("Ct8MLGv1N/cV6vWLwJY+18dY2GsrmrNldgnISB7pmbcf7cn9S4FZ4MCk9S8+AAAAAAAAAAAAAADABV1mwv//////////////gruloUEAAAAAAAAAAAAAAIjHhpPh8//////////////C9GHQvgsAAAAAAAAAAAAApZ+7JMPz/////////////+Wma/v1CwAAAAAAAAAAAAAAoNshXQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAM0UAkibCAAAAAAAAAAAAADyg5AsmwgAAAAAAAAAAAAA8bcNREwAAAAAAAAAAAAAAHvRRkAjAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA5i32yLSoX+GmfbRNwS3l2zMPesZrctxliv7fD0pBW0NWh8juusQCAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEJUQy1QRVJQICAgICAgICAgICAgICAgICAgICAgICAggA8F/f////+A8PoCAAAAAABcsuwiAAAAXd8ZJAAAAAAMo9RlAAAAAHFDcmgAAAAAcUNyaAAAAABxQ3JoAAAAAFWi1GUAAAAAAAAAAAAAAAAAAAAAAAAAAKCGAQAAAAAAoIYBAAAAAAAA4fUFAAAAAAAAAAAAAAAAAAAAAAAAAAC4JxgAAAAAAMMoAAAAAAAAQB8AAAAAAABMHQAA1DAAAPQBAAAsAQAAAAAAABAnAACvDAAA6BYAAAEAAQAAAAAAtf8AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAADGWMgrDAAAAIYLIy8MAAAAGqnUZQAAAADw8e0qDAAAAJ2/oiwMAAAAqXuqAQAAAADzI+4DAAAAAFoCAAAAAAAABeZ6i7gsAAAysGg95QAAAO7ctlC7AAAAGqnUZQAAAACtjtSUAAAAABAOAAAAAAAAIKEHAAAAAAAAAAAArQMAAAAAAAAAAAAAvUntAAAAAABAQrwtDAAAAAAAAAABAAAAAAAAAAAAAABAQrwtDAAAAAAAAAAAAAAAAgAAAAAAAABNHs4pDAAAAKy6Ei0MAAAAGqnUZQAAAAAAAAAAAAAAANc6N0nNZAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAALiOnpdEBAAAAAAAAAAAAAPbvSFTRBgAAAAAAAAAAAACcYg8AAAAAAAAAAAAAAAAAVVPNPM4BAAAAAAAAAAAAAAnhde3VAQAAAAAAAAAAAADOl72AhQMAAAAAAAAAAAAAlsXLPwMAAAAAAAAAAAAAADdkBsLPBgAAAAAAAAAAAACAqlKWAAAAAAAAAAAAAAAArfoPPH4AAAAAAAAAAAAAAM57zZkzAAAAAAAAAAAAAACBrvFTdAAAAAAAAAAAAAAA8tmZKi8AAAAAAAAAAAAAAPnS3A4AAAAAsz4P/v////8AAAAAAAAAAAAAAAAAAAAAMgAAABwlAADcBTIAZMgAAAAAAAAAAAAAAAAAAAAAAAA=");
        let mut perp_market_bytes = unsafe {
            crate::test_utils::aligned_account_bytes_from_b64::<PerpMarket>(&perp_market_str)
        };

        let key = Pubkey::default();
        let owner = Pubkey::from_str("vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P").unwrap();
        let mut lamports = 0;
        let perp_market_account_info = create_account_info(
            &key,
            true,
            &mut lamports,
            &mut perp_market_bytes[..],
            &owner,
        );

        let perp_market_loader: AccountLoader<PerpMarket> =
            AccountLoader::try_from(&perp_market_account_info).unwrap();
        let perp_market = perp_market_loader.load_mut().unwrap();

        let oracle_price = perp_market
            .market_stats
            .historical_oracle_data
            .last_oracle_price;
        let slot = 249352956_u64;
        let base_asset_amount = 100 * BASE_PRECISION_U64;

        let (long_start, long_end) = OrderParams::get_perp_baseline_start_end_price_offset(
            &perp_market,
            PositionDirection::Long,
            1,
        )
        .unwrap();
        // Both start offsets are far inside the tier A band. The market is tier A with an oracle TWAP
        // of 52_240_981_581, so the OtterSec #146 clamp sits at 2% = 1_044_819_631. The real basis
        // here is under 0.1% of price, so the clamp does not bind.
        assert_eq!(long_start, 18863011); // legacy: 25635886 ($25 above)
        assert_eq!(long_end, 113427779); // legacy: 115193672

        let (short_start, short_end) = OrderParams::get_perp_baseline_start_end_price_offset(
            &perp_market,
            PositionDirection::Short,
            1,
        )
        .unwrap();
        assert_eq!(short_start, 47489360); // legacy: 47008307
        assert_eq!(short_end, -47075408);

        let params = OrderParams::get_close_perp_params(
            &perp_market,
            PositionDirection::Long,
            base_asset_amount,
        )
        .unwrap();

        let auction_start_price = params.auction_start_price.unwrap();
        let auction_end_price = params.auction_end_price.unwrap();
        let oracle_price_offset = params.oracle_price_offset.unwrap();
        let auction_duration = params.auction_duration.unwrap();
        assert_eq!(auction_start_price, long_start); // $25 above
        assert_eq!(auction_end_price, long_end); // 115
        assert_eq!(oracle_price_offset, long_end);
        assert_eq!(auction_duration, 80);

        let order = get_order(&params, slot);

        validate_order(
            &order,
            &perp_market,
            Some(oracle_price),
            slot,
            SlotClock::baseline(),
        )
        .unwrap();
    }
}

/// OtterSec #146: the baseline auction start offset is clamped to the tier auction-width band.
///
/// Each test pairs an in-band case with an out-of-band case built the same way, so a passing
/// assertion pins the clamp and not some unrelated bound. The in-band case must return the raw
/// offset unchanged.
mod get_perp_baseline_start_price_offset {
    use crate::{
        state::{
            oracle::HistoricalOracleData,
            perp_market::{ContractTier, MarketStats, PerpMarket, AMM},
        },
        OrderParams, PositionDirection, PRICE_PRECISION_I64, PRICE_PRECISION_U64,
        QUOTE_PRECISION_U64,
    };

    const ORACLE_TWAP: i64 = 100 * PRICE_PRECISION_I64;

    /// Market on the fast-TWAP-only path, the path the crank can move.
    ///
    /// `last_mark_price_twap_5min` is set to `ORACLE_TWAP + mark_5min_premium`, and the bid/ask TWAPs
    /// sit at oracle. That makes the fast and slow offsets diverge by more than 50bps of the 5min
    /// TWAP, so `get_perp_baseline_start_price_offset` returns the fast offset alone.
    fn market_with_mark_5min_premium(
        contract_tier: ContractTier,
        mark_5min_premium: i64,
    ) -> PerpMarket {
        PerpMarket {
            contract_tier,
            market_stats: MarketStats {
                last_bid_price_twap: ORACLE_TWAP as u64,
                last_ask_price_twap: ORACLE_TWAP as u64,
                last_mark_price_twap_5min: (ORACLE_TWAP + mark_5min_premium) as u64,
                volume_24h: 1_000_000 * QUOTE_PRECISION_U64,
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price: ORACLE_TWAP,
                    last_oracle_price_twap: ORACLE_TWAP,
                    last_oracle_price_twap_5min: ORACLE_TWAP,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            amm: AMM::default(),
            ..PerpMarket::default()
        }
    }

    /// Tier A allows a 2% auction width, so a 1% premium passes through and a 10% premium is cut to
    /// 2%.
    #[test]
    fn clamps_a_tier_long_offset_and_leaves_an_in_band_one_alone() {
        let in_band = market_with_mark_5min_premium(ContractTier::A, PRICE_PRECISION_I64);
        let offset =
            OrderParams::get_perp_baseline_start_price_offset(&in_band, PositionDirection::Long)
                .unwrap();
        assert_eq!(offset, PRICE_PRECISION_I64); // 1%, the raw fast offset

        let out_of_band = market_with_mark_5min_premium(ContractTier::A, 10 * PRICE_PRECISION_I64);
        let offset = OrderParams::get_perp_baseline_start_price_offset(
            &out_of_band,
            PositionDirection::Long,
        )
        .unwrap();
        assert_eq!(offset, 2 * PRICE_PRECISION_I64); // 2% = ORACLE_TWAP / 50
    }

    /// The bound is symmetric. A negative premium is a mark TWAP below oracle, which pushes the short
    /// side of the band.
    #[test]
    fn clamps_the_short_side_symmetrically() {
        let in_band = market_with_mark_5min_premium(ContractTier::A, -PRICE_PRECISION_I64);
        let offset =
            OrderParams::get_perp_baseline_start_price_offset(&in_band, PositionDirection::Short)
                .unwrap();
        assert_eq!(offset, -PRICE_PRECISION_I64);

        let out_of_band = market_with_mark_5min_premium(ContractTier::A, -10 * PRICE_PRECISION_I64);
        let offset = OrderParams::get_perp_baseline_start_price_offset(
            &out_of_band,
            PositionDirection::Short,
        )
        .unwrap();
        assert_eq!(offset, -2 * PRICE_PRECISION_I64);
    }

    /// The bound is tier-aware. The same 10% premium that tier A cuts to 2% is in band for
    /// HighlySpeculative, which allows 20%.
    #[test]
    fn riskier_tiers_get_a_wider_bound() {
        let market = market_with_mark_5min_premium(
            ContractTier::HighlySpeculative,
            10 * PRICE_PRECISION_I64,
        );
        let offset =
            OrderParams::get_perp_baseline_start_price_offset(&market, PositionDirection::Long)
                .unwrap();
        assert_eq!(offset, 10 * PRICE_PRECISION_I64);

        let market = market_with_mark_5min_premium(
            ContractTier::HighlySpeculative,
            30 * PRICE_PRECISION_I64,
        );
        let offset =
            OrderParams::get_perp_baseline_start_price_offset(&market, PositionDirection::Long)
                .unwrap();
        assert_eq!(offset, 20 * PRICE_PRECISION_I64); // 20% = ORACLE_TWAP / 5
    }

    /// The low-volume fallback path divides a mark TWAP, not the oracle TWAP, so it also needs the
    /// clamp. `volume_24h = 0` selects it, and tier A then uses `last_bid_price_twap / 500`.
    #[test]
    fn clamps_the_low_volume_fallback_path() {
        let mut in_band = market_with_mark_5min_premium(ContractTier::A, 0);
        in_band.market_stats.volume_24h = 0;
        in_band.market_stats.last_bid_price_twap = 500 * PRICE_PRECISION_U64;
        let offset =
            OrderParams::get_perp_baseline_start_price_offset(&in_band, PositionDirection::Long)
                .unwrap();
        assert_eq!(offset, PRICE_PRECISION_I64); // 500 / 500 = 1% of oracle, in band

        let mut out_of_band = market_with_mark_5min_premium(ContractTier::A, 0);
        out_of_band.market_stats.volume_24h = 0;
        out_of_band.market_stats.last_bid_price_twap = 2000 * PRICE_PRECISION_U64;
        let offset = OrderParams::get_perp_baseline_start_price_offset(
            &out_of_band,
            PositionDirection::Long,
        )
        .unwrap();
        assert_eq!(offset, 2 * PRICE_PRECISION_I64); // raw 4%, cut to 2%
    }

    /// The end offset derives from the start offset with a `min`/`max` against it, so clamping the
    /// start keeps the long band ordered as start <= end.
    #[test]
    fn clamped_start_stays_ordered_against_the_end_offset() {
        let market = market_with_mark_5min_premium(ContractTier::A, 10 * PRICE_PRECISION_I64);
        let (start, end) = OrderParams::get_perp_baseline_start_end_price_offset(
            &market,
            PositionDirection::Long,
            1,
        )
        .unwrap();
        assert_eq!(start, 2 * PRICE_PRECISION_I64);
        assert!(end >= start);

        let (start, end) = OrderParams::get_perp_baseline_start_end_price_offset(
            &market,
            PositionDirection::Short,
            1,
        )
        .unwrap();
        assert!(end <= start);
    }
}

#[test]
fn test_parse_optional_params() {
    let (success_condition, auction_duration_percentage) = parse_optional_params(Some(0x00001234));
    assert_eq!(success_condition, 0x34);
    assert_eq!(auction_duration_percentage, 0x12);
}

/// The digest is what binds a filler to the taker's choice, so its two
/// properties are load-bearing: an empty route reads as "none signed", and the
/// same choice digests identically however a client ordered or repeated it.
#[test]
fn a_signed_route_digests_canonically() {
    use {
        crate::state::order_params::{route_digest, NO_ROUTE_DIGEST},
        anchor_lang::prelude::Pubkey,
    };

    assert_eq!(
        route_digest(&[]),
        NO_ROUTE_DIGEST,
        "no route is the zero digest"
    );

    let a = Pubkey::new_from_array([7; 32]);
    let b = Pubkey::new_from_array([9; 32]);
    assert_eq!(route_digest(&[a, b]), route_digest(&[b, a]), "order-free");
    assert_eq!(
        route_digest(&[a, b, a]),
        route_digest(&[a, b]),
        "duplicates collapse"
    );
    assert_ne!(
        route_digest(&[a]),
        route_digest(&[a, b]),
        "adding a quoter is a different route"
    );
    assert_ne!(
        route_digest(&[a]),
        NO_ROUTE_DIGEST,
        "a real route is never 'none'"
    );

    // Pinned bytes, so the SDK mirror (`getRouteDigest`) has something exact to
    // agree with rather than only the properties above.
    assert_eq!(
        route_digest(&[a]),
        [0x4b, 0xb0, 0x6f, 0x8e, 0x4e, 0x3a, 0x77, 0x15]
    );
    assert_eq!(
        route_digest(&[a, b]),
        [0x49, 0x44, 0x0f, 0xa4, 0x64, 0xb1, 0x71, 0xc0]
    );
}
