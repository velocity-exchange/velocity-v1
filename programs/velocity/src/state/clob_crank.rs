//! Relay condition block for a perp market's CLOB cranks.
//!
//! Relay turners discover work by reading a *condition block* — a
//! `relay-spec` wire structure naming, per condition, when to wake, which
//! instruction to simulate to find work (the resolver), and which instruction
//! does it (the executor). The block lives on a velocity-owned account rather
//! than the CLOB's for two reasons the plan settles: removal has to adjust the
//! maker's `User` (open-order aggregates and the reward debit), which only
//! velocity can do; and the resolver has to stage *velocity's* account list.
//! `ConditionV0` names its wake account explicitly, so a velocity-hosted
//! condition watching foreign CLOB bytes is native to the spec — nothing is
//! mirrored.
//!
//! Two conditions per market, in fixed slots so the resolver can address them
//! by index:
//!
//! - [`CLOB_CRANK_EVICT`] — `WakeKind::OnAccountChange` over the CLOB market's
//!   `bid_count` / `ask_count`, so a turner wakes when the book grows toward
//!   its soft cap.
//! - [`CLOB_CRANK_EXPIRE`] — `WakeKind::AtTimestamp`. Velocity mediates every
//!   placement (`place_clob_order`), so it maintains a min-over-inserts
//!   `wake_ts` hint here as it places; the executor recomputes the true value
//!   as it works, repairing the hint.
//!
//! The block is held as an opaque byte region accessed through
//! `relay_spec::read_block` / `read_block_mut` rather than as typed fields.
//! That keeps `relay-spec`'s pod types out of velocity's zero-copy layout —
//! the region's size is the only thing this account commits to — and means a
//! spec revision that adds a field is a version bump here, not a layout
//! migration.
//!
//! `block` is the FIRST field so it begins at offset 8 (past anchor's
//! discriminator), which is the 8-aligned offset `read_block` requires.

use anchor_lang::prelude::*;
use relay_spec::bytemuck::Zeroable;
use relay_spec::{ConditionBlockHeaderV0, BLOCK_HEADER_LEN, CONDITION_LEN};

use crate::error::ErrorCode;
use crate::msg;

/// Index of the evict condition (book grew toward its soft cap).
pub const CLOB_CRANK_EVICT: usize = 0;
/// Index of the expire condition (an order's `wake_ts` came due).
pub const CLOB_CRANK_EXPIRE: usize = 1;
/// Conditions hosted per market.
pub const CLOB_CRANK_CONDITIONS: usize = 2;

/// Bytes the condition block occupies: header + the fixed condition array.
pub const CLOB_CRANK_BLOCK_LEN: usize = BLOCK_HEADER_LEN + CLOB_CRANK_CONDITIONS * CONDITION_LEN;

#[account(zero_copy(unsafe))]
#[derive(Debug)]
#[repr(C)]
pub struct ClobCrankConditionsV0 {
    /// The relay condition block, read in place by turners and rewritten in
    /// place by velocity. First field, so it sits at the 8-aligned offset 8.
    pub block: [u8; CLOB_CRANK_BLOCK_LEN],
    /// Lamports the executor pays the keeper per crank, mirrored into both
    /// conditions' `min_payment`. This account doubles as the reservoir those
    /// lamports come from: relay's `assert_paid_v0` measures the keeper's
    /// lamport balance, so a crank that moves no lamports cannot express a
    /// fee, and turners would have no signal to prioritize (or decline) the
    /// work. Held here rather than in a global PDA because the executor
    /// already has to touch this account to repair the expiry hint — so the
    /// reservoir costs no extra account in a crank transaction. Kept topped
    /// off out of band (gas station); an empty reservoir stops cranks rather
    /// than silently paying nothing, which is the failure ops can see.
    pub keeper_payment_lamports: u64,
    /// The perp market these conditions crank. Also the PDA seed.
    pub market_index: u16,
    pub padding: [u8; 6],
}

impl Default for ClobCrankConditionsV0 {
    fn default() -> Self {
        // `[u8; N]` derives Default only up to N = 32.
        Self {
            block: [0; CLOB_CRANK_BLOCK_LEN],
            keeper_payment_lamports: 0,
            market_index: 0,
            padding: [0; 6],
        }
    }
}

impl ClobCrankConditionsV0 {
    /// 8 (discriminator) + block + trailing fields. Kept as a const so the
    /// alignment invariant below is checked at compile time.
    pub const SIZE: usize = 8 + CLOB_CRANK_BLOCK_LEN + 8 + 2 + 6;

    /// The block region, for `relay_spec::read_block`.
    pub fn block(&self) -> &[u8] {
        &self.block
    }

    /// The block region for in-place updates (e.g. the expiry `wake_ts` hint).
    pub fn block_mut(&mut self) -> &mut [u8] {
        &mut self.block
    }

    /// Stamp the spec header. Conditions are written separately, by index, so
    /// a fresh account is a valid (if inactive) block from the first write.
    pub fn init_header(&mut self) -> Result<()> {
        let header = ConditionBlockHeaderV0::new(CLOB_CRANK_CONDITIONS as u8);
        self.block[..BLOCK_HEADER_LEN].copy_from_slice(bytemuck::bytes_of(&header));
        Ok(())
    }

    /// Overwrite one condition slot. `index` must be one of the two constants
    /// above — the slots are fixed so the resolver can address them.
    pub fn write_condition(
        &mut self,
        index: usize,
        condition: &relay_spec::ConditionV0,
    ) -> Result<()> {
        if index >= CLOB_CRANK_CONDITIONS {
            msg!(
                "clob crank condition index {} exceeds {}",
                index,
                CLOB_CRANK_CONDITIONS
            );
            return Err(ErrorCode::DefaultError.into());
        }
        let start = BLOCK_HEADER_LEN + index * CONDITION_LEN;
        self.block[start..start + CONDITION_LEN].copy_from_slice(bytemuck::bytes_of(condition));
        Ok(())
    }

    /// Move `keeper_payment_lamports` from the conditions account to `keeper`,
    /// so relay's `assert_paid_v0` sees the keeper's balance grow.
    ///
    /// Velocity owns this PDA, so the debit is a direct lamport mutation — a
    /// system-program transfer would need the PDA to sign, and only the owning
    /// program may decrement an account's lamports anyway. The reservoir must
    /// stay rent-exempt: dropping below the minimum would make the account
    /// purgeable and take the market's conditions with it. When it can't cover
    /// the payment the crank fails here rather than underpaying, because an
    /// underpaid crank fails `assert_paid_v0` after doing the work — same
    /// revert, but the reason would be buried in relay instead of naming the
    /// empty reservoir.
    ///
    /// Returns the lamports paid.
    pub fn pay_keeper_lamports<'info>(
        conditions: &AccountInfo<'info>,
        keeper: &AccountInfo<'info>,
        amount: u64,
        rent_minimum: u64,
    ) -> Result<u64> {
        if amount == 0 {
            return Ok(0);
        }
        let available = conditions.lamports().saturating_sub(rent_minimum);
        if available < amount {
            msg!(
                "clob crank reservoir {} holds {} spendable lamports, needs {}",
                conditions.key(),
                available,
                amount
            );
            return Err(ErrorCode::InsufficientCrankReservoir.into());
        }
        **conditions.try_borrow_mut_lamports()? = conditions
            .lamports()
            .checked_sub(amount)
            .ok_or(ErrorCode::MathError)?;
        **keeper.try_borrow_mut_lamports()? = keeper
            .lamports()
            .checked_add(amount)
            .ok_or(ErrorCode::MathError)?;
        Ok(amount)
    }

    /// Read one condition slot back.
    pub fn read_condition(&self, index: usize) -> Result<relay_spec::ConditionV0> {
        if index >= CLOB_CRANK_CONDITIONS {
            return Err(ErrorCode::DefaultError.into());
        }
        let start = BLOCK_HEADER_LEN + index * CONDITION_LEN;
        Ok(bytemuck::pod_read_unaligned(
            &self.block[start..start + CONDITION_LEN],
        ))
    }
}

// The block must start at an 8-aligned offset for `read_block`'s zero-copy
// cast; anchor's discriminator puts field 0 at offset 8.
const _: () = assert!(BLOCK_HEADER_LEN % 8 == 0);
const _: () = assert!(CONDITION_LEN % 8 == 0);

// Zero-copy alignment invariant (see docs/alignment-and-native-offsets.md):
// no u128 fields, and `(SIZE - 8) % 16 == 0` so the struct sizes identically
// on x86_64 and SBF.
const _: () = assert!((ClobCrankConditionsV0::SIZE - 8) % 16 == 0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_matches_the_layout_and_the_spec() {
        assert_eq!(CLOB_CRANK_BLOCK_LEN, 16 + 2 * 280);
        assert_eq!(ClobCrankConditionsV0::SIZE, 8 + 576 + 16);
        // the u64 reservoir field must land 8-aligned, right after the block
        assert_eq!(std::mem::align_of::<ClobCrankConditionsV0>(), 8);
        assert_eq!(
            std::mem::size_of::<ClobCrankConditionsV0>(),
            ClobCrankConditionsV0::SIZE - 8
        );
    }

    #[test]
    fn header_then_conditions_round_trip_through_the_spec() {
        let mut acct = ClobCrankConditionsV0::default();
        acct.init_header().unwrap();

        let mut condition = relay_spec::ConditionV0::zeroed();
        condition.wake_ts = 1_234;
        condition.active = 1;
        acct.write_condition(CLOB_CRANK_EXPIRE, &condition).unwrap();

        // relay's own reader must accept what we wrote, at offset 0 of the
        // region (offset 8 of the account).
        let (header, conditions) = relay_spec::read_block(acct.block(), 0).unwrap();
        assert_eq!(header.num_conditions, CLOB_CRANK_CONDITIONS as u8);
        assert_eq!(conditions.len(), CLOB_CRANK_CONDITIONS);
        assert_eq!(conditions[CLOB_CRANK_EXPIRE].wake_ts, 1_234);
        assert_eq!(conditions[CLOB_CRANK_EXPIRE].active, 1);
        // The untouched slot is a zeroed (inactive) condition, not garbage.
        assert_eq!(conditions[CLOB_CRANK_EVICT].active, 0);

        assert_eq!(
            acct.read_condition(CLOB_CRANK_EXPIRE).unwrap().wake_ts,
            1_234
        );
    }

    #[test]
    fn out_of_range_condition_index_is_rejected() {
        let mut acct = ClobCrankConditionsV0::default();
        acct.init_header().unwrap();
        assert!(acct
            .write_condition(CLOB_CRANK_CONDITIONS, &relay_spec::ConditionV0::zeroed())
            .is_err());
        assert!(acct.read_condition(CLOB_CRANK_CONDITIONS).is_err());
    }
}
