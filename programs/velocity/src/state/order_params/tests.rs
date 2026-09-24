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

        let oracle_price_offset = params.oracle_price_offset.unwrap();
        // twap 100.000000, bound 0.5% = 0.500000
        assert_eq!(oracle_price_offset, 500_000);

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

        let oracle_price_offset = params.oracle_price_offset.unwrap();
        // twap 100.000000, bound 0.5% = 0.500000
        assert_eq!(oracle_price_offset, 500_000);

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

        let oracle_price_offset = params.oracle_price_offset.unwrap();
        // twap 100.000000, bound 0.5% = 0.500000
        assert_eq!(oracle_price_offset, 500_000);

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

        let oracle_price_offset = params.oracle_price_offset.unwrap();
        // twap 100.000000, bound 0.5% = 0.500000
        assert_eq!(oracle_price_offset, -500_000);

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

        let oracle_price_offset = params.oracle_price_offset.unwrap();
        // twap 100.000000, bound 0.5% = 0.500000
        assert_eq!(oracle_price_offset, -500_000);

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

        let oracle_price_offset = params.oracle_price_offset.unwrap();
        // twap 100.000000, bound 0.5% = 0.500000
        assert_eq!(oracle_price_offset, -500_000);

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

        let oracle_price_offset = params.oracle_price_offset.unwrap();
        // twap 45132.962200, bound 0.5%
        assert_eq!(oracle_price_offset, -225_664_811);

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

        let oracle_price_offset = params.oracle_price_offset.unwrap();
        // legacy: auction_end_price=-1021, oracle_price_offset=-1021
        // twap 0.077600, bound 0.5%
        assert_eq!(oracle_price_offset, -388);

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
            clob_node_index: 0,
            clob_order_id: 0,
            unused_auction_duration: 0,
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

        let params = OrderParams::get_close_perp_params(
            &perp_market,
            PositionDirection::Long,
            base_asset_amount,
        )
        .unwrap();

        let oracle_price_offset = params.oracle_price_offset.unwrap();
        assert!(oracle_price_offset > 0);

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
