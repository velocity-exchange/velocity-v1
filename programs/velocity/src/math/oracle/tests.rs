use crate::{
    math::{
        constants::{AMM_RESERVE_PRECISION, PEG_PRECISION, PRICE_PRECISION, PRICE_PRECISION_U64},
        oracle::*,
    },
    state::{
        oracle::HistoricalOracleData,
        perp_market::{ContractTier, MarketStats, PerpMarket, AMM},
        state::{OracleGuardRails, PriceDivergenceGuardRails, State, ValidityGuardRails},
    },
};

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
        .get_mm_oracle_price_data(oracle_price_data, 10000, &state.oracle_guard_rails.validity)
        .unwrap();

    let state = State {
        oracle_guard_rails: OracleGuardRails {
            price_divergence: PriceDivergenceGuardRails {
                mark_oracle_percent_divergence: 1,
                oracle_twap_5min_percent_divergence: 10,
            },
            validity: ValidityGuardRails {
                slots_before_stale_for_amm: 10,      // 5s
                slots_before_stale_for_margin: 120,  // 60s
                confidence_interval_max_size: 20000, // 2%
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
    )
    .unwrap();
    assert!(oracle_status.mark_too_divergent);
    assert!(oracle_status.oracle_validity == OracleValidity::TooUncertain);
}

/// `oracle_slot_delay_override` is the max oracle delay, in slots, that
/// immediate (JIT / auction-skipping) AMM fills tolerate.
///
/// The negative case is the one that mattered. It used to clamp to
/// `max(override, 0)`, i.e. a threshold of zero, requiring the price to have
/// been written in this very slot. For an MM-oracle-sourced price that is
/// unsatisfiable by construction, because the program refuses MM-oracle writes
/// closer together than `MM_ORACLE_MIN_SLOT_GAP` slots. Any market left at the
/// init default of `-1` therefore failed this gate on roughly half of all slots
/// regardless of how hard it was cranked.
#[test]
fn immediate_staleness_threshold_by_override() {
    let guard_rails = ValidityGuardRails {
        slots_before_stale_for_amm: 10,
        slots_before_stale_for_margin: 120,
        confidence_interval_max_size: 20_000,
        too_volatile_ratio: 5,
    };

    let is_valid = |delay: i64, immediate_override: i8| -> bool {
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
            0,
        )
        .unwrap();
        matches!(validity, OracleValidity::Valid)
    };

    // Unset resolves to MM_ORACLE_MIN_SLOT_GAP, the tightest window the crank
    // can actually satisfy, rather than to zero.
    let min_gap = MM_ORACLE_MIN_SLOT_GAP as i64;
    for delay in 0..=min_gap {
        assert!(
            is_valid(delay, -1),
            "delay {delay} should be Valid when unset"
        );
    }
    assert!(
        !is_valid(min_gap + 1, -1),
        "unset must not tolerate more than MM_ORACLE_MIN_SLOT_GAP"
    );

    // An explicit positive threshold still wins in both directions, tighter or
    // looser than the default.
    assert!(is_valid(1, 1));
    assert!(!is_valid(2, 1));
    assert!(is_valid(5, 5));
    assert!(!is_valid(6, 5));

    // Zero remains the explicit "no immediate AMM fills on this market"
    // sentinel: never Valid, not even same-slot.
    assert!(!is_valid(0, 0));
}
