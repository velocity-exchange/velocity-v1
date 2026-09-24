// Not a `//!` crate doc: `clob-state-v2` brings this file in with `include!`,
// and `include!` cannot splice an inner doc comment into a module that has
// already started, so the comment would fail to compile in that twin.
//
// The CLOB market account's order-node layout, declared once so an indexer
// that decodes a node cannot drift from the book that wrote it. `clob-wire`
// states the no-shared-layout rule; this crate is its one exception, because
// an indexer has no call that answers "which orders does this user hold".
// Does not cover the header, the free list, or the two sorted lists: a reader
// wants the best bid from the book, and every live order from an arena walk,
// which needs no links at all.

// The v2 IdlType derive emits `anchor_lang::`, so the v2 IDL build points that
// path at the fork. The feature is undefined in the v1 crate, so this is inert.
#[cfg(feature = "idl-build-v2")]
extern crate anchor_lang_v2 as anchor_lang;

pub use quoter_spec::{SideV0, UserRefV0};
use {
    bytemuck::{Pod, Zeroable},
    solana_address::Address as Pubkey,
};

/// The list terminator: no next node, no head, no free slot.
pub const NIL: u32 = u32::MAX;

/// Account-data offset of the order-node tail. The market is
/// `[disc][ClobHeaderV0][len: u32][OrderNodeV0 tail]`, padded to the node's alignment.
/// A number rather than an expression, because the header stays the book's.
/// `clob::state` asserts the two agree at compile time.
pub const ORDERS_OFFSET: usize = 9648;

/// Flags on a node's `bit_flags` byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub enum OrderBitFlag {
    /// Node holds a live order. A clear bit means the node is on the free list.
    Open = 1,
    /// Order is an ask. A clear bit means a bid.
    Ask = 2,
    /// The order is an unfilled taker remainder migrated onto the book, not a
    /// quote someone chose to post. It demands liquidity, so in a cross it is
    /// the aggressor and the cross prices at the counterparty's side.
    TakerOrigin = 4,
    /// The order only reduces its owner's position. The book is position-blind,
    /// so a fill against it is clamped to the owner's `base_cover` cap from the
    /// caller's user set. See [`OrderNodeV0::is_reduce_only`].
    ReduceOnly = 8,
}

impl OrderBitFlag {
    /// The bit when `set` is true, and zero otherwise. Use it to compose a
    /// node's `bit_flags`.
    pub fn bit_if(self, set: bool) -> u8 {
        if set {
            self as u8
        } else {
            0
        }
    }
}

/// One arena slot, holding either a live order threaded into a side's price-time list
/// or a free node threaded into the free list through `next`. The velocity `User` is
/// inline and there is no seat table, so user capacity is order capacity.
///
/// 104 bytes, with one spare. Capacity times this size is the account's rent, so growth
/// room lives on the header and a field belongs here only when a walk of one side has
/// to read it. `authority` and `sub_account_id` stay two fields rather than one
/// [`UserRefV0`]: at 34 bytes it would misalign every `u64` field after it.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct OrderNodeV0 {
    /// Authority wallet of the velocity `User` that fills settle against.
    /// Velocity verifies control before it CPIs place or cancel. It pairs with
    /// `sub_account_id` below. See [`UserRefV0`] for why identity is stored in
    /// derivable form.
    pub authority: Pubkey,
    /// PRICE_PRECISION.
    pub price: u64,
    /// Remaining unfilled size, base precision.
    pub base_asset_amount: u64,
    /// First slot at which this order may match, in either direction.
    pub activation_slot: u64,
    /// Timestamp after which the order is expired. Zero means
    /// good-till-cancelled.
    pub max_ts: i64,
    pub order_id: u64,
    /// Slot the order was placed. It is the age input for the unknown-user
    /// grace check and for the crank reward's time component.
    pub placed_slot: u64,
    /// Toward the best of book. [`NIL`] marks the head.
    pub prev: u32,
    /// Away from the best of book, or the next free node. [`NIL`] marks the
    /// tail.
    pub next: u32,
    /// Toward the oldest taker-origin order on this side. [`NIL`] marks the
    /// head. It means nothing unless [`OrderBitFlag::TakerOrigin`] is set.
    pub taker_origin_prev: u32,
    /// Toward the newest taker-origin order on this side. [`NIL`] marks the tail. These
    /// orders are threaded on their own list in rest order, so enumerating the
    /// remainders that claim a side costs their count rather than the side's.
    pub taker_origin_next: u32,
    pub bit_flags: u8,
    pub padding0: u8,
    /// Sub-account half of the user identity (see `authority`).
    pub sub_account_id: u16,
    /// The placing caller's own id for this order. The book stores it and
    /// reports it wherever it names the order. The book never reads it. Zero
    /// when the caller keeps no id of its own.
    pub client_order_id: u32,
}

/// The node's bytes, for a test or a fixture that has to write one.
pub fn node_bytes(node: &OrderNodeV0) -> &[u8] {
    bytemuck::bytes_of(node)
}

/// Encoded width of one arena slot.
pub const NODE_BYTES: usize = core::mem::size_of::<OrderNodeV0>();
const _: () = assert!(NODE_BYTES == 104);

impl OrderNodeV0 {
    pub fn user_ref(&self) -> UserRefV0 {
        UserRefV0 {
            authority: self.authority,
            sub_account_id: self.sub_account_id,
        }
    }

    /// `self.user_ref() == *user`, without building the ref.
    pub fn is_owned_by(&self, user: &UserRefV0) -> bool {
        self.authority == user.authority && self.sub_account_id == user.sub_account_id
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

    /// The order only reduces its owner's position. The book clamps a fill
    /// against it to the owner's `base_cover` cap.
    pub fn is_reduce_only(&self) -> bool {
        self.is_bit_flag_set(OrderBitFlag::ReduceOnly)
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

/// Every live order in a market account, paired with its arena index. Iteration is in
/// arena order, because a reader after one user's orders does not care about price
/// queues and a walk of the array skips the links. A short account yields nothing
/// rather than failing, because an account feed can hand over a partial write. A
/// misaligned one decodes normally: `pod_read_unaligned` reads a copy, so the slice
/// need not start on the node's alignment. The index is the node's own slot, which
/// is half of the handle a cancel takes.
pub fn live_orders(account_data: &[u8]) -> impl Iterator<Item = (u32, OrderNodeV0)> + '_ {
    account_data
        .get(ORDERS_OFFSET..)
        .unwrap_or_default()
        .chunks_exact(NODE_BYTES)
        .enumerate()
        .map(|(index, bytes)| {
            (
                index as u32,
                bytemuck::pod_read_unaligned::<OrderNodeV0>(bytes),
            )
        })
        .filter(|(_, node)| node.is_open())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(nodes: &[OrderNodeV0]) -> Vec<u8> {
        let mut data = vec![0u8; ORDERS_OFFSET];
        for node in nodes {
            data.extend_from_slice(bytemuck::bytes_of(node));
        }

        data
    }

    fn node(open: bool, client_order_id: u32) -> OrderNodeV0 {
        OrderNodeV0 {
            authority: Pubkey::new_from_array([7u8; 32]),
            price: 100,
            base_asset_amount: 5,
            activation_slot: 0,
            max_ts: 0,
            order_id: client_order_id as u64,
            placed_slot: 0,
            prev: NIL,
            next: NIL,
            taker_origin_prev: NIL,
            taker_origin_next: NIL,
            bit_flags: OrderBitFlag::Open.bit_if(open),
            padding0: 0,
            sub_account_id: 0,
            client_order_id,
        }
    }

    /// The index is half of a cancel hint, so it has to be the node's own slot
    /// rather than its place in the live set. A free node between two live
    /// ones is what tells the two apart.
    #[test]
    fn live_orders_report_their_arena_index_not_their_position() {
        let data = account(&[node(true, 10), node(false, 0), node(true, 30)]);
        let live: Vec<(u32, u32)> = live_orders(&data)
            .map(|(index, node)| (index, node.client_order_id))
            .collect();
        assert_eq!(live, vec![(0, 10), (2, 30)]);
    }

    /// An account feed can hand over a short or partly written account, and a
    /// reader that panics on one crashes the whole publisher.
    #[test]
    fn a_short_account_yields_nothing_rather_than_failing() {
        assert_eq!(live_orders(&[]).count(), 0);
        assert_eq!(live_orders(&vec![0u8; ORDERS_OFFSET - 1]).count(), 0);
        // A trailing partial node is dropped, not decoded.
        let mut data = account(&[node(true, 1)]);
        data.truncate(data.len() - 1);
        assert_eq!(live_orders(&data).count(), 0);
    }
}
