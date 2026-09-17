// The CLOB market account's order-node layout.
//
// # Why this is a crate and not the book's private business
//
// `clob-wire` states the rule this crate is the exception to. A caller that
// reads the market account's bytes reads the book's memory rather than calling
// it, and on chain that is always wrong. A shared layout is an ABI that two
// programs can disagree about, and velocity removed every such read.
//
// An indexer is the case the rule does not cover. It has to answer which orders
// a user holds, over every order on the book, at the tick rate of a live feed.
// No call answers that. `orders_v0` describes refs a caller already holds.
// `quote_v0` reports the depth a taker of some size would reach, which is a
// different question and a truncated answer. What is left is the account, and
// the account is public data an indexer already subscribes to.
//
// So the layout is declared once, here, and the book uses these types rather
// than its own. An indexer that decodes a node cannot drift from the book that
// wrote it, because there is one declaration. This crate must never become a
// way for another program to read the book. Nothing on chain depends on it, and
// an assertion inside the book pins [`ORDERS_OFFSET`] so the book stays free to
// move anything above it.
//
// # What is not here
//
// The header, the free list, the two sorted lists, and every traversal over
// them. A reader that wants the best bid asks the book. A reader that wants
// every live order walks the arena, which needs no links at all.

// The v2 IdlType derive emits `anchor_lang::`, so the v2 IDL build points that
// path at the fork. The feature is undefined in the v1 crate, so this is inert.
#[cfg(feature = "idl-build-v2")]
extern crate anchor_lang_v2 as anchor_lang;

pub use quoter_spec::{SideV0 as Side, UserRefV0};
use {
    bytemuck::{Pod, Zeroable},
    solana_address::Address as Pubkey,
};

/// The list terminator: no next node, no head, no free slot.
pub const NIL: u32 = u32::MAX;

/// Account-data offset of the order-node tail.
///
/// The market is `[disc][ClobHeaderV0][len: u32][OrderNodeV0 tail]`, padded to
/// the node's alignment, so this is a fact about the header's size. It is a
/// number here rather than an expression because the header is the book's and
/// stays the book's. `clob::state` asserts the two agree at compile time, so a
/// header that grows fails the book's own build rather than moving the arena
/// under a reader.
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

/// One arena slot. It holds a live order threaded into a side's price-time
/// list, or a free node threaded into the free list through `next`. The
/// velocity `User` is stored inline and there is no seat table, so user
/// capacity is order capacity, governed by the one eviction rule.
///
/// 104 bytes, with one spare byte. The node is the per-order cost of a market,
/// because capacity times this size is the account's rent. Growth room lives on
/// the header, and a field belongs here only when a walk of one side has to
/// read it. The two taker-origin links are that case. The reservation walk
/// enumerates the claimants on a side, and reaching them through the
/// price-sorted list would read every order on it.
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
    /// Toward the newest taker-origin order on this side. [`NIL`] marks the
    /// tail.
    ///
    /// The side's taker-origin orders are threaded on their own list, in rest
    /// order. A crossing remainder claims the depth it crosses, so every read
    /// of a side has to enumerate the remainders that claim it. The list makes
    /// that cost the number of remainders instead of the number of orders.
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

    pub fn side(&self) -> Side {
        if self.is_bit_flag_set(OrderBitFlag::Ask) {
            Side::Ask
        } else {
            Side::Bid
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

/// Every live order in a market account, paired with its arena index.
///
/// The iteration is in arena order rather than book order. A reader that wants
/// one user's orders does not care where they sit in a price queue, and a walk
/// of the array skips the links. A short or misaligned account yields nothing
/// rather than failing, because an account feed can hand over a partial write.
///
/// The index is the node's own slot, not its position among live orders. It is
/// half of the handle a cancel takes, so a count of live orders would hand a
/// caller a hint pointing at some other order.
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
