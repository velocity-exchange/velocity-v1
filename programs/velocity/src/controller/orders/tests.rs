use {
    crate::{
        math::oracle::oracle_validity,
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

fn get_user_keys() -> (Pubkey, Pubkey, Pubkey) {
    (Pubkey::default(), Pubkey::default(), Pubkey::default())
}

fn get_oracle_map<'a>() -> OracleMap<'a> {
    OracleMap::empty()
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
        crate::math::oracle::LogMode::SafeMMOracle,
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

pub mod fulfill_order_with_maker_order {
    use {
        super::*,
        crate::{
            controller::{orders::fulfill_perp_order_step, position::PositionDirection},
            create_anchor_account_info,
            error::VelocityResult,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I128, BASE_PRECISION_I64,
                    BASE_PRECISION_U64, BID_ASK_SPREAD_PRECISION, PEG_PRECISION, PRICE_PRECISION,
                    PRICE_PRECISION_I64, PRICE_PRECISION_U64, QUOTE_PRECISION_I64,
                    QUOTE_PRECISION_U64,
                },
                oracle::OracleValidity,
            },
            state::{
                fulfillment::PerpFulfillmentMethod,
                oracle::HistoricalOracleData,
                oracle_map::OracleMap,
                perp_market::{MarketStats, PerpMarket, AMM},
                pyth_lazer_oracle::PythLazerOracle,
                revenue_share::RevenueShareEscrowZeroCopyMut,
                state::{FeeStructure, ValidityGuardRails},
                user::{Order, OrderType, PerpPosition, User, UserStats},
            },
            test_utils::{get_orders, get_positions, get_pyth_price},
        },
        anchor_lang::prelude::Pubkey,
        std::str::FromStr,
    };

    /// Test-only shim that preserves the legacy `fulfill_perp_order_with_match`
    /// signature on top of the unified `fulfill_perp_order_step`. Lets the
    /// match-side unit tests stay intact (call shape unchanged) while the
    /// production fill path now goes through the new orchestrator.
    #[allow(clippy::too_many_arguments)]
    fn fulfill_perp_order_with_match(
        market: &mut PerpMarket,
        taker: &mut User,
        taker_stats: &mut UserStats,
        taker_order_index: usize,
        taker_key: &Pubkey,
        maker: &mut User,
        maker_stats: &mut Option<&mut UserStats>,
        maker_order_index: usize,
        maker_key: &Pubkey,
        filler: &mut Option<&mut User>,
        filler_stats: &mut Option<&mut UserStats>,
        filler_key: &Pubkey,
        reserve_price_before: u64,
        valid_oracle_price: Option<i64>,
        taker_limit_price: Option<u64>,
        maker_price: u64,
        now: i64,
        slot: u64,
        validity_guard_rails: &ValidityGuardRails,
        fee_structure: &FeeStructure,
        oracle_map: &mut OracleMap,
        is_liquidation: bool,
        rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
    ) -> VelocityResult<(u64, u64, u64)> {
        let method =
            PerpFulfillmentMethod::Match(*maker_key, maker_order_index as u16, maker_price);
        // Match tests historically passed `reserve_price_before = 0` because
        // the legacy `fulfill_perp_order_with_match` recomputed reserve_price
        // internally from the AMM. The unified `fulfill_perp_order_step` uses
        // the value passed in (production callers thread the live value in
        // from `fill_perp_order`). Mirror the legacy behavior in the shim so
        // tests don't all need to compute it themselves.
        let effective_reserve_price = if reserve_price_before != 0 {
            reserve_price_before
        } else {
            market.amm.reserve_price().unwrap_or(0)
        };
        let mut maker_opt: Option<&mut User> = Some(maker);
        let mut maker_stats_opt: Option<&mut UserStats> = maker_stats.take();
        let result = fulfill_perp_order_step(
            market,
            taker,
            taker_stats,
            taker_order_index,
            taker_key,
            method,
            &mut maker_opt,
            &mut maker_stats_opt,
            Some(maker_order_index),
            Some(maker_key),
            filler,
            filler_stats,
            filler_key,
            effective_reserve_price,
            valid_oracle_price,
            taker_limit_price,
            now,
            slot,
            validity_guard_rails,
            fee_structure,
            oracle_map,
            is_liquidation,
            // Legacy match path always allowed AMM JIT participation.
            true,
            rev_share_escrow,
            false,
            0,
            // The step-level harness has no margin context. Allow the builder
            // fee so these tests measure fee math, not the margin gate.
            true,
        );
        // Restore caller's `maker_stats` so the test can keep using it after.
        *maker_stats = maker_stats_opt;
        result
    }

    #[test]
    fn long_taker_order_fulfilled_start_of_auction() {
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 100 * PRICE_PRECISION_I64,
                auction_end_price: 200 * PRICE_PRECISION_I64,
                auction_duration: 5,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                price: 100 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut market = PerpMarket::default_test();

        let now = 1_i64;
        let slot = 1_u64;

        let fee_structure = get_fee_structure();

        let (taker_key, maker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let taker_limit_price = taker.orders[0]
            .get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;

        fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut get_oracle_map(),
            false,
            &mut None,
        )
        .unwrap();

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, -100050000);
        assert_eq!(
            taker_position.quote_entry_amount,
            -100 * QUOTE_PRECISION_I64
        );
        assert_eq!(taker_position.quote_break_even_amount, -100050000);
        assert_eq!(taker_position.open_bids, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 50000);
        assert_eq!(taker_stats.taker_volume_30d, 100 * QUOTE_PRECISION_U64);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert!(taker.orders[0].is_available());

        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, -BASE_PRECISION_I64);
        assert_eq!(maker_position.quote_asset_amount, 100030000);
        assert_eq!(maker_position.quote_entry_amount, 100 * QUOTE_PRECISION_I64);
        assert_eq!(maker_position.quote_break_even_amount, 100030000);
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_asks, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 30000);
        assert_eq!(maker_stats.maker_volume_30d, 100 * QUOTE_PRECISION_U64);
        assert!(maker.orders[0].is_available());

        assert_eq!(market.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market.base_asset_amount_long, BASE_PRECISION_I128);
        assert_eq!(market.base_asset_amount_short, -BASE_PRECISION_I128);
        assert_eq!(market.quote_asset_amount, -20000);
        assert_eq!(market.fee_ledger.total_exchange_fee, 50000);
        assert_eq!(market.fee_ledger.pending_protocol_fee, 20000);
        assert_eq!(market.fee_ledger.pending_if_fee, 0);
        assert_eq!(market.amm.total_fee, 0);
        assert_eq!(market.amm.total_fee_minus_distributions, 0);
        assert_eq!(market.amm.net_revenue_since_last_funding, 0);
    }

    #[test]
    fn long_taker_order_fulfilled_middle_of_auction() {
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 100 * PRICE_PRECISION_I64,
                auction_end_price: 200 * PRICE_PRECISION_I64,
                auction_duration: 5,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                price: 160 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut market = PerpMarket::default_test();

        let now = 3_i64;
        let slot = 3_u64;

        let fee_structure = get_fee_structure();

        let (taker_key, maker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let taker_limit_price = taker.orders[0]
            .get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;

        fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut get_oracle_map(),
            false,
            &mut None,
        )
        .unwrap();

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, -160080000);
        assert_eq!(
            taker_position.quote_entry_amount,
            -160 * QUOTE_PRECISION_I64
        );
        assert_eq!(taker_position.quote_break_even_amount, -160080000);
        assert_eq!(taker_position.open_bids, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 80000);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 160 * QUOTE_PRECISION_U64);
        assert!(taker.orders[0].is_available());

        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, -BASE_PRECISION_I64);
        assert_eq!(maker_position.quote_asset_amount, 160048000);
        assert_eq!(maker_position.quote_entry_amount, 160 * QUOTE_PRECISION_I64);
        assert_eq!(maker_position.quote_break_even_amount, 160048000);
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_asks, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 48000);
        assert_eq!(maker_stats.maker_volume_30d, 160 * QUOTE_PRECISION_U64);
        assert!(maker.orders[0].is_available());

        assert_eq!(market.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market.base_asset_amount_long, BASE_PRECISION_I128);
        assert_eq!(market.base_asset_amount_short, -BASE_PRECISION_I128);
        assert_eq!(market.quote_asset_amount, -32000);
        assert_eq!(market.fee_ledger.total_exchange_fee, 80000);
        assert_eq!(market.fee_ledger.pending_protocol_fee, 32000);
        assert_eq!(market.fee_ledger.pending_if_fee, 0);
        assert_eq!(market.amm.total_fee, 0);
        assert_eq!(market.amm.total_fee_minus_distributions, 0);
        assert_eq!(market.amm.net_revenue_since_last_funding, 0);
    }

    #[test]
    fn short_taker_order_fulfilled_start_of_auction() {
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Market,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 200 * PRICE_PRECISION_I64,
                auction_end_price: 100 * PRICE_PRECISION_I64,
                auction_duration: 5,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                price: 180 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut market = PerpMarket::default_test();

        let now = 1_i64;
        let slot = 1_u64;

        let fee_structure = get_fee_structure();

        let (taker_key, maker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let taker_limit_price = taker.orders[0]
            .get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;

        fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut get_oracle_map(),
            false,
            &mut None,
        )
        .unwrap();

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, -BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, 179910000);
        assert_eq!(taker_position.quote_entry_amount, 180 * QUOTE_PRECISION_I64);
        assert_eq!(taker_position.quote_break_even_amount, 179910000);
        assert_eq!(taker_position.open_asks, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 90000);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 180 * QUOTE_PRECISION_U64);
        assert!(taker.orders[0].is_available());

        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(maker_position.quote_asset_amount, -179946000);
        assert_eq!(
            maker_position.quote_entry_amount,
            -180 * QUOTE_PRECISION_I64
        );
        assert_eq!(maker_position.quote_break_even_amount, -179946000);
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_bids, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 54000);
        assert_eq!(maker_stats.maker_volume_30d, 180 * QUOTE_PRECISION_U64);
        assert!(maker.orders[0].is_available());

        assert_eq!(market.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market.base_asset_amount_long, BASE_PRECISION_I128);
        assert_eq!(market.base_asset_amount_short, -BASE_PRECISION_I128);
        assert_eq!(market.quote_asset_amount, -36000);
        assert_eq!(market.fee_ledger.total_exchange_fee, 90000);
        assert_eq!(market.fee_ledger.pending_protocol_fee, 36000);
        assert_eq!(market.fee_ledger.pending_if_fee, 0);
        assert_eq!(market.amm.total_fee, 0);
        assert_eq!(market.amm.total_fee_minus_distributions, 0);
        assert_eq!(market.amm.net_revenue_since_last_funding, 0);
    }

    #[test]
    fn short_taker_order_fulfilled_middle_of_auction() {
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Market,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 200 * PRICE_PRECISION_I64,
                auction_end_price: 100 * PRICE_PRECISION_I64,
                auction_duration: 5,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                price: 140 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut market = PerpMarket::default_test();

        let now = 3_i64;
        let slot = 3_u64;

        let fee_structure = get_fee_structure();

        let (taker_key, maker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let taker_limit_price = taker.orders[0]
            .get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;

        fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut get_oracle_map(),
            false,
            &mut None,
        )
        .unwrap();

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, -BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, 139930000);
        assert_eq!(taker_position.quote_entry_amount, 140 * QUOTE_PRECISION_I64);
        assert_eq!(taker_position.quote_break_even_amount, 139930000);
        assert_eq!(taker_position.open_asks, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 70000);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 140 * QUOTE_PRECISION_U64);
        assert!(taker.orders[0].is_available());

        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(maker_position.quote_asset_amount, -139958000);
        assert_eq!(
            maker_position.quote_entry_amount,
            -140 * QUOTE_PRECISION_I64
        );
        assert_eq!(maker_position.quote_break_even_amount, -139958000);
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_bids, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 42000);
        assert_eq!(maker_stats.maker_volume_30d, 140 * QUOTE_PRECISION_U64);
        assert!(maker.orders[0].is_available());

        assert_eq!(market.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market.base_asset_amount_long, BASE_PRECISION_I128);
        assert_eq!(market.base_asset_amount_short, -BASE_PRECISION_I128);
        assert_eq!(market.quote_asset_amount, -28000);
        assert_eq!(market.fee_ledger.total_exchange_fee, 70000);
        assert_eq!(market.fee_ledger.pending_protocol_fee, 28000);
        assert_eq!(market.fee_ledger.pending_if_fee, 0);
        assert_eq!(market.amm.total_fee, 0);
        assert_eq!(market.amm.total_fee_minus_distributions, 0);
        assert_eq!(market.amm.net_revenue_since_last_funding, 0);
    }

    #[test]
    fn long_taker_order_auction_price_does_not_satisfy_maker() {
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 100 * PRICE_PRECISION_I64,
                auction_end_price: 200 * PRICE_PRECISION_I64,
                auction_duration: 5,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: 100 * BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                price: 201 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: 100 * BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut market = PerpMarket::default_test();

        let now = 1_i64;
        let slot = 3_u64;

        let fee_structure = FeeStructure::test_default();

        let (taker_key, maker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let taker_limit_price = taker.orders[0]
            .get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;
        let (base_asset_amount, _, _) = fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut get_oracle_map(),
            false,
            &mut None,
        )
        .unwrap();

        assert_eq!(base_asset_amount, 0);
    }

    #[test]
    fn short_taker_order_auction_price_does_not_satisfy_maker() {
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Market,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                auction_start_price: 200 * PRICE_PRECISION_I64,
                auction_end_price: 100 * PRICE_PRECISION_I64,
                auction_duration: 5,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: 100 * BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                price: 99 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: 100 * BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut market = PerpMarket::default_test();

        let now = 1_i64;
        let slot = 3_u64;

        let fee_structure = FeeStructure::test_default();

        let (taker_key, maker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let taker_limit_price = taker.orders[0]
            .get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;

        let (base_asset_amount, _, _) = fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut get_oracle_map(),
            false,
            &mut None,
        )
        .unwrap();

        assert_eq!(base_asset_amount, 0);
    }

    #[test]
    fn maker_taker_same_direction() {
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Market,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 200 * PRICE_PRECISION_I64,
                auction_end_price: 100 * PRICE_PRECISION_I64,
                auction_duration: 5,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: 100 * BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                price: 200 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: 100 * BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut market = PerpMarket::default_test();

        let now = 1_i64;
        let slot = 1_u64;

        let fee_structure = FeeStructure::test_default();

        let (taker_key, maker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let taker_limit_price = taker.orders[0]
            .get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;

        let (base_asset_amount, _, _) = fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut get_oracle_map(),
            false,
            &mut None,
        )
        .unwrap();

        assert_eq!(base_asset_amount, 0);
    }

    #[test]
    fn maker_taker_different_market_index() {
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 1,
                order_type: OrderType::Market,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                auction_start_price: 200 * PRICE_PRECISION_I64,
                auction_end_price: 100 * PRICE_PRECISION_I64,
                auction_duration: 5,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: 100 * BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                price: 200 * PRICE_PRECISION_U64,
                auction_duration: 5,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: 100 * BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut market = PerpMarket::default_test();

        let now = 1_i64;
        let slot = 1_u64;

        let fee_structure = FeeStructure::test_default();

        let (taker_key, maker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let taker_limit_price = taker.orders[0]
            .get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;

        let (base_asset_amount, _, _) = fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut get_oracle_map(),
            false,
            &mut None,
        )
        .unwrap();

        assert_eq!(base_asset_amount, 0);
    }

    #[test]
    fn long_taker_order_bigger_than_maker() {
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: 100 * BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 100 * PRICE_PRECISION_I64,
                auction_end_price: 200 * PRICE_PRECISION_I64,
                auction_duration: 5,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: 100 * BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                price: 120 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut market = PerpMarket::default_test();

        let now = 1_i64;
        let slot = 1_u64;

        let fee_structure = FeeStructure::test_default();

        let (taker_key, maker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let taker_limit_price = taker.orders[0]
            .get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;

        fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut get_oracle_map(),
            false,
            &mut None,
        )
        .unwrap();

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, -120120000);
        assert_eq!(
            taker_position.quote_entry_amount,
            -120 * QUOTE_PRECISION_I64
        );
        assert_eq!(taker_position.quote_break_even_amount, -120120000);
        assert_eq!(taker_stats.taker_volume_30d, 120 * QUOTE_PRECISION_U64);

        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, -BASE_PRECISION_I64);
        assert_eq!(maker_position.quote_asset_amount, 120072000);
        assert_eq!(maker_position.quote_entry_amount, 120 * QUOTE_PRECISION_I64);
        assert_eq!(maker_position.quote_break_even_amount, 120072000);
        assert_eq!(maker_stats.maker_volume_30d, 120 * QUOTE_PRECISION_U64);

        assert_eq!(market.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market.base_asset_amount_long, BASE_PRECISION_I128);
        assert_eq!(market.base_asset_amount_short, -BASE_PRECISION_I128);
        assert_eq!(market.quote_asset_amount, -48000);
    }

    #[test]
    fn long_taker_order_smaller_than_maker() {
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 100 * PRICE_PRECISION_I64,
                auction_end_price: 200 * PRICE_PRECISION_I64,
                auction_duration: 5,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: 100 * BASE_PRECISION_U64,
                price: 120 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: 100 * BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut market = PerpMarket::default_test();

        let now = 1_i64;
        let slot = 1_u64;

        let fee_structure = FeeStructure::test_default();

        let (taker_key, maker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let taker_limit_price = taker.orders[0]
            .get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;

        fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut get_oracle_map(),
            false,
            &mut None,
        )
        .unwrap();

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, -120120000);
        assert_eq!(
            taker_position.quote_entry_amount,
            -120 * QUOTE_PRECISION_I64
        );
        assert_eq!(taker_position.quote_break_even_amount, -120120000);
        assert_eq!(taker_stats.taker_volume_30d, 120 * QUOTE_PRECISION_U64);

        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, -BASE_PRECISION_I64);
        assert_eq!(maker_position.quote_asset_amount, 120072000);
        assert_eq!(maker_position.quote_entry_amount, 120 * QUOTE_PRECISION_I64);
        assert_eq!(maker_position.quote_break_even_amount, 120072000);
        assert_eq!(maker_stats.maker_volume_30d, 120 * QUOTE_PRECISION_U64);

        assert_eq!(market.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market.base_asset_amount_long, BASE_PRECISION_I128);
        assert_eq!(market.base_asset_amount_short, -BASE_PRECISION_I128);
        assert_eq!(market.quote_asset_amount, -48000);
    }

    #[test]
    fn double_dutch_auction() {
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 100 * PRICE_PRECISION_I64,
                auction_end_price: 200 * PRICE_PRECISION_I64,
                auction_duration: 10,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Market,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 200 * PRICE_PRECISION_I64,
                auction_end_price: 100 * PRICE_PRECISION_I64,
                auction_duration: 10,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut market = PerpMarket::default_test();

        let now = 5_i64;
        let slot = 5_u64;

        let fee_structure = get_fee_structure();

        let (taker_key, maker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let taker_limit_price = taker.orders[0]
            .get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = taker.orders[0]
            .force_get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut get_oracle_map(),
            false,
            &mut None,
        )
        .unwrap();

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, -150075000);
        assert_eq!(
            taker_position.quote_entry_amount,
            -150 * QUOTE_PRECISION_I64
        );
        assert_eq!(taker_position.quote_break_even_amount, -150075000);
        assert_eq!(taker_position.open_bids, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 75000);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 150 * QUOTE_PRECISION_U64);
        assert!(taker.orders[0].is_available());

        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, -BASE_PRECISION_I64);
        assert_eq!(maker_position.quote_asset_amount, 150045000);
        assert_eq!(maker_position.quote_entry_amount, 150 * QUOTE_PRECISION_I64);
        assert_eq!(maker_position.quote_break_even_amount, 150045000);
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_asks, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 45000);
        assert_eq!(maker_stats.maker_volume_30d, 150 * QUOTE_PRECISION_U64);
        assert!(maker.orders[0].is_available());

        assert_eq!(market.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market.base_asset_amount_long, BASE_PRECISION_I128);
        assert_eq!(market.base_asset_amount_short, -BASE_PRECISION_I128);
        assert_eq!(market.quote_asset_amount, -30000);
        assert_eq!(market.fee_ledger.total_exchange_fee, 75000);
        assert_eq!(market.fee_ledger.pending_protocol_fee, 30000);
        assert_eq!(market.fee_ledger.pending_if_fee, 0);
        assert_eq!(market.amm.total_fee, 0);
        assert_eq!(market.amm.total_fee_minus_distributions, 0);
        assert_eq!(market.amm.net_revenue_since_last_funding, 0);
    }

    #[test]
    fn taker_bid_crosses_maker_ask() {
        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 100 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                price: 150 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut market = PerpMarket::default_test();

        let now = 5_i64;
        let slot = 5_u64;

        let fee_structure = get_fee_structure();
        let (maker_key, taker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let taker_limit_price = taker.orders[0]
            .get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;

        fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut get_oracle_map(),
            false,
            &mut None,
        )
        .unwrap();

        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, -BASE_PRECISION_I64);
        assert_eq!(maker_position.quote_asset_amount, 100030000);
        assert_eq!(maker_position.quote_entry_amount, 100 * QUOTE_PRECISION_I64);
        assert_eq!(maker_position.quote_break_even_amount, 100030000);
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_asks, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 30000);
        assert_eq!(maker_stats.maker_volume_30d, 100 * QUOTE_PRECISION_U64);
        assert!(maker.orders[0].is_available());

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, -100050000);
        assert_eq!(
            taker_position.quote_entry_amount,
            -100 * QUOTE_PRECISION_I64
        );
        assert_eq!(taker_position.quote_break_even_amount, -100050000);
        assert_eq!(taker_position.open_bids, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 50000);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 100 * QUOTE_PRECISION_U64);
        assert!(taker.orders[0].is_available());

        assert_eq!(market.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market.base_asset_amount_long, BASE_PRECISION_I128);
        assert_eq!(market.base_asset_amount_short, -BASE_PRECISION_I128);
        assert_eq!(market.quote_asset_amount, -20000);
        assert_eq!(market.fee_ledger.total_exchange_fee, 50000);
        assert_eq!(market.fee_ledger.pending_protocol_fee, 20000);
        assert_eq!(market.fee_ledger.pending_if_fee, 0);
        assert_eq!(market.amm.total_fee, 0);
        assert_eq!(market.amm.total_fee_minus_distributions, 0);
        assert_eq!(market.amm.net_revenue_since_last_funding, 0);
    }

    #[test]
    fn taker_ask_crosses_maker_bid() {
        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
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
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                price: 50 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut market = PerpMarket::default_test();

        let now = 5_i64;
        let slot = 5_u64;

        let fee_structure = get_fee_structure();

        let (maker_key, taker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let taker_limit_price = taker.orders[0]
            .get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;

        fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut get_oracle_map(),
            false,
            &mut None,
        )
        .unwrap();

        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(maker_position.quote_asset_amount, -99970000);
        assert_eq!(
            maker_position.quote_entry_amount,
            -100 * QUOTE_PRECISION_I64
        );
        assert_eq!(maker_position.quote_break_even_amount, -99970000);
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_bids, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 30000);
        assert!(maker.orders[0].is_available());
        assert_eq!(maker_stats.maker_volume_30d, 100 * QUOTE_PRECISION_U64);

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, -BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, 99950000);
        assert_eq!(taker_position.quote_entry_amount, 100 * QUOTE_PRECISION_I64);
        assert_eq!(taker_position.quote_break_even_amount, 99950000);
        assert_eq!(taker_position.open_asks, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 50000);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 100 * QUOTE_PRECISION_U64);
        assert!(taker.orders[0].is_available());

        assert_eq!(market.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market.base_asset_amount_long, BASE_PRECISION_I128);
        assert_eq!(market.base_asset_amount_short, -BASE_PRECISION_I128);
        assert_eq!(market.quote_asset_amount, -20000);
        assert_eq!(market.fee_ledger.total_exchange_fee, 50000);
        assert_eq!(market.fee_ledger.pending_protocol_fee, 20000);
        assert_eq!(market.fee_ledger.pending_if_fee, 0);
        assert_eq!(market.amm.total_fee, 0);
        assert_eq!(market.amm.total_fee_minus_distributions, 0);
        assert_eq!(market.amm.net_revenue_since_last_funding, 0);
    }

    #[test]
    fn fallback_price_doesnt_cross_maker() {
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 0,
                auction_duration: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                price: 120 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_spread: 100,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            order_tick_size: 1,
            ..PerpMarket::default_test()
        };

        let now = 0_i64;
        let slot = 0_u64;

        let fee_structure = get_fee_structure();

        let (taker_key, maker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let taker_limit_price = taker.orders[0]
            .get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;

        let (base_asset_amount, _, _) = fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut get_oracle_map(),
            false,
            &mut None,
        )
        .unwrap();

        assert_eq!(base_asset_amount, 0);
    }

    #[test]
    fn fallback_price_crosses_maker() {
        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 0,
                auction_duration: 0,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                price: 105 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                // Wide spread so the AMM fallback ask sits above the maker's
                // resting 105 short — half of this is added on the long side.
                base_spread: (BID_ASK_SPREAD_PRECISION / 5) as u32,
                max_spread: (BID_ASK_SPREAD_PRECISION / 5) as u32,
                amm_jit_intensity: 0,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            order_tick_size: 1,
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

        let now = 0_i64;
        let slot = 0_u64;

        let fee_structure = get_fee_structure();

        let (taker_key, maker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let taker_limit_price = taker.orders[0]
            .get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;

        let (base_asset_amount, _, _) = fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut get_oracle_map(),
            false,
            &mut None,
        )
        .unwrap();

        assert_eq!(base_asset_amount, 1000000000);
    }

    #[test]
    fn taker_oracle_bid_crosses_maker_ask() {
        let now = 50000_i64;
        let slot = 50000_u64;

        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 100 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Oracle,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                auction_start_price: 999 * PRICE_PRECISION_I64 / 10, // $99.9
                auction_end_price: 100 * PRICE_PRECISION_I64,
                auction_duration: 10, // auction is 1 cent per slot
                slot: slot - 5,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut oracle_price = get_pyth_price(100, 6);
        oracle_price.posted_slot = slot - 10000;
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let mut market = PerpMarket::default_test();
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap = 999 * PRICE_PRECISION_I64 / 10;
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap_ts = now - 1;
        market.oracle = oracle_price_key;
        market.oracle_source = crate::state::oracle::OracleSource::PythLazer;

        let (opd, ov) = oracle_map
            .get_price_data_and_validity(
                MarketType::Perp,
                market.market_index,
                &(
                    oracle_price_key,
                    crate::state::oracle::OracleSource::PythLazer,
                ),
                market
                    .market_stats
                    .historical_oracle_data
                    .last_oracle_price_twap,
                market.get_max_confidence_interval_multiplier().unwrap(),
                0,
                0,
                None,
            )
            .unwrap();

        assert_eq!(opd.delay, 10000); // quite long time
        assert_eq!(ov, OracleValidity::StaleForMargin);

        let fee_structure = get_fee_structure();
        let (maker_key, taker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let oracle_price = 100 * PRICE_PRECISION_I64;

        let valid_oracle_price = Some(oracle_price);
        let taker_limit_price = taker.orders[0]
            .get_limit_price(valid_oracle_price, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;

        fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            Some(oracle_price),
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut oracle_map,
            false,
            &mut None,
        )
        .unwrap();

        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, -BASE_PRECISION_I64);
        assert_eq!(maker_position.quote_asset_amount, 100030000);
        assert_eq!(maker_position.quote_entry_amount, 100 * QUOTE_PRECISION_I64);
        assert_eq!(maker_position.quote_break_even_amount, 100030000);
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_asks, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 30000);
        assert_eq!(maker_stats.maker_volume_30d, 100 * QUOTE_PRECISION_U64);
        assert!(maker.orders[0].is_available());

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, -100050000);
        assert_eq!(
            taker_position.quote_entry_amount,
            -100 * QUOTE_PRECISION_I64
        );
        assert_eq!(taker_position.quote_break_even_amount, -100050000);
        assert_eq!(taker_position.open_bids, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 50000);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 100 * QUOTE_PRECISION_U64);
        assert!(taker.orders[0].is_available());

        assert_eq!(market.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market.base_asset_amount_long, BASE_PRECISION_I128);
        assert_eq!(market.base_asset_amount_short, -BASE_PRECISION_I128);
        assert_eq!(market.quote_asset_amount, -20000);
        assert_eq!(market.fee_ledger.total_exchange_fee, 50000);
        assert_eq!(market.fee_ledger.pending_protocol_fee, 20000);
        assert_eq!(market.fee_ledger.pending_if_fee, 0);
        assert_eq!(market.amm.total_fee, 0);
        assert_eq!(market.amm.total_fee_minus_distributions, 0);
        assert_eq!(market.amm.net_revenue_since_last_funding, 0);
    }

    #[test]
    fn taker_oracle_bid_after_auction_crosses_maker_ask() {
        let now = 11_i64;
        let slot = 11_u64;

        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 100 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Oracle,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                auction_start_price: 0,
                auction_end_price: 0,
                oracle_price_offset: (100 * PRICE_PRECISION_I64),
                auction_duration: 10,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut oracle_price = get_pyth_price(99, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let taker_price = taker.orders[0]
            .get_limit_price(
                Some(
                    oracle_map
                        .get_price_data(&(
                            oracle_price_key,
                            crate::state::oracle::OracleSource::PythLazer,
                        ))
                        .unwrap()
                        .price,
                ),
                None,
                slot,
                1,
            )
            .unwrap();
        assert_eq!(taker_price, Some(199000000)); // $51

        let mut market = PerpMarket::default_test();
        market.oracle = oracle_price_key;
        market.oracle_source = crate::state::oracle::OracleSource::PythLazer;

        let fee_structure = get_fee_structure();
        let (maker_key, taker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let oracle_price = 100 * PRICE_PRECISION_I64;

        let maker_price = maker.orders[0].price;

        fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            Some(oracle_price),
            taker_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut oracle_map,
            false,
            &mut None,
        )
        .unwrap();

        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, -BASE_PRECISION_I64);
        assert_eq!(maker_position.quote_asset_amount, 100030000);
        assert_eq!(maker_position.quote_entry_amount, 100 * QUOTE_PRECISION_I64);
        assert_eq!(maker_position.quote_break_even_amount, 100030000);
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_asks, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 30000);
        assert_eq!(maker_stats.maker_volume_30d, 100 * QUOTE_PRECISION_U64);
        assert!(maker.orders[0].is_available());

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, -100050000);
        assert_eq!(
            taker_position.quote_entry_amount,
            -100 * QUOTE_PRECISION_I64
        );
        assert_eq!(taker_position.quote_break_even_amount, -100050000);
        assert_eq!(taker_position.open_bids, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 50000);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 100 * QUOTE_PRECISION_U64);
        assert!(taker.orders[0].is_available());

        assert_eq!(market.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market.base_asset_amount_long, BASE_PRECISION_I128);
        assert_eq!(market.base_asset_amount_short, -BASE_PRECISION_I128);
        assert_eq!(market.quote_asset_amount, -20000);
        assert_eq!(market.fee_ledger.total_exchange_fee, 50000);
        assert_eq!(market.fee_ledger.pending_protocol_fee, 20000);
        assert_eq!(market.fee_ledger.pending_if_fee, 0);
        assert_eq!(market.amm.total_fee, 0);
        assert_eq!(market.amm.total_fee_minus_distributions, 0);
        assert_eq!(market.amm.net_revenue_since_last_funding, 0);
    }

    #[test]
    fn taker_oracle_ask_crosses_maker_bid() {
        let now = 5_i64;
        let slot = 5_u64;

        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
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
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Oracle,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                auction_start_price: 0,
                auction_end_price: -100 * PRICE_PRECISION_I64,
                auction_duration: 10,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let mut market = PerpMarket::default_test();
        market.oracle = oracle_price_key;
        market.oracle_source = crate::state::oracle::OracleSource::PythLazer;

        let fee_structure = get_fee_structure();

        let (maker_key, taker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let valid_oracle_price = Some(
            oracle_map
                .get_price_data(&(
                    oracle_price_key,
                    crate::state::oracle::OracleSource::PythLazer,
                ))
                .unwrap()
                .price,
        );
        let taker_limit_price = taker.orders[0]
            .get_limit_price(valid_oracle_price, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;

        fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut oracle_map,
            false,
            &mut None,
        )
        .unwrap();

        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(maker_position.quote_asset_amount, -99970000);
        assert_eq!(
            maker_position.quote_entry_amount,
            -100 * QUOTE_PRECISION_I64
        );
        assert_eq!(maker_position.quote_break_even_amount, -99970000);
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_bids, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 30000);
        assert!(maker.orders[0].is_available());
        assert_eq!(maker_stats.maker_volume_30d, 100 * QUOTE_PRECISION_U64);

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, -BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, 99950000);
        assert_eq!(taker_position.quote_entry_amount, 100 * QUOTE_PRECISION_I64);
        assert_eq!(taker_position.quote_break_even_amount, 99950000);
        assert_eq!(taker_position.open_asks, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 50000);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 100 * QUOTE_PRECISION_U64);
        assert!(taker.orders[0].is_available());

        assert_eq!(market.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market.base_asset_amount_long, BASE_PRECISION_I128);
        assert_eq!(market.base_asset_amount_short, -BASE_PRECISION_I128);
        assert_eq!(market.quote_asset_amount, -20000);
        assert_eq!(market.fee_ledger.total_exchange_fee, 50000);
        assert_eq!(market.fee_ledger.pending_protocol_fee, 20000);
        assert_eq!(market.fee_ledger.pending_if_fee, 0);
        assert_eq!(market.amm.total_fee, 0);
        assert_eq!(market.amm.total_fee_minus_distributions, 0);
        assert_eq!(market.amm.net_revenue_since_last_funding, 0);
    }

    #[test]
    fn taker_oracle_ask_after_action_crosses_maker_bid() {
        let now = 11_i64;
        let slot = 11_u64;

        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
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
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Oracle,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                auction_start_price: 0,
                auction_end_price: 0,
                oracle_price_offset: (-50 * PRICE_PRECISION_I64),
                auction_duration: 10,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut oracle_price = get_pyth_price(101, 6);
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            oracle_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

        let mut market = PerpMarket::default_test();
        market.oracle = oracle_price_key;
        market.oracle_source = crate::state::oracle::OracleSource::PythLazer;

        let taker_price = taker.orders[0]
            .get_limit_price(
                Some(
                    oracle_map
                        .get_price_data(&(
                            oracle_price_key,
                            crate::state::oracle::OracleSource::PythLazer,
                        ))
                        .unwrap()
                        .price,
                ),
                None,
                slot,
                1,
            )
            .unwrap();
        assert_eq!(taker_price, Some(51000000)); // $51

        let fee_structure = get_fee_structure();

        let (maker_key, taker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let maker_price = maker.orders[0].price;

        fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut oracle_map,
            false,
            &mut None,
        )
        .unwrap();

        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(maker_position.quote_asset_amount, -99970000);
        assert_eq!(
            maker_position.quote_entry_amount,
            -100 * QUOTE_PRECISION_I64
        );
        assert_eq!(maker_position.quote_break_even_amount, -99970000);
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_bids, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 30000);
        assert!(maker.orders[0].is_available());
        assert_eq!(maker_stats.maker_volume_30d, 100 * QUOTE_PRECISION_U64);

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, -BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, 99950000);
        assert_eq!(taker_position.quote_entry_amount, 100 * QUOTE_PRECISION_I64);
        assert_eq!(taker_position.quote_break_even_amount, 99950000);
        assert_eq!(taker_position.open_asks, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 50000);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 100 * QUOTE_PRECISION_U64);
        assert!(taker.orders[0].is_available());

        assert_eq!(market.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market.base_asset_amount_long, BASE_PRECISION_I128);
        assert_eq!(market.base_asset_amount_short, -BASE_PRECISION_I128);
        assert_eq!(market.quote_asset_amount, -20000);
        assert_eq!(market.fee_ledger.total_exchange_fee, 50000);
        assert_eq!(market.fee_ledger.pending_protocol_fee, 20000);
        assert_eq!(market.fee_ledger.pending_if_fee, 0);
        assert_eq!(market.amm.total_fee, 0);
        assert_eq!(market.amm.total_fee_minus_distributions, 0);
        assert_eq!(market.amm.net_revenue_since_last_funding, 0);
    }

    #[test]
    fn limit_auction_crosses_maker_bid() {
        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
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
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                price: 10 * PRICE_PRECISION_U64,
                auction_end_price: 10 * PRICE_PRECISION_I64,
                auction_start_price: 100 * PRICE_PRECISION_I64,
                auction_duration: 10,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut market = PerpMarket::default_test();

        let now = 5_i64;
        let slot = 5_u64;

        assert_eq!(
            taker.orders[0]
                .get_limit_price(None, None, slot, market.order_tick_size)
                .unwrap(),
            Some(55000000)
        );

        let fee_structure = get_fee_structure();

        let (maker_key, taker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let taker_limit_price = taker.orders[0]
            .get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;

        fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut get_oracle_map(),
            false,
            &mut None,
        )
        .unwrap();

        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(maker_position.quote_asset_amount, -99970000);
        assert_eq!(
            maker_position.quote_entry_amount,
            -100 * QUOTE_PRECISION_I64
        );
        assert_eq!(maker_position.quote_break_even_amount, -99970000);
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_bids, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 30000);
        assert!(maker.orders[0].is_available());
        assert_eq!(maker_stats.maker_volume_30d, 100 * QUOTE_PRECISION_U64);

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, -BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, 99950000);
        assert_eq!(taker_position.quote_entry_amount, 100 * QUOTE_PRECISION_I64);
        assert_eq!(taker_position.quote_break_even_amount, 99950000);
        assert_eq!(taker_position.open_asks, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 50000);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 100 * QUOTE_PRECISION_U64);
        assert!(taker.orders[0].is_available());

        assert_eq!(market.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market.base_asset_amount_long, BASE_PRECISION_I128);
        assert_eq!(market.base_asset_amount_short, -BASE_PRECISION_I128);
        assert_eq!(market.quote_asset_amount, -20000);
        assert_eq!(market.fee_ledger.total_exchange_fee, 50000);
        assert_eq!(market.fee_ledger.pending_protocol_fee, 20000);
        assert_eq!(market.fee_ledger.pending_if_fee, 0);
        assert_eq!(market.amm.total_fee, 0);
        assert_eq!(market.amm.total_fee_minus_distributions, 0);
        assert_eq!(market.amm.net_revenue_since_last_funding, 0);
    }

    #[test]
    fn limit_auction_crosses_maker_ask() {
        let mut maker = User {
            orders: get_orders(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 100 * PRICE_PRECISION_U64,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                order_type: OrderType::Limit,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                price: 150 * PRICE_PRECISION_U64,
                auction_start_price: 50 * PRICE_PRECISION_I64,
                auction_end_price: 150 * PRICE_PRECISION_I64,
                auction_duration: 10,
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 1,
                open_bids: BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        let mut market = PerpMarket::default_test();

        let now = 5_i64;
        let slot = 5_u64;

        assert_eq!(
            taker.orders[0]
                .get_limit_price(None, None, slot, market.order_tick_size)
                .unwrap(),
            Some(100000000)
        );

        let fee_structure = get_fee_structure();
        let (maker_key, taker_key, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let taker_limit_price = taker.orders[0]
            .get_limit_price(None, None, slot, market.order_tick_size)
            .unwrap();

        let maker_price = maker.orders[0].price;

        fulfill_perp_order_with_match(
            &mut market,
            &mut taker,
            &mut taker_stats,
            0,
            &taker_key,
            &mut maker,
            &mut Some(&mut maker_stats),
            0,
            &maker_key,
            &mut None,
            &mut None,
            &filler_key,
            0,
            None,
            taker_limit_price,
            maker_price,
            now,
            slot,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            &mut get_oracle_map(),
            false,
            &mut None,
        )
        .unwrap();

        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, -BASE_PRECISION_I64);
        assert_eq!(maker_position.quote_asset_amount, 100030000);
        assert_eq!(maker_position.quote_entry_amount, 100 * QUOTE_PRECISION_I64);
        assert_eq!(maker_position.quote_break_even_amount, 100030000);
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_asks, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 30000);
        assert_eq!(maker_stats.maker_volume_30d, 100 * QUOTE_PRECISION_U64);
        assert!(maker.orders[0].is_available());

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, -100050000);
        assert_eq!(
            taker_position.quote_entry_amount,
            -100 * QUOTE_PRECISION_I64
        );
        assert_eq!(taker_position.quote_break_even_amount, -100050000);
        assert_eq!(taker_position.open_bids, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 50000);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 100 * QUOTE_PRECISION_U64);
        assert!(taker.orders[0].is_available());

        assert_eq!(market.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market.base_asset_amount_long, BASE_PRECISION_I128);
        assert_eq!(market.base_asset_amount_short, -BASE_PRECISION_I128);
        assert_eq!(market.quote_asset_amount, -20000);
        assert_eq!(market.fee_ledger.total_exchange_fee, 50000);
        assert_eq!(market.fee_ledger.pending_protocol_fee, 20000);
        assert_eq!(market.fee_ledger.pending_if_fee, 0);
        assert_eq!(market.amm.total_fee, 0);
        assert_eq!(market.amm.total_fee_minus_distributions, 0);
        assert_eq!(market.amm.net_revenue_since_last_funding, 0);
    }
}

pub mod fulfill_order {
    use {
        super::*,
        crate::{
            controller::{
                orders::{fill_perp_order, fulfill_perp_order, validate_market_within_price_band},
                position::PositionDirection,
            },
            create_anchor_account_info,
            error::ErrorCode,
            get_orders,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I64, BASE_PRECISION_U64,
                    MAX_CONCENTRATION_COEFFICIENT, PEG_PRECISION, PRICE_PRECISION,
                    PRICE_PRECISION_I64, PRICE_PRECISION_U64, QUOTE_PRECISION_I64,
                    QUOTE_PRECISION_U64, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                margin::calculate_margin_requirement_and_total_collateral_and_liability_info,
            },
            state::{
                fill_mode::FillMode,
                margin_calculation::MarginContext,
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
                    slots_before_stale_for_amm: 10,     // 5s
                    slots_before_stale_for_margin: 120, // 60s
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
            min_perp_auction_duration: 1,
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
    fn fulfill_with_amm_and_maker() {
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

        let maker_key = Pubkey::default();
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
                price: 100_010_000 * PRICE_PRECISION_U64 / 1_000_000, // .01 worse than amm
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
        create_anchor_account_info!(maker, User, maker_account_info);
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

        let order_index = 0;
        let min_auction_duration = 0;
        let user_can_skip_auction_duration = taker
            .can_skip_auction_duration(&taker_stats, false)
            .unwrap();
        let is_amm_available = get_amm_is_available(
            &taker.orders[order_index],
            min_auction_duration,
            &market,
            &mut oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        let (base_asset_amount, _) = fulfill_perp_order(
            &mut taker,
            order_index,
            &taker_key,
            &mut taker_stats,
            &makers_and_referrers,
            &maker_and_referrer_stats,
            &[(
                Pubkey::default(),
                0,
                100_010_000 * PRICE_PRECISION_U64 / 1_000_000,
            )],
            &mut Some(&mut filler),
            &filler_key,
            &mut Some(&mut filler_stats),
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            100 * PRICE_PRECISION_U64,
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            is_amm_available,
            true,
            FillMode::Fill,
            false,
            &mut None,
            false,
            0,
        )
        .unwrap();

        assert_eq!(base_asset_amount, BASE_PRECISION_U64);

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, -100306387);
        assert_eq!(taker_position.quote_entry_amount, -100256258);
        assert_eq!(taker_position.quote_break_even_amount, -100306387);
        assert_eq!(taker_position.open_bids, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 50129);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 100256237);
        assert!(taker.orders[0].is_available());

        let maker = makers_and_referrers.get_ref_mut(&maker_key).unwrap();
        let maker_stats = maker_and_referrer_stats
            .get_ref_mut(&maker_authority)
            .unwrap();
        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, -BASE_PRECISION_I64 / 2);
        assert_eq!(maker_position.quote_break_even_amount, 50_020_001);
        assert_eq!(maker_position.quote_entry_amount, 50_005_000);
        assert_eq!(maker_position.quote_asset_amount, 50_020_001); // 50_005_000 + 50_005_000 * .0003
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_asks, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 15001);
        assert_eq!(maker_stats.maker_volume_30d, 50_005_000);
        assert!(maker.orders[0].is_available());

        assert_eq!(filler_stats.filler_volume_30d, 100_256_237);
        assert_eq!(filler.perp_positions[0].quote_asset_amount, 5012);

        let market_after = market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.amm.base_asset_amount_with_amm, 500000000);
        assert_eq!(market_after.base_asset_amount_long, 1000000000);
        assert_eq!(market_after.base_asset_amount_short, -500000000);
        assert_eq!(market_after.quote_asset_amount, -50281374);

        assert_eq!(market_after.fee_ledger.total_exchange_fee, 50129);
        // amm numerator is 0: the AMM books only its spread surplus; the
        // remainders of BOTH halves (AMM fill 22614 + DLOB match 7502) are
        // the protocol's pending carveout
        assert_eq!(market_after.fee_ledger.pending_protocol_fee, 30116);
        assert_eq!(market_after.fee_ledger.pending_if_fee, 0);
        assert_eq!(market_after.fee_ledger.pending_amm_provision, 0);
        assert_eq!(market_after.amm.total_fee, 1);
        assert_eq!(market_after.amm.total_fee_minus_distributions, 1);
        assert_eq!(market_after.amm.net_revenue_since_last_funding, 1);

        let reserve_price = market_after.amm.reserve_price().unwrap();
        assert_eq!(reserve_price, 101_007_550);
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            &mut oracle_map,
            slot,
            user_can_skip_auction_duration,
        );
        assert!(is_amm_available);

        let (base_asset_amount, quote_asset_amount) = fulfill_perp_order(
            &mut taker,
            order_index,
            &taker_key,
            &mut taker_stats,
            &UserMap::empty(),
            &UserStatsMap::empty(),
            &[],
            &mut Some(&mut filler),
            &filler_key,
            &mut Some(&mut filler_stats),
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::OracleGuardRails::default().validity,
            &fee_structure,
            100 * PRICE_PRECISION_U64, // stale stored reserve price
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            is_amm_available,
            true,
            FillMode::Fill,
            false,
            &mut None,
            false,
            0,
        )
        .unwrap();

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
        let market_after = market_map.get_ref(&0).unwrap();
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            &mut oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        let (base_asset_amount, quote_asset_amount) = fulfill_perp_order(
            &mut taker,
            order_index,
            &taker_key,
            &mut taker_stats,
            &UserMap::empty(),
            &UserStatsMap::empty(),
            &[],
            &mut Some(&mut filler),
            &filler_key,
            &mut Some(&mut filler_stats),
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::OracleGuardRails::default().validity,
            &fee_structure,
            100 * PRICE_PRECISION_U64,
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            is_amm_available,
            true,
            FillMode::Fill,
            false,
            &mut None,
            false,
            0,
        )
        .unwrap();

        // No projection, no cross, no fill; pre-change behavior preserved.
        assert_eq!(base_asset_amount, 0);
        assert_eq!(quote_asset_amount, 0);

        let market_after = market_map.get_ref(&0).unwrap();
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            &mut oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        let (base_asset_amount, quote_asset_amount) = fulfill_perp_order(
            &mut taker,
            order_index,
            &taker_key,
            &mut taker_stats,
            &UserMap::empty(),
            &UserStatsMap::empty(),
            &[],
            &mut Some(&mut filler),
            &filler_key,
            &mut Some(&mut filler_stats),
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::OracleGuardRails::default().validity,
            &fee_structure,
            100 * PRICE_PRECISION_U64, // stale stored reserve price
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            is_amm_available,
            true,
            FillMode::Fill,
            false,
            &mut None,
            false,
            0,
        )
        .unwrap();

        // No cross, so no fill.
        assert_eq!(base_asset_amount, 0);
        assert_eq!(quote_asset_amount, 0);

        // The curve was still refreshed toward the oracle: reserve price
        // snapped from the stored 100 to near 102, and `last_update_slot`
        // advanced to this slot so a subsequent same-slot fill skips the
        // projection.
        let market_after = market_map.get_ref(&0).unwrap();
        let reserve_price_after = market_after.amm.reserve_price().unwrap();
        assert!(reserve_price_after > 101 * PRICE_PRECISION as u64);
        assert!(reserve_price_after < 103 * PRICE_PRECISION as u64);
        assert_eq!(market_after.amm.last_update_slot, slot);
    }

    #[test]
    fn fulfill_with_multiple_maker_orders() {
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

        let maker_key = Pubkey::default();
        let maker_authority =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        let mut maker = User {
            authority: maker_authority,
            orders: get_orders!(
                Order {
                    market_index: 0,
                    post_only: true,
                    order_type: OrderType::Limit,
                    direction: PositionDirection::Short,
                    base_asset_amount: BASE_PRECISION_U64 / 2,
                    price: 90 * PRICE_PRECISION_U64,
                    ..Order::default()
                },
                Order {
                    market_index: 0,
                    post_only: true,
                    order_type: OrderType::Limit,
                    direction: PositionDirection::Short,
                    base_asset_amount: BASE_PRECISION_U64 / 2,
                    price: 95 * PRICE_PRECISION_U64, // .01 worse than amm
                    ..Order::default()
                }
            ),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 2,
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
        create_anchor_account_info!(maker, User, maker_account_info);
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

        let order_index = 0;
        let min_auction_duration = 10;
        let user_can_skip_auction_duration = taker
            .can_skip_auction_duration(&taker_stats, false)
            .unwrap();
        let is_amm_available = get_amm_is_available(
            &taker.orders[order_index],
            min_auction_duration,
            &market,
            &mut oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        let (base_asset_amount, _) = fulfill_perp_order(
            &mut taker,
            order_index,
            &taker_key,
            &mut taker_stats,
            &makers_and_referrers,
            &maker_and_referrer_stats,
            &[
                (maker_key, 0, 90 * PRICE_PRECISION_U64),
                (maker_key, 1, 95 * PRICE_PRECISION_U64),
            ],
            &mut Some(&mut filler),
            &filler_key,
            &mut Some(&mut filler_stats),
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            100 * PRICE_PRECISION_U64,
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            is_amm_available,
            true,
            FillMode::Fill,
            false,
            &mut None,
            false,
            0,
        )
        .unwrap();

        assert_eq!(base_asset_amount, BASE_PRECISION_U64);

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, -92546250);
        assert_eq!(taker_position.quote_entry_amount, -92500000);
        assert_eq!(taker_position.quote_break_even_amount, -92546250);
        assert_eq!(taker_position.open_bids, 0);
        assert_eq!(taker_position.open_orders, 0);

        let maker = makers_and_referrers.get_ref_mut(&maker_key).unwrap();
        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, -BASE_PRECISION_I64);
        assert_eq!(maker_position.quote_break_even_amount, 92527750);
        assert_eq!(maker_position.quote_entry_amount, 92500000);
        assert_eq!(maker_position.quote_asset_amount, 92527750);
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_asks, 0);
    }

    #[test]
    fn fulfill_with_maker_then_amm() {
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
                peg_multiplier: 100_050 * PEG_PRECISION / 1000,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                base_spread: 100, // 1 basis point
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
                price: 150 * PRICE_PRECISION_U64,
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

        let maker_key = Pubkey::default();
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
                price: 100_010_000 * PRICE_PRECISION_U64 / 1_000_000, // .01 worse than amm
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
        create_anchor_account_info!(maker, User, maker_account_info);
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

        let order_index = 0;
        let min_auction_duration = 0;
        let user_can_skip_auction_duration = taker
            .can_skip_auction_duration(&taker_stats, false)
            .unwrap();
        let is_amm_available = get_amm_is_available(
            &taker.orders[order_index],
            min_auction_duration,
            &market,
            &mut oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        let (base_asset_amount, _) = fulfill_perp_order(
            &mut taker,
            order_index,
            &taker_key,
            &mut taker_stats,
            &makers_and_referrers,
            &maker_and_referrer_stats,
            &[(maker_key, 0, 100_010_000 * PRICE_PRECISION_U64 / 1_000_000)],
            &mut Some(&mut filler),
            &filler_key,
            &mut Some(&mut filler_stats),
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            100 * PRICE_PRECISION_U64,
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            is_amm_available,
            true,
            FillMode::Fill,
            false,
            &mut None,
            false,
            0,
        )
        .unwrap();

        assert_eq!(base_asset_amount, BASE_PRECISION_U64);

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, -100334046);
        assert_eq!(taker_position.quote_entry_amount, -100283903);
        assert_eq!(taker_position.quote_break_even_amount, -100334046);
        assert_eq!(taker_position.open_bids, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 50143);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 100283883);
        assert!(taker.orders[0].is_available());

        let maker = makers_and_referrers.get_ref_mut(&maker_key).unwrap();
        let maker_stats = maker_and_referrer_stats
            .get_ref_mut(&maker_authority)
            .unwrap();
        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, -BASE_PRECISION_I64 / 2);
        assert_eq!(maker_position.quote_break_even_amount, 50_020_001);
        assert_eq!(maker_position.quote_entry_amount, 50_005_000);
        assert_eq!(maker_position.quote_asset_amount, 50_020_001); // 50_005_000 + 50_005_000 * .0003
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_asks, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 15001);
        assert_eq!(maker_stats.maker_volume_30d, 50_005_000);
        assert!(maker.orders[0].is_available());

        assert_eq!(filler_stats.filler_volume_30d, 100283883);
        assert_eq!(filler.perp_positions[0].quote_asset_amount, 5014);

        let market_after = market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.amm.base_asset_amount_with_amm, 500000000);
        assert_eq!(market_after.base_asset_amount_long, 1000000000);
        assert_eq!(market_after.base_asset_amount_short, -500000000);
        assert_eq!(market_after.quote_asset_amount, -50309031);

        let expected_market_fee = (taker_stats.fees.total_fee_paid
            - (maker_stats.fees.total_fee_rebate
                + filler.perp_positions[0].quote_asset_amount as u64))
            as i128;
        assert_eq!(expected_market_fee, 30128);
        assert_eq!(market_after.fee_ledger.total_exchange_fee, 50143);
        // amm numerator is 0: the AMM books only its spread surplus; both
        // halves' remainders (== expected_market_fee) are the protocol's
        // pending carveout
        assert_eq!(market_after.fee_ledger.pending_protocol_fee, 30128);
        assert_eq!(market_after.amm.total_fee, 2521);
        assert_eq!(market_after.amm.total_fee_minus_distributions, 2521);
        assert_eq!(market_after.amm.net_revenue_since_last_funding, 2521);

        let reserve_price = market_after.amm.reserve_price().unwrap();
        assert_eq!(reserve_price, 101_058_054);
    }

    #[test]
    fn fulfill_with_maker_with_auction_incomplete() {
        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_fill_reserve_fraction: 1,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 1,
            order_tick_size: 1,
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

        let mut oracle_map = get_oracle_map();

        let mut taker = User {
            orders: get_orders(Order {
                market_index: 0,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 100 * PRICE_PRECISION_I64,
                auction_end_price: 200 * PRICE_PRECISION_I64,
                auction_duration: 5,
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

        let maker_key = Pubkey::default();
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
                scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };
        create_anchor_account_info!(maker, User, maker_account_info);
        let makers_and_referrers = UserMap::load_one(&maker_account_info).unwrap();

        let now = 0_i64;
        let slot = 0_u64;

        let fee_structure = get_fee_structure();

        let (taker_key, _, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats {
            authority: maker_authority,
            ..UserStats::default()
        };
        create_anchor_account_info!(maker_stats, UserStats, maker_stats_account_info);
        let maker_and_referrer_stats = UserStatsMap::load_one(&maker_stats_account_info).unwrap();

        let order_index = 0;
        let min_auction_duration = 10;
        let user_can_skip_auction_duration = taker
            .can_skip_auction_duration(&taker_stats, false)
            .unwrap();
        let is_amm_available = get_amm_is_available(
            &taker.orders[order_index],
            min_auction_duration,
            &market,
            &mut oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        let (base_asset_amount, _) = fulfill_perp_order(
            &mut taker,
            order_index,
            &taker_key,
            &mut taker_stats,
            &makers_and_referrers,
            &maker_and_referrer_stats,
            &[(maker_key, 0, 100 * PRICE_PRECISION_U64)],
            &mut None,
            &filler_key,
            &mut None,
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            100 * PRICE_PRECISION_U64,
            None,
            now,
            slot,
            is_amm_available,
            true,
            FillMode::Fill,
            false,
            &mut None,
            false,
            0,
        )
        .unwrap();

        assert_eq!(base_asset_amount, BASE_PRECISION_U64 / 2);

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, BASE_PRECISION_I64 / 2);
        assert_eq!(taker_position.quote_asset_amount, -50025000);
        assert_eq!(taker_position.quote_entry_amount, -50 * QUOTE_PRECISION_I64);
        assert_eq!(taker_position.quote_break_even_amount, -50025000);
        assert_eq!(taker_position.open_bids, BASE_PRECISION_I64 / 2);
        assert_eq!(taker_position.open_orders, 1);
        assert_eq!(taker_stats.fees.total_fee_paid, 25000);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 50 * QUOTE_PRECISION_U64);

        let maker = makers_and_referrers.get_ref_mut(&maker_key).unwrap();
        let maker_stats = maker_and_referrer_stats
            .get_ref_mut(&maker_authority)
            .unwrap();
        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, -BASE_PRECISION_I64 / 2);
        assert_eq!(maker_position.quote_asset_amount, 50015000);
        assert_eq!(maker_position.quote_entry_amount, 50 * QUOTE_PRECISION_I64);
        assert_eq!(maker_position.quote_break_even_amount, 50015000);
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_asks, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 15000);
        assert_eq!(maker_stats.maker_volume_30d, 50 * QUOTE_PRECISION_U64);

        let market_after = market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market_after.base_asset_amount_long, 500000000);
        assert_eq!(market_after.base_asset_amount_short, -500000000);
        assert_eq!(market_after.quote_asset_amount, -10000);
        assert_eq!(market_after.fee_ledger.total_exchange_fee, 25000);
        assert_eq!(market_after.fee_ledger.pending_protocol_fee, 10000);
        assert_eq!(market_after.fee_ledger.pending_if_fee, 0);
        assert_eq!(market_after.amm.total_fee, 0);
        assert_eq!(market_after.amm.total_fee_minus_distributions, 0);
        assert_eq!(market_after.amm.net_revenue_since_last_funding, 0);
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            &mut oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        let (base_asset_amount, _) = fulfill_perp_order(
            &mut taker,
            order_index,
            &taker_key,
            &mut taker_stats,
            &UserMap::empty(),
            &UserStatsMap::empty(),
            &[],
            &mut None,
            &filler_key,
            &mut None,
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            100 * PRICE_PRECISION_U64,
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            is_amm_available,
            true,
            FillMode::Fill,
            false,
            &mut None,
            false,
            0,
        )
        .unwrap();

        assert_eq!(base_asset_amount, BASE_PRECISION_U64);

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, -101060608);
        assert_eq!(taker_position.quote_entry_amount, -101010102);
        assert_eq!(taker_position.quote_break_even_amount, -101060608);
        assert_eq!(taker_position.open_bids, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 50506);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 101010102);
        assert!(taker.orders[0].is_available());

        let market_after = market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.amm.base_asset_amount_with_amm, 1000000000);
        assert_eq!(market_after.base_asset_amount_long, 1000000000);
        assert_eq!(market_after.base_asset_amount_short, 0);
        assert_eq!(market_after.quote_asset_amount, -101060608);
        // amm numerator is 0: the taker-fee remainder is the protocol's
        // pending carveout; the AMM books nothing (no surplus here)
        assert_eq!(market_after.fee_ledger.pending_protocol_fee, 50506);
        assert_eq!(market_after.amm.total_fee, 0);
        assert_eq!(market_after.amm.total_fee_minus_distributions, 0);
        assert_eq!(market_after.amm.net_revenue_since_last_funding, 0);
    }

    #[test]
    fn maker_position_reducing_above_maintenance_check() {
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
                base_spread: 0, // 1 basis point
                ..AMM::default()
            },
            number_of_users_with_base: 1,
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

        let maker_key = Pubkey::default();
        let maker_authority =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        let mut maker = User {
            authority: maker_authority,
            orders: get_orders!(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: 2 * BASE_PRECISION_U64,
                price: 100 * PRICE_PRECISION_U64, // .01 worse than amm
                ..Order::default()
            }),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                open_orders: 1,
                open_asks: -2 * BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 501 * SPOT_BALANCE_PRECISION_U64 / 100,
                ..SpotPosition::default()
            }),
            ..User::default()
        };
        create_anchor_account_info!(maker, User, maker_account_info);
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

        let order_index = 0;
        let min_auction_duration = 10;
        let user_can_skip_auction_duration = taker
            .can_skip_auction_duration(&taker_stats, false)
            .unwrap();
        let is_amm_available = get_amm_is_available(
            &taker.orders[order_index],
            min_auction_duration,
            &market,
            &mut oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        let result = fulfill_perp_order(
            &mut taker,
            order_index,
            &taker_key,
            &mut taker_stats,
            &makers_and_referrers,
            &maker_and_referrer_stats,
            &[(maker_key, 0, 95 * PRICE_PRECISION_U64)],
            &mut Some(&mut filler),
            &filler_key,
            &mut Some(&mut filler_stats),
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            100 * PRICE_PRECISION_U64,
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            is_amm_available,
            true,
            FillMode::Fill,
            false,
            &mut None,
            false,
            0,
        );

        assert!(result.is_ok());

        let margin_calc = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &maker,
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::liquidation(0),
        )
        .unwrap();

        assert_eq!(
            margin_calc.margin_requirement,
            margin_calc.total_collateral as u128
        );
    }

    #[test]
    fn maker_insufficient_collateral() {
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

        let maker_key = Pubkey::default();
        let maker_authority =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        let mut maker = User {
            authority: maker_authority,
            orders: get_orders!(Order {
                market_index: 0,
                post_only: true,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                price: 95 * PRICE_PRECISION_U64, // .01 worse than amm
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
                scaled_balance: 10 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };
        create_anchor_account_info!(maker, User, maker_account_info);
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

        let order_index = 0;
        let min_auction_duration = 0;
        let user_can_skip_auction_duration = taker
            .can_skip_auction_duration(&taker_stats, false)
            .unwrap();
        let is_amm_available = get_amm_is_available(
            &taker.orders[order_index],
            min_auction_duration,
            &market,
            &mut oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        let result = fulfill_perp_order(
            &mut taker,
            order_index,
            &taker_key,
            &mut taker_stats,
            &makers_and_referrers,
            &maker_and_referrer_stats,
            &[(maker_key, 0, 95 * PRICE_PRECISION_U64)],
            &mut Some(&mut filler),
            &filler_key,
            &mut Some(&mut filler_stats),
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            100 * PRICE_PRECISION_U64,
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            is_amm_available,
            true,
            FillMode::Fill,
            false,
            &mut None,
            false,
            0,
        );

        assert_eq!(result, Err(ErrorCode::InsufficientCollateral));
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            &mut oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        let (base_asset_amount, _) = fulfill_perp_order(
            &mut taker,
            order_index,
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
            reserve_price_before,
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            is_amm_available,
            true,
            FillMode::Fill,
            false,
            &mut None,
            false,
            0,
        )
        .unwrap();

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

        let market_after = market_map.get_ref(&0).unwrap();
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

        let market_after = market_map.get_ref(&0).unwrap();
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            &mut oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        let (base_asset_amount, _) = fulfill_perp_order(
            &mut taker,
            order_index,
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
            reserve_price_before,
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            is_amm_available,
            true,
            FillMode::Fill,
            false,
            &mut None,
            false,
            0,
        )
        .unwrap();

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

        let market_after = market_map.get_ref(&0).unwrap();
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

        let market_after = market_map.get_ref(&0).unwrap();
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
            min_perp_auction_duration: 1,
            default_market_order_time_in_force: 10,
            ..State::default()
        };

        let (base_asset_amount, _) = fill_perp_order(
            1,
            &state,
            &user_account_loader,
            &user_stats_account_loader,
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &filler_account_loader,
            &filler_stats_account_loader,
            &UserMap::empty(),
            &UserStatsMap::empty(),
            None,
            &clock,
            FillMode::Fill,
            &mut None,
        )
        .unwrap();

        assert_eq!(base_asset_amount, 0);

        // Will fill if MM oracle price is not too volatile at mm oracle price
        market.market_stats.mm_oracle_price = 101 * PRICE_PRECISION_I64;
        create_anchor_account_info!(market, PerpMarket, market_account_info);
        let market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();

        let (base_asset_amount, quote_asset_amount) = fill_perp_order(
            1,
            &state,
            &user_account_loader,
            &user_stats_account_loader,
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &filler_account_loader,
            &filler_stats_account_loader,
            &UserMap::empty(),
            &UserStatsMap::empty(),
            None,
            &clock,
            FillMode::Fill,
            &mut None,
        )
        .unwrap();

        assert_eq!(base_asset_amount, BASE_PRECISION_U64);
        assert_eq!(quote_asset_amount, 101010102);
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
    //     let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();
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

    #[test]
    fn fulfill_users_with_multiple_orders_and_markets() {
        let mut sol_market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 1,
            order_tick_size: 1,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price: 100 * PRICE_PRECISION_I64,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(sol_market, PerpMarket, sol_market_account_info);
        let mut btc_market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 20000 * PEG_PRECISION,
                max_slippage_ratio: 50,
                max_fill_reserve_fraction: 100,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            market_index: 1,
            order_step_size: 1,
            order_tick_size: 1,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price: 20000 * PRICE_PRECISION_I64,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(btc_market, PerpMarket, btc_market_account_info);
        let market_map = PerpMarketMap::load_multiple(
            vec![&sol_market_account_info, &btc_market_account_info],
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
        let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();

        let mut oracle_map = get_oracle_map();

        let mut taker_orders = [Order::default(); 32];
        taker_orders[0] = Order {
            market_index: 0,
            status: OrderStatus::Open,
            order_type: OrderType::Market,
            direction: PositionDirection::Long,
            base_asset_amount: BASE_PRECISION_U64,
            slot: 0,
            auction_start_price: 100 * PRICE_PRECISION_I64,
            auction_end_price: 200 * PRICE_PRECISION_I64,
            auction_duration: 5,
            ..Order::default()
        };
        taker_orders[1] = Order {
            market_index: 1,
            status: OrderStatus::Open,
            order_type: OrderType::Market,
            direction: PositionDirection::Long,
            base_asset_amount: BASE_PRECISION_U64,
            slot: 0,
            auction_start_price: 20000 * PRICE_PRECISION_I64,
            auction_end_price: 20100 * PRICE_PRECISION_I64,
            auction_duration: 5,
            ..Order::default()
        };

        // Taker has sol order and position at index 0, btc at index 1
        let mut taker_positions = [PerpPosition::default(); 8];
        taker_positions[0] = PerpPosition {
            market_index: 0,
            open_orders: 1,
            open_bids: BASE_PRECISION_I64,
            ..PerpPosition::default()
        };
        taker_positions[1] = PerpPosition {
            market_index: 1,
            open_orders: 1,
            open_bids: BASE_PRECISION_I64,
            ..PerpPosition::default()
        };

        let mut taker = User {
            orders: taker_orders,
            perp_positions: taker_positions,
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 10_000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };

        // Maker has sol order and position at index 1, btc at index 1
        let maker_key = Pubkey::default();
        let maker_authority =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        let mut maker_orders = [Order::default(); 32];
        maker_orders[0] = Order {
            market_index: 1,
            post_only: true,
            order_type: OrderType::Limit,
            direction: PositionDirection::Short,
            base_asset_amount: BASE_PRECISION_U64 / 2,
            price: 20000 * PRICE_PRECISION_U64,
            ..Order::default()
        };
        maker_orders[1] = Order {
            market_index: 0,
            post_only: true,
            order_type: OrderType::Limit,
            direction: PositionDirection::Short,
            base_asset_amount: BASE_PRECISION_U64 / 2,
            price: 100 * PRICE_PRECISION_U64,
            ..Order::default()
        };

        let mut maker_positions = [PerpPosition::default(); 8];
        maker_positions[0] = PerpPosition {
            market_index: 1,
            open_orders: 1,
            open_asks: -BASE_PRECISION_I64 / 2,
            ..PerpPosition::default()
        };
        maker_positions[1] = PerpPosition {
            market_index: 0,
            open_orders: 1,
            open_asks: -BASE_PRECISION_I64 / 2,
            ..PerpPosition::default()
        };

        let mut maker = User {
            authority: maker_authority,
            orders: maker_orders,
            perp_positions: maker_positions,
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: 10_000 * SPOT_BALANCE_PRECISION_U64,
                ..SpotPosition::default()
            }),
            ..User::default()
        };
        create_anchor_account_info!(maker, User, maker_account_info);
        let makers_and_referrers = UserMap::load_one(&maker_account_info).unwrap();

        // random
        let now = 1; //80080880_i64;
        let slot = 0; //7893275_u64;

        let fee_structure = get_fee_structure();

        let (taker_key, _, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats {
            authority: maker_authority,
            ..UserStats::default()
        };
        create_anchor_account_info!(maker_stats, UserStats, maker_stats_account_info);
        let maker_and_referrer_stats = UserStatsMap::load_one(&maker_stats_account_info).unwrap();

        let taker_before = taker;
        let maker_before = maker;

        let order_index = 0;
        let min_auction_duration = 10;
        let user_can_skip_auction_duration = taker
            .can_skip_auction_duration(&taker_stats, false)
            .unwrap();
        let is_amm_available = get_amm_is_available(
            &taker.orders[order_index],
            min_auction_duration,
            &market_map.get_ref(&0).unwrap(),
            &mut oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        let (base_asset_amount, _) = fulfill_perp_order(
            &mut taker,
            order_index,
            &taker_key,
            &mut taker_stats,
            &makers_and_referrers,
            &maker_and_referrer_stats,
            &[(maker_key, 1, 100 * PRICE_PRECISION_U64)],
            &mut None,
            &filler_key,
            &mut None,
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            100 * PRICE_PRECISION_U64,
            None,
            now,
            slot,
            is_amm_available,
            true,
            FillMode::Fill,
            false,
            &mut None,
            false,
            0,
        )
        .unwrap();

        assert_eq!(base_asset_amount, BASE_PRECISION_U64 / 2);

        let taker_position = &taker.perp_positions[0].clone();
        assert_eq!(taker_position.base_asset_amount, BASE_PRECISION_I64 / 2);
        assert_eq!(taker_position.quote_asset_amount, -50025000);
        assert_eq!(taker_position.quote_entry_amount, -50 * QUOTE_PRECISION_I64);
        assert_eq!(taker_position.quote_break_even_amount, -50025000);
        assert_eq!(taker_position.open_bids, BASE_PRECISION_I64 / 2);
        assert_eq!(taker_position.open_orders, 1);
        assert_eq!(taker_stats.fees.total_fee_paid, 25000);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 50 * QUOTE_PRECISION_U64);

        let taker_order = &taker.orders[0].clone();
        assert_eq!(taker_order.base_asset_amount_filled, BASE_PRECISION_U64 / 2);
        assert_eq!(taker_order.quote_asset_amount_filled, 50000000);

        // BTC Market shouldnt be affected
        assert_eq!(taker.perp_positions[1], taker_before.perp_positions[1]);
        assert_eq!(taker.orders[1], taker_before.orders[1]);

        let maker = makers_and_referrers.get_ref_mut(&maker_key).unwrap();
        let maker_stats = maker_and_referrer_stats
            .get_ref_mut(&maker_authority)
            .unwrap();
        let maker_position = &maker.perp_positions[1];
        assert_eq!(maker_position.base_asset_amount, -BASE_PRECISION_I64 / 2);
        assert_eq!(maker_position.quote_asset_amount, 50015000);
        assert_eq!(maker_position.quote_entry_amount, 50 * QUOTE_PRECISION_I64);
        assert_eq!(maker_position.quote_break_even_amount, 50015000);
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_asks, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 15000);
        assert_eq!(maker_stats.maker_volume_30d, 50 * QUOTE_PRECISION_U64);

        assert!(maker.orders[1].is_available());

        // BTC Market shouldnt be affected
        assert_eq!(maker.perp_positions[0], maker_before.perp_positions[0]);
        assert_eq!(maker.orders[0], maker_before.orders[0]);

        let market_after = market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.amm.base_asset_amount_with_amm, 0);
        assert_eq!(market_after.base_asset_amount_long, 500000000);
        assert_eq!(market_after.base_asset_amount_short, -500000000);
        assert_eq!(market_after.quote_asset_amount, -10000);
        assert_eq!(market_after.fee_ledger.total_exchange_fee, 25000);
        assert_eq!(market_after.fee_ledger.pending_protocol_fee, 10000);
        assert_eq!(market_after.fee_ledger.pending_if_fee, 0);
        assert_eq!(market_after.amm.total_fee, 0);
        assert_eq!(market_after.amm.total_fee_minus_distributions, 0);
        assert_eq!(market_after.amm.net_revenue_since_last_funding, 0);

        assert_eq!(market_after.market_stats.last_mark_price_twap_ts, 1);
        assert_eq!(
            market_after
                .market_stats
                .historical_oracle_data
                .last_oracle_price_twap_ts,
            0
        );
        assert_eq!(market_after.market_stats.last_ask_price_twap, 50000000);
        assert_eq!(market_after.market_stats.last_bid_price_twap, 50000000);
        assert_eq!(market_after.market_stats.last_mark_price_twap, 50000000);
        assert_eq!(market_after.market_stats.last_mark_price_twap_5min, 333332);
        assert_eq!(
            market_after
                .market_stats
                .historical_oracle_data
                .last_oracle_price_twap,
            0
        );
        assert_eq!(
            market_after
                .market_stats
                .historical_oracle_data
                .last_oracle_price_twap_5min,
            0
        );
    }

    #[test]
    fn fulfill_with_amm_when_maker_is_filler() {
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

        let maker_key = Pubkey::new_unique();
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
                price: 100_010_000 * PRICE_PRECISION_U64 / 1_000_000, // .01 worse than amm
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

        let fee_structure = get_fee_structure();

        let (taker_key, _, _) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats {
            authority: maker_authority,
            ..UserStats::default()
        };
        create_anchor_account_info!(maker_stats, UserStats, maker_stats_account_info);
        let maker_and_referrer_stats = UserStatsMap::load_one(&maker_stats_account_info).unwrap();

        let order_index = 0;
        let min_auction_duration = 0;
        let user_can_skip_auction_duration = taker
            .can_skip_auction_duration(&taker_stats, false)
            .unwrap();
        let is_amm_available = get_amm_is_available(
            &taker.orders[order_index],
            min_auction_duration,
            &market_map.get_ref(&0).unwrap(),
            &mut oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        let (base_asset_amount, _) = fulfill_perp_order(
            &mut taker,
            order_index,
            &taker_key,
            &mut taker_stats,
            &makers_and_referrers,
            &maker_and_referrer_stats,
            &[(maker_key, 0, 100_010_000 * PRICE_PRECISION_U64 / 1_000_000)],
            &mut None,
            &maker_key,
            &mut None,
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            100 * PRICE_PRECISION_U64,
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            is_amm_available,
            true,
            FillMode::Fill,
            false,
            &mut None,
            false,
            0,
        )
        .unwrap();

        assert_eq!(base_asset_amount, BASE_PRECISION_U64);

        let taker_position = &taker.perp_positions[0];
        assert_eq!(taker_position.base_asset_amount, BASE_PRECISION_I64);
        assert_eq!(taker_position.quote_asset_amount, -100306387);
        assert_eq!(taker_position.quote_entry_amount, -100256258);
        assert_eq!(taker_position.quote_break_even_amount, -100306387);
        assert_eq!(taker_position.open_bids, 0);
        assert_eq!(taker_position.open_orders, 0);
        assert_eq!(taker_stats.fees.total_fee_paid, 50129);
        assert_eq!(taker_stats.fees.total_referee_discount, 0);
        assert_eq!(taker_stats.fees.total_token_discount, 0);
        assert_eq!(taker_stats.taker_volume_30d, 100256237);
        assert!(taker.orders[0].is_available());

        let maker = makers_and_referrers.get_ref_mut(&maker_key).unwrap();
        let maker_stats = maker_and_referrer_stats
            .get_ref_mut(&maker_authority)
            .unwrap();
        let maker_position = &maker.perp_positions[0];
        assert_eq!(maker_position.base_asset_amount, -BASE_PRECISION_I64 / 2);
        assert_eq!(maker_position.quote_break_even_amount, 50_020_001);
        assert_eq!(maker_position.quote_entry_amount, 50_005_000);
        assert_eq!(maker_position.quote_asset_amount, 50022513); // 50_005_000 + 50_005_000 * .0003
        assert_eq!(maker_position.open_orders, 0);
        assert_eq!(maker_position.open_asks, 0);
        assert_eq!(maker_stats.fees.total_fee_rebate, 15001);
        assert_eq!(maker_stats.maker_volume_30d, 50_005_000);
        assert_eq!(maker_stats.filler_volume_30d, 50251257); // gets filler volume
        assert!(maker.orders[0].is_available());
    }

    // `fulfill_with_amm_when_maker_is_filler` with a hard gate firing: the AMM
    // would JIT the residual, but must not. Only the DLOB maker's half fills;
    // AMM reserves untouched.
    #[test]
    fn amm_jit_suppressed_in_match_when_amm_unavailable() {
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
                base_spread: 0,
                amm_jit_intensity: 100,
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

        let maker_key = Pubkey::new_unique();
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
                price: 100_010_000 * PRICE_PRECISION_U64 / 1_000_000, // .01 worse than amm
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

        let fee_structure = get_fee_structure();
        let (taker_key, _, filler_key) = get_user_keys();

        let mut taker_stats = UserStats::default();
        let mut maker_stats = UserStats {
            authority: maker_authority,
            ..UserStats::default()
        };
        create_anchor_account_info!(maker_stats, UserStats, maker_stats_account_info);
        let maker_and_referrer_stats = UserStatsMap::load_one(&maker_stats_account_info).unwrap();

        let mut filler = User::default();
        let mut filler_stats = UserStats::default();

        let order_index = 0;

        // Hard gate firing: both standalone AMM and JIT are off.
        let amm_is_available = false;
        let amm_jit_allowed = false;

        let (base_asset_amount, _) = fulfill_perp_order(
            &mut taker,
            order_index,
            &taker_key,
            &mut taker_stats,
            &makers_and_referrers,
            &maker_and_referrer_stats,
            &[(maker_key, 0, 100_010_000 * PRICE_PRECISION_U64 / 1_000_000)],
            &mut Some(&mut filler),
            &filler_key,
            &mut Some(&mut filler_stats),
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            100 * PRICE_PRECISION_U64,
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            amm_is_available,
            amm_jit_allowed,
            FillMode::Fill,
            false,
            &mut None,
            false,
            0,
        )
        .unwrap();

        // Only the DLOB maker's half fills; the AMM does not JIT the residual.
        assert_eq!(base_asset_amount, BASE_PRECISION_U64 / 2);
        assert_eq!(
            taker.perp_positions[0].base_asset_amount,
            BASE_PRECISION_I64 / 2
        );

        let maker = makers_and_referrers.get_ref(&maker_key).unwrap();
        assert_eq!(
            maker.perp_positions[0].base_asset_amount,
            -BASE_PRECISION_I64 / 2
        );

        // The AMM curve and net position are untouched — no JIT swap happened.
        let market_after = market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.amm.base_asset_amount_with_amm, 0);
        assert_eq!(
            market_after.amm.base_asset_reserve,
            100 * AMM_RESERVE_PRECISION
        );
        assert_eq!(
            market_after.amm.quote_asset_reserve,
            100 * AMM_RESERVE_PRECISION
        );
    }

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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, slot, None).unwrap();

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
            &mut oracle_map,
            slot,
            user_can_skip_auction_duration,
        );

        assert!(!user_can_skip_auction_duration);
        assert!(!is_amm_available);

        let (base_asset_amount, _) = fulfill_perp_order(
            &mut taker,
            order_index,
            &taker_key,
            &mut taker_stats,
            &UserMap::empty(),
            &UserStatsMap::empty(),
            &[],
            &mut Some(&mut filler),
            &filler_key,
            &mut Some(&mut filler_stats),
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &crate::state::state::ValidityGuardRails::default(),
            &fee_structure,
            100 * PRICE_PRECISION_U64,
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            is_amm_available,
            true,
            FillMode::Fill,
            false,
            &mut None,
            false,
            0,
        )
        .unwrap();

        assert_eq!(base_asset_amount, 0);
        assert_eq!(taker.perp_positions[0].base_asset_amount, 0);

        let market_after = market_map.get_ref(&0).unwrap();
        assert_eq!(market_after.amm.base_asset_amount_with_amm, -1000000000);
    }
}

pub mod fill_order {
    use {
        super::*,
        crate::{
            controller::{orders::fill_perp_order, position::PositionDirection},
            create_anchor_account_info,
            error::ErrorCode,
            math::constants::{
                AMM_RESERVE_PRECISION, BASE_PRECISION_I64, BASE_PRECISION_U64, PEG_PRECISION,
                PRICE_PRECISION_I64, PRICE_PRECISION_U64, SPOT_BALANCE_PRECISION_U64,
                SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
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
                user::{MarketType, OrderStatus, OrderType, SpotPosition, User, UserStats},
                user_map::{UserMap, UserStatsMap},
            },
            test_utils::{get_orders, get_positions, get_pyth_price, get_spot_positions},
            QUOTE_PRECISION_I64,
        },
        anchor_lang::prelude::{AccountLoader, Clock},
        std::str::FromStr,
    };

    #[test]
    fn maker_order_canceled_for_breaching_oracle_price_band() {
        let clock = Clock {
            slot: 56,
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, clock.slot, None).unwrap();

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

        let mut user = User {
            authority: Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap(), // different authority than filler
            orders: get_orders(Order {
                market_index: 0,
                order_id: 1,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 0,
                auction_end_price: 50 * PRICE_PRECISION_I64,
                auction_duration: 5,
                price: 50 * PRICE_PRECISION_U64,
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

        let maker_key = Pubkey::from_str("My11111111111111111111111111111111111111113").unwrap();
        let maker_authority =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        let mut maker = User {
            authority: maker_authority,
            orders: get_orders(Order {
                market_index: 0,
                order_id: 1,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 50 * PRICE_PRECISION_U64,
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
        create_anchor_account_info!(maker, &maker_key, User, maker_account_info);
        let makers_and_referrers = UserMap::load_one(&maker_account_info).unwrap();

        let mut maker_stats = UserStats {
            authority: maker_authority,
            ..UserStats::default()
        };
        create_anchor_account_info!(maker_stats, UserStats, maker_stats_account_info);
        let maker_and_referrer_stats = UserStatsMap::load_one(&maker_stats_account_info).unwrap();

        let filler_key = Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        create_anchor_account_info!(User::default(), &filler_key, User, user_account_info);
        let filler_account_loader: AccountLoader<User> =
            AccountLoader::try_from(&user_account_info).unwrap();

        create_anchor_account_info!(UserStats::default(), UserStats, filler_stats_account_info);
        let filler_stats_account_loader: AccountLoader<UserStats> =
            AccountLoader::try_from(&filler_stats_account_info).unwrap();

        let state = State {
            min_perp_auction_duration: 1,
            default_market_order_time_in_force: 10,
            ..State::default()
        };

        let (base_asset_amount, _) = fill_perp_order(
            1,
            &state,
            &user_account_loader,
            &user_stats_account_loader,
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &filler_account_loader,
            &filler_stats_account_loader,
            &makers_and_referrers,
            &maker_and_referrer_stats,
            None,
            &clock,
            FillMode::Fill,
            &mut None,
        )
        .unwrap();

        assert_eq!(base_asset_amount, 0);

        // order canceled
        let maker = makers_and_referrers.get_ref_mut(&maker_key).unwrap();
        assert!(maker.orders[0].is_available());
    }

    #[test]
    fn fallback_maker_order_id() {
        let clock = Clock {
            slot: 56,
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, clock.slot, None).unwrap();

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

        let mut user = User {
            authority: Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap(), // different authority than filler
            orders: get_orders(Order {
                market_index: 0,
                order_id: 1,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                market_type: MarketType::Perp,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 0,
                auction_end_price: 100 * PRICE_PRECISION_I64,
                auction_duration: 5,
                price: 100 * PRICE_PRECISION_U64,
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

        let maker_key = Pubkey::from_str("My11111111111111111111111111111111111111113").unwrap();
        let maker_authority =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        let maker_order_id = 1;
        let mut maker = User {
            authority: maker_authority,
            orders: get_orders(Order {
                market_index: 0,
                order_id: maker_order_id,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                market_type: MarketType::Perp,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 100 * PRICE_PRECISION_U64,
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
        create_anchor_account_info!(maker, &maker_key, User, maker_account_info);
        let makers_and_referrers = UserMap::load_one(&maker_account_info).unwrap();

        let mut maker_stats = UserStats {
            authority: maker_authority,
            ..UserStats::default()
        };
        create_anchor_account_info!(maker_stats, UserStats, maker_stats_account_info);
        let maker_and_referrer_stats = UserStatsMap::load_one(&maker_stats_account_info).unwrap();

        let filler_key = Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        create_anchor_account_info!(User::default(), &filler_key, User, user_account_info);
        let filler_account_loader: AccountLoader<User> =
            AccountLoader::try_from(&user_account_info).unwrap();

        create_anchor_account_info!(UserStats::default(), UserStats, filler_stats_account_info);
        let filler_stats_account_loader: AccountLoader<UserStats> =
            AccountLoader::try_from(&filler_stats_account_info).unwrap();

        let state = State {
            min_perp_auction_duration: 1,
            default_market_order_time_in_force: 10,
            ..State::default()
        };

        let (base_asset_amount, _) = fill_perp_order(
            1,
            &state,
            &user_account_loader,
            &user_stats_account_loader,
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &filler_account_loader,
            &filler_stats_account_loader,
            &makers_and_referrers,
            &maker_and_referrer_stats,
            None,
            &clock,
            FillMode::Fill,
            &mut None,
        )
        .unwrap();

        assert_eq!(base_asset_amount, 1000000000);
    }

    #[test]
    fn expire_order() {
        let mut market = PerpMarket {
            amm: AMM {
                terminal_quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                max_slippage_ratio: 100,
                max_fill_reserve_fraction: 100,
                max_base_asset_reserve: 200 * AMM_RESERVE_PRECISION,
                min_base_asset_reserve: 50 * AMM_RESERVE_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            order_step_size: 10000000,
            order_tick_size: 1,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(PRICE_PRECISION_I64),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };

        market.status = MarketStatus::Active;

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

        let mut oracle_map = get_oracle_map();

        let mut user = User {
            authority: Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap(),
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
                max_ts: 10,
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
            min_perp_auction_duration: 1,
            default_market_order_time_in_force: 10,
            ..State::default()
        };

        let clock = Clock {
            slot: 11,
            epoch_start_timestamp: 0,
            epoch: 0,
            leader_schedule_epoch: 0,
            unix_timestamp: 11,
        };

        let (base_asset_amount, _) = fill_perp_order(
            1,
            &state,
            &user_account_loader,
            &user_stats_account_loader,
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &filler_account_loader,
            &filler_stats_account_loader,
            &UserMap::empty(),
            &UserStatsMap::empty(),
            None,
            &clock,
            FillMode::Fill,
            &mut None,
        )
        .unwrap();

        let user_after = user_account_loader.load().unwrap();
        assert_eq!(base_asset_amount, 0);
        assert_eq!(user_after.perp_positions[0].open_orders, 0);
        assert_eq!(user_after.perp_positions[0].open_bids, 0);
        assert_eq!(user_after.perp_positions[0].quote_asset_amount, -10000);
        assert!(user_after.orders[0].is_available()); // order canceled

        let filler_after = filler_account_loader.load().unwrap();
        assert_eq!(filler_after.perp_positions[0].quote_asset_amount, 10000);
    }

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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, clock.slot, None).unwrap();

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
            min_perp_auction_duration: 1,
            default_market_order_time_in_force: 10,
            ..State::default()
        };

        let err = fill_perp_order(
            1,
            &state,
            &user_account_loader,
            &user_stats_account_loader,
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &filler_account_loader,
            &filler_stats_account_loader,
            &UserMap::empty(),
            &UserStatsMap::empty(),
            None,
            &clock,
            FillMode::Fill,
            &mut None,
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
            math::constants::{
                AMM_RESERVE_PRECISION, BASE_PRECISION_I64, BASE_PRECISION_U64,
                LAMPORTS_PER_SOL_I64, LAMPORTS_PER_SOL_U64, PEG_PRECISION, PRICE_PRECISION_U64,
                SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, clock.slot, None).unwrap();

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
            min_perp_auction_duration: 1,
            default_market_order_time_in_force: 10,
            ..State::default()
        };

        force_cancel_orders(
            &state,
            &user_account_loader,
            &spot_market_map,
            &market_map,
            &mut oracle_map,
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
            math::constants::{
                AMM_RESERVE_PRECISION, BASE_PRECISION_I64, LAMPORTS_PER_SOL_I64, PEG_PRECISION,
                SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
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
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, clock.slot, None).unwrap();

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
            &market_map,
            &spot_market_map,
            &mut oracle_map,
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

pub mod insert_maker_order_info {
    use {
        crate::controller::{orders::insert_maker_order_info, position::PositionDirection},
        solana_program::pubkey::Pubkey,
    };

    #[test]
    fn bids() {
        let mut bids = Vec::with_capacity(3);
        bids.push((Pubkey::default(), 1, 10));
        bids.push((Pubkey::default(), 0, 1));
        let maker_direction = PositionDirection::Long;

        insert_maker_order_info(&mut bids, (Pubkey::default(), 2, 100), maker_direction);

        assert_eq!(
            bids,
            vec![
                (Pubkey::default(), 2, 100),
                (Pubkey::default(), 1, 10),
                (Pubkey::default(), 0, 1),
            ]
        );
    }

    #[test]
    fn asks() {
        let mut asks = Vec::with_capacity(3);
        asks.push((Pubkey::default(), 0, 1));
        asks.push((Pubkey::default(), 1, 10));
        let maker_direction = PositionDirection::Short;

        insert_maker_order_info(&mut asks, (Pubkey::default(), 2, 100), maker_direction);

        assert_eq!(
            asks,
            vec![
                (Pubkey::default(), 0, 1),
                (Pubkey::default(), 1, 10),
                (Pubkey::default(), 2, 100)
            ]
        );
    }
}

pub mod get_maker_orders_info {
    use {
        super::*,
        crate::{
            controller::{orders::get_maker_orders_info, position::PositionDirection},
            create_anchor_account_info, get_orders,
            math::constants::{
                AMM_RESERVE_PRECISION, BASE_PRECISION_I64, BASE_PRECISION_U64, PEG_PRECISION,
                PRICE_PRECISION_I64, PRICE_PRECISION_U64, SPOT_BALANCE_PRECISION_U64,
                SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
            },
            state::{
                oracle::{HistoricalOracleData, OracleSource},
                perp_market::{MarketStats, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                user::{OrderStatus, OrderType, SpotPosition, User},
                user_map::UserMap,
            },
            test_utils::{get_orders, get_positions, get_pyth_price, get_spot_positions},
            QUOTE_PRECISION_I64,
        },
        anchor_lang::prelude::{AccountLoader, Clock},
        std::str::FromStr,
    };

    #[test]
    fn one_maker_order_canceled_for_breaching_oracle_price_band() {
        let clock = Clock {
            slot: 56,
            epoch_start_timestamp: 0,
            epoch: 0,
            leader_schedule_epoch: 0,
            unix_timestamp: 0,
        };

        let mut pyth_price = get_pyth_price(100, 6);
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            pyth_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, clock.slot, None).unwrap();

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
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: pyth_price.price,
                    last_oracle_price_twap_5min: pyth_price.price,
                    last_oracle_price: pyth_price.price,
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

        let taker_key = Pubkey::default();
        let taker_authority =
            Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        let user = User {
            authority: taker_authority,
            orders: get_orders(Order {
                market_index: 0,
                order_id: 1,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 0,
                auction_end_price: 50 * PRICE_PRECISION_I64,
                auction_duration: 5,
                price: 50 * PRICE_PRECISION_U64,
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

        let mut maker_orders = [Order::default(); 32];
        maker_orders[0] = Order {
            market_index: 0,
            order_id: 1,
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            direction: PositionDirection::Short,
            base_asset_amount: BASE_PRECISION_U64,
            slot: 0,
            price: 50 * PRICE_PRECISION_U64,
            post_only: true,
            ..Order::default()
        };
        maker_orders[1] = Order {
            market_index: 0,
            order_id: 2,
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            direction: PositionDirection::Short,
            base_asset_amount: BASE_PRECISION_U64,
            slot: 0,
            price: 100 * PRICE_PRECISION_U64,
            post_only: true,
            ..Order::default()
        };

        let mut maker = User {
            orders: maker_orders,
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 2,
                open_asks: -2 * BASE_PRECISION_I64,
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
        create_anchor_account_info!(maker, &maker_key, User, maker_account_info);

        let makers_and_referrers = UserMap::load_one(&maker_account_info).unwrap();

        let filler_key = Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        let mut filler = User::default();

        let maker_order_price_and_indexes = get_maker_orders_info(
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            &makers_and_referrers,
            &taker_key,
            &user.orders[0],
            &mut Some(&mut filler),
            &filler_key,
            0,
            oracle_price,
            None,
            clock.unix_timestamp,
            clock.slot,
        )
        .unwrap();

        assert_eq!(
            maker_order_price_and_indexes,
            vec![(maker_key, 1, 100 * PRICE_PRECISION_U64)]
        );
    }

    #[test]
    fn one_maker_order_canceled_for_being_expired() {
        let clock = Clock {
            slot: 56,
            epoch_start_timestamp: 0,
            epoch: 0,
            leader_schedule_epoch: 0,
            unix_timestamp: 6,
        };

        let mut pyth_price = get_pyth_price(100, 6);
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            pyth_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, clock.slot, None).unwrap();

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
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: pyth_price.price,
                    last_oracle_price_twap_5min: pyth_price.price,
                    last_oracle_price: pyth_price.price,
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

        let taker_key = Pubkey::default();
        let taker_authority =
            Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        let user = User {
            authority: taker_authority,
            orders: get_orders(Order {
                market_index: 0,
                order_id: 1,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 0,
                auction_end_price: 50 * PRICE_PRECISION_I64,
                auction_duration: 5,
                price: 100 * PRICE_PRECISION_U64,
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

        let mut maker_orders = [Order::default(); 32];
        maker_orders[0] = Order {
            market_index: 0,
            order_id: 1,
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            direction: PositionDirection::Short,
            base_asset_amount: BASE_PRECISION_U64,
            slot: 0,
            price: 100 * PRICE_PRECISION_U64,
            max_ts: 1,
            post_only: true,
            ..Order::default()
        };
        maker_orders[1] = Order {
            market_index: 0,
            order_id: 2,
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            direction: PositionDirection::Short,
            base_asset_amount: BASE_PRECISION_U64,
            slot: 0,
            price: 100 * PRICE_PRECISION_U64,
            post_only: true,
            ..Order::default()
        };

        let mut maker = User {
            orders: maker_orders,
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 2,
                open_asks: -2 * BASE_PRECISION_I64,
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
        create_anchor_account_info!(maker, &maker_key, User, maker_account_info);

        let makers_and_referrers = UserMap::load_one(&maker_account_info).unwrap();

        let filler_key = Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        let mut filler = User::default();

        let maker_order_price_and_indexes = get_maker_orders_info(
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            &makers_and_referrers,
            &taker_key,
            &user.orders[0],
            &mut Some(&mut filler),
            &filler_key,
            0,
            oracle_price,
            None,
            clock.unix_timestamp,
            clock.slot,
        )
        .unwrap();

        assert_eq!(
            maker_order_price_and_indexes,
            vec![(maker_key, 1, 100 * PRICE_PRECISION_U64)]
        );
    }

    #[test]
    fn one_maker_order_canceled_for_being_reduce_only() {
        let clock = Clock {
            slot: 6,
            epoch_start_timestamp: 0,
            epoch: 0,
            leader_schedule_epoch: 0,
            unix_timestamp: 0,
        };

        let mut pyth_price = get_pyth_price(100, 6);
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            pyth_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, clock.slot, None).unwrap();

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
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: pyth_price.price,
                    last_oracle_price_twap_5min: pyth_price.price,
                    last_oracle_price: pyth_price.price,
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

        let taker_key = Pubkey::default();
        let taker_authority =
            Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        let user = User {
            authority: taker_authority,
            orders: get_orders(Order {
                market_index: 0,
                order_id: 1,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 0,
                auction_end_price: 50 * PRICE_PRECISION_I64,
                auction_duration: 5,
                price: 100 * PRICE_PRECISION_U64,
                max_ts: 1,
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

        let mut maker_orders = [Order::default(); 32];
        maker_orders[0] = Order {
            market_index: 0,
            order_id: 1,
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            direction: PositionDirection::Short,
            base_asset_amount: BASE_PRECISION_U64,
            slot: 0,
            price: 100 * PRICE_PRECISION_U64,
            reduce_only: true,
            ..Order::default()
        };

        let mut maker = User {
            orders: maker_orders,
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: -BASE_PRECISION_I64,
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
        let maker_key = Pubkey::from_str("My11111111111111111111111111111111111111113").unwrap();
        create_anchor_account_info!(maker, &maker_key, User, maker_account_info);

        let makers_and_referrers = UserMap::load_one(&maker_account_info).unwrap();

        let filler_key = Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        let mut filler = User::default();

        let maker_order_price_and_indexes = get_maker_orders_info(
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            &makers_and_referrers,
            &taker_key,
            &user.orders[0],
            &mut Some(&mut filler),
            &filler_key,
            0,
            oracle_price,
            None,
            clock.unix_timestamp,
            clock.slot,
        )
        .unwrap();

        assert_eq!(maker_order_price_and_indexes, vec![],);
    }

    #[test]
    fn two_makers() {
        let clock = Clock {
            slot: 6,
            epoch_start_timestamp: 0,
            epoch: 0,
            leader_schedule_epoch: 0,
            unix_timestamp: 0,
        };

        let mut pyth_price = get_pyth_price(100, 6);
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            pyth_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, clock.slot, None).unwrap();

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
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: pyth_price.price,
                    last_oracle_price_twap_5min: pyth_price.price,
                    last_oracle_price: pyth_price.price,
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

        let taker_key = Pubkey::default();
        let taker_authority =
            Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        let user = User {
            authority: taker_authority,
            orders: get_orders(Order {
                market_index: 0,
                order_id: 1,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 0,
                auction_end_price: 50 * PRICE_PRECISION_I64,
                auction_duration: 5,
                price: 100 * PRICE_PRECISION_U64,
                max_ts: 1,
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

        let mut first_maker = User {
            orders: get_orders!(
                Order {
                    market_index: 0,
                    order_id: 1,
                    status: OrderStatus::Open,
                    order_type: OrderType::Limit,
                    direction: PositionDirection::Short,
                    base_asset_amount: BASE_PRECISION_U64,
                    slot: 0,
                    price: 100 * PRICE_PRECISION_U64,
                    ..Order::default()
                },
                Order {
                    market_index: 0,
                    order_id: 1,
                    status: OrderStatus::Open,
                    order_type: OrderType::Limit,
                    direction: PositionDirection::Short,
                    base_asset_amount: BASE_PRECISION_U64,
                    slot: 0,
                    price: 102 * PRICE_PRECISION_U64,
                    ..Order::default()
                }
            ),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 2,
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
        let first_maker_key =
            Pubkey::from_str("My11111111111111111111111111111111111111113").unwrap();
        create_anchor_account_info!(
            first_maker,
            &first_maker_key,
            User,
            first_maker_account_info
        );

        let mut second_maker = User {
            orders: get_orders!(
                Order {
                    market_index: 0,
                    order_id: 1,
                    status: OrderStatus::Open,
                    order_type: OrderType::Limit,
                    direction: PositionDirection::Short,
                    base_asset_amount: BASE_PRECISION_U64,
                    slot: 0,
                    price: 101 * PRICE_PRECISION_U64,
                    ..Order::default()
                },
                Order {
                    market_index: 0,
                    order_id: 1,
                    status: OrderStatus::Open,
                    order_type: OrderType::Limit,
                    direction: PositionDirection::Short,
                    base_asset_amount: BASE_PRECISION_U64,
                    slot: 0,
                    price: 103 * PRICE_PRECISION_U64,
                    ..Order::default()
                }
            ),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 2,
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
        let second_maker_key =
            Pubkey::from_str("My11111111111111111111111111111111111111112").unwrap();
        create_anchor_account_info!(
            second_maker,
            &second_maker_key,
            User,
            second_maker_account_info
        );

        let mut makers_and_referrers = UserMap::load_one(&first_maker_account_info).unwrap();
        makers_and_referrers
            .insert(
                second_maker_key,
                AccountLoader::try_from(&second_maker_account_info).unwrap(),
            )
            .unwrap();

        let filler_key = Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        let mut filler = User::default();

        let maker_order_price_and_indexes = get_maker_orders_info(
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            &makers_and_referrers,
            &taker_key,
            &user.orders[0],
            &mut Some(&mut filler),
            &filler_key,
            0,
            oracle_price,
            None,
            clock.unix_timestamp,
            clock.slot,
        )
        .unwrap();

        assert_eq!(
            maker_order_price_and_indexes,
            vec![
                (first_maker_key, 0, 100000000),
                (second_maker_key, 0, 101000000),
                (first_maker_key, 1, 102000000),
                (second_maker_key, 1, 103000000),
            ],
        );
    }

    #[test]
    fn jit_maker_order_id() {
        let clock = Clock {
            slot: 6,
            epoch_start_timestamp: 0,
            epoch: 0,
            leader_schedule_epoch: 0,
            unix_timestamp: 0,
        };

        let mut pyth_price = get_pyth_price(100, 6);
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            pyth_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, clock.slot, None).unwrap();

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
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: pyth_price.price,
                    last_oracle_price_twap_5min: pyth_price.price,
                    last_oracle_price: pyth_price.price,
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

        let taker_key = Pubkey::default();
        let taker_authority =
            Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        let user = User {
            authority: taker_authority,
            orders: get_orders(Order {
                market_index: 0,
                order_id: 1,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 0,
                auction_end_price: 50 * PRICE_PRECISION_I64,
                auction_duration: 5,
                price: 100 * PRICE_PRECISION_U64,
                max_ts: 1,
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

        let mut first_maker = User {
            orders: get_orders!(
                Order {
                    market_index: 0,
                    order_id: 1,
                    status: OrderStatus::Open,
                    order_type: OrderType::Limit,
                    direction: PositionDirection::Short,
                    base_asset_amount: BASE_PRECISION_U64,
                    slot: 0,
                    price: 100 * PRICE_PRECISION_U64,
                    ..Order::default()
                },
                Order {
                    market_index: 0,
                    order_id: 2,
                    status: OrderStatus::Open,
                    order_type: OrderType::Limit,
                    direction: PositionDirection::Short,
                    base_asset_amount: BASE_PRECISION_U64,
                    slot: 0,
                    price: 102 * PRICE_PRECISION_U64,
                    ..Order::default()
                }
            ),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 2,
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
        let first_maker_key =
            Pubkey::from_str("My11111111111111111111111111111111111111113").unwrap();
        create_anchor_account_info!(
            first_maker,
            &first_maker_key,
            User,
            first_maker_account_info
        );

        let makers_and_referrers = UserMap::load_one(&first_maker_account_info).unwrap();

        let filler_key = Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        let mut filler = User::default();

        let maker_order_price_and_indexes = get_maker_orders_info(
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            &makers_and_referrers,
            &taker_key,
            &user.orders[0],
            &mut Some(&mut filler),
            &filler_key,
            0,
            oracle_price,
            Some(2),
            clock.unix_timestamp,
            clock.slot,
        )
        .unwrap();

        assert_eq!(
            maker_order_price_and_indexes,
            vec![(first_maker_key, 1, 102000000),],
        );
    }

    #[test]
    fn two_makers_with_max_orders() {
        let clock = Clock {
            slot: 6,
            epoch_start_timestamp: 0,
            epoch: 0,
            leader_schedule_epoch: 0,
            unix_timestamp: 0,
        };

        let mut pyth_price = get_pyth_price(100, 6);
        let oracle_price = 100 * PRICE_PRECISION_I64;
        let oracle_price_key =
            Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(
            pyth_price,
            &oracle_price_key,
            PythLazerOracle,
            oracle_account_info
        );
        let mut oracle_map = OracleMap::load_one(&oracle_account_info, clock.slot, None).unwrap();

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
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: pyth_price.price,
                    last_oracle_price_twap_5min: pyth_price.price,
                    last_oracle_price: pyth_price.price,
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

        let taker_key = Pubkey::default();
        let taker_authority =
            Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        let user = User {
            authority: taker_authority,
            orders: get_orders(Order {
                market_index: 0,
                order_id: 1,
                status: OrderStatus::Open,
                order_type: OrderType::Market,
                direction: PositionDirection::Long,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                auction_start_price: 0,
                auction_end_price: 50 * PRICE_PRECISION_I64,
                auction_duration: 5,
                price: 100 * PRICE_PRECISION_U64,
                max_ts: 1,
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

        let mut first_maker = User {
            orders: [Order {
                market_index: 0,
                order_id: 1,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 100 * PRICE_PRECISION_U64,
                ..Order::default()
            }; 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 2,
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
        let first_maker_key =
            Pubkey::from_str("My11111111111111111111111111111111111111113").unwrap();
        create_anchor_account_info!(
            first_maker,
            &first_maker_key,
            User,
            first_maker_account_info
        );

        let mut second_maker = User {
            orders: [Order {
                market_index: 0,
                order_id: 1,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                direction: PositionDirection::Short,
                base_asset_amount: BASE_PRECISION_U64,
                slot: 0,
                price: 101 * PRICE_PRECISION_U64,
                ..Order::default()
            }; 32],
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                open_orders: 2,
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
        let second_maker_key =
            Pubkey::from_str("My11111111111111111111111111111111111111112").unwrap();
        create_anchor_account_info!(
            second_maker,
            &second_maker_key,
            User,
            second_maker_account_info
        );

        let mut makers_and_referrers = UserMap::load_one(&first_maker_account_info).unwrap();
        makers_and_referrers
            .insert(
                second_maker_key,
                AccountLoader::try_from(&second_maker_account_info).unwrap(),
            )
            .unwrap();

        let filler_key = Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
        let mut filler = User::default();

        let maker_order_price_and_indexes = get_maker_orders_info(
            &market_map,
            &spot_market_map,
            &mut oracle_map,
            &makers_and_referrers,
            &taker_key,
            &user.orders[0],
            &mut Some(&mut filler),
            &filler_key,
            0,
            oracle_price,
            None,
            clock.unix_timestamp,
            clock.slot,
        )
        .unwrap();

        assert_eq!(maker_order_price_and_indexes.len(), 64);
    }
}

pub mod update_trigger_order_params {
    use crate::{
        controller::orders::update_trigger_order_params,
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
        let (start, end, duration) = get_auction_params(&params, &oracle(), 1, 10).unwrap();

        assert_eq!(start, 99_700_000);
        assert_eq!(end, 100_300_000);
        // The client asked for 5 slots and the sanitizer preserved it, but the
        // placed order is floored to the mainnet minimum of 10.
        assert_eq!(duration, 10);
        assert_ne!(duration, params.auction_duration.unwrap());
    }

    #[test]
    fn preserves_client_duration_only_when_floor_is_low_enough() {
        let params = aggressive_5_slot_market_order();

        // Lowering state.min_perp_auction_duration to <= the client's choice is
        // what actually lets a 5-slot auction survive end-to-end.
        let (_, _, duration) = get_auction_params(&params, &oracle(), 1, 5).unwrap();
        assert_eq!(duration, 5);

        let (_, _, duration) = get_auction_params(&params, &oracle(), 1, 3).unwrap();
        assert_eq!(duration, 5);
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
            slots_before_stale_for_amm: 10,
            slots_before_stale_for_margin: 120,
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
        .get_mm_oracle_price_data(oracle_price_data, slot, &guard_rails.validity)
        .unwrap();
    let validity = crate::vlp::amm::refresh::compute_amm_refresh_validity_with_guard_rails(
        &market,
        &mm_oracle_price_data,
        &guard_rails.validity,
    )
    .unwrap();

    market
        .update_oracle_derived_stats(&mm_oracle_price_data, validity, now, slot)
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

/// The margin gate on the builder fee.
///
/// A builder fee is an additive debit on the taker that the builder later
/// claims into its own account, and the taker is the party that approves the
/// builder. The fee is therefore a transfer out of the taker's account, so
/// `fulfill_perp_order` charges it only when the taker meets initial margin —
/// the gate a withdrawal clears. A position-decreasing fill is otherwise
/// checked against maintenance margin alone, which lets an under-margined
/// taker reduce the position in slices and route out value that the
/// initial-margin gate holds in the account (OtterSec #83).
pub mod builder_fee_margin_gate {
    use {
        super::*,
        crate::{
            controller::{orders::fulfill_perp_order, position::PositionDirection},
            create_anchor_account_info,
            math::constants::{
                AMM_RESERVE_PRECISION, BASE_PRECISION_I64, BASE_PRECISION_U64,
                MAX_CONCENTRATION_COEFFICIENT, PEG_PRECISION, PRICE_PRECISION, PRICE_PRECISION_I64,
                PRICE_PRECISION_U64, QUOTE_PRECISION_I64, SPOT_BALANCE_PRECISION_U64,
                SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
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
    /// liability that the fill does not touch. `sol_oracle_conf` is that
    /// market's oracle confidence interval, which decides whether the oracle
    /// on that liability is valid.
    fn run_reducing_builder_fill_with_borrow(
        collateral_dollars: u64,
        sol_borrow_hundredths: u64,
        sol_oracle_conf: u64,
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
        sol_oracle_price.conf = sol_oracle_conf;
        let sol_oracle_price_key =
            Pubkey::from_str("Gnt27xtC473ZT2Mw5u8wZ68Z3gULkSTb5DuxJy7eJotD").unwrap();
        create_anchor_account_info!(
            sol_oracle_price,
            &sol_oracle_price_key,
            PythLazerOracle,
            sol_oracle_account_info
        );

        let oracle_account_infos = Vec::from([oracle_account_info, sol_oracle_account_info]);
        let mut oracle_map =
            OracleMap::load(&mut oracle_account_infos.iter().peekable(), slot, None).unwrap();

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
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -ENTRY_PRICE * QUOTE_PRECISION_I64,
                quote_entry_amount: -ENTRY_PRICE * QUOTE_PRECISION_I64,
                quote_break_even_amount: -ENTRY_PRICE * QUOTE_PRECISION_I64,
                open_orders: 1,
                open_asks: -BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
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
            &mut oracle_map,
            slot,
            user_can_skip_auction_duration,
        );
        assert!(is_amm_available);

        let (base_filled, _) = fulfill_perp_order(
            &mut taker,
            order_index,
            &taker_key,
            &mut taker_stats,
            &UserMap::empty(),
            &UserStatsMap::empty(),
            &[],
            &mut Some(&mut filler),
            &filler_key,
            &mut Some(&mut filler_stats),
            &spot_market_map,
            &market_map,
            &mut oracle_map,
            &ValidityGuardRails::default(),
            &fee_structure,
            ENTRY_PRICE as u64 * PRICE_PRECISION_U64,
            Some(market.market_stats.historical_oracle_data.last_oracle_price),
            now,
            slot,
            is_amm_available,
            true,
            FillMode::Fill,
            false,
            &mut Some(&mut escrow),
            false,
            0,
        )
        .unwrap();

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
        // of collateral. The taker still clears initial margin, and the borrow
        // oracle is precise, so the fee is charged. Control for the invalid
        // oracle case below.
        let (base_filled, fees_accrued) = run_reducing_builder_fill_with_borrow(50, 25, 0);

        assert_eq!(base_filled, BASE_PRECISION_U64);
        assert!(
            fees_accrued > 900_000 && fees_accrued < 1_100_000,
            "expected about 1% of notional, got {fees_accrued}"
        );
    }

    #[test]
    fn waives_builder_fee_when_a_liability_oracle_is_invalid() {
        // The same taker, and the same margin state, but the borrow's oracle
        // is too uncertain to price the liability. The reduction still fills
        // and the fee is waived.
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
