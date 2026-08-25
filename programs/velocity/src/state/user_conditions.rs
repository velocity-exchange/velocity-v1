//! Per-user relay condition block for liquidations — the keeper-bot
//! architecture (bucket by risk, recheck the bucket on oracle moves,
//! event-recheck on the user's own changes, coarse full sweep), expressed
//! as relay conditions.
//!
//! One account per `User`, opt-in. Three kinds of liquidation condition, plus
//! the trigger-order slots that share the block (see [`TRIGGER_SLOT_BASE`] for
//! why the two live on one account):
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
//! floor, which remains how a liquidator that wants to take the inventory
//! itself liquidates.

use {
    crate::error::ErrorCode,
    anchor_lang::prelude::*,
    relay_anchor::RelayBlock,
    relay_spec::{ConditionBlock, RelayBlockV0},
};

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

/// The shortest fallback interval a *paid* self-sync may ask for.
///
/// The interval doubles as the rate limit on what the treasury pays for
/// resyncing one account, so a caller free to name one slot would be paid
/// every slot. Roughly a minute of slots, which is far below the cadence at
/// which a user's thresholds actually go stale.
pub const LIQ_SYNC_MIN_FALLBACK_SLOTS: u64 = 150;

/// The most cost units a self-sync may price its resync at.
///
/// The opt-in states what a resync costs and the protocol treasury pays that
/// figure to whoever cranks it. Opting in is permissionless, so an unbounded
/// figure would let anyone name their own price against protocol funds. This
/// is a measured ceiling on what a resync really requests; a caller may state
/// less, never more.
pub const LIQ_SYNC_MAX_COST_UNITS: u32 = 200_000;

/// PDA seed: `["user_conditions", user key]`.
pub const USER_CONDITIONS_PDA_SEED: &[u8] = b"user_conditions";

/// The sync watch, the sync fallback, then the liveness poll.
///
/// There are no per-exposure threshold slots. Velocity used to solve, per
/// position, the price at which the account turned liquidatable and watch that
/// number. Doing so meant a second implementation of the margin engine living
/// beside the real one, approximate by construction and needing to be kept in
/// step with every future change to margin — for a latency gain over the poll
/// below that a keeper bot already provides. The resolver runs the real
/// calculation, so the poll is exact and the estimate bought nothing that
/// justified maintaining it.
pub const LIQ_SYNC_WATCH: usize = 0;
pub const LIQ_SYNC_FALLBACK: usize = 1;
/// The coverage floor: a slow poll that asks the liquidation resolver the
/// real question rather than re-deriving a threshold.
///
/// The thresholds are a latency device. Each one predicts, from a snapshot,
/// the price at which an account turns liquidatable, and a prediction can be
/// wrong in ways no arithmetic fixes: funding and borrow interest accrue
/// against a clock rather than a watched value, an admin can raise a margin
/// ratio, an oracle's confidence can widen, and the account can hold more
/// exposures than there are slots.
///
/// None of that has to be predicted, because the resolver recomputes the real
/// maintenance-margin calculation before it stages anything and reports no
/// work when the account is healthy. A wake that fires early costs one
/// simulation. A wake that never fires is the only failure. So this poll
/// exists to guarantee that something asks, and the thresholds exist to make
/// the asking early.
pub const LIQ_LIVENESS_POLL: usize = 2;

/// How often the liveness poll asks.
///
/// A floor on how long an account can be liquidatable with every threshold
/// having missed it, so shorter is safer for the protocol. It is not free:
/// the poll resolves to no work almost every time, relay tracks how often a
/// program's cranks turn out to be worth landing, and a program that mostly
/// wastes a turner's simulation is one turners learn to deprioritize. Roughly
/// two minutes of slots keeps the floor tight without flooding.
pub const LIQ_LIVENESS_POLL_SLOTS: u64 = 300;
/// Trigger-order slots follow the liquidation ones in the same block.
/// One account per user, not two: both are keyed by the user, invalidated
/// by the same account changing, and want the same margin map — carrying
/// them separately paid rent, a `WatchV0`, and a turner registry entry
/// twice for one user.
pub const TRIGGER_SLOT_BASE: usize = 3;
pub const TRIGGER_CONDITION_SLOTS: usize = 8;
pub const USER_CONDITIONS: usize = TRIGGER_SLOT_BASE + TRIGGER_CONDITION_SLOTS;

/// Each trigger slot's resolver names that slot's own market oracle and
/// perp market, so unlike the margin map there is no one list every
/// condition can share; the region is per slot.
pub const TRIGGER_RESOLVERS_PER_SLOT: usize = 5;
/// 5 refs is 165 bytes; the stride is rounded up so the whole region
/// keeps `(SIZE - 8) % 16 == 0`. Padding sits between stripes, never
/// inside one, so each slot's list stays contiguous.
pub const TRIGGER_RESOLVERS_STRIDE: usize = 168;
const _: () =
    assert!(TRIGGER_RESOLVERS_STRIDE >= TRIGGER_RESOLVERS_PER_SLOT * relay_spec::ACCOUNT_REF_LEN);
pub const TRIGGER_RESOLVERS_LEN: usize = TRIGGER_CONDITION_SLOTS * TRIGGER_RESOLVERS_STRIDE;

/// Account-data offset of the per-slot trigger resolver lists.
pub const TRIGGER_RESOLVERS_OFFSET: usize =
    relay_spec::block_offset!(UserConditionsV0, trigger_resolvers);

/// Account-data offset of the relay block (what a `WatchV0` registers at).
pub const USER_CONDITIONS_BLOCK_OFFSET: usize = relay_spec::block_offset!(UserConditionsV0, relay);

/// Capacity of the stored sync account list — the remaining-accounts list
/// the sync was last called with, verbatim ([`relay_spec::AccountRefV0`]
/// wire): the user's full margin maps followed by the markets'
/// crank-conditions accounts and quoter entries. Staged executors reuse it
/// — `load_maps` parses positionally and stops at the first non-map
/// account, so the tail is inert there but still reaches a staged resync,
/// which re-classifies everything. It lives in the relay block's built-in
/// resolver region because it *doubles as the threshold conditions'
/// indirect resolver account list*: a condition may only carry a pointer,
/// and `ResolveLiquidatePerpWithFill` needs its named accounts plus the
/// whole margin map — stored once, shared by every threshold slot.
pub const LIQ_SYNC_ACCOUNTS_MAX: usize = 32;

/// The stored list's leading entries are the resolver's named accounts —
/// `[scratch, conditions, user, state]`, deliberately nothing
/// market-specific, so all twelve threshold slots share one list.
/// [`UserConditionsV0::read_sync_accounts`] skips them.
pub const LIQ_RESOLVER_PREFIX: usize = 4;

#[account(zero_copy(unsafe))]
#[derive(Debug)]
#[repr(C)]
pub struct UserConditionsV0 {
    /// Everything relay needs hosted, in one field: the `relay-spec` header,
    /// the condition slots, and the shared sync account list (see
    /// [`LIQ_SYNC_ACCOUNTS_MAX`]). First field, so its watch offset is 8.
    pub relay: RelayBlock<USER_CONDITIONS, LIQ_SYNC_ACCOUNTS_MAX>,
    /// Parallel to the trigger condition slots.
    pub trigger_slots: [TriggerSlotMetaV0; TRIGGER_CONDITION_SLOTS],
    /// Per-slot trigger resolver lists (see [`TRIGGER_RESOLVERS_LEN`]).
    pub trigger_resolvers: [u8; TRIGGER_RESOLVERS_LEN],
    /// The `User` these conditions watch.
    pub user: Pubkey,
    /// Fee the sync executor pays its keeper out of the protocol crank
    /// treasury.
    ///
    /// Stated by whoever opts in, and capped at
    /// [`LIQ_SYNC_MAX_COST_UNITS`] when it is priced, because opting in is
    /// permissionless and the payer is protocol funds rather than the account
    /// itself. [`Self::last_paid_sync_slot`] bounds how often it can be drawn.
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
    /// Slot the treasury last paid a keeper for resyncing this account.
    ///
    /// A resync is paid at most once per [`Self::sync_fallback_slots`], which
    /// is the cadence the fallback poll already runs at. Opting in is
    /// permissionless and the treasury pays, so without this anyone could
    /// crank the same account in a loop and draw the fee every time — real
    /// work is not required for the instruction to succeed, only for it to be
    /// worth paying for.
    pub last_paid_sync_slot: u64,
    /// Tail reserve: 8 bytes of alignment slack plus room for two more
    /// pubkeys, so a future sync input can be captured here instead of
    /// forcing an `extend_account` migration on every opted-in user.
    pub padding: [u8; 64],
}

impl Default for UserConditionsV0 {
    fn default() -> Self {
        Self {
            relay: RelayBlock::default(),
            trigger_slots: [TriggerSlotMetaV0::default(); TRIGGER_CONDITION_SLOTS],
            trigger_resolvers: [0; TRIGGER_RESOLVERS_LEN],
            user: Pubkey::default(),
            sync_payment_lamports: 0,
            sync_fallback_slots: 0,
            positions_digest: 0,
            last_paid_sync_slot: 0,
            padding: [0; 64],
        }
    }
}

impl UserConditionsV0 {
    pub const SIZE: usize = 8
        + RelayBlockV0::<USER_CONDITIONS, LIQ_SYNC_ACCOUNTS_MAX>::SIZE
        + TRIGGER_CONDITION_SLOTS * core::mem::size_of::<TriggerSlotMetaV0>()
        + TRIGGER_RESOLVERS_LEN
        + 32
        + 8
        + 8
        + 8
        + 8
        + 64;

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
        ConditionBlock::block(&self.relay)
    }

    /// Anchor-flavoured wrappers over [`relay_spec::ConditionBlock`]'s
    /// provided methods, so handlers keep using `?` with the program's own
    /// error type.
    pub fn init_block(&mut self) -> Result<()> {
        self.relay
            .init(USER_CONDITIONS_BLOCK_OFFSET as u32)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn set_condition(
        &mut self,
        index: usize,
        condition: &relay_spec::ConditionV0,
    ) -> Result<()> {
        ConditionBlock::write_condition(&mut self.relay, index, condition)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn get_condition(&self, index: usize) -> Result<relay_spec::ConditionV0> {
        ConditionBlock::read_condition(&self.relay, index)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn edit_condition(
        &mut self,
        index: usize,
        f: impl FnOnce(&mut relay_spec::ConditionV0),
    ) -> Result<()> {
        ConditionBlock::update_condition(&mut self.relay, index, f)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn clear_condition(&mut self, index: usize) -> Result<()> {
        ConditionBlock::deactivate_condition(&mut self.relay, index)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    /// Write trigger slot `index`'s resolver list and describe where it
    /// landed. (The liquidation side's shared list is
    /// [`Self::write_sync_accounts`] — this region is per slot, see
    /// [`TRIGGER_RESOLVERS_PER_SLOT`].)
    pub fn write_slot_resolvers(
        &mut self,
        index: usize,
        refs: &[relay_spec::AccountRefV0],
    ) -> Result<relay_spec::ResolverListV0> {
        if index >= TRIGGER_CONDITION_SLOTS || refs.len() > TRIGGER_RESOLVERS_PER_SLOT {
            return Err(ErrorCode::DefaultError.into());
        }
        let base = index * TRIGGER_RESOLVERS_STRIDE;
        for (i, r) in refs.iter().enumerate() {
            let at = base + i * relay_spec::ACCOUNT_REF_LEN;
            self.trigger_resolvers[at..at + 32].copy_from_slice(&r.address);
            self.trigger_resolvers[at + 32] = r.writable;
        }
        Ok(relay_spec::ResolverListV0::new(
            (TRIGGER_RESOLVERS_OFFSET + base) as u32,
            refs.len() as u8,
        ))
    }

    /// Deactivate the trigger slot watching `(market_index, order_id)` —
    /// called when the trigger fires (or the order otherwise dies) so a
    /// level-triggered wake goes quiet. Missing slot is fine: syncs are
    /// best-effort.
    pub fn release_slot(&mut self, market_index: u16, order_id: u32) {
        for (index, meta) in self.trigger_slots.iter_mut().enumerate() {
            if meta.market_index == market_index && meta.order_id == order_id {
                *meta = TriggerSlotMetaV0::default();
                let _ = ConditionBlock::deactivate_condition(
                    &mut self.relay,
                    TRIGGER_SLOT_BASE + index,
                );
                return;
            }
        }
    }

    /// Store the shared sync account list and describe where it landed
    /// (the threshold conditions' indirect resolver list).
    pub fn write_sync_accounts(
        &mut self,
        refs: &[relay_spec::AccountRefV0],
    ) -> Result<relay_spec::ResolverListV0> {
        self.relay.write_resolvers(refs).map_err(|_| {
            msg!("sync account list of {} exceeds the region", refs.len());
            error!(ErrorCode::DefaultError)
        })
    }

    /// The margin-map accounts only — the resolver-list prefix is skipped,
    /// so staged executors see exactly the list the sync was called with.
    pub fn read_sync_accounts(&self) -> Vec<relay_spec::AccountRefV0> {
        let refs = self.relay.resolver_refs();
        refs.get(LIQ_RESOLVER_PREFIX..).unwrap_or(&[]).to_vec()
    }
}

const _: () = assert!((UserConditionsV0::SIZE - 8) % 16 == 0);
const _: () = assert!(UserConditionsV0::SIZE <= 10_240);

const _: () = assert!(USER_CONDITIONS_BLOCK_OFFSET % 8 == 0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_matches_the_layout() {
        assert_eq!(
            std::mem::size_of::<UserConditionsV0>(),
            UserConditionsV0::SIZE - 8
        );
        assert_eq!(
            TRIGGER_RESOLVERS_OFFSET,
            8 + core::mem::offset_of!(UserConditionsV0, trigger_resolvers)
        );
    }
}

#[cfg(test)]
mod merged_size_tests {
    use super::*;

    /// One account per user instead of two. The number is load-bearing —
    /// it is rent every user pays — so it is pinned rather than left to
    /// drift as fields are added.
    #[test]
    fn size_is_pinned() {
        // The sync watch, the sync fallback, the liveness poll, then the
        // trigger slots. No per-exposure thresholds.
        assert_eq!(USER_CONDITIONS, 11);
        assert_eq!(LIQ_LIVENESS_POLL, 2);
        assert_eq!(TRIGGER_SLOT_BASE, 3);
        assert_eq!(relay_spec::CONDITION_LEN, 192);
        println!("UserConditionsV0::SIZE = {}", UserConditionsV0::SIZE);
        assert!(UserConditionsV0::SIZE <= 10_240);
    }

    /// Opting in is permissionless and the protocol treasury pays the resync
    /// keeper, so what a caller may price its own resync at is capped. An
    /// uncapped figure would let anyone name their own price against protocol
    /// funds and collect it by cranking itself.
    #[test]
    fn a_self_sync_cannot_price_itself_above_the_ceiling() {
        use crate::instructions::{validate_sync_args, SyncLiqConditionsArgs};
        let args = |units: u32| SyncLiqConditionsArgs {
            sync_cost_units: units,
            sync_fallback_slots: 150,
        };
        assert!(validate_sync_args(&args(LIQ_SYNC_MAX_COST_UNITS)).is_ok());
        assert!(validate_sync_args(&args(LIQ_SYNC_MAX_COST_UNITS + 1)).is_err());
        assert!(validate_sync_args(&args(u32::MAX)).is_err());
    }

    /// The interval is also the rate limit on what the treasury pays for one
    /// account, so a paid sync cannot name one short enough to be paid every
    /// slot. An unpaid sync is free to poll as it likes.
    #[test]
    fn a_paid_self_sync_cannot_ask_to_be_paid_every_slot() {
        use crate::instructions::{validate_sync_args, SyncLiqConditionsArgs};
        let paid = |slots: u64| SyncLiqConditionsArgs {
            sync_cost_units: 1_000,
            sync_fallback_slots: slots,
        };
        assert!(validate_sync_args(&paid(LIQ_SYNC_MIN_FALLBACK_SLOTS)).is_ok());
        assert!(validate_sync_args(&paid(LIQ_SYNC_MIN_FALLBACK_SLOTS - 1)).is_err());
        assert!(validate_sync_args(&paid(1)).is_err());
        assert!(validate_sync_args(&SyncLiqConditionsArgs {
            sync_cost_units: 0,
            sync_fallback_slots: 1,
        })
        .is_ok());
    }
}
