//! P2 "amm-pricing" — host-tier property harnesses over velocity's pure vAMM
//! math (`vlp/amm/math/{amm,spread,repeg,cp_curve,jit}`). No LiteSVM: every
//! harness constructs `AMM` / `PerpMarket` fixtures via `..Default::default()`
//! and drives the math functions directly, so Crucible gets edge coverage over
//! the compiled curve code.
//!
//! Two harness sets:
//!  * `prop_*`  — always-on invariants (Families IV + II): constant-product
//!    conservation, spread caps, price bounds, repeg/k cost sign, JIT clamp.
//!  * `regr_269_*` — PENDING PR #269 (OtterSec F7). Each asserts the FIXED
//!    invariant, so it reports a violation on current (pre-fix) master.

use crucible_fuzzer::*;

use velocity::controller::position::PositionDirection;
use velocity::math::bn::U192;
use velocity::math::constants::{
    AMM_RESERVE_PRECISION, MAX_CONCENTRATION_COEFFICIENT, PEG_PRECISION, PRICE_PRECISION,
};
use velocity::math::oracle::OracleValidity;
use velocity::math::safe_math::SafeMath;
use velocity::state::market_status::MarketStatus;
use velocity::state::oracle::{MMOraclePriceData, OraclePriceData};
use velocity::state::perp_market::{PerpMarket, AMM};
use velocity::vlp::amm::controller::SwapDirection;
use velocity::vlp::amm::math::amm::{
    calculate_amm_available_liquidity, calculate_bid_ask_bounds, calculate_price,
    calculate_quote_asset_amount_swapped, calculate_swap_output, sanitize_new_price,
};
use velocity::vlp::amm::math::cp_curve::{adjust_k_cost, get_update_k_result};
use velocity::vlp::amm::math::jit::{
    calculate_amm_jit_liquidity, calculate_clamped_jit_base_asset_amount,
};
use velocity::vlp::amm::math::repeg::{
    calculate_repeg_cost, project_post_refresh, project_post_refresh_scalar, ProjectionInputs,
};
use velocity::vlp::amm::math::spread::{
    calculate_base_asset_amount_to_trade_to_price, cap_to_max_spread,
};

#[derive(Clone)]
struct AmmFixture {
    // Host-tier harnesses don't drive instructions, but `#[fuzz_fixture]`
    // requires a `TestContext` field for its snapshot/clone wiring. Unused.
    ctx: TestContext,
}

#[fuzz_fixture]
impl AmmFixture {
    pub fn setup() -> Self {
        AmmFixture {
            ctx: TestContext::new(),
        }
    }

    // `#[fuzz_fixture]` requires at least one discovered action.
    pub fn action_noop(&mut self) {
        let _ = &self.ctx;
    }
}

// ---------------------------------------------------------------------------
// Invariant harnesses (Families IV + II) — required-green.
// ---------------------------------------------------------------------------

/// Family IV/II: constant-product `k` is conserved within rounding by a swap
/// and rounding always favors the pool (never the trader), and a base→quote
/// round-trip returns no more quote than was paid.
#[cfg(feature = "prop_k_conserved_swap")]
#[crucible_fuzz]
fn prop_k_conserved_swap(
    fixture: &mut AmmFixture,
    #[range(1_000_000_000..1_000_000_000_000_000u64)] sqrt_k: u64,
    #[range(1..500_000u64)] delta_num: u64,
) {
    let _ = &fixture.ctx;
    let sk = sqrt_k as u128;
    let base = sk; // balanced pool: base == quote == sqrt_k, so k == base*quote
                   // delta in (0, base/2]
    let delta = ((base.saturating_mul(delta_num as u128)) / 1_000_000)
        .min(base / 2)
        .max(1);

    let k = U192::from(sk).safe_mul(U192::from(sk)).unwrap();

    // Single swap (add base): output reserve is floored, so product <= k
    // (pool keeps at least k; the trader receives the rounded-down amount).
    let (new_out, new_in) = calculate_swap_output(delta, base, SwapDirection::Add, sk).unwrap();
    let prod = U192::from(new_in).safe_mul(U192::from(new_out)).unwrap();
    fuzz_assert!(prod <= k);
    // Rounding is tight: bumping output by one unit would exceed k.
    let prod_plus = U192::from(new_in)
        .safe_mul(U192::from(new_out + 1))
        .unwrap();
    fuzz_assert!(prod_plus > k);

    // Round-trip: go long (Remove base) then unwind (Add base). Quote received
    // on the unwind must not exceed quote paid to open — no free value.
    let (q_after_open, base_after_open) =
        calculate_swap_output(delta, base, SwapDirection::Remove, sk).unwrap();
    let quote_paid = calculate_quote_asset_amount_swapped(
        base, // quote reserve before == base (balanced)
        q_after_open,
        SwapDirection::Remove,
        PEG_PRECISION,
    )
    .unwrap();

    let (q_after_close, _base_after_close) =
        calculate_swap_output(delta, base_after_open, SwapDirection::Add, sk).unwrap();
    let quote_recv = calculate_quote_asset_amount_swapped(
        q_after_open,
        q_after_close,
        SwapDirection::Add,
        PEG_PRECISION,
    )
    .unwrap();

    fuzz_assert!(quote_recv <= quote_paid);
}

/// Family IV: `cap_to_max_spread` never lets `long + short` exceed `max_spread`
/// and is idempotent.
#[cfg(feature = "prop_cap_to_max_spread")]
#[crucible_fuzz]
fn prop_cap_to_max_spread(
    fixture: &mut AmmFixture,
    #[range(0..10_000_000u64)] long_spread: u64,
    #[range(0..10_000_000u64)] short_spread: u64,
    #[range(1..2_000_000u64)] max_spread: u64,
) {
    let _ = &fixture.ctx;
    let Ok((l, s)) = cap_to_max_spread(long_spread, short_spread, max_spread) else {
        return;
    };
    let total = l.checked_add(s).unwrap();
    fuzz_assert_le!(total, max_spread);

    // Idempotent: re-capping an already-capped pair is a no-op.
    let (l2, s2) = cap_to_max_spread(l, s, max_spread).unwrap();
    fuzz_assert_eq!(l2, l);
    fuzz_assert_eq!(s2, s);
}

/// Family IV: bid <= reserve price <= ask; reserve price is monotone in the
/// reserves; concentration bounds bracket sqrt_k; `sanitize_new_price` stays
/// within the configured band.
#[cfg(feature = "prop_bid_ask_price_bounds")]
#[crucible_fuzz]
fn prop_bid_ask_price_bounds(
    fixture: &mut AmmFixture,
    #[range(1_000_000_000..1_000_000_000_000u64)] base_r: u64,
    #[range(1_000_000_000..1_000_000_000_000u64)] quote_r: u64,
    #[range(1..1_000_000_000u64)] dq: u64,
    #[range(1_000_000..2_000_000u64)] peg: u64,
    #[range(0..500_000u32)] long_spread: u32,
    #[range(0..500_000u32)] short_spread: u32,
    #[range(1_000_001..1_414_201u64)] coef: u64,
    // NOTE: use unsigned params for signed values. `#[range]` reduces with `%`,
    // which preserves the sign of a negative arbitrary input, so a signed range
    // can yield values below `start` (e.g. denom == 0 -> divide-by-zero).
    #[range(1..1_000_000_000_000u64)] new_price_u: u64,
    #[range(1..1_000_000_000_000u64)] twap_u: u64,
    #[range(1..100_000u64)] denom_u: u64,
) {
    let _ = &fixture.ctx;
    let new_price = new_price_u as i64;
    let twap = twap_u as i64;
    let denom = denom_u as i64;

    // Concentration bounds are a reciprocal pair around sqrt_k:
    //   bid = sqrt_k * PREC / coef,  ask = sqrt_k * coef / PREC
    // so bid < sqrt_k < ask and bid*ask == sqrt_k^2 (modulo integer rounding).
    // The balanced fixture pool has sqrt_k == base_r, so pass base_r as sqrt_k
    // (the previous code passed base_r but then asserted base_r was bracketed by
    // bounds *derived from base_r*, so it was tautological, and never checked the two
    // bounds against each other). The geometric-symmetry check below is the real
    // invariant: a wrong formula or asymmetric rounding on one leg breaks the
    // product even though each bound still sits on the correct side of sqrt_k.
    let sqrt_k = base_r as u128;
    let (bid_bound, ask_bound) = calculate_bid_ask_bounds(coef as u128, sqrt_k).unwrap();
    fuzz_assert!(bid_bound < sqrt_k && sqrt_k < ask_bound);
    let prod = bid_bound.checked_mul(ask_bound).unwrap();
    let k_sq = sqrt_k.checked_mul(sqrt_k).unwrap();
    // Tolerance 0.1% of sqrt_k^2 dwarfs the O(1) integer-division rounding on each
    // leg (sqrt_k >= 1e9 here), so this cannot false-positive but does catch a
    // formula/rounding asymmetry.
    let tol = k_sq / 1000;
    fuzz_assert!(prod <= k_sq + tol && prod + tol >= k_sq);

    // Price monotonicity: up in quote reserve, down in base reserve.
    let p1 = calculate_price(quote_r as u128, base_r as u128, peg as u128).unwrap();
    let p_more_quote =
        calculate_price((quote_r + dq) as u128, base_r as u128, peg as u128).unwrap();
    let p_more_base = calculate_price(quote_r as u128, (base_r + dq) as u128, peg as u128).unwrap();
    fuzz_assert!(p_more_quote >= p1);
    fuzz_assert!(p_more_base <= p1);

    // bid <= reserve price <= ask (zero reference-price offset).
    let amm = AMM {
        base_asset_reserve: base_r as u128,
        quote_asset_reserve: quote_r as u128,
        peg_multiplier: peg as u128,
        ..AMM::default()
    };
    let rp = amm.reserve_price().unwrap();
    let (bid, ask) = amm.bid_ask_price(rp, long_spread, short_spread, 0).unwrap();
    fuzz_assert_le!(bid, rp);
    fuzz_assert_le!(rp, ask);

    // sanitize_new_price stays within +/- (twap / denom) of the twap.
    let out = sanitize_new_price(new_price, twap, Some(denom)).unwrap();
    let band = (twap / denom).unsigned_abs() as i128;
    let diff = (out as i128 - twap as i128).abs();
    fuzz_assert!(diff <= band);
}

/// Family IV/II: repeg / k-adjust cost sign & bounds.
/// * `calculate_repeg_cost` sign == sign(quote-terminal) * sign(Δpeg), zero when
///   either factor is zero.
/// * `adjust_k_cost`: increasing k costs the protocol (cost >= 0); decreasing k
///   relieves it (cost <= 0).
#[cfg(feature = "prop_repeg_k_cost")]
#[crucible_fuzz]
fn prop_repeg_k_cost(
    fixture: &mut AmmFixture,
    #[range(1_000_000_000..1_000_000_000_000u64)] qar: u64,
    #[range(1_000_000_000..1_000_000_000_000u64)] tqar: u64,
    #[range(1_000_000..100_000_000_000u64)] peg: u64,
    #[range(1_000_000..100_000_000_000u64)] new_peg: u64,
    #[range(1_000_000_000..1_000_000_000_000u64)] sqrt_k: u64,
    #[range(-800_000_000..800_000_000i64)] baa: i64,
) {
    let _ = &fixture.ctx;
    // Clamp |baa| < sqrt_k/4 (a signed `#[range]` can exceed its bounds via the
    // sign-preserving `%` reduction) so the k-adjust swap math stays in-domain.
    let baa = (baa as i128) % (sqrt_k as i128 / 4);

    // --- calculate_repeg_cost sign consistency ---
    let amm_peg = AMM {
        quote_asset_reserve: qar as u128,
        terminal_quote_asset_reserve: tqar as u128,
        peg_multiplier: peg as u128,
        ..AMM::default()
    };
    let cost = calculate_repeg_cost(&amm_peg, new_peg as u128).unwrap();
    let d1 = qar as i128 - tqar as i128;
    let d2 = new_peg as i128 - peg as i128;
    if d1 == 0 || d2 == 0 {
        fuzz_assert_eq!(cost, 0i128);
    } else if (d1 > 0) == (d2 > 0) {
        fuzz_assert!(cost >= 0);
    } else {
        fuzz_assert!(cost <= 0);
    }

    // --- adjust_k_cost sign vs k direction ---
    // Balanced pool, |baa| < sqrt_k, so the k-decrease passes validation.
    let sk = sqrt_k as u128;
    let amm_k = AMM {
        base_asset_reserve: sk,
        quote_asset_reserve: sk,
        sqrt_k: sk,
        terminal_quote_asset_reserve: sk,
        peg_multiplier: PEG_PRECISION,
        concentration_coef: MAX_CONCENTRATION_COEFFICIENT,
        base_asset_amount_with_amm: baa as i128,
        ..AMM::default()
    };

    let k_up = U192::from(sk + sk / 10);
    if let Ok(up) = get_update_k_result(&amm_k, MarketStatus::Initialized, k_up, false) {
        let cost_up = adjust_k_cost(&amm_k, &up).unwrap();
        fuzz_assert!(cost_up >= 0);
    }

    let k_down = U192::from(sk - sk / 10);
    if let Ok(down) = get_update_k_result(&amm_k, MarketStatus::Initialized, k_down, false) {
        let cost_down = adjust_k_cost(&amm_k, &down).unwrap();
        fuzz_assert!(cost_down <= 0);
    }
}

/// Family IV: `calculate_clamped_jit_base_asset_amount` never exceeds its input
/// (intensity <= 100 scaling) and never exceeds the AMM's net inventory.
#[cfg(feature = "prop_jit_clamped_bound")]
#[crucible_fuzz]
fn prop_jit_clamped_bound(
    fixture: &mut AmmFixture,
    #[range(0..1_000_000_000_000u64)] jit_in: u64,
    #[range(0..101u8)] intensity: u8,
    #[range(-500_000_000_000..500_000_000_000i64)] baa: i64,
) {
    let _ = &fixture.ctx;
    let market = PerpMarket {
        amm: AMM {
            amm_jit_intensity: intensity,
            base_asset_amount_with_amm: baa as i128,
            ..AMM::default()
        },
        ..PerpMarket::default()
    };
    let clamped = calculate_clamped_jit_base_asset_amount(&market, jit_in).unwrap();
    fuzz_assert_le!(clamped, jit_in);
    fuzz_assert!((clamped as u128) <= (baa as i128).unsigned_abs());
}

// ---------------------------------------------------------------------------
// Regression harnesses — PENDING PR #269 (OtterSec F7). These assert the FIXED
// invariant and MUST report a violation on current (pre-fix) master.
// ---------------------------------------------------------------------------

/// PENDING PR #269 (F7 #58): `calculate_base_asset_amount_to_trade_to_price`
/// must size the cap against the spread-adjusted ask/bid reserves ALWAYS —
/// even when `base_spread == 0` — because the swap executes against those
/// reserves. On master the `base_spread > 0` gate falls through to the raw
/// `base_asset_reserve`, so with a nonzero cached vol/inventory spread the cap
/// is sized off a different curve than the fill and the AMM can execute past
/// the taker's limit. Asserting the fixed (ask/bid) result fails on master.
#[cfg(feature = "regr_269_limit_cap_reserve_basis")]
#[crucible_fuzz]
fn regr_269_limit_cap_reserve_basis(
    fixture: &mut AmmFixture,
    #[range(1_000_000_000..1_000_000_000_000u64)] sqrt_k: u64,
    #[range(1_000_000..2_000_000u64)] peg: u64,
    #[range(1_000_000..100_000_000u64)] offset: u64,
    #[range(0..2u8)] dir_sel: u8,
) {
    let _ = &fixture.ctx;
    let sk = sqrt_k as u128;
    let base = sk;
    let pegm = peg as u128;
    let off = offset as u128;
    if off >= base {
        return;
    }

    // base_spread == 0, but the cached spread reserves diverge from the raw
    // reserve (a real nonzero vol/inventory spread).
    let ask_base = base - off; // asks pull the base reserve down
    let bid_base = base + off; // bids push it up
    let amm = AMM {
        sqrt_k: sk,
        base_asset_reserve: base,
        quote_asset_reserve: base,
        peg_multiplier: pegm,
        base_spread: 0,
        ask_base_asset_reserve: ask_base,
        bid_base_asset_reserve: bid_base,
        ..AMM::default()
    };

    let dir = if dir_sel == 0 {
        PositionDirection::Long
    } else {
        PositionDirection::Short
    };

    let rp = amm.reserve_price().unwrap();
    // A price well below reserve targets a base reserve far above all three
    // reserve values, so both the buggy and fixed paths resolve to the same
    // direction and differ only by `offset` — an unambiguous inequality.
    let limit_price = (rp / 4).max(1);

    let Ok((amount, out_dir)) =
        calculate_base_asset_amount_to_trade_to_price(&amm, limit_price, dir)
    else {
        return;
    };

    // FIXED behavior: always size against ask (Long) / bid (Short) reserves.
    let base_before_fixed = match dir {
        PositionDirection::Long => ask_base,
        PositionDirection::Short => bid_base,
    };
    let inv = U192::from(sk).safe_mul(U192::from(sk)).unwrap();
    let nbr_sq = inv
        .safe_mul(U192::from(PRICE_PRECISION))
        .unwrap()
        .safe_div(U192::from(limit_price))
        .unwrap()
        .safe_mul(U192::from(pegm))
        .unwrap()
        .safe_div(U192::from(PEG_PRECISION))
        .unwrap();
    let nbr = nbr_sq.integer_sqrt().try_to_u128().unwrap();

    let (exp_amount, exp_dir) = if nbr > base_before_fixed {
        (
            (nbr - base_before_fixed).min(u64::MAX as u128) as u64,
            PositionDirection::Short,
        )
    } else {
        (
            (base_before_fixed - nbr).min(u64::MAX as u128) as u64,
            PositionDirection::Long,
        )
    };

    fuzz_assert!(out_dir == exp_dir);
    fuzz_assert_eq!(amount, exp_amount);
}

/// PENDING PR #269 (F7 #61): the scalar fill/funding-path projection
/// (`project_post_refresh_scalar`) must honor the market's `min_order_size`
/// k-down floor — i.e. it must agree with the full `project_post_refresh`.
/// On master the scalar path builds its synthetic `PerpMarket` from
/// `PerpMarket::default()` (min_order_size == 0), so `can_lower_k` /
/// `get_lower_bound_sqrt_k` see a floor of 0 and can lower k below what a full
/// refresh enforces — the two projections diverge. Asserting they agree fails
/// on master whenever `min_order_size` changes the k path.
#[cfg(feature = "regr_269_scalar_k_floor")]
#[crucible_fuzz]
fn regr_269_scalar_k_floor(
    fixture: &mut AmmFixture,
    #[range(8_000_000_000..18_500_000_000u64)] oracle_price_u: u64,
    #[range(0..60_000_000_000u64)] min_order_size: u64,
    #[range(100..201u8)] curve_update_intensity: u8,
    #[range(0..40_000_000u64)] tfmd: u64,
) {
    let _ = &fixture.ctx;
    // Unsigned param cast to a positive price (see the note in
    // `prop_bid_ask_price_bounds` about signed `#[range]` reduction).
    let oracle_price = oracle_price_u as i64;

    // Fixture adapted from the repeg unit tests: a net-short AMM with a small
    // fee budget and an oracle below the reserve price, which drives
    // adjust_amm into the "use full budget peg" branch where the k-down floor
    // (and thus min_order_size) matters.
    let amm = AMM {
        base_asset_reserve: 65 * AMM_RESERVE_PRECISION,
        quote_asset_reserve: 63_015_384_615,
        terminal_quote_asset_reserve: 64 * AMM_RESERVE_PRECISION,
        sqrt_k: 64 * AMM_RESERVE_PRECISION,
        peg_multiplier: 19_400_000_000,
        base_asset_amount_with_amm: -(AMM_RESERVE_PRECISION as i128),
        concentration_coef: MAX_CONCENTRATION_COEFFICIENT,
        min_base_asset_reserve: 45 * AMM_RESERVE_PRECISION,
        max_base_asset_reserve: 90 * AMM_RESERVE_PRECISION,
        base_spread: 250,
        max_spread: 50_000,
        curve_update_intensity,
        total_fee_minus_distributions: tfmd as i128,
        ..AMM::default()
    };
    let mut market = PerpMarket {
        amm,
        status: MarketStatus::Active,
        ..PerpMarket::default()
    };
    market.market_stats.min_order_size = min_order_size;

    let opd = OraclePriceData {
        price: oracle_price,
        confidence: 0,
        delay: 0,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };
    let Ok(mm) = MMOraclePriceData::new(oracle_price, 0, 0, OracleValidity::Valid, opd) else {
        return;
    };

    let Ok(full) = project_post_refresh(&market, &mm, Some(OracleValidity::Valid)) else {
        return;
    };
    let inputs = ProjectionInputs::from_market(&market);
    let Ok(scalar) =
        project_post_refresh_scalar(&market.amm, &inputs, &mm, Some(OracleValidity::Valid))
    else {
        return;
    };

    // The fix threads min_order_size onto the synthetic market so the scalar
    // projection matches the full one.
    fuzz_assert_eq!(scalar.sqrt_k, full.sqrt_k);
    fuzz_assert_eq!(scalar.peg_multiplier, full.peg_multiplier);
    fuzz_assert_eq!(scalar.base_asset_reserve, full.base_asset_reserve);
    fuzz_assert_eq!(scalar.quote_asset_reserve, full.quote_asset_reserve);
    fuzz_assert_eq!(scalar.cost, full.cost);
}

/// PENDING PR #269 (F7 #63): a JIT slice sized from a DLOB match context must
/// be clamped to `calculate_amm_available_liquidity` (the per-fill reserve
/// throttle, `max_fill_reserve_fraction`), the same bound the standalone AMM
/// path enforces. `calculate_amm_jit_liquidity` (the sizing
/// `AmmJitQuoter::from_match_context` uses) only bounds by oracle proximity,
/// intensity and inventory — not by how far one fill pushes reserves. Asserting
/// the fixed bound (jit <= available liquidity) fails on master.
#[cfg(feature = "regr_269_jit_available_liquidity")]
#[crucible_fuzz]
fn regr_269_jit_available_liquidity(
    fixture: &mut AmmFixture,
    #[range(4_000_000..40_000_000_000u64)] maker_size: u64,
    #[range(4_000_000..60_000_000_000u64)] baa: u64,
) {
    let _ = &fixture.ctx;
    let base = 100 * AMM_RESERVE_PRECISION; // 1e11
    let step: u64 = 1_000_000;
    // Tight ask-side room => small per-side available liquidity.
    let max_base = base + 2 * step as u128;

    let market = PerpMarket {
        amm: AMM {
            base_asset_reserve: base,
            quote_asset_reserve: base,
            sqrt_k: base,
            peg_multiplier: PEG_PRECISION,
            min_base_asset_reserve: 0,
            max_base_asset_reserve: max_base,
            base_asset_amount_with_amm: baa as i128, // net long => wants to JIT a Short taker
            amm_jit_intensity: 100,
            max_fill_reserve_fraction: 1,
            ..AMM::default()
        },
        order_step_size: step,
        ..PerpMarket::default()
    };

    let oracle_price: i64 = PEG_PRECISION as i64; // reserve price for balanced pool
    let taker_dir = PositionDirection::Short;

    let Ok(jit) = calculate_amm_jit_liquidity(
        &market,
        taker_dir,
        oracle_price as u64, // maker/auction price == oracle => no wash shrink
        Some(oracle_price),
        maker_size, // base_asset_amount (== the size JIT halves)
        maker_size, // taker_unfilled
        maker_size, // maker_unfilled
        true,       // taker_has_limit_price => no "amm fills next round" short-circuit
    ) else {
        return;
    };

    let avail = calculate_amm_available_liquidity(&market.amm, &taker_dir, step).unwrap();

    // FIXED invariant: the match-context JIT slice is throttled to the same
    // per-fill reserve bound as the standalone path.
    fuzz_assert_le!(jit, avail);
}
