//! The protocol's single relay crank treasury.
//!
//! Every relay crank is paid from the market reservoir it cranks, and every
//! reservoir is refilled from here. One account is funded and watched, and the
//! reservoirs draw from it.
//!
//! The payment stays on the market reservoir rather than moving here. A crank
//! writes whatever pays it, and a writable account has a fixed compute budget
//! per block. One account that paid every crank would serialize the whole
//! protocol's cranks into that budget. That binds during a market-wide move,
//! when liquidations across many markets must land together. A per-market
//! reservoir keeps that ceiling per market. This account is written only when
//! a reservoir refills, which is rare.
//!
//! The refill is itself a relay crank, so no process watches balances. A
//! reservoir mirrors its spendable lamports into its own account data. A
//! condition on that value wakes when it falls below the watermark, and the
//! refill crank moves lamports from here to there. See
//! [`crate::state::clob_crank::ClobCrankConditionsV0::spendable_mirror`].
//!
//! Both levels are stated in cranks rather than lamports. A market's cranks
//! are priced from what they cost to land. A market with an expensive cross
//! then holds a proportionally larger balance from the same setting, so one
//! figure serves every market.

use {
    crate::{
        error::{ErrorCode, VelocityResult},
        math::safe_math::SafeMath,
    },
    anchor_lang::prelude::*,
};

/// PDA seed: `["crank_treasury"]`.
pub const CRANK_TREASURY_PDA_SEED: &[u8] = b"crank_treasury";

/// The protocol's lamport pool for relay cranks.
#[account(zero_copy(unsafe))]
#[derive(Debug, PartialEq, Eq)]
#[repr(C)]
pub struct CrankTreasuryV0 {
    /// Lifetime lamports paid to keepers that refilled a reservoir.
    pub total_paid: u64,
    /// Lifetime lamports moved out to market reservoirs.
    pub total_refilled: u64,
    /// Reserved.
    pub padding_u64: u64,
    /// Refill a reservoir up to this many of its most expensive crank.
    ///
    /// Read at refill time, so re-tuning it takes effect on every market at
    /// once.
    pub refill_target_cranks: u16,
    /// Wake the refill when a reservoir can pay fewer than this many. The attach resolves it to lamports
    /// on the market, so a change reaches a market only at its next attach. Size it for the refill's own
    /// round trip: itself a relay crank a turner polls, simulates and lands, worst together with
    /// ordinary work at a market-wide move, when an empty reservoir stops cranking unreported.
    pub refill_watermark_cranks: u16,
    /// Tail reserve, so a later field costs no migration.
    pub padding: [u8; 36],
}

// `#[derive(Default)]` covers arrays up to 32 elements only, and `padding`
// holds 36.
impl Default for CrankTreasuryV0 {
    fn default() -> Self {
        Self {
            total_paid: 0,
            total_refilled: 0,
            padding_u64: 0,
            refill_target_cranks: 0,
            refill_watermark_cranks: 0,
            padding: [0; 36],
        }
    }
}

impl CrankTreasuryV0 {
    pub const SIZE: usize = 8 + 8 + 8 + 8 + 2 + 2 + 36;

    /// The balance a refill takes a reservoir to, given the most expensive crank it pays. Returns zero
    /// on an unpriced treasury, staging no refill. A target at or below the wake level would leave the
    /// reservoir still due after a refill, stuck against an executor that can only revert.
    /// `handle_update_crank_treasury` refuses that target, so a priced treasury always progresses.
    pub fn refill_target(&self, max_crank_payment: u64) -> VelocityResult<u64> {
        max_crank_payment.safe_mul(u64::from(self.refill_target_cranks))
    }

    /// The balance a reservoir wakes its refill at, resolved to lamports for
    /// the market that stores it.
    pub fn refill_watermark(&self, max_crank_payment: u64) -> VelocityResult<u64> {
        max_crank_payment.safe_mul(u64::from(self.refill_watermark_cranks))
    }

    /// Move lamports out of the treasury, never below its own rent exemption.
    ///
    /// Velocity owns the treasury, so this is a direct lamport move rather
    /// than a system transfer. Both destinations take the same path, a market
    /// reservoir and a refill keeper.
    pub fn pay_out<'info>(
        treasury: &AccountInfo<'info>,
        recipient: &AccountInfo<'info>,
        amount: u64,
        rent_minimum: u64,
    ) -> Result<u64> {
        if amount == 0 {
            return Ok(0);
        }

        let available = treasury.lamports().saturating_sub(rent_minimum);
        if available < amount {
            msg!(
                "crank treasury holds {} spendable lamports, needs {}",
                available,
                amount
            );

            return Err(ErrorCode::InsufficientCrankTreasury.into());
        }

        **treasury.try_borrow_mut_lamports()? = treasury
            .lamports()
            .checked_sub(amount)
            .ok_or(ErrorCode::MathError)?;
        **recipient.try_borrow_mut_lamports()? = recipient
            .lamports()
            .checked_add(amount)
            .ok_or(ErrorCode::MathError)?;
        Ok(amount)
    }
}

// Zero-copy alignment invariant (see docs/alignment-and-native-offsets.md):
// no u128 fields, and `(SIZE - 8) % 16 == 0` so the struct sizes identically
// on x86_64 and SBF.
const _: () = assert!((CrankTreasuryV0::SIZE - 8).is_multiple_of(16));

#[cfg(test)]
mod tests {
    use super::*;

    /// An unpriced treasury stages nothing, rather than staging a refill that
    /// can only revert.
    #[test]
    fn an_unpriced_treasury_has_no_target() {
        assert_eq!(CrankTreasuryV0::default().refill_target(10_000).unwrap(), 0);
    }

    #[test]
    fn the_target_scales_with_what_the_market_pays() {
        let treasury = CrankTreasuryV0 {
            refill_target_cranks: 64,
            ..CrankTreasuryV0::default()
        };

        assert_eq!(treasury.refill_target(10_000).unwrap(), 640_000);
        // A market whose cranks cost twice as much holds twice the balance
        // from the same setting.
        assert_eq!(treasury.refill_target(20_000).unwrap(), 1_280_000);
    }

    /// Both levels scale with the same market price, so the target stays
    /// above the watermark whatever a market's cranks cost.
    #[test]
    fn a_priced_target_clears_the_watermark() {
        let treasury = CrankTreasuryV0 {
            refill_target_cranks: 1_000,
            refill_watermark_cranks: 100,
            ..CrankTreasuryV0::default()
        };

        assert!(treasury.refill_target(1_000).unwrap() > treasury.refill_watermark(1_000).unwrap());
        assert!(
            treasury.refill_target(50_000).unwrap() > treasury.refill_watermark(50_000).unwrap()
        );
    }
}
