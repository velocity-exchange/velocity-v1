//! Per-user relay condition block for trigger orders — the on-chain half of
//! "trigger orders as relay conditions".
//!
//! One account per `User` (PDA off the user key), **opt-in**: anyone may
//! create and sync it (`sync_trigger_conditions`, rent on the caller —
//! normally the user's UI), and a user without one simply stays on the
//! keeper-bot path, which remains the correctness floor throughout.
//!
//! Each armed trigger order gets one [`relay_spec::WakeKind::OnValueCross`]
//! condition watching the market oracle's raw price at the trigger
//! threshold — level-triggered, so a turner pays nothing while the price is
//! away from the trigger and wakes exactly when it crosses. The sync writes
//! the threshold in the oracle's own raw units (converted from
//! PRICE_PRECISION using the oracle account's exponent, rounded toward
//! early-firing — the resolver and executor re-verify with the real oracle
//! code, so early is a wasted simulation and late would be a miss).
//!
//! Conditions are hints: `sync_trigger_conditions` is permissionless and
//! idempotent, rewriting the block from the user's live orders. A stale
//! block (an order cancelled without a re-sync) stages executors that fail
//! and back off; a missing block misses nothing keepers wouldn't cover.

use {
    crate::{error::ErrorCode, msg},
    anchor_lang::prelude::*,
    relay_spec::{
        ConditionBlockHeaderV0, ResolvedCrankV0, ResponsePointerV0, BLOCK_HEADER_LEN, CONDITION_LEN,
    },
};

/// PDA seed: `["trigger_conditions", user key]`.
pub const TRIGGER_CONDITIONS_PDA_SEED: &[u8] = b"trigger_conditions";

/// Watched trigger orders per user. Orders past the cap stay keeper-only.
pub const TRIGGER_CONDITION_SLOTS: usize = 8;

pub const TRIGGER_CONDITIONS_BLOCK_LEN: usize =
    BLOCK_HEADER_LEN + TRIGGER_CONDITION_SLOTS * CONDITION_LEN;

/// The staged executor is a single trigger call (a dozen fixed accounts)
/// plus the user's margin-map section.
pub const TRIGGER_CONDITIONS_STAGING_LEN: usize = 2048;

/// The user's margin-map account section ([`relay_spec::AccountRefV0`]
/// wire: oracles readonly, then spot + perp markets writable), captured at
/// sync — the staged trigger executors append it, because the margin gate
/// inside a trigger loads every market the user touches and a four-account
/// resolver cannot derive other markets' oracles. Goes stale when the
/// user's positions change; a stale map fails the executor's simulation
/// until the next sync, never fires wrongly.
pub const TRIGGER_MAP_ACCOUNTS_MAX: usize = 24;
pub const TRIGGER_MAP_ACCOUNTS_LEN: usize = TRIGGER_MAP_ACCOUNTS_MAX * relay_spec::ACCOUNT_REF_LEN;

/// Account-data offset of the staging region.
pub const TRIGGER_CONDITIONS_STAGING_OFFSET: usize = 8 + TRIGGER_CONDITIONS_BLOCK_LEN;

/// What a resolver needs to stage the right executor for a fired slot,
/// captured at sync time: the order's identity plus the market's CLOB
/// linkage when the trigger-limit path applies (zeroed for the plain
/// `trigger_order` path).
#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct TriggerSlotMetaV0 {
    /// The market's canonical CLOB entry / book / program — set when this
    /// slot's executor is `trigger_clob_order`, zeroed for `trigger_order`.
    pub quoter: Pubkey,
    pub clob_market: Pubkey,
    pub clob_program: Pubkey,
    pub order_id: u32,
    pub market_index: u16,
    pub padding: [u8; 2],
}

#[account(zero_copy(unsafe))]
#[derive(Debug)]
#[repr(C)]
pub struct TriggerConditionsV0 {
    /// The relay condition block; first field, at the 8-aligned offset 8.
    pub block: [u8; TRIGGER_CONDITIONS_BLOCK_LEN],
    /// Scratch the resolvers stage into. Simulation-only.
    pub staging: [u8; TRIGGER_CONDITIONS_STAGING_LEN],
    /// Per-slot executor inputs, parallel to the block's condition slots.
    pub slots: [TriggerSlotMetaV0; TRIGGER_CONDITION_SLOTS],
    /// The user's margin-map section (see [`TRIGGER_MAP_ACCOUNTS_LEN`]).
    pub map_accounts: [u8; TRIGGER_MAP_ACCOUNTS_LEN],
    /// The `User` these conditions watch triggers for.
    pub user: Pubkey,
    /// Live entries in `map_accounts`.
    pub map_accounts_count: u8,
    pub padding: [u8; 7],
}

impl Default for TriggerConditionsV0 {
    fn default() -> Self {
        Self {
            block: [0; TRIGGER_CONDITIONS_BLOCK_LEN],
            staging: [0; TRIGGER_CONDITIONS_STAGING_LEN],
            slots: [TriggerSlotMetaV0::default(); TRIGGER_CONDITION_SLOTS],
            map_accounts: [0; TRIGGER_MAP_ACCOUNTS_LEN],
            user: Pubkey::default(),
            map_accounts_count: 0,
            padding: [0; 7],
        }
    }
}

impl TriggerConditionsV0 {
    pub const SIZE: usize = 8
        + TRIGGER_CONDITIONS_BLOCK_LEN
        + TRIGGER_CONDITIONS_STAGING_LEN
        + TRIGGER_CONDITION_SLOTS * core::mem::size_of::<TriggerSlotMetaV0>()
        + TRIGGER_MAP_ACCOUNTS_LEN
        + 32
        + 1
        + 7;

    /// Write the margin-map section the staged executors append.
    pub fn write_map_accounts(&mut self, refs: &[relay_spec::AccountRefV0]) -> Result<()> {
        if refs.len() > TRIGGER_MAP_ACCOUNTS_MAX {
            msg!("map section of {} exceeds the region", refs.len());
            return Err(ErrorCode::DefaultError.into());
        }
        for (i, r) in refs.iter().enumerate() {
            let start = i * relay_spec::ACCOUNT_REF_LEN;
            self.map_accounts[start..start + 32].copy_from_slice(&r.address);
            self.map_accounts[start + 32] = r.writable;
        }
        self.map_accounts_count = refs.len() as u8;
        Ok(())
    }

    /// Read the margin-map section back for staging.
    pub fn read_map_accounts(&self) -> Vec<relay_spec::AccountRefV0> {
        (0..(self.map_accounts_count as usize).min(TRIGGER_MAP_ACCOUNTS_MAX))
            .map(|i| {
                let start = i * relay_spec::ACCOUNT_REF_LEN;
                let mut address = [0u8; 32];
                address.copy_from_slice(&self.map_accounts[start..start + 32]);
                relay_spec::AccountRefV0 {
                    address,
                    writable: self.map_accounts[start + 32],
                }
            })
            .collect()
    }

    pub fn block(&self) -> &[u8] {
        &self.block
    }

    pub fn init_header(&mut self) -> Result<()> {
        let header = ConditionBlockHeaderV0::new(TRIGGER_CONDITION_SLOTS as u8);
        self.block[..BLOCK_HEADER_LEN].copy_from_slice(bytemuck::bytes_of(&header));
        Ok(())
    }

    pub fn write_condition(
        &mut self,
        index: usize,
        condition: &relay_spec::ConditionV0,
    ) -> Result<()> {
        if index >= TRIGGER_CONDITION_SLOTS {
            msg!("trigger condition index {} out of range", index);
            return Err(ErrorCode::DefaultError.into());
        }
        let start = BLOCK_HEADER_LEN + index * CONDITION_LEN;
        self.block[start..start + CONDITION_LEN].copy_from_slice(bytemuck::bytes_of(condition));
        Ok(())
    }

    /// Deactivate the slot watching `(market_index, order_id)` — called when
    /// the trigger fires (or the order otherwise dies) so a level-triggered
    /// wake goes quiet. Missing slot is fine: syncs are best-effort.
    pub fn release_slot(&mut self, market_index: u16, order_id: u32) {
        for (index, meta) in self.slots.iter_mut().enumerate() {
            if meta.market_index == market_index && meta.order_id == order_id {
                *meta = TriggerSlotMetaV0::default();
                // active byte sits last-ish in the condition; zero the whole
                // slot rather than reaching into spec internals.
                let start = BLOCK_HEADER_LEN + index * CONDITION_LEN;
                self.block[start..start + CONDITION_LEN].fill(0);
                return;
            }
        }
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
        Ok(
            ResponsePointerV0::new(0, TRIGGER_CONDITIONS_STAGING_OFFSET as u32, len as u32)
                .to_bytes(),
        )
    }
}

const _: () = assert!((TriggerConditionsV0::SIZE - 8) % 16 == 0);
const _: () = assert!(TriggerConditionsV0::SIZE <= 10_240);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_matches_the_layout() {
        assert_eq!(
            std::mem::size_of::<TriggerConditionsV0>(),
            TriggerConditionsV0::SIZE - 8
        );
        assert_eq!(
            TRIGGER_CONDITIONS_STAGING_OFFSET,
            8 + core::mem::offset_of!(TriggerConditionsV0, staging)
        );
    }

    #[test]
    fn release_zeroes_the_slot_and_its_condition() {
        let mut acct = TriggerConditionsV0::default();
        acct.init_header().unwrap();
        let mut condition = relay_spec::ConditionV0::on_value_cross(
            [7; 32],
            8,
            8,
            1_000,
            0,
            relay_spec::CrankSpecV0 {
                resolver_program: [1; 32],
                resolver_disc: [2; 8],
                executor_program: [1; 32],
                executor_disc: [3; 8],
                min_payment: 5,
            },
            &[],
        );
        condition.active = 1;
        acct.write_condition(2, &condition).unwrap();
        acct.slots[2].market_index = 4;
        acct.slots[2].order_id = 99;

        acct.release_slot(4, 99);
        let (_, conditions) = relay_spec::read_block(acct.block(), 0).unwrap();
        assert_eq!(conditions[2].active, 0);
        assert_eq!(acct.slots[2].order_id, 0);
        // Releasing a missing slot is a no-op.
        acct.release_slot(4, 99);
    }
}
