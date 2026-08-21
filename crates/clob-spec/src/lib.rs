//! Layout of the CLOB market account: the one declaration the book and its
//! readers share.
//!
//! # Why this is a crate
//!
//! The CLOB stores an order's owner on the node — an authority and a
//! sub-account, not an account key — so anything that has to name the users a
//! fill will settle against must read the arena. Velocity does that on chain
//! (the cranks find the tail, the expired order, the crossed pair; the router
//! bounds which users a book's entry may touch), and it lives in a different
//! cargo workspace from the CLOB, so it cannot see the book's types.
//!
//! It used to restate them: a second set of byte offsets, and a node decoder
//! that read each field by literal range. Nothing bound the two, so a field
//! reordered in the book silently misparsed every node on the reader's side —
//! which reads as an empty or nonsense book rather than as an error.
//!
//! So the layout is declared once, here, and both sides depend on it. The
//! book asserts its own structs against these constants, which is what turns
//! a layout change into a compile error in one place instead of a wrong
//! answer somewhere else.
//!
//! # What belongs here
//!
//! Layout, and nothing that decides anything. Which orders are matchable,
//! which the walk may skip, what a crank does with the tail — that is policy,
//! it differs per reader, and it stays with the reader. The one exception is
//! [`OrderNodeV0::is_matchable`], which is a property of the node's own
//! fields rather than of any caller's intent.

pub use quoter_spec::{SideV0, UserRefV0};
use {
    bytemuck::{Pod, Zeroable},
    solana_address::Address,
    static_assertions::const_assert_eq,
};

/// The list terminator: no next node, no head, no free slot.
pub const NIL: u32 = u32::MAX;

/// Flags on a node's `bit_flags` byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum OrderBitFlag {
    /// Node holds a live order (clear = node is on the free list).
    Open = 1,
    /// Order is an ask (clear = bid).
    Ask = 2,
    /// The order is an unfilled taker remainder migrated onto the book rather
    /// than a quote someone chose to post: it demands liquidity, and in a
    /// cross it is the aggressor, so the cross prices at the counterparty's
    /// side.
    TakerOrigin = 4,
}

impl OrderBitFlag {
    /// This bit when `set`, nothing otherwise — for composing a node's
    /// `bit_flags`.
    pub fn bit_if(self, set: bool) -> u8 {
        if set {
            self as u8
        } else {
            0
        }
    }
}

/// One arena slot: a live order threaded into a side's price-time list, or a
/// free node threaded into the free list via `next`. The velocity `User` is
/// stored inline (no seat table): user capacity is order capacity, governed
/// by the one eviction rule.
///
/// Deliberately kept at 96 bytes with only the five spare bytes below: the
/// node is the per-order cost of a market (capacity × this size is the
/// account's rent), so growth room lives on the header instead. A future
/// field wider than those spare bytes needs an `OrderNodeV1` arena.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
pub struct OrderNodeV0 {
    /// Authority wallet of the velocity `User` fills settle against
    /// (velocity verifies control before it CPIs place/cancel). Paired with
    /// `sub_account_id` below — see [`UserRefV0`] for why identity is stored
    /// in derivable form.
    pub authority: Address,
    /// PRICE_PRECISION.
    pub price: u64,
    /// Remaining unfilled size, base precision.
    pub base_asset_amount: u64,
    /// First slot at which this order may match, in either direction.
    pub activation_slot: u64,
    /// Timestamp after which the order is expired (0 = good-till-cancelled).
    pub max_ts: i64,
    pub order_id: u64,
    /// Slot the order was placed — age input for the unknown-user grace check
    /// and for the crank reward's time-based component.
    pub placed_slot: u64,
    /// Toward the best of book; [`NIL`] if head.
    pub prev: u32,
    /// Away from the best of book (or next free node); [`NIL`] if tail.
    pub next: u32,
    pub bit_flags: u8,
    pub padding0: u8,
    /// Sub-account half of the user identity (see `authority`).
    pub sub_account_id: u16,
    pub padding: [u8; 4],
}

/// Encoded width of one arena slot.
pub const NODE_BYTES: usize = core::mem::size_of::<OrderNodeV0>();
const_assert_eq!(NODE_BYTES, 96);

impl OrderNodeV0 {
    pub fn user_ref(&self) -> UserRefV0 {
        UserRefV0 {
            authority: self.authority,
            sub_account_id: self.sub_account_id,
        }
    }

    pub fn is_bit_flag_set(&self, flag: OrderBitFlag) -> bool {
        self.bit_flags & flag as u8 != 0
    }

    /// Node holds a live order.
    pub fn is_open(&self) -> bool {
        self.is_bit_flag_set(OrderBitFlag::Open)
    }

    /// The order is a migrated taker remainder.
    pub fn is_taker_origin(&self) -> bool {
        self.is_bit_flag_set(OrderBitFlag::TakerOrigin)
    }

    pub fn side(&self) -> SideV0 {
        if self.is_bit_flag_set(OrderBitFlag::Ask) {
            SideV0::Ask
        } else {
            SideV0::Bid
        }
    }

    /// Past its `max_ts`. A zero `max_ts` is good-till-cancelled.
    pub fn is_expired(&self, now: i64) -> bool {
        self.max_ts != 0 && self.max_ts < now
    }

    /// Past its activation slot, so the speed bump no longer holds it.
    pub fn is_active(&self, slot: u64) -> bool {
        self.activation_slot <= slot
    }

    /// Live and matchable right now: open, activated, not expired.
    pub fn is_matchable(&self, slot: u64, now: i64) -> bool {
        self.is_open() && self.is_active(slot) && !self.is_expired(now)
    }
}

/// Encoded width of the market header, discriminator excluded.
///
/// The book const-asserts `size_of::<ClobHeaderV0>()` against this, so the
/// arena offset below moves with the header rather than after it.
pub const HEADER_BYTES: usize = 8504;

/// Bytes anchor writes ahead of an account's data.
pub const DISCRIMINATOR_BYTES: usize = 8;

/// Field offsets from the start of account data, discriminator included. The
/// book asserts each against its own struct.
pub const BEST_BID_OFFSET: usize = 112;
pub const BEST_ASK_OFFSET: usize = 116;
/// The side tails, what `evict_worst_v0` removes.
pub const WORST_BID_OFFSET: usize = 120;
pub const WORST_ASK_OFFSET: usize = 124;
/// The two counts are adjacent, so one change-watch covers both.
pub const BID_COUNT_OFFSET: usize = 136;
pub const ASK_COUNT_OFFSET: usize = 140;
/// What a placement's `activation_slot` becomes when the caller chooses no
/// delay.
pub const DEFAULT_ACTIVATION_DELAY_OFFSET: usize = 144;
/// The floor on a resting order's size: below it the book culls rather than
/// rests.
pub const MIN_ORDER_SIZE_OFFSET: usize = 88;
/// The soft cap a side may be evicted past.
pub const EVICT_THRESHOLD_OFFSET: usize = 156;
/// The velocity perp market this book serves.
pub const MARKET_INDEX_OFFSET: usize = 160;

/// Start of the node arena: `[discriminator][header][len: u32]`, padded to the
/// node's alignment.
pub const ORDERS_OFFSET: usize =
    (DISCRIMINATOR_BYTES + HEADER_BYTES + 4).next_multiple_of(core::mem::align_of::<OrderNodeV0>());
const_assert_eq!(ORDERS_OFFSET, 8520);

/// Read a `u32` header field at an account-data offset.
pub fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// Read a `u16` header field at an account-data offset.
///
/// The market index and the per-market tuning fields are `u16` and packed
/// adjacently, so reading one as a `u32` silently picks up the next.
pub fn u16_at(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        data.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

/// Read a `u64` header field at an account-data offset.
pub fn u64_at(data: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        data.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

/// Arena capacity implied by the account's length.
pub fn capacity(data_len: usize) -> usize {
    data_len.saturating_sub(ORDERS_OFFSET) / NODE_BYTES
}

/// Read node `index` out of a market account's data. `None` past the arena.
///
/// Copied rather than borrowed: a reader off chain holds the account in a
/// `Vec<u8>`, which promises only byte alignment, and the node wants eight.
/// The copy is 96 bytes and the alternative is a panic on a host that a
/// borrow would have to guard for anyway.
pub fn node(data: &[u8], index: u32) -> Option<OrderNodeV0> {
    let start = ORDERS_OFFSET + (index as usize).checked_mul(NODE_BYTES)?;
    let bytes = data.get(start..start.checked_add(NODE_BYTES)?)?;
    Some(bytemuck::pod_read_unaligned(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offsets are what a reader indexes with, so they are pinned to the
    /// field order here as well as against the book's own struct.
    #[test]
    fn the_node_reads_back_from_the_bytes_it_writes() {
        let mut node = OrderNodeV0::zeroed();
        node.authority = Address::new_from_array([7u8; 32]);
        node.price = 101;
        node.base_asset_amount = 5;
        node.activation_slot = 9;
        node.max_ts = 1_000;
        node.order_id = 42;
        node.placed_slot = 8;
        node.next = NIL;
        node.sub_account_id = 3;
        node.bit_flags = OrderBitFlag::Open as u8 | OrderBitFlag::Ask as u8;

        let mut data = vec![0u8; ORDERS_OFFSET + 2 * NODE_BYTES];
        let at = ORDERS_OFFSET + NODE_BYTES;
        data[at..at + NODE_BYTES].copy_from_slice(bytemuck::bytes_of(&node));

        let read = super::node(&data, 1).expect("in the arena");
        assert_eq!(read, node);
        assert_eq!(read.user_ref().sub_account_id, 3);
        assert_eq!(read.side(), SideV0::Ask);
        assert!(read.is_open());
        assert!(!read.is_taker_origin());
        assert!(read.is_matchable(9, 999));
        assert!(!read.is_matchable(8, 999), "not activated yet");
        assert!(!read.is_matchable(9, 1_001), "expired");
        assert_eq!(capacity(data.len()), 2);
        assert!(super::node(&data, 2).is_none(), "past the arena");
    }

    /// An unaligned buffer is what an off-chain reader has, so reading one
    /// must not panic.
    #[test]
    fn a_node_reads_out_of_an_unaligned_buffer() {
        let mut data = vec![0u8; 1 + ORDERS_OFFSET + NODE_BYTES];
        let mut node = OrderNodeV0::zeroed();
        node.price = 7;
        data[1 + ORDERS_OFFSET..].copy_from_slice(bytemuck::bytes_of(&node));
        assert_eq!(super::node(&data[1..], 0).expect("read").price, 7);
    }
}
