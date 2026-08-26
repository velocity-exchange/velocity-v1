use {
    crate::{
        math::{
            constants::ONE_BPS_DENOMINATOR,
            oracle::{self, oracle_validity},
        },
        state::{
            fill_mode::FillMode,
            oracle_map::OracleMap,
            perp_market::PerpMarket,
            state::{FeeStructure, FeeTier, State},
            user::{MarketType, Order, PerpPosition},
        },
    },
    anchor_lang::prelude::Pubkey,
};

/// A response account a mock can hand back, holding the bytes a real
/// quoter would have written.
///
/// Leaks: the account has to outlive the executor that names it, and a
/// unit test's process ends before that matters. Production never takes
/// this path — there the bytes are already in the quoter's account.
fn response_account(
    changes: &[crate::state::prop_amm::UserBalanceChangeV0],
    completed: &[crate::state::prop_amm::CompletedOrderV0],
) -> crate::state::prop_amm::ResponseLocationV0<'static> {
    let bytes = quoter_spec::wincode::serialize(&quoter_spec::ExecuteResponseV0 {
        changes,
        cancelled: &[],
        completed,
        partial: &[],
    })
    .unwrap();
    let len = bytes.len();
    let data: &'static mut [u8] = Box::leak(bytes.into_boxed_slice());
    let key: &'static anchor_lang::prelude::Pubkey =
        Box::leak(Box::new(anchor_lang::prelude::Pubkey::new_unique()));
    let owner: &'static anchor_lang::prelude::Pubkey = Box::leak(Box::new(crate::ID));
    let lamports: &'static mut u64 = Box::leak(Box::new(0u64));
    crate::state::prop_amm::ResponseLocationV0 {
        account: anchor_lang::prelude::AccountInfo::new(
            key, false, true, lamports, data, owner, false,
        ),
        start: 0,
        end: len,
    }
}

fn get_fee_structure() -> FeeStructure {
    let mut fee_tiers = [FeeTier::default(); 10];
    fee_tiers[0] = FeeTier {
        fee_numerator: 5,
        fee_denominator: ONE_BPS_DENOMINATOR,
        maker_rebate_numerator: 3,
        maker_rebate_denominator: ONE_BPS_DENOMINATOR,
        ..FeeTier::default()
    };
    FeeStructure {
        fee_tiers,
        ..FeeStructure::test_default()
    }
}

fn get_user_keys() -> (Pubkey, Pubkey, Pubkey) {
    (Pubkey::default(), Pubkey::default(), Pubkey::default())
}

fn get_state(min_auction_duration: u8) -> State {
    State {
        min_perp_auction_duration: min_auction_duration,
        ..State::default()
    }
}

pub fn get_amm_is_available(
    order: &Order,
    min_auction_duration: u8,
    market: &PerpMarket,
    oracle_map: &mut OracleMap,
    slot: u64,
    user_can_skip_auction_duration: bool,
) -> bool {
    let state = get_state(min_auction_duration);
    let oracle_price_data = oracle_map.get_price_data(&market.oracle_id()).unwrap();
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(*oracle_price_data, slot, &state.oracle_guard_rails.validity)
        .unwrap();
    let safe_oracle_price_data = mm_oracle_price_data.get_safe_oracle_price_data();
    let safe_oracle_validity = oracle_validity(
        MarketType::Perp,
        market.market_index,
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap,
        &safe_oracle_price_data,
        &state.oracle_guard_rails.validity,
        market.get_max_confidence_interval_multiplier().unwrap(),
        &market.oracle_source,
        oracle::LogMode::SafeMMOracle,
        market.oracle_slot_delay_override,
        mm_oracle_price_data.is_safe_price_mm_sourced(),
        market.oracle_low_risk_slot_delay_override,
    )
    .unwrap();
    market
        .amm_can_fill_order(
            order,
            slot,
            FillMode::Fill,
            &state,
            safe_oracle_validity,
            user_can_skip_auction_duration,
            &mm_oracle_price_data,
        )
        .unwrap()
}

#[cfg(test)]
pub mod amm_jit {
    use {
        super::*,
        crate::{
            controller::{orders::fulfill_perp_order, position::PositionDirection},
            create_anchor_account_info,
            math::constants::{
                AMM_RESERVE_PRECISION, BASE_PRECISION_I128, BASE_PRECISION_I64, BASE_PRECISION_U64,
                CONCENTRATION_PRECISION, PEG_PRECISION, PRICE_PRECISION, PRICE_PRECISION_I64,
                PRICE_PRECISION_U64, QUOTE_PRECISION_I64, SPOT_BALANCE_PRECISION_U64,
                SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
            },
            state::{
                fill_mode::FillMode,
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                perp_market::{MarketStats, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                user::{OrderStatus, OrderType, SpotPosition, User, UserStats},
                user_map::{UserMap, UserStatsMap},
            },
            test_utils::{get_orders, get_positions, get_pyth_price, get_spot_positions},
        },
        std::str::FromStr,
    };

    /// The router pass (single pass over maker books + the vAMM ladder) —
    /// the successor to the JIT machinery above. Maker's 0.5 @ 100 fills at
    /// the better price; the vAMM ladder covers the rest within the taker's
    /// 102 limit. Both settle through the same fee-policy paths as the
    /// legacy step loop.
    #[test]
    fn router_pass_fills_taker_across_maker_and_amm() {
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
                base_asset_amount_with_amm: (AMM_RESERVE_PRECISION / 2) as i128,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_spread: 20000,
                ..AMM::default()
            },
            base_asset_amount_long: (AMM_RESERVE_PRECISION / 2) as i128,
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
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u64::MAX as u128;
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
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        // Taker buys 1 with a 105 limit — room for the maker AND the full
        // vAMM ladder (rungs are marginal prices, the deepest sits ~103).
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 0,
                auction_end_price: 105 * PRICE_PRECISION_I64,
                price: 105 * PRICE_PRECISION_U64,
                auction_duration: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
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

        let maker_key = Pubkey::from_str("My11111111111111111111111111111111111111113").unwrap();
        let maker_authority =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        let mut maker = User {
            authority: maker_authority,
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
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
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };
        create_anchor_account_info!(maker, &maker_key, User, maker_account_info);
        let makers_and_referrers = UserMap::load_one(&maker_account_info).unwrap();

        let mut filler = User::default();
        let fee_structure = get_fee_structure();
        let (taker_key, _, filler_key) = get_user_keys();
        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats {
            authority: maker_authority,
            ..UserStats::default()
        };
        create_anchor_account_info!(maker_stats, UserStats, maker_stats_account_info);
        let maker_and_referrer_stats = UserStatsMap::load_one(&maker_stats_account_info).unwrap();
        let mut filler_stats = UserStats::default();

        // Router mode with no external quoters: vAMM + the passed maker.
        let mut no_externals = crate::state::prop_amm::NoExternalQuoters;
        let mut router_inputs = crate::math::router::RouterFillInputs {
            books: &[],
            executor: &mut no_externals,
            protocol_authority: Pubkey::default(),
            // Test fixtures stand in for a taker-signed fill: no filler
            // obligation, so a withheld book does not end the pass.
            obligation: crate::math::router::FillerObligation {
                taker_signed: true,
                tx_accounts: None,
                unrouted_quoters: 0,
            },
        };
        let (base_asset_amount, quote_asset_amount) = fulfill_perp_order(
            &mut taker,
            0,
            &taker_key,
            &mut taker_stats,
            &makers_and_referrers,
            &maker_and_referrer_stats,
            &[(maker_key, 0, 100 * PRICE_PRECISION_U64)],
            &mut Some(&mut filler),
            &filler_key,
            &mut Some(&mut filler_stats),
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            true,
            FillMode::Fill,
            false,
            &mut router_inputs,
            &mut None,
            false,
            0,
        )
        .unwrap();

        // Fully filled: 0.5 from the maker at 100, 0.5 from the vAMM ladder
        // within the 105 limit.
        assert_eq!(base_asset_amount, BASE_PRECISION_U64);
        assert!(quote_asset_amount > 0);
        assert_eq!(
            taker.perp_positions[0].base_asset_amount,
            BASE_PRECISION_I64
        );

        let maker_after = makers_and_referrers.get_ref(&maker_key).unwrap();
        assert_eq!(
            maker_after.orders[0].base_asset_amount_filled,
            BASE_PRECISION_U64 / 2
        );

        // The vAMM's half moved the AMM's counterparty position.
        let market_after = market_map.get_ref(&0).unwrap();
        assert_eq!(
            market_after.amm.base_asset_amount_with_amm,
            AMM_RESERVE_PRECISION as i128
        );
    }

    /// The router pass with an external CPI book: a mock executor stands in
    /// for the CLOB's `execute_v0`, filling against a loaded maker that has
    /// no velocity order — just the open-order aggregates a velocity-mediated
    /// CLOB placement reserves. The external book (best price) and the DLOB
    /// maker split the taker; the fill settles through
    /// `settle_external_match_fill` and unwinds the CLOB maker's aggregates.
    #[test]
    fn router_pass_settles_external_clob_fills_against_loaded_makers() {
        use crate::state::prop_amm::{
            CompletedOrderV0, Direction, ExecuteResponseV0, ExternalQuoterExecutor, PriceLevel,
            QuoterType, UserBalanceChangeV0,
        };

        struct MockClobExecutor {
            user: Pubkey,
            user_ref: crate::state::prop_amm::ClobUserRefV0,
            price: u64,
        }
        impl ExternalQuoterExecutor<'static> for MockClobExecutor {
            fn quoter_type(&self, _index: usize) -> QuoterType {
                QuoterType::Clob
            }
            fn quoter_user(&self, _index: usize) -> Pubkey {
                self.user
            }
            fn quoter_key(&self, _index: usize) -> Pubkey {
                self.user
            }
            fn subjects(
                &self,
                _index: usize,
                _direction: Direction,
                _size: u64,
            ) -> crate::error::VelocityResult<crate::state::prop_amm::QuoterSubjects> {
                Ok(crate::state::prop_amm::QuoterSubjects::Book)
            }
            fn execute(
                &mut self,
                _index: usize,
                _direction: Direction,
                size: u64,
            ) -> crate::error::VelocityResult<crate::state::prop_amm::ResponseLocationV0<'static>>
            {
                let quote_size =
                    ((size as u128) * (self.price as u128) / BASE_PRECISION_U64 as u128) as u64;
                Ok(response_account(
                    &[UserBalanceChangeV0 {
                        base_size: size,
                        quote_size,
                        user: self.user_ref,
                        _pad: [0; 6],
                    }],
                    &[CompletedOrderV0 {
                        order_id: 1,
                        change_index: 0,
                        client_order_id: 0,
                    }],
                ))
            }
        }

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
                base_asset_amount_with_amm: (AMM_RESERVE_PRECISION / 2) as i128,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_spread: 20000,
                ..AMM::default()
            },
            base_asset_amount_long: (AMM_RESERVE_PRECISION / 2) as i128,
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
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u64::MAX as u128;
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
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 0,
                auction_end_price: 105 * PRICE_PRECISION_I64,
                price: 105 * PRICE_PRECISION_U64,
                auction_duration: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
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

        // DLOB maker: 0.5 ask at 100.
        let maker_key = Pubkey::from_str("My11111111111111111111111111111111111111113").unwrap();
        let maker_authority =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        let mut maker = User {
            authority: maker_authority,
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
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
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };
        create_anchor_account_info!(maker, &maker_key, User, maker_account_info);
        let mut makers_and_referrers = UserMap::load_one(&maker_account_info).unwrap();

        // "CLOB maker": no velocity order — a velocity-mediated CLOB
        // placement reserved 0.5 of open asks and one open-order slot.
        let clob_maker_key =
            Pubkey::from_str("CLoB111111111111111111111111111111111111111").unwrap();
        let clob_maker_authority =
            Pubkey::from_str("6ncQ5nmiZjHJK8QPGevKJnnLKtSXjZ4Q2r8bTHTNiFEf").unwrap();
        let mut clob_maker = User {
            authority: clob_maker_authority,
            open_orders: 1,
            has_open_order: true,
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64 / 2,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };
        create_anchor_account_info!(clob_maker, &clob_maker_key, User, clob_maker_account_info);
        makers_and_referrers.0.insert(
            clob_maker_key,
            anchor_lang::prelude::AccountLoader::try_from(&clob_maker_account_info).unwrap(),
        );

        let mut filler = User::default();
        let fee_structure = get_fee_structure();
        let (taker_key, _, filler_key) = get_user_keys();
        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats {
            authority: maker_authority,
            ..UserStats::default()
        };
        create_anchor_account_info!(maker_stats, UserStats, maker_stats_account_info);
        let mut maker_and_referrer_stats =
            UserStatsMap::load_one(&maker_stats_account_info).unwrap();
        let mut clob_maker_stats = UserStats {
            authority: clob_maker_authority,
            ..UserStats::default()
        };
        create_anchor_account_info!(clob_maker_stats, UserStats, clob_maker_stats_account_info);
        maker_and_referrer_stats.0.insert(
            clob_maker_authority,
            anchor_lang::prelude::AccountLoader::try_from(&clob_maker_stats_account_info).unwrap(),
        );
        let mut filler_stats = UserStats::default();

        // External CLOB book: 0.5 at 99 — the best price on the fill.
        let external_levels = [PriceLevel {
            price: 99 * PRICE_PRECISION_U64,
            size: BASE_PRECISION_U64 / 2,
        }];
        let external_books = [crate::math::router::QuoterBook {
            priority: QuoterType::Clob.default_priority(),
            levels: &external_levels,
            withheld: PriceLevel::default(),
        }];
        let mut executor = MockClobExecutor {
            user: clob_maker_key,
            user_ref: crate::state::prop_amm::ClobUserRefV0 {
                authority: clob_maker_authority,
                sub_account_id: 0,
            },
            price: 99 * PRICE_PRECISION_U64,
        };
        let mut router_inputs = crate::math::router::RouterFillInputs {
            books: &external_books,
            executor: &mut executor,
            protocol_authority: Pubkey::default(),
            // Test fixtures stand in for a taker-signed fill: no filler
            // obligation, so a withheld book does not end the pass.
            obligation: crate::math::router::FillerObligation {
                taker_signed: true,
                tx_accounts: None,
                unrouted_quoters: 0,
            },
        };

        let (base_asset_amount, quote_asset_amount) = fulfill_perp_order(
            &mut taker,
            0,
            &taker_key,
            &mut taker_stats,
            &makers_and_referrers,
            &maker_and_referrer_stats,
            &[(maker_key, 0, 100 * PRICE_PRECISION_U64)],
            &mut Some(&mut filler),
            &filler_key,
            &mut Some(&mut filler_stats),
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            true,
            FillMode::Fill,
            false,
            &mut router_inputs,
            &mut None,
            false,
            0,
        )
        .unwrap();

        // Fully filled: 0.5 from the external book at 99, 0.5 from the DLOB
        // maker at 100. The vAMM (ask above 100) gets nothing.
        assert_eq!(base_asset_amount, BASE_PRECISION_U64);
        assert_eq!(
            quote_asset_amount,
            (99 * QUOTE_PRECISION_I64 / 2 + 100 * QUOTE_PRECISION_I64 / 2) as u64
        );
        assert_eq!(
            taker.perp_positions[0].base_asset_amount,
            BASE_PRECISION_I64
        );

        // The CLOB maker took the short side and its reserved aggregates
        // unwound: fill decremented open_asks, the completed order freed the
        // open-order slot.
        let clob_maker_after = makers_and_referrers.get_ref(&clob_maker_key).unwrap();
        assert_eq!(
            clob_maker_after.perp_positions[0].base_asset_amount,
            -BASE_PRECISION_I64 / 2
        );
        assert_eq!(clob_maker_after.perp_positions[0].open_asks, 0);
        assert_eq!(clob_maker_after.perp_positions[0].open_orders, 0);
        assert_eq!(clob_maker_after.open_orders, 0);
        assert!(!clob_maker_after.has_open_order);

        let maker_after = makers_and_referrers.get_ref(&maker_key).unwrap();
        assert_eq!(
            maker_after.orders[0].base_asset_amount_filled,
            BASE_PRECISION_U64 / 2
        );

        // No vAMM participation.
        let market_after = market_map.get_ref(&0).unwrap();
        assert_eq!(
            market_after.amm.base_asset_amount_with_amm,
            (AMM_RESERVE_PRECISION / 2) as i128
        );
    }

    /// A Custom quoter's response may name one subject: the account its
    /// registration consented for. The loaded-user set is much wider than
    /// that — it holds the taker and every rival source's makers — so naming
    /// one of those would be minting a position onto a stranger at a price
    /// the quoter chose, and velocity refuses the whole fill.
    ///
    /// Same fixture as the fill above, except the external quoter's response
    /// names the *DLOB* maker. This is the wiring test for the rule
    /// `QuoterSubjects::permits` states, which `state::prop_amm::tests`
    /// covers case by case — including why a book, whose subjects velocity
    /// cannot establish independently, is bound by the price and margin
    /// checks instead.
    #[test]
    fn router_pass_rejects_an_external_quoter_naming_another_sources_maker() {
        use crate::state::prop_amm::{
            ClobUserRefV0, Direction, ExecuteResponseV0, ExternalQuoterExecutor, PriceLevel,
            QuoterSubjects, QuoterType, UserBalanceChangeV0,
        };

        /// Its book rests `resting`; its response names `names`.
        struct HostileCustomExecutor {
            user: Pubkey,
            resting: ClobUserRefV0,
            names: ClobUserRefV0,
            price: u64,
        }
        impl ExternalQuoterExecutor<'static> for HostileCustomExecutor {
            fn quoter_type(&self, _index: usize) -> QuoterType {
                QuoterType::Custom
            }
            fn quoter_user(&self, _index: usize) -> Pubkey {
                self.user
            }
            fn quoter_key(&self, _index: usize) -> Pubkey {
                self.user
            }
            fn subjects(
                &self,
                _index: usize,
                _direction: Direction,
                _size: u64,
            ) -> crate::error::VelocityResult<QuoterSubjects> {
                Ok(QuoterSubjects::Account(self.user))
            }
            fn execute(
                &mut self,
                _index: usize,
                _direction: Direction,
                size: u64,
            ) -> crate::error::VelocityResult<crate::state::prop_amm::ResponseLocationV0<'static>>
            {
                let quote_size =
                    ((size as u128) * (self.price as u128) / BASE_PRECISION_U64 as u128) as u64;
                Ok(response_account(
                    &[UserBalanceChangeV0 {
                        base_size: size,
                        quote_size,
                        user: self.names,
                        _pad: [0; 6],
                    }],
                    &[],
                ))
            }
        }

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
                base_asset_amount_with_amm: (AMM_RESERVE_PRECISION / 2) as i128,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_spread: 20000,
                ..AMM::default()
            },
            base_asset_amount_long: (AMM_RESERVE_PRECISION / 2) as i128,
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
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u64::MAX as u128;
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
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 0,
                auction_end_price: 105 * PRICE_PRECISION_I64,
                price: 105 * PRICE_PRECISION_U64,
                auction_duration: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
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

        // The DLOB maker — loaded, but resting on velocity's own book, not on
        // the external quoter's.
        let maker_key = Pubkey::from_str("My11111111111111111111111111111111111111113").unwrap();
        let maker_authority =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        let mut maker = User {
            authority: maker_authority,
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
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
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };
        create_anchor_account_info!(maker, &maker_key, User, maker_account_info);
        let mut makers_and_referrers = UserMap::load_one(&maker_account_info).unwrap();

        let clob_maker_key =
            Pubkey::from_str("CLoB111111111111111111111111111111111111111").unwrap();
        let clob_maker_authority =
            Pubkey::from_str("6ncQ5nmiZjHJK8QPGevKJnnLKtSXjZ4Q2r8bTHTNiFEf").unwrap();
        let mut clob_maker = User {
            authority: clob_maker_authority,
            open_orders: 1,
            has_open_order: true,
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64 / 2,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };
        create_anchor_account_info!(clob_maker, &clob_maker_key, User, clob_maker_account_info);
        makers_and_referrers.0.insert(
            clob_maker_key,
            anchor_lang::prelude::AccountLoader::try_from(&clob_maker_account_info).unwrap(),
        );

        let mut filler = User::default();
        let fee_structure = get_fee_structure();
        let (taker_key, _, filler_key) = get_user_keys();
        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats {
            authority: maker_authority,
            ..UserStats::default()
        };
        create_anchor_account_info!(maker_stats, UserStats, maker_stats_account_info);
        let mut maker_and_referrer_stats =
            UserStatsMap::load_one(&maker_stats_account_info).unwrap();
        let mut clob_maker_stats = UserStats {
            authority: clob_maker_authority,
            ..UserStats::default()
        };
        create_anchor_account_info!(clob_maker_stats, UserStats, clob_maker_stats_account_info);
        maker_and_referrer_stats.0.insert(
            clob_maker_authority,
            anchor_lang::prelude::AccountLoader::try_from(&clob_maker_stats_account_info).unwrap(),
        );
        let mut filler_stats = UserStats::default();

        let external_levels = [PriceLevel {
            price: 99 * PRICE_PRECISION_U64,
            size: BASE_PRECISION_U64 / 2,
        }];
        let external_books = [crate::math::router::QuoterBook {
            priority: QuoterType::Clob.default_priority(),
            levels: &external_levels,
            withheld: PriceLevel::default(),
        }];
        let mut executor = HostileCustomExecutor {
            user: clob_maker_key,
            resting: ClobUserRefV0 {
                authority: clob_maker_authority,
                sub_account_id: 0,
            },
            // The DLOB maker: loaded, settleable, and none of this quoter's
            // business.
            names: ClobUserRefV0 {
                authority: maker_authority,
                sub_account_id: 0,
            },
            price: 99 * PRICE_PRECISION_U64,
        };
        let mut router_inputs = crate::math::router::RouterFillInputs {
            books: &external_books,
            executor: &mut executor,
            protocol_authority: Pubkey::default(),
            // Test fixtures stand in for a taker-signed fill: no filler
            // obligation, so a withheld book does not end the pass.
            obligation: crate::math::router::FillerObligation {
                taker_signed: true,
                tx_accounts: None,
                unrouted_quoters: 0,
            },
        };

        let result = fulfill_perp_order(
            &mut taker,
            0,
            &taker_key,
            &mut taker_stats,
            &makers_and_referrers,
            &maker_and_referrer_stats,
            &[(maker_key, 0, 100 * PRICE_PRECISION_U64)],
            &mut Some(&mut filler),
            &filler_key,
            &mut Some(&mut filler_stats),
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            true,
            FillMode::Fill,
            false,
            &mut router_inputs,
            &mut None,
            false,
            0,
        );

        assert_eq!(
            result,
            Err(crate::error::ErrorCode::QuoterSubjectNotPermitted)
        );
    }

    /// A Custom PropAMM's depth is never margin-reserved, so the router pass
    /// clamps its book to what the quoted user's account supports before the
    /// split — a thin maker fills what its margin covers instead of failing
    /// the whole fill at the post-check; the vAMM covers the residual.
    #[test]
    fn router_pass_clamps_custom_book_to_the_quoter_users_margin() {
        use crate::state::prop_amm::{
            CompletedOrderV0, Direction, ExecuteResponseV0, ExternalQuoterExecutor, PriceLevel,
            QuoterType, UserBalanceChangeV0,
        };

        struct MockCustomExecutor {
            user: Pubkey,
            user_ref: crate::state::prop_amm::ClobUserRefV0,
            price: u64,
            requested: u64,
        }
        impl ExternalQuoterExecutor<'static> for MockCustomExecutor {
            fn quoter_type(&self, _index: usize) -> QuoterType {
                QuoterType::Custom
            }
            fn quoter_user(&self, _index: usize) -> Pubkey {
                self.user
            }
            fn quoter_key(&self, _index: usize) -> Pubkey {
                self.user
            }
            fn subjects(
                &self,
                _index: usize,
                _direction: Direction,
                _size: u64,
            ) -> crate::error::VelocityResult<crate::state::prop_amm::QuoterSubjects> {
                Ok(crate::state::prop_amm::QuoterSubjects::Account(self.user))
            }
            fn execute(
                &mut self,
                _index: usize,
                _direction: Direction,
                size: u64,
            ) -> crate::error::VelocityResult<crate::state::prop_amm::ResponseLocationV0<'static>>
            {
                self.requested = size;
                let quote_size =
                    ((size as u128) * (self.price as u128) / BASE_PRECISION_U64 as u128) as u64;
                Ok(response_account(
                    &[UserBalanceChangeV0 {
                        base_size: size,
                        quote_size,
                        user: self.user_ref,
                        _pad: [0; 6],
                    }],
                    &[],
                ))
            }
        }

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
                base_asset_amount_with_amm: (AMM_RESERVE_PRECISION / 2) as i128,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_spread: 20000,
                ..AMM::default()
            },
            base_asset_amount_long: (AMM_RESERVE_PRECISION / 2) as i128,
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
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u64::MAX as u128;
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
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 0,
                auction_end_price: 105 * PRICE_PRECISION_I64,
                price: 105 * PRICE_PRECISION_U64,
                auction_duration: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
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

        // The PropAMM's user: quotes 0.5 but only 3 USDC of collateral —
        // at 10% initial margin and ~$99 that supports well under 0.5.
        let custom_maker_key =
            Pubkey::from_str("CLoB111111111111111111111111111111111111111").unwrap();
        let custom_maker_authority =
            Pubkey::from_str("6ncQ5nmiZjHJK8QPGevKJnnLKtSXjZ4Q2r8bTHTNiFEf").unwrap();
        let mut custom_maker = User {
            authority: custom_maker_authority,
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 3 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };
        create_anchor_account_info!(custom_maker, &custom_maker_key, User, custom_maker_info);
        let makers_and_referrers = UserMap::load_one(&custom_maker_info).unwrap();

        let mut filler = User::default();
        let fee_structure = get_fee_structure();
        let (taker_key, _, filler_key) = get_user_keys();
        let mut taker_stats = UserStats::default();
        let mut custom_maker_stats = UserStats {
            authority: custom_maker_authority,
            ..UserStats::default()
        };
        create_anchor_account_info!(custom_maker_stats, UserStats, custom_maker_stats_info);
        let maker_and_referrer_stats = UserStatsMap::load_one(&custom_maker_stats_info).unwrap();
        let mut filler_stats = UserStats::default();

        let external_levels = [PriceLevel {
            price: 99 * PRICE_PRECISION_U64,
            size: BASE_PRECISION_U64 / 2,
        }];
        let external_books = [crate::math::router::QuoterBook {
            priority: QuoterType::Custom.default_priority(),
            levels: &external_levels,
            withheld: PriceLevel::default(),
        }];
        let mut executor = MockCustomExecutor {
            user: custom_maker_key,
            user_ref: crate::state::prop_amm::ClobUserRefV0 {
                authority: custom_maker_authority,
                sub_account_id: 0,
            },
            price: 99 * PRICE_PRECISION_U64,
            requested: 0,
        };
        let mut router_inputs = crate::math::router::RouterFillInputs {
            books: &external_books,
            executor: &mut executor,
            protocol_authority: Pubkey::default(),
            // Test fixtures stand in for a taker-signed fill: no filler
            // obligation, so a withheld book does not end the pass.
            obligation: crate::math::router::FillerObligation {
                taker_signed: true,
                tx_accounts: None,
                unrouted_quoters: 0,
            },
        };

        let (base_asset_amount, _) = fulfill_perp_order(
            &mut taker,
            0,
            &taker_key,
            &mut taker_stats,
            &makers_and_referrers,
            &maker_and_referrer_stats,
            &[],
            &mut Some(&mut filler),
            &filler_key,
            &mut Some(&mut filler_stats),
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            true,
            FillMode::Fill,
            false,
            &mut router_inputs,
            &mut None,
            false,
            0,
        )
        .unwrap();

        // The book was clamped before the split: the executor was asked for
        // less than the quoted 0.5, the thin maker's position matches what
        // its margin supports, and the vAMM covered the taker's remainder.
        let requested = executor.requested;
        assert!(requested > 0, "clamp zeroed the book entirely");
        assert!(
            requested < BASE_PRECISION_U64 / 2,
            "clamp did not bite: {}",
            requested
        );
        assert_eq!(base_asset_amount, BASE_PRECISION_U64);
        assert_eq!(
            taker.perp_positions[0].base_asset_amount,
            BASE_PRECISION_I64
        );
        let custom_maker_after = makers_and_referrers.get_ref(&custom_maker_key).unwrap();
        assert_eq!(
            custom_maker_after.perp_positions[0].base_asset_amount,
            -(requested as i64)
        );
        let market_after = market_map.get_ref(&0).unwrap();
        assert_eq!(
            market_after.amm.base_asset_amount_with_amm,
            (AMM_RESERVE_PRECISION / 2) as i128 + (BASE_PRECISION_U64 - requested) as i128
        );
    }
}
