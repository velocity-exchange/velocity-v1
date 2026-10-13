//! Local mirror of the program's fill-time AMM quote preparation.
//!
//! At fill time the program does not quote the AMM from its stored account state:
//! `AmmQuoter::setup` first projects the curve onto the (safe MM) oracle
//! (`project_post_refresh_scalar`, slot-idempotent) and then refreshes the cached
//! spread state (`update_amm_quote_state` — long/short spread, reference price
//! offset, ask/bid reserves) before `best_price` reads bid/ask. Crossing checks
//! against the raw cached account therefore mis-price the vAMM quote whenever the
//! oracle has moved since the last on-chain refresh — both in the curve *and* in
//! the spread. A stale-tight local spread makes the bot see phantom crosses and
//! send fills that no-op on-chain with "taker does not cross amm".
//!
//! Before `setup`, `fill_perp_order` refreshes the market's oracle-derived stats (oracle
//! TWAPs, confidence and standard deviation) from the same oracle, and the spread refresh
//! reads those stats. [`project_perp_market_for_quoting`] reproduces that refresh and the
//! `setup` sequence, using the program's own functions, so local bid/ask matches what
//! `determine_perp_fulfillment_methods` will compare against on-chain. The
//! incident-shaped regression test in `dlob/tests.rs`
//! (`dlob_vamm_taker_candidate_requires_fill_path_quote`) pins the filler's
//! vAMM-cross decisions against this projection.

use program::{
    math::time::SlotClock,
    state::{oracle::OraclePriceData, perp_market::PerpMarket, state::ValidityGuardRails},
    vlp::amm::{
        math::{
            repeg::{project_post_refresh_scalar, ProjectionInputs},
            spread::update_amm_quote_state,
        },
        refresh::compute_amm_refresh_validity_with_guard_rails,
    },
};

use crate::types::{SdkError, SdkResult};

/// Project a copy of `perp_market` to the state the program's fill path quotes the
/// AMM against at `slot`, mirroring `fill_perp_order` then `AmmQuoter::setup`:
///
/// 1. oracle-derived stats refresh (`PerpMarket::update_oracle_derived_stats` at unix
///    time `now`), which `fill_perp_order` runs before quoting;
/// 2. curve projection (`project_post_refresh_scalar`) on the refreshed market, skipped
///    when `amm.last_update_slot >= slot` (a crank already projected this slot) — same
///    slot-idempotency as the program;
/// 3. cached spread-state refresh (`update_amm_quote_state`) from the refreshed stats.
///
/// `exchange_oracle` is the complete exchange oracle observation the program will see,
/// including the price, confidence, delay and sequence of an update the same tx posts.
///
/// Do not gate AMM fills on the result: the program checks its AMM gates on the market
/// before this refresh, and the projection moves the fee and revenue counters they read.
///
/// Quote off the result with `amm.ask_price(reserve_price, long_spread,
/// reference_price_offset)` / `bid_price(...)` — the same reads as
/// `AmmQuoter::best_price`.
pub fn project_perp_market_for_quoting(
    mut perp_market: PerpMarket,
    exchange_oracle: OraclePriceData,
    guard_rails: &ValidityGuardRails,
    slot: u64,
    slot_clock: SlotClock,
    now: i64,
) -> SdkResult<PerpMarket> {
    // `fill_perp_order` refreshes the oracle-derived stats before any quote
    let stats_mm_oracle = perp_market
        .get_mm_oracle_price_data(exchange_oracle, slot, guard_rails, slot_clock)
        .map_err(|e| SdkError::Anchor(Box::new(e.into())))?;
    let stats_validity = compute_amm_refresh_validity_with_guard_rails(
        &perp_market,
        &stats_mm_oracle,
        guard_rails,
        slot,
        slot_clock,
    )
    .map_err(|e| SdkError::Anchor(Box::new(e.into())))?;
    perp_market
        .update_oracle_derived_stats(&stats_mm_oracle, stats_validity, now, slot)
        .map_err(|e| SdkError::Anchor(Box::new(e.into())))?;

    // the quote setup then reads the oracle again, against the refreshed stats
    let mm_oracle = perp_market
        .get_mm_oracle_price_data(exchange_oracle, slot, guard_rails, slot_clock)
        .map_err(|e| SdkError::Anchor(Box::new(e.into())))?;
    let validity = compute_amm_refresh_validity_with_guard_rails(
        &perp_market,
        &mm_oracle,
        guard_rails,
        slot,
        slot_clock,
    )
    .map_err(|e| SdkError::Anchor(Box::new(e.into())))?;

    if perp_market.amm.last_update_slot < slot {
        let projection_inputs = ProjectionInputs {
            market_status: perp_market.status,
            market_config: perp_market.market_config,
            min_order_size: perp_market.market_stats.min_order_size,
        };
        project_post_refresh_scalar(&perp_market.amm, &projection_inputs, &mm_oracle, validity)
            .and_then(|projection| projection.apply_to(&mut perp_market.amm))
            .map_err(|e| SdkError::Anchor(Box::new(e.into())))?;
    }

    let market_stats = perp_market.market_stats;
    let reserve_price = perp_market
        .amm
        .reserve_price()
        .map_err(|e| SdkError::Anchor(Box::new(e.into())))?;
    update_amm_quote_state(
        &mut perp_market.amm,
        &market_stats,
        &mm_oracle,
        reserve_price,
        slot,
    )
    .map_err(|e| SdkError::Anchor(Box::new(e.into())))?;

    Ok(perp_market)
}

/// Replica of the program's `PerpMarket::default_btc_test` (cfg(test)-gated
/// there, so not importable): BTC-ish market at peg $19,400, short 1 BTC of
/// AMM inventory, 2.5bps base / 9.75bps max spread, live curve updates.
///
/// Used by the DLOB regression tests (`dlob/tests.rs`) that exercise the
/// filler's vAMM-cross decisions.
#[cfg(test)]
pub(crate) fn btc_market_fixture() -> PerpMarket {
    use program::{
        state::{
            market_status::MarketStatus, oracle::HistoricalOracleData, perp_market::MarketStats,
        },
        vlp::amm::state::AMM,
    };

    const AMM_RESERVE_PRECISION: u128 = 1_000_000_000;
    const PRICE_PRECISION_I64: i64 = 1_000_000;
    const MAX_CONCENTRATION_COEFFICIENT: u128 = 1_414_200;

    let amm = AMM {
        base_asset_reserve: 65 * AMM_RESERVE_PRECISION,
        quote_asset_reserve: 63_015_384_615,
        terminal_quote_asset_reserve: 64 * AMM_RESERVE_PRECISION,
        sqrt_k: 64 * AMM_RESERVE_PRECISION,
        peg_multiplier: 19_400_000_000,
        concentration_coef: MAX_CONCENTRATION_COEFFICIENT,
        max_base_asset_reserve: 90 * AMM_RESERVE_PRECISION,
        min_base_asset_reserve: 45 * AMM_RESERVE_PRECISION,
        base_asset_amount_with_amm: -(AMM_RESERVE_PRECISION as i128),
        curve_update_intensity: 100,
        base_spread: 250,
        max_spread: 975,
        max_fill_reserve_fraction: 1,
        ..AMM::default()
    };
    PerpMarket {
        market_stats: MarketStats {
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price: 19_400 * PRICE_PRECISION_I64,
                last_oracle_price_twap: 19_400 * PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: 19_400 * PRICE_PRECISION_I64,
                last_oracle_price_twap_ts: 1_662_800_000_i64,
                ..HistoricalOracleData::default()
            },
            last_mark_price_twap_ts: 1_662_800_000,
            mark_std: 1_000_000,
            last_oracle_valid: true,
            funding_period: 3600,
            ..MarketStats::default()
        },
        amm,
        order_step_size: 1,
        order_tick_size: 1,
        margin_ratio_initial: 1000,
        margin_ratio_maintenance: 500,
        status: MarketStatus::Initialized,
        ..PerpMarket::default()
    }
}

/// Mainnet-shaped oracle validity guard rails for tests.
#[cfg(test)]
pub(crate) fn validity_guard_rails_fixture() -> ValidityGuardRails {
    use program::math::time::legacy_slot_duration_i64;
    ValidityGuardRails {
        slots_before_stale_for_amm: legacy_slot_duration_i64(10),
        slots_before_stale_for_margin: legacy_slot_duration_i64(120),
        confidence_interval_max_size: 20_000,
        too_volatile_ratio: 5,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        btc_market_fixture, project_perp_market_for_quoting, validity_guard_rails_fixture,
    };
    use program::{math::time::SlotClock, state::oracle::OraclePriceData};

    #[test]
    fn projection_refreshes_oracle_stats_before_quoting() {
        // `fill_perp_order` updates the oracle TWAPs before the quote setup reads them
        let market = btc_market_fixture();
        let twap_ts = market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap_ts;
        let moved = OraclePriceData {
            price: 19_600 * 1_000_000,
            confidence: 1_000,
            delay: 0,
            has_sufficient_number_of_data_points: true,
            sequence_id: None,
        };
        let projected = project_perp_market_for_quoting(
            market,
            moved,
            &validity_guard_rails_fixture(),
            100,
            SlotClock::baseline(),
            twap_ts + 60,
        )
        .unwrap();

        let before = market.market_stats.historical_oracle_data;
        let after = projected.market_stats.historical_oracle_data;
        assert_eq!(after.last_oracle_price_twap_ts, twap_ts + 60);
        assert!(after.last_oracle_price_twap > before.last_oracle_price_twap);
    }
}
