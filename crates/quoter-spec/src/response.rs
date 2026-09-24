//! What a quoter answers. A response is plain data in the quoter's account, and
//! velocity reads it there with no decode step. Velocity's heap is 32 KB and
//! never reclaims, and one fill calls every registered quoter twice.
//!
//! Each section is a length prefix and then that many fixed-width records. The
//! sections are contiguous and come in declaration order. The caller owns any
//! bytes after the last one.
//!
//! Every record is `#[repr(C)]` and `Pod`, so a field moved into a padding hole
//! stops the build instead of changing the wire. The `u64` fields lead, so the
//! 34-byte [`UserRefV0`] cannot push one out of alignment.

use {
    crate::{SpecError, UserRefV0},
    bytemuck::{Pod, Zeroable},
    wincode::{SchemaRead, SchemaWrite},
};

/// One user's share of an executed fill. The sign follows the taker's direction,
/// not this user's. `base_size` is subtracted when the taker went long and added
/// when it went short. `quote_size` moves the opposite way.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Pod, Zeroable, SchemaRead, SchemaWrite)]
#[wincode(assert_zero_copy)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct UserBalanceChangeV0 {
    pub base_size: u64,
    pub quote_size: u64,
    pub user: UserRefV0,
    pub _pad: [u8; 6],
}

/// One order a quoter removed because the part a fill left of it fell under the
/// market's minimum. Like a completed order, it unwinds the maker's aggregates.
/// Unlike one, it carries a size to release.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Pod, Zeroable, SchemaRead, SchemaWrite)]
#[wincode(assert_zero_copy)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct CancelledRemainderV0 {
    pub order_id: u64,
    pub base_asset_amount: u64,
    /// The price the removed order was resting at. The caller reports this one
    /// removal itself, and there is at most one per execute.
    pub price: u64,
    /// See [`CompletedOrderV0::client_order_id`].
    pub client_order_id: u32,
    pub user: UserRefV0,
    /// [`L3_ROW_FLAG_REDUCE_ONLY`] when the order was reduce-only.
    pub flags: u8,
    pub _pad: [u8; 1],
}

impl CancelledRemainderV0 {
    /// Whether the removed order was reduce-only. The caller disarms the
    /// owner's reduce-only count with it.
    pub fn is_reduce_only(&self) -> bool {
        self.flags & L3_ROW_FLAG_REDUCE_ONLY != 0
    }
}

/// One resting order a fill fully consumed, naming the balance change it belongs
/// to by index. The reader decrements that user's open-order count and releases any
/// per-order state, so an id for an order still on the book releases live state.
/// Velocity holds a change merged from N orders to N roundings, not one.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Pod, Zeroable, SchemaRead, SchemaWrite)]
#[wincode(assert_zero_copy)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct CompletedOrderV0 {
    pub order_id: u64,
    /// Which entry of [`ExecuteResponseV0::changes`] this order belongs to.
    /// [`ExecuteResponseV0::parse`] refuses an index past the end, which would
    /// unwind another user's live margin.
    pub change_index: u16,
    /// [`L3_ROW_FLAG_REDUCE_ONLY`] when the order was reduce-only.
    pub flags: u8,
    pub _pad: [u8; 1],
    /// The caller's own id for this order, minted when it asked for the
    /// placement, so the caller closes its record without a map between the two
    /// id spaces. Zero when the caller supplied none.
    pub client_order_id: u32,
}

impl CompletedOrderV0 {
    /// Whether the consumed order was reduce-only. The caller disarms the
    /// owner's reduce-only count with it.
    pub fn is_reduce_only(&self) -> bool {
        self.flags & L3_ROW_FLAG_REDUCE_ONLY != 0
    }
}

/// The one order a fill left resting with less size than it found. At most one
/// exists per execute, because only the last order a best-first walk reached can
/// be partial. A balance change merges every order of one maker, so this record
/// carries the per-order part. `base_filled` is this fill's, not the lifetime's.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Pod, Zeroable, SchemaRead, SchemaWrite)]
#[wincode(assert_zero_copy)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct PartiallyFilledOrderV0 {
    pub order_id: u64,
    pub base_filled: u64,
    /// See [`CompletedOrderV0::client_order_id`].
    pub client_order_id: u32,
    /// Which entry of [`ExecuteResponseV0::changes`] this fill is part of.
    /// Bounded like [`CompletedOrderV0::change_index`].
    pub change_index: u32,
}

/// One rung of a quoted ladder: `size` available at `price`.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Pod, Zeroable, SchemaRead, SchemaWrite)]
#[wincode(assert_zero_copy)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct PriceLevelV0 {
    pub price: u64,
    pub size: u64,
}

/// Widths the quoters' own section arithmetic is built from.
pub const USER_REF_BYTES: usize = UserRefV0::SIZE;
pub const CHANGE_BYTES: usize = core::mem::size_of::<UserBalanceChangeV0>();
pub const CANCELLED_BYTES: usize = core::mem::size_of::<CancelledRemainderV0>();
pub const COMPLETED_BYTES: usize = core::mem::size_of::<CompletedOrderV0>();
pub const PARTIAL_BYTES: usize = core::mem::size_of::<PartiallyFilledOrderV0>();
pub const PRICE_LEVEL_BYTES: usize = core::mem::size_of::<PriceLevelV0>();

/// Bytes wincode spends on a slice's length prefix. A quoter that streams
/// records writes this prefix itself, and `the_length_prefix_is_what_wincode_writes`
/// pins the two encodings together.
pub const LEN_BYTES: usize = 8;

/// The length prefix wincode writes ahead of a slice of `count` records.
#[inline]
pub fn len_prefix(count: usize) -> [u8; LEN_BYTES] {
    (count as u64).to_le_bytes()
}

// A field reordered or widened fails here instead of changing what the other
// program reads.
const _: () = {
    assert!(USER_REF_BYTES == 34);
    assert!(CHANGE_BYTES == 56);
    assert!(CANCELLED_BYTES == 64);
    assert!(COMPLETED_BYTES == 16);
    assert!(PARTIAL_BYTES == 24);
    assert!(PRICE_LEVEL_BYTES == 16);
};

/// What `execute_v0` answers: every balance change the fill produced, every sub-min
/// remainder it removed, and every resting order it consumed. The fields borrow
/// straight out of the quoter's account, so reading one allocates nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct ExecuteResponseV0<'a> {
    pub changes: &'a [UserBalanceChangeV0],
    pub cancelled: &'a [CancelledRemainderV0],
    pub completed: &'a [CompletedOrderV0],
    /// The order the fill left resting smaller, at most one. A slice rather
    /// than an option, so every section is framed the same way.
    pub partial: &'a [PartiallyFilledOrderV0],
}

impl<'a> ExecuteResponseV0<'a> {
    /// Read a response out of `bytes`. Validates what makes the bytes readable, and
    /// that every completed order names a balance change that exists. What the
    /// numbers mean stays the caller's to check.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, SpecError> {
        let response: Self = wincode::deserialize(bytes).map_err(|_| SpecError::Read)?;
        if !completed_orders_fit(response.completed, response.changes.len())
            || !partial_orders_fit(response.partial, response.changes.len())
        {
            return Err(SpecError::DanglingCompletedOrder);
        }

        Ok(response)
    }

    /// The orders the fill consumed for `change_index`.
    pub fn completed_for(
        &self,
        change_index: usize,
    ) -> impl Iterator<Item = &'a CompletedOrderV0> + '_ {
        self.completed
            .iter()
            .filter(move |entry| entry.change_index as usize == change_index)
    }

    /// How many orders the fill consumed for `change_index`.
    pub fn completed_count(&self, change_index: usize) -> usize {
        self.completed_for(change_index).count()
    }

    /// The caller's id for the one order behind `change_index`, when there is
    /// exactly one. A change usually merges several orders of one maker. It names
    /// one order when the fill consumed one and left none partial, or left one
    /// partial and consumed none.
    pub fn sole_client_order_id(&self, change_index: usize) -> Option<u32> {
        let index = change_index as u32;
        let mut ids = self
            .completed
            .iter()
            .filter(|entry| entry.change_index as u32 == index)
            .map(|entry| entry.client_order_id)
            .chain(
                self.partial
                    .iter()
                    .filter(|entry| entry.change_index == index)
                    .map(|entry| entry.client_order_id),
            );
        let first = ids.next()?;
        ids.next().is_none().then_some(first)
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.cancelled.is_empty() && self.completed.is_empty()
    }
}

/// What `quote_v0` answers: the ladder the quoter is standing behind, and
/// what it had to leave out.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct QuoteResponseV0<'a> {
    pub levels: &'a [PriceLevelV0],
    /// The best price this quoter could have offered but did not, and the base
    /// there, because that liquidity belongs to a user the caller did not load.
    /// Zeroed when nothing was left out. A caller that fills elsewhere at a worse
    /// price then routed around a competitor, not out of room.
    pub withheld: PriceLevelV0,
}

impl<'a> QuoteResponseV0<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, SpecError> {
        wincode::deserialize(bytes).map_err(|_| SpecError::Read)
    }

    pub fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }
}

/// Whether every completed order names a balance change that exists. The reader
/// refuses a response that breaks it, and [`crate::ExecuteWriter`] refuses to
/// write one.
pub(crate) fn completed_orders_fit(completed: &[CompletedOrderV0], changes: usize) -> bool {
    completed
        .iter()
        .all(|entry| (entry.change_index as usize) < changes)
}

/// Whether the partial-fill section is one a fill could have produced: at most one
/// record, naming a balance change that exists. A second partial would mean the walk
/// continued past an order it did not finish.
pub(crate) fn partial_orders_fit(partial: &[PartiallyFilledOrderV0], changes: usize) -> bool {
    partial.len() <= 1
        && partial
            .iter()
            .all(|entry| (entry.change_index as usize) < changes)
}

/// Where in its response account a quoter wrote the response: the return data of
/// `quote_v0` and `execute_v0`. Declared once, so no two quoters can disagree
/// about which `u32` comes first.
#[repr(C)]
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct ResponsePointerV0 {
    pub offset: u32,
    pub len: u32,
}

impl ResponsePointerV0 {
    /// Point at the `len` bytes a quoter streamed at `offset`. The offset is
    /// the quoter's own, because each account puts its response region
    /// somewhere different.
    pub fn at(offset: usize, len: usize) -> Self {
        Self {
            offset: offset as u32,
            len: len as u32,
        }
    }
}

/// One resting order behind a quoted book, as `quote_l3_v0` reports it. A caller
/// that must carry those users' accounts or draw the book needs the attribution
/// the aggregated ladder drops. A quoter with no orders does not implement the leg.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Pod, Zeroable, SchemaRead, SchemaWrite)]
#[wincode(assert_zero_copy)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct L3RowV0 {
    pub price: u64,
    pub size: u64,
    /// The quoter's own handle for the order, for a caller that wants to
    /// cancel or track it. Zero when the row is not an order.
    pub order_id: u64,
    /// The other half of the handle. A cancel or a fill takes both. Zero when
    /// the quoter keeps no arena, which is every non-book quoter.
    pub node_index: u32,
    /// Who this row settles against.
    pub user: UserRefV0,
    /// The `L3_ROW_FLAG_*` bits.
    pub flags: u8,
    pub _pad: [u8; 1],
    /// Slot the order was placed in. The id already gives price-time order.
    /// This gives elapsed time, which prices the work of resolving the order.
    /// Zero when the quoter keeps no such record.
    pub placed_slot: u64,
}

/// The row is an unfilled taker remainder the caller migrated onto the book. It
/// demands liquidity rather than offering it, so a cross cannot count on depth
/// behind it.
pub const L3_ROW_FLAG_TAKER_ORIGIN: u8 = 1;

/// The order can end a fill walk when its owner is absent from the caller's user
/// set. The quoter sets it, so an account-set builder never reimplements the size
/// floor. The book still skips an order inside its grace window whatever the size.
pub const L3_ROW_FLAG_BLOCKS_WALK: u8 = 2;

/// The order is reduce-only, and its owner carries an authoritative `base_cap`.
/// A caller binds the fill to the cover its own accounting reserved, and stops
/// tracking the owner's reduce-only exposure when the order leaves the book.
pub const L3_ROW_FLAG_REDUCE_ONLY: u8 = 4;

/// A taker-origin order reserves some or all of this row's size, and `size`
/// already has the reservation subtracted. A caller that settles the cross reads
/// with `include_taker_origin_reservations` and sees the whole size.
pub const L3_ROW_FLAG_RESERVED: u8 = 8;

/// Encoded width of an [`L3RowV0`].
pub const L3_ROW_BYTES: usize = core::mem::size_of::<L3RowV0>();

/// The answer to `quote_l3_v0`: the resting orders behind a quoted book, best
/// price first, read in place like every response here.
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct L3ResponseV0<'a> {
    pub rows: &'a [L3RowV0],
    /// The walk stopped on a bound rather than on the end of the book, so there
    /// is depth behind the last row. A caller collecting users knows its list is
    /// a prefix.
    pub more: u8,
}

impl<'a> L3ResponseV0<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, SpecError> {
        wincode::deserialize(bytes).map_err(|_| SpecError::Read)
    }
}
