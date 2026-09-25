use crate::math::time::SlotClock;
// use crate::create_anchor_account_info;
use {
    crate::{
        controller::funding::update_funding_rate,
        math::{
            constants::{
                AMM_RESERVE_PRECISION, BPS_PRECISION, ONE_HOUR, ONE_HOUR_I128, PEG_PRECISION,
                PERCENTAGE_PRECISION_U32, PRICE_PRECISION, PRICE_PRECISION_I64,
                PRICE_PRECISION_U64, QUOTE_PRECISION,
            },
            funding::*,
            helpers::on_the_hour_update,
            oracle::{block_operation, OracleValidity},
            time::legacy_slot_duration_i64,
        },
        state::{
            oracle::{HistoricalOracleData, MMOraclePriceData},
            oracle_map::OracleMap,
            perp_market::{ContractTier, FeeLedger, MarketStats, PerpMarket, AMM},
            state::{OracleGuardRails, State, ValidityGuardRails},
        },
        test_utils::get_pyth_price,
        vlp::amm::refresh::_update_amm,
    },
    solana_program::pubkey::Pubkey,
    std::{cmp::min, str::FromStr},
};

fn calculate_funding_rate(
    mid_price_twap: u128,
    oracle_price_twap: i128,
    funding_period: i64,
) -> VelocityResult<i128> {
    // funding period = 1 hour, window = 1 day
    // low periodicity => quickly updating/settled funding rates
    //                 => lower funding rate payment per interval
    let period_adjustment = (24_i128)
        .safe_mul(ONE_HOUR_I128)?
        .safe_div(funding_period as i128)?;

    let price_spread = mid_price_twap.cast::<i128>()?.safe_sub(oracle_price_twap)?;

    // clamp price divergence to 3% for funding rate calculation
    let max_price_spread = oracle_price_twap.safe_div(33)?; // 3%
    let clamped_price_spread = max(-max_price_spread, min(price_spread, max_price_spread));

    let funding_rate = clamped_price_spread
        .safe_mul(FUNDING_RATE_BUFFER.cast()?)?
        .safe_div(period_adjustment.cast()?)?;

    Ok(funding_rate)
}

use crate::{create_anchor_account_info, state::pyth_lazer_oracle::PythLazerOracle};

#[test]
fn balanced_funding_test() {
    // balanced market no fees collected

    let sqrt_k0 = 100 * AMM_RESERVE_PRECISION + 8793888383;
    let px0 = 32_513_929;
    let mut count = 0;

    while count < 2 {
        let px = px0 + count;
        let sqrt_k = sqrt_k0 + count;

        let market = PerpMarket {
            amm: AMM {
                base_asset_reserve: sqrt_k,
                quote_asset_reserve: sqrt_k,
                sqrt_k,
                peg_multiplier: px,
                base_asset_amount_with_amm: 0,
                total_fee_minus_distributions: (count * 1000000783) as i128,

                ..AMM::default()
            },
            base_asset_amount_long: 12295081967,
            base_asset_amount_short: -12295081967,
            fee_ledger: FeeLedger {
                total_exchange_fee: (count * 1000000783) / 2888,
                ..FeeLedger::default()
            },
            market_stats: MarketStats {
                funding_period: 3600,
                last_mark_price_twap: (px * 999 / 1000) as u64,
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: (px * 1001 / 1000) as i64,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        let balanced_funding = calculate_funding_rate(
            market.market_stats.last_mark_price_twap as u128,
            market
                .market_stats
                .historical_oracle_data
                .last_oracle_price_twap as i128,
            market.market_stats.funding_period,
        )
        .unwrap();

        assert_eq!(
            market.market_stats.last_mark_price_twap
                < (market
                    .market_stats
                    .historical_oracle_data
                    .last_oracle_price_twap as u64),
            true
        );

        let (long_funding, short_funding, _) = calculate_funding_rate_long_short(
            &crate::math::funding::FundingMarketInputs::from_market(&market),
            balanced_funding,
        )
        .unwrap();

        assert_eq!(balanced_funding, -2709458);
        assert_eq!(long_funding, -2709458);
        assert_eq!(short_funding, -2709458);
        count += 1;
    }

    let sqrt_k0 = 55 * AMM_RESERVE_PRECISION + 48383;
    let px0 = 19_902_513_929;
    let mut count = 0;

    while count < 2 {
        let px = px0 + count;
        let sqrt_k = sqrt_k0 + count;

        let market = PerpMarket {
            amm: AMM {
                base_asset_reserve: sqrt_k,
                quote_asset_reserve: sqrt_k,
                sqrt_k,
                peg_multiplier: px,
                base_asset_amount_with_amm: 0,
                total_fee_minus_distributions: (count * 1000000783) as i128,

                ..AMM::default()
            },
            base_asset_amount_long: 7845926098328,
            base_asset_amount_short: -7845926098328,
            fee_ledger: FeeLedger {
                total_exchange_fee: (count * 1000000783) / 2888,
                ..FeeLedger::default()
            },
            market_stats: MarketStats {
                funding_period: 3600,
                last_mark_price_twap: (px * 999 / 1000) as u64,
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price_twap: (px * 888 / 1000) as i64,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        let balanced_funding = calculate_funding_rate(
            market.market_stats.last_mark_price_twap as u128,
            market
                .market_stats
                .historical_oracle_data
                .last_oracle_price_twap as i128,
            market.market_stats.funding_period,
        )
        .unwrap();

        //sanity, funding CANT be larger than oracle twap price
        assert_eq!(balanced_funding < (px * FUNDING_RATE_BUFFER) as i128, true);
        assert_eq!(
            balanced_funding
                < ((market
                    .market_stats
                    .historical_oracle_data
                    .last_oracle_price_twap as u128)
                    * FUNDING_RATE_BUFFER) as i128,
            true
        );

        assert_eq!(
            market.market_stats.last_mark_price_twap
                > (market
                    .market_stats
                    .historical_oracle_data
                    .last_oracle_price_twap as u64),
            true
        );

        let (long_funding, short_funding, _) = calculate_funding_rate_long_short(
            &crate::math::funding::FundingMarketInputs::from_market(&market),
            balanced_funding,
        )
        .unwrap();

        assert_eq!(balanced_funding, 22_314_939_833); // 2_231_493 in PRICE_PRECISION
        assert_eq!(long_funding, 22_314_939_833);
        assert_eq!(short_funding, 22_314_939_833);
        count += 1;
    }
}

#[test]
fn capped_sym_funding_test() {
    // more shorts than longs, positive funding, 1/3 of fee pool too small
    let mut market = PerpMarket {
        amm: AMM {
            base_asset_reserve: 512295081967,
            quote_asset_reserve: 488 * AMM_RESERVE_PRECISION,
            sqrt_k: 500 * AMM_RESERVE_PRECISION,
            peg_multiplier: 50000000,
            base_asset_amount_with_amm: -12295081967,
            total_fee_minus_distributions: (QUOTE_PRECISION as i128) / 2,

            ..AMM::default()
        },
        base_asset_amount_long: 12295081967,
        base_asset_amount_short: -12295081967 * 2,
        // pendings no longer floor the funding budget: tfmd contains only
        // the AMM's own equity post-isolation, spendable down to zero
        // (capped at 1/3 per period)
        fee_ledger: FeeLedger {
            total_exchange_fee: QUOTE_PRECISION / 2,
            pending_protocol_fee: QUOTE_PRECISION / 4,
            ..FeeLedger::default()
        },
        market_stats: MarketStats {
            funding_period: 3600,
            last_mark_price_twap: 50 * PRICE_PRECISION_U64,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (49 * PRICE_PRECISION) as i64,

                ..HistoricalOracleData::default()
            },
            ..MarketStats::default()
        },
        ..PerpMarket::default()
    };

    let balanced_funding = calculate_funding_rate(
        market.market_stats.last_mark_price_twap as u128,
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap as i128,
        market.market_stats.funding_period,
    )
    .unwrap();

    assert_eq!(balanced_funding, 41666666);

    let (long_funding, short_funding, capped_pnl) = calculate_funding_rate_long_short(
        &crate::math::funding::FundingMarketInputs::from_market(&market),
        balanced_funding,
    )
    .unwrap();

    // `calculate_funding_rate_long_short` is pure now; the caller records
    // the AMM's PnL explicitly so this test does so too.
    use crate::vlp::amm::quoter::AmmContract;
    market.amm.record_amm_pnl(capped_pnl).unwrap();

    assert_eq!(long_funding, balanced_funding);
    assert!(long_funding > short_funding);
    assert_eq!(short_funding, 27611040);

    // only spend 1/3 of the (full, unfloored) 0.5-QUOTE fee pool
    assert_eq!(market.amm.total_fee_minus_distributions, 333334);

    // more longs than shorts, positive funding, amm earns funding
    market = PerpMarket {
        amm: AMM {
            base_asset_reserve: 512295081967,
            quote_asset_reserve: 488 * AMM_RESERVE_PRECISION,
            sqrt_k: 500 * AMM_RESERVE_PRECISION,
            peg_multiplier: 50000000,
            base_asset_amount_with_amm: 12295081967,
            total_fee_minus_distributions: (QUOTE_PRECISION as i128) / 2,

            ..AMM::default()
        },
        base_asset_amount_long: 12295081967 * 2,
        base_asset_amount_short: -12295081967,
        fee_ledger: FeeLedger {
            total_exchange_fee: QUOTE_PRECISION / 2,
            ..FeeLedger::default()
        },
        market_stats: MarketStats {
            funding_period: 3600,
            last_mark_price_twap: 50 * PRICE_PRECISION_U64,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (49 * PRICE_PRECISION) as i64,

                ..HistoricalOracleData::default()
            },
            ..MarketStats::default()
        },
        ..PerpMarket::default()
    };

    assert_eq!(balanced_funding, 41666666);

    let (long_funding, short_funding, capped_pnl) = calculate_funding_rate_long_short(
        &crate::math::funding::FundingMarketInputs::from_market(&market),
        balanced_funding,
    )
    .unwrap();
    market.amm.record_amm_pnl(capped_pnl).unwrap();

    assert_eq!(long_funding, balanced_funding);
    assert_eq!(long_funding, short_funding);
    let new_fees = market.amm.total_fee_minus_distributions;
    assert!(new_fees > QUOTE_PRECISION as i128 / 2);
    assert_eq!(new_fees, 1012295); // made over $.50
}

#[test]
fn max_funding_rates() {
    let now = 0_i64;
    let slot = 0_u64;

    let state = State {
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

    let mut oracle_price = get_pyth_price(51, 6);
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
        market_index: 0,
        amm: AMM {
            base_asset_reserve: 512295081967,
            quote_asset_reserve: 488 * AMM_RESERVE_PRECISION,
            sqrt_k: 500 * AMM_RESERVE_PRECISION,
            peg_multiplier: 50000000,
            base_asset_amount_with_amm: -12295081967, //~12
            total_fee_minus_distributions: ((QUOTE_PRECISION * 99999) as i128),

            ..AMM::default()
        },
        oracle: oracle_price_key,
        oracle_source: crate::state::oracle::OracleSource::PythLazer,
        base_asset_amount_long: 12295081967,
        base_asset_amount_short: -12295081967 * 2,
        fee_ledger: FeeLedger {
            total_exchange_fee: QUOTE_PRECISION / 2,
            ..FeeLedger::default()
        },
        market_stats: MarketStats {
            funding_period: 3600,
            last_mark_price_twap: 50 * PRICE_PRECISION_U64,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (49 * PRICE_PRECISION) as i64,

                ..HistoricalOracleData::default()
            },
            ..MarketStats::default()
        },
        ..PerpMarket::default()
    };

    let res1 = market
        .get_max_price_divergence_for_funding_rate(
            market
                .market_stats
                .historical_oracle_data
                .last_oracle_price_twap,
        )
        .unwrap();
    assert_eq!(res1, 4900000);
    market.contract_tier = ContractTier::B;
    let res1 = market
        .get_max_price_divergence_for_funding_rate(
            market
                .market_stats
                .historical_oracle_data
                .last_oracle_price_twap,
        )
        .unwrap();
    assert_eq!(res1, 1484848);

    let did_succeed = update_funding_rate(
        0,
        &mut market,
        &mut oracle_map,
        now,
        slot,
        &state.oracle_guard_rails,
        false,
        None,
    )
    .unwrap();

    assert!(!did_succeed);
}

/// OtterSec #109 — the funding crank must evaluate its own oracle gate against
/// the TWAP as it stood *before* the instruction's own refresh.
///
/// `handle_update_funding_rate` used to call the composed
/// `update_oracle_derived_stats` (which advances `last_oracle_price_twap` /
/// `last_oracle_price_twap_5min`) and only then call `update_funding_rate`, whose
/// gate `oracle::block_operation` reads those same two fields. The refresh drags
/// the TWAP toward the live price, so a genuinely `TooVolatile` oracle cleared its
/// own gate inside the same instruction and went on to mutate cumulative funding.
/// It now calls the TWAP-free `refresh_amm_quote_state` instead.
#[test]
fn funding_gate_not_cleared_by_own_twap_refresh() {
    let now = 3600_i64;
    let slot = 1_u64;

    let state = State {
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

    // Live oracle 51, mark 12, and a funding-period TWAP still sitting at a stale
    // 8. 51 / 8 == 6 > too_volatile_ratio(5), so the oracle is `TooVolatile` and
    // funding must not update. The 5-min TWAP sits on the mark so the divergence
    // half of the gate stays clear and only the volatility term is in play.
    let mut oracle_price = get_pyth_price(51, 6);
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

    let market = PerpMarket {
        market_index: 0,
        status: crate::state::market_status::MarketStatus::Active,
        // ContractTier::C => a 50% sanitize band, wide enough for one refresh to
        // carry the TWAP from 8 to 12 and clear the ratio.
        contract_tier: ContractTier::C,
        amm: AMM {
            base_asset_reserve: 500 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 500 * AMM_RESERVE_PRECISION,
            sqrt_k: 500 * AMM_RESERVE_PRECISION,
            peg_multiplier: 12_000_000,
            ..AMM::default()
        },
        oracle: oracle_price_key,
        oracle_source: crate::state::oracle::OracleSource::PythLazer,
        market_stats: MarketStats {
            funding_period: 3600,
            last_mark_price_twap: 12 * PRICE_PRECISION_U64,
            last_mark_price_twap_5min: 12 * PRICE_PRECISION_U64,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price: (51 * PRICE_PRECISION) as i64,
                last_oracle_price_twap: (8 * PRICE_PRECISION) as i64,
                last_oracle_price_twap_5min: (12 * PRICE_PRECISION) as i64,
                ..HistoricalOracleData::default()
            },
            ..MarketStats::default()
        },
        ..PerpMarket::default()
    };

    let reserve_price = market.amm.reserve_price().unwrap();
    assert_eq!(reserve_price, 12 * PRICE_PRECISION_U64);

    let oracle_price_data = *oracle_map.get_price_data(&market.oracle_id()).unwrap();
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
        .unwrap();
    let validity = crate::vlp::amm::refresh::compute_amm_refresh_validity(
        &market,
        &mm_oracle_price_data,
        &state,
        slot,
    )
    .unwrap();
    assert_eq!(validity, Some(OracleValidity::TooVolatile));

    // Baseline: the gate blocks this oracle.
    assert!(block_operation(
        &market,
        &oracle_price_data,
        &state.oracle_guard_rails,
        reserve_price,
        slot,
        SlotClock::baseline(),
    )
    .unwrap());

    // Pre-fix ordering: the crank's own refresh moves the TWAP 8 -> 12, and the
    // very same oracle now passes the very same gate.
    let mut unfixed = market;
    unfixed
        .update_oracle_derived_stats(&mm_oracle_price_data, validity, now, slot)
        .unwrap();
    assert_eq!(
        unfixed
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap,
        (12 * PRICE_PRECISION) as i64
    );
    assert!(
        !block_operation(
            &unfixed,
            &oracle_price_data,
            &state.oracle_guard_rails,
            unfixed.amm.reserve_price().unwrap(),
            slot,
            SlotClock::baseline(),
        )
        .unwrap(),
        "the pre-fix ordering is expected to clear its own gate — if this trips, \
         the test fixture no longer reproduces #109"
    );

    // Fixed ordering: the TWAP-free half leaves both gate inputs untouched, so a
    // too-volatile oracle stays blocked.
    let mut fixed = market;
    fixed
        .refresh_amm_quote_state(&mm_oracle_price_data, validity, slot)
        .unwrap();
    let historical = fixed.market_stats.historical_oracle_data;
    assert_eq!(
        historical.last_oracle_price_twap,
        (8 * PRICE_PRECISION) as i64
    );
    assert_eq!(
        historical.last_oracle_price_twap_5min,
        (12 * PRICE_PRECISION) as i64
    );
    assert_eq!(historical.last_oracle_price_twap_ts, 0);
    assert!(block_operation(
        &fixed,
        &oracle_price_data,
        &state.oracle_guard_rails,
        fixed.amm.reserve_price().unwrap(),
        slot,
        SlotClock::baseline(),
    )
    .unwrap());
    // ...and it still performed its own half: `last_oracle_valid` is stamped
    // (false here, since a TooVolatile oracle is not valid for an AMM fill).
    assert!(!fixed.market_stats.last_oracle_valid);
}

#[test]
fn unsettled_funding_pnl() {
    let mut now = 0_i64;
    let mut slot = 0_u64;

    let state = State {
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

    let mut oracle_price = get_pyth_price(51, 6);
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
        market_index: 0,
        amm: AMM {
            base_asset_reserve: 512295081967,
            quote_asset_reserve: 488 * AMM_RESERVE_PRECISION,
            sqrt_k: 500 * AMM_RESERVE_PRECISION,
            peg_multiplier: 50000000,
            base_asset_amount_with_amm: -12295081967 + -((AMM_RESERVE_PRECISION * 500) as i128), //~ 12 - 500
            total_fee_minus_distributions: ((QUOTE_PRECISION * 99999) as i128),

            ..AMM::default()
        },
        oracle: oracle_price_key,
        oracle_source: crate::state::oracle::OracleSource::PythLazer,
        base_asset_amount_long: 12295081967,
        base_asset_amount_short: -12295081967 * 2,
        fee_ledger: FeeLedger {
            total_exchange_fee: QUOTE_PRECISION / 2,
            ..FeeLedger::default()
        },
        market_stats: MarketStats {
            funding_period: 3600,
            last_mark_price_twap: 50 * PRICE_PRECISION_U64,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: (49 * PRICE_PRECISION) as i64,

                ..HistoricalOracleData::default()
            },
            ..MarketStats::default()
        },
        ..PerpMarket::default()
    };
    assert_eq!(market.amm.reserve_price().unwrap(), 47628800);
    assert_eq!(market.net_unsettled_funding_pnl, 0);

    let time_until_next_update = on_the_hour_update(
        now,
        market.last_funding_rate_ts,
        market.market_stats.funding_period,
    )
    .unwrap();

    assert_eq!(time_until_next_update, 3600);
    let time_until_next_update = on_the_hour_update(
        now + 3600,
        market.last_funding_rate_ts,
        market.market_stats.funding_period,
    )
    .unwrap();
    let oracle_price_data = oracle_map.get_price_data(&market.oracle_id()).unwrap();
    let mm_oracle_price_data = MMOraclePriceData::new(
        oracle_price_data.price,
        oracle_price_data.delay + 1,
        0,
        OracleValidity::default(),
        *oracle_price_data,
    )
    .unwrap();

    assert_eq!(time_until_next_update, 0);
    let block_funding_rate_update = block_operation(
        &market,
        oracle_price_data,
        &state.oracle_guard_rails,
        market.amm.reserve_price().unwrap(),
        slot,
        SlotClock::baseline(),
    )
    .unwrap();
    assert_eq!(block_funding_rate_update, true);
    assert_eq!(market.amm.last_update_slot, slot);

    now += 3600;
    slot += 3600 * 2;

    let cost = _update_amm(&mut market, &mm_oracle_price_data, &state, now, slot).unwrap();
    assert_eq!(cost, 0);
    assert_eq!(market.amm.last_update_slot, slot);
    assert_eq!(market.market_stats.last_mark_price_twap, 50000000);
    assert_eq!(
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap,
        51000000
    );
    // oracle twap > mark, expect negative funding

    let block_funding_rate_update = block_operation(
        &market,
        oracle_price_data,
        &state.oracle_guard_rails,
        market.amm.reserve_price().unwrap(),
        slot,
        SlotClock::baseline(),
    )
    .unwrap();
    assert_eq!(block_funding_rate_update, false);
    assert_eq!(market.amm.total_fee_minus_distributions, 99999000000);

    let did_succeed = update_funding_rate(
        0,
        &mut market,
        &mut oracle_map,
        now,
        slot,
        &state.oracle_guard_rails,
        false,
        None,
    )
    .unwrap();
    assert!(did_succeed);
    assert_eq!(market.market_stats.last_mark_price_twap, 47629736);
    assert!(market.market_stats.last_mark_price_twap > market.amm.reserve_price().unwrap());

    assert_eq!(
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap,
        51000000
    );

    assert_eq!(market.cumulative_funding_rate_long, -138727625); // negative funding
    assert_eq!(market.cumulative_funding_rate_short, -138727625);
    assert_eq!(market.last_funding_rate, -138727625);
    assert_eq!(
        market.market_stats.last_24h_avg_funding_rate,
        -138727625 / 24 + 1
    );
    assert_eq!(market.last_funding_rate_ts, now);
    assert_eq!(market.amm.net_revenue_since_last_funding, 0); // back to 0
                                                              // AMM-as-user migration: the AMM now settles its funding from cum-rate
                                                              // deltas decomposed across the long and short sides (each side reads
                                                              // `base_asset_amount_long/short`), instead of from
                                                              // `base_asset_amount_with_amm`. This test fixture sets
                                                              // `base_asset_amount_with_amm` artificially divergent from
                                                              // `base_long + base_short`; the new math reflects only the user-side
                                                              // imbalance (~$1.72 gain), not the inflated `with_amm` value (~$70.61
                                                              // gain under the legacy single-net-position math).
    assert_eq!(market.amm.total_fee_minus_distributions, 100000705667);
    assert_eq!(market.amm.total_fee, 0);

    assert_ne!(market.net_unsettled_funding_pnl, 0); // important: imbalanced market adds funding rev
                                                     // net_unsettled_funding_pnl uses the math function's third return
                                                     // (uncapped, derived from `base_asset_amount_with_amm`), so it still
                                                     // reflects the legacy single-net-position value — the AMM-as-user
                                                     // migration only changed how the AMM books its own settlement, not
                                                     // this aggregate.
    assert_eq!(market.net_unsettled_funding_pnl, -71069480);
}

// The funding premium must leave the dead zone continuously: crossing the
// threshold should nudge the premium by the ramp, not snap on the full
// threshold the way the old hard cliff did.
#[test]
fn funding_premium_continuous_across_dead_zone() {
    let oracle_twap: i64 = 100 * PRICE_PRECISION_U64 as i64;
    let clamp_threshold = oracle_twap * 5 / BPS_PRECISION as i64; // 5bps as a price
    let ramp_slope = PERCENTAGE_PRECISION_U32; // 1.0x
    let offset: i64 = 12_345; // arbitrary baseline carry

    // at the edge of the band: still noise, offset only
    let at_floor =
        calculate_funding_premium_with_offset(clamp_threshold, clamp_threshold, ramp_slope, offset)
            .unwrap();
    assert_eq!(at_floor, offset);

    // one tick past the band: premium turns on by the ramp (1 tick), not by
    // the whole threshold. this single-tick step is the continuity guarantee
    let just_above = calculate_funding_premium_with_offset(
        clamp_threshold + 1,
        clamp_threshold,
        ramp_slope,
        offset,
    )
    .unwrap();
    assert_eq!(just_above - at_floor, 1);
    assert!(just_above - at_floor < clamp_threshold); // a hard cliff would jump by ~threshold

    // symmetric on the short side
    let just_below = calculate_funding_premium_with_offset(
        -(clamp_threshold + 1),
        clamp_threshold,
        ramp_slope,
        offset,
    )
    .unwrap();
    assert_eq!(just_below - at_floor, -1);

    // well outside the band the premium is the spread shrunk by the threshold
    let far = calculate_funding_premium_with_offset(
        2 * clamp_threshold,
        clamp_threshold,
        ramp_slope,
        offset,
    )
    .unwrap();
    assert_eq!(far - offset, clamp_threshold);
}

/// Property tests for `calculate_amm_funding_payment` — the AMM-as-user
/// settlement math. Locks the invariants the cum-rate-delta decomposition
/// has to satisfy so the migration to "AMM is just another user position"
/// can't silently drift.
mod amm_funding_payment {
    use {
        super::*,
        crate::math::constants::{
            AMM_TO_QUOTE_PRECISION_RATIO, FUNDING_RATE_BUFFER, PRICE_PRECISION,
        },
    };

    /// Zero deltas → zero payment, regardless of position sizes.
    #[test]
    fn zero_delta_pays_nothing() {
        let payment = calculate_amm_funding_payment(
            1_000_000_000_000, // base_long
            -500_000_000_000,  // base_short
            12345,             // cum_long
            6789,              // cum_short
            12345,             // last_cum_long (same → delta 0)
            6789,              // last_cum_short (same → delta 0)
        )
        .unwrap();
        assert_eq!(payment, 0);
    }

    /// Zero user positions on both sides → zero payment, regardless of
    /// deltas. The AMM has no exposure to settle.
    #[test]
    fn zero_positions_pays_nothing() {
        let payment = calculate_amm_funding_payment(0, 0, 1_000_000, -500_000, 0, 0).unwrap();
        assert_eq!(payment, 0);
    }

    /// Balanced book (`base_long == -base_short`) + symmetric cum rates →
    /// AMM nets out to zero. Sanity check that the long-side and
    /// short-side contributions cancel when there's no imbalance.
    #[test]
    fn balanced_book_symmetric_rates_pays_nothing() {
        let payment = calculate_amm_funding_payment(
            1_000_000_000_000,
            -1_000_000_000_000,
            10_000,
            10_000,
            0,
            0,
        )
        .unwrap();
        assert_eq!(payment, 0);
    }

    /// Long-biased book + positive funding rate → longs pay shorts;
    /// AMM (net short on the imbalance) earns. Sign check.
    #[test]
    fn long_bias_positive_rate_amm_earns() {
        let payment = calculate_amm_funding_payment(
            2_000_000_000_000,  // base_long (more longs)
            -1_000_000_000_000, // base_short
            10_000,             // cum_long delta = +10000
            10_000,             // cum_short delta = +10000 (symmetric)
            0,
            0,
        )
        .unwrap();
        assert!(payment > 0, "AMM should earn when long-biased and rate > 0");
    }

    /// Short-biased book + positive funding rate → AMM (net long on the
    /// imbalance) pays.
    #[test]
    fn short_bias_positive_rate_amm_pays() {
        let payment = calculate_amm_funding_payment(
            1_000_000_000_000,
            -2_000_000_000_000, // more shorts
            10_000,
            10_000,
            0,
            0,
        )
        .unwrap();
        assert!(payment < 0, "AMM should pay when short-biased and rate > 0");
    }

    /// Reversing the sign of the funding rate reverses the AMM's payment.
    #[test]
    fn rate_sign_inverts_payment() {
        let pos = calculate_amm_funding_payment(
            2_000_000_000_000,
            -1_000_000_000_000,
            10_000,
            10_000,
            0,
            0,
        )
        .unwrap();
        let neg = calculate_amm_funding_payment(
            2_000_000_000_000,
            -1_000_000_000_000,
            -10_000,
            -10_000,
            0,
            0,
        )
        .unwrap();
        assert_eq!(pos, -neg);
    }

    /// Calling `calculate_amm_funding_payment` twice with the same `last_*`
    /// is the same as calling once. Concretely: settling a single funding
    /// period against the current cum rates is idempotent if last_* are
    /// re-read after each call.
    #[test]
    fn settle_then_advance_last_cum_rate_is_consistent() {
        let base_long = 1_500_000_000_000;
        let base_short = -800_000_000_000;
        let cum_long = 5_000;
        let cum_short = 3_000;

        // Single settle from a zero baseline.
        let one_shot =
            calculate_amm_funding_payment(base_long, base_short, cum_long, cum_short, 0, 0)
                .unwrap();

        // Two-step settle: cum rates advance halfway, then to full. The
        // sum should match the single settle (math is linear in delta).
        let step_a = calculate_amm_funding_payment(
            base_long, base_short, 2_000, // partial cum_long
            1_000, // partial cum_short
            0, 0,
        )
        .unwrap();
        let step_b = calculate_amm_funding_payment(
            base_long, base_short, cum_long, cum_short,
            2_000, // last_cum_long = previous cum_long
            1_000, // last_cum_short = previous cum_short
        )
        .unwrap();

        // Allow ±1 quote unit of rounding because magnitude rounding in
        // `_calculate_funding_payment` happens per-call.
        assert!(
            (step_a + step_b - one_shot).abs() <= 1,
            "two-step settle ({} + {} = {}) should match one-shot ({})",
            step_a,
            step_b,
            step_a + step_b,
            one_shot
        );
    }

    /// Uncapped case (rate_long == rate_short): the AMM's payment via two-
    /// side decomp equals the legacy single-net-position math
    /// `-calculate_funding_payment_in_quote_precision(rate, B)` where
    /// `B = base_long + base_short`. Locks the equivalence with master in
    /// the no-cap path.
    #[test]
    fn uncapped_matches_legacy_single_position_math() {
        // Fixtures over a few combinations: long-bias / short-bias /
        // balanced × positive / negative rate.
        let fixtures: &[(i128, i128, i128)] = &[
            // (base_long, base_short, rate)
            (
                2_000_000_000_000,
                -1_000_000_000_000,
                FUNDING_RATE_BUFFER as i128 / 10,
            ),
            (
                1_000_000_000_000,
                -2_000_000_000_000,
                FUNDING_RATE_BUFFER as i128 / 10,
            ),
            (
                2_000_000_000_000,
                -1_000_000_000_000,
                -(FUNDING_RATE_BUFFER as i128 / 10),
            ),
            (
                1_500_000_000_000,
                -1_500_000_000_000, // balanced
                FUNDING_RATE_BUFFER as i128 / 20,
            ),
        ];

        for (i, &(base_long, base_short, rate)) in fixtures.iter().enumerate() {
            // Two-side decomp: cum_long delta == cum_short delta == rate
            // (uncapped → same delta on both sides).
            let amm_payment =
                calculate_amm_funding_payment(base_long, base_short, rate, rate, 0, 0).unwrap();

            // Legacy: -calculate_funding_payment(rate, base_long + base_short).
            let net_pos = base_long + base_short;
            let legacy_owe = calculate_funding_payment_in_quote_precision(rate, net_pos).unwrap();
            let legacy_amm_pnl = -legacy_owe;

            // Magnitude must match within a small rounding tolerance —
            // the two-side decomp adds two partial payments, each
            // independently rounded.
            let _ = (PRICE_PRECISION, AMM_TO_QUOTE_PRECISION_RATIO); // silence
            assert!(
                (amm_payment - legacy_amm_pnl).abs() <= 2,
                "fixture {}: two-side decomp ({}) should match legacy single-pos math ({})",
                i,
                amm_payment,
                legacy_amm_pnl
            );
        }
    }

    /// Capped case: capping only fires when the AMM would have paid
    /// (uncapped imbalance < 0). The cap reduces what users on the
    /// receiving side get, which moves the AMM's payment toward zero —
    /// in extreme caps it can even flip the sign (longs pay full, shorts
    /// receive nothing, AMM nets positive). The invariant: `capped >
    /// uncapped` when `uncapped < 0`.
    #[test]
    fn capped_short_side_reduces_amm_debt() {
        let base_long = 1_000_000_000_000;
        let base_short = -2_000_000_000_000; // more shorts → uncapped, AMM pays
        let full_rate = FUNDING_RATE_BUFFER as i128 / 10;
        let capped_rate = full_rate / 3; // shorts receive only 1/3 of what longs pay

        let uncapped =
            calculate_amm_funding_payment(base_long, base_short, full_rate, full_rate, 0, 0)
                .unwrap();
        let capped =
            calculate_amm_funding_payment(base_long, base_short, full_rate, capped_rate, 0, 0)
                .unwrap();

        assert!(uncapped < 0, "AMM should owe in uncapped scenario");
        assert!(
            capped > uncapped,
            "Capping the short-side rate should reduce AMM's debt ({} → {})",
            uncapped,
            capped
        );
    }
}

/// A market that resumes after a long gap must not price funding off one quote.
///
/// `calculate_new_twap` weights the incoming sample by the time since the last mark
/// TWAP write, so past a few funding periods the next sample replaces the TWAP
/// outright. That gap is longest after a funding pause, because both funding cranks
/// reject while the pause is set, and funding fires on the first crank after it lifts.
/// `MarketStats::update_mark_twap` re-seeds the mark TWAPs from the oracle TWAP
/// instead, so the resuming market pays what a market with no premium pays.
#[test]
fn funding_after_a_long_mark_twap_gap_charges_the_offset_alone() {
    let now = 1_662_800_000_i64 + 4 * ONE_HOUR;
    let slot = 1_u64;

    let state = State {
        oracle_guard_rails: OracleGuardRails {
            validity: ValidityGuardRails {
                slots_before_stale_for_amm: legacy_slot_duration_i64(10),
                slots_before_stale_for_margin: legacy_slot_duration_i64(120),
                confidence_interval_max_size: 1000,
                too_volatile_ratio: 5,
            },
            ..OracleGuardRails::default()
        },
        ..State::default()
    };

    let mut oracle_price = get_pyth_price(1, 6);
    let oracle_price_key =
        Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
    create_anchor_account_info!(
        oracle_price,
        &oracle_price_key,
        PythLazerOracle,
        oracle_account_info
    );

    // `market` carries a 10% mark premium it has not written for four funding periods.
    // `control` holds no premium and is current, so it charges the offset alone by
    // construction. The two must reach the same funding rate.
    // The AMM quotes 2% above the oracle, so the sample this update would otherwise
    // blend in is distinguishable from the oracle TWAP the re-seed writes.
    let amm = AMM {
        peg_multiplier: PEG_PRECISION * 102 / 100,
        ..AMM::default_test()
    };

    let market_of = |last_mark_price_twap: u64, last_mark_price_twap_ts: i64| PerpMarket {
        market_index: 0,
        amm,
        oracle: oracle_price_key,
        oracle_source: crate::state::oracle::OracleSource::PythLazer,
        market_stats: MarketStats {
            funding_period: ONE_HOUR,
            last_mark_price_twap,
            last_mark_price_twap_5min: last_mark_price_twap,
            last_bid_price_twap: last_mark_price_twap,
            last_ask_price_twap: last_mark_price_twap,
            last_mark_price_twap_ts,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price: PRICE_PRECISION_I64,
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                last_oracle_price_twap_ts: now - 60,
                ..HistoricalOracleData::default()
            },
            ..MarketStats::default()
        },
        ..PerpMarket::default()
    };

    let mut market = market_of(110 * PRICE_PRECISION_U64 / 100, now - 4 * ONE_HOUR);
    let mut control = market_of(PRICE_PRECISION_U64, now);

    // Guards against a vacuous pass: the quote the re-seed discards must differ from
    // the oracle TWAP it seeds onto.
    let amm_quote = market.amm.reserve_price().unwrap();
    assert_ne!(amm_quote, PRICE_PRECISION_U64);

    let mut oracle_map =
        OracleMap::load_one(&oracle_account_info, slot, SlotClock::baseline(), None).unwrap();
    assert!(update_funding_rate(
        0,
        &mut market,
        &mut oracle_map,
        now,
        slot,
        &state.oracle_guard_rails,
        false,
        None,
    )
    .unwrap());

    let mut oracle_map =
        OracleMap::load_one(&oracle_account_info, slot, SlotClock::baseline(), None).unwrap();
    assert!(update_funding_rate(
        0,
        &mut control,
        &mut oracle_map,
        now,
        slot,
        &state.oracle_guard_rails,
        false,
        None,
    )
    .unwrap());

    // The stale premium is discarded, not carried into the first period after the gap.
    assert_eq!(
        market.market_stats.last_mark_price_twap,
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap as u64
    );
    assert_eq!(market.market_stats.last_mark_price_twap_ts, now);
    assert_ne!(market.market_stats.last_mark_price_twap, amm_quote);

    assert_eq!(
        market.cumulative_funding_rate_long,
        control.cumulative_funding_rate_long
    );
    assert_eq!(
        market.cumulative_funding_rate_short,
        control.cumulative_funding_rate_short
    );
    assert_eq!(market.last_funding_rate, control.last_funding_rate);
}
