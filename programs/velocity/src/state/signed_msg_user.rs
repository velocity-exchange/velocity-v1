//! The per-authority record of signed messages, `SignedMsgUserOrders`.
//!
//! The record is authority-scoped, so every subaccount shares it. Each entry
//! is replay protection for one message and the routing state of the order
//! that message became. See [`SignedMsgOrderId`].
//!
//! The header's `version` names the entry layout. Version 1 stores 40-byte
//! entries. An account created before the version field stores 24-byte
//! entries with no routing state. It has version 0 and exactly
//! [`SignedMsgUserOrders::legacy_space`] bytes, and [`is_legacy_layout`] is
//! the only test for it. An account created at 40 bytes before the version
//! field has version 0 at another size, so it reads as current. Both sizes
//! have 28 bytes after the entries that no layout reads.
//!
//! A legacy account migrates in place on its first mutable load. The size and
//! the lamports stay the same, so the account then holds fewer entries: the
//! capacity becomes the entry bytes over 40. The migration keeps every entry
//! that is not empty, or fails when they do not fit. Dropping an entry could
//! re-admit a message that is still placeable. A resize migrates as well and
//! restores the capacity the owner asks for.

use {
    crate::{
        error::{ErrorCode, VelocityResult},
        math::{
            safe_unwrap::SafeUnwrap,
            time::{Millis, SlotClock, SLOT_DURATION_TRANSITION_MS},
        },
        msg, validate, ID,
    },
    anchor_lang::{
        account,
        prelude::{
            borsh::{BorshDeserialize, BorshSerialize},
            Pubkey,
        },
        zero_copy, *,
    },
    prelude::AccountInfo,
    std::cell::{Ref, RefMut},
};

pub const SIGNED_MSG_PDA_SEED: &str = "SIGNED_MSG";
pub const SIGNED_MSG_WS_PDA_SEED: &str = "SIGNED_MSG_WS";
/// Grace past `max_slot` before a signed-message order id can be pruned. The
/// read site converts it to actual slots.
pub const SIGNED_MSG_EVICTION_BUFFER: Millis = Millis::from_ms(4_000);
/// How long a keeper may take to land a signed message. The order's worst
/// price was measured against the oracle at signing, so a message that lands
/// later than this no longer describes the market the signer agreed to.
pub const SIGNED_MSG_FILL_WINDOW: Millis = Millis::from_ms(30_000);

/// The last slot a keeper may place a signed message at. A resting limit's
/// message slot is itself the deadline, because the client stamps it ahead by
/// its signing budget. Any other order gets `SIGNED_MSG_FILL_WINDOW` past it.
pub fn signed_msg_max_slot(slot_clock: SlotClock, order_slot: u64, is_resting_limit: bool) -> u64 {
    if is_resting_limit {
        return order_slot;
    }

    slot_clock.slot_at_or_after_duration(order_slot, SIGNED_MSG_FILL_WINDOW)
}

mod tests;

/// One signed message this user sent, and what is still live from it.
///
/// The entry does two jobs. The `uuid` is replay protection, which is what
/// this account was built for. The rest is the routing state of the order the
/// message became. A signed-message order routes at placement and rests any
/// remainder on the market's CLOB. Somebody else builds the later transaction
/// that fills that remainder. `route_digest` holds that filler to the quoters
/// the taker chose, so it has to outlive the message.
///
/// The field order leaves no padding hole under `#[repr(C)]`. The two `u64`
/// fields sit on eight-byte boundaries, and the two byte arrays need no
/// alignment of their own. The stride is 40 bytes.
#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug, BorshDeserialize, BorshSerialize)]
#[repr(C)]
pub struct SignedMsgOrderId {
    pub uuid: [u8; 8],
    pub max_slot: u64,
    /// The CLOB order this message's remainder rests as, or zero when nothing
    /// of it rests. An entry naming a live order survives the stale sweep,
    /// because the fill that resolves it still needs the route below.
    pub clob_order_id: u64,
    pub order_id: u32,
    /// The market whose book `clob_order_id` names. Each book numbers its own
    /// orders, so the id alone can name an order on another market.
    pub market_index: u16,
    pub padding: u16,
    /// [`crate::state::order_params::route_digest`] of the quoter entries the
    /// taker's signed route named. Zero when the message named no route.
    pub route_digest: [u8; crate::state::order_params::ROUTE_DIGEST_LEN],
}

unsafe impl bytemuck::Pod for SignedMsgOrderId {}
unsafe impl bytemuck::Zeroable for SignedMsgOrderId {}

// The stride of the entry array, and therefore of every account this type is
// stored in. `SignedMsgUserOrders::space` derives the account size from it, so
// a change here changes what a client must allocate.
static_assertions::const_assert_eq!(std::mem::size_of::<SignedMsgOrderId>(), 40);

impl SignedMsgOrderId {
    pub fn new(uuid: [u8; 8], max_slot: u64, order_id: u32) -> Self {
        Self {
            uuid,
            max_slot,
            clob_order_id: 0,
            order_id,
            market_index: 0,
            padding: 0,
            route_digest: crate::state::order_params::NO_ROUTE_DIGEST,
        }
    }

    /// Whether this entry still describes an order resting on a book.
    pub fn rests_on_clob(&self) -> bool {
        self.clob_order_id != 0
    }

    fn rests_as(&self, market_index: u16, clob_order_id: u64) -> bool {
        clob_order_id != 0
            && self.clob_order_id == clob_order_id
            && self.market_index == market_index
    }
}

/**
 * This struct is a duplicate of SignedMsgUserOrdersZeroCopy
 * It is used to give anchor an struct to generate the idl for clients
 * The struct SignedMsgUserOrdersZeroCopy is used to load the data in efficiently
 */
#[account]
#[derive(Default, Eq, PartialEq, Debug)]
pub struct SignedMsgUserOrders {
    pub authority_pubkey: Pubkey,
    /// The entry layout. Version 1 stores 40-byte entries. Version 0 at the
    /// `legacy_space` size stores 24-byte entries.
    pub version: u32,
    pub signed_msg_order_data: Vec<SignedMsgOrderId>,
}

/// The entry layout of an account that this program writes.
pub const SIGNED_MSG_USER_ORDERS_VERSION: u32 = 1;

pub const MAX_SIGNED_MSG_USER_ORDERS: usize = 128;

/// The discriminator and [`SignedMsgUserOrdersFixed`]. The entries start here.
const HEADER_LEN: usize = 8 + std::mem::size_of::<SignedMsgUserOrdersFixed>();

const ENTRY_LEN: usize = std::mem::size_of::<SignedMsgOrderId>();

impl SignedMsgUserOrders {
    /// 8 orders - 396 bytes - 0.00364704 SOL for rent
    /// 16 orders - 716 bytes - 0.00587424 SOL for rent
    /// 32 orders - 1356 bytes - 0.01032864 SOL for rent
    /// 64 orders - 2636 bytes - 0.01923744 SOL for rent
    pub fn space(num_orders: usize) -> usize {
        8 + 32 + 4 + 32 + num_orders * ENTRY_LEN
    }

    /// The size of a legacy account with `num_orders` entries.
    pub fn legacy_space(num_orders: usize) -> usize {
        8 + 32 + 4 + 32 + num_orders * LEGACY_ENTRY_LEN
    }

    pub fn validate(&self) -> VelocityResult<()> {
        validate_len(self.signed_msg_order_data.len())
    }
}

fn validate_len(len: usize) -> VelocityResult {
    validate!(
        (1..=MAX_SIGNED_MSG_USER_ORDERS).contains(&len),
        ErrorCode::DefaultError,
        "SignedMsgUserOrders len must be between 1 and 128"
    )
}

#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct SignedMsgUserOrdersFixed {
    pub user_pubkey: Pubkey,
    pub version: u32,
    pub len: u32,
}

unsafe impl bytemuck::Pod for SignedMsgUserOrdersFixed {}
unsafe impl bytemuck::Zeroable for SignedMsgUserOrdersFixed {}

/// The entry of an account created before layout version 1.
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct LegacySignedMsgOrderId {
    uuid: [u8; 8],
    max_slot: u64,
    order_id: u32,
    padding: u32,
}

unsafe impl bytemuck::Pod for LegacySignedMsgOrderId {}
unsafe impl bytemuck::Zeroable for LegacySignedMsgOrderId {}

const LEGACY_ENTRY_LEN: usize = std::mem::size_of::<LegacySignedMsgOrderId>();

static_assertions::const_assert_eq!(LEGACY_ENTRY_LEN, 24);

/// Slots added to the `max_slot` of a migrated entry. A legacy auction order
/// had a deadline at the end of its auction. This program places the same
/// message until `SIGNED_MSG_FILL_WINDOW` past its slot. The count is that
/// window at the shortest slot duration, so the uuid outlives the message.
/// allow-verbose: a replay bound that the arithmetic below cannot show.
const LEGACY_DEADLINE_EXTENSION_SLOTS: u64 = SIGNED_MSG_FILL_WINDOW
    .as_ms()
    .div_ceil(SLOT_DURATION_TRANSITION_MS[SLOT_DURATION_TRANSITION_MS.len() - 1] as u64);

impl LegacySignedMsgOrderId {
    fn migrated(self) -> SignedMsgOrderId {
        let max_slot = self
            .max_slot
            .saturating_add(LEGACY_DEADLINE_EXTENSION_SLOTS);

        SignedMsgOrderId::new(self.uuid, max_slot, self.order_id)
    }
}

/// Whether an account of `data_len` bytes stores legacy 24-byte entries. A
/// migrated account can keep the legacy size, so version 1 is always current.
pub fn is_legacy_layout(fixed: &SignedMsgUserOrdersFixed, data_len: usize) -> bool {
    fixed.version == 0 && data_len == SignedMsgUserOrders::legacy_space(fixed.len as usize)
}

/// The entries of a legacy account that are not empty, newest first.
fn legacy_live_entries(entries: &[u8], legacy_len: u32) -> Vec<SignedMsgOrderId> {
    let mut live: Vec<SignedMsgOrderId> = entries[..legacy_len as usize * LEGACY_ENTRY_LEN]
        .chunks_exact(LEGACY_ENTRY_LEN)
        .map(bytemuck::pod_read_unaligned::<LegacySignedMsgOrderId>)
        .filter(|entry| entry.max_slot != 0)
        .map(LegacySignedMsgOrderId::migrated)
        .collect();
    live.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.max_slot));

    live
}

/// Write `entries` at the current stride and zero every byte after them.
fn write_entries(data: &mut [u8], entries: &[SignedMsgOrderId]) {
    data.fill(0);
    data.chunks_exact_mut(ENTRY_LEN)
        .zip(entries)
        .for_each(|(slot, entry)| slot.copy_from_slice(bytemuck::bytes_of(entry)));
}

/// Rewrite a legacy account at the current stride, with no change of size.
///
/// Every entry that is not empty must fit, because a dropped entry can hold
/// the uuid of a message that is still placeable. When they do not fit, the
/// load fails and placement fails with it until the owner resizes.
fn migrate_legacy_in_place(
    fixed: &mut SignedMsgUserOrdersFixed,
    data: &mut [u8],
) -> VelocityResult {
    let live = legacy_live_entries(data, fixed.len);
    let capacity = data.len() / ENTRY_LEN;
    validate!(
        live.len() <= capacity,
        ErrorCode::SignedMsgUserOrdersAccountFull,
        "legacy signed msg user orders hold {} entries but migrate to {} slots; resize the account",
        live.len(),
        capacity
    )?;

    write_entries(data, &live);
    msg!(
        "signed msg user orders migrated to version {}: {} of {} entries kept, {} slots",
        SIGNED_MSG_USER_ORDERS_VERSION,
        live.len(),
        fixed.len,
        capacity
    );

    fixed.len = capacity as u32;
    fixed.version = SIGNED_MSG_USER_ORDERS_VERSION;

    Ok(())
}

/// The bytes that `len` current entries take. A header that names more
/// entries than `available` bytes hold is an error, not a read past the data.
fn current_entries_len(
    fixed: &SignedMsgUserOrdersFixed,
    available: usize,
) -> VelocityResult<usize> {
    let entries_len = fixed.len as usize * ENTRY_LEN;
    validate!(
        entries_len <= available,
        ErrorCode::DefaultError,
        "signed msg user orders len {} exceeds the account data",
        fixed.len
    )?;

    Ok(entries_len)
}

fn entry_at(data: &[u8], index: u32) -> &SignedMsgOrderId {
    let size = std::mem::size_of::<SignedMsgOrderId>();
    let start = index as usize * size;
    bytemuck::from_bytes(&data[start..start + size])
}

fn entry_at_mut(data: &mut [u8], index: u32) -> &mut SignedMsgOrderId {
    let size = std::mem::size_of::<SignedMsgOrderId>();
    let start = index as usize * size;
    bytemuck::from_bytes_mut(&mut data[start..start + size])
}

pub struct SignedMsgUserOrdersZeroCopy<'a> {
    pub fixed: Ref<'a, SignedMsgUserOrdersFixed>,
    pub data: Ref<'a, [u8]>,
}

impl<'a> SignedMsgUserOrdersZeroCopy<'a> {
    /// The entries this view holds. The view of a legacy account holds none,
    /// whatever its header says.
    pub fn len(&self) -> u32 {
        (self.data.len() / ENTRY_LEN) as u32
    }

    pub fn get(&self, index: u32) -> &SignedMsgOrderId {
        entry_at(&self.data, index)
    }

    pub fn iter(&self) -> impl Iterator<Item = &SignedMsgOrderId> + '_ {
        (0..self.len()).map(move |i| self.get(i))
    }

    /// The route the signer chose for the order resting as `clob_order_id` on
    /// the book of `market_index`. `None` means the order carries no signed
    /// route: it was placed directly, or its entry was reclaimed.
    pub fn route_for_clob_order(
        &self,
        market_index: u16,
        clob_order_id: u64,
    ) -> Option<crate::state::order_params::RouteDigest> {
        self.iter()
            .find(|entry| entry.rests_as(market_index, clob_order_id))
            .map(|entry| entry.route_digest)
    }
}

pub struct SignedMsgUserOrdersZeroCopyMut<'a> {
    pub fixed: RefMut<'a, SignedMsgUserOrdersFixed>,
    pub data: RefMut<'a, [u8]>,
}

impl<'a> SignedMsgUserOrdersZeroCopyMut<'a> {
    pub fn len(&self) -> u32 {
        self.fixed.len
    }

    pub fn get(&self, index: u32) -> &SignedMsgOrderId {
        entry_at(&self.data, index)
    }

    pub fn get_mut(&mut self, index: u32) -> &mut SignedMsgOrderId {
        entry_at_mut(&mut self.data, index)
    }

    /// Replay check and stale sweep in one pass.
    ///
    /// An entry that names a live CLOB order is kept even once its `max_slot`
    /// is old. The remainder still rests, and the fill that resolves it reads
    /// the route from here. Only the pressure `add_signed_msg_order_id`
    /// describes reclaims such an entry, and only once it is past the buffer.
    ///
    /// This is the replay guard. It matches on the uuid, so an entry must
    /// outlive every slot at which its own message can still be placed.
    pub fn check_exists_and_prune_stale_signed_msg_order_ids(
        &mut self,
        signed_msg_order_id: SignedMsgOrderId,
        current_slot: u64,
        slot_clock: SlotClock,
    ) -> bool {
        let mut uuid_exists = false;
        for i in 0..self.len() {
            let existing_signed_msg_order_id = self.get_mut(i);
            let expired = slot_clock.elapsed(existing_signed_msg_order_id.max_slot, current_slot)
                > SIGNED_MSG_EVICTION_BUFFER;
            if existing_signed_msg_order_id.uuid == signed_msg_order_id.uuid && !expired {
                uuid_exists = true;
            } else if expired && !existing_signed_msg_order_id.rests_on_clob() {
                *existing_signed_msg_order_id = SignedMsgOrderId::default();
            }
        }
        uuid_exists
    }

    /// Take the free slot, or reclaim an expired retained one.
    ///
    /// A signed limit order carries no expiry, so without a second pass the
    /// retained entries fill the account and the user can no longer trade. A
    /// full account therefore reclaims the entry whose `max_slot` is oldest,
    /// among the entries already past the eviction buffer.
    ///
    /// An entry carries the uuid `check_exists_and_prune_stale_signed_msg_order_ids` matches
    /// on, so reclaiming a live entry would re-admit its message and fill the same signed order
    /// a second time. An entry past `max_slot` plus the buffer cannot be re-admitted anyway,
    /// because placement refuses a message whose `max_slot` is behind the current slot.
    /// Releasing such an entry costs its resting order the route, and the fill then treats the
    /// order as unrouted. The taker's own limit price still bounds that fill.
    ///
    /// Returns the index of the entry written. A retained entry can hold the
    /// same uuid, so a later write must use this index and not the uuid.
    pub fn add_signed_msg_order_id(
        &mut self,
        signed_msg_order_id: SignedMsgOrderId,
        current_slot: u64,
        slot_clock: SlotClock,
    ) -> VelocityResult<u32> {
        if signed_msg_order_id.max_slot == 0
            || signed_msg_order_id.order_id == 0
            || signed_msg_order_id.uuid == [0; 8]
        {
            return Err(ErrorCode::InvalidSignedMsgOrderId);
        }

        if let Some(free) = (0..self.len()).find(|&i| self.get(i).max_slot == 0) {
            *self.get_mut(free) = signed_msg_order_id;
            return Ok(free);
        }

        let stalest = (0..self.len())
            .filter(|i| {
                let entry = self.get(*i);
                entry.rests_on_clob()
                    && slot_clock.elapsed(entry.max_slot, current_slot) > SIGNED_MSG_EVICTION_BUFFER
            })
            .min_by_key(|i| self.get(*i).max_slot);
        match stalest {
            Some(index) => {
                msg!(
                    "signed msg order account full; reclaiming slot {} from clob order {}",
                    index,
                    self.get(index).clob_order_id
                );

                *self.get_mut(index) = signed_msg_order_id;
                Ok(index)
            }
            None => Err(ErrorCode::SignedMsgUserOrdersAccountFull),
        }
    }

    /// Record that the message at `index` now rests on the book, with the
    /// route the fill that resolves it must carry. `index` is the one
    /// `add_signed_msg_order_id` returned for the message.
    pub fn set_resting_route(
        &mut self,
        index: u32,
        market_index: u16,
        clob_order_id: u64,
        route_digest: crate::state::order_params::RouteDigest,
    ) {
        let entry = self.get_mut(index);
        entry.market_index = market_index;
        entry.clob_order_id = clob_order_id;
        entry.route_digest = route_digest;
    }

    /// Point the entry of a replaced order at its replacement, so the route
    /// follows the order through a modify. The book gives the replacement a
    /// new id.
    pub fn move_resting_route(
        &mut self,
        market_index: u16,
        clob_order_id: u64,
        new_clob_order_id: u64,
    ) {
        if let Some(entry) =
            self.find_entry_mut(|entry| entry.rests_as(market_index, clob_order_id))
        {
            entry.clob_order_id = new_clob_order_id;
        }
    }

    fn find_entry_mut(
        &mut self,
        matches: impl Fn(&SignedMsgOrderId) -> bool,
    ) -> Option<&mut SignedMsgOrderId> {
        let index = (0..self.len()).find(|&i| matches(self.get(i)))?;

        Some(self.get_mut(index))
    }

    /// Release the entry's hold once its order leaves the book, so the stale
    /// sweep can reclaim the slot the ordinary way. Reclaim safety in
    /// `add_signed_msg_order_id` rests on the eviction buffer, not on this.
    pub fn clear_resting_route(&mut self, market_index: u16, clob_order_id: u64) {
        if let Some(entry) =
            self.find_entry_mut(|entry| entry.rests_as(market_index, clob_order_id))
        {
            entry.clob_order_id = 0;
            entry.route_digest = crate::state::order_params::NO_ROUTE_DIGEST;
        }
    }
}

pub trait SignedMsgUserOrdersLoader<'a> {
    /// A legacy account loads with no entries, because it holds no route.
    fn load(&self) -> VelocityResult<SignedMsgUserOrdersZeroCopy<'_>>;
    /// A legacy account migrates first. See `migrate_legacy_in_place`.
    fn load_mut(&self) -> VelocityResult<SignedMsgUserOrdersZeroCopyMut<'_>>;
}

fn validate_record_owner_and_len(account: &AccountInfo) -> VelocityResult {
    validate!(
        account.owner == &ID,
        ErrorCode::DefaultError,
        "invalid signed_msg user orders owner",
    )?;

    validate!(
        account.data_len() >= HEADER_LEN,
        ErrorCode::DefaultError,
        "signed_msg user orders account too small",
    )
}

fn validate_discriminator(discriminator: &[u8]) -> VelocityResult {
    validate!(
        discriminator == SignedMsgUserOrders::DISCRIMINATOR,
        ErrorCode::DefaultError,
        "invalid signed_msg user orders discriminator",
    )
}

/// The header and every byte after it, in either layout.
fn load_untrimmed<'b>(account: &'b AccountInfo) -> VelocityResult<SignedMsgUserOrdersZeroCopy<'b>> {
    validate_record_owner_and_len(account)?;

    let data = account.try_borrow_data().safe_unwrap()?;
    let (discriminator, data) = Ref::map_split(data, |d| d.split_at(8));
    validate_discriminator(&discriminator)?;

    let (fixed, data) = Ref::map_split(data, |d| d.split_at(40));
    Ok(SignedMsgUserOrdersZeroCopy {
        fixed: Ref::map(fixed, |b| bytemuck::from_bytes(b)),
        data,
    })
}

fn load_mut_untrimmed<'b>(
    account: &'b AccountInfo,
) -> VelocityResult<SignedMsgUserOrdersZeroCopyMut<'b>> {
    validate_record_owner_and_len(account)?;

    let data = account.try_borrow_mut_data().safe_unwrap()?;
    let (discriminator, data) = RefMut::map_split(data, |d| d.split_at_mut(8));
    validate_discriminator(&discriminator)?;

    let (fixed, data) = RefMut::map_split(data, |d| d.split_at_mut(40));
    Ok(SignedMsgUserOrdersZeroCopyMut {
        fixed: RefMut::map(fixed, |b| bytemuck::from_bytes_mut(b)),
        data,
    })
}

/// Check that `account` is a record in either layout.
pub fn validate_signed_msg_user_orders_account(account: &AccountInfo) -> VelocityResult {
    load_untrimmed(account).map(drop)
}

impl<'a> SignedMsgUserOrdersLoader<'a> for AccountInfo<'a> {
    fn load(&self) -> VelocityResult<SignedMsgUserOrdersZeroCopy<'_>> {
        let record = load_untrimmed(self)?;
        let entries_len = if is_legacy_layout(&record.fixed, HEADER_LEN + record.data.len()) {
            0
        } else {
            current_entries_len(&record.fixed, record.data.len())?
        };

        Ok(SignedMsgUserOrdersZeroCopy {
            fixed: record.fixed,
            data: Ref::map(record.data, |d| &d[..entries_len]),
        })
    }

    fn load_mut(&self) -> VelocityResult<SignedMsgUserOrdersZeroCopyMut<'_>> {
        let mut record = load_mut_untrimmed(self)?;
        if is_legacy_layout(&record.fixed, HEADER_LEN + record.data.len()) {
            migrate_legacy_in_place(&mut record.fixed, &mut record.data)?;
        }

        let entries_len = current_entries_len(&record.fixed, record.data.len())?;
        Ok(SignedMsgUserOrdersZeroCopyMut {
            fixed: record.fixed,
            data: RefMut::map(record.data, |d| &mut d[..entries_len]),
        })
    }
}

/// Every entry of a record, read out so a resize can write the account again
/// at a new size. A legacy record reads as its entries that are not empty,
/// newest first, so a shrink drops its oldest entries.
pub struct SignedMsgUserOrdersSnapshot {
    pub user_pubkey: Pubkey,
    /// In a legacy record this can exceed `entries.len()`.
    pub header_len: u32,
    pub entries: Vec<SignedMsgOrderId>,
}

impl SignedMsgUserOrdersSnapshot {
    pub fn read(account: &AccountInfo) -> VelocityResult<Self> {
        let record = load_untrimmed(account)?;
        let entries = if is_legacy_layout(&record.fixed, HEADER_LEN + record.data.len()) {
            legacy_live_entries(&record.data, record.fixed.len)
        } else {
            let entries_len = current_entries_len(&record.fixed, record.data.len())?;
            record.data[..entries_len]
                .chunks_exact(ENTRY_LEN)
                .map(bytemuck::pod_read_unaligned)
                .collect()
        };

        Ok(Self {
            user_pubkey: record.fixed.user_pubkey,
            header_len: record.fixed.len,
            entries,
        })
    }

    /// Keep the first `num_orders` entries, and add empty ones to reach it.
    pub fn resize(&mut self, num_orders: usize) -> VelocityResult {
        validate_len(num_orders)?;
        self.entries
            .resize_with(num_orders, SignedMsgOrderId::default);

        Ok(())
    }

    /// Write the record at the current layout. The account must already have
    /// `SignedMsgUserOrders::space` bytes for the entries.
    pub fn write(&self, account: &AccountInfo) -> VelocityResult {
        let mut record = load_mut_untrimmed(account)?;
        validate!(
            HEADER_LEN + record.data.len() == SignedMsgUserOrders::space(self.entries.len()),
            ErrorCode::DefaultError,
            "signed msg user orders account size does not match {} entries",
            self.entries.len()
        )?;

        *record.fixed = SignedMsgUserOrdersFixed {
            user_pubkey: self.user_pubkey,
            version: SIGNED_MSG_USER_ORDERS_VERSION,
            len: self.entries.len() as u32,
        };

        write_entries(&mut record.data, &self.entries);

        Ok(())
    }
}

/// The record of `authority`, when a removal path carries it as an optional
/// account. Any other account reads as absent, so a crank cannot fail on the
/// record of the user whose order it removes. A read-only record reads as
/// absent too, because a write to it fails the transaction.
pub fn carried_signed_msg_record<'a>(
    account: Option<&'a AccountInfo<'_>>,
    authority: &Pubkey,
) -> Option<SignedMsgUserOrdersZeroCopyMut<'a>> {
    let account = account.filter(|account| {
        account.owner == &ID
            && account.is_writable
            && account
                .try_borrow_data()
                .is_ok_and(|data| data.starts_with(SignedMsgUserOrders::DISCRIMINATOR))
    })?;
    let record = account.load_mut().ok()?;

    (record.fixed.user_pubkey == *authority).then_some(record)
}

/// Release the entries of the taker-origin remainders a removal took off the
/// book of `market_index`. The removals belong to one user, and `account` may
/// be that user's record.
pub fn release_removed_remainders(
    account: Option<&AccountInfo<'_>>,
    market_index: u16,
    removed: &[crate::state::prop_amm::RemovedOrderV0],
) {
    let mut remainders = removed.iter().filter(|order| order.taker_origin).peekable();
    let Some(owner) = remainders.peek().map(|order| order.user.authority) else {
        return;
    };

    let Some(mut record) = carried_signed_msg_record(account, &owner) else {
        return;
    };

    remainders.for_each(|order| {
        record.clear_resting_route(market_index, order.order_id);
    });
}

/**
 * Used to store authenticated delegates for swift-like ws connections
 */
#[account]
#[derive(Default, Eq, PartialEq, Debug)]
pub struct SignedMsgWsDelegates {
    pub delegates: Vec<Pubkey>,
}

impl SignedMsgWsDelegates {
    pub fn space(&self, add: bool) -> usize {
        let delegate_count = if add {
            self.delegates.len() + 1
        } else {
            self.delegates.len() - 1
        };
        8 + 4 + delegate_count * 32
    }
}
