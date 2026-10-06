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
    slot_clock: SlotClock,
) -> SdkResult<PerpMarket> {
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
