//! Marginal tier ladder arithmetic.

use crate::state::{Tier, BPS_DENOMINATOR};

/// Cumulative pool entitlement of the ladder at gross `x`, summed slice by slice.
pub fn pool_entitlement(tiers: &[Tier], x: u128) -> u128 {
    let mut acc = 0u128;
    for (i, tier) in tiers.iter().enumerate() {
        let lo = tier.threshold as u128;
        if x <= lo {
            break;
        }
        let hi = tiers
            .get(i + 1)
            .map(|t| t.threshold as u128)
            .unwrap_or(u128::MAX);
        let slice = x.min(hi) - lo;
        acc += slice * tier.pool_bps as u128 / BPS_DENOMINATOR;
    }
    acc
}

/// Pool share of `incoming` when `period_fees` has already been distributed today.
pub fn pool_share(tiers: &[Tier], period_fees: u128, incoming: u128) -> u128 {
    pool_entitlement(tiers, period_fees + incoming) - pool_entitlement(tiers, period_fees)
}

#[cfg(test)]
mod tests {
    use super::*;

    const USDT: u128 = 1_000_000;

    fn tier(threshold: u64, pool_bps: u16) -> Tier {
        Tier {
            threshold,
            pool_bps,
        }
    }

    fn ladder() -> Vec<Tier> {
        vec![
            tier(0, 6000),
            tier(30_000_000_000, 7000),
            tier(100_000_000_000, 9000),
        ]
    }

    #[test]
    fn agreed_ladder_splits_a_120k_day() {
        // 18k + 49k + 18k
        assert_eq!(pool_entitlement(&ladder(), 120_000 * USDT), 85_000_000_000);
        assert_eq!(pool_share(&ladder(), 0, 120_000 * USDT), 85_000_000_000);
    }

    #[test]
    fn chunking_the_day_does_not_change_the_split() {
        let whole = pool_share(&ladder(), 0, 120_000 * USDT);
        let first = pool_share(&ladder(), 0, 20_000 * USDT);
        let second = pool_share(&ladder(), 20_000 * USDT, 100_000 * USDT);
        assert_eq!(first + second, whole);
    }

    #[test]
    fn degenerate_ladders() {
        assert_eq!(pool_share(&[tier(0, 0)], 0, 120_000 * USDT), 0);
        assert_eq!(
            pool_share(&[tier(0, 10_000)], 0, 120_000 * USDT),
            120_000 * USDT
        );
    }

    #[test]
    fn zero_gross_entitles_nothing() {
        assert_eq!(pool_entitlement(&ladder(), 0), 0);
        assert_eq!(pool_share(&ladder(), 0, 0), 0);
    }
}
