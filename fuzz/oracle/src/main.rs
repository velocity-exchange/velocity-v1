//! P9 `oracle` — host-tier property + regression harnesses for Family VIII
//! (oracle validity / staleness / confidence gating).
//!
//! All harnesses are host-tier: they call velocity's pure oracle math directly
//! (no LiteSVM / `.so`), so Crucible reports `edges 0/0` — coverage is not the
//! point here, the invariant assertions are. A clean run = 0 crashes.
//!
//! Every harness calls a real velocity function and asserts on *that* function's
//! output — no harness models program logic in-harness.
//!
//! Layout:
//!   * `inv_*`   — always-on Family VIII invariants (expected to PASS on master).
//!   * `regr_*`  — pinned audit findings. `regr_268_prelaunch_delay` asserts the
//!                 *fixed* `get_prelaunch_price` invariant, so it FAILS on current
//!                 (pre-fix) master and flips green when the fix lands; the
//!                 `get_fallback_price` overflow guard already landed
//!                 (commit c9819d0bc) and PASSES.
//!
//! The #268 pyth-lazer confidence-floor and staleness/replay findings are NOT
//! here: that logic lives in the `post_pyth_lazer_oracle_update` instruction
//! handler (sig-verify + sysvars), so it must be driven via `raw_call` at the
//! SVM tier (P7/P8) rather than modeled at the host tier.
//!
//! Each `#[crucible_fuzz]` fn's generated `main` is gated behind a feature named
//! after the fn, so a single-feature build compiles the other harness fns as
//! dead code — allow it crate-wide rather than warn per build.
#![allow(dead_code)]

use {
    crucible_fuzzer::*,
    velocity::{
        controller::position::PositionDirection,
        create_anchor_account_info,
        math::{
            constants::{
                AMM_RESERVE_PRECISION, PEG_PRECISION, PRICE_PRECISION_I64, PRICE_PRECISION_U64,
            },
            oracle::{is_oracle_valid_for_action, OracleValidity, VelocityAction},
        },
        state::{
            oracle::{get_prelaunch_price, HistoricalOracleData, OraclePriceData, PrelaunchOracle},
            perp_market::{ContractTier, MarketStats, PerpMarket, AMM},
            state::ValidityGuardRails,
        },
        vlp::amm::math::amm::{calculate_new_oracle_price_twap, TwapPeriod},
    },
};

// ---------------------------------------------------------------------------
// Fixture (host-tier: ctx is unused, present only for #[fuzz_fixture] wiring).
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct OracleFixture {
    ctx: TestContext,
}

#[fuzz_fixture]
impl OracleFixture {
    pub fn setup() -> Self {
        OracleFixture {
            ctx: TestContext::new(),
        }
    }

    // #[fuzz_fixture] requires at least one discovered action.
    pub fn action_noop(&mut self) {
        let _ = &self.ctx;
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Map a small index onto every *reachable* `OracleValidity` variant.
/// (`StaleForAMM { immediate: false, low_risk: true }` is unreachable — see the
/// `From<OracleValidity> for u8` impl — so it is intentionally omitted.)
fn validity_from_idx(i: u8) -> OracleValidity {
    match i {
        0 => OracleValidity::NonPositive,
        1 => OracleValidity::TooVolatile,
        2 => OracleValidity::TooUncertain,
        3 => OracleValidity::StaleForMargin,
        4 => OracleValidity::InsufficientDataPoints,
        5 => OracleValidity::StaleForAMM {
            immediate: true,
            low_risk: true,
        },
        6 => OracleValidity::StaleForAMM {
            immediate: true,
            low_risk: false,
        },
        7 => OracleValidity::StaleForAMM {
            immediate: false,
            low_risk: false,
        },
        _ => OracleValidity::Valid,
    }
}

/// Actions ordered strictest (smallest accepted validity set) → most permissive.
/// The accepted-validity sets nest along this chain (verified against
/// `is_oracle_valid_for_action`):
///   {Valid} ⊆ {+StaleForAMM(low_risk=false)} ⊆ {+all StaleForAMM, +Insufficient}
///          ⊆ {+StaleForMargin} ⊆ {+TooUncertain} ⊆ {everything but NonPositive}
const ACTIONS_STRICT_TO_LOOSE: [Option<VelocityAction>; 7] = [
    None, // None == strictest: only Valid
    Some(VelocityAction::FillOrderAmmImmediate),
    Some(VelocityAction::FillOrderAmmLowRisk),
    Some(VelocityAction::OracleOrderPrice),
    Some(VelocityAction::SettlePnl),
    Some(VelocityAction::TriggerOrder),
    Some(VelocityAction::UpdateTwap),
];

/// A well-formed AMM (mirrors the config used in `math/oracle/tests.rs`) so
/// `reserve_price` / `ask_price` / `bid_price` succeed.
fn healthy_amm() -> AMM {
    AMM {
        base_asset_reserve: 2 * AMM_RESERVE_PRECISION,
        quote_asset_reserve: 2 * AMM_RESERVE_PRECISION,
        peg_multiplier: 33 * PEG_PRECISION,
        ..AMM::default()
    }
}

// ===========================================================================
// INVARIANT HARNESSES (Family VIII)
// ===========================================================================

/// Family VIII #1 — **validity monotonicity**: an oracle validity that clears a
/// *stricter* `VelocityAction` clears every *weaker* one (the accepted sets nest).
#[cfg(feature = "inv_validity_monotonicity")]
#[crucible_fuzz]
fn inv_validity_monotonicity(
    fixture: &mut OracleFixture,
    #[range(0..9u8)] v: u8,
    #[range(0..7u8)] a: u8,
    #[range(0..7u8)] b: u8,
) {
    let _ = &fixture.ctx;
    let validity = validity_from_idx(v);
    let lo = a.min(b) as usize; // stricter
    let hi = a.max(b) as usize; // weaker

    let ok_strict = is_oracle_valid_for_action(validity, ACTIONS_STRICT_TO_LOOSE[lo]).unwrap();
    let ok_weak = is_oracle_valid_for_action(validity, ACTIONS_STRICT_TO_LOOSE[hi]).unwrap();

    // valid-for-stricter ⇒ valid-for-weaker
    if ok_strict {
        fuzz_assert!(ok_weak);
    }
}

/// Family VIII #2 — **TWAP is bounded**: `calculate_new_oracle_price_twap`
/// returns a value between the previous TWAP and the (clamped) new price. It is
/// a `calculate_weighted_average` of the two, which carries a ±1-unit rounding
/// bias, so the bound is checked with a 1-unit tolerance. Timestamps are set so
/// no oracle-invalidity interpolation kicks in (`interpolated == oracle_price`).
#[cfg(feature = "inv_twap_between")]
#[crucible_fuzz]
fn inv_twap_between(
    fixture: &mut OracleFixture,
    #[range(1..1_000_000_000_000u64)] oracle_price: u64,
    #[range(1..1_000_000_000_000u64)] old_twap: u64,
    #[range(0..200_000u64)] since: u64,
    #[range(1..200_000u64)] period: u64,
) {
    let _ = &fixture.ctx;
    let oracle_price = oracle_price as i64;
    let old_twap = old_twap as i64;
    let ts = 1_700_000_000_i64;

    let market_stats = MarketStats {
        last_mark_price_twap: old_twap as u64,
        // last_mark_price_twap_ts <= last_oracle_price_twap_ts ⇒ no interpolation
        last_mark_price_twap_ts: ts,
        funding_period: period as i64,
        historical_oracle_data: HistoricalOracleData {
            last_oracle_price_twap: old_twap,
            last_oracle_price_twap_5min: old_twap,
            last_oracle_price_twap_ts: ts,
            ..HistoricalOracleData::default()
        },
        ..MarketStats::default()
    };

    let now = ts + since as i64;
    let r = calculate_new_oracle_price_twap(
        &market_stats,
        now,
        oracle_price,
        TwapPeriod::FundingPeriod,
    )
    .unwrap();

    let lo = oracle_price.min(old_twap) - 1;
    let hi = oracle_price.max(old_twap) + 1;
    fuzz_assert!(r >= lo);
    fuzz_assert!(r <= hi);
}

/// Family VIII #3 — **confidence never gets less conservative**: the safe
/// MM-oracle confidence produced by `PerpMarket::get_mm_oracle_price_data` is
/// always ≥ the exchange-oracle confidence it is derived from (the fallback
/// path returns the exchange data unchanged; the MM path adds a non-negative
/// `mm_oracle_diff_premium`). The exchange confidence is the floor here.
#[cfg(feature = "inv_confidence_floor")]
#[crucible_fuzz]
fn inv_confidence_floor(
    fixture: &mut OracleFixture,
    #[range(1_000..1_000_000_000_000u64)] mm_price: u64,
    // `exch_price` is derived within `diff_bps` (PERCENTAGE_PRECISION units, so
    // 10_000 == 1%) of `mm_price` on either side. MM_EXCHANGE_FALLBACK_THRESHOLD
    // is 1%, so spanning 0..3% lands ~1/3 of inputs on the MM premium branch (the
    // path this invariant actually guards) and the rest on the fallback branch.
    // The previous version drew the two prices independently over the full range,
    // so they were almost always >1% apart and the premium branch was never hit.
    #[range(0..30_000u64)] diff_bps: u64,
    #[range(0..2u8)] dir: u8,
    #[range(0..1_000_000u64)] exch_conf: u64,
    #[range(0..100_000u64)] mm_slot: u64,
    // clock_slot - mm_slot; straddles the 10-slot amm-staleness boundary so the
    // mm oracle is fresh enough to be UseMMOraclePrice-valid on a real fraction
    // of inputs (large `extra_slot` previously forced the stale fallback anyway).
    #[range(0..30u64)] mm_age: u64,
) {
    let _ = &fixture.ctx;
    let clock_slot = mm_slot + mm_age; // clock_slot >= mm_slot

    // PERCENTAGE_PRECISION == 1_000_000; offset is a relative fraction of mm_price.
    let offset = ((mm_price as u128) * (diff_bps as u128) / 1_000_000u128) as u64;
    let exch_price = if dir == 0 {
        mm_price.saturating_sub(offset).max(1)
    } else {
        mm_price.saturating_add(offset)
    };

    let mut market = PerpMarket {
        contract_tier: ContractTier::B,
        ..PerpMarket::default()
    };
    market.market_stats.mm_oracle_price = mm_price as i64;
    market.market_stats.mm_oracle_slot = mm_slot;
    market
        .market_stats
        .historical_oracle_data
        .last_oracle_price_twap = mm_price as i64;

    let exchange = OraclePriceData {
        price: exch_price as i64,
        confidence: exch_conf,
        delay: 0,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };

    let guard = ValidityGuardRails {
        slots_before_stale_for_amm: 10,
        slots_before_stale_for_margin: 120,
        confidence_interval_max_size: 20_000,
        too_volatile_ratio: 5,
    };

    let mm = market
        .get_mm_oracle_price_data(exchange, clock_slot, &guard)
        .unwrap();

    fuzz_assert!(mm.get_confidence() >= exchange.confidence);
}

// ===========================================================================
// REGRESSION HARNESSES
// ===========================================================================

/// audit regression #1 — **`get_fallback_price` never overflows** over any
/// `seconds_til_order_expiry`, including `i64::MAX` / `i64::MIN` / near-boundary.
///
/// PENDING source: already fixed on master (commit c9819d0bc / OtterSec
/// order-amm-correctness finding #11). This is a REGRESSION GUARD, not a
/// pending failure: `max_ts` is unbounded at placement and the pre-fix code
/// multiplied `seconds_til_order_expiry * 20` *before* clamping, so with
/// `overflow-checks = true` a `max_ts = i64::MAX` order aborted every fallback
/// fill. The fix clamps the seconds operand first. Expected: PASS (no crash).
#[cfg(feature = "regr_fallback_price_overflow")]
#[crucible_fuzz]
fn regr_fallback_price_overflow(fixture: &mut OracleFixture, #[range(0..u64::MAX)] secs_bits: u64) {
    let _ = &fixture.ctx;
    let amm = healthy_amm();
    let min_order_size = 1_000_000_000u64; // BASE_PRECISION
    let oracle_price = 100 * PRICE_PRECISION_I64;

    let market_stats = MarketStats {
        last_ask_price_twap: 100 * PRICE_PRECISION_U64,
        last_bid_price_twap: 100 * PRICE_PRECISION_U64,
        historical_oracle_data: HistoricalOracleData {
            last_oracle_price_twap: 100 * PRICE_PRECISION_I64,
            ..HistoricalOracleData::default()
        },
        ..MarketStats::default()
    };

    // Boundary values the finding pins, plus the fuzzed one.
    let boundaries: [i64; 8] = [
        secs_bits as i64,
        i64::MAX,
        i64::MAX - 1,
        i64::MIN,
        -1,
        0,
        1,
        5,
    ];

    for &secs in boundaries.iter() {
        for dir in [PositionDirection::Long, PositionDirection::Short] {
            // liquidity branch (uses amm ask/bid) and no-liquidity branch (uses oracle).
            for avail in [min_order_size, 0u64] {
                // A panic here (overflow abort) is the bug; the fix makes this Ok.
                let _ = amm.get_fallback_price(
                    &market_stats,
                    &dir,
                    avail,
                    oracle_price,
                    secs,
                    min_order_size,
                );
            }
        }
    }

    // Stronger guard: the AMM liquidity branch must return Ok even at i64::MAX.
    for dir in [PositionDirection::Long, PositionDirection::Short] {
        let r = amm.get_fallback_price(
            &market_stats,
            &dir,
            min_order_size,
            oracle_price,
            i64::MAX,
            min_order_size,
        );
        fuzz_assert!(r.is_ok());
    }
}

/// PENDING PR #268 (OtterSec F4, finding #71) — **prelaunch staleness delay**.
/// The fix reverses `get_prelaunch_price`'s delay to `slot - amm_last_update_slot`
/// (staleness grows as the clock advances). Current master computes
/// `amm_last_update_slot.saturating_sub(slot)`, which floors to 0 once the clock
/// passes the last AMM update, clearing every freshness gate.
///
/// This asserts the FIXED invariant, so it FAILS on current master: with
/// `slot > amm_last_update_slot` master reports delay 0 while the fix reports
/// `slot - amm_last_update_slot > 0`.
#[cfg(feature = "regr_268_prelaunch_delay")]
#[crucible_fuzz]
fn regr_268_prelaunch_delay(
    fixture: &mut OracleFixture,
    #[range(0..1_000_000_000u64)] amm_slot: u64,
    #[range(1..1_000_000u64)] extra: u64,
) {
    let _ = &fixture.ctx;
    let slot = amm_slot + extra; // slot > amm_slot

    let mut oracle = PrelaunchOracle {
        price: PRICE_PRECISION_I64,
        amm_last_update_slot: amm_slot,
        ..PrelaunchOracle::default()
    };
    create_anchor_account_info!(oracle, PrelaunchOracle, oracle_ai);

    let data = get_prelaunch_price(&oracle_ai, slot).unwrap();

    // FIXED invariant: staleness delay = slot - amm_last_update_slot.
    let expected = (slot - amm_slot) as i64;
    fuzz_assert_eq!(data.delay, expected);
}

// DEFERRED TO SVM (P7/P8): #268 pyth-lazer confidence floor + staleness/replay live in the post_pyth_lazer_oracle_update instruction handler — must be driven via raw_call at the SVM tier, not modeled here.
