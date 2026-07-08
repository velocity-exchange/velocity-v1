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
//! [`project_perp_market_for_quoting`] reproduces the `setup` sequence exactly,
//! using the program's own functions, so local bid/ask matches what
//! `determine_perp_fulfillment_methods` will compare against on-chain. The parity
//! test below pins this against the real `AmmQuoter::setup` — if the program's
//! quote-prep gains another step, the test fails rather than the filler spamming
//! no-op fills.

use program::{
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
/// AMM against at `slot`, mirroring `AmmQuoter::setup`:
///
/// 1. curve projection (`project_post_refresh_scalar`), skipped when
///    `amm.last_update_slot >= slot` (a crank already projected this slot) — same
///    slot-idempotency as the program;
/// 2. cached spread-state refresh (`update_amm_quote_state`).
///
/// `exchange_oracle` is the exchange oracle reading the program will see; callers
/// that post a fresher oracle update in the same tx as the fill should override its
/// `price`/`delay` accordingly.
///
/// Quote off the result with `amm.ask_price(reserve_price, long_spread,
/// reference_price_offset)` / `bid_price(...)` — the same reads as
/// `AmmQuoter::best_price`.
pub fn project_perp_market_for_quoting(
    mut perp_market: PerpMarket,
    exchange_oracle: OraclePriceData,
    guard_rails: &ValidityGuardRails,
    slot: u64,
) -> SdkResult<PerpMarket> {
    let mm_oracle = perp_market
        .get_mm_oracle_price_data(exchange_oracle, slot, guard_rails)
        .map_err(|e| SdkError::Anchor(Box::new(e.into())))?;
    let validity =
        compute_amm_refresh_validity_with_guard_rails(&perp_market, &mm_oracle, guard_rails)
            .map_err(|e| SdkError::Anchor(Box::new(e.into())))?;

    if perp_market.amm.last_update_slot < slot {
        let projection_inputs = ProjectionInputs {
            market_status: perp_market.status,
            market_config: perp_market.market_config,
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
/// Shared by the parity tests below and the DLOB regression tests
/// (`dlob/tests.rs`) that exercise the filler's vAMM-cross decisions.
#[cfg(test)]
pub(crate) fn btc_market_fixture() -> PerpMarket {
    use program::state::{
        market_status::MarketStatus, oracle::HistoricalOracleData, perp_market::MarketStats,
    };
    use program::vlp::amm::state::AMM;

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
    ValidityGuardRails {
        slots_before_stale_for_amm: 10,
        slots_before_stale_for_margin: 120,
        confidence_interval_max_size: 20_000,
        too_volatile_ratio: 5,
    }
}

#[cfg(test)]
mod tests {
    use program::{
        controller::position::PositionDirection,
        state::quoter::{QuoteContext, Quoter},
        vlp::amm::quoter::AmmQuoter,
    };

    use super::*;

    const PRICE_PRECISION_I64: i64 = 1_000_000;

    fn btc_market() -> PerpMarket {
        btc_market_fixture()
    }

    fn guard_rails() -> ValidityGuardRails {
        validity_guard_rails_fixture()
    }

    /// Run the program's actual fill-path quote prep (`AmmQuoter::setup`) and
    /// return (bid, ask).
    fn program_fill_path_quote(
        market: &PerpMarket,
        exchange_oracle: &OraclePriceData,
        rails: &ValidityGuardRails,
        slot: u64,
    ) -> (u64, u64) {
        let mm_oracle = market
            .get_mm_oracle_price_data(*exchange_oracle, slot, rails)
            .unwrap();
        let validity =
            compute_amm_refresh_validity_with_guard_rails(market, &mm_oracle, rails).unwrap();
        let ctx = QuoteContext {
            stats: &market.market_stats,
            oracle: exchange_oracle,
            mm_oracle: Some(&mm_oracle),
            oracle_validity: validity,
            fee_budget: 0,
            tick: market.order_tick_size,
            step_size: market.order_step_size,
            slot,
            base_precision: 1_000_000_000,
            market_status: market.status,
            market_config: market.market_config,
        };
        let mut amm = market.amm;
        let mut quoter = AmmQuoter { amm: &mut amm };
        quoter.setup(&ctx).unwrap();
        (
            quoter.best_price(&ctx, PositionDirection::Short).unwrap(),
            quoter.best_price(&ctx, PositionDirection::Long).unwrap(),
        )
    }

    /// Quote off a projected market the way the filler does.
    fn projected_quote(market: &PerpMarket) -> (u64, u64) {
        let reserve_price = market.amm.reserve_price().unwrap();
        (
            market
                .amm
                .bid_price(
                    reserve_price,
                    market.amm.short_spread,
                    market.amm.reference_price_offset,
                )
                .unwrap(),
            market
                .amm
                .ask_price(
                    reserve_price,
                    market.amm.long_spread,
                    market.amm.reference_price_offset,
                )
                .unwrap(),
        )
    }

    /// The projected market's bid/ask must match what the program's own
    /// `AmmQuoter::setup` + `best_price` produce at fill time — for a fresh
    /// oracle, a stale (for-AMM) oracle, and an already-projected slot.
    #[test]
    fn projected_quote_matches_program_fill_path() {
        let rails = guard_rails();
        let cases = [
            // (oracle price, delay, slot) — moved oracle, fresh
            (19_600 * PRICE_PRECISION_I64, 0_i64, 100_u64),
            // moved oracle, stale for AMM (delay > slots_before_stale_for_amm)
            (19_600 * PRICE_PRECISION_I64, 12, 100),
            // oracle below peg
            (19_150 * PRICE_PRECISION_I64, 1, 100),
        ];
        for (price, delay, slot) in cases {
            let market = btc_market();
            let exchange_oracle = OraclePriceData {
                price,
                confidence: 1_000,
                delay,
                has_sufficient_number_of_data_points: true,
                sequence_id: None,
            };
            let (want_bid, want_ask) =
                program_fill_path_quote(&market, &exchange_oracle, &rails, slot);
            let projected =
                project_perp_market_for_quoting(market, exchange_oracle, &rails, slot).unwrap();
            let (got_bid, got_ask) = projected_quote(&projected);
            assert_eq!(
                (got_bid, got_ask),
                (want_bid, want_ask),
                "projected quote diverged from program fill path (price={price} delay={delay} slot={slot})"
            );
        }
    }

    /// Slot-idempotency parity: when `amm.last_update_slot >= slot` the program
    /// skips the curve projection but still refreshes the spread state — the
    /// helper must do the same.
    #[test]
    fn projected_quote_matches_program_fill_path_when_curve_already_projected() {
        let rails = guard_rails();
        let slot = 100;
        let mut market = btc_market();
        market.amm.last_update_slot = slot; // crank already projected this slot
        let exchange_oracle = OraclePriceData {
            price: 19_600 * PRICE_PRECISION_I64,
            confidence: 1_000,
            delay: 0,
            has_sufficient_number_of_data_points: true,
            sequence_id: None,
        };
        let (want_bid, want_ask) = program_fill_path_quote(&market, &exchange_oracle, &rails, slot);
        let projected =
            project_perp_market_for_quoting(market, exchange_oracle, &rails, slot).unwrap();
        assert_eq!(projected_quote(&projected), (want_bid, want_ask));
    }

    /// Regression shape for the vamm_taker spam: quoting the *cached* account
    /// state (stale-tight spreads, un-projected curve) must NOT be trusted — with
    /// a moved oracle it diverges from the program's fill-time quote, which is
    /// exactly the phantom cross that produced repeated on-chain
    /// "taker does not cross amm" no-op fills. Also guards the parity test above
    /// against passing vacuously (i.e. proves setup actually changes the quote in
    /// this fixture).
    #[test]
    fn cached_account_quote_diverges_from_fill_path_when_oracle_moved() {
        let rails = guard_rails();
        let market = btc_market();
        let exchange_oracle = OraclePriceData {
            price: 19_600 * PRICE_PRECISION_I64,
            confidence: 1_000,
            delay: 0,
            has_sufficient_number_of_data_points: true,
            sequence_id: None,
        };
        let naive = projected_quote(&market); // raw cached account state
        let (fill_bid, fill_ask) = program_fill_path_quote(&market, &exchange_oracle, &rails, 100);
        assert_ne!(
            naive,
            (fill_bid, fill_ask),
            "fixture no longer exercises the projection: cached and fill-path quotes agree"
        );
    }
}
