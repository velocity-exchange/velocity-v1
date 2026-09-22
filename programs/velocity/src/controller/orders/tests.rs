use {
    crate::{
        math::{
            oracle::oracle_validity,
            time::{legacy_slot_duration_i64, legacy_slot_duration_u8, SlotClock},
        },
        state::{
            fill_mode::FillMode,
            market_status::MarketStatus,
            oracle_map::OracleMap,
            perp_market::PerpMarket,
            state::{FeeStructure, FeeTier, State},
            user::{MarketType, Order, PerpPosition},
        },
    },
    anchor_lang::prelude::Pubkey,
};
#[test]
fn validate_spot_dlob_trading_enabled_for_market_type_rejects_spot() {
    let result = super::validate_spot_dlob_trading_enabled_for_market_type(MarketType::Spot);
    assert_eq!(
        result,
        Err(crate::error::ErrorCode::SpotDlobTradingDisabled)
    );
}
#[test]
fn validate_spot_dlob_trading_enabled_for_market_type_allows_perp() {
    let result = super::validate_spot_dlob_trading_enabled_for_market_type(MarketType::Perp);
    assert_eq!(result, Ok(()));
}
fn get_fee_structure() -> FeeStructure {
    let mut fee_tiers = [FeeTier::default(); 10];
    fee_tiers[0] = FeeTier {
        fee_numerator: 5,
        fee_denominator: 10000,
        maker_rebate_numerator: 3,
        maker_rebate_denominator: 10000,
        ..FeeTier::default()
    };
    FeeStructure {
        fee_tiers,
        ..FeeStructure::test_default()
    }
}

/// Distinct keys, in (taker, maker, filler) order. They must not alias: the
/// fill path distinguishes a self-fill (`filler_key` == the taker) from a maker
/// that cranked its own fill (`filler_key` == a maker) purely by key, and the
/// two earn different rewards.
fn get_user_keys() -> (Pubkey, Pubkey, Pubkey) {
    (
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    )
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
        crate::math::oracle::LogMode::SafeMMOracle,
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

/// Router inputs with no external quoters — the fill routes across the vAMM
/// alone. Two locals rather than a helper
/// because `RouterLeg` borrows its executor.
macro_rules! no_router {
    ($name:ident) => {
        let mut no_externals = crate::state::prop_amm::NoExternalQuoters;
        let mut $name = crate::math::router::RouterLeg {
            books: &[],
            executor: &mut no_externals,
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
    };
}

pub mod fulfill_order {
    use {
        super::*,
        crate::{
            controller::{
                orders::{
                    fill_perp_order_without_external_books, fill_within_taker_risk_limits,
                    validate_market_within_price_band, FillAmounts, FillConditions, FillParties,
                    FillerSide, OfferedLiquidity, PricingRules, TakerSide,
                },
                position::PositionDirection,
            },
            create_anchor_account_info,
            instructions::optional_accounts::AccountMaps,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I64, BASE_PRECISION_U64,
                    MAX_CONCENTRATION_COEFFICIENT, PEG_PRECISION, PRICE_PRECISION,
                    PRICE_PRECISION_I64, PRICE_PRECISION_U64, QUOTE_PRECISION_I64,
                    SPOT_BALANCE_PRECISION_U64, SPOT_CUMULATIVE_INTEREST_PRECISION,
                    SPOT_WEIGHT_PRECISION,
                },
                time::SlotClock,
            },
            state::{
                fill_mode::FillMode,
                oracle::{HistoricalOracleData, OracleSource},
                perp_market::{MarketStats, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                state::{OracleGuardRails, State, ValidityGuardRails},
                user::{OrderStatus, OrderType, SpotPosition, User, UserStats},
                user_map::{UserMap, UserStatsMap},
            },
            test_utils::{get_orders, get_positions, get_pyth_price, get_spot_positions},
            PERCENTAGE_PRECISION_U64,
        },
        std::{str::FromStr, u64},
    };
    #[test]
    fn validate_market_within_price_band_tests() {
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_spread: 100,
                max_spread: 1000,
                ..AMM::default()
            },

            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
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
        let mut state = State {
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
        let oracle_price = market.market_stats.historical_oracle_data.last_oracle_price;
        // valid initial state
        assert!(validate_market_within_price_band(&market, &state, oracle_price).unwrap());
        // twap_5min $50 and mark $100 breaches 10% divergence -> failure
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min = 50 * PRICE_PRECISION as i64;
        assert!(validate_market_within_price_band(&market, &state, oracle_price).is_err());
        // within 60% ok -> success
        state
            .oracle_guard_rails
            .price_divergence
            .mark_oracle_percent_divergence = 6 * PERCENTAGE_PRECISION_U64 / 10;
        assert!(validate_market_within_price_band(&market, &state, oracle_price).unwrap());
        // twap_5min $20 and mark $100 breaches 60% divergence -> failure
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min = 20 * PRICE_PRECISION as i64;
        assert!(validate_market_within_price_band(&market, &state, oracle_price).is_err());
    }
    #[test]
    fn fulfill_with_amm_skip_auction_duration() {
        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            _oracle_account_info
        );

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

            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
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

        market.amm.max_base_asset_reserve = i128::MAX as u128;
        market.amm.min_base_asset_reserve = 0;
        let mut state = State {
            min_perp_auction_duration: legacy_slot_duration_u8(1),
            default_market_order_time_in_force: 10,
            ..State::default()
        };

        assert!(!market.can_skip_auction_duration(&state, false).unwrap());
        market.amm.net_revenue_since_last_funding = 1;
        assert!(!market.can_skip_auction_duration(&state, false).unwrap());
        assert!(market.can_skip_auction_duration(&state, true).unwrap());
        assert!(!state.amm_immediate_fill_paused().unwrap());
        state.exchange_status = 0b10000000;
        assert!(state.amm_immediate_fill_paused().unwrap());
        assert!(!market.can_skip_auction_duration(&state, true).unwrap());
    }
    #[test]
    fn fulfill_with_amm_routes_off_projected_reserve_price() {
        // Stale-curve deadlock regression: the stored curve sits at 100 while
        // the oracle has moved to 102. The taker sells at 101.9, crossable
        // against the projected (post-refresh) AMM bid near 102, but not
        // against the stale stored bid at 100. Routing must quote off the
        // projected curve, otherwise the fill that would refresh the curve
        // is the one being blocked.
        let now = 0_i64;
        let slot = 5_u64;
        let mut oracle_price = get_pyth_price(102, 6);
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
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                terminal_quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                concentration_coef: MAX_CONCENTRATION_COEFFICIENT,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_spread: 0,
                max_spread: 1000,
                curve_update_intensity: 100,
                ..AMM::default()
            },

            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 1000,
            order_tick_size: 1,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price: (102 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap: (102 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap_5min: (102 * PRICE_PRECISION) as i64,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
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
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_duration: 0,
                price: 101_900_000, // 101.9: crosses projected bid, not stale bid
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
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
        let mut filler = User::default();
        let fee_structure = get_fee_structure();
        let (taker_key, _, filler_key) = get_user_keys();
        let mut taker_stats = UserStats::default();
        let mut filler_stats = UserStats::default();
        let order_index = 0;
        let user_can_skip_auction_duration = taker
            .can_skip_auction_duration(&taker_stats, false)
            .unwrap();
        let is_amm_available = get_amm_is_available(
            &taker.orders[order_index],
            0,
            &market,
            &mut maps.oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        assert!(is_amm_available);
        no_router!(router);
        no_router!(router);
        let mut order = taker.orders[order_index];
        let FillAmounts {
            base: base_asset_amount,
            quote: quote_asset_amount,
        } = fill_within_taker_risk_limits(
            &mut TakerSide::bind(&mut taker, &mut taker_stats, taker_key, &mut order, true)
                .unwrap(),
            &PricingRules {
                fee_structure: &fee_structure,
                validity_guard_rails: &crate::state::state::OracleGuardRails::default().validity,
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
                is_amm_available,
                false,
            ),
            &mut FillParties {
                maps: &mut maps,
                makers_and_referrer: &UserMap::empty(),
                makers_and_referrer_stats: &UserStatsMap::empty(),
            },
            &mut OfferedLiquidity {
                router: &mut router,
            },
            &mut FillerSide {
                user: &mut Some(&mut filler),
                stats: &mut Some(&mut filler_stats),
                key: filler_key,
                rev_share_escrow: &mut None,
            },
        )
        .unwrap();
        taker.orders[order_index] = order;
        // Fill happened against the projected curve near the oracle price,
        // impossible against the stale stored bid at 100 (101.9 > 100 never
        // crosses). Partial: the sell walks the curve from ~102 down to the
        // 101.9 limit price.
        assert!(base_asset_amount > 0);
        let avg_fill_price =
            quote_asset_amount as u128 * BASE_PRECISION_U64 as u128 / base_asset_amount as u128;
        assert!(avg_fill_price > 101_900_000 && avg_fill_price < 102_100_000);
        // The executed curve matches the routing projection: the AMM snapped
        // toward the oracle before quoting (then the sell moved it back down
        // a touch), so the post-fill reserve price sits near 102, not 100.
        let market_after = maps.perp_market_map.get_ref(&0).unwrap();
        let reserve_price_after = market_after.amm.reserve_price().unwrap();
        assert!(reserve_price_after > 101 * PRICE_PRECISION as u64);
    }
    #[test]
    fn fulfill_with_amm_projection_passthrough_keeps_stale_routing() {
        // Companion to fulfill_with_amm_routes_off_projected_reserve_price:
        // when the projection is a passthrough (curve_update_intensity == 0),
        // routing must behave exactly as before: the taker does not cross
        // the stored curve and no fulfillment method is selected.
        let now = 0_i64;
        let slot = 5_u64;
        let mut oracle_price = get_pyth_price(102, 6);
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
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                terminal_quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                concentration_coef: MAX_CONCENTRATION_COEFFICIENT,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_spread: 0,
                max_spread: 1000,
                curve_update_intensity: 0,
                ..AMM::default()
            },

            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 1000,
            order_tick_size: 1,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price: (102 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap: (102 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap_5min: (102 * PRICE_PRECISION) as i64,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
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
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_duration: 0,
                price: 101_900_000,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
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
        let mut filler = User::default();
        let fee_structure = get_fee_structure();
        let (taker_key, _, filler_key) = get_user_keys();
        let mut taker_stats = UserStats::default();
        let mut filler_stats = UserStats::default();
        let order_index = 0;
        let user_can_skip_auction_duration = taker
            .can_skip_auction_duration(&taker_stats, false)
            .unwrap();
        let is_amm_available = get_amm_is_available(
            &taker.orders[order_index],
            0,
            &market,
            &mut maps.oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        no_router!(router);
        let mut order = taker.orders[order_index];
        let FillAmounts {
            base: base_asset_amount,
            quote: quote_asset_amount,
        } = fill_within_taker_risk_limits(
            &mut TakerSide::bind(&mut taker, &mut taker_stats, taker_key, &mut order, true)
                .unwrap(),
            &PricingRules {
                fee_structure: &fee_structure,
                validity_guard_rails: &crate::state::state::OracleGuardRails::default().validity,
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
                is_amm_available,
                false,
            ),
            &mut FillParties {
                maps: &mut maps,
                makers_and_referrer: &UserMap::empty(),
                makers_and_referrer_stats: &UserStatsMap::empty(),
            },
            &mut OfferedLiquidity {
                router: &mut router,
            },
            &mut FillerSide {
                user: &mut Some(&mut filler),
                stats: &mut Some(&mut filler_stats),
                key: filler_key,
                rev_share_escrow: &mut None,
            },
        )
        .unwrap();
        taker.orders[order_index] = order;
        // No projection, no cross, no fill; pre-change behavior preserved.
        assert_eq!(base_asset_amount, 0);
        assert_eq!(quote_asset_amount, 0);
        let market_after = maps.perp_market_map.get_ref(&0).unwrap();
        let reserve_price_after = market_after.amm.reserve_price().unwrap();
        assert_eq!(reserve_price_after, 100 * PRICE_PRECISION as u64);
    }
    #[test]
    fn fulfill_no_cross_still_refreshes_curve() {
        // Routing projects and applies the refresh on the real AMM before
        // selecting a fulfillment method, so a stale-curve fill attempt that
        // finds no crossing method still snaps the curve toward oracle before
        // returning zero. This is what lets the first fill step's `setup` skip
        // re-projecting, and it matches a permissionless `update_amms` crank:
        // the stored curve at 100 heals toward the oracle at 102 even though
        // the taker's short at 103 never crosses the projected bid near 102.
        let now = 0_i64;
        let slot = 5_u64;
        let mut oracle_price = get_pyth_price(102, 6);
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
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                terminal_quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                concentration_coef: MAX_CONCENTRATION_COEFFICIENT,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_spread: 0,
                max_spread: 1000,
                curve_update_intensity: 100,
                ..AMM::default()
            },

            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 1000,
            order_tick_size: 1,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price: (102 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap: (102 * PRICE_PRECISION) as i64,
                    last_oracle_price_twap_5min: (102 * PRICE_PRECISION) as i64,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
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
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_duration: 0,
                price: 103_000_000, // 103: above the projected bid near 102, never crosses
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
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
        let mut filler = User::default();
        let fee_structure = get_fee_structure();
        let (taker_key, _, filler_key) = get_user_keys();
        let mut taker_stats = UserStats::default();
        let mut filler_stats = UserStats::default();
        let order_index = 0;
        let user_can_skip_auction_duration = taker
            .can_skip_auction_duration(&taker_stats, false)
            .unwrap();
        let is_amm_available = get_amm_is_available(
            &taker.orders[order_index],
            0,
            &market,
            &mut maps.oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        no_router!(router);
        let mut order = taker.orders[order_index];
        let FillAmounts {
            base: base_asset_amount,
            quote: quote_asset_amount,
        } = fill_within_taker_risk_limits(
            &mut TakerSide::bind(&mut taker, &mut taker_stats, taker_key, &mut order, true)
                .unwrap(),
            &PricingRules {
                fee_structure: &fee_structure,
                validity_guard_rails: &crate::state::state::OracleGuardRails::default().validity,
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
                is_amm_available,
                false,
            ),
            &mut FillParties {
                maps: &mut maps,
                makers_and_referrer: &UserMap::empty(),
                makers_and_referrer_stats: &UserStatsMap::empty(),
            },
            &mut OfferedLiquidity {
                router: &mut router,
            },
            &mut FillerSide {
                user: &mut Some(&mut filler),
                stats: &mut Some(&mut filler_stats),
                key: filler_key,
                rev_share_escrow: &mut None,
            },
        )
        .unwrap();
        taker.orders[order_index] = order;
        // No cross, so no fill.
        assert_eq!(base_asset_amount, 0);
        assert_eq!(quote_asset_amount, 0);
        // The curve was still refreshed toward the oracle: reserve price
        // snapped from the stored 100 to near 102, and `last_update_slot`
        // advanced to this slot so a subsequent same-slot fill skips the
        // projection.
        let market_after = maps.perp_market_map.get_ref(&0).unwrap();
        let reserve_price_after = market_after.amm.reserve_price().unwrap();
        assert!(reserve_price_after > 101 * PRICE_PRECISION as u64);
        assert!(reserve_price_after < 103 * PRICE_PRECISION as u64);
        assert_eq!(market_after.amm.last_update_slot, slot);
    }
    #[test]
    fn fulfill_with_amm_end_of_auction() {
        let now = 0_i64;
        let slot = 6_u64;
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
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 10,
                max_fill_reserve_fraction: 100,
                ..AMM::default()
            },

            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
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
                auction_start_price: 0,
                auction_end_price: 100 * PRICE_PRECISION_I64,
                auction_duration: 5,
                price: 150 * PRICE_PRECISION_U64,
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
        let fee_structure = get_fee_structure();
        let (taker_key, _, filler_key) = get_user_keys();
        let mut taker_stats = UserStats::default();
        let order_index = 0;
        let min_auction_duration = 0;
        let user_can_skip_auction_duration = taker
            .can_skip_auction_duration(&taker_stats, false)
            .unwrap();
        let is_amm_available = get_amm_is_available(
            &taker.orders[order_index],
            min_auction_duration,
            &market,
            &mut maps.oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        no_router!(router);
        let mut order = taker.orders[order_index];
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
                is_amm_available,
                false,
            ),
            &mut FillParties {
                maps: &mut maps,
                makers_and_referrer: &UserMap::empty(),
                makers_and_referrer_stats: &UserStatsMap::empty(),
            },
            &mut OfferedLiquidity {
                router: &mut router,
            },
            &mut FillerSide {
                user: &mut None,
                stats: &mut None,
                key: filler_key,
                rev_share_escrow: &mut None,
            },
        )
        .unwrap();
        taker.orders[order_index] = order;
        assert_eq!(base_asset_amount, BASE_PRECISION_U64);
        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, -101060615);
        assert_eq!(taker_position.quote_entry_amount, -101010109);
        assert_eq!(taker_position.quote_break_even_amount, -101060615);
        assert_eq!(taker_position.open_bids, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 50506);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 101010109);
        assert!(taker.orders[0].is_available());
        let market_after = maps.perp_market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.amm.base_asset_amount_with_amm, 1000000000);
        assert_eq!(market_after.base_asset_amount_long, 1000000000);
        assert_eq!(market_after.base_asset_amount_short, 0);
        assert_eq!(market_after.quote_asset_amount, -101060615);
        // amm numerator is 0: the taker-fee remainder is the protocol's
        // pending carveout; the AMM books nothing (no surplus here)
        assert_eq!(market_after.fee_ledger.pending_protocol_fee, 50506);
        assert_eq!(market_after.amm.total_fee, 7);
        assert_eq!(market_after.amm.total_fee_minus_distributions, 7);
        assert_eq!(market_after.amm.net_revenue_since_last_funding, 7);
    }
    #[test]
    fn fulfill_post_only_ask_with_amm() {
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
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_spread: 0, // 1 basis point
                ..AMM::default()
            },

            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
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

        market.amm.max_base_asset_reserve = u64::MAX as u128;
        market.amm.min_base_asset_reserve = 0;
        let reserve_price_before = market.amm.reserve_price().unwrap();
        let bid_price = market.amm.bid_price(reserve_price_before, 0, 0).unwrap();
        println!("bid_price: {}", bid_price); // $100
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
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_duration: 0,
                price: 100 * PRICE_PRECISION_U64 - (PRICE_PRECISION_U64 / 10), // 99.9
                post_only: true,
                ..Order::default()
            }),

            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
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
        let makers_and_referrers = UserMap::empty();
        let mut filler = User::default();
        let fee_structure = get_fee_structure();
        let (taker_key, _, filler_key) = get_user_keys();
        let mut taker_stats = UserStats::default();
        let maker_and_referrer_stats = UserStatsMap::empty();
        let mut filler_stats = UserStats::default();
        let order_index = 0;
        let min_auction_duration = 0;
        let user_can_skip_auction_duration = taker
            .can_skip_auction_duration(&taker_stats, false)
            .unwrap();
        let is_amm_available = get_amm_is_available(
            &taker.orders[order_index],
            min_auction_duration,
            &market,
            &mut maps.oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        no_router!(router);
        let mut order = taker.orders[order_index];
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
                is_amm_available,
                false,
            ),
            &mut FillParties {
                maps: &mut maps,
                makers_and_referrer: &makers_and_referrers,
                makers_and_referrer_stats: &maker_and_referrer_stats,
            },
            &mut OfferedLiquidity {
                router: &mut router,
            },
            &mut FillerSide {
                user: &mut Some(&mut filler),
                stats: &mut Some(&mut filler_stats),
                key: filler_key,
                rev_share_escrow: &mut None,
            },
        )
        .unwrap();
        taker.orders[order_index] = order;
        assert_eq!(base_asset_amount, 35032000);
        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, -35032000);
        assert_eq!(taker_position.quote_asset_amount, 3500746);
        assert_eq!(taker_position.quote_entry_amount, 3499697);
        assert_eq!(taker_position.quote_break_even_amount, 3500746);
        assert_eq!(taker_stats.fees.total_fee_paid, 0);
        assert_eq!(taker_stats.fees.total_fee_rebate, 1049);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 0);
        assert_eq!(taker_stats.maker_volume_30d, 3499697);
        let market_after = maps.perp_market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.amm.base_asset_amount_with_amm, -35032000);
        assert_eq!(market_after.base_asset_amount_long, 0);
        assert_eq!(market_after.base_asset_amount_short, -35032000);
        assert_eq!(market_after.quote_asset_amount, 3500868);
        // amm numerator is 0: the spread-derived post-only house fee is the
        // protocol's pending carveout; the AMM books nothing
        assert_eq!(market_after.fee_ledger.pending_protocol_fee, 1105);
        assert_eq!(market_after.amm.total_fee, 0);
        assert_eq!(market_after.amm.total_fee_minus_distributions, 0);
        assert_eq!(market_after.amm.net_revenue_since_last_funding, 0);
        let market_after = maps.perp_market_map.get_ref(&0).unwrap();
        let reserve_price = market_after.amm.reserve_price().unwrap();
        let bid_price = market_after.amm.bid_price(reserve_price, 0, 0).unwrap();
        assert_eq!(bid_price, 99929972); // ~ 99.9 * (1.0003)
    }
    #[test]
    fn fulfill_post_only_bid_with_amm() {
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
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_spread: 0, // 1 basis point
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
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

        market.amm.max_base_asset_reserve = u64::MAX as u128;
        market.amm.min_base_asset_reserve = 0;
        let reserve_price_before = market.amm.reserve_price().unwrap();
        let bid_price = market.amm.bid_price(reserve_price_before, 0, 0).unwrap();
        println!("bid_price: {}", bid_price); // $100
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
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_duration: 0,
                price: 100 * PRICE_PRECISION_U64 + (PRICE_PRECISION_U64 / 10), // 100.1
                post_only: true,
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
        let makers_and_referrers = UserMap::empty();
        let mut filler = User::default();
        let fee_structure = get_fee_structure();
        let (taker_key, _, filler_key) = get_user_keys();
        let mut taker_stats = UserStats::default();
        let maker_and_referrer_stats = UserStatsMap::empty();
        let mut filler_stats = UserStats::default();
        let order_index = 0;
        let min_auction_duration = 0;
        let user_can_skip_auction_duration = taker
            .can_skip_auction_duration(&taker_stats, false)
            .unwrap();
        let is_amm_available = get_amm_is_available(
            &taker.orders[order_index],
            min_auction_duration,
            &market,
            &mut maps.oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        no_router!(router);
        let mut order = taker.orders[order_index];
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
                is_amm_available,
                false,
            ),
            &mut FillParties {
                maps: &mut maps,
                makers_and_referrer: &makers_and_referrers,
                makers_and_referrer_stats: &maker_and_referrer_stats,
            },
            &mut OfferedLiquidity {
                router: &mut router,
            },
            &mut FillerSide {
                user: &mut Some(&mut filler),
                stats: &mut Some(&mut filler_stats),
                key: filler_key,
                rev_share_escrow: &mut None,
            },
        )
        .unwrap();
        taker.orders[order_index] = order;
        assert_eq!(base_asset_amount, 34966000);
        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, 34966000);
        assert_eq!(taker_position.quote_asset_amount, -3499046);
        assert_eq!(taker_position.quote_entry_amount, -3500096);
        assert_eq!(taker_position.quote_break_even_amount, -3499046);
        assert_eq!(taker_stats.fees.total_fee_paid, 0);
        assert_eq!(taker_stats.fees.total_fee_rebate, 1050);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 0);
        assert_eq!(taker_stats.maker_volume_30d, 3500096);
        let market_after = maps.perp_market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.amm.base_asset_amount_with_amm, 34966000);
        assert_eq!(market_after.base_asset_amount_long, 34966000);
        assert_eq!(market_after.base_asset_amount_short, 0);
        assert_eq!(market_after.quote_asset_amount, -3498924);
        // amm numerator is 0: the spread-derived post-only house fee is the
        // protocol's pending carveout; the AMM books nothing
        assert_eq!(market_after.fee_ledger.pending_protocol_fee, 1100);
        assert_eq!(market_after.amm.total_fee, 0);
        assert_eq!(market_after.amm.total_fee_minus_distributions, 0);
        assert_eq!(market_after.amm.net_revenue_since_last_funding, 0);
        let market_after = maps.perp_market_map.get_ref(&0).unwrap();
        let reserve_price = market_after.amm.reserve_price().unwrap();
        let ask_price = market_after.amm.ask_price(reserve_price, 0, 0).unwrap();
        assert_eq!(ask_price, 100069968); // ~ 100.1 * (0.9997)
    }
    #[test]
    fn amm_unavailable_from_volatile_mm_oracle() {
        use anchor_lang::prelude::{AccountLoader, Clock};
        let slot = 56_u64;
        let clock = Clock {
            slot,
            epoch_start_timestamp: 0,
            epoch: 0,
            leader_schedule_epoch: 0,
            unix_timestamp: 0,
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
                terminal_quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_spread: 0, // 1 basis point
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 1000,
            order_tick_size: 1,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                mm_oracle_price: 102 * PRICE_PRECISION_I64,
                mm_oracle_slot: slot,
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
        market.status = MarketStatus::Active;
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
                auction_start_price: 0,
                auction_end_price: 100 * PRICE_PRECISION_I64,
                auction_duration: 0,
                price: 150 * PRICE_PRECISION_U64,
                order_id: 1,
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

        create_anchor_account_info!(taker, User, user_account_info);
        let user_account_loader: AccountLoader<User> =
            AccountLoader::try_from(&user_account_info).unwrap();
        create_anchor_account_info!(UserStats::default(), UserStats, user_stats_account_info);
        let user_stats_account_loader: AccountLoader<UserStats> =
            AccountLoader::try_from(&user_stats_account_info).unwrap();
        let filler_key = Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        create_anchor_account_info!(User::default(), &filler_key, User, user_account_info);
        let filler_account_loader: AccountLoader<User> =
            AccountLoader::try_from(&user_account_info).unwrap();
        create_anchor_account_info!(UserStats::default(), UserStats, filler_stats_account_info);
        let filler_stats_account_loader: AccountLoader<UserStats> =
            AccountLoader::try_from(&filler_stats_account_info).unwrap();
        let state = State {
            min_perp_auction_duration: legacy_slot_duration_u8(1),
            default_market_order_time_in_force: 10,
            ..State::default()
        };
        let mut taker_order = user_account_loader.load().unwrap().orders[0];
        let filled = fill_perp_order_without_external_books(
            &mut taker_order,
            true,
            &state,
            &user_account_loader,
            &user_stats_account_loader,
            &mut maps,
            &filler_account_loader,
            &filler_stats_account_loader,
            &UserMap::empty(),
            &UserStatsMap::empty(),
            &clock,
            FillMode::Fill,
            &mut None,
            false,
        )
        .unwrap();
        assert_eq!(filled.base, 0);
        // Will fill if MM oracle price is not too volatile at mm oracle price
        market.market_stats.mm_oracle_price = 101 * PRICE_PRECISION_I64;
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();
        maps.perp_market_map = perp_market_map;
        let mut taker_order = user_account_loader.load().unwrap().orders[0];
        let filled = fill_perp_order_without_external_books(
            &mut taker_order,
            true,
            &state,
            &user_account_loader,
            &user_stats_account_loader,
            &mut maps,
            &filler_account_loader,
            &filler_stats_account_loader,
            &UserMap::empty(),
            &UserStatsMap::empty(),
            &clock,
            FillMode::Fill,
            &mut None,
            false,
        )
        .unwrap();
        assert_eq!(filled.base, BASE_PRECISION_U64);
        assert_eq!(filled.quote, 101010102);
    }

    // Add back if we check free collateral in fill again
    // #[test]
    // fn fulfill_with_negative_free_collateral() {
    //     let now = 0_i64;
    //     let slot = 6_u64;
    //
    //     let mut oracle_price = get_pyth_price(100, 6);
    //     let oracle_price_key =
    //         Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
    //     let pyth_program = crate::ids::pyth_program::id();
    //     create_account_info!(
    //         oracle_price,
    //         &oracle_price_key,
    //         &pyth_program,
    //         oracle_account_info
    //     );
    //     let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, crate::math::time::SlotClock::baseline(), None).unwrap();
    //
    //     let mut market = PerpMarket {
    //         amm: AMM {
    //             base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
    //             quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
    //             bid_base_asset_reserve: 101 * AMM_RESERVE_PRECISION,
    //             bid_quote_asset_reserve: 99 * AMM_RESERVE_PRECISION,
    //             ask_base_asset_reserve: 99 * AMM_RESERVE_PRECISION,
    //             ask_quote_asset_reserve: 101 * AMM_RESERVE_PRECISION,
    //             sqrt_k: 100 * AMM_RESERVE_PRECISION,
    //             peg_multiplier: 100 * PEG_PRECISION,
    //             max_slippage_ratio: 10,
    //             max_fill_reserve_fraction: 100,
    //             order_step_size: 10000000,
    //             order_tick_size: 1,
    //             oracle: oracle_price_key,
    //             historical_oracle_data: HistoricalOracleData {
    //                 last_oracle_price: (100 * PRICE_PRECISION) as i64,
    //                 last_oracle_price_twap: (100 * PRICE_PRECISION) as i64,
    //                 last_oracle_price_twap_5min: (100 * PRICE_PRECISION) as i64,
    //
    //                 ..HistoricalOracleData::default()
    //             },
    //             ..AMM::default()
    //         },
    //         margin_ratio_initial: 1000,
    //         margin_ratio_maintenance: 500,
    //         status: MarketStatus::Initialized,
    //         ..PerpMarket::default_test()
    //     };
    //     market.amm.max_base_asset_reserve = u128::MAX;
    //     market.amm.min_base_asset_reserve = 0;
    //
    //     create_anchor_account_info!(market, PerpMarket, market_account_info);
    //     let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();
    //
    //     let mut spot_market = SpotMarket {
    //         market_index: 0,
    //         oracle_source: OracleSource::QuoteAsset,
    //         cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
    //         decimals: 6,
    //         initial_asset_weight: SPOT_WEIGHT_PRECISION,
    //         maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
    //         ..SpotMarket::default()
    //     };
    //     create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
    //     let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();
    //
    //     let mut taker = User {
    //         orders: get_orders(Order {
    //             market_index: 0,
    //             status: OrderStatus::Open,
    //             order_type: OrderType::Market,
    //             direction: PositionDirection::Long,
    //             base_asset_amount: 100 * BASE_PRECISION_U64,
    //             slot: 0,
    //             auction_start_price: 0,
    //             auction_end_price: 100 * PRICE_PRECISION_U64,
    //             auction_duration: 5,
    //             ..Order::default()
    //         }),
    //         perp_positions: get_positions(PerpPosition {
    //             market_index: 0,
    //             open_orders: 1,
    //             open_bids: 100 * BASE_PRECISION_I64,
    //             ..PerpPosition::default()
    //         }),
    //         spot_positions: get_spot_positions(SpotPosition {
    //             market_index: 0,
    //             balance_type: SpotBalanceType::Deposit,
    //             scaled_balance: SPOT_BALANCE_PRECISION_U64,
    //             ..SpotPosition::default()
    //         }),
    //         ..User::default()
    //     };
    //
    //     let _maker = User {
    //         orders: get_orders(Order {
    //             market_index: 0,
    //             post_only: true,
    //             order_type: OrderType::Limit,
    //             direction: PositionDirection::Short,
    //             base_asset_amount: BASE_PRECISION_U64 / 2,
    //             price: 100 * PRICE_PRECISION_U64,
    //             ..Order::default()
    //         }),
    //         perp_positions: get_positions(PerpPosition {
    //             market_index: 0,
    //             open_orders: 1,
    //             open_asks: -BASE_PRECISION_I64 / 2,
    //             ..PerpPosition::default()
    //         }),
    //         ..User::default()
    //     };
    //
    //     let fee_structure = get_fee_structure();
    //
    //     let (taker_key, _, filler_key) = get_user_keys();
    //
    //     let mut taker_stats = UserStats::default();
    //
    //     let (base_asset_amount, _) = fulfill_perp_order(
    //         &mut taker,
    //         0,
    //         &taker_key,
    //         &mut taker_stats,
    //         &mut None,
    //         &mut None,
    //         None,
    //         None,
    //         &mut None,
    //         &filler_key,
    //         &mut None,
    //         &mut None,
    //         &spot_market_map,
    //         &market_map,
    //         &mut oracle_map,
    //         &fee_structure,
    //         0,
    //         None,
    //         now,
    //         slot,
    //         false,
    //         true,
    //         &mut None,
    //         false    false,
    //         false    0,
    //         false)
    //     .unwrap();
    //
    //     assert_eq!(base_asset_amount, 0);
    //
    //     assert_eq!(taker.perp_positions[0], PerpPosition::default());
    //     assert_eq!(taker.orders[0], Order::default());
    // }
    // `fulfill_with_amm_when_maker_is_filler` with a hard gate firing: the AMM
    // would JIT the residual, but must not. Only the maker's half fills; AMM
    // reserves untouched.
    #[test]
    fn paused_operations_blocks_amm_fill() {
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
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_spread: 0,
                base_asset_amount_with_amm: -1000000000,
                amm_jit_intensity: 100,
                max_base_asset_reserve: 200 * AMM_RESERVE_PRECISION,
                min_base_asset_reserve: 0,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
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
                auction_start_price: 0,
                auction_end_price: 100 * PRICE_PRECISION_I64,
                auction_duration: 0,
                price: 150 * PRICE_PRECISION_U64,
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
        let mut filler = User::default();
        let fee_structure = get_fee_structure();
        let (taker_key, _, filler_key) = get_user_keys();
        let mut taker_stats = UserStats {
            paused_operations: 4,
            ..UserStats::default()
        };
        let mut filler_stats = UserStats::default();
        let order_index = 0;
        let min_auction_duration = 0;
        let user_can_skip_auction_duration = taker
            .can_skip_auction_duration(&taker_stats, false)
            .unwrap();
        let is_amm_available = get_amm_is_available(
            &taker.orders[order_index],
            min_auction_duration,
            &market,
            &mut maps.oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        assert!(!user_can_skip_auction_duration);
        assert!(!is_amm_available);
        no_router!(router);
        let mut order = taker.orders[order_index];
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
                is_amm_available,
                false,
            ),
            &mut FillParties {
                maps: &mut maps,
                makers_and_referrer: &UserMap::empty(),
                makers_and_referrer_stats: &UserStatsMap::empty(),
            },
            &mut OfferedLiquidity {
                router: &mut router,
            },
            &mut FillerSide {
                user: &mut Some(&mut filler),
                stats: &mut Some(&mut filler_stats),
                key: filler_key,
                rev_share_escrow: &mut None,
            },
        )
        .unwrap();
        taker.orders[order_index] = order;
        assert_eq!(base_asset_amount, 0);
        assert_eq!(taker.perp_positions[0].base_asset_amount, 0);
        let market_after = maps.perp_market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.amm.base_asset_amount_with_amm, -1000000000);
    }
}
pub mod fill_order {
    use {
        super::*,
        crate::{
            controller::{
                orders::fill_perp_order_without_external_books, position::PositionDirection,
            },
            create_anchor_account_info,
            error::ErrorCode,
            instructions::optional_accounts::AccountMaps,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I64, BASE_PRECISION_U64, PEG_PRECISION,
                    PRICE_PRECISION_I64, PRICE_PRECISION_U64, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                time::SlotClock,
            },
            state::{
                fill_mode::FillMode,
                oracle::{HistoricalOracleData, OracleSource},
                perp_market::{MarketStats, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                state::State,
                user::{OrderStatus, OrderType, SpotPosition, User, UserStats},
                user_map::{UserMap, UserStatsMap},
            },
            test_utils::{get_orders, get_positions, get_pyth_price, get_spot_positions},
            QUOTE_PRECISION_I64,
        },
        anchor_lang::prelude::{AccountLoader, Clock},
        std::str::FromStr,
    };
    #[test]
    fn max_open_interest() {
        let clock = Clock {
            slot: 6,
            epoch_start_timestamp: 0,
            epoch: 0,
            leader_schedule_epoch: 0,
            unix_timestamp: 0,
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

        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            clock.slot,
            SlotClock::baseline(),
            None,
        )
        .unwrap();
        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                terminal_quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 100,
                max_fill_reserve_fraction: 100,
                max_spread: 1000,
                base_spread: 0,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 1000,
            order_tick_size: 1,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            max_open_interest: 100,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: oracle_price.price,
                    last_oracle_price_twap_5min: oracle_price.price,
                    last_oracle_price: oracle_price.price,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };

        market.status = MarketStatus::Active;
        market.amm.max_base_asset_reserve = i128::MAX as u128;
        market.amm.min_base_asset_reserve = 0;
        let (_new_ask_base_asset_reserve, _new_ask_quote_asset_reserve) =
            crate::vlp::amm::math::spread::calculate_spread_reserves(
                &market,
                PositionDirection::Long,
            )
            .unwrap();
        let (_new_bid_base_asset_reserve, _new_bid_quote_asset_reserve) =
            crate::vlp::amm::math::spread::calculate_spread_reserves(
                &market,
                PositionDirection::Short,
            )
            .unwrap();
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
        let mut user = User {
            orders: get_orders(Order {
                market_index: 0,
                order_id: 1,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 0,
                auction_end_price: 102 * PRICE_PRECISION_I64,
                auction_duration: 5,
                price: 102 * PRICE_PRECISION_U64,
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

        create_anchor_account_info!(user, User, user_account_info);
        let user_account_loader: AccountLoader<User> =
            AccountLoader::try_from(&user_account_info).unwrap();
        create_anchor_account_info!(UserStats::default(), UserStats, user_stats_account_info);
        let user_stats_account_loader: AccountLoader<UserStats> =
            AccountLoader::try_from(&user_stats_account_info).unwrap();
        let filler_key = Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        create_anchor_account_info!(User::default(), &filler_key, User, user_account_info);
        let filler_account_loader: AccountLoader<User> =
            AccountLoader::try_from(&user_account_info).unwrap();
        create_anchor_account_info!(UserStats::default(), UserStats, filler_stats_account_info);
        let filler_stats_account_loader: AccountLoader<UserStats> =
            AccountLoader::try_from(&filler_stats_account_info).unwrap();
        let state = State {
            min_perp_auction_duration: legacy_slot_duration_u8(1),
            default_market_order_time_in_force: 10,
            ..State::default()
        };
        let mut taker_order = user_account_loader.load().unwrap().orders[0];
        let err = fill_perp_order_without_external_books(
            &mut taker_order,
            true,
            &state,
            &user_account_loader,
            &user_stats_account_loader,
            &mut maps,
            &filler_account_loader,
            &filler_stats_account_loader,
            &UserMap::empty(),
            &UserStatsMap::empty(),
            &clock,
            FillMode::Fill,
            &mut None,
            false,
        );

        assert_eq!(err, Err(ErrorCode::MaxOpenInterest));
    }
}
pub mod force_cancel_orders {
    use {
        super::*,
        crate::{
            controller::{orders::force_cancel_orders, position::PositionDirection},
            create_anchor_account_info,
            instructions::optional_accounts::AccountMaps,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I64, BASE_PRECISION_U64,
                    LAMPORTS_PER_SOL_I64, LAMPORTS_PER_SOL_U64, PEG_PRECISION, PRICE_PRECISION_U64,
                    SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                time::SlotClock,
            },
            state::{
                oracle::{HistoricalOracleData, OracleSource},
                perp_market::{MarketStats, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                state::State,
                user::{MarketType, OrderStatus, OrderType, SpotPosition, User, UserStats},
            },
            test_utils::{get_positions, get_pyth_price, get_spot_positions},
        },
        anchor_lang::prelude::{AccountLoader, Clock},
        std::str::FromStr,
    };
    #[test]
    fn cancel_order_after_fulfill() {
        let clock = Clock {
            slot: 6,
            epoch_start_timestamp: 0,
            epoch: 0,
            leader_schedule_epoch: 0,
            unix_timestamp: 0,
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

        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            clock.slot,
            SlotClock::baseline(),
            None,
        )
        .unwrap();
        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                terminal_quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                // bid_base_asset_reserve: 101 * AMM_RESERVE_PRECISION,
                // bid_quote_asset_reserve: 99 * AMM_RESERVE_PRECISION,
                // ask_base_asset_reserve: 99 * AMM_RESERVE_PRECISION,
                // ask_quote_asset_reserve: 101 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 100,
                max_fill_reserve_fraction: 100,
                max_spread: 1000,
                base_spread: 0,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 1000,
            order_tick_size: 1,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: oracle_price.price,
                    last_oracle_price_twap_5min: oracle_price.price,
                    last_oracle_price: oracle_price.price,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };

        market.status = MarketStatus::Active;
        market.amm.max_base_asset_reserve = u128::MAX;
        market.amm.min_base_asset_reserve = 0;
        let (_new_ask_base_asset_reserve, _new_ask_quote_asset_reserve) =
            crate::vlp::amm::math::spread::calculate_spread_reserves(
                &market,
                PositionDirection::Long,
            )
            .unwrap();
        let (_new_bid_base_asset_reserve, _new_bid_quote_asset_reserve) =
            crate::vlp::amm::math::spread::calculate_spread_reserves(
                &market,
                PositionDirection::Short,
            )
            .unwrap();
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();
        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            deposit_balance: SPOT_BALANCE_PRECISION,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            ..SpotMarket::default()
        };

        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            deposit_balance: SPOT_BALANCE_PRECISION,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..SpotMarket::default_base_market()
        };

        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_multiple(
            vec![
                &usdc_spot_market_account_info,
                &sol_spot_market_account_info,
            ],
            true,
        )
        .unwrap();
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);
        let mut orders = [Order::default(); 32];
        orders[0] = Order {
            market_index: 0,
            order_id: 1,
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            market_type: MarketType::Perp,
            direction: PositionDirection::Long,
            base_asset_amount: 100 * BASE_PRECISION_U64,
            slot: 0,
            price: 102 * PRICE_PRECISION_U64,
            ..Order::default()
        };

        orders[1] = Order {
            market_index: 0,
            order_id: 2,
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            market_type: MarketType::Perp,
            direction: PositionDirection::Short,
            base_asset_amount: BASE_PRECISION_U64,
            slot: 0,
            price: 102 * PRICE_PRECISION_U64,
            ..Order::default()
        };

        orders[2] = Order {
            market_index: 1,
            order_id: 1,
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            market_type: MarketType::Spot,
            direction: PositionDirection::Long,
            base_asset_amount: 100 * LAMPORTS_PER_SOL_U64,
            slot: 0,
            price: 102 * PRICE_PRECISION_U64,
            ..Order::default()
        };

        orders[3] = Order {
            market_index: 1,
            order_id: 1,
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            market_type: MarketType::Spot,
            direction: PositionDirection::Short,
            base_asset_amount: LAMPORTS_PER_SOL_U64,
            slot: 0,
            price: 102 * PRICE_PRECISION_U64,
            ..Order::default()
        };

        let mut user = User {
            authority: Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap(), // different authority than filler
            orders,
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                open_orders: 2,
                open_bids: 100 * BASE_PRECISION_I64,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 1,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: SPOT_BALANCE_PRECISION_U64,
                open_orders: 2,
                open_bids: 100 * LAMPORTS_PER_SOL_I64,
                open_asks: -LAMPORTS_PER_SOL_I64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        create_anchor_account_info!(user, User, user_account_info);
        let user_account_loader: AccountLoader<User> =
            AccountLoader::try_from(&user_account_info).unwrap();
        create_anchor_account_info!(UserStats::default(), UserStats, user_stats_account_info);
        let _user_stats_account_loader: AccountLoader<UserStats> =
            AccountLoader::try_from(&user_stats_account_info).unwrap();
        let filler_key = Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        create_anchor_account_info!(User::default(), &filler_key, User, user_account_info);
        let filler_account_loader: AccountLoader<User> =
            AccountLoader::try_from(&user_account_info).unwrap();
        create_anchor_account_info!(UserStats::default(), UserStats, filler_stats_account_info);
        let _filler_stats_account_loader: AccountLoader<UserStats> =
            AccountLoader::try_from(&filler_stats_account_info).unwrap();
        let state = State {
            min_perp_auction_duration: legacy_slot_duration_u8(1),
            default_market_order_time_in_force: 10,
            ..State::default()
        };

        force_cancel_orders(
            &state,
            &user_account_loader,
            &mut maps,
            &filler_account_loader,
            &clock,
        )
        .unwrap();
        let user = user_account_loader.load().unwrap();
        assert!(user.orders[0].is_available());
        assert!(!user.orders[1].is_available());
        assert!(user.orders[2].is_available());
        assert!(!user.orders[3].is_available());
        assert_eq!(user.spot_positions[0].scaled_balance, 20000001);
        assert_eq!(user.spot_positions[0].balance_type, SpotBalanceType::Borrow,);
    }
}
pub mod cancel_reduce_only_trigger_orders {
    use {
        super::*,
        crate::{
            controller::{orders::cancel_reduce_only_trigger_orders, position::PositionDirection},
            create_anchor_account_info,
            instructions::optional_accounts::AccountMaps,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I64, LAMPORTS_PER_SOL_I64, PEG_PRECISION,
                    SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                time::SlotClock,
            },
            state::{
                oracle::{HistoricalOracleData, OracleSource},
                perp_market::{MarketStats, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                user::{MarketType, OrderStatus, OrderType, SpotPosition, User},
            },
            test_utils::{get_positions, get_pyth_price, get_spot_positions},
        },
        anchor_lang::prelude::Clock,
        std::str::FromStr,
    };
    #[test]
    fn test() {
        let clock = Clock {
            slot: 6,
            epoch_start_timestamp: 0,
            epoch: 0,
            leader_schedule_epoch: 0,
            unix_timestamp: 0,
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

        let mut oracle_map = OracleMap::load_one(
            &oracle_account_info,
            clock.slot,
            SlotClock::baseline(),
            None,
        )
        .unwrap();
        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                terminal_quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                // bid_base_asset_reserve: 101 * AMM_RESERVE_PRECISION,
                // bid_quote_asset_reserve: 99 * AMM_RESERVE_PRECISION,
                // ask_base_asset_reserve: 99 * AMM_RESERVE_PRECISION,
                // ask_quote_asset_reserve: 101 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 100,
                max_fill_reserve_fraction: 100,
                max_spread: 1000,
                base_spread: 0,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 1000,
            order_tick_size: 1,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: oracle_price.price,
                    last_oracle_price_twap_5min: oracle_price.price,
                    last_oracle_price: oracle_price.price,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };

        market.status = MarketStatus::Active;
        market.amm.max_base_asset_reserve = u128::MAX;
        market.amm.min_base_asset_reserve = 0;
        let (_new_ask_base_asset_reserve, _new_ask_quote_asset_reserve) =
            crate::vlp::amm::math::spread::calculate_spread_reserves(
                &market,
                PositionDirection::Long,
            )
            .unwrap();
        let (_new_bid_base_asset_reserve, _new_bid_quote_asset_reserve) =
            crate::vlp::amm::math::spread::calculate_spread_reserves(
                &market,
                PositionDirection::Short,
            )
            .unwrap();
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();
        let mut usdc_spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            deposit_balance: SPOT_BALANCE_PRECISION,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            ..SpotMarket::default()
        };

        create_anchor_account_info!(usdc_spot_market, SpotMarket, usdc_spot_market_account_info);
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            deposit_balance: SPOT_BALANCE_PRECISION,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            oracle: oracle_price_key,
            oracle_source: crate::state::oracle::OracleSource::PythLazer,
            ..SpotMarket::default_base_market()
        };

        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_multiple(
            vec![
                &usdc_spot_market_account_info,
                &sol_spot_market_account_info,
            ],
            true,
        )
        .unwrap();
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);
        let mut orders = [Order::default(); 32];
        orders[0] = Order {
            market_index: 0,
            order_id: 1,
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            market_type: MarketType::Perp,
            ..Order::default()
        };

        orders[1] = Order {
            market_index: 1,
            order_id: 2,
            status: OrderStatus::Open,
            order_type: OrderType::TriggerMarket,
            market_type: MarketType::Perp,
            reduce_only: true,
            ..Order::default()
        };

        orders[2] = Order {
            market_index: 0,
            order_id: 2,
            status: OrderStatus::Open,
            order_type: OrderType::TriggerMarket,
            market_type: MarketType::Perp,
            reduce_only: true,
            ..Order::default()
        };

        orders[3] = Order {
            market_index: 0,
            order_id: 2,
            status: OrderStatus::Open,
            order_type: OrderType::TriggerMarket,
            market_type: MarketType::Spot,
            reduce_only: true,
            ..Order::default()
        };

        orders[4] = Order {
            market_index: 0,
            order_id: 2,
            status: OrderStatus::Open,
            order_type: OrderType::TriggerLimit,
            market_type: MarketType::Perp,
            reduce_only: true,
            ..Order::default()
        };

        let mut user = User {
            authority: Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap(), // different authority than filler
            orders,
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                open_orders: 2,
                open_bids: 100 * BASE_PRECISION_I64,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 1,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: SPOT_BALANCE_PRECISION_U64,
                open_orders: 2,
                open_bids: 100 * LAMPORTS_PER_SOL_I64,
                open_asks: -LAMPORTS_PER_SOL_I64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        cancel_reduce_only_trigger_orders(
            &mut user,
            &Pubkey::default(),
            Some(&Pubkey::default()),
            &mut maps,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(user.orders[0].status, OrderStatus::Open);
        assert_eq!(user.orders[1].status, OrderStatus::Open);
        assert_eq!(user.orders[2].status, OrderStatus::Canceled);
        assert_eq!(user.orders[3].status, OrderStatus::Open);
        assert_eq!(user.orders[4].status, OrderStatus::Canceled);
    }
}
pub mod update_trigger_order_params {
    use crate::{
        controller::orders::update_trigger_order_params,
        math::time::SlotClock,
        state::{
            oracle::OraclePriceData,
            user::{Order, OrderTriggerCondition, OrderType},
        },
        PositionDirection, PRICE_PRECISION_I64, PRICE_PRECISION_U64,
    };
    #[test]
    fn test() {
        let mut order = Order {
            order_type: OrderType::TriggerMarket,
            direction: PositionDirection::Long,
            trigger_condition: OrderTriggerCondition::Above,
            ..Order::default()
        };
        let oracle_price_data = OraclePriceData {
            price: 100 * PRICE_PRECISION_I64,
            confidence: 100 * PRICE_PRECISION_U64,
            ..OraclePriceData::default()
        };
        let slot = 10;
        let min_auction_duration = 10;
        update_trigger_order_params(
            &mut order,
            &oracle_price_data,
            slot,
            min_auction_duration,
            None,
            SlotClock::baseline(),
        )
        .unwrap();
        assert_eq!(order.slot, slot);
        assert_eq!(order.auction_duration, min_auction_duration);
        assert_eq!(
            order.trigger_condition,
            OrderTriggerCondition::TriggeredAbove
        );
        assert_eq!(order.auction_start_price, 100000000);
        assert_eq!(order.auction_end_price, 100500000);
        let mut order = Order {
            order_type: OrderType::TriggerMarket,
            direction: PositionDirection::Short,
            trigger_condition: OrderTriggerCondition::Below,
            ..Order::default()
        };

        update_trigger_order_params(
            &mut order,
            &oracle_price_data,
            slot,
            min_auction_duration,
            None,
            SlotClock::baseline(),
        )
        .unwrap();
        assert_eq!(order.slot, slot);
        assert_eq!(order.auction_duration, min_auction_duration);
        assert_eq!(
            order.trigger_condition,
            OrderTriggerCondition::TriggeredBelow
        );
        assert_eq!(order.auction_start_price, 100000000);
        assert_eq!(order.auction_end_price, 99500000);
        let mut order = Order {
            order_type: OrderType::TriggerMarket,
            direction: PositionDirection::Short,
            trigger_condition: OrderTriggerCondition::TriggeredAbove,
            ..Order::default()
        };
        let err = update_trigger_order_params(
            &mut order,
            &oracle_price_data,
            slot,
            min_auction_duration,
            None,
            SlotClock::baseline(),
        );

        assert!(err.is_err());
        let mut order = Order {
            order_type: OrderType::TriggerMarket,
            direction: PositionDirection::Short,
            trigger_condition: OrderTriggerCondition::TriggeredBelow,
            ..Order::default()
        };
        let err = update_trigger_order_params(
            &mut order,
            &oracle_price_data,
            slot,
            min_auction_duration,
            None,
            SlotClock::baseline(),
        );

        assert!(err.is_err());
    }
}

mod update_maker_fills_map {
    use {
        crate::{controller::orders::update_maker_fills_map, PositionDirection},
        solana_program::pubkey::Pubkey,
        std::collections::BTreeMap,
    };
    #[test]
    fn test() {
        let mut map: BTreeMap<Pubkey, (i64, bool)> = BTreeMap::new();
        let maker_key = Pubkey::new_unique();
        let fill = 100;
        let direction = PositionDirection::Long;
        update_maker_fills_map(&mut map, &maker_key, direction, fill, false).unwrap();
        assert_eq!(map.get(&maker_key).unwrap().0, fill as i64);
        assert_eq!(map.get(&maker_key).unwrap().1, false);
        update_maker_fills_map(&mut map, &maker_key, direction, fill, false).unwrap();
        assert_eq!(map.get(&maker_key).unwrap().0, 2 * fill as i64);
        assert_eq!(map.get(&maker_key).unwrap().1, false);
        let maker_key = Pubkey::new_unique();
        let direction = PositionDirection::Short;
        update_maker_fills_map(&mut map, &maker_key, direction, fill, false).unwrap();
        assert_eq!(map.get(&maker_key).unwrap().0, -(fill as i64));
        assert_eq!(map.get(&maker_key).unwrap().1, false);
        update_maker_fills_map(&mut map, &maker_key, direction, fill, false).unwrap();
        assert_eq!(map.get(&maker_key).unwrap().0, -2 * fill as i64);
        assert_eq!(map.get(&maker_key).unwrap().1, false);
    }
    #[test]
    fn test_isolated_position_true() {
        let mut map: BTreeMap<Pubkey, (i64, bool)> = BTreeMap::new();
        let fill = 100;
        // Single insert with isolated_position true
        let maker_key = Pubkey::new_unique();
        update_maker_fills_map(&mut map, &maker_key, PositionDirection::Long, fill, true).unwrap();
        assert_eq!(map.get(&maker_key).unwrap().0, fill as i64);
        assert_eq!(map.get(&maker_key).unwrap().1, true);
        // Merge: same maker_key, two updates both with true
        update_maker_fills_map(&mut map, &maker_key, PositionDirection::Long, fill, true).unwrap();
        assert_eq!(map.get(&maker_key).unwrap().0, 2 * fill as i64);
        assert_eq!(map.get(&maker_key).unwrap().1, true);
        // Last write wins: first false, then true -> final .1 is true
        let maker_key2 = Pubkey::new_unique();
        update_maker_fills_map(&mut map, &maker_key2, PositionDirection::Short, fill, false)
            .unwrap();
        update_maker_fills_map(&mut map, &maker_key2, PositionDirection::Short, fill, true)
            .unwrap();
        assert_eq!(map.get(&maker_key2).unwrap().0, -2 * fill as i64);
        assert_eq!(map.get(&maker_key2).unwrap().1, true);
    }
}
mod order_is_low_risk_for_amm {
    use {
        super::*,
        crate::state::user::{OrderBitFlag, OrderStatus},
    };

    fn base_perp_order() -> Order {
        Order {
            status: OrderStatus::Open,
            market_type: MarketType::Perp,
            slot: 100,
            ..Order::default()
        }
    }
    #[test]
    fn older_than_oracle_delay_returns_true() {
        let order = base_perp_order();
        let clock_slot = 110u64;
        let mm_oracle_delay = 10i64;
        let is_low = order
            .is_low_risk_for_amm(mm_oracle_delay, clock_slot, false, true)
            .unwrap();
        assert!(is_low);
    }
    #[test]
    fn not_older_than_delay_returns_false() {
        let order = base_perp_order();
        let clock_slot = 110u64;
        let mm_oracle_delay = 11i64;
        let is_low = order
            .is_low_risk_for_amm(mm_oracle_delay, clock_slot, false, true)
            .unwrap();
        assert!(!is_low);
    }
    #[test]
    fn liquidation_always_low_risk() {
        let order = base_perp_order();
        let is_low = order
            .is_low_risk_for_amm(0, order.slot, true, true)
            .unwrap();
        assert!(is_low);
    }
    #[test]
    fn safe_trigger_order_flag_sets_low_risk() {
        let mut order = base_perp_order();
        order.add_bit_flag(OrderBitFlag::SafeTriggerOrder);
        let is_low = order
            .is_low_risk_for_amm(0, order.slot, false, true)
            .unwrap();
        assert!(is_low);
    }
    #[test]
    fn user_can_skip_auction_duration() {
        let order = base_perp_order();
        let clock_slot = 110u64;
        let mm_oracle_delay = 10i64;
        let is_low = order
            .is_low_risk_for_amm(mm_oracle_delay, clock_slot, false, true)
            .unwrap();
        assert!(is_low);
        let is_low = order
            .is_low_risk_for_amm(mm_oracle_delay, clock_slot, false, false)
            .unwrap();
        assert!(!is_low);
    }
}

/// The signed-message sanitizer relaxation (`state::order_params`) preserves a
/// client's fully-specified auction tuple on A/B markets — including a short
/// `auction_duration`. But the duration the order is *placed* with is not the
/// value the sanitizer leaves behind: `get_auction_params` independently floors
/// it to `state.min_perp_auction_duration` at build time. On mainnet (program
/// `vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P`, state PDA
/// `2etx5NvPNxeMZ7EfHE6GjJfW2imRYEUANehNS1WB4CVW`) that floor is 10 as of
/// 2026-07-10, so a client's 5-slot signed-message auction is placed as a
/// 10-slot auction. These tests pin that end-to-end behavior so the "5 stays 5"
/// unit tests in `state::order_params::tests` don't read as the whole story.
mod get_auction_params_min_duration_floor {
    use crate::{
        controller::orders::get_auction_params,
        state::{oracle::OraclePriceData, order_params::OrderParams, user::OrderType},
        PositionDirection, PRICE_PRECISION_I64,
    };

    fn oracle() -> OraclePriceData {
        OraclePriceData {
            price: 100 * PRICE_PRECISION_I64,
            ..OraclePriceData::default()
        }
    }

    /// A fully-specified, aggressive 5-slot market auction — the shape a
    /// signed-message order has after the A/B sanitizer preserves it.
    fn aggressive_5_slot_market_order() -> OrderParams {
        OrderParams {
            order_type: OrderType::Market,
            direction: PositionDirection::Long,
            auction_duration: Some(5),
            auction_start_price: Some(99_700_000),
            auction_end_price: Some(100_300_000),
            price: 100_300_000,
            ..OrderParams::default()
        }
    }
    #[test]
    fn floors_preserved_client_duration_to_mainnet_min() {
        let params = aggressive_5_slot_market_order();
        // tick_size = 1 is identity, so the only change is the duration floor.
        let auction = get_auction_params(&params, &oracle(), 1, 10).unwrap();
        assert_eq!(auction.start_price, 99_700_000);
        assert_eq!(auction.end_price, 100_300_000);
        // The client asked for 5 slots and the sanitizer preserved it, but the
        // placed order is floored to the mainnet minimum of 10.
        assert_eq!(auction.duration, 10);
        assert_ne!(auction.duration, params.auction_duration.unwrap());
    }
    #[test]
    fn preserves_client_duration_only_when_floor_is_low_enough() {
        let params = aggressive_5_slot_market_order();
        // Lowering state.min_perp_auction_duration to <= the client's choice is
        // what actually lets a 5-slot auction survive end-to-end.
        let auction = get_auction_params(&params, &oracle(), 1, 5).unwrap();
        assert_eq!(auction.duration, 5);

        let auction = get_auction_params(&params, &oracle(), 1, 3).unwrap();
        assert_eq!(auction.duration, 5);
    }
}

/// OtterSec #112 — a perp fill must measure its band checks against the 5-minute
/// oracle TWAP as it stood *before* the fill's own refresh.
///
/// `fill_perp_order` captures `oracle_twap_5min` once and feeds it to both
/// `is_oracle_too_divergent_with_twap_5min` and
/// `validate_fill_price_within_price_bands`. It used to read that value *after*
/// calling `update_oracle_derived_stats`, which advances the TWAP toward the live
/// oracle price — so a currently-divergent oracle normalized itself inside the
/// same instruction and cleared the checks meant to stop the fill. The capture now
/// happens before the refresh.
///
/// This pins the primitive that made it exploitable: one refresh moves the 5-min
/// TWAP far enough to flip the divergence verdict.
#[test]
fn oracle_derived_stats_refresh_can_flip_the_5min_divergence_verdict() {
    use crate::{
        math::{
            constants::{PERCENTAGE_PRECISION_U64, PRICE_PRECISION, PRICE_PRECISION_U64},
            orders::is_oracle_too_divergent_with_twap_5min,
        },
        state::{
            oracle::{HistoricalOracleData, OraclePriceData, OracleSource},
            perp_market::{ContractTier, MarketStats, AMM},
            state::{OracleGuardRails, ValidityGuardRails},
        },
    };

    let now = 3600_i64;
    let slot = 1_u64;
    // Live oracle at 20 against a 5-min TWAP still at 10 — a 100% divergence,
    // well past the 50% default ceiling, so the fill must be refused.
    let oracle_price = (20 * PRICE_PRECISION) as i64;
    let max_divergence = (PERCENTAGE_PRECISION_U64 / 2) as i64;
    let guard_rails = OracleGuardRails {
        validity: ValidityGuardRails {
            slots_before_stale_for_amm: legacy_slot_duration_i64(10),
            slots_before_stale_for_margin: legacy_slot_duration_i64(120),
            confidence_interval_max_size: 1000,
            too_volatile_ratio: 5,
        },
        ..OracleGuardRails::default()
    };
    let mut market = PerpMarket {
        market_index: 0,
        status: MarketStatus::Active,
        // 50% sanitize band, wide enough for one refresh to carry 10 -> 15.
        contract_tier: ContractTier::C,
        amm: AMM {
            base_asset_reserve: 500 * crate::math::constants::AMM_RESERVE_PRECISION,
            quote_asset_reserve: 500 * crate::math::constants::AMM_RESERVE_PRECISION,
            sqrt_k: 500 * crate::math::constants::AMM_RESERVE_PRECISION,
            peg_multiplier: 20_000_000,
            ..AMM::default()
        },
        oracle_source: OracleSource::QuoteAsset,
        market_stats: MarketStats {
            funding_period: 3600,
            last_mark_price_twap: 20 * PRICE_PRECISION_U64,
            last_mark_price_twap_5min: 20 * PRICE_PRECISION_U64,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price: oracle_price,
                last_oracle_price_twap: (10 * PRICE_PRECISION) as i64,
                last_oracle_price_twap_5min: (10 * PRICE_PRECISION) as i64,
                ..HistoricalOracleData::default()
            },
            ..MarketStats::default()
        },
        ..PerpMarket::default()
    };
    let pre_refresh_twap_5min = market
        .market_stats
        .historical_oracle_data
        .last_oracle_price_twap_5min;
    // What the fill now reads: the oracle is too divergent, so the fill is refused.
    assert!(
        is_oracle_too_divergent_with_twap_5min(oracle_price, pre_refresh_twap_5min, max_divergence)
            .unwrap(),
        "the pre-refresh TWAP must still see this oracle as too divergent"
    );

    let oracle_price_data = OraclePriceData {
        price: oracle_price,
        confidence: 0,
        delay: 0,
        has_sufficient_number_of_data_points: true,
        ..OraclePriceData::default()
    };
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &guard_rails.validity,
            SlotClock::baseline(),
        )
        .unwrap();
    let validity = crate::vlp::amm::refresh::compute_amm_refresh_validity_with_guard_rails(
        &market,
        &mm_oracle_price_data,
        &guard_rails.validity,
        slot,
        SlotClock::baseline(),
    )
    .unwrap();
    market
        .update_oracle_derived_stats(
            &mm_oracle_price_data,
            validity,
            now,
            slot,
            SlotClock::baseline(),
        )
        .unwrap();
    let post_refresh_twap_5min = market
        .market_stats
        .historical_oracle_data
        .last_oracle_price_twap_5min;
    assert!(post_refresh_twap_5min > pre_refresh_twap_5min);
    // What the fill used to read: the same oracle now looks acceptable.
    assert!(
        !is_oracle_too_divergent_with_twap_5min(
            oracle_price,
            post_refresh_twap_5min,
            max_divergence
        )
        .unwrap(),
        "the refresh is expected to normalize the divergence away — if this trips, \
         the fixture no longer reproduces #112"
    );
}
pub mod builder_fee_margin_gate {
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
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I64, BASE_PRECISION_U64,
                    MAX_CONCENTRATION_COEFFICIENT, PEG_PRECISION, PRICE_PRECISION,
                    PRICE_PRECISION_I64, PRICE_PRECISION_U64, QUOTE_PRECISION_I64,
                    SPOT_BALANCE_PRECISION_U64, SPOT_CUMULATIVE_INTEREST_PRECISION,
                    SPOT_WEIGHT_PRECISION,
                },
                time::SlotClock,
            },
            state::{
                oracle::{HistoricalOracleData, OracleSource},
                perp_market::{MarketStats, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                revenue_share::{
                    BuilderInfo, RevenueShareEscrow, RevenueShareEscrowFixed,
                    RevenueShareEscrowZeroCopyMut, RevenueShareOrder, RevenueShareOrderBitFlag,
                },
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                state::ValidityGuardRails,
                user::{OrderBitFlag, OrderStatus, OrderType, SpotPosition, User, UserStats},
                user_map::{UserMap, UserStatsMap},
            },
            test_utils::{get_orders, get_positions, get_pyth_price, get_spot_positions},
        },
        anchor_lang::Discriminator,
        std::{
            cell::{RefCell, RefMut},
            str::FromStr,
        },
    };
    /// The builder charges the global maximum, 1% of notional.
    const BUILDER_FEE_TENTH_BPS: u16 = 1000;
    /// The taker holds one base unit long, entered at the oracle price.
    const ENTRY_PRICE: i64 = 100;
    /// Order id of the taker's reducing order. The escrow row is keyed on it.
    const ORDER_ID: u32 = 1;
    /// Price of the spot market the taker borrows in.
    const SOL_PRICE: i64 = 100;
    /// A confidence interval this wide makes an oracle invalid for a margin
    /// calculation. The widest tolerance any asset tier allows is 100% of the
    /// price, so this is twice the price.
    const WIDE_ORACLE_CONF: u64 = 2 * SOL_PRICE as u64 * PRICE_PRECISION_U64;
    /// A second perp market the taker holds a position in. The fill never
    /// touches it, so spoiling its oracle leaves market 0's AMM able to fill.
    const OTHER_PERP_INDEX: u16 = 1;
    /// The taker's spot positions: a quote deposit that carries the margin,
    /// and a borrow in spot market 1 that the fill does not touch.
    fn sol_borrow_positions(
        collateral_dollars: u64,
        sol_borrow_hundredths: u64,
    ) -> [SpotPosition; 8] {
        let mut spot_positions = get_spot_positions(SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: collateral_dollars * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        });
        if sol_borrow_hundredths > 0 {
            spot_positions[1] = SpotPosition {
                market_index: 1,
                balance_type: SpotBalanceType::Borrow,
                scaled_balance: sol_borrow_hundredths * SPOT_BALANCE_PRECISION_U64 / 100,
                ..SpotPosition::default()
            };
        }
        spot_positions
    }
    /// The taker's perp positions: the one-unit long the order reduces, and a
    /// hundredth of a unit in `OTHER_PERP_INDEX`. The second is small enough to
    /// leave every margin verdict in these tests unchanged, and it is here only so
    /// that market's oracle is one the account's liabilities are priced on.
    fn taker_perp_positions() -> [PerpPosition; 8] {
        let mut perp_positions = get_positions(PerpPosition {
            market_index: 0,
            base_asset_amount: BASE_PRECISION_I64,
            quote_asset_amount: -ENTRY_PRICE * QUOTE_PRECISION_I64,
            quote_entry_amount: -ENTRY_PRICE * QUOTE_PRECISION_I64,
            quote_break_even_amount: -ENTRY_PRICE * QUOTE_PRECISION_I64,
            open_orders: 1,
            open_asks: -BASE_PRECISION_I64,
            ..PerpPosition::default()
        });
        perp_positions[1] = PerpPosition {
            market_index: OTHER_PERP_INDEX,
            base_asset_amount: BASE_PRECISION_I64 / 100,
            quote_asset_amount: -ENTRY_PRICE * QUOTE_PRECISION_I64 / 100,
            quote_entry_amount: -ENTRY_PRICE * QUOTE_PRECISION_I64 / 100,
            quote_break_even_amount: -ENTRY_PRICE * QUOTE_PRECISION_I64 / 100,
            ..PerpPosition::default()
        };
        perp_positions
    }
    /// Fills the reducing order for a taker whose only liability is the perp
    /// position, on markets whose oracles are all valid.
    fn run_reducing_builder_fill(collateral_dollars: u64) -> (u64, u64) {
        run_reducing_builder_fill_with_borrow(collateral_dollars, 0, 0)
    }
    /// Serializes an escrow that holds one open builder row and one approved
    /// builder. The layout is the one the production loader reads:
    /// discriminator, fixed header, `padding0`, orders length, orders,
    /// `padding1`, builders length, builders.
    fn escrow_backing(order: &RevenueShareOrder, builder: &BuilderInfo) -> (Vec<u128>, usize) {
        let len = RevenueShareEscrow::space(1, 1);
        let mut backing = vec![0u128; len.div_ceil(16)];
        {
            let full: &mut [u8] = bytemuck::cast_slice_mut(&mut backing);
            let buf = &mut full[..len];
            buf[0..8].copy_from_slice(RevenueShareEscrow::DISCRIMINATOR);
            let header = 8 + std::mem::size_of::<RevenueShareEscrowFixed>();
            let order_size = std::mem::size_of::<RevenueShareOrder>();
            buf[header + 4..header + 8].copy_from_slice(&1u32.to_le_bytes());
            buf[header + 8..header + 8 + order_size].copy_from_slice(bytemuck::bytes_of(order));
            let builders_len_offset = header + 12 + order_size;
            let builder_size = std::mem::size_of::<BuilderInfo>();
            buf[builders_len_offset..builders_len_offset + 4].copy_from_slice(&1u32.to_le_bytes());
            buf[builders_len_offset + 4..builders_len_offset + 4 + builder_size]
                .copy_from_slice(bytemuck::bytes_of(builder));
        }
        (backing, len)
    }
    /// Fills one position-decreasing, builder-coded order and returns
    /// `(base_filled, builder_fees_accrued)`. `collateral_dollars` sets the
    /// taker's quote deposit, which decides whether the taker meets initial
    /// margin. The market uses a 10% initial and a 5% maintenance ratio, so on
    /// a one-unit position at $100 the taker needs $10 to clear initial margin
    /// and $5 to clear maintenance.
    ///
    /// `sol_borrow_hundredths` gives the taker a borrow in spot market 1, a
    /// liability that the fill does not touch. `other_perp_oracle_conf` is the
    /// confidence interval on the oracle of a second perp market the taker also
    /// holds a position in, which decides whether the oracle on that liability
    /// is valid.
    ///
    /// The spoiled oracle is a perp one because a spot borrow the calculation
    /// cannot value fails the fill outright, well before the fee is decided. The
    /// broad `all_liability_oracles_valid` flag the gate reads covers perp
    /// oracles too, and a perp position the fill does not touch is the liability
    /// that reaches the gate without reaching that reject.
    fn run_reducing_builder_fill_with_borrow(
        collateral_dollars: u64,
        sol_borrow_hundredths: u64,
        other_perp_oracle_conf: u64,
    ) -> (u64, u64) {
        let now = 0_i64;
        let slot = 5_u64;
        let mut oracle_price = get_pyth_price(ENTRY_PRICE, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut sol_oracle_price = get_pyth_price(SOL_PRICE, 6);
        let sol_oracle_price_key =
            Pubkey::from_str("Gnt27xtC473ZT2Mw5u8wZ68Z3gULkSTb5DuxJy7eJotD").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            sol_oracle_account_info
        );
        let mut other_perp_oracle_price = get_pyth_price(ENTRY_PRICE, 6);
        other_perp_oracle_price.conf = other_perp_oracle_conf;
        let other_perp_oracle_price_key =
            Pubkey::from_str("BAtFj4kQttZRVep3UZS2aZRDixkGYgWsbqTBVDbnSsPF").unwrap();
        create_anchor_account_info!(
            other_perp_oracle_price,
            &other_perp_oracle_price_key,
            PythLazerOracle,
            other_perp_oracle_account_info
        );
        let oracle_account_infos = Vec::from([
            oracle_account_info,
            sol_oracle_account_info,
            other_perp_oracle_account_info,
        ]);
        let mut oracle_map = OracleMap::load(
            &mut oracle_account_infos.iter().peekable(),
            slot,
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
                terminal_quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                concentration_coef: MAX_CONCENTRATION_COEFFICIENT,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_spread: 0,
                max_spread: 1000,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 1000,
            order_tick_size: 1,
            oracle: oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price: ENTRY_PRICE * PRICE_PRECISION as i64,
                    last_oracle_price_twap: ENTRY_PRICE * PRICE_PRECISION as i64,
                    last_oracle_price_twap_5min: ENTRY_PRICE * PRICE_PRECISION as i64,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u64::MAX as u128;
        market.amm.min_base_asset_reserve = 0;
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        // The same market on a different oracle. The taker's position here is a
        // perp liability the fill does not touch.
        let mut other_perp_market = PerpMarket {
            market_index: OTHER_PERP_INDEX,
            oracle: other_perp_oracle_price_key,
            ..market
        };
        create_anchor_account_info!(
            other_perp_market,
            PerpMarket,
            other_perp_market_account_info
        );
        let market_map = PerpMarketMap::load_multiple(
            vec![&market_account_info, &other_perp_market_account_info],
            true,
        )
        .unwrap();
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
        let mut sol_spot_market = SpotMarket {
            market_index: 1,
            oracle: sol_oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(
                SOL_PRICE * PRICE_PRECISION_I64,
            ),
            ..SpotMarket::default_base_market()
        };
        create_anchor_account_info!(sol_spot_market, SpotMarket, sol_spot_market_account_info);
        let spot_market_map = SpotMarketMap::load_multiple(
            vec![&spot_market_account_info, &sol_spot_market_account_info],
            true,
        )
        .unwrap();
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);
        // Long one unit, closing it with a builder-coded market sell. The order
        // reduces the position, so the post-fill check uses maintenance margin.
        let mut taker = User {
            orders: get_orders(Order {
                order_id: ORDER_ID,
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_duration: 0,
                price: 90 * PRICE_PRECISION_U64,
                bit_flags: OrderBitFlag::HasBuilder as u8,
                ..Order::default()
            }),
            perp_positions: taker_perp_positions(),
            spot_positions: sol_borrow_positions(collateral_dollars, sol_borrow_hundredths),
            ..User::default()
        };
        let builder_row = RevenueShareOrder::new(
            0,
            taker.sub_account_id,
            ORDER_ID,
            BUILDER_FEE_TENTH_BPS,
            MarketType::Perp,
            0,
            RevenueShareOrderBitFlag::Open as u8,
            0,
        );
        let builder_info = BuilderInfo {
            authority: Pubkey::default(),
            max_fee_tenth_bps: BUILDER_FEE_TENTH_BPS,
            padding: [0; 6],
        };
        let (mut escrow_store, escrow_len) = escrow_backing(&builder_row, &builder_info);
        let escrow_bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut escrow_store);
        let escrow_cell = RefCell::new(&mut escrow_bytes[..escrow_len]);
        let escrow_data = RefMut::map(escrow_cell.borrow_mut(), |d| &mut **d);
        let (_disc, escrow_data) = RefMut::map_split(escrow_data, |d| d.split_at_mut(8));
        let (escrow_fixed, escrow_data) = RefMut::map_split(escrow_data, |d| {
            d.split_at_mut(std::mem::size_of::<RevenueShareEscrowFixed>())
        });
        let mut escrow = RevenueShareEscrowZeroCopyMut {
            fixed: RefMut::map(escrow_fixed, |b| bytemuck::from_bytes_mut(b)),
            data: escrow_data,
        };
        let mut filler = User::default();
        let fee_structure = get_fee_structure();
        let (taker_key, _, filler_key) = get_user_keys();
        let mut taker_stats = UserStats::default();
        let mut filler_stats = UserStats::default();
        let order_index = 0;
        let user_can_skip_auction_duration = taker
            .can_skip_auction_duration(&taker_stats, false)
            .unwrap();
        let is_amm_available = get_amm_is_available(
            &taker.orders[order_index],
            0,
            &market,
            &mut maps.oracle_map,
            slot,
            user_can_skip_auction_duration,
        );
        assert!(is_amm_available);
        // Router mode with no external quoters: vAMM + the passed makers.
        let mut no_externals = crate::state::prop_amm::NoExternalQuoters;
        let mut router_inputs = crate::math::router::RouterLeg {
            books: &[],
            executor: &mut no_externals,
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
        let mut order = taker.orders[order_index];
        let FillAmounts {
            base: base_filled, ..
        } = fill_within_taker_risk_limits(
            &mut TakerSide::bind(&mut taker, &mut taker_stats, taker_key, &mut order, true)
                .unwrap(),
            &PricingRules {
                fee_structure: &fee_structure,
                validity_guard_rails: &ValidityGuardRails::default(),
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
                is_amm_available,
                false,
            ),
            &mut FillParties {
                maps: &mut maps,
                makers_and_referrer: &UserMap::empty(),
                makers_and_referrer_stats: &UserStatsMap::empty(),
            },
            &mut OfferedLiquidity {
                router: &mut router_inputs,
            },
            &mut FillerSide {
                user: &mut Some(&mut filler),
                stats: &mut Some(&mut filler_stats),
                key: filler_key,
                rev_share_escrow: &mut Some(&mut escrow),
            },
        )
        .unwrap();
        taker.orders[order_index] = order;
        (base_filled, escrow.get_order(0).unwrap().fees_accrued)
    }
    #[test]
    fn charges_builder_fee_when_taker_meets_initial_margin() {
        // $50 of collateral against a $100 position clears the 10% initial
        // requirement, so the fee is value the taker could also have withdrawn.
        let (base_filled, fees_accrued) = run_reducing_builder_fill(50);
        assert_eq!(base_filled, BASE_PRECISION_U64);
        // About 1% of a fill worth about $100.
        assert!(
            fees_accrued > 900_000 && fees_accrued < 1_100_000,
            "expected about 1% of notional, got {fees_accrued}"
        );
    }
    #[test]
    fn charges_builder_fee_when_a_liability_oracle_is_valid() {
        // A quarter of a unit borrowed in spot market 1, about $25 against $50
        // of collateral. The taker still clears initial margin, and every oracle
        // its liabilities are priced on is precise, so the fee is charged.
        // Control for the invalid oracle case below.
        let (base_filled, fees_accrued) = run_reducing_builder_fill_with_borrow(50, 25, 0);
        assert_eq!(base_filled, BASE_PRECISION_U64);
        assert!(
            fees_accrued > 900_000 && fees_accrued < 1_100_000,
            "expected about 1% of notional, got {fees_accrued}"
        );
    }
    #[test]
    fn waives_builder_fee_when_a_liability_oracle_is_invalid() {
        // The same taker, and the same margin state, but the oracle on the perp
        // market the fill does not touch is too uncertain to price that position.
        // The reduction still fills and the fee is waived.
        let (base_filled, fees_accrued) =
            run_reducing_builder_fill_with_borrow(50, 25, WIDE_ORACLE_CONF);
        assert_eq!(base_filled, BASE_PRECISION_U64);
        assert_eq!(fees_accrued, 0);
    }
    #[test]
    fn waives_builder_fee_when_taker_below_initial_margin() {
        // $7 of collateral clears the 5% maintenance requirement but not the
        // 10% initial one. The reduction still fills and the fee is waived.
        let (base_filled, fees_accrued) = run_reducing_builder_fill(7);
        assert_eq!(base_filled, BASE_PRECISION_U64);
        assert_eq!(fees_accrued, 0);
    }
}
/// The taker-side counterpart of the floored-maker pruning.
///
/// A risk-increasing fill for a floored taker ends at the buffered-floor
/// gate, which fails closed on any invalid oracle in the taker's portfolio.
/// A floored maker in that state is pruned before matching; a taker was not,
/// so its visible order made every fill attempt revert deterministically for
/// the length of an unrelated oracle outage. `fulfill_perp_order` now
/// withholds the fill up front (zero fill, no error, order left resting),
/// and only for the combination the gate would reject: floor set, order
/// risk-increasing, some oracle invalid.
mod taker_floor_unverifiable_withholds_fill {
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
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I64, BASE_PRECISION_U64,
                    MAX_CONCENTRATION_COEFFICIENT, PEG_PRECISION, PRICE_PRECISION,
                    PRICE_PRECISION_U64, QUOTE_PRECISION_I64, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                time::SlotClock,
            },
            state::{
                oracle::{HistoricalOracleData, OracleSource},
                perp_market::{MarketStats, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                state::ValidityGuardRails,
                user::{OrderStatus, OrderType, SpotPosition, User, UserStats},
                user_map::{UserMap, UserStatsMap},
            },
            test_utils::{get_orders, get_positions, get_pyth_price, get_spot_positions},
        },
        std::str::FromStr,
    };
    /// The taker holds one base unit long in market 0, entered at the oracle
    /// price.
    const ENTRY_PRICE: i64 = 100;
    /// A second perp market the taker holds a small position in. The fill
    /// never touches it; its oracle is the one the tests spoil.
    const OTHER_PERP_INDEX: u16 = 1;
    /// A confidence interval this wide makes an oracle invalid for a margin
    /// calculation: the widest tolerance any asset tier allows is 100% of the
    /// price, so this is twice the price.
    const WIDE_ORACLE_CONF: u64 = 2 * ENTRY_PRICE as u64 * PRICE_PRECISION_U64;
    /// Fills one market order on market 0 for a taker who also holds a
    /// hundredth of a unit long in `OTHER_PERP_INDEX`, and returns the base
    /// filled. `equity_floor` sets the taker's floor, `reducing` picks the
    /// order side against the taker's one-unit long, `other_perp_oracle_conf`
    /// decides whether the untouched market's oracle is valid.
    fn run_fill(equity_floor: u64, reducing: bool, other_perp_oracle_conf: u64) -> u64 {
        let now = 0_i64;
        let slot = 5_u64;
        let mut oracle_price = get_pyth_price(ENTRY_PRICE, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut other_perp_oracle_price = get_pyth_price(ENTRY_PRICE, 6);
        other_perp_oracle_price.conf = other_perp_oracle_conf;
        let other_perp_oracle_price_key =
            Pubkey::from_str("BAtFj4kQttZRVep3UZS2aZRDixkGYgWsbqTBVDbnSsPF").unwrap();
        create_anchor_account_info!(
            other_perp_oracle_price,
            &other_perp_oracle_price_key,
            PythLazerOracle,
            other_perp_oracle_account_info
        );
        let oracle_account_infos = Vec::from([oracle_account_info, other_perp_oracle_account_info]);
        let mut oracle_map = OracleMap::load(
            &mut oracle_account_infos.iter().peekable(),
            slot,
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
                terminal_quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                concentration_coef: MAX_CONCENTRATION_COEFFICIENT,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_spread: 0,
                max_spread: 1000,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 1000,
            order_tick_size: 1,
            oracle: oracle_price_key,
            oracle_source: OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price: ENTRY_PRICE * PRICE_PRECISION as i64,
                    last_oracle_price_twap: ENTRY_PRICE * PRICE_PRECISION as i64,
                    last_oracle_price_twap_5min: ENTRY_PRICE * PRICE_PRECISION as i64,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default_test()
        };
        market.amm.max_base_asset_reserve = u64::MAX as u128;
        market.amm.min_base_asset_reserve = 0;
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let mut other_perp_market = PerpMarket {
            market_index: OTHER_PERP_INDEX,
            oracle: other_perp_oracle_price_key,
            ..market
        };
        create_anchor_account_info!(
            other_perp_market,
            PerpMarket,
            other_perp_market_account_info
        );
        let market_map = PerpMarketMap::load_multiple(
            vec![&market_account_info, &other_perp_market_account_info],
            true,
        )
        .unwrap();
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
        let spot_market_map =
            SpotMarketMap::load_multiple(vec![&spot_market_account_info], true).unwrap();
        let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);
        // A quarter unit either way: short reduces the one-unit long, long
        // increases it.
        let (direction, price) = if reducing {
            (PositionDirection::Short, 90 * PRICE_PRECISION_U64)
        } else {
            (PositionDirection::Long, 110 * PRICE_PRECISION_U64)
        };
        let order_base = BASE_PRECISION_U64 / 4;
        let mut perp_positions = get_positions(PerpPosition {
            market_index: 0,
            base_asset_amount: BASE_PRECISION_I64,
            quote_asset_amount: -ENTRY_PRICE * QUOTE_PRECISION_I64,
            quote_entry_amount: -ENTRY_PRICE * QUOTE_PRECISION_I64,
            quote_break_even_amount: -ENTRY_PRICE * QUOTE_PRECISION_I64,
            open_orders: 1,
            open_asks: if reducing { -(order_base as i64) } else { 0 },
            open_bids: if reducing { 0 } else { order_base as i64 },
            ..PerpPosition::default()
        });
        perp_positions[1] = PerpPosition {
            market_index: OTHER_PERP_INDEX,
            base_asset_amount: BASE_PRECISION_I64 / 100,
            quote_asset_amount: -ENTRY_PRICE * QUOTE_PRECISION_I64 / 100,
            quote_entry_amount: -ENTRY_PRICE * QUOTE_PRECISION_I64 / 100,
            quote_break_even_amount: -ENTRY_PRICE * QUOTE_PRECISION_I64 / 100,
            ..PerpPosition::default()
        };
        let mut taker = User {
            equity_floor,
            orders: get_orders(Order {
                order_id: 1,
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction,
                base_asset_amount: order_base,
                slot: 0,
                auction_duration: 0,
                price,
                ..Order::default()
            }),
            perp_positions,
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };
        let mut filler = User::default();
        let fee_structure = get_fee_structure();
        let (taker_key, _, filler_key) = get_user_keys();
        let mut taker_stats = UserStats::default();
        let mut filler_stats = UserStats::default();
        let order_index = 0;
        let user_can_skip_auction_duration = taker
            .can_skip_auction_duration(&taker_stats, false)
            .unwrap();
        let is_amm_available = get_amm_is_available(
            &taker.orders[order_index],
            0,
            &market,
            &mut maps.oracle_map,
            slot,
            user_can_skip_auction_duration,
        );
        assert!(is_amm_available);
        no_router!(router_inputs);
        let mut order = taker.orders[order_index];
        let FillAmounts {
            base: base_filled, ..
        } = fill_within_taker_risk_limits(
            &mut TakerSide::bind(&mut taker, &mut taker_stats, taker_key, &mut order, true)
                .unwrap(),
            &PricingRules {
                fee_structure: &fee_structure,
                validity_guard_rails: &ValidityGuardRails::default(),
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
                is_amm_available,
                false,
            ),
            &mut FillParties {
                maps: &mut maps,
                makers_and_referrer: &UserMap::empty(),
                makers_and_referrer_stats: &UserStatsMap::empty(),
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
        taker.orders[order_index] = order;
        base_filled
    }
    const FLOOR: u64 = QUOTE_PRECISION_I64 as u64;
    #[test]
    fn withholds_a_floored_takers_risk_increasing_fill_when_an_oracle_is_invalid() {
        // The spoiled oracle belongs to a market the fill never touches, yet
        // the buffered-floor gate would still fail closed on it post-fill.
        // The fill is withheld instead of reverting: zero fill, no error.
        let base_filled = run_fill(FLOOR, false, WIDE_ORACLE_CONF);
        assert_eq!(base_filled, 0);
    }
    #[test]
    fn reducing_fill_of_a_floored_taker_still_fills() {
        // A reducing order is exempt at the gate, so the precheck must not
        // withhold it either; the lazy breaker trip downstream tolerates the
        // invalid oracle.
        let base_filled = run_fill(FLOOR, true, WIDE_ORACLE_CONF);
        assert_eq!(base_filled, BASE_PRECISION_U64 / 4);
    }
    #[test]
    fn unfloored_taker_fills_despite_the_invalid_oracle() {
        // No floor, no gate: the same portfolio fills.
        let base_filled = run_fill(0, false, WIDE_ORACLE_CONF);
        assert_eq!(base_filled, BASE_PRECISION_U64 / 4);
    }
    #[test]
    fn floored_taker_fills_when_every_oracle_is_valid() {
        // The verifiable case passes the precheck and the post-fill gate
        // alike: $50 of net equity clears a $1 floor.
        let base_filled = run_fill(FLOOR, false, 0);
        assert_eq!(base_filled, BASE_PRECISION_U64 / 4);
    }
}
/// OtterSec #143 / #144 / #148 — a fill that reduces the position must not be
/// exempt from the spot-valuation gates.
///
/// The transfer these findings describe needs two accounts, and it works with both
/// seats reducing: one seat closes into the worst in-band price and leaves bad debt
/// its misvalued collateral was never able to cover, while the other settles the
/// matching profit out of the PnL pool. A gate keyed on risk direction closes
/// neither seat.
///
/// One fixture drives all three findings. The taker holds a partial-reduce order,
/// a borrow in market 1, and its collateral as a deposit in market 2. Each test
/// spoils one input and asserts the fill stops.
mod keeper_reward {
    /// A keeper reward must never leave the user's account without landing in
    /// the filler's.
    ///
    /// `force_get_perp_position_mut` fails when the filler already holds a
    /// position in every slot and none is this market's, and the reward is
    /// documented as not throwing in that case. The order of the two halves is
    /// therefore load-bearing: debiting first and then bailing took the quote
    /// off the user and credited nobody, so it accrued to the pool instead of
    /// to the keeper that earned it.
    #[test]
    fn an_unpayable_keeper_reward_does_not_debit_the_user() {
        use crate::{
            controller::orders::pay_keeper_flat_reward_for_perps,
            state::{perp_market::PerpMarket, user::User},
        };

        let mut market = PerpMarket {
            market_index: 0,
            ..PerpMarket::default()
        };
        let mut user = User::default();
        user.perp_positions[0].market_index = 0;
        user.perp_positions[0].quote_asset_amount = 1_000_000;
        // Every slot occupied by another market, so this market cannot be
        // added — exactly the case the early return exists for.
        let mut filler = User::default();
        for (i, position) in filler.perp_positions.iter_mut().enumerate() {
            position.market_index = (i as u16) + 1;
            position.base_asset_amount = 1;
        }

        let paid =
            pay_keeper_flat_reward_for_perps(&mut user, Some(&mut filler), &mut market, 5_000, 10)
                .unwrap();
        assert_eq!(paid, 0, "an unpayable reward is not paid");
        assert_eq!(
            user.perp_positions[0].quote_asset_amount, 1_000_000,
            "and the user keeps the quote it would have paid"
        );
    }
}
