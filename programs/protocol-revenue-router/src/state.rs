//! The router's singleton config account and its tier ladder.

use {crate::errors::RouterError, anchor_lang::prelude::*};

pub const ROUTER_CONFIG_SEED: &[u8] = b"router_config";
pub const MAX_TIERS: usize = 8;
pub const BPS_DENOMINATOR: u128 = 10_000;
pub const SECONDS_PER_DAY: i64 = 86_400;

/// One slice of the marginal ladder: gross from `threshold` up to the next
/// tier's threshold is split `pool_bps` to the recovery pool.
#[derive(
    AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, PartialEq, Eq, Debug, InitSpace,
)]
pub struct Tier {
    pub threshold: u64,
    pub pool_bps: u16,
}

#[account]
#[derive(InitSpace)]
pub struct RouterConfig {
    pub bump: u8,
    pub admin: Pubkey,
    pub cranker: Pubkey,
    pub usdt_mint: Pubkey,
    pub treasury: Pubkey,
    pub tiers: [Tier; MAX_TIERS],
    pub tier_count: u8,
    pub period_day: i64,
    pub period_fees: u128,
    pub lifetime_fees: u128,
    pub lifetime_to_pool: u128,
    pub lifetime_to_treasury: u128,
    pub _reserved: [u8; 128],
}

impl RouterConfig {
    pub fn active_tiers(&self) -> &[Tier] {
        &self.tiers[..self.tier_count as usize]
    }

    /// Validates and stores a ladder. Unused slots are zeroed so a shorter
    /// ladder never leaves stale thresholds behind.
    pub fn set_tiers(&mut self, tiers: &[Tier]) -> Result<()> {
        require!(!tiers.is_empty(), RouterError::EmptyTiers);
        require!(tiers.len() <= MAX_TIERS, RouterError::TooManyTiers);
        require!(
            tiers[0].threshold == 0,
            RouterError::FirstTierThresholdNotZero
        );
        for (i, tier) in tiers.iter().enumerate() {
            require!(
                u128::from(tier.pool_bps) <= BPS_DENOMINATOR,
                RouterError::InvalidBps
            );
            if i > 0 {
                require!(
                    tier.threshold > tiers[i - 1].threshold,
                    RouterError::TiersNotIncreasing
                );
            }
        }

        self.tiers = [Tier::default(); MAX_TIERS];
        self.tiers[..tiers.len()].copy_from_slice(tiers);
        self.tier_count = tiers.len() as u8;
        Ok(())
    }

    /// Rolls the daily period forward. A clock that moves backwards never resets.
    pub fn roll_period(&mut self, now_day: i64) {
        if now_day > self.period_day {
            self.period_day = now_day;
            self.period_fees = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank() -> RouterConfig {
        RouterConfig {
            bump: 0,
            admin: Pubkey::default(),
            cranker: Pubkey::default(),
            usdt_mint: Pubkey::default(),
            treasury: Pubkey::default(),
            tiers: [Tier::default(); MAX_TIERS],
            tier_count: 0,
            period_day: 0,
            period_fees: 0,
            lifetime_fees: 0,
            lifetime_to_pool: 0,
            lifetime_to_treasury: 0,
            _reserved: [0u8; 128],
        }
    }

    fn tier(threshold: u64, pool_bps: u16) -> Tier {
        Tier {
            threshold,
            pool_bps,
        }
    }

    #[test]
    fn set_tiers_accepts_the_agreed_ladder() {
        let mut config = blank();
        config
            .set_tiers(&[
                tier(0, 6000),
                tier(30_000_000_000, 7000),
                tier(100_000_000_000, 9000),
            ])
            .unwrap();
        assert_eq!(config.tier_count, 3);
        assert_eq!(config.active_tiers().len(), 3);
        assert_eq!(config.tiers[3], Tier::default());
    }

    #[test]
    fn set_tiers_zeroes_slots_left_by_a_longer_ladder() {
        let mut config = blank();
        config
            .set_tiers(&[tier(0, 6000), tier(30_000_000_000, 7000)])
            .unwrap();
        config.set_tiers(&[tier(0, 5000)]).unwrap();
        assert_eq!(config.tier_count, 1);
        assert_eq!(config.tiers[1], Tier::default());
    }

    #[test]
    fn set_tiers_rejects_empty() {
        assert!(blank().set_tiers(&[]).is_err());
    }

    #[test]
    fn set_tiers_rejects_nine_tiers() {
        let tiers: Vec<Tier> = (0..9).map(|i| tier(i as u64 * 1_000, 5000)).collect();
        assert!(blank().set_tiers(&tiers).is_err());
    }

    #[test]
    fn set_tiers_rejects_nonzero_first_threshold() {
        assert!(blank().set_tiers(&[tier(5, 6000)]).is_err());
    }

    #[test]
    fn set_tiers_rejects_non_increasing_thresholds() {
        assert!(blank()
            .set_tiers(&[tier(0, 6000), tier(100, 7000), tier(100, 8000)])
            .is_err());
    }

    #[test]
    fn set_tiers_rejects_bps_above_denominator() {
        assert!(blank().set_tiers(&[tier(0, 10_001)]).is_err());
    }

    #[test]
    fn roll_period_never_moves_backwards() {
        let mut config = blank();
        config.period_day = 100;
        config.period_fees = 42;

        config.roll_period(99);
        assert_eq!(config.period_day, 100);
        assert_eq!(config.period_fees, 42);

        config.roll_period(100);
        assert_eq!(config.period_fees, 42);

        config.roll_period(101);
        assert_eq!(config.period_day, 101);
        assert_eq!(config.period_fees, 0);
    }
}
