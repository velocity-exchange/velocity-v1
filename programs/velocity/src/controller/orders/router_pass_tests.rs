use {
    crate::{
        math::{
            constants::ONE_BPS_DENOMINATOR,
            oracle::{self, oracle_validity},
            time::{legacy_slot_duration_u8, SlotClock},
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
    crate::state::prop_amm::ResponseLocationV0::new(
        anchor_lang::prelude::AccountInfo::new(key, false, true, lamports, data, owner, false),
        &quoter_spec::ResponsePointerV0 {
            offset: 0,
            len: len as u32,
        },
    )
    .unwrap()
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
        min_perp_auction_duration: legacy_slot_duration_u8(min_auction_duration),
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
        .get_mm_oracle_price_data(
            *oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
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
        slot,
        SlotClock::baseline(),
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
            controller::{
                orders::{
                    fill_within_taker_risk_limits, FillAmounts, FillConditions, FillParties,
                    FillerSide, OfferedLiquidity, PricingRules, TakerSide,
                },
                position::PositionDirection,
            },
            create_anchor_account_info,
            instructions::optional_accounts::AccountMaps,
            math::constants::{
                AMM_RESERVE_PRECISION, BASE_PRECISION_I64, BASE_PRECISION_U64, PEG_PRECISION,
                PRICE_PRECISION, PRICE_PRECISION_U64, QUOTE_PRECISION_I64,
                SPOT_BALANCE_PRECISION_U64, SPOT_CUMULATIVE_INTEREST_PRECISION,
                SPOT_WEIGHT_PRECISION,
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

    /// The pass routes a Custom book at exactly the depth it was handed, and
    /// settles the quoted user for it.
    ///
    /// A Custom PropAMM's depth is never margin-reserved, so a ladder deeper
    /// than the quoted user's account supports would fail the post-check and
    /// take the whole fill with it. The ladder reaches the pass already cut
    /// to that account's room, because `QuotedRoute::assemble` trims it while
    /// the route is built. This is the other half of that: handed a ladder
    /// inside the room, the pass fills it, settles the maker for the whole of
    /// it, and the vAMM covers the taker's residual.
    #[test]
    fn router_pass_settles_a_custom_book_at_the_depth_it_was_quoted() {
        use crate::state::prop_amm::{
            DirectionV0, ExternalQuoterExecutor, PriceLevelV0, QuoterType, UserBalanceChangeV0,
        };

        struct MockCustomExecutor {
            user: Pubkey,
            user_ref: crate::state::prop_amm::UserRefV0,
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
                _direction: DirectionV0,
                _size: u64,
            ) -> crate::error::VelocityResult<crate::state::prop_amm::QuoterSubjects> {
                Ok(crate::state::prop_amm::QuoterSubjects::Account(self.user))
            }
            fn execute(
                &mut self,
                _index: usize,
                _direction: DirectionV0,
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
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);

        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 105 * PRICE_PRECISION_U64,
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

        // The PropAMM's user: 3 USDC of collateral, which at 10% initial
        // margin and ~$99 carries about 0.3 base. The ladder below offers
        // 0.25, which is what the trim would have left of a deeper quote.
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

        let external_levels = [PriceLevelV0 {
            price: 99 * PRICE_PRECISION_U64,
            size: BASE_PRECISION_U64 / 4,
        }];
        let external_books = [crate::math::router::QuoterBook {
            priority: QuoterType::Custom.default_priority(),
            levels: &external_levels,
            withheld: PriceLevelV0::default(),
        }];
        let mut executor = MockCustomExecutor {
            user: custom_maker_key,
            user_ref: crate::state::prop_amm::UserRefV0 {
                authority: custom_maker_authority,
                sub_account_id: 0,
            },

            price: 99 * PRICE_PRECISION_U64,
            requested: 0,
        };
        let mut router_inputs = crate::math::router::RouterLeg {
            books: &external_books,
            executor: &mut executor,
            standing: crate::instructions::FillerStanding {
                protocol_authority: Pubkey::default(),
                taker_exposure_closed_by_caller: false,
                // Test fixtures stand in for a taker-signed fill: no filler
                // obligation, so a withheld book does not end the pass.
                obligation: crate::math::router::FillerObligation {
                    taker_signed: true,
                    tx_accounts: None,
                    unrouted_quoters: 0,
                },
            },

            worst_fill_price: None,
        };

        let mut order = taker.orders[0];
        let FillAmounts {
            base: base_asset_amount,
            ..
        } = fill_within_taker_risk_limits(
            &mut TakerSide::bind(&mut taker, &mut taker_stats, taker_key, &mut order, true)
                .unwrap(),
            &PricingRules {
                fee_structure: &fee_structure,
                validity_guard_rails: &crate::state::state::ValidityGuardRails::default(),
                promo_fee_tier: 0,
                referrer_is_accelerated: false,
                vamm_maker_rebate: false,
                // The taker layer takes this decision itself and overrides it.
                builder_fee_allowed: false,
            },
            &FillConditions::for_layer_test(
                FillMode::Fill,
                now,
                slot,
                Some(market.market_stats.historical_oracle_data.last_oracle_price),
                true,
                false,
            ),
            &mut FillParties {
                maps: &mut maps,
                makers_and_referrer: &makers_and_referrers,
                makers_and_referrer_stats: &maker_and_referrer_stats,
            },
            &mut OfferedLiquidity {
                router: &mut router_inputs,
            },
            &mut FillerSide {
                user: &mut Some(&mut filler),
                stats: &mut Some(&mut filler_stats),
                key: filler_key,
                rev_share_escrow: &mut None,
            },
        )
        .unwrap();
        taker.orders[0] = order;

        // The whole quoted ladder was routed and settled: the executor was
        // asked for the 0.25 it offered, the thin maker holds exactly that,
        // and the vAMM covered the taker's remainder.
        let requested = executor.requested;
        assert_eq!(
            requested,
            BASE_PRECISION_U64 / 4,
            "the pass routes the ladder it was handed"
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

        let market_after = maps.perp_market_map.get_ref(&0).unwrap();
        assert_eq!(
            market_after.amm.base_asset_amount_with_amm,
            (AMM_RESERVE_PRECISION / 2) as i128 + (BASE_PRECISION_U64 - requested) as i128
        );
    }

    /// One fill takes a CLOB book and a Custom PropAMM in the same pass, and
    /// the pass reports the worse of the two prices.
    ///
    /// This is what lets `crank_cross_match` middle a PropAMM against the
    /// book. Its legs used to name one source each and read that source's
    /// resting ladder to bound themselves, which only a book can answer — so
    /// a PropAMM could never be a leg. A leg is now an ordinary router fill
    /// over every source the transaction carries, and the worst price it
    /// reports is what the other leg is measured against.
    #[test]
    fn a_clob_and_a_custom_book_fill_one_pass_and_the_worst_price_spans_both() {
        use crate::state::prop_amm::{
            CompletedOrderV0, DirectionV0, ExternalQuoterExecutor, PriceLevelV0, QuoterType,
            UserBalanceChangeV0, UserRefV0,
        };

        /// Book 0 is a CLOB, book 1 a Custom PropAMM. Each answers for its
        /// own maker at its own price.
        struct MockTwoBookExecutor {
            users: [Pubkey; 2],
            user_refs: [UserRefV0; 2],
            prices: [u64; 2],
            requested: [u64; 2],
        }

        impl ExternalQuoterExecutor<'static> for MockTwoBookExecutor {
            fn quoter_type(&self, index: usize) -> QuoterType {
                if index == 0 {
                    QuoterType::Clob
                } else {
                    QuoterType::Custom
                }
            }
            fn quoter_user(&self, index: usize) -> Pubkey {
                self.users[index]
            }
            fn quoter_key(&self, index: usize) -> Pubkey {
                self.users[index]
            }
            fn subjects(
                &self,
                index: usize,
                _direction: DirectionV0,
                _size: u64,
            ) -> crate::error::VelocityResult<crate::state::prop_amm::QuoterSubjects> {
                Ok(if index == 0 {
                    crate::state::prop_amm::QuoterSubjects::Book
                } else {
                    crate::state::prop_amm::QuoterSubjects::Account(self.users[index])
                })
            }
            fn execute(
                &mut self,
                index: usize,
                _direction: DirectionV0,
                size: u64,
            ) -> crate::error::VelocityResult<crate::state::prop_amm::ResponseLocationV0<'static>>
            {
                self.requested[index] = size;
                let quote_size = ((size as u128) * (self.prices[index] as u128)
                    / BASE_PRECISION_U64 as u128) as u64;
                let completed: &[CompletedOrderV0] = if index == 0 {
                    &[CompletedOrderV0 {
                        order_id: 1,
                        change_index: 0,
                        flags: 0,
                        _pad: [0; 1],
                        client_order_id: 0,
                    }]
                } else {
                    &[]
                };

                Ok(response_account(
                    &[UserBalanceChangeV0 {
                        base_size: size,
                        quote_size,
                        user: self.user_refs[index],
                        _pad: [0; 6],
                    }],
                    completed,
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
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);

        // Buys 1 with a 105 limit: room for both books and for the vAMM,
        // which quotes above both and so wins nothing.
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 105 * PRICE_PRECISION_U64,
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
                scaled_balance: 1000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        // The book's maker holds the open-order aggregates a velocity-mediated
        // CLOB placement reserves, and no velocity order.
        let clob_maker_key =
            Pubkey::from_str("My11111111111111111111111111111111111111113").unwrap();
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

        create_anchor_account_info!(clob_maker, &clob_maker_key, User, clob_maker_info);
        let mut makers_and_referrers = UserMap::load_one(&clob_maker_info).unwrap();

        // The PropAMM's quoted user reserves nothing — its depth is bounded
        // by the pre-execute margin clamp instead, and its collateral here is
        // deep enough that the clamp keeps the whole half.
        let custom_maker_key =
            Pubkey::from_str("CLoB111111111111111111111111111111111111111").unwrap();
        let custom_maker_authority =
            Pubkey::from_str("9Q5nmiZjHJK8QPGevKJnnLKtSXjZ4Q2r8bTHTNiFEf1").unwrap();
        let mut custom_maker = User {
            authority: custom_maker_authority,
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 100 * 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        create_anchor_account_info!(custom_maker, &custom_maker_key, User, custom_maker_info);
        makers_and_referrers.0.insert(
            custom_maker_key,
            anchor_lang::prelude::AccountLoader::try_from(&custom_maker_info).unwrap(),
        );

        let mut filler = User::default();
        let fee_structure = get_fee_structure();
        let (taker_key, _, filler_key) = get_user_keys();
        let mut taker_stats = UserStats::default();
        let mut clob_maker_stats = UserStats {
            authority: clob_maker_authority,
            ..UserStats::default()
        };

        create_anchor_account_info!(clob_maker_stats, UserStats, clob_maker_stats_info);
        let mut maker_and_referrer_stats = UserStatsMap::load_one(&clob_maker_stats_info).unwrap();
        let mut custom_maker_stats = UserStats {
            authority: custom_maker_authority,
            ..UserStats::default()
        };

        create_anchor_account_info!(custom_maker_stats, UserStats, custom_maker_stats_info);
        maker_and_referrer_stats.0.insert(
            custom_maker_authority,
            anchor_lang::prelude::AccountLoader::try_from(&custom_maker_stats_info).unwrap(),
        );

        let mut filler_stats = UserStats::default();

        // The CLOB at 99 and the PropAMM at 100. The split is price-first, so
        // the book's half goes first and the PropAMM's half sets the worst
        // price of the pass.
        let clob_levels = [PriceLevelV0 {
            price: 99 * PRICE_PRECISION_U64,
            size: BASE_PRECISION_U64 / 2,
        }];
        let custom_levels = [PriceLevelV0 {
            price: 100 * PRICE_PRECISION_U64,
            size: BASE_PRECISION_U64 / 2,
        }];
        let external_books = [
            crate::math::router::QuoterBook {
                priority: QuoterType::Clob.default_priority(),
                levels: &clob_levels,
                withheld: PriceLevelV0::default(),
            },
            crate::math::router::QuoterBook {
                priority: QuoterType::Custom.default_priority(),
                levels: &custom_levels,
                withheld: PriceLevelV0::default(),
            },
        ];
        let mut executor = MockTwoBookExecutor {
            users: [clob_maker_key, custom_maker_key],
            user_refs: [
                UserRefV0 {
                    authority: clob_maker_authority,
                    sub_account_id: 0,
                },
                UserRefV0 {
                    authority: custom_maker_authority,
                    sub_account_id: 0,
                },
            ],

            prices: [99 * PRICE_PRECISION_U64, 100 * PRICE_PRECISION_U64],
            requested: [0; 2],
        };
        let mut router_inputs = crate::math::router::RouterLeg {
            books: &external_books,
            executor: &mut executor,
            standing: crate::instructions::FillerStanding {
                protocol_authority: Pubkey::default(),
                taker_exposure_closed_by_caller: false,
                // Test fixtures stand in for a taker-signed fill: no filler
                // obligation, so a withheld book does not end the pass.
                obligation: crate::math::router::FillerObligation {
                    taker_signed: true,
                    tx_accounts: None,
                    unrouted_quoters: 0,
                },
            },

            worst_fill_price: None,
        };

        let mut order = taker.orders[0];
        let FillAmounts {
            base: base_asset_amount,
            quote: quote_asset_amount,
        } = fill_within_taker_risk_limits(
            &mut TakerSide::bind(&mut taker, &mut taker_stats, taker_key, &mut order, true)
                .unwrap(),
            &PricingRules {
                fee_structure: &fee_structure,
                validity_guard_rails: &crate::state::state::ValidityGuardRails::default(),
                promo_fee_tier: 0,
                referrer_is_accelerated: false,
                vamm_maker_rebate: false,
                // The taker layer takes this decision itself and overrides it.
                builder_fee_allowed: false,
            },
            &FillConditions::for_layer_test(
                FillMode::Fill,
                now,
                slot,
                Some(market.market_stats.historical_oracle_data.last_oracle_price),
                true,
                false,
            ),
            &mut FillParties {
                maps: &mut maps,
                makers_and_referrer: &makers_and_referrers,
                makers_and_referrer_stats: &maker_and_referrer_stats,
            },
            &mut OfferedLiquidity {
                router: &mut router_inputs,
            },
            &mut FillerSide {
                user: &mut Some(&mut filler),
                stats: &mut Some(&mut filler_stats),
                key: filler_key,
                rev_share_escrow: &mut None,
            },
        )
        .unwrap();
        // The PropAMM's 100 is the worst of the two, so a cross leg built
        // this way is measured against 100 rather than the 99.5 blend.
        let worst_fill_price = router_inputs.worst_fill_price;
        let requested = executor.requested;

        // Both sources filled their half, in one pass, with no vAMM slice.
        assert_eq!(base_asset_amount, BASE_PRECISION_U64);
        assert_eq!(
            quote_asset_amount,
            (99 * QUOTE_PRECISION_I64 / 2 + 100 * QUOTE_PRECISION_I64 / 2) as u64
        );
        assert_eq!(requested, [BASE_PRECISION_U64 / 2; 2]);
        assert_eq!(
            taker.perp_positions[0].base_asset_amount,
            BASE_PRECISION_I64
        );

        let clob_maker_after = makers_and_referrers.get_ref(&clob_maker_key).unwrap();
        assert_eq!(
            clob_maker_after.perp_positions[0].base_asset_amount,
            -BASE_PRECISION_I64 / 2
        );

        // The book's reservation unwound; the PropAMM had none to unwind.
        assert_eq!(clob_maker_after.perp_positions[0].open_asks, 0);
        assert_eq!(clob_maker_after.perp_positions[0].open_orders, 0);
        let custom_maker_after = makers_and_referrers.get_ref(&custom_maker_key).unwrap();
        assert_eq!(
            custom_maker_after.perp_positions[0].base_asset_amount,
            -BASE_PRECISION_I64 / 2
        );
        assert_eq!(custom_maker_after.perp_positions[0].open_asks, 0);

        let market_after = maps.perp_market_map.get_ref(&0).unwrap();
        assert_eq!(
            market_after.amm.base_asset_amount_with_amm,
            (AMM_RESERVE_PRECISION / 2) as i128
        );

        assert_eq!(worst_fill_price, Some(100 * PRICE_PRECISION_U64));
    }
}

#[cfg(test)]
mod amm_house_capture {
    //! Unit tests for `settle_amm_house_normal_quote`: the shade capture and
    //! the limit cap on a normal (non-post_only) sole-AMM fill.
    use {
        super::super::settle_amm_house_normal_quote,
        crate::{
            controller::position::PositionDirection, math::constants::BASE_PRECISION_U64,
            state::quoter::QuoterFill,
        },
    };

    /// A live-curve fill of one base unit at `quote`, with an existing AMM
    /// spread surplus. Prices are PRICE_PRECISION; a per-unit price on one
    /// base unit equals the notional.
    fn curve_fill(side: PositionDirection, quote: u64, surplus: i64) -> QuoterFill {
        QuoterFill {
            side,
            base_filled: BASE_PRECISION_U64,
            quote_filled: quote,
            quote_asset_amount_surplus: surplus,
            ..QuoterFill::ZERO
        }
    }

    #[test]
    fn long_charges_the_shade_and_books_the_gap() {
        let fill = curve_fill(PositionDirection::Long, 50_000_000, 100_000);
        // The router quoted the slice at 51 (taker-worse than the 50 curve).
        let (quote, surplus) = settle_amm_house_normal_quote(
            &fill,
            PositionDirection::Long,
            None,
            51_000_000,
            BASE_PRECISION_U64,
        )
        .unwrap();
        assert_eq!(quote, 51_000_000);
        // Existing spread surplus plus the captured shade gap.
        assert_eq!(surplus, 100_000 + 1_000_000);
    }

    #[test]
    fn short_charges_the_shade_and_books_the_gap() {
        let fill = curve_fill(PositionDirection::Short, 50_000_000, 100_000);
        // A short's shade is taker-worse when the taker receives less.
        let (quote, surplus) = settle_amm_house_normal_quote(
            &fill,
            PositionDirection::Short,
            None,
            49_000_000,
            BASE_PRECISION_U64,
        )
        .unwrap();
        assert_eq!(quote, 49_000_000);
        assert_eq!(surplus, 100_000 + 1_000_000);
    }

    #[test]
    fn a_taker_favorable_allocation_never_lowers_the_charge() {
        // Rounding could make the allocation quote look better than the
        // curve. The taker still pays the curve; no leak the other way.
        let fill = curve_fill(PositionDirection::Long, 50_000_000, 100_000);
        let (quote, surplus) = settle_amm_house_normal_quote(
            &fill,
            PositionDirection::Long,
            None,
            49_000_000,
            BASE_PRECISION_U64,
        )
        .unwrap();
        assert_eq!(quote, 50_000_000);
        assert_eq!(surplus, 100_000);
    }

    #[test]
    fn long_never_charged_worse_than_its_limit() {
        // The curve charges 50 but the taker's limit is 49.5. Cap at the
        // limit and book the improvement against the surplus.
        let fill = curve_fill(PositionDirection::Long, 50_000_000, 100_000);
        let (quote, surplus) =
            settle_amm_house_normal_quote(&fill, PositionDirection::Long, Some(49_500_000), 0, 0)
                .unwrap();
        assert_eq!(quote, 49_500_000);
        assert_eq!(surplus, 100_000 - 500_000);
    }

    #[test]
    fn short_never_receives_less_than_its_limit() {
        // The curve returns 50 but the taker's limit demands at least 50.5.
        let fill = curve_fill(PositionDirection::Short, 50_000_000, 100_000);
        let (quote, surplus) =
            settle_amm_house_normal_quote(&fill, PositionDirection::Short, Some(50_500_000), 0, 0)
                .unwrap();
        assert_eq!(quote, 50_500_000);
        assert_eq!(surplus, 100_000 - 500_000);
    }

    #[test]
    fn the_limit_caps_the_shade() {
        // Shade would push to 51, but the limit caps at 50.5.
        let fill = curve_fill(PositionDirection::Long, 50_000_000, 100_000);
        let (quote, surplus) = settle_amm_house_normal_quote(
            &fill,
            PositionDirection::Long,
            Some(50_500_000),
            51_000_000,
            BASE_PRECISION_U64,
        )
        .unwrap();
        assert_eq!(quote, 50_500_000);
        assert_eq!(surplus, 100_000 + 500_000);
    }

    #[test]
    fn a_partial_fill_scales_the_shade_taker_worse() {
        // Half the allocation base filled: charge half the shaded quote,
        // rounded taker-worse (up for a long).
        let fill = QuoterFill {
            side: PositionDirection::Long,
            base_filled: BASE_PRECISION_U64 / 2,
            quote_filled: 25_000_000,
            quote_asset_amount_surplus: 0,
            ..QuoterFill::ZERO
        };
        let (quote, surplus) = settle_amm_house_normal_quote(
            &fill,
            PositionDirection::Long,
            None,
            51_000_001, // odd so the ceil is observable
            BASE_PRECISION_U64,
        )
        .unwrap();
        assert_eq!(quote, 25_500_001);
        assert_eq!(surplus, 500_001);
    }
}

/// A book's report is held to what velocity reserved for the user it names.
///
/// The reservation is the only record velocity keeps of a plain CLOB order, and
/// it is written under the owner's own signature — so it is what stops a book
/// from opening a position for someone who never placed one, or from filling
/// more than they posted.
#[cfg(test)]
pub mod hostile_book_reports {
    use {
        super::*,
        crate::{
            controller::position::{
                release_reserved_open_base, release_reserved_open_orders, PositionDirection,
            },
            error::ErrorCode,
            math::constants::{BASE_PRECISION_I64, BASE_PRECISION_U64},
            state::{
                prop_amm::QuoterType,
                user::{OrderBitFlag, OrderStatus, OrderTriggerCondition, OrderType, User},
            },
        },
    };

    fn resting(direction: PositionDirection, base: u64) -> PerpPosition {
        let mut position = PerpPosition {
            market_index: 0,
            open_orders: 1,
            ..PerpPosition::default()
        };

        match direction {
            PositionDirection::Long => position.open_bids = base as i64,
            PositionDirection::Short => position.open_asks = -(base as i64),
        }

        position
    }

    #[test]
    fn reserved_open_base_reads_the_side_the_orders_rest_on() {
        let position = PerpPosition {
            market_index: 0,
            open_bids: 3 * BASE_PRECISION_I64,
            open_asks: -2 * BASE_PRECISION_I64,
            ..PerpPosition::default()
        };

        assert_eq!(
            position.reserved_open_base(PositionDirection::Long),
            3 * BASE_PRECISION_U64
        );
        assert_eq!(
            position.reserved_open_base(PositionDirection::Short),
            2 * BASE_PRECISION_U64
        );

        // A position with nothing resting reserves nothing on either side,
        // which is the case that stops a book naming a stranger.
        let idle = PerpPosition::default();
        assert_eq!(idle.reserved_open_base(PositionDirection::Long), 0);
        assert_eq!(idle.reserved_open_base(PositionDirection::Short), 0);
    }

    #[test]
    fn a_release_up_to_the_reservation_is_exact() {
        let mut position = resting(PositionDirection::Short, BASE_PRECISION_U64);
        release_reserved_open_base(
            &mut position,
            &PositionDirection::Short,
            BASE_PRECISION_U64 / 4,
        )
        .unwrap();
        assert_eq!(position.open_asks, -3 * BASE_PRECISION_I64 / 4);

        release_reserved_open_base(
            &mut position,
            &PositionDirection::Short,
            3 * BASE_PRECISION_U64 / 4,
        )
        .unwrap();
        assert_eq!(position.open_asks, 0);
    }

    #[test]
    fn a_release_past_the_reservation_fails_instead_of_clamping() {
        let mut position = resting(PositionDirection::Short, BASE_PRECISION_U64);
        assert_eq!(
            release_reserved_open_base(
                &mut position,
                &PositionDirection::Short,
                BASE_PRECISION_U64 + 1,
            ),
            Err(ErrorCode::QuoterReportExceedsReservation)
        );
        assert_eq!(position.open_asks, -BASE_PRECISION_I64);
    }

    #[test]
    fn a_user_with_nothing_resting_cannot_be_named_at_all() {
        let mut position = PerpPosition {
            market_index: 0,
            ..PerpPosition::default()
        };

        assert_eq!(
            release_reserved_open_base(&mut position, &PositionDirection::Long, 1),
            Err(ErrorCode::QuoterReportExceedsReservation)
        );
    }

    /// A report on the side the user did not post on is the same as a report
    /// against a user who posted nothing.
    #[test]
    fn a_release_on_the_other_side_is_not_covered_by_this_sides_reservation() {
        let mut position = resting(PositionDirection::Short, BASE_PRECISION_U64);
        assert_eq!(
            release_reserved_open_base(&mut position, &PositionDirection::Long, 1),
            Err(ErrorCode::QuoterReportExceedsReservation)
        );
    }

    #[test]
    fn retired_order_counts_are_held_to_the_slots_that_are_open() {
        let mut position = PerpPosition {
            market_index: 0,
            open_orders: 2,
            ..PerpPosition::default()
        };

        release_reserved_open_orders(&mut position, 2).unwrap();
        assert_eq!(position.open_orders, 0);
        assert_eq!(
            release_reserved_open_orders(&mut position, 1),
            Err(ErrorCode::QuoterReportExceedsReservation)
        );
    }

    /// An evicted trigger's shadow row takes back the remainder the book
    /// reports. A report above the row's own size would grow the order.
    #[test]
    fn an_evicted_trigger_cannot_be_re_armed_larger_than_it_was() {
        let placed = Order {
            status: OrderStatus::Open,
            market_index: 0,
            market_type: MarketType::Perp,
            order_type: OrderType::TriggerLimit,
            trigger_condition: OrderTriggerCondition::TriggeredAbove,
            base_asset_amount: BASE_PRECISION_U64,
            bit_flags: OrderBitFlag::PlacedOnClob as u8,
            ..Order::default()
        };
        let mut user = User {
            perp_positions: crate::test_utils::get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                ..PerpPosition::default()
            }),

            orders: crate::test_utils::get_orders(placed),
            ..User::default()
        };

        user.orders[0].set_clob_order_ref(0, 7);

        assert_eq!(
            user.re_arm_placed_trigger_slot(0, 7, BASE_PRECISION_U64 + 1, 0),
            Err(ErrorCode::QuoterReportExceedsReservation)
        );

        // The honest case still re-arms with the unfilled remainder.
        assert!(user
            .re_arm_placed_trigger_slot(0, 7, BASE_PRECISION_U64 / 2, 0)
            .unwrap());
        assert_eq!(user.orders[0].base_asset_amount, BASE_PRECISION_U64 / 2);
    }

    /// A maker's declared band only ever tightens the market's.
    #[test]
    fn a_declared_band_cannot_widen_the_markets() {
        let mut config = crate::state::prop_amm::QuoterConfigV0 {
            quoter_type: QuoterType::Custom,
            ..Default::default()
        };

        // Undeclared: the market's band stands.
        assert_eq!(config.oracle_band(1000), 1000);

        // Tighter: the declaration stands.
        config.max_oracle_deviation_bps = 250;
        assert_eq!(config.oracle_band(1000), 250);

        // Wider than the market's: the market's still stands.
        config.max_oracle_deviation_bps = 5000;
        assert_eq!(config.oracle_band(1000), 1000);
    }
}
