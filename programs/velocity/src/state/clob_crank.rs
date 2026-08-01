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
//! Three conditions per market, in fixed slots so the resolver can address
//! them by index:
//!
//! - [`CLOB_CRANK_EVICT`] — `WakeKind::OnAccountChange` over the CLOB market's
//!   `bid_count` / `ask_count`, so a turner wakes when the book grows toward
//!   its soft cap.
//! - [`CLOB_CRANK_EXPIRE`] — `WakeKind::AtTimestamp`. Velocity mediates every
//!   placement (`place_clob_order`), so it maintains a min-over-inserts
//!   `wake_ts` hint here as it places; the executor recomputes the true value
//!   as it works, repairing the hint.
//! - [`CLOB_CRANK_EXPIRE_FALLBACK`] — `WakeKind::EverySlots`, the same
//!   resolver/executor as the expire condition. The hint above is best-effort:
//!   `place_clob_order` takes the conditions account as an *optional* account
//!   (placement must not brick on a market whose conditions were never
//!   initialized), so an expiring placement that omits it would otherwise be
//!   work with no wake — the liveness bug the spec warns about. The fallback
//!   poll makes a missed hint cost latency, never liveness.
//!
//! The account also hosts the `staging` region resolvers write their
//! `ResolvedCrankV0` (executor account list + args) into. Resolvers are only
//! ever simulated, so the write never lands on chain — the region is scratch
//! that costs rent but no write contention.
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

use {
    crate::error::ErrorCode,
    anchor_lang::prelude::*,
    relay_spec::{ConditionBlock, BLOCK_HEADER_LEN, CONDITION_LEN},
};

/// PDA seed: `["clob_crank_conditions", market_index]`.
pub const CLOB_CRANK_CONDITIONS_PDA_SEED: &[u8] = b"clob_crank_conditions";

/// Index of the evict condition (book grew toward its soft cap).
pub const CLOB_CRANK_EVICT: usize = 0;
/// Index of the expire condition (an order's `wake_ts` came due).
pub const CLOB_CRANK_EXPIRE: usize = 1;
/// Index of the expire fallback (periodic poll catching missed hints).
pub const CLOB_CRANK_EXPIRE_FALLBACK: usize = 2;
/// Index of the cross condition (the book's best bid/ask moved — a crossing
/// order is by definition a new best, so the watch catches every new cross).
pub const CLOB_CRANK_CROSS: usize = 3;
/// Index of the cross fallback (periodic poll — the liveness floor for
/// PropAMM-side crosses, which have no single account to watch).
pub const CLOB_CRANK_CROSS_FALLBACK: usize = 4;
/// Index of the cross-activation condition: `WakeKind::AtSlot` over the
/// minimum *upcoming* `activation_slot` on the book. Activation changes
/// nothing on-chain, but it is exactly when makers who lined up against a
/// speed-bumped order expect the cross to fire — so the program tells
/// turners the slot precisely, min-folded at placement and repaired to the
/// next future activation (or `u64::MAX`) by every landing crank's scan.
pub const CLOB_CRANK_CROSS_ACTIVATION: usize = 5;
/// Conditions hosted per market.
pub const CLOB_CRANK_CONDITIONS: usize = 6;

/// Bytes the condition block occupies: header + the fixed condition array.
pub const CLOB_CRANK_BLOCK_LEN: usize = BLOCK_HEADER_LEN + CLOB_CRANK_CONDITIONS * CONDITION_LEN;

/// Bytes reserved for a resolver's staged `ResolvedCrankV0`. The largest
/// staged call is the cross executor: ~12 fixed accounts plus two accounts
/// per staged maker (33 bytes each), so this bounds a cross at roughly two
/// dozen makers — far past what one profitable top-of-book cross touches.
/// The whole account must stay under the 10,240-byte CPI allocation limit
/// so the attach ix can `init` it in one step.
pub const CLOB_CRANK_STAGING_LEN: usize = 2048;

/// Account-data offset of the staging region (what a `ResponsePointerV0`'s
/// `offset` is relative to): discriminator + the block.
pub const CLOB_CRANK_STAGING_OFFSET: usize = 8 + CLOB_CRANK_BLOCK_LEN;

/// Every condition on this account resolves with the same four accounts,
/// so the list is stored once and each condition points at it. Padded to
/// keep (SIZE - 8) %% 16 == 0 (see docs/alignment-and-native-offsets.md).
pub const CLOB_CRANK_RESOLVERS_MAX: usize = 4;
pub const CLOB_CRANK_RESOLVERS_LEN: usize = 144;
const _: () =
    assert!(CLOB_CRANK_RESOLVERS_LEN >= CLOB_CRANK_RESOLVERS_MAX * relay_spec::ACCOUNT_REF_LEN);
pub const CLOB_CRANK_RESOLVERS_OFFSET: usize = CLOB_CRANK_STAGING_OFFSET + CLOB_CRANK_STAGING_LEN;

#[account(zero_copy(unsafe))]
#[derive(Debug)]
#[repr(C)]
pub struct ClobCrankConditionsV0 {
    /// The relay condition block, read in place by turners and rewritten in
    /// place by velocity. First field, so it sits at the 8-aligned offset 8.
    pub block: [u8; CLOB_CRANK_BLOCK_LEN],
    /// Scratch the resolvers stage their `ResolvedCrankV0` into. Only ever
    /// written under simulation; on-chain contents are meaningless.
    pub staging: [u8; CLOB_CRANK_STAGING_LEN],
    /// The resolver account list every condition here points at.
    pub resolvers: [u8; CLOB_CRANK_RESOLVERS_LEN],
    /// The market's oracle, captured at attach time. Resolvers hold only
    /// four fixed accounts, so the staged executor's map section is derived
    /// from here rather than from the perp market account; an admin oracle
    /// rotation goes live for the cranks on re-attach.
    pub oracle: Pubkey,
    /// Lamports the executor pays the keeper per crank, mirrored into each
    /// conditions' `min_payment`. This account doubles as the reservoir those
    /// lamports come from: relay's `assert_paid_v0` measures the keeper's
    /// lamport balance, so a crank that moves no lamports cannot express a
    /// fee, and turners would have no signal to prioritize (or decline) the
    /// work. Held here rather than in a global PDA because the executor
    /// already has to touch this account to repair the expiry hint — so the
    /// reservoir costs no extra account in a crank transaction.
    ///
    /// Refilled by the maker, not the protocol: the flat removal reward the
    /// maker pays accrues to a protocol-owned `User`, and a hot role withdraws
    /// that quote and converts it to SOL to top these reservoirs off. An empty
    /// reservoir stops cranks rather than silently paying nothing, which is the
    /// failure mode ops can actually see.
    pub keeper_payment_lamports: u64,
    /// The perp market these conditions crank. Also the PDA seed.
    pub market_index: u16,
    /// The market's quote spot market, captured at attach time (the staged
    /// executor's map section needs its PDA).
    pub quote_spot_market_index: u16,
    pub padding: [u8; 4],
}

impl Default for ClobCrankConditionsV0 {
    fn default() -> Self {
        // `[u8; N]` derives Default only up to N = 32.
        Self {
            block: [0; CLOB_CRANK_BLOCK_LEN],
            staging: [0; CLOB_CRANK_STAGING_LEN],
            resolvers: [0; CLOB_CRANK_RESOLVERS_LEN],
            oracle: Pubkey::default(),
            keeper_payment_lamports: 0,
            market_index: 0,
            quote_spot_market_index: 0,
            padding: [0; 4],
        }
    }
}

impl ClobCrankConditionsV0 {
    /// 8 (discriminator) + block + staging + trailing fields. Kept as a const
    /// so the alignment invariant below is checked at compile time.
    pub const SIZE: usize = 8
        + CLOB_CRANK_BLOCK_LEN
        + CLOB_CRANK_STAGING_LEN
        + CLOB_CRANK_RESOLVERS_LEN
        + 32
        + 8
        + 2
        + 2
        + 4;

    /// Store the resolver account list and describe where it landed.
    pub fn write_resolvers(
        &mut self,
        refs: &[relay_spec::AccountRefV0],
    ) -> Result<relay_spec::ResolverListV0> {
        if refs.len() > CLOB_CRANK_RESOLVERS_MAX {
            return Err(ErrorCode::DefaultError.into());
        }
        for (i, r) in refs.iter().enumerate() {
            let at = i * relay_spec::ACCOUNT_REF_LEN;
            self.resolvers[at..at + 32].copy_from_slice(&r.address);
            self.resolvers[at + 32] = r.writable;
        }
        Ok(relay_spec::ResolverListV0::new(
            CLOB_CRANK_RESOLVERS_OFFSET as u32,
            refs.len() as u8,
        ))
    }

    /// The block region, for `relay_spec::read_block`.
    pub fn block(&self) -> &[u8] {
        &self.block
    }

    /// Anchor-flavoured wrappers over the spec trait's provided methods,
    /// so handlers keep using `?` with the program's own error type.
    pub fn init_block(&mut self) -> Result<()> {
        ConditionBlock::init_header(self).map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn set_condition(
        &mut self,
        index: usize,
        condition: &relay_spec::ConditionV0,
    ) -> Result<()> {
        ConditionBlock::write_condition(self, index, condition)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn get_condition(&self, index: usize) -> Result<relay_spec::ConditionV0> {
        ConditionBlock::read_condition(self, index).map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn edit_condition(
        &mut self,
        index: usize,
        f: impl FnOnce(&mut relay_spec::ConditionV0),
    ) -> Result<()> {
        ConditionBlock::update_condition(self, index, f)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn clear_condition(&mut self, index: usize) -> Result<()> {
        ConditionBlock::deactivate_condition(self, index)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    /// The block region for in-place updates (e.g. the expiry `wake_ts` hint).
    pub fn block_mut(&mut self) -> &mut [u8] {
        &mut self.block
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

    /// The expire condition's `wake_ts` hint.
    pub fn expire_wake_ts(&self) -> Result<i64> {
        Ok(self.get_condition(CLOB_CRANK_EXPIRE)?.wake_ts)
    }

    /// Min-fold a newly placed order's `max_ts` into the expire hint — the
    /// cheap, conservative maintenance `place_clob_order` does. Only ever
    /// moves the wake earlier, so it can never make the hint fire late.
    pub fn note_expiry(&mut self, max_ts: i64) -> Result<()> {
        let conditions = relay_spec::read_block_mut(&mut self.block, 0)
            .map_err(|_| error!(ErrorCode::DefaultError))?;
        let hint = &mut conditions[CLOB_CRANK_EXPIRE].wake_ts;
        *hint = (*hint).min(max_ts);
        Ok(())
    }

    /// Overwrite the expire hint with a recomputed true minimum (`i64::MAX`
    /// when no live order expires) — what the crank executor does after a
    /// removal, so a due hint goes quiet instead of firing forever.
    pub fn repair_expiry(&mut self, true_min_ts: i64) -> Result<()> {
        let conditions = relay_spec::read_block_mut(&mut self.block, 0)
            .map_err(|_| error!(ErrorCode::DefaultError))?;
        conditions[CLOB_CRANK_EXPIRE].wake_ts = true_min_ts;
        Ok(())
    }

    /// The cross-activation condition's `wake_slot` hint.
    pub fn activation_wake_slot(&self) -> Result<u64> {
        Ok(self.get_condition(CLOB_CRANK_CROSS_ACTIVATION)?.wake_slot)
    }

    /// Min-fold a newly placed order's activation slot into the
    /// cross-activation wake — cheap, conservative placement-side
    /// maintenance, exactly like [`Self::note_expiry`]. Only future slots
    /// matter: an already-active placement is covered by the cross
    /// condition's change-watch over the book's bests.
    pub fn note_activation(&mut self, activation_slot: u64) -> Result<()> {
        let conditions = relay_spec::read_block_mut(&mut self.block, 0)
            .map_err(|_| error!(ErrorCode::DefaultError))?;
        let hint = &mut conditions[CLOB_CRANK_CROSS_ACTIVATION].wake_slot;
        *hint = (*hint).min(activation_slot);
        Ok(())
    }

    /// Overwrite the cross-activation hint with the recomputed minimum
    /// *future* activation (`u64::MAX` when nothing is pending) — every
    /// landing crank does this from its book scan, so a fired hint goes
    /// quiet even when the activation produced no cross.
    pub fn repair_activation(&mut self, min_future_slot: u64) -> Result<()> {
        let conditions = relay_spec::read_block_mut(&mut self.block, 0)
            .map_err(|_| error!(ErrorCode::DefaultError))?;
        conditions[CLOB_CRANK_CROSS_ACTIVATION].wake_slot = min_future_slot;
        Ok(())
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

/// Block hosting + staging, from the spec (see
/// [`relay_spec::ConditionBlock`]): `init_header`, `write_condition`,
/// `read_condition`, `update_condition`, `deactivate_condition`, and
/// `stage` are all provided.
impl ConditionBlock for ClobCrankConditionsV0 {
    const NUM_CONDITIONS: usize = CLOB_CRANK_CONDITIONS;
    const STAGING_OFFSET: u32 = CLOB_CRANK_STAGING_OFFSET as u32;

    fn block(&self) -> &[u8] {
        &self.block
    }

    fn block_mut(&mut self) -> &mut [u8] {
        &mut self.block
    }

    fn staging_mut(&mut self) -> &mut [u8] {
        &mut self.staging
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        relay_spec::{bytemuck::Zeroable, ResolvedCrankV0, ResponsePointerV0},
    };

    #[test]
    fn size_matches_the_layout_and_the_spec() {
        assert_eq!(CLOB_CRANK_BLOCK_LEN, 16 + 6 * relay_spec::CONDITION_LEN);
        assert_eq!(
            ClobCrankConditionsV0::SIZE,
            8 + CLOB_CRANK_BLOCK_LEN + CLOB_CRANK_STAGING_LEN + CLOB_CRANK_RESOLVERS_LEN + 48
        );
        // The whole account must clear anchor init's 10,240-byte CPI
        // allocation ceiling, or attaching a CLOB to a market breaks.
        assert!(ClobCrankConditionsV0::SIZE <= 10_240);
        // the staging region must land where the pointer offset says it does
        assert_eq!(
            CLOB_CRANK_STAGING_OFFSET,
            8 + core::mem::offset_of!(ClobCrankConditionsV0, staging)
        );
        // the u64 reservoir field must land 8-aligned, past block + staging
        assert_eq!(std::mem::align_of::<ClobCrankConditionsV0>(), 8);
        assert_eq!(
            std::mem::size_of::<ClobCrankConditionsV0>(),
            ClobCrankConditionsV0::SIZE - 8
        );
    }

    #[test]
    fn header_then_conditions_round_trip_through_the_spec() {
        let mut acct = ClobCrankConditionsV0::default();
        acct.init_block().unwrap();

        let mut condition = relay_spec::ConditionV0::zeroed();
        condition.wake_ts = 1_234;
        condition.active = 1;
        acct.set_condition(CLOB_CRANK_EXPIRE, &condition).unwrap();

        // relay's own reader must accept what we wrote, at offset 0 of the
        // region (offset 8 of the account).
        let (header, conditions) = relay_spec::read_block(acct.block(), 0).unwrap();
        assert_eq!(header.num_conditions, CLOB_CRANK_CONDITIONS as u8);
        assert_eq!(conditions.len(), CLOB_CRANK_CONDITIONS);
        assert_eq!(conditions[CLOB_CRANK_EXPIRE].wake_ts, 1_234);
        assert_eq!(conditions[CLOB_CRANK_EXPIRE].active, 1);
        // The untouched slots are zeroed (inactive) conditions, not garbage.
        assert_eq!(conditions[CLOB_CRANK_EVICT].active, 0);
        assert_eq!(conditions[CLOB_CRANK_EXPIRE_FALLBACK].active, 0);

        assert_eq!(
            acct.get_condition(CLOB_CRANK_EXPIRE).unwrap().wake_ts,
            1_234
        );
    }

    #[test]
    fn expiry_hint_min_folds_and_repairs() {
        let mut acct = ClobCrankConditionsV0::default();
        acct.init_block().unwrap();
        let mut condition = relay_spec::ConditionV0::zeroed();
        condition.wake_ts = i64::MAX;
        condition.active = 1;
        acct.set_condition(CLOB_CRANK_EXPIRE, &condition).unwrap();

        acct.note_expiry(5_000).unwrap();
        assert_eq!(acct.expire_wake_ts().unwrap(), 5_000);
        // A later expiry never moves the hint back.
        acct.note_expiry(9_000).unwrap();
        assert_eq!(acct.expire_wake_ts().unwrap(), 5_000);
        acct.note_expiry(1_000).unwrap();
        assert_eq!(acct.expire_wake_ts().unwrap(), 1_000);

        acct.repair_expiry(i64::MAX).unwrap();
        assert_eq!(acct.expire_wake_ts().unwrap(), i64::MAX);
    }

    #[test]
    fn staged_payload_round_trips_through_the_pointer() {
        let mut acct = ClobCrankConditionsV0::default();
        acct.init_block().unwrap();
        let resolved = ResolvedCrankV0 {
            accounts: (0..11u8)
                .map(|i| relay_spec::AccountRefV0::writable([i; 32]))
                .collect(),
            data: vec![1, 2, 3],
        };
        let pointer_bytes = acct.stage(&resolved).unwrap();
        let pointer = ResponsePointerV0::read(&pointer_bytes).unwrap();
        assert!(pointer.has_work());
        assert_eq!(pointer.account_index, 0);
        assert_eq!(pointer.offset() as usize, CLOB_CRANK_STAGING_OFFSET);
        // The turner reads [offset..offset+len] of the *account*; simulate
        // that read against the account's would-be data layout.
        let staged = &acct.staging[..pointer.len() as usize];
        assert_eq!(ResolvedCrankV0::read(staged).unwrap(), resolved);
    }

    #[test]
    fn out_of_range_condition_index_is_rejected() {
        let mut acct = ClobCrankConditionsV0::default();
        acct.init_block().unwrap();
        assert!(acct
            .set_condition(CLOB_CRANK_CONDITIONS, &relay_spec::ConditionV0::zeroed())
            .is_err());
        assert!(acct.get_condition(CLOB_CRANK_CONDITIONS).is_err());
    }
}
