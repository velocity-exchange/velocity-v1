//! Pure math primitives for the per-`SpotMarket` collateral usage circuit breaker.
//!
//! These helpers are intentionally split out from the impl-method API on
//! `SpotMarket` / `SpotPosition` so they can be unit-tested without an account
//! fixture. All arithmetic is fixed-point in `SPOT_WEIGHT_PRECISION` units
//! (10_000 = 1.0×) and never involves floating-point.

use crate::error::DriftResult;
use crate::math::casting::Cast;
use crate::math::constants::SPOT_WEIGHT_PRECISION;
use crate::math::safe_math::SafeMath;

/// Returns the threshold (in `scaled_balance` units) above which `collateral_usage` is
/// considered "in breach." When the breaker is disabled (any of `twap_period == 0`,
/// `trigger_ratio_bps == 0`, or `twap == 0` — the bootstrap clause), returns
/// `u64::MAX` so that no delta can ever land above it.
///
/// `trigger_ratio_bps` is in `SPOT_WEIGHT_PRECISION` units (e.g. 20_000 = 2.0×).
pub fn collateral_usage_threshold_amount(
    collateral_usage_twap: u64,
    twap_period: u32,
    trigger_ratio_bps: u16,
) -> u64 {
    if twap_period == 0 || trigger_ratio_bps == 0 || collateral_usage_twap == 0 {
        return u64::MAX;
    }
    let twap = collateral_usage_twap as u128;
    let ratio = trigger_ratio_bps as u128;
    let denom = SPOT_WEIGHT_PRECISION as u128;
    let scaled = twap.saturating_mul(ratio) / denom;
    if scaled > u64::MAX as u128 {
        u64::MAX
    } else {
        scaled as u64
    }
}

/// Returns the position's current ramp factor in `SPOT_WEIGHT_PRECISION` units
/// (10_000 = full weight, 0 = zero weight). Encodes the discount rule:
///   - never stamped (`start == 0`) → full weight
///   - matured (`now >= end`)       → full weight
///   - active warmup                → linear interpolation, clamped to [0, 10_000]
pub fn warmup_factor_bps(warmup_start_ts: i64, warmup_end_ts: i64, now: i64) -> u32 {
    if warmup_start_ts == 0 || now >= warmup_end_ts {
        return SPOT_WEIGHT_PRECISION;
    }
    let duration = warmup_end_ts.saturating_sub(warmup_start_ts).max(1);
    let elapsed = now.saturating_sub(warmup_start_ts).max(0);
    if elapsed >= duration {
        return SPOT_WEIGHT_PRECISION;
    }
    let factor = (elapsed as u128)
        .saturating_mul(SPOT_WEIGHT_PRECISION as u128)
        / (duration as u128);
    if factor >= SPOT_WEIGHT_PRECISION as u128 {
        SPOT_WEIGHT_PRECISION
    } else {
        factor as u32
    }
}

/// Multiply a balance by a factor in `SPOT_WEIGHT_PRECISION` units, rounding down.
/// Returns the effective collateral contribution corresponding to this balance.
pub fn apply_warmup_factor(balance: u64, factor_bps: u32) -> u64 {
    let product = (balance as u128).saturating_mul(factor_bps as u128);
    (product / SPOT_WEIGHT_PRECISION as u128).min(u64::MAX as u128) as u64
}

/// Result of the rebase calculation: either "clear the warmup state" or "write back
/// these new (start, end) timestamps."
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RebaseDecision {
    /// Position's effective collateral now equals its full balance — clear `(start, end)`.
    Clear,
    /// Stamp the position with these new timestamps.
    Set { start: i64, end: i64 },
}

/// Compute the new `(warmup_start_ts, warmup_end_ts)` pair for a deposit increase, given
/// the partial-trigger split and the position's existing warmup state.
///
/// See `SpotPosition::rebase_collateral_usage_warmup_for_increase` for the algorithm.
/// This helper performs the pure arithmetic and returns a `RebaseDecision` that the
/// caller writes back into the position. All inputs/outputs are in fixed-point.
pub fn rebase_warmup_for_increase(
    pre_warmup_start_ts: i64,
    pre_warmup_end_ts: i64,
    pre_balance: u64,
    new_balance: u64,
    delta_below_threshold: u64,
    delta_above_threshold: u64,
    warmup_seconds: u32,
    now: i64,
) -> DriftResult<RebaseDecision> {
    // Edge case: degenerate `new_balance == 0` — nothing to ramp; clear the position.
    if new_balance == 0 {
        return Ok(RebaseDecision::Clear);
    }

    // Step 1: pre_factor based on existing (start, end).
    let pre_factor_bps = warmup_factor_bps(pre_warmup_start_ts, pre_warmup_end_ts, now);

    // Step 2: pre_effective + below-threshold portion of new delta = post_effective at `now`.
    let pre_effective = apply_warmup_factor(pre_balance, pre_factor_bps);
    let post_effective_now = pre_effective.saturating_add(delta_below_threshold);

    // Step 3: if at or above full balance, clear warmup state entirely.
    if post_effective_now >= new_balance {
        return Ok(RebaseDecision::Clear);
    }

    // Step 4: target factor R at `now` for the new total balance.
    let r_bps_u128 = (post_effective_now as u128)
        .saturating_mul(SPOT_WEIGHT_PRECISION as u128)
        / (new_balance as u128);
    // R must be strictly less than SPOT_WEIGHT_PRECISION because of step 3's guard.
    let r_bps = r_bps_u128.min((SPOT_WEIGHT_PRECISION - 1) as u128) as u32;

    // Step 5: choose new_end.
    // If we're adding any above-threshold collateral, that new tranche needs the full
    // `warmup_seconds` from now. Otherwise we preserve the existing maturity.
    let now_plus_w = now.safe_add(warmup_seconds as i64)?;
    let new_end = if delta_above_threshold > 0 {
        // max(now + W, old_warmup_end_ts)
        if pre_warmup_end_ts > now_plus_w {
            pre_warmup_end_ts
        } else {
            now_plus_w
        }
    } else {
        // delta_above_threshold == 0 with R < 1 means the position must have had
        // pre_factor < 1, which means pre_warmup_end_ts > now (was in active warmup).
        pre_warmup_end_ts
    };

    // Step 6: solve for new_start such that
    //   factor(now) = (now - new_start) / (new_end - new_start) = R
    //   new_start   = now - R * (new_end - now) / (1 - R)
    let new_duration_to_end = new_end.safe_sub(now)?.max(1);
    let one_minus_r_bps = (SPOT_WEIGHT_PRECISION as u128).saturating_sub(r_bps as u128).max(1);
    let offset = (r_bps as u128)
        .saturating_mul(new_duration_to_end as u128)
        / one_minus_r_bps;

    let offset_i64: i64 = offset.cast()?;
    let new_start = now.safe_sub(offset_i64)?;

    Ok(RebaseDecision::Set {
        start: new_start,
        end: new_end,
    })
}

/// Split a deposit `delta` into below/above-threshold portions given the *pre-add*
/// market state. The below-threshold portion is everything that fits under the current
/// threshold; everything else is above.
///
/// `threshold_amount == u64::MAX` (breaker disabled or bootstrap) → all below.
pub fn split_delta_at_threshold(
    pre_market_usage: u64,
    delta: u64,
    threshold_amount: u64,
) -> (u64, u64) {
    if threshold_amount == u64::MAX {
        return (delta, 0);
    }
    let headroom = threshold_amount.saturating_sub(pre_market_usage);
    let below = headroom.min(delta);
    let above = delta - below;
    (below, above)
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 172_800; // 48h
    const SPOT: u32 = SPOT_WEIGHT_PRECISION;

    #[test]
    fn threshold_disabled_when_period_zero() {
        assert_eq!(collateral_usage_threshold_amount(100, 0, 20_000), u64::MAX);
    }

    #[test]
    fn threshold_disabled_when_ratio_zero() {
        assert_eq!(collateral_usage_threshold_amount(100, W, 0), u64::MAX);
    }

    #[test]
    fn threshold_bootstrap_when_twap_zero() {
        assert_eq!(collateral_usage_threshold_amount(0, W, 20_000), u64::MAX);
    }

    #[test]
    fn threshold_normal_2x() {
        // twap=100, ratio=2.0× → threshold=200
        assert_eq!(collateral_usage_threshold_amount(100, W, 20_000), 200);
    }

    #[test]
    fn warmup_factor_never_stamped_is_full() {
        assert_eq!(warmup_factor_bps(0, 0, 1_000), SPOT);
        assert_eq!(warmup_factor_bps(0, 5_000, 1_000), SPOT);
    }

    #[test]
    fn warmup_factor_matured_is_full() {
        assert_eq!(warmup_factor_bps(100, 200, 200), SPOT);
        assert_eq!(warmup_factor_bps(100, 200, 300), SPOT);
    }

    #[test]
    fn warmup_factor_midway() {
        // start=100, end=200, now=150 → factor=5_000 (50%)
        assert_eq!(warmup_factor_bps(100, 200, 150), 5_000);
    }

    #[test]
    fn warmup_factor_at_start_is_zero() {
        assert_eq!(warmup_factor_bps(1, 100, 1), 0);
    }

    #[test]
    fn split_delta_disabled() {
        let (below, above) = split_delta_at_threshold(0, 100, u64::MAX);
        assert_eq!(below, 100);
        assert_eq!(above, 0);
    }

    #[test]
    fn split_delta_entirely_below() {
        // pre=0, threshold=200, delta=100 → all below
        let (below, above) = split_delta_at_threshold(0, 100, 200);
        assert_eq!(below, 100);
        assert_eq!(above, 0);
    }

    #[test]
    fn split_delta_entirely_above() {
        // pre=300 (already over), threshold=200, delta=100 → all above
        let (below, above) = split_delta_at_threshold(300, 100, 200);
        assert_eq!(below, 0);
        assert_eq!(above, 100);
    }

    #[test]
    fn split_delta_straddles_threshold() {
        // pre=5, threshold=10, delta=10 → below=5, above=5
        let (below, above) = split_delta_at_threshold(5, 10, 10);
        assert_eq!(below, 5);
        assert_eq!(above, 5);
    }

    /// User-spec scenario: market usage=5, threshold=10, user adds 10.
    /// 5 of the 10 is "their shit that needs to be waited on."
    #[test]
    fn rebase_partial_trigger_user_spec() {
        // User starts with 0 balance, deposits 10.
        // pre_balance=0, new_balance=10. delta_below=5, delta_above=5.
        let result = rebase_warmup_for_increase(
            0, 0,    // never stamped
            0, 10,   // pre/new balance
            5, 5,    // delta split
            W,
            1_000,   // now
        )
        .unwrap();
        match result {
            RebaseDecision::Set { start, end } => {
                // R = 5/10 = 0.5. new_end = now + W. new_start = now - 0.5*W/0.5 = now - W.
                assert_eq!(end, 1_000 + W as i64);
                assert_eq!(start, 1_000 - W as i64);
                // Verify factor at now == 0.5
                let factor = warmup_factor_bps(start, end, 1_000);
                assert!(
                    (factor as i32 - 5_000).abs() <= 1,
                    "factor={} expected ~5000",
                    factor
                );
                // Verify factor at end == 1.0
                assert_eq!(warmup_factor_bps(start, end, end), SPOT);
            }
            other => panic!("expected Set, got {:?}", other),
        }
    }

    #[test]
    fn rebase_below_only_on_unstamped_clears() {
        // pre never stamped, deposit entirely below threshold → clear (already at full weight).
        let result = rebase_warmup_for_increase(0, 0, 0, 10, 10, 0, W, 1_000).unwrap();
        assert_eq!(result, RebaseDecision::Clear);
    }

    #[test]
    fn rebase_below_only_on_warming_position_preserves_end() {
        // Position currently warming: start=1, end=2W, now=W → factor≈0.5 on pre_balance=10_000.
        // pre_effective ≈ 5_000. Deposit 10_000 entirely below threshold. new_balance=20_000.
        // post_effective ≈ 15_000. R ≈ 0.75. new_end preserved at old end = 2W.
        // Larger balances avoid integer-rounding artifacts at the 1-bp scale.
        let now: i64 = W as i64;
        let pre_start: i64 = 1;
        let pre_end: i64 = 2 * W as i64;
        let result = rebase_warmup_for_increase(
            pre_start,
            pre_end,
            10_000,
            20_000,
            10_000, // delta_below
            0,      // delta_above
            W,
            now,
        )
        .unwrap();
        match result {
            RebaseDecision::Set { start, end } => {
                assert_eq!(end, 2 * W as i64, "end must be preserved");
                let factor = warmup_factor_bps(start, end, now);
                assert!(
                    (factor as i32 - 7_500).abs() <= 5,
                    "factor={} expected ~7500",
                    factor
                );
            }
            other => panic!("expected Set, got {:?}", other),
        }
    }

    #[test]
    fn rebase_above_only_fresh_stamp() {
        // Never stamped, all above threshold → fresh ramp.
        let result = rebase_warmup_for_increase(0, 0, 0, 10, 0, 10, W, 1_000).unwrap();
        match result {
            RebaseDecision::Set { start, end } => {
                assert_eq!(start, 1_000);
                assert_eq!(end, 1_000 + W as i64);
            }
            other => panic!("expected Set, got {:?}", other),
        }
    }

    #[test]
    fn rebase_continuity_across_active_warmup() {
        // Position stamped at factor≈0.5 (start=1, end=2W, pre_balance=10, now=W).
        // Add 10 more, all above threshold. Expect continuity: post_effective_at_now ≈ 5.
        let now: i64 = W as i64;
        let result = rebase_warmup_for_increase(1, 2 * W as i64, 10, 20, 0, 10, W, now).unwrap();
        match result {
            RebaseDecision::Set { start, end } => {
                let factor = warmup_factor_bps(start, end, now);
                let post_eff = apply_warmup_factor(20, factor);
                assert!(
                    (post_eff as i64 - 5).abs() <= 1,
                    "post_effective at now should be ~5, got {}",
                    post_eff
                );
                // new_end = max(now+W, old_end) = max(2W, 2W) = 2W
                assert_eq!(end, 2 * W as i64);
            }
            other => panic!("expected Set, got {:?}", other),
        }
    }

    #[test]
    fn rebase_zero_new_balance_clears() {
        let result = rebase_warmup_for_increase(0, 0, 0, 0, 0, 0, W, 1_000).unwrap();
        assert_eq!(result, RebaseDecision::Clear);
    }

    #[test]
    fn rebase_above_only_extends_old_end_when_old_further() {
        // Position in warmup with end far in future (e.g. 5W from now). Add above-threshold.
        // Should keep old end (it's > now+W).
        let now: i64 = 0;
        let old_end = 5 * W as i64;
        let result = rebase_warmup_for_increase(0, old_end, 10, 20, 0, 10, W, now).unwrap();
        match result {
            RebaseDecision::Set { end, .. } => {
                assert_eq!(end, old_end);
            }
            other => panic!("expected Set, got {:?}", other),
        }
    }
}
