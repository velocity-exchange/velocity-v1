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
    crate::error::ErrorCode,
    anchor_lang::prelude::*,
    relay_spec::{ConditionBlock, BLOCK_HEADER_LEN, CONDITION_LEN},
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

/// The region doubles as the threshold conditions' *indirect resolver
/// account list*: a condition may only carry four refs inline, and
/// `ResolveLiquidatePerpWithFill` needs its three named accounts plus the
/// whole margin map. Relay reads `num_resolver_accounts` refs straight out
/// of the block's own account at `resolver_list_offset`, so the list is
/// stored once, here, with the resolver's named accounts first.
///
/// The prefix is `[liq_conditions, user, state]` — deliberately nothing
/// market-specific, so all twelve threshold slots share one list.
pub const LIQ_RESOLVER_PREFIX: usize = 3;

/// Byte offset of `sync_accounts` within the account, for
/// `set_indirect_resolver_accounts`.
pub const LIQ_SYNC_ACCOUNTS_OFFSET: usize =
    LIQ_CONDITIONS_STAGING_OFFSET + LIQ_CONDITIONS_STAGING_LEN;

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
    /// Digest of the exposures the last sync ran against. The resolver
    /// compares it to the user's current positions to decide staleness —
    /// comparing *watched markets* instead never converges for a user
    /// whose exposures produce no watchable threshold (an unsupported
    /// oracle layout, a market with no reservoir), leaving the
    /// level-triggered sync wake firing forever. The localnet harness
    /// caught exactly that loop, once a second.
    pub positions_digest: u64,
    /// Live entries in `sync_accounts`.
    pub sync_accounts_count: u8,
    pub padding: [u8; 7],
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
            positions_digest: 0,
            sync_accounts_count: 0,
            padding: [0; 7],
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
        + 8
        + 1
        + 7;

    /// FNV-1a over every exposure that moves a threshold. Cheap enough for
    /// the executor to recompute on each sync, and exact enough that a
    /// changed position always changes the digest.
    pub fn digest_positions(user: &crate::state::user::User) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut fold = |value: u64| {
            for byte in value.to_le_bytes() {
                hash ^= byte as u64;
                hash = hash.wrapping_mul(0x1000_0000_01b3);
            }
        };
        for position in user.perp_positions.iter() {
            if position.base_asset_amount == 0 && position.quote_asset_amount == 0 {
                continue;
            }
            fold(position.market_index as u64);
            fold(position.base_asset_amount as u64);
            fold(position.quote_asset_amount as u64);
        }
        for position in user.spot_positions.iter() {
            if position.scaled_balance == 0 {
                continue;
            }
            fold(position.market_index as u64);
            fold(position.scaled_balance);
            fold(position.balance_type as u64);
        }
        hash
    }

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

    /// Writes the full resolver list: [`LIQ_RESOLVER_PREFIX`] named
    /// accounts followed by the margin map.
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

    /// The margin-map accounts only — the resolver-list prefix is skipped,
    /// so staged executors see exactly the list the sync was called with.
    pub fn read_sync_accounts(&self) -> Vec<relay_spec::AccountRefV0> {
        ((LIQ_RESOLVER_PREFIX)..(self.sync_accounts_count as usize).min(LIQ_SYNC_ACCOUNTS_MAX))
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

/// Block hosting + staging, from the spec (see
/// [`relay_spec::ConditionBlock`]): `init_header`, `write_condition`,
/// `read_condition`, `update_condition`, `deactivate_condition`, and
/// `stage` are all provided.
impl ConditionBlock for LiqConditionsV0 {
    const NUM_CONDITIONS: usize = LIQ_CONDITIONS;
    const STAGING_OFFSET: u32 = LIQ_CONDITIONS_STAGING_OFFSET as u32;

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
