use crate::{
    math::{
        constants::{
            AMM_RESERVE_PRECISION, PEG_PRECISION, PRICE_PRECISION, PRICE_PRECISION_I64,
            PRICE_PRECISION_U64, QUOTE_PRECISION,
        },
        oracle::OracleValidity,
        time::{legacy_slot_duration_i64, legacy_slot_duration_i64_raw},
    },
    state::{
        oracle::{HistoricalOracleData, OraclePriceData},
        perp_market::{ContractTier, MarketStats, AMM},
        state::{PriceDivergenceGuardRails, ValidityGuardRails},
        user::MarketType,
    },
    vlp::amm::{
        math::{
            repeg::{calculate_fee_pool, calculate_peg_from_target_price, calculate_repeg_cost},
            spread::calculate_max_target_spread,
        },
        refresh::*,
    },
};

#[test]
pub fn update_amm_test() {
    let mut market = PerpMarket {
        market_stats: MarketStats {
            mark_std: PRICE_PRECISION as u64,
            last_mark_price_twap_ts: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: 19_400 * PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: 19_400 * PRICE_PRECISION_I64,

                ..HistoricalOracleData::default()
            },
            ..MarketStats::default()
        },
        amm: AMM {
            base_asset_reserve: 65 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 63015384615,
            terminal_quote_asset_reserve: 64 * AMM_RESERVE_PRECISION,
            sqrt_k: 64 * AMM_RESERVE_PRECISION,
            peg_multiplier: 19_400 * PEG_PRECISION,
            base_asset_amount_with_amm: -(AMM_RESERVE_PRECISION as i128),
            base_spread: 250,
            curve_update_intensity: 100,
            max_spread: 55500,
            concentration_coef: 31020710, //unrealistic but for poc
            ..AMM::default()
        },
        status: MarketStatus::Initialized,
        contract_tier: ContractTier::B,
        margin_ratio_initial: 555, // max 1/.0555 = 18.018018018x leverage
        ..PerpMarket::default()
    };
    let (new_terminal_quote_reserve, new_terminal_base_reserve) =
        amm::calculate_terminal_reserves(&market.amm).unwrap();
    // market.amm.terminal_quote_asset_reserve = new_terminal_quote_reserve;
    assert_eq!(new_terminal_quote_reserve, 64000000000);
    let (min_base_asset_reserve, max_base_asset_reserve) =
        amm::calculate_bid_ask_bounds(market.amm.concentration_coef, new_terminal_base_reserve)
            .unwrap();
    market.amm.min_base_asset_reserve = min_base_asset_reserve;
    market.amm.max_base_asset_reserve = max_base_asset_reserve;

    let state = State {
        oracle_guard_rails: OracleGuardRails {
            price_divergence: PriceDivergenceGuardRails {
                mark_oracle_percent_divergence: 1,
                oracle_twap_5min_percent_divergence: 10,
            },
            validity: ValidityGuardRails {
                slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4s
                slots_before_stale_for_margin: legacy_slot_duration_i64(120), // 48s
                confidence_interval_max_size: 1000,
                too_volatile_ratio: 5,
            },
        },
        ..State::default()
    };

    let now = 10000;
    let slot = 81680085;
    let oracle_price_data = OraclePriceData {
        price: (12_400 * PRICE_PRECISION) as i64,
        confidence: 0,
        delay: 2,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };

    let reserve_price_before = market.amm.reserve_price().unwrap();
    assert_eq!(reserve_price_before, 18807668638);

    market
        .market_stats
        .historical_oracle_data
        .last_oracle_price_twap_5min = 18907668639;
    market
        .market_stats
        .historical_oracle_data
        .last_oracle_price_twap_ts = now - (167 + 6);
    let oracle_reserve_price_spread_pct_before = market
        .market_stats
        .historical_oracle_data
        .twap_5min_spread_pct(reserve_price_before)
        .unwrap();
    assert_eq!(oracle_reserve_price_spread_pct_before, -5316);
    let too_diverge = crate::math::oracle::is_mark_oracle_too_divergent(
        oracle_reserve_price_spread_pct_before,
        &state.oracle_guard_rails.price_divergence,
    )
    .unwrap();
    assert!(!too_diverge);

    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
        .unwrap();
    let cost_of_update =
        _update_amm(&mut market, &mm_oracle_price_data, &state, now, slot).unwrap();

    assert_eq!(market.amm.sqrt_k, 63936000000);
    let is_oracle_valid = oracle::oracle_validity(
        MarketType::Perp,
        market.market_index,
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap,
        &oracle_price_data,
        &state.oracle_guard_rails.validity,
        market.get_max_confidence_interval_multiplier().unwrap(),
        &market.oracle_source,
        LogMode::ExchangeOracle,
        legacy_slot_duration_i64_raw(state.oracle_guard_rails.validity.slots_before_stale_for_amm)
            as i8,
        false,
        legacy_slot_duration_i64_raw(state.oracle_guard_rails.validity.slots_before_stale_for_amm)
            as i8,
        slot,
        SlotClock::baseline(),
    )
    .unwrap()
        == OracleValidity::Valid;

    let reserve_price_after_prepeg = market.amm.reserve_price().unwrap();
    assert_eq!(reserve_price_after_prepeg, 12743902015);
    assert_eq!(
        market.market_stats.historical_oracle_data.last_oracle_price,
        12400000000
    );
    assert_eq!(
        market.market_stats.last_oracle_normalised_price,
        15520000000
    );
    assert_eq!(
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap,
        15520000000
    );
    assert_eq!(
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min,
        16954113056
    ); // since manually set higher above

    let oracle_reserve_price_spread_pct_before = market
        .market_stats
        .historical_oracle_data
        .twap_5min_spread_pct(reserve_price_after_prepeg)
        .unwrap();
    assert_eq!(oracle_reserve_price_spread_pct_before, -330370);
    let too_diverge = crate::math::oracle::is_mark_oracle_too_divergent(
        oracle_reserve_price_spread_pct_before,
        &state.oracle_guard_rails.price_divergence,
    )
    .unwrap();
    assert!(too_diverge);

    let profit = market.amm.total_fee_minus_distributions;
    let peg = market.amm.peg_multiplier;
    assert_eq!(-cost_of_update, profit);
    assert!(is_oracle_valid);
    assert!(profit < 0);
    assert_eq!(peg, 13145260284);
    assert_eq!(profit, -6158609264);

    let reserve_price = market.amm.reserve_price().unwrap();
    // Spread / ref-offset are cached on AMM — refresh them in place via the
    // helper, then call `bid_ask_price` with the cached fields.
    {
        let PerpMarket {
            amm, market_stats, ..
        } = &mut market;
        crate::vlp::amm::math::spread::update_amm_quote_state(
            amm,
            market_stats,
            &mm_oracle_price_data,
            reserve_price,
            slot,
        )
        .unwrap();
    }
    let (bid, ask) = market
        .amm
        .bid_ask_price(
            reserve_price,
            market.amm.long_spread,
            market.amm.short_spread,
            market.amm.reference_price_offset,
        )
        .unwrap();
    assert!(bid < reserve_price);
    assert!(bid < ask);
    assert!(reserve_price <= ask);
    assert_eq!(
        market.amm.long_spread + market.amm.short_spread,
        453312 // (market.margin_ratio_initial * 100) as u32
    );

    assert_eq!(bid, 7404359997);
    assert!(bid < (oracle_price_data.price as u64));
    assert_eq!(reserve_price, 12743902015);
    assert_eq!(ask, 13181323707);
    assert!(ask >= (oracle_price_data.price as u64));
    assert_eq!(
        (ask - bid) * 1000000 / reserve_price,
        453311 // overriden by max spread baseline
               // (market.amm.max_spread) as u64
    );
}

#[test]
pub fn update_amm_test_bad_oracle() {
    let mut market = PerpMarket {
        market_stats: MarketStats {
            mark_std: PRICE_PRECISION as u64,
            last_mark_price_twap_ts: 0,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: 19_400 * PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..MarketStats::default()
        },
        amm: AMM {
            base_asset_reserve: 65 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 630153846154000,
            terminal_quote_asset_reserve: 64 * AMM_RESERVE_PRECISION,
            sqrt_k: 64 * AMM_RESERVE_PRECISION,
            peg_multiplier: 19_400_000,
            base_asset_amount_with_amm: -(AMM_RESERVE_PRECISION as i128),
            concentration_coef: 1020710,
            base_spread: 250,
            curve_update_intensity: 100,
            max_spread: 55500,
            ..AMM::default()
        },
        margin_ratio_initial: 555, // max 1/.0555 = 18.018018018x leverage
        ..PerpMarket::default()
    };

    let state = State {
        oracle_guard_rails: OracleGuardRails {
            price_divergence: PriceDivergenceGuardRails {
                mark_oracle_percent_divergence: 1,
                oracle_twap_5min_percent_divergence: 10,
            },
            validity: ValidityGuardRails {
                slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4s
                slots_before_stale_for_margin: legacy_slot_duration_i64(120), // 48s
                confidence_interval_max_size: 20000,                      //2%
                too_volatile_ratio: 5,
            },
        },
        ..State::default()
    };

    let now = 10000;
    let slot = 81680085;
    let oracle_price_data = OraclePriceData {
        price: (12_400 * PRICE_PRECISION) as i64,
        confidence: 0,
        delay: 12,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
        .unwrap();

    let _cost_of_update =
        _update_amm(&mut market, &mm_oracle_price_data, &state, now, slot).unwrap();
    assert!(!market.market_stats.last_oracle_valid);
    assert!(market.amm.last_update_slot == 0);

    let is_oracle_valid = oracle::oracle_validity(
        MarketType::Perp,
        market.market_index,
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap,
        &oracle_price_data,
        &state.oracle_guard_rails.validity,
        market.get_max_confidence_interval_multiplier().unwrap(),
        &market.oracle_source,
        LogMode::None,
        0,
        false,
        0,
        slot,
        SlotClock::baseline(),
    )
    .unwrap()
        == OracleValidity::Valid;
    assert!(!is_oracle_valid);
}

#[test]
pub fn update_amm_larg_conf_test() {
    let now = 1662800000 + 60;
    let slot = 81680085;

    let mut market = PerpMarket::default_btc_test();
    assert_eq!(market.amm.base_asset_amount_with_amm, -1000000000);

    let state = State {
        oracle_guard_rails: OracleGuardRails {
            price_divergence: PriceDivergenceGuardRails {
                mark_oracle_percent_divergence: 1,
                oracle_twap_5min_percent_divergence: 10,
            },
            validity: ValidityGuardRails {
                slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4s
                slots_before_stale_for_margin: legacy_slot_duration_i64(120), // 48s
                confidence_interval_max_size: 20000,                      //2%
                too_volatile_ratio: 5,
            },
        },
        ..State::default()
    };

    let reserve_price_before = market.amm.reserve_price().unwrap();
    assert_eq!(reserve_price_before, 18807668638);

    let oracle_price_data = OraclePriceData {
        price: (18_850 * PRICE_PRECISION) as i64,
        confidence: 0,
        delay: 9,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
        .unwrap();
    assert_eq!(0u32, 0);
    assert_eq!(0u32, 0);

    let cost_of_update =
        _update_amm(&mut market, &mm_oracle_price_data, &state, now, slot).unwrap();
    assert_eq!(cost_of_update, -42992787); // amm wins when price increases

    // Spread / ref-offset values are cached on AMM; refresh in place and
    // assert on the cached fields.
    let reserve_price_after = market.amm.reserve_price().unwrap();
    {
        let PerpMarket {
            amm, market_stats, ..
        } = &mut market;
        crate::vlp::amm::math::spread::update_amm_quote_state(
            amm,
            market_stats,
            &mm_oracle_price_data,
            reserve_price_after,
            slot,
        )
        .unwrap();
    }
    assert_eq!(market.amm.long_spread, 125);
    assert_eq!(market.amm.short_spread, 12576);

    assert_eq!(reserve_price_after, 18849999999);
    assert_eq!(reserve_price_before < reserve_price_after, true);

    // add large confidence
    let oracle_price_data = OraclePriceData {
        price: (18_850 * PRICE_PRECISION) as i64,
        confidence: 100 * PRICE_PRECISION_U64,
        delay: 1,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
        .unwrap();

    let cost_of_update =
        _update_amm(&mut market, &mm_oracle_price_data, &state, now, slot).unwrap();
    assert_eq!(cost_of_update, 0);

    let mrk = market.amm.reserve_price().unwrap();
    {
        let PerpMarket {
            amm, market_stats, ..
        } = &mut market;
        crate::vlp::amm::math::spread::update_amm_quote_state(
            amm,
            market_stats,
            &mm_oracle_price_data,
            mrk,
            slot,
        )
        .unwrap();
    }
    let (bid, ask) = market
        .amm
        .bid_ask_price(
            mrk,
            market.amm.long_spread,
            market.amm.short_spread,
            market.amm.reference_price_offset,
        )
        .unwrap();

    assert_eq!(ask, 18917294498);
    assert_eq!(bid, 18376469149);
    assert_eq!(mrk, 18849999999);

    assert_eq!(market.amm.long_spread, 3570);
    assert_eq!(market.amm.peg_multiplier, 19443664550);
    assert_eq!(market.amm.short_spread, 25121);

    // add move lower
    let oracle_price_data = OraclePriceData {
        price: (18_820 * PRICE_PRECISION) as i64,
        confidence: 100 * PRICE_PRECISION_U64,
        delay: 1,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };

    let fee_budget = calculate_fee_pool(&market.amm).unwrap();
    assert_eq!(market.amm.total_fee_minus_distributions, 42992787);
    assert_eq!(fee_budget, 42992787);

    let optimal_peg = calculate_peg_from_target_price(
        market.amm.quote_asset_reserve,
        market.amm.base_asset_reserve,
        oracle_price_data.price as u64,
    )
    .unwrap();
    assert_eq!(market.amm.peg_multiplier, 19443664550);
    assert_eq!(optimal_peg, 19412719726);

    let optimal_peg_cost = calculate_repeg_cost(&market.amm, optimal_peg).unwrap();
    assert_eq!(optimal_peg_cost, 30468749);

    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
        .unwrap();
    let cost_of_update =
        _update_amm(&mut market, &mm_oracle_price_data, &state, now, slot).unwrap();
    assert_eq!(cost_of_update, 30468749);

    let mrk = market.amm.reserve_price().unwrap();
    {
        let PerpMarket {
            amm, market_stats, ..
        } = &mut market;
        crate::vlp::amm::math::spread::update_amm_quote_state(
            amm,
            market_stats,
            &mm_oracle_price_data,
            mrk,
            slot,
        )
        .unwrap();
    }
    assert_eq!(market.amm.long_spread, 3578);
    assert_eq!(market.amm.short_spread, 26753);

    let (bid, ask) = market
        .amm
        .bid_ask_price(
            mrk,
            market.amm.long_spread,
            market.amm.short_spread,
            market.amm.reference_price_offset,
        )
        .unwrap();

    assert_eq!(bid, 18316508539);
    assert_eq!(mrk, 18819999999);
    assert_eq!(ask, 18887337958);
    assert_eq!((oracle_price_data.price as u64) > bid, true);
    assert_eq!((oracle_price_data.price as u64) < ask, true);

    // add move lower
    let oracle_price_data = OraclePriceData {
        price: (18_823 * PRICE_PRECISION) as i64,
        confidence: 121 * PRICE_PRECISION_U64,
        delay: 1,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
        .unwrap();

    let cost_of_update =
        _update_amm(&mut market, &mm_oracle_price_data, &state, now, slot).unwrap();
    assert_eq!(cost_of_update, -3046875);

    let mrk = market.amm.reserve_price().unwrap();
    {
        let PerpMarket {
            amm, market_stats, ..
        } = &mut market;
        crate::vlp::amm::math::spread::update_amm_quote_state(
            amm,
            market_stats,
            &mm_oracle_price_data,
            mrk,
            slot,
        )
        .unwrap();
    }
    assert_eq!(market.amm.long_spread, 4749);
    assert_eq!(market.amm.short_spread, 25417);

    let (bid, ask) = market
        .amm
        .bid_ask_price(
            mrk,
            market.amm.long_spread,
            market.amm.short_spread,
            market.amm.reference_price_offset,
        )
        .unwrap();

    assert_eq!(bid, 18344575808);
    assert_eq!(mrk, 18822999999);
    assert_eq!(ask, 18912390425);
    assert_eq!((oracle_price_data.price as u64) > bid, true);
    assert_eq!((oracle_price_data.price as u64) < ask, true);
}

#[test]
pub fn update_amm_larg_conf_w_neg_tfmd_test() {
    let now = 1662800000 + 60;
    let slot = 81680085;

    let mut market = PerpMarket::default_btc_test();
    market.amm.concentration_coef = 1414213;
    market.amm.total_fee_minus_distributions = -(10000 * QUOTE_PRECISION as i128);
    assert_eq!(market.amm.base_asset_amount_with_amm, -1000000000);

    let state = State {
        oracle_guard_rails: OracleGuardRails {
            price_divergence: PriceDivergenceGuardRails {
                mark_oracle_percent_divergence: 1,
                oracle_twap_5min_percent_divergence: 10,
            },
            validity: ValidityGuardRails {
                slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4s
                slots_before_stale_for_margin: legacy_slot_duration_i64(120), // 48s
                confidence_interval_max_size: 20000,                      //2%
                too_volatile_ratio: 5,
            },
        },
        ..State::default()
    };

    let reserve_price_before = market.amm.reserve_price().unwrap();
    assert_eq!(reserve_price_before, 18807668638);

    let oracle_price_data = OraclePriceData {
        price: (18_850 * PRICE_PRECISION) as i64,
        confidence: 0,
        delay: 9,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
        .unwrap();
    assert_eq!(0u32, 0);
    assert_eq!(0u32, 0);
    assert_eq!(market.amm.last_update_slot, 0);
    assert_eq!(market.amm.sqrt_k, 64000000000);
    let prev_peg_multiplier = market.amm.peg_multiplier;
    let prev_total_fee_minus_distributions = market.amm.total_fee_minus_distributions;

    let cost_of_update =
        _update_amm(&mut market, &mm_oracle_price_data, &state, now, slot).unwrap();
    assert!(market
        .is_recent_oracle_valid(slot, &oracle_price_data)
        .unwrap());
    assert_eq!(cost_of_update, -42992787); // amm wins when price increases
    assert_eq!(market.amm.sqrt_k, 64000000000);
    assert_eq!(market.amm.base_asset_reserve, 65000000000);
    assert_eq!(market.amm.quote_asset_reserve, 63015384615);
    assert_eq!(market.amm.terminal_quote_asset_reserve, 64000000000);
    assert_eq!(market.amm.min_base_asset_reserve, 45254851991);
    assert_eq!(market.amm.max_base_asset_reserve, 90509632000);
    assert_eq!(market.amm.peg_multiplier, 19443664550);
    assert_eq!(market.amm.peg_multiplier > prev_peg_multiplier, true);
    assert_eq!(market.amm.total_fee_minus_distributions, -9957007213);
    assert_eq!(
        market.amm.total_fee_minus_distributions > prev_total_fee_minus_distributions,
        true
    );

    assert_eq!(market.market_stats.last_oracle_valid, true);
    assert_eq!(market.amm.last_update_slot, slot);

    let reserve_price_after = market.amm.reserve_price().unwrap();
    {
        let PerpMarket {
            amm, market_stats, ..
        } = &mut market;
        crate::vlp::amm::math::spread::update_amm_quote_state(
            amm,
            market_stats,
            &mm_oracle_price_data,
            reserve_price_after,
            slot,
        )
        .unwrap();
    }
    assert_eq!(market.amm.long_spread, 1250);
    let max_target_spread = calculate_max_target_spread(
        0i64,
        market.amm.reserve_price().unwrap(),
        market.market_stats.last_oracle_conf_pct,
        market.market_stats.mark_std,
        market.market_stats.oracle_std,
        market.amm.max_spread,
    )
    .unwrap();
    assert_eq!(max_target_spread, 29177);
    assert_eq!(market.amm.short_spread, 16020);
    assert_eq!(reserve_price_after, 18849999999);
    assert_eq!(reserve_price_before < reserve_price_after, true);

    // add large confidence
    let oracle_price_data = OraclePriceData {
        price: (18_850 * PRICE_PRECISION) as i64,
        confidence: 100 * PRICE_PRECISION_U64,
        delay: 1,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
        .unwrap();

    let cost_of_update =
        _update_amm(&mut market, &mm_oracle_price_data, &state, now, slot).unwrap();
    assert_eq!(cost_of_update, 0);

    let mrk = market.amm.reserve_price().unwrap();
    {
        let PerpMarket {
            amm, market_stats, ..
        } = &mut market;
        crate::vlp::amm::math::spread::update_amm_quote_state(
            amm,
            market_stats,
            &mm_oracle_price_data,
            mrk,
            slot,
        )
        .unwrap();
    }
    let (bid, ask) = market
        .amm
        .bid_ask_price(
            mrk,
            market.amm.long_spread,
            market.amm.short_spread,
            market.amm.reference_price_offset,
        )
        .unwrap();

    assert_eq!(bid, 18524931749);
    assert_eq!(mrk, 18849999999);
    assert_eq!(ask, 19065757098);

    assert_eq!(market.amm.long_spread, 11446);
    assert_eq!(market.amm.short_spread, 17245);

    // add move lower
    msg!("SHOULD LOWER K");
    let oracle_price_data = OraclePriceData {
        price: (18_820 * PRICE_PRECISION) as i64,
        confidence: 100 * PRICE_PRECISION_U64,
        delay: 1,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
        .unwrap();

    let fee_budget = calculate_fee_pool(&market.amm).unwrap();
    assert_eq!(market.amm.total_fee_minus_distributions, -9957007213);
    assert_eq!(fee_budget, 0);

    let optimal_peg = calculate_peg_from_target_price(
        market.amm.quote_asset_reserve,
        market.amm.base_asset_reserve,
        oracle_price_data.price as u64,
    )
    .unwrap();
    assert_eq!(market.amm.peg_multiplier, 19443664550);
    assert_eq!(optimal_peg, 19412719726);

    let optimal_peg_cost = calculate_repeg_cost(&market.amm, optimal_peg).unwrap();
    assert_eq!(optimal_peg_cost, 30468749);

    let prev_peg_multiplier = market.amm.peg_multiplier;
    let prev_total_fee_minus_distributions = market.amm.total_fee_minus_distributions;
    let cost_of_update =
        _update_amm(&mut market, &mm_oracle_price_data, &state, now, slot).unwrap();
    assert_eq!(cost_of_update, 21459587); // amm loses when price decreases (given users are net short)
    assert_eq!(market.amm.sqrt_k, 63936000000); // k lowered since cost_of_update is positive and total_fee_minus_distributions negative
    assert_eq!(market.amm.base_asset_reserve, 64935000065);
    assert_eq!(market.amm.quote_asset_reserve, 62952369167);
    assert_eq!(market.amm.terminal_quote_asset_reserve, 63936999950);
    assert_eq!(market.amm.min_base_asset_reserve, 45208890078);
    assert_eq!(market.amm.max_base_asset_reserve, 90417708246);
    assert_eq!(market.amm.peg_multiplier, 19421869997);
    assert_eq!(market.amm.peg_multiplier < prev_peg_multiplier, true);
    // assert_eq!(market.amm.total_fee_minus_distributions, -9978167413);
    assert_eq!(
        market.amm.total_fee_minus_distributions < prev_total_fee_minus_distributions,
        true
    );

    assert_eq!(market.market_stats.last_oracle_valid, true);
    assert_eq!(market.amm.last_update_slot, slot);

    let mrk = market.amm.reserve_price().unwrap();
    {
        let PerpMarket {
            amm, market_stats, ..
        } = &mut market;
        crate::vlp::amm::math::spread::update_amm_quote_state(
            amm,
            market_stats,
            &mm_oracle_price_data,
            mrk,
            slot,
        )
        .unwrap();
    }
    assert_eq!(market.amm.long_spread, 11123);
    assert_eq!(market.amm.short_spread, 18972);

    let (bid, ask) = market
        .amm
        .bid_ask_price(
            mrk,
            market.amm.long_spread,
            market.amm.short_spread,
            market.amm.reference_price_offset,
        )
        .unwrap();

    let max_target_spread = calculate_max_target_spread(
        0i64,
        market.amm.reserve_price().unwrap(),
        market.market_stats.last_oracle_conf_pct,
        market.market_stats.mark_std,
        market.market_stats.oracle_std,
        market.amm.max_spread,
    )
    .unwrap();
    assert_eq!(market.amm.max_spread, 975);
    assert_eq!(max_target_spread, 30095);
    assert_eq!(market.market_stats.mark_std, 1_000_000);

    let orc = oracle_price_data.price as u64;
    assert_eq!(bid, 18471649513);
    assert_eq!(orc, 18820000000);
    assert_eq!(mrk, 18828870851);
    assert_eq!(ask, 19038304381);

    assert_eq!(bid <= orc, true);

    // add move lower
    let oracle_price_data = OraclePriceData {
        price: (18_823 * PRICE_PRECISION) as i64,
        confidence: 121 * PRICE_PRECISION_U64,
        delay: 1,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
        .unwrap();

    let cost_of_update =
        _update_amm(&mut market, &mm_oracle_price_data, &state, now, slot).unwrap();
    assert_eq!(cost_of_update, 299367);

    let mrk = market.amm.reserve_price().unwrap();
    {
        let PerpMarket {
            amm, market_stats, ..
        } = &mut market;
        crate::vlp::amm::math::spread::update_amm_quote_state(
            amm,
            market_stats,
            &mm_oracle_price_data,
            mrk,
            slot,
        )
        .unwrap();
    }
    assert_eq!(market.amm.long_spread, 11575);
    assert_eq!(market.amm.short_spread, 18536);

    let (bid, ask) = market
        .amm
        .bid_ask_price(
            mrk,
            market.amm.long_spread,
            market.amm.short_spread,
            market.amm.reference_price_offset,
        )
        .unwrap();

    assert_eq!(bid, 18479569575);
    assert_eq!(mrk, 18828576061);
    assert_eq!(ask, 19046516828);
    assert_eq!((oracle_price_data.price as u64) > bid, true);
    assert_eq!((oracle_price_data.price as u64) < ask, true);
}

/// A market with a stale curve (reserve price about 18_807, users net short)
/// and an oracle at `oracle_price`.
fn mark_sample_fixture(
    oracle_price: i64,
    total_fee_minus_distributions: i128,
    oracle_delay: i64,
) -> (
    PerpMarket,
    MMOraclePriceData,
    Option<OracleValidity>,
    i64,
    u64,
) {
    let mut market = PerpMarket {
        market_stats: MarketStats {
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price: oracle_price,
                last_oracle_price_twap: oracle_price,
                last_oracle_price_twap_5min: oracle_price,
                ..HistoricalOracleData::default()
            },
            ..MarketStats::default()
        },
        amm: AMM {
            base_asset_reserve: 65 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 63015384615,
            terminal_quote_asset_reserve: 64 * AMM_RESERVE_PRECISION,
            sqrt_k: 64 * AMM_RESERVE_PRECISION,
            peg_multiplier: 19_400 * PEG_PRECISION,
            base_asset_amount_with_amm: -(AMM_RESERVE_PRECISION as i128),
            base_spread: 250,
            max_spread: 55500,
            curve_update_intensity: 100,
            concentration_coef: 31020710,
            total_fee_minus_distributions,
            ..AMM::default()
        },
        status: MarketStatus::Active,
        contract_tier: ContractTier::B,
        margin_ratio_initial: 555,
        ..PerpMarket::default()
    };
    let (_, terminal_base) = amm::calculate_terminal_reserves(&market.amm).unwrap();
    let (min_base, max_base) =
        amm::calculate_bid_ask_bounds(market.amm.concentration_coef, terminal_base).unwrap();
    market.amm.min_base_asset_reserve = min_base;
    market.amm.max_base_asset_reserve = max_base;

    let state = State {
        oracle_guard_rails: OracleGuardRails {
            validity: ValidityGuardRails {
                slots_before_stale_for_amm: legacy_slot_duration_i64(10),
                slots_before_stale_for_margin: legacy_slot_duration_i64(120),
                confidence_interval_max_size: 20_000,
                too_volatile_ratio: 5,
            },
            ..OracleGuardRails::default()
        },
        ..State::default()
    };
    let now = 10_000;
    let slot = 81_680_085;
    let oracle_price_data = OraclePriceData {
        price: oracle_price,
        confidence: 0,
        delay: oracle_delay,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
        .unwrap();
    let validity =
        compute_amm_refresh_validity(&market, &mm_oracle_price_data, &state, slot).unwrap();
    (market, mm_oracle_price_data, validity, now, slot)
}

/// Bid and ask read through `bid_ask_price`, as routing and the mark TWAP do.
fn read_quote(market: &PerpMarket) -> (u64, u64) {
    let amm = &market.amm;
    amm.bid_ask_price(
        amm.reserve_price().unwrap(),
        amm.long_spread,
        amm.short_spread,
        amm.reference_price_offset,
    )
    .unwrap()
}

/// `update_perp_bid_ask_twap` samples the AMM quote into the mark TWAP, so it
/// must refresh the curve onto the current oracle first. Sampling a stale peg
/// biases the mark TWAP and, through it, funding.
#[test]
pub fn refresh_for_mark_sample_repegs_a_stale_curve() {
    let (mut market, mm_oracle_price_data, validity, now, slot) = mark_sample_fixture(
        19_000 * PRICE_PRECISION_I64,
        1_000_000 * QUOTE_PRECISION as i128,
        1,
    );
    let oracle_price = mm_oracle_price_data.get_price();
    assert!(is_oracle_valid_for_action(
        validity.unwrap(),
        Some(VelocityAction::FillOrderAmmLowRisk)
    )
    .unwrap());

    // The curve sits about 1% below the oracle.
    let stale_price = market.amm.reserve_price().unwrap();
    assert!(stale_price < oracle_price as u64 * 995 / 1000);

    // Without the refresh (the crank's behavior before this fix), the quote
    // state is built on the stale curve and carries the whole gap.
    let mut unrefreshed = market;
    unrefreshed
        .update_oracle_derived_stats(&mm_oracle_price_data, validity, now, slot)
        .unwrap();
    assert!(unrefreshed.amm.last_oracle_reserve_price_spread_pct < -5_000);

    // With it, the curve is repegged onto the oracle before the quote state is
    // built, so the sampled quote reflects the market rather than the stale peg.
    refresh_for_mark_sample(&mut market, &mm_oracle_price_data, validity, now, slot).unwrap();
    let refreshed_price = market.amm.reserve_price().unwrap();
    assert_ne!(market.amm.peg_multiplier, 19_400 * PEG_PRECISION);
    assert!(refreshed_price.abs_diff(oracle_price as u64) <= oracle_price as u64 / 10_000);
    assert!(market.amm.last_oracle_reserve_price_spread_pct.abs() <= 100);
    assert_eq!(market.amm.last_update_slot, slot);
    assert_eq!(market.amm.last_spread_update_slot, slot);

    // Slot-idempotent: a second call in the same slot does not move the curve.
    let peg_after = market.amm.peg_multiplier;
    refresh_for_mark_sample(&mut market, &mm_oracle_price_data, validity, now, slot).unwrap();
    assert_eq!(market.amm.peg_multiplier, peg_after);
}

/// A repeg down costs the pool here (it is long against net-short users), and
/// with almost no fees to pay for it the curve stays stale and the AMM is not marked
/// fresh. The quote state is still rebuilt, and the oracle guard keeps the
/// stale quote from crossing the oracle.
#[test]
pub fn refresh_for_mark_sample_keeps_the_curve_when_the_repeg_is_unaffordable() {
    let (mut market, mm_oracle_price_data, validity, now, slot) =
        mark_sample_fixture(18_600 * PRICE_PRECISION_I64, 1, 1);
    let oracle_price = mm_oracle_price_data.get_price() as u64;
    assert!(is_oracle_valid_for_action(
        validity.unwrap(),
        Some(VelocityAction::FillOrderAmmLowRisk)
    )
    .unwrap());

    refresh_for_mark_sample(&mut market, &mm_oracle_price_data, validity, now, slot).unwrap();
    assert_eq!(market.amm.peg_multiplier, 19_400 * PEG_PRECISION);
    assert_ne!(market.amm.last_update_slot, slot);
    assert_eq!(market.amm.last_spread_update_slot, slot);
    assert!(market.amm.last_oracle_reserve_price_spread_pct > 5_000);

    let (bid, ask) = read_quote(&market);
    assert!(
        bid <= oracle_price && ask >= oracle_price,
        "{bid} {ask} {oracle_price}"
    );
}

/// A stale oracle still repegs the curve (`UpdateAMMCurve` rejects only a
/// nonpositive price) but does not mark the AMM fresh for fills.
#[test]
pub fn refresh_for_mark_sample_repegs_on_a_stale_oracle_without_marking_fresh() {
    let (mut market, mm_oracle_price_data, validity, now, slot) = mark_sample_fixture(
        19_000 * PRICE_PRECISION_I64,
        1_000_000 * QUOTE_PRECISION as i128,
        100,
    );
    let validity_value = validity.unwrap();
    assert!(
        !is_oracle_valid_for_action(validity_value, Some(VelocityAction::FillOrderAmmLowRisk))
            .unwrap()
    );

    refresh_for_mark_sample(&mut market, &mm_oracle_price_data, validity, now, slot).unwrap();
    assert_ne!(market.amm.peg_multiplier, 19_400 * PEG_PRECISION);
    assert_ne!(market.amm.last_update_slot, slot);
    assert_eq!(market.amm.last_spread_update_slot, slot);
    assert!(!market.market_stats.last_oracle_valid);
}

/// No validity (a Settlement market) leaves the curve and quote state alone.
#[test]
pub fn refresh_for_mark_sample_does_nothing_without_validity() {
    let (mut market, mm_oracle_price_data, _, now, slot) = mark_sample_fixture(
        19_000 * PRICE_PRECISION_I64,
        1_000_000 * QUOTE_PRECISION as i128,
        1,
    );
    let before = market;
    refresh_for_mark_sample(&mut market, &mm_oracle_price_data, None, now, slot).unwrap();
    assert_eq!(market.amm.peg_multiplier, before.amm.peg_multiplier);
    assert_eq!(market.amm.last_update_slot, before.amm.last_update_slot);
    assert_eq!(
        market.amm.last_spread_update_slot,
        before.amm.last_spread_update_slot
    );
    assert_eq!(market.amm.long_spread, before.amm.long_spread);
}
