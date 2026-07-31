//! Per-user relay condition block for liquidations — the keeper-bot
//! architecture (bucket by risk, recheck the bucket on oracle moves,
//! event-recheck on the user's own changes, coarse full sweep), expressed
//! as relay conditions.
//!
//! One account per `User`, opt-in, three kinds of condition:
//!
//! - **Threshold slots** — one `OnValueCross` per exposure, watching that
//!   exposure's oracle at a *conservative* single-oracle liquidation-price
//!   estimate (closed-form slope, holding other prices fixed, haircut
//!   toward early). Crossing one is "entering the high-risk bucket": the
//!   level-triggered wake plus the turner's backoff then re-checks the
//!   user for free until they either liquidate or recover — and recovery
//!   silences the wake with no cleanup transaction.
//! - **The sync watch** — `OnAccountChange` over the user's own position
//!   regions, whose *executor is the sync itself*: positions change → the
//!   thresholds are re-derived, paid a small fee from this account's own
//!   lamports. The hint set maintains itself.
//! - **The fallback poll** — coarse `EverySlots` into the same sync,
//!   catching correlated drift the single-oracle thresholds under-model
//!   and any missed syncs.
//!
//! Cross-margin honesty: a threshold assumes other prices fixed, so hints
//! fire early (the haircut) or bounded-late (the poll), never wrongly —
//! the staged `liquidate_perp_with_fill` re-validates liquidatability
//! exactly, and the liquidator is only the filler, so the protocol `User`
//! warehouses no inventory. Users the sync can't model (no perp positions,
//! unsupported oracle layouts, past the slot cap) stay on the keeper-bot
//! floor, which remains the plan of record for inventory-taking
//! liquidations.

use {
    crate::{error::ErrorCode, msg},
    anchor_lang::prelude::*,
    relay_spec::{
        ConditionBlockHeaderV0, ResolvedCrankV0, ResponsePointerV0, BLOCK_HEADER_LEN, CONDITION_LEN,
    },
};

/// PDA seed: `["liq_conditions", user key]`.
pub const LIQ_CONDITIONS_PDA_SEED: &[u8] = b"liq_conditions";

/// Watched exposures per user (perp positions + non-quote spot exposures).
pub const LIQ_THRESHOLD_SLOTS: usize = 12;
/// Threshold slots, then the sync watch, then the fallback poll.
pub const LIQ_SYNC_WATCH: usize = LIQ_THRESHOLD_SLOTS;
pub const LIQ_SYNC_FALLBACK: usize = LIQ_THRESHOLD_SLOTS + 1;
pub const LIQ_CONDITIONS: usize = LIQ_THRESHOLD_SLOTS + 2;

pub const LIQ_CONDITIONS_BLOCK_LEN: usize = BLOCK_HEADER_LEN + LIQ_CONDITIONS * CONDITION_LEN;

/// The staged executor: a dozen named accounts plus the stored account
/// list below.
pub const LIQ_CONDITIONS_STAGING_LEN: usize = 2048;

pub const LIQ_CONDITIONS_STAGING_OFFSET: usize = 8 + LIQ_CONDITIONS_BLOCK_LEN;

/// The remaining-accounts list the sync was last called with, verbatim
/// ([`relay_spec::AccountRefV0`] wire): the user's full margin maps
/// followed by the markets' crank-conditions accounts. Staged executors
/// reuse it — the liquidation's map parser stops at the first non-map
/// account, so the tail is inert there.
pub const LIQ_SYNC_ACCOUNTS_MAX: usize = 32;
pub const LIQ_SYNC_ACCOUNTS_LEN: usize = LIQ_SYNC_ACCOUNTS_MAX * relay_spec::ACCOUNT_REF_LEN;

/// Per-threshold-slot metadata: which perp market the staged liquidation
/// targets (for a perp exposure, its own market; for a spot-collateral
/// exposure, the user's largest perp position).
#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct LiqSlotMetaV0 {
    pub target_market_index: u16,
    /// 1 = live slot.
    pub active: u8,
    pub padding: [u8; 1],
}

#[account(zero_copy(unsafe))]
#[derive(Debug)]
#[repr(C)]
pub struct LiqConditionsV0 {
    /// The relay condition block; first field, at the 8-aligned offset 8.
    pub block: [u8; LIQ_CONDITIONS_BLOCK_LEN],
    /// Scratch the resolvers stage into. Simulation-only.
    pub staging: [u8; LIQ_CONDITIONS_STAGING_LEN],
    /// See [`LIQ_SYNC_ACCOUNTS_LEN`].
    pub sync_accounts: [u8; LIQ_SYNC_ACCOUNTS_LEN],
    /// Parallel to the threshold condition slots.
    pub slots: [LiqSlotMetaV0; LIQ_THRESHOLD_SLOTS],
    /// The `User` these conditions watch.
    pub user: Pubkey,
    /// Fee the sync executor pays its keeper from this account's own
    /// lamports (the account doubles as the sync reservoir — whoever wants
    /// this user's hints self-maintaining funds it; empty degrades to
    /// manual syncs + the thresholds from the last sync).
    pub sync_payment_lamports: u64,
    /// The fallback poll interval.
    pub sync_fallback_slots: u64,
    /// Live entries in `sync_accounts`.
    pub sync_accounts_count: u8,
    pub padding: [u8; 15],
}

impl Default for LiqConditionsV0 {
    fn default() -> Self {
        Self {
            block: [0; LIQ_CONDITIONS_BLOCK_LEN],
            staging: [0; LIQ_CONDITIONS_STAGING_LEN],
            sync_accounts: [0; LIQ_SYNC_ACCOUNTS_LEN],
            slots: [LiqSlotMetaV0::default(); LIQ_THRESHOLD_SLOTS],
            user: Pubkey::default(),
            sync_payment_lamports: 0,
            sync_fallback_slots: 0,
            sync_accounts_count: 0,
            padding: [0; 15],
        }
    }
}

impl LiqConditionsV0 {
    pub const SIZE: usize = 8
        + LIQ_CONDITIONS_BLOCK_LEN
        + LIQ_CONDITIONS_STAGING_LEN
        + LIQ_SYNC_ACCOUNTS_LEN
        + LIQ_THRESHOLD_SLOTS * core::mem::size_of::<LiqSlotMetaV0>()
        + 32
        + 8
        + 8
        + 1
        + 15;

    pub fn block(&self) -> &[u8] {
        &self.block
    }

    pub fn init_header(&mut self) -> Result<()> {
        let header = ConditionBlockHeaderV0::new(LIQ_CONDITIONS as u8);
        self.block[..BLOCK_HEADER_LEN].copy_from_slice(bytemuck::bytes_of(&header));
        Ok(())
    }

    pub fn write_condition(
        &mut self,
        index: usize,
        condition: &relay_spec::ConditionV0,
    ) -> Result<()> {
        if index >= LIQ_CONDITIONS {
            msg!("liq condition index {} out of range", index);
            return Err(ErrorCode::DefaultError.into());
        }
        let start = BLOCK_HEADER_LEN + index * CONDITION_LEN;
        self.block[start..start + CONDITION_LEN].copy_from_slice(bytemuck::bytes_of(condition));
        Ok(())
    }

    pub fn write_sync_accounts(&mut self, refs: &[relay_spec::AccountRefV0]) -> Result<()> {
        if refs.len() > LIQ_SYNC_ACCOUNTS_MAX {
            msg!("sync account list of {} exceeds the region", refs.len());
            return Err(ErrorCode::DefaultError.into());
        }
        for (i, r) in refs.iter().enumerate() {
            let start = i * relay_spec::ACCOUNT_REF_LEN;
            self.sync_accounts[start..start + 32].copy_from_slice(&r.address);
            self.sync_accounts[start + 32] = r.writable;
        }
        self.sync_accounts_count = refs.len() as u8;
        Ok(())
    }

    pub fn read_sync_accounts(&self) -> Vec<relay_spec::AccountRefV0> {
        (0..(self.sync_accounts_count as usize).min(LIQ_SYNC_ACCOUNTS_MAX))
            .map(|i| {
                let start = i * relay_spec::ACCOUNT_REF_LEN;
                let mut address = [0u8; 32];
                address.copy_from_slice(&self.sync_accounts[start..start + 32]);
                relay_spec::AccountRefV0 {
                    address,
                    writable: self.sync_accounts[start + 32],
                }
            })
            .collect()
    }

    pub fn stage(
        &mut self,
        resolved: &ResolvedCrankV0,
    ) -> Result<[u8; relay_spec::RESPONSE_POINTER_LEN]> {
        let len = resolved.write_into(&mut self.staging).map_err(|e| {
            msg!(
                "staging a {}-byte resolved crank failed: {:?}",
                resolved.encoded_len(),
                e
            );
            error!(ErrorCode::DefaultError)
        })?;
        Ok(ResponsePointerV0::new(0, LIQ_CONDITIONS_STAGING_OFFSET as u32, len as u32).to_bytes())
    }

    /// Pay the sync keeper from this account's own lamports, best-effort:
    /// never fail for insufficiency (a manual sync must always land — only
    /// relay's own payment guard holds turners to the full fee) and never
    /// dip below rent exemption.
    pub fn pay_sync_keeper<'info>(
        conditions: &AccountInfo<'info>,
        keeper: &AccountInfo<'info>,
        amount: u64,
        rent_minimum: u64,
    ) -> Result<u64> {
        let spendable = conditions.lamports().saturating_sub(rent_minimum);
        let amount = amount.min(spendable);
        if amount == 0 {
            return Ok(0);
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
}

const _: () = assert!((LiqConditionsV0::SIZE - 8) % 16 == 0);
const _: () = assert!(LiqConditionsV0::SIZE <= 10_240);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_matches_the_layout() {
        assert_eq!(
            std::mem::size_of::<LiqConditionsV0>(),
            LiqConditionsV0::SIZE - 8
        );
        assert_eq!(
            LIQ_CONDITIONS_STAGING_OFFSET,
            8 + core::mem::offset_of!(LiqConditionsV0, staging)
        );
    }
}
