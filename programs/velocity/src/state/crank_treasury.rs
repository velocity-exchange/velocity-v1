//! The protocol's single relay crank treasury.
//!
//! Every relay crank is paid from the market reservoir it cranks, and every
//! reservoir is refilled from here. One account is funded and watched; the
//! reservoirs top themselves up out of it.
//!
//! The payment stays on the market reservoir rather than moving here because
//! a crank writes whatever pays it, and a writable account has a fixed compute
//! budget per block. One account paying every crank would serialize the whole
//! protocol's cranks into that budget, and the moment that binds is a
//! market-wide move, when liquidations across many markets must land together.
//! A per-market reservoir keeps that ceiling per market. This account is
//! written only when a reservoir refills, which is rare.
//!
//! The refill is itself a relay crank, so nobody watches balances. A reservoir
//! mirrors its spendable lamports into its own account data, a condition on
//! that value wakes when it falls below the watermark, and the refill crank
//! moves lamports here to there. See
//! [`crate::state::clob_crank::ClobCrankConditionsV0::spendable_mirror`].
//!
//! Both levels are stated in cranks rather than lamports. A market's cranks
//! are priced from what they cost to land, so a market with an expensive cross
//! carries a proportionally larger float from the same setting, and one figure
//! serves every market.

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
    /// Wake the refill when a reservoir can pay fewer than this many.
    ///
    /// A refill needs two levels or it fills by nothing. This is the low one,
    /// and unlike the target it is *resolved to lamports at attach* and stored
    /// on the market, because it is the threshold relay compares the mirrored
    /// balance against and a condition carries its own threshold. Changing it
    /// therefore reaches a market on its next attach.
    ///
    /// Size it for the refill's own round trip. The refill is itself a relay
    /// crank — polled for, simulated, then landed — and the reservoir goes on
    /// paying for ordinary work throughout. Both terms are worst together: a
    /// market-wide move is when cranks fire fastest and when the network is
    /// slowest to land one, and a reservoir that runs dry stops cranking at
    /// exactly that point with nothing else to report it.
    pub refill_watermark_cranks: u16,
    /// Tail reserve, so a later field costs no migration.
    pub padding: [u8; 36],
}

// `padding` is longer than 32 bytes, which `#[derive(Default)]` does not cover
// (arrays only derive it up to 32).
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

    /// The balance a refill takes a reservoir to, given the most expensive
    /// crank that reservoir pays.
    ///
    /// Zero on a treasury nobody has priced yet, which stages no refill at
    /// all. That is the safe reading: a target at or below the wake level
    /// would leave the reservoir still due after a refill, and the condition
    /// would stay lit forever against an executor that can only revert.
    /// [`crate::instructions::handle_update_crank_treasury`] refuses a target
    /// that low, so a priced treasury always makes progress.
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
    /// The treasury is velocity-owned, so this is a direct lamport move rather
    /// than a system transfer. Both destinations — a market reservoir and a
    /// refill keeper — are credited the same way.
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
const _: () = assert!((CrankTreasuryV0::SIZE - 8) % 16 == 0);

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
        // A market whose cranks cost twice as much carries twice the float
        // from the same setting.
        assert_eq!(treasury.refill_target(20_000).unwrap(), 1_280_000);
    }

    /// Both levels scale with the same market price, so the hysteresis holds
    /// whatever a market's cranks cost.
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
