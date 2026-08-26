use crate::{
    math::{
        constants::{AMM_RESERVE_PRECISION, PEG_PRECISION, PRICE_PRECISION, PRICE_PRECISION_U64},
        oracle::*,
        time::{legacy_slot_duration_i64, SlotDuration},
    },
    state::{
        oracle::HistoricalOracleData,
        perp_market::{ContractTier, MarketStats, PerpMarket, AMM},
        state::{OracleGuardRails, PriceDivergenceGuardRails, State, ValidityGuardRails},
    },
};

#[test]
fn mm_immediate_threshold_matches_write_gate_at_every_gate() {
    // The unset MM-sourced immediate-fill threshold in `oracle_validity` and the
    // crank's write gate in `update_mm_oracle` both convert MM_ORACLE_MIN_WRITE_GAP
    // to slots and MUST agree at every slot duration — otherwise a quote the crank
    // was allowed to post reads stale on the fill path. Both ceil; pin the values.
    use crate::math::{constants::MM_ORACLE_MIN_WRITE_GAP, time::SlotDuration};
    for (ms, expected) in [(400u16, 2u64), (350, 3), (300, 3), (250, 4), (200, 4)] {
        let d = SlotDuration::from_state_ms(ms);
        // the immediate threshold (oracle_validity, `slots_ceil`) and the write
        // gate (update_mm_oracle, `to_slots_ceil`) are the same expression
        let threshold = MM_ORACLE_MIN_WRITE_GAP.to_slots_ceil(d);
        assert_eq!(threshold, expected, "MM write-gap slots wrong at {ms}ms");
    }
}

#[test]
fn staleness_windows_scale_at_non_baseline_duration() {
    let guard_rails = ValidityGuardRails {
        slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4 seconds
        slots_before_stale_for_margin: legacy_slot_duration_i64(120),
        confidence_interval_max_size: 20_000,
        too_volatile_ratio: 5,
    };
    let oracle_price_data = OraclePriceData {
        price: (100 * PRICE_PRECISION) as i64,
        confidence: 1,
        delay: 15,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };
    let validity = |current_slot, slot_clock| {
        oracle_validity(
            MarketType::Perp,
            0,
            oracle_price_data.price,
            &oracle_price_data,
            &guard_rails,
            1,
            &OracleSource::PythLazer,
            LogMode::None,
            10,
            false,
            10,
            current_slot,
            slot_clock,
        )
        .unwrap()
    };

    // 15 slots of delay: 6s at 400ms (stale beyond the 4s window), 3s at 200ms
    assert!(matches!(
        validity(1_000_000, SlotClock::baseline()),
        OracleValidity::StaleForAMM { .. }
    ));
    assert_eq!(
        validity(
            1_000_000,
            SlotClock::from_state_fields([1, 1, 1, 1], 0, 0, 0)
        ),
        OracleValidity::Valid
    );
}

#[test]
fn calculate_oracle_valid() {
    let prev = 1656682258;
    let now = prev + 3600;
    let state = State::default();

    let px = 32 * PRICE_PRECISION;
    let amm = AMM {
        base_asset_reserve: 2 * AMM_RESERVE_PRECISION,
        quote_asset_reserve: 2 * AMM_RESERVE_PRECISION,
        peg_multiplier: 33 * PEG_PRECISION,
        ..AMM::default()
    };
    let market_stats = MarketStats {
        historical_oracle_data: HistoricalOracleData {
            last_oracle_price_twap_5min: px as i64,
            last_oracle_price_twap: (px as i64) - 1000,
            last_oracle_price_twap_ts: prev,
            ..HistoricalOracleData::default()
        },
        mark_std: PRICE_PRECISION as u64,
        last_mark_price_twap_ts: prev,
        funding_period: 3600_i64,
        ..MarketStats::default()
    };
    let mut oracle_price_data = OraclePriceData {
        price: (34 * PRICE_PRECISION) as i64,
        confidence: PRICE_PRECISION_U64 / 100,
        delay: 1,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };
    let mut market: PerpMarket = PerpMarket {
        amm,
        market_stats,
        contract_tier: ContractTier::B,
        ..PerpMarket::default()
    };
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(
            oracle_price_data,
            10000,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
        .unwrap();

    let state = State {
        oracle_guard_rails: OracleGuardRails {
            price_divergence: PriceDivergenceGuardRails {
                mark_oracle_percent_divergence: 1,
                oracle_twap_5min_percent_divergence: 10,
            },
            validity: ValidityGuardRails {
                slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4s
                slots_before_stale_for_margin: legacy_slot_duration_i64(120), // 48s
                confidence_interval_max_size: 20000,                      // 2%
                too_volatile_ratio: 5,
            },
        },
        ..State::default()
    };

    let mut oracle_status = get_oracle_status(
        &market,
        &oracle_price_data,
        &state.oracle_guard_rails,
        market.amm.reserve_price().unwrap(),
        1_000_000,
        SlotClock::baseline(),
    )
    .unwrap();

    assert!(oracle_status.oracle_validity == OracleValidity::Valid);
    assert_eq!(oracle_status.oracle_reserve_price_spread_pct, 30303); //0.030303 ()
    assert!(!oracle_status.mark_too_divergent);

    let _new_oracle_twap = market
        .market_stats
        .update_oracle_twap(&market.amm, now, &mm_oracle_price_data, None, None)
        .unwrap();
    assert_eq!(
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap,
        (34 * PRICE_PRECISION - PRICE_PRECISION / 100) as i64
    );

    oracle_price_data = OraclePriceData {
        price: (34 * PRICE_PRECISION) as i64,
        confidence: PRICE_PRECISION_U64 / 100,
        delay: 11,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };
    oracle_status = get_oracle_status(
        &market,
        &oracle_price_data,
        &state.oracle_guard_rails,
        market.amm.reserve_price().unwrap(),
        1_000_000,
        SlotClock::baseline(),
    )
    .unwrap();
    assert!(oracle_status.oracle_validity != OracleValidity::Valid);

    oracle_price_data.delay = 8;
    market
        .market_stats
        .historical_oracle_data
        .last_oracle_price_twap_5min = 32 * PRICE_PRECISION as i64;
    market
        .market_stats
        .historical_oracle_data
        .last_oracle_price_twap = 21 * PRICE_PRECISION as i64;
    oracle_status = get_oracle_status(
        &market,
        &oracle_price_data,
        &state.oracle_guard_rails,
        market.amm.reserve_price().unwrap(),
        1_000_000,
        SlotClock::baseline(),
    )
    .unwrap();
    assert!(oracle_status.oracle_validity == OracleValidity::Valid);
    assert!(!oracle_status.mark_too_divergent);

    market
        .market_stats
        .historical_oracle_data
        .last_oracle_price_twap_5min = 29 * PRICE_PRECISION as i64;
    oracle_status = get_oracle_status(
        &market,
        &oracle_price_data,
        &state.oracle_guard_rails,
        market.amm.reserve_price().unwrap(),
        1_000_000,
        SlotClock::baseline(),
    )
    .unwrap();
    assert!(oracle_status.mark_too_divergent);
    assert!(oracle_status.oracle_validity == OracleValidity::Valid);

    oracle_price_data.confidence = PRICE_PRECISION_U64;
    oracle_status = get_oracle_status(
        &market,
        &oracle_price_data,
        &state.oracle_guard_rails,
        market.amm.reserve_price().unwrap(),
        1_000_000,
        SlotClock::baseline(),
    )
    .unwrap();
    assert!(oracle_status.mark_too_divergent);
    assert!(oracle_status.oracle_validity == OracleValidity::TooUncertain);
}

/// `oracle_slot_delay_override` is the max oracle delay, in slots, that
/// immediate (JIT / auction-skipping) AMM fills tolerate.
///
/// The negative (unset) case is the one that mattered, and its resolution is
/// source-aware. It used to clamp to `max(override, 0)`, i.e. a threshold of
/// zero, requiring the price to have been written in this very slot. For an
/// MM-oracle-sourced price that is unsatisfiable by construction, because the
/// program refuses MM-oracle writes closer together than
/// `MM_ORACLE_MIN_SLOT_GAP` slots — so unset resolves to that gap for an
/// MM-sourced price. An exchange-oracle price has no such floor and keeps the
/// strict zero threshold, so the safe-price fallback path (which engages
/// exactly when the MM oracle is stale or diverged) is not widened.
#[test]
fn immediate_staleness_threshold_by_override() {
    let guard_rails = ValidityGuardRails {
        slots_before_stale_for_amm: legacy_slot_duration_i64(10),
        slots_before_stale_for_margin: legacy_slot_duration_i64(120),
        confidence_interval_max_size: 20_000,
        too_volatile_ratio: 5,
    };

    let is_valid = |delay: i64, immediate_override: i8, mm_sourced: bool| -> bool {
        let oracle_price_data = OraclePriceData {
            price: (100 * PRICE_PRECISION) as i64,
            confidence: 1,
            delay,
            has_sufficient_number_of_data_points: true,
            sequence_id: None,
        };
        let validity = oracle_validity(
            MarketType::Perp,
            0,
            (100 * PRICE_PRECISION) as i64,
            &oracle_price_data,
            &guard_rails,
            1,
            &OracleSource::PythLazer,
            LogMode::ExchangeOracle,
            immediate_override,
            mm_sourced,
            0,
            1_000_000,
            SlotClock::baseline(),
        )
        .unwrap();
        matches!(validity, OracleValidity::Valid)
    };

    // Unset + MM-sourced resolves to MM_ORACLE_MIN_WRITE_GAP, the tightest
    // window the crank can actually satisfy, rather than to zero.
    let min_gap =
        crate::math::constants::MM_ORACLE_MIN_WRITE_GAP.to_slots(SlotDuration::BASELINE) as i64;
    for delay in 0..=min_gap {
        assert!(
            is_valid(delay, -1, true),
            "delay {delay} should be Valid when unset and MM-sourced"
        );
    }
    assert!(
        !is_valid(min_gap + 1, -1, true),
        "unset must not tolerate more than MM_ORACLE_MIN_WRITE_GAP"
    );

    let is_valid_350 = |delay: i64| {
        let oracle_price_data = OraclePriceData {
            price: (100 * PRICE_PRECISION) as i64,
            confidence: 1,
            delay,
            has_sufficient_number_of_data_points: true,
            sequence_id: None,
        };
        matches!(
            oracle_validity(
                MarketType::Perp,
                0,
                (100 * PRICE_PRECISION) as i64,
                &oracle_price_data,
                &guard_rails,
                1,
                &OracleSource::PythLazer,
                LogMode::ExchangeOracle,
                -1,
                true,
                0,
                1_000_000,
                SlotClock::from_state_fields([1, 0, 0, 0], 0, 0, 0),
            )
            .unwrap(),
            OracleValidity::Valid
        )
    };
    // 800ms rounds up to the crank's legal three-slot interval (1050ms).
    assert!(is_valid_350(3));
    assert!(!is_valid_350(4));

    // Unset + exchange-sourced keeps the strict zero threshold: the exchange
    // oracle can be same-slot fresh, so nothing forces a wider window there.
    assert!(is_valid(0, -1, false));
    assert!(
        !is_valid(1, -1, false),
        "unset must not widen the exchange-sourced threshold"
    );

    // An explicit positive threshold still wins in both directions, tighter or
    // looser than the default, regardless of the price source.
    for mm_sourced in [false, true] {
        assert!(is_valid(1, 1, mm_sourced));
        assert!(!is_valid(2, 1, mm_sourced));
        assert!(is_valid(5, 5, mm_sourced));
        assert!(!is_valid(6, 5, mm_sourced));
    }

    // Zero remains the explicit "no immediate AMM fills on this market"
    // sentinel: never Valid, not even same-slot, regardless of source.
    assert!(!is_valid(0, 0, true));
    assert!(!is_valid(0, 0, false));
}

#[test]
fn fill_order_match_admits_the_same_set_as_margin_calc() {
    // A DLOB match must not execute at a price the program cannot do margin
    // with. `FillOrderMatch` used to admit `StaleForMargin`, which let both
    // sides exactly close (reducing, so the equity-floor gate is skipped)
    // while the lazy breaker stayed blind for want of `MarginCalc` validity,
    // crystallizing a temporary mark loss at an unusable price (OtterSec
    // #142). The two actions therefore admit the same set.
    let states = [
        OracleValidity::NonPositive,
        OracleValidity::TooVolatile,
        OracleValidity::TooUncertain,
        OracleValidity::StaleForMargin,
        OracleValidity::InsufficientDataPoints,
        OracleValidity::StaleForAMM {
            immediate: true,
            low_risk: true,
        },
        OracleValidity::StaleForAMM {
            immediate: true,
            low_risk: false,
        },
        OracleValidity::Valid,
    ];

    for validity in states {
        assert_eq!(
            is_oracle_valid_for_action(validity, Some(VelocityAction::FillOrderMatch)).unwrap(),
            is_oracle_valid_for_action(validity, Some(VelocityAction::MarginCalc)).unwrap(),
            "FillOrderMatch and MarginCalc disagree on {:?}",
            validity
        );
    }

    assert!(
        !is_oracle_valid_for_action(
            OracleValidity::StaleForMargin,
            Some(VelocityAction::FillOrderMatch)
        )
        .unwrap(),
        "a stale-for-margin oracle must not admit a match"
    );

    // The actions that shared the arm keep their own, looser policy: they read
    // a price for bookkeeping rather than admitting a trade against it.
    for action in [
        VelocityAction::UpdateAmmCache,
        VelocityAction::UpdateLpPoolAum,
        VelocityAction::LpPoolSwap,
    ] {
        assert!(
            is_oracle_valid_for_action(OracleValidity::StaleForMargin, Some(action)).unwrap(),
            "{:?} should not have been tightened alongside FillOrderMatch",
            action
        );
    }
}
