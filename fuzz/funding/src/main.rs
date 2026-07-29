//! Family VI — funding zero-sum host-tier property harnesses.
//!
//! Fuzzes the pure funding math in `velocity::math::funding` directly (no
//! LiteSVM). Each `#[crucible_fuzz]` fn is gated on a feature named exactly
//! like the fn, because the macro emits a `fn main()` per harness — building
//! `--features <name>` selects exactly one. The shared fixture + `action_noop`
//! satisfy `#[fuzz_fixture]`'s ≥1-action requirement even though these are
//! stateless single-op properties.

// Imports are shared across the cfg-gated harness fns; only the enabled
// feature's fn uses its subset, so suppress the per-build unused warnings.
#[allow(unused_imports)]
use velocity::math::constants::PERCENTAGE_PRECISION_I128;
#[allow(unused_imports)]
use velocity::math::funding::{
    calculate_amm_funding_payment, calculate_funding_payment,
    calculate_funding_payment_in_quote_precision, calculate_funding_premium_with_offset,
    calculate_funding_rate_long_short, validate_funding_pnl_profitability, FundingMarketInputs,
};
use {crucible_fuzzer::*, velocity::state::user::PerpPosition};

#[derive(Clone)]
struct FundingFixture {
    // #[fuzz_fixture] requires a TestContext field for its snapshot/clone
    // wiring even for host-tier math harnesses. Left unused.
    ctx: TestContext,
}

#[fuzz_fixture]
impl FundingFixture {
    pub fn setup() -> Self {
        FundingFixture {
            ctx: TestContext::new(),
        }
    }

    // #[fuzz_fixture] requires at least one discovered action.
    pub fn action_noop(&mut self) {
        let _ = &self.ctx;
    }
}

/// Map an unsigned magnitude + a 0/1 sign selector to a signed i128.
#[allow(dead_code)]
fn signed(mag: u64, neg: u64) -> i128 {
    let m = mag as i128;
    if neg == 1 {
        -m
    } else {
        m
    }
}

/// Invariant 1 — Funding zero-sum. Given the (long, short) rates that
/// `calculate_funding_rate_long_short` publishes for a period, the funding the
/// long aggregate pays plus what the short aggregate pays plus the AMM's own
/// funding settlement must net to ~0 (± rounding toward the protocol). The AMM
/// is the counterparty to both user aggregates, so no quote is created or
/// destroyed by a funding update.
#[cfg(feature = "prop_funding_zero_sum")]
#[crucible_fuzz]
fn prop_funding_zero_sum(
    fixture: &mut FundingFixture,
    #[range(0..10_000_000_000_000u64)] base_long_mag: u64,
    #[range(0..10_000_000_000_000u64)] base_short_mag: u64,
    #[range(0..1_000_000_000_000u64)] rate_mag: u64,
    #[range(0..2u64)] rate_neg: u64,
    #[range(0..1_000_000_000_000u64)] tfmd: u64,
) {
    let _ = &fixture.ctx;

    let base_long = base_long_mag as i128; // longs: base >= 0
    let base_short = -(base_short_mag as i128); // shorts: base <= 0
    let funding_rate = signed(rate_mag, rate_neg);

    let inputs = FundingMarketInputs {
        // Balanced accounting: AMM counterparty == net user position.
        net_counterparty_position: base_long + base_short,
        base_asset_amount_long: base_long,
        base_asset_amount_short: base_short,
        total_fee_minus_distributions: tfmd as i128,
    };

    let (rate_long, rate_short, _pnl) =
        match calculate_funding_rate_long_short(&inputs, funding_rate) {
            Ok(v) => v,
            Err(_) => return, // graceful reject is not a violation
        };

    // What each user aggregate pays (quote precision). Positive = the side
    // receives; sign handled inside the helper.
    let user_long = match calculate_funding_payment_in_quote_precision(rate_long, base_long) {
        Ok(v) => v,
        Err(_) => return,
    };
    let user_short = match calculate_funding_payment_in_quote_precision(rate_short, base_short) {
        Ok(v) => v,
        Err(_) => return,
    };

    // The AMM settles from the same cum-rate deltas, decomposed across the two
    // sides it is counterparty to (last_* = 0 → deltas == the period rates).
    let amm =
        match calculate_amm_funding_payment(base_long, base_short, rate_long, rate_short, 0, 0) {
            Ok(v) => v,
            Err(_) => return,
        };

    // Users + AMM net to zero. The user side divides by the quote ratio per
    // side while the AMM divides once at the end, so allow a small rounding
    // slack (ratio = 1000; three truncations toward zero → |err| ≤ 2).
    let net = user_long + user_short + amm;
    fuzz_assert!(
        net.abs() <= 2,
        "funding not zero-sum: user_long={} user_short={} amm={} net={}",
        user_long,
        user_short,
        amm,
        net
    );
}

/// Invariant 2 — `calculate_funding_payment` sign correctness and magnitude
/// monotonicity. A long (base > 0) pays when the cumulative rate rises
/// (payment ≤ 0); a short receives. Zero delta or zero position ⇒ zero
/// payment. Magnitude is monotone non-decreasing in |position|. No silent
/// overflow: a graceful `Err` is fine, a panic is a finding.
#[cfg(feature = "prop_funding_payment_sign")]
#[crucible_fuzz]
fn prop_funding_payment_sign(
    fixture: &mut FundingFixture,
    #[range(0..1_000_000_000_000_000u64)] base_mag: u64,
    #[range(0..2u64)] base_neg: u64,
    #[range(0..1_000_000_000_000u64)] delta_mag: u64,
    #[range(0..2u64)] delta_neg: u64,
) {
    let _ = &fixture.ctx;

    let base = signed(base_mag, base_neg) as i64;
    let delta = signed(delta_mag, delta_neg); // last_cumulative = 0 → cum == delta

    let pos = PerpPosition {
        base_asset_amount: base,
        last_cumulative_funding_rate: 0,
        ..PerpPosition::default()
    };

    let payment = match calculate_funding_payment(delta, &pos) {
        Ok(p) => p,
        Err(_) => return,
    };

    // Sign correctness: payment sign == -sign(base) * sign(delta).
    if base == 0 || delta == 0 {
        fuzz_assert_eq!(payment, 0i64);
    } else if (base > 0) == (delta > 0) {
        // same sign (long & rate up, or short & rate down) → position pays
        fuzz_assert!(payment <= 0, "expected pays (<=0), got {}", payment);
    } else {
        fuzz_assert!(payment >= 0, "expected receives (>=0), got {}", payment);
    }

    // Magnitude monotone in |position|: halving the position cannot increase
    // the magnitude of the payment.
    let pos_half = PerpPosition {
        base_asset_amount: base / 2,
        last_cumulative_funding_rate: 0,
        ..PerpPosition::default()
    };
    if let Ok(payment_half) = calculate_funding_payment(delta, &pos_half) {
        fuzz_assert!(
            (payment_half as i128).abs() <= (payment as i128).abs(),
            "magnitude not monotone: half={} full={}",
            payment_half,
            payment
        );
    }
}

/// Invariant 3 — Capped funding rate stays within bounds and the resulting
/// AMM PnL is profitability-valid. With `total_fee_minus_distributions ≥ 0`:
/// each published side rate never exceeds the uncapped rate in magnitude and
/// never flips sign; a positive-imbalance period is never capped (both sides
/// == the raw rate); and `validate_funding_pnl_profitability` accepts the
/// returned PnL (funding never drives AMM equity below zero).
#[cfg(feature = "prop_capped_funding_rate")]
#[crucible_fuzz]
fn prop_capped_funding_rate(
    fixture: &mut FundingFixture,
    #[range(0..10_000_000_000_000u64)] base_long_mag: u64,
    #[range(0..10_000_000_000_000u64)] base_short_mag: u64,
    #[range(0..1_000_000_000_000u64)] rate_mag: u64,
    #[range(0..2u64)] rate_neg: u64,
    #[range(0..500_000_000_000u64)] tfmd: u64,
) {
    let _ = &fixture.ctx;

    let base_long = base_long_mag as i128;
    let base_short = -(base_short_mag as i128);
    let funding_rate = signed(rate_mag, rate_neg);

    let inputs = FundingMarketInputs {
        net_counterparty_position: base_long + base_short,
        base_asset_amount_long: base_long,
        base_asset_amount_short: base_short,
        // Non-negative equity keeps the profitability gate satisfiable.
        total_fee_minus_distributions: tfmd as i128,
    };

    let (rate_long, rate_short, pnl) =
        match calculate_funding_rate_long_short(&inputs, funding_rate) {
            Ok(v) => v,
            Err(_) => return,
        };

    // Capped side never exceeds the uncapped rate in magnitude.
    fuzz_assert!(
        rate_long.unsigned_abs() <= funding_rate.unsigned_abs(),
        "long rate {} exceeds uncapped {}",
        rate_long,
        funding_rate
    );
    fuzz_assert!(
        rate_short.unsigned_abs() <= funding_rate.unsigned_abs(),
        "short rate {} exceeds uncapped {}",
        rate_short,
        funding_rate
    );

    // Capping never flips sign (product with the raw rate is non-negative).
    fuzz_assert!(rate_long * funding_rate >= 0, "long rate flipped sign");
    fuzz_assert!(rate_short * funding_rate >= 0, "short rate flipped sign");

    // The published PnL must pass the profitability gate the orchestrator runs
    // before committing the funding update.
    fuzz_assert!(
        validate_funding_pnl_profitability(&inputs, pnl).is_ok(),
        "profitability gate rejected published pnl {} (tfmd {})",
        pnl,
        tfmd
    );
}

/// Invariant 4 — `calculate_funding_premium_with_offset` bounds. Inside the
/// dead zone the premium is exactly the offset. Outside it, the ramped
/// component is bounded by |price_spread| × ramp_slope / PERCENTAGE_PRECISION,
/// and the premium is monotone non-decreasing in the price spread.
#[cfg(feature = "prop_funding_premium_bounded")]
#[crucible_fuzz]
fn prop_funding_premium_bounded(
    fixture: &mut FundingFixture,
    #[range(0..1_000_000_000_000u64)] spread_mag: u64,
    #[range(0..2u64)] spread_neg: u64,
    #[range(0..1_000_000_000u64)] clamp_threshold: u64,
    #[range(0..2_000_000u64)] ramp_slope: u64,
    #[range(0..1_000_000_000u64)] offset_mag: u64,
    #[range(0..2u64)] offset_neg: u64,
) {
    let _ = &fixture.ctx;

    let spread = signed(spread_mag, spread_neg) as i64;
    let threshold = clamp_threshold as i64;
    let slope = ramp_slope as u32;
    let offset = signed(offset_mag, offset_neg) as i64;

    let premium = match calculate_funding_premium_with_offset(spread, threshold, slope, offset) {
        Ok(p) => p,
        Err(_) => return,
    };

    if spread.abs() <= threshold {
        // Dead zone: offset only.
        fuzz_assert_eq!(premium, offset);
    } else {
        // Outside: |ramped| ≤ |spread| * slope / PERCENTAGE_PRECISION (+1 for
        // truncation).
        let ramped = premium as i128 - offset as i128;
        let bound =
            (spread.abs() as i128).saturating_mul(slope as i128) / PERCENTAGE_PRECISION_I128 + 1;
        fuzz_assert!(
            ramped.abs() <= bound,
            "ramped {} exceeds bound {} (spread {} slope {})",
            ramped,
            bound,
            spread,
            slope
        );
    }

    // Monotone non-decreasing in the price spread.
    if spread < i64::MAX {
        if let Ok(premium_up) =
            calculate_funding_premium_with_offset(spread + 1, threshold, slope, offset)
        {
            fuzz_assert!(
                premium_up >= premium,
                "premium not monotone: f({})={} < f({})={}",
                spread + 1,
                premium_up,
                spread,
                premium
            );
        }
    }
}

/// Bonus invariant — `calculate_amm_funding_payment` is antisymmetric in the
/// cumulative-rate deltas: negating both side deltas negates the payment
/// exactly (the magnitude is sign-independent; the quote-ratio division
/// truncates symmetrically toward zero). Guards the AMM-as-counterparty
/// settlement math against a sign asymmetry that would let a funding flip
/// leak or absorb quote.
#[cfg(feature = "prop_amm_funding_antisymmetry")]
#[crucible_fuzz]
fn prop_amm_funding_antisymmetry(
    fixture: &mut FundingFixture,
    #[range(0..10_000_000_000_000u64)] base_long_mag: u64,
    #[range(0..10_000_000_000_000u64)] base_short_mag: u64,
    #[range(0..1_000_000_000_000u64)] long_delta_mag: u64,
    #[range(0..2u64)] long_delta_neg: u64,
    #[range(0..1_000_000_000_000u64)] short_delta_mag: u64,
    #[range(0..2u64)] short_delta_neg: u64,
) {
    let _ = &fixture.ctx;

    let base_long = base_long_mag as i128;
    let base_short = -(base_short_mag as i128);
    let long_delta = signed(long_delta_mag, long_delta_neg);
    let short_delta = signed(short_delta_mag, short_delta_neg);

    let pos =
        match calculate_amm_funding_payment(base_long, base_short, long_delta, short_delta, 0, 0) {
            Ok(v) => v,
            Err(_) => return,
        };
    let neg =
        match calculate_amm_funding_payment(base_long, base_short, -long_delta, -short_delta, 0, 0)
        {
            Ok(v) => v,
            Err(_) => return,
        };

    fuzz_assert_eq!(pos, -neg);

    // Sanity: quote conservation with per-side ratio division. The AMM payment
    // must equal the negated sum of the two user-side payments.
    let ul = match calculate_funding_payment_in_quote_precision(long_delta, base_long) {
        Ok(v) => v,
        Err(_) => return,
    };
    let us = match calculate_funding_payment_in_quote_precision(short_delta, base_short) {
        Ok(v) => v,
        Err(_) => return,
    };
    let net = pos + ul + us;
    fuzz_assert!(
        net.abs() <= 2,
        "amm/user not zero-sum: amm={} ul={} us={} net={}",
        pos,
        ul,
        us,
        net
    );
}
