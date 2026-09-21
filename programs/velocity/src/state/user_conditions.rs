//! Per-user relay condition block for liquidations and trigger orders.
//!
//! One account per `User`, opt-in. The liquidation side arms three
//! conditions. The trigger-order slots share the same block. See
//! [`TRIGGER_SLOT_BASE`] for why the two live on one account.
//!
//! - [`LIQ_SYNC_WATCH`] is an `OnAccountChange` over the user's own position
//!   regions. Its executor is the sync itself, so a position change rewrites
//!   the block. The protocol crank treasury pays the keeper that runs it.
//! - [`LIQ_SYNC_FALLBACK`] is a coarse `EverySlots` poll into the same sync.
//!   It catches whatever the watch misses.
//! - [`LIQ_LIVENESS_POLL`] wakes the liquidation resolver on a fixed
//!   interval.
//!
//! No condition here predicts a liquidation price. The staged
//! `liquidate_perp_with_fill` runs the real maintenance-margin calculation
//! and reports no work while the account is healthy. The liquidator is only
//! the filler, so the protocol `User` warehouses no inventory. An account
//! whose markets pay no crank reservoir arms no liveness poll and stays on
//! the keeper-bot floor. That floor is also how a liquidator that wants to
//! take the inventory itself liquidates.

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
    /// The market's quoter slab, book and program. They are set when this
    /// slot's executor is `trigger_limit_order_v1`, and zeroed for
    /// `trigger_order`.
    pub quoter_slab: Pubkey,
    pub clob_market: Pubkey,
    pub clob_program: Pubkey,
    pub order_id: u32,
    pub market_index: u16,
    pub padding: [u8; 2],
}

/// The shortest fallback interval a paid self-sync may ask for. The interval is also the
/// rate limit on what the treasury pays to resync one account, so a caller free to name
/// one slot would be paid every slot. Roughly a minute of slots.
pub const LIQ_SYNC_MIN_FALLBACK_SLOTS: u64 = 150;

/// The most cost units a self-sync may price its resync at. Opting in is permissionless,
/// so an unbounded figure would let anyone name their own price against protocol funds. A
/// low figure is the safe direction. Measure again with `sol_log_compute_units` around a
/// full-map resync when the rewrite changes.
pub const LIQ_SYNC_MAX_COST_UNITS: u32 = 40_000;

/// The longest fallback interval a paid self-sync may ask for. Opting in is
/// permissionless, so without an upper bound a third party could name an interval long
/// enough that the poll never fires and the block reads as covered. Roughly a day.
pub const LIQ_SYNC_MAX_FALLBACK_SLOTS: u64 = 216_000;

/// PDA seed: `["user_conditions", user key]`.
pub const USER_CONDITIONS_PDA_SEED: &[u8] = b"user_conditions";

/// The sync watch, the sync fallback, then the liveness poll. No per-exposure threshold
/// slot, because watching a solved liquidation price needs a second margin engine that is
/// approximate by construction. The resolver runs the real calculation instead.
pub const LIQ_SYNC_WATCH: usize = 0;
pub const LIQ_SYNC_FALLBACK: usize = 1;
/// The coverage floor: a slow poll that asks the liquidation resolver the real question.
/// The resolver recomputes maintenance margin before staging anything. A wake that fires
/// early costs one simulation, and a wake that never fires is the only failure.
pub const LIQ_LIVENESS_POLL: usize = 2;

/// How often the liveness poll asks. A shorter interval is safer but not free, because
/// the poll resolves to no work almost every time and turners deprioritize a program whose
/// cranks mostly waste a simulation. Roughly two minutes of slots.
pub const LIQ_LIVENESS_POLL_SLOTS: u64 = 300;
/// Trigger-order slots follow the liquidation ones in the same block, sharing one account
/// per user. Both are keyed by the user, invalidated by the same writes, and want the same
/// margin map. Separate accounts would pay rent and a `WatchV0` twice.
pub const TRIGGER_SLOT_BASE: usize = 3;
pub const TRIGGER_CONDITION_SLOTS: usize = 8;
pub const USER_CONDITIONS: usize = TRIGGER_SLOT_BASE + TRIGGER_CONDITION_SLOTS;

/// Each trigger slot's resolver names its own market oracle and perp market, so no one
/// list serves every condition the way the margin map does. The region is striped by slot.
/// See [`relay_spec::write_resolver_stripe`] and [`relay_spec::resolver_stripes_len`].
pub const TRIGGER_RESOLVERS_PER_SLOT: usize = 5;
pub const TRIGGER_RESOLVERS_LEN: usize =
    relay_spec::resolver_stripes_len(TRIGGER_CONDITION_SLOTS, TRIGGER_RESOLVERS_PER_SLOT);

/// Account-data offset of the per-slot trigger resolver lists.
pub const TRIGGER_RESOLVERS_OFFSET: usize =
    relay_spec::block_offset!(UserConditionsV0, trigger_resolvers);

/// Account-data offset of the relay block (what a `WatchV0` registers at).
pub const USER_CONDITIONS_BLOCK_OFFSET: usize = relay_spec::block_offset!(UserConditionsV0, relay);

/// Capacity of the stored sync account list: four fixed resolver accounts plus roughly
/// three per market the user is exposed in, so forty-eight covers about fourteen markets.
/// A list that does not fit reverts the sync rather than truncating, which would deny the
/// opt-in to the accounts carrying the most risk.
pub const LIQ_SYNC_ACCOUNTS_MAX: usize = 48;

/// The stored list starts with the resolver's named accounts, which are
/// `[scratch, conditions, user, state]`. None of them is market-specific, so
/// every condition slot shares one list.
/// [`UserConditionsV0::read_sync_accounts`] skips this prefix.
pub const LIQ_RESOLVER_PREFIX: usize = 4;

#[account(zero_copy(unsafe))]
#[derive(Debug)]
#[repr(C)]
pub struct UserConditionsV0 {
    /// Everything relay needs, in one field. It holds the `relay-spec`
    /// header, the condition slots, and the shared sync account list. See
    /// [`LIQ_SYNC_ACCOUNTS_MAX`]. This is the first field, so its watch offset
    /// is 8.
    pub relay: RelayBlock<USER_CONDITIONS, LIQ_SYNC_ACCOUNTS_MAX>,
    /// Parallel to the trigger condition slots.
    pub trigger_slots: [TriggerSlotMetaV0; TRIGGER_CONDITION_SLOTS],
    /// Per-slot trigger resolver lists. See [`TRIGGER_RESOLVERS_LEN`].
    pub trigger_resolvers: [u8; TRIGGER_RESOLVERS_LEN],
    /// The `User` these conditions watch.
    pub user: Pubkey,
    /// Fee the sync executor pays its keeper out of the protocol crank treasury. Stated by
    /// whoever opts in and capped at [`LIQ_SYNC_MAX_COST_UNITS`], because opting in is
    /// permissionless and the payer is protocol funds. A block below
    /// [`LIQ_SYNC_MIN_FALLBACK_SLOTS`] pays nothing.
    pub sync_payment_lamports: u64,
    /// The fallback poll interval.
    pub sync_fallback_slots: u64,
    /// Digest of the exposures the last sync ran against, compared to the user's current
    /// positions to decide staleness. Comparing watched markets never converges for a user
    /// whose exposures arm no condition, and the level-triggered wake then fires forever.
    pub positions_digest: u64,
    /// Slot the treasury last paid a keeper for resyncing this account. A resync is paid
    /// at most once per [`Self::sync_fallback_slots`]. Opting in is permissionless and the
    /// instruction succeeds whether or not it had work, so without this slot anyone could
    /// crank the same account in a loop and draw the fee every time.
    pub last_paid_sync_slot: u64,
    /// Tail reserve, sized for two more pubkeys. A future sync input is
    /// captured here instead of forcing an `extend_account` migration on every
    /// opted-in user.
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

    /// FNV-1a over every live position. It is cheap enough for the executor to
    /// recompute on each sync.
    pub fn digest_positions(user: &crate::state::user::User) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut fold = |value: u64| {
            for byte in value.to_le_bytes() {
                hash ^= byte as u64;
                hash = hash.wrapping_mul(0x1000_0000_01b3);
            }
        };

        // The skip uses the margin engine's own emptiness test, and the fold covers every
        // field that can change the account's margin. A position holding only open orders
        // or only an isolated balance is live to `is_available`, and a narrower test left
        // those out of the digest, so the watch never fired when their open orders
        // changed.
        for position in user.perp_positions.iter() {
            if position.is_available() {
                continue;
            }

            fold(position.market_index as u64);
            fold(position.base_asset_amount as u64);
            fold(position.quote_asset_amount as u64);
            fold(position.open_bids as u64);
            fold(position.open_asks as u64);
            fold(position.isolated_position_scaled_balance);
        }
        for position in user.spot_positions.iter() {
            if position.is_available() {
                continue;
            }

            fold(position.market_index as u64);
            fold(position.scaled_balance);
            fold(position.balance_type as u64);
            fold(position.open_bids as u64);
            fold(position.open_asks as u64);
        }

        hash
    }

    pub fn block(&self) -> &[u8] {
        ConditionBlock::block(&self.relay)
    }

    /// This method and the four below wrap
    /// [`relay_spec::ConditionBlock`] and return the program's own error
    /// type, so a handler can use `?`.
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
    /// landed. This region is per slot. See [`TRIGGER_RESOLVERS_PER_SLOT`].
    /// The liquidation side's shared list is [`Self::write_sync_accounts`].
    pub fn write_slot_resolvers(
        &mut self,
        index: usize,
        refs: &[relay_spec::AccountRefV0],
    ) -> Result<relay_spec::ResolverListV0> {
        relay_spec::write_resolver_stripe(
            &mut self.trigger_resolvers,
            TRIGGER_RESOLVERS_OFFSET as u32,
            TRIGGER_RESOLVERS_PER_SLOT,
            index,
            refs,
        )
        .map_err(|_| ErrorCode::DefaultError.into())
    }

    /// Deactivate the trigger slot watching `(market_index, order_id)`. The
    /// caller runs this when the trigger fires, or when the order ends for
    /// another reason, so the level-triggered wake goes quiet. A missing slot
    /// is not an error, because a sync is best-effort.
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

    /// Store the shared sync account list and describe where it landed. Every
    /// condition on this account points relay at that list.
    pub fn write_sync_accounts(
        &mut self,
        refs: &[relay_spec::AccountRefV0],
    ) -> Result<relay_spec::ResolverListV0> {
        // A block written by an older spec is migrated first, so the slots
        // below are in the shape this program addresses them by.
        self.relay
            .migrate()
            .map_err(|_| error!(ErrorCode::DefaultError))?;

        self.relay.write_resolvers(refs).map_err(|_| {
            msg!("sync account list of {} exceeds the region", refs.len());
            error!(ErrorCode::DefaultError)
        })
    }

    /// The margin-map accounts only. The call skips the resolver-list prefix,
    /// so a staged executor sees exactly the list the sync was called with.
    pub fn read_sync_accounts(&self) -> Vec<relay_spec::AccountRefV0> {
        let refs = self.relay.resolver_refs();
        refs.get(LIQ_RESOLVER_PREFIX..).unwrap_or(&[]).to_vec()
    }
}

const _: () = assert!((UserConditionsV0::SIZE - 8).is_multiple_of(16));
const _: () = assert!(UserConditionsV0::SIZE <= 10_240);

const _: () = assert!(USER_CONDITIONS_BLOCK_OFFSET.is_multiple_of(8));

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

    /// One account per user instead of two. Every user pays rent on this
    /// size, so the test pins it rather than letting it drift as fields are
    /// added.
    #[test]
    fn size_is_pinned() {
        // The sync watch, the sync fallback, the liveness poll, then the
        // trigger slots.
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

    /// The interval is also the rate limit on what the treasury pays for one account, so a
    /// paid sync cannot name one short enough to be paid every slot. The rule reads the
    /// terms a block holds, so a pricing change cannot move a paid block past it.
    #[test]
    fn a_paid_self_sync_cannot_ask_to_be_paid_every_slot() {
        let paid = |slots: u64| terms(1_000, slots);
        assert!(paid(LIQ_SYNC_MIN_FALLBACK_SLOTS).is_ok());
        assert!(paid(LIQ_SYNC_MIN_FALLBACK_SLOTS - 1).is_err());
        assert!(paid(1).is_err());
        assert!(paid(0).is_err());
        assert!(terms(0, 1).is_ok());
    }

    /// Zero cost units is an unpaid opt-in and must store a zero payment. The fee rails
    /// charge a signature for a transaction of any shape, so pricing zero units through
    /// them would pay that fee every interval for an account that named no work.
    #[test]
    fn a_zero_cost_sync_stores_no_payment() {
        let unpaid = terms(0, LIQ_SYNC_MIN_FALLBACK_SLOTS).unwrap();
        assert_eq!(unpaid.sync_payment_lamports, 0);
        assert_eq!(unpaid.payable_lamports(), 0);

        let priced = terms(1_000, LIQ_SYNC_MIN_FALLBACK_SLOTS).unwrap();
        assert!(priced.sync_payment_lamports > 0);
    }

    /// An unpaid opt-in may name any interval, including one slot, because
    /// nothing is drawn from the treasury for it.
    #[test]
    fn an_unpaid_sync_may_poll_every_slot() {
        let unpaid = terms(0, 1).unwrap();
        assert!(unpaid.interval_is_sound());
        assert_eq!(unpaid.payable_lamports(), 0);
    }

    /// A block holding a payment with an interval below the floor pays
    /// nothing. Blocks armed before the floor existed cannot be cranked for
    /// lamports, whatever their stored terms say.
    #[test]
    fn a_block_below_the_interval_floor_pays_nothing() {
        use crate::instructions::SyncLiqConditionsTerms;
        let armed = SyncLiqConditionsTerms {
            sync_payment_lamports: 5_000,
            sync_fallback_slots: 1,
        };

        assert!(!armed.interval_is_sound());
        assert_eq!(armed.payable_lamports(), 0);

        let sound = SyncLiqConditionsTerms {
            sync_payment_lamports: 5_000,
            sync_fallback_slots: LIQ_SYNC_MIN_FALLBACK_SLOTS,
        };

        assert_eq!(sound.payable_lamports(), 5_000);
    }

    /// Price one sync's terms through the network's flat fee model.
    fn terms(
        cost_units: u32,
        fallback_slots: u64,
    ) -> anchor_lang::Result<crate::instructions::SyncLiqConditionsTerms> {
        crate::instructions::price_sync_terms(
            &crate::state::state::TransactionFeeRails::FLAT_PER_SIGNATURE,
            &crate::instructions::SyncLiqConditionsArgs {
                sync_cost_units: cost_units,
                sync_fallback_slots: fallback_slots,
            },
        )
    }
}
