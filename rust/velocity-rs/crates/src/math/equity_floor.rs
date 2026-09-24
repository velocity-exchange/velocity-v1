//! Client-side equity floor helpers, the Rust mirror of the TypeScript SDK's
//! `calculateEquityFloorAutoDelta` and `getEquityFloorLevel` (`math/margin.ts`).
//! The on-chain predicates live on `User` in the program crate,
//! `is_below_equity_floor` and `is_below_buffered_equity_floor`. This module
//! covers the two computations a client performs off-chain. It sizes the floor
//! delta a quote transfer must carry, and it classifies a subaccount's equity
//! against its floor thresholds for monitoring.
//!
//! These helpers take net equity, the metric the on-chain floor checks use.
//! The program crate computes it in `calculate_user_equity`: unweighted asset
//! value, plus funding-inclusive perp pnl, minus unweighted spot liability
//! value, at live oracle prices. It is not the weighted margin numerator
//! `total_collateral`.

/// Warning threshold multiple used when none is specified: warn while equity
/// is inside `floor + 2 * buffer`.
pub const DEFAULT_WARNING_BUFFER_MULTIPLE: u64 = 2;

/// Minimal equity floor delta to carry with a quote transfer of `amount` out
/// of a subaccount, so the debited side ends at or above its buffered floor
/// `equity_floor + equity_floor_buffer`. The first `net_equity - (floor +
/// buffer)` carries no floor; the remainder carries floor one for one,
/// capped at `equity_floor`. Zero when no floor is set. The result never
/// exceeds `amount`, so a credited side already at its buffered floor stays
/// there. All values are QUOTE_PRECISION.
pub fn calculate_equity_floor_auto_delta(
    amount: u64,
    net_equity: i128,
    equity_floor: u64,
    equity_floor_buffer: u64,
) -> u64 {
    if equity_floor == 0 {
        return 0;
    }
    let buffered_floor = (equity_floor as i128).saturating_add(equity_floor_buffer as i128);
    let excess = net_equity.saturating_sub(buffered_floor).max(0);
    let shortfall = (amount as i128).saturating_sub(excess).max(0) as u128;
    shortfall.min(equity_floor as u128) as u64
}

/// Severity of a subaccount's equity relative to its floor. Ordered least to
/// most severe (`Disabled < Healthy < Warning < Critical < Breached`), so the
/// worst level across subaccounts is `max`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EquityFloorLevel {
    /// No floor set; buffer is ignored and nothing is enforced.
    Disabled,
    /// At/above every threshold.
    Healthy,
    /// Inside `warning_buffer_multiple` buffers of the floor.
    Warning,
    /// Below `floor + buffer`: risk-increasing actions are rejecting.
    Critical,
    /// Below the floor: the permissionless breaker can trip.
    Breached,
}

/// Classifies `net_equity` against the floor thresholds, mirroring the
/// TypeScript `getEquityFloorLevel` so Rust and TS consumers report identical
/// levels. All comparisons are strict less-thans, matching the program's own
/// checks. All values QUOTE_PRECISION.
pub fn equity_floor_level(
    net_equity: i128,
    equity_floor: u64,
    equity_floor_buffer: u64,
    warning_buffer_multiple: u64,
) -> EquityFloorLevel {
    if equity_floor == 0 {
        return EquityFloorLevel::Disabled;
    }
    if net_equity < equity_floor as i128 {
        return EquityFloorLevel::Breached;
    }
    let buffered_floor = (equity_floor as i128).saturating_add(equity_floor_buffer as i128);
    if net_equity < buffered_floor {
        return EquityFloorLevel::Critical;
    }
    let warning_line = (equity_floor as i128).saturating_add(
        (equity_floor_buffer as i128).saturating_mul(warning_buffer_multiple as i128),
    );
    if net_equity < warning_line {
        return EquityFloorLevel::Warning;
    }
    EquityFloorLevel::Healthy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_delta_zero_when_floor_disabled() {
        assert_eq!(calculate_equity_floor_auto_delta(1_000, 50, 0, 500), 0);
    }

    #[test]
    fn auto_delta_zero_while_transfer_fits_inside_buffered_headroom() {
        // equity 1000, floor 300, buffer 100 -> excess 600
        assert_eq!(calculate_equity_floor_auto_delta(600, 1_000, 300, 100), 0);
    }

    #[test]
    fn auto_delta_carries_shortfall_one_for_one() {
        // excess 600, amount 700 -> delta 100
        assert_eq!(calculate_equity_floor_auto_delta(700, 1_000, 300, 100), 100);
    }

    #[test]
    fn auto_delta_caps_at_the_floor() {
        // excess 600, amount 950 -> uncapped 350, capped at floor 300
        assert_eq!(calculate_equity_floor_auto_delta(950, 1_000, 300, 100), 300);
    }

    #[test]
    fn auto_delta_handles_negative_collateral_and_extremes() {
        // deeply negative equity: everything up to the floor cap must move
        assert_eq!(
            calculate_equity_floor_auto_delta(1_000, -1_000_000, 300, 100),
            300
        );
        assert_eq!(
            calculate_equity_floor_auto_delta(u64::MAX, i128::MIN, u64::MAX, u64::MAX),
            u64::MAX
        );
        assert_eq!(
            calculate_equity_floor_auto_delta(u64::MAX, i128::MAX, u64::MAX, u64::MAX),
            0
        );
    }

    /// Mirrors the TS invariant fuzz: delta never exceeds amount or floor, and
    /// whenever the debit side started at/above its buffered floor and the cap
    /// did not bind, it ends at/above its reduced buffered floor, minimally.
    #[test]
    fn auto_delta_invariants_hold_under_fuzzing() {
        let mut state: u64 = 0x5EED_CAFE;
        let mut next = |modulus: u64| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) % modulus
        };

        for i in 0..20_000 {
            let net_equity = next(2_000_000) as i128;
            let floor = next(1_000_000);
            let buffer = next(200_000);
            let amount = next(2_000_000);

            let delta = calculate_equity_floor_auto_delta(amount, net_equity, floor, buffer);

            assert!(delta <= amount, "delta above amount at iteration {}", i);
            assert!(delta <= floor, "delta above floor at iteration {}", i);
            if floor == 0 {
                assert_eq!(delta, 0, "delta with disabled floor at iteration {}", i);
                continue;
            }

            let net_equity_after = net_equity - amount as i128;
            let floor_after = floor - delta;
            let started_above = net_equity >= (floor + buffer) as i128;
            if started_above && delta < floor && amount as i128 <= net_equity {
                assert!(
                    net_equity_after >= (floor_after + buffer) as i128,
                    "auto delta leaves debit side below buffered floor at iteration {}",
                    i
                );
                if delta > 0 {
                    assert!(
                        net_equity_after < (floor_after + 1 + buffer) as i128,
                        "auto delta is not minimal at iteration {}",
                        i
                    );
                }
            }
        }
    }

    #[test]
    fn level_boundaries_are_exact() {
        let level = |c: i128| equity_floor_level(c, 1_000, 100, DEFAULT_WARNING_BUFFER_MULTIPLE);
        assert_eq!(level(999), EquityFloorLevel::Breached);
        assert_eq!(level(1_000), EquityFloorLevel::Critical);
        assert_eq!(level(1_099), EquityFloorLevel::Critical);
        assert_eq!(level(1_100), EquityFloorLevel::Warning);
        assert_eq!(level(1_199), EquityFloorLevel::Warning);
        assert_eq!(level(1_200), EquityFloorLevel::Healthy);
    }

    #[test]
    fn level_disabled_without_floor() {
        assert_eq!(
            equity_floor_level(-5, 0, 100, DEFAULT_WARNING_BUFFER_MULTIPLE),
            EquityFloorLevel::Disabled
        );
    }

    #[test]
    fn level_honors_custom_warning_multiple() {
        assert_eq!(
            equity_floor_level(1_499, 1_000, 100, 5),
            EquityFloorLevel::Warning
        );
        assert_eq!(
            equity_floor_level(1_500, 1_000, 100, 5),
            EquityFloorLevel::Healthy
        );
    }

    #[test]
    fn level_zero_buffer_collapses_bands() {
        assert_eq!(
            equity_floor_level(999, 1_000, 0, DEFAULT_WARNING_BUFFER_MULTIPLE),
            EquityFloorLevel::Breached
        );
        assert_eq!(
            equity_floor_level(1_000, 1_000, 0, DEFAULT_WARNING_BUFFER_MULTIPLE),
            EquityFloorLevel::Healthy
        );
    }

    #[test]
    fn level_severity_orders_for_max_aggregation() {
        assert!(EquityFloorLevel::Breached > EquityFloorLevel::Critical);
        assert!(EquityFloorLevel::Critical > EquityFloorLevel::Warning);
        assert!(EquityFloorLevel::Warning > EquityFloorLevel::Healthy);
        assert!(EquityFloorLevel::Healthy > EquityFloorLevel::Disabled);
    }
}
