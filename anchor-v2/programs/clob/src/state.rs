//! CLOB market account layout and the wire types the quoter interface
//! exchanges. Design doc: "PropAMM + Order Flow Design".
//!
//! The market is a [`Slab`]: `[disc][ClobHeaderV0][len][OrderNodeV0 tail]`.
//! Capacity is derived from the account's data length at load, so each
//! market picks its arena size at creation (and can grow via realloc).
//!
//! This module is layout only. The book algorithm over these structs — free
//! list, the two best-first sorted intrusive lists, and every arena access —
//! lives in [`crate::book`]; the response encoder lives in
//! [`crate::response`].

use {
    anchor_lang_v2::{accounts::Slab, prelude::*},
    static_assertions::{const_assert, const_assert_eq},
};

pub const ZERO_ADDRESS: Address = Address::new_from_array([0u8; 32]);

/// Response region size. Responses live in the header (quoter interface:
/// return data carries only a [`ResponsePointerV0`]), so payload size is not
/// bound by the 1024-byte return-data cap.
pub const RESPONSE_BUFFER_BYTES: usize = 8192;

// Borsh widths of the response wire types. Each is built from the field
// types of the struct it measures, and
// `tests::response::wire_widths_match_the_response_types` pins every one
// against wincode's encoding of that struct — so a field added to a wire type
// fails a test instead of silently shifting the ceilings below.

/// Byte width of a borsh sequence count (a `Vec`'s length prefix).
pub const COUNT_BYTES: usize = core::mem::size_of::<u32>();

/// Borsh width of a [`UserRefV0`]: 32-byte authority + u16 sub-account. The
/// response encoder compares and writes users in this form, so the constant
/// is the single definition of that width.
pub const USER_REF_BYTES: usize = core::mem::size_of::<Address>() + core::mem::size_of::<u16>();

/// Borsh width of a [`PriceLevel`].
pub const PRICE_LEVEL_BYTES: usize = 2 * core::mem::size_of::<u64>();

/// Borsh width of one `completed_order_ids` entry.
pub const ORDER_ID_BYTES: usize = core::mem::size_of::<u64>();

/// Borsh width of a [`UserBalanceChange`] that completed no orders — the
/// narrowest a balance-change record can be, and the width a full response of
/// them is derived from.
pub const CHANGE_MIN_BYTES: usize = USER_REF_BYTES + 2 * core::mem::size_of::<u64>() + COUNT_BYTES;

/// Borsh width of a [`CancelledRemainderV0`].
pub const CANCELLED_BYTES: usize = USER_REF_BYTES + 2 * core::mem::size_of::<u64>();

// Hard ceilings on the per-market response/batch config — bound by the
// response region and the 32KB program heap, which don't vary per market.
// The per-market operating points live on the header. Partial execution is
// the interface contract; the router sees smaller balance changes.

/// Ceiling on `max_quote_levels`: a [`QuoteResponseV0`] is a count followed
/// by that many [`PriceLevel`]s.
pub const QUOTE_LEVELS_CEILING: u16 =
    ((RESPONSE_BUFFER_BYTES - COUNT_BYTES) / PRICE_LEVEL_BYTES) as u16;

pub const EXECUTE_FILLS_CEILING: u16 = 128;

/// Ceiling on `max_execute_users`. An [`ExecuteResponseV0`] is
/// `[changes count][records…][cancelled count][at most one cancelled]`, and a
/// record is [`CHANGE_MIN_BYTES`] plus [`ORDER_ID_BYTES`] per order it
/// completed. Every completed id belongs to a fill, so the whole id space is
/// bounded by `EXECUTE_FILLS_CEILING`: reserving it here — rather than
/// dividing the region by the record width alone — is what makes a market
/// configured *at* this ceiling unable to overrun the response region,
/// whatever the book holds.
pub const EXECUTE_USERS_CEILING: u16 = ((RESPONSE_BUFFER_BYTES
    - 2 * COUNT_BYTES
    - CANCELLED_BYTES
    - EXECUTE_FILLS_CEILING as usize * ORDER_ID_BYTES)
    / CHANGE_MIN_BYTES) as u16;

// The widest response either instruction can produce at the ceilings fits the
// region, so `ResponseTooLarge` is unreachable for a market whose config the
// init/update checks accepted.
const_assert!(
    COUNT_BYTES + QUOTE_LEVELS_CEILING as usize * PRICE_LEVEL_BYTES <= RESPONSE_BUFFER_BYTES
);
const_assert!(
    2 * COUNT_BYTES
        + CANCELLED_BYTES
        + EXECUTE_FILLS_CEILING as usize * ORDER_ID_BYTES
        + EXECUTE_USERS_CEILING as usize * CHANGE_MIN_BYTES
        <= RESPONSE_BUFFER_BYTES
);

/// Taker direction, as passed through the quoter interface.
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub enum Direction {
    Long,
    Short,
}

impl Direction {
    /// The book side this taker direction consumes.
    pub fn book_side(self) -> Side {
        match self {
            Direction::Long => Side::Ask,
            Direction::Short => Side::Bid,
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Direction::Long => 0,
            Direction::Short => 1,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub enum Side {
    Bid,
    Ask,
}

impl Side {
    pub fn to_u8(self) -> u8 {
        match self {
            Side::Bid => 0,
            Side::Ask => 1,
        }
    }

    /// Whether `resting` is a worse price for this side's makers than
    /// `candidate` — i.e. the point a new order at `candidate` cuts in front
    /// of. Bids rank high-to-low, asks low-to-high.
    pub fn is_worse_price(self, resting: u64, candidate: u64) -> bool {
        match self {
            Side::Bid => resting < candidate,
            Side::Ask => resting > candidate,
        }
    }

    /// The node bit that marks membership of this side (bids carry none —
    /// `OrderBitFlag::Ask` clear means bid).
    pub fn side_bit(self) -> u8 {
        match self {
            Side::Bid => 0,
            Side::Ask => OrderBitFlag::Ask as u8,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OrderBitFlag {
    /// Node holds a live order (clear = node is on the free list).
    Open = 1,
    /// Order is an ask (clear = bid).
    Ask = 2,
}

#[account]
pub struct ClobHeaderV0 {
    /// Admin able to configure the market.
    pub authority: Address,
    /// Only signer allowed to place/cancel/execute (the velocity signer PDA;
    /// velocity verifies `User` authority and flow-attestation policy —
    /// including zero-delay activation — before CPI'ing here).
    pub place_authority: Address,
    /// Prices must be a multiple of this (PRICE_PRECISION). Enforced at
    /// placement, not baked into the stored representation, so it can be
    /// changed without repricing the resting book.
    pub order_tick_size: u64,
    /// Sizes must be a multiple of this (base precision).
    pub order_step_size: u64,
    /// Floor on order size so every resting order has real capital at risk.
    pub min_order_size: u64,
    /// Base units per whole unit (velocity perps: 1e9; spot varies).
    /// Immutable after init — resting order sizes are denominated in it.
    pub base_precision: u64,
    /// Starts at 1 so a zeroed (freed) node can never match a live order id.
    pub next_order_id: u64,
    pub best_bid: u32,
    pub best_ask: u32,
    /// List tails, so eviction of the worst order is O(1).
    pub worst_bid: u32,
    pub worst_ask: u32,
    pub free_head: u32,
    pub free_count: u32,
    pub bid_count: u32,
    pub ask_count: u32,
    /// Default taker speed bump: slots added to the placement slot to get
    /// `activation_slot` when the caller doesn't choose a delay.
    pub default_activation_delay_slots: u32,
    /// Upper bound on a caller-chosen activation delay (auction flow).
    pub max_activation_delay_slots: u32,
    /// Fills race the tx's fixed account set: quote/execute take the set of
    /// users the caller can settle, and an order whose user is absent is
    /// skipped while younger than this many slots (the keeper couldn't have
    /// known it) but fails the call once older (the keeper is stale).
    pub unknown_user_grace_slots: u32,
    /// Soft cap: `evict_worst` is allowed once a side holds at least this
    /// many orders. Eviction is crank-mediated through velocity (so the
    /// evicted maker's margin aggregates stay exact); the buffer up to the
    /// per-side hard cap is what the crank has to work with.
    pub evict_threshold_per_side: u32,
    /// Velocity perp market index this book serves.
    pub market_index: u16,
    /// Per-market response/batch tuning, each bounded by its `*_CEILING`.
    pub max_quote_levels: u16,
    pub max_execute_fills: u16,
    pub max_execute_users: u16,
    /// Growth room: four pubkeys' worth of reserved bytes so a later field
    /// (a fee destination, a second authority, a paused-operations bitmap)
    /// can be added without moving `response`, changing the account size, or
    /// migrating every live market. Must stay zero until claimed.
    pub padding: [u8; 128],
    /// Scratch region `quote_v0`/`execute_v0` write their borsh response
    /// into; return data carries a [`ResponsePointerV0`] locating it. Last
    /// field, so [`RESPONSE_OFFSET`] is the header size minus its length.
    pub response: [u8; RESPONSE_BUFFER_BYTES],
}

const_assert_eq!(core::mem::size_of::<ClobHeaderV0>(), 8480);

/// The market account: header + order-node tail, capacity from data length.
pub type ClobMarketV0 = Slab<ClobHeaderV0, OrderNodeV0>;

/// Account-data offset of the header's `response` region.
pub const RESPONSE_OFFSET: usize = 8 + core::mem::size_of::<ClobHeaderV0>() - RESPONSE_BUFFER_BYTES;

/// Account-data offset of the order-node tail: `[disc][H][len: u32]` padded
/// to the node's 8-byte alignment.
pub const ORDERS_OFFSET: usize = (8 + core::mem::size_of::<ClobHeaderV0>() + 4).next_multiple_of(8);

/// One arena slot: a live order threaded into a side's price-time list, or a
/// free node threaded into the free list via `next`. The velocity `User` is
/// stored inline (no seat table): user capacity is order capacity, governed
/// by the one eviction rule.
///
/// Deliberately kept at 96 bytes with only the five spare bytes below: the
/// node is the per-order cost of a market (capacity × this size is the
/// account's rent), so growth room lives on [`ClobHeaderV0`] instead. A
/// future field wider than those spare bytes needs a `OrderNodeV1` arena.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
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
    /// Slot the order was placed — age input for the unknown-user grace
    /// check (see `ClobHeaderV0::unknown_user_grace_slots`).
    pub placed_slot: u64,
    /// Toward the best of book; `NIL` if head.
    pub prev: u32,
    /// Away from the best of book (or next free node); `NIL` if tail.
    pub next: u32,
    pub bit_flags: u8,
    pub padding0: u8,
    /// Sub-account half of the user identity (see `authority`).
    pub sub_account_id: u16,
    pub padding: [u8; 4],
}

const_assert_eq!(core::mem::size_of::<OrderNodeV0>(), 96);

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

    pub fn side(&self) -> Side {
        if self.is_bit_flag_set(OrderBitFlag::Ask) {
            Side::Ask
        } else {
            Side::Bid
        }
    }

    pub fn is_expired(&self, now: i64) -> bool {
        self.max_ts != 0 && self.max_ts < now
    }

    pub fn is_active(&self, slot: u64) -> bool {
        self.activation_slot <= slot
    }
}

/// Order handle: an O(1) node hint verified against the order id, so a stale
/// hint (node freed/reused) fails closed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct OrderRefV0 {
    pub node_index: u32,
    pub order_id: u64,
}

/// A velocity user in its *derivable* form: authority wallet + sub-account
/// index. Both the `User` PDA (`["user", authority, sub_account_id]`) and
/// the `UserStats` PDA (`["user_stats", authority]`) derive from it, which
/// is why the book stores this rather than the `User` account key — an
/// off-chain reader (a relay resolver staging a crank) can reach every
/// user-derived account from the node alone, where a stored `User` key is a
/// dead end (its authority lives inside account data the reader can't
/// load).
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct UserRefV0 {
    pub authority: Address,
    pub sub_account_id: u16,
}

impl UserRefV0 {
    /// The user's borsh encoding, for comparing against and writing into the
    /// response region without a heap round-trip.
    pub fn to_bytes(self) -> [u8; USER_REF_BYTES] {
        let mut bytes = [0u8; USER_REF_BYTES];
        bytes[..32].copy_from_slice(&self.authority.to_bytes());
        bytes[32..].copy_from_slice(&self.sub_account_id.to_le_bytes());
        bytes
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct PriceLevel {
    pub price: u64,
    pub size: u64,
}

/// One user's share of an executed fill. Mirrors velocity's quoter-interface
/// `UserBalanceChange`.
///
/// `execute` writes this encoding into the response region field by field
/// (see [`crate::response`]) rather than serializing this struct — the type
/// remains the schema of record for that layout, and the response unit
/// tests pin the two against each other.
#[derive(Clone, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct UserBalanceChange {
    pub user: UserRefV0,
    pub base_size: u64,
    pub quote_size: u64,
    /// Orders of this user fully consumed (and removed) by the fill, by id.
    /// The caller decrements the user's open-order count by the length, and
    /// the ids let it release per-order state it keeps against the book (a
    /// placed trigger slot). Sub-min culls ride the separate `cancelled` vec
    /// because their remainders also need unwinding.
    pub completed_order_ids: Vec<u64>,
}

/// Where in the market account the borsh response was written. Returned via
/// return data by `quote_v0`/`execute_v0`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct ResponsePointerV0 {
    pub offset: u32,
    pub len: u32,
}

#[derive(Clone, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct QuoteResponseV0 {
    /// Levels the quoter will fill at, best price first.
    pub levels: Vec<PriceLevel>,
}

#[derive(Clone, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct ExecuteResponseV0 {
    pub balance_changes: Vec<UserBalanceChange>,
    /// Sub-min remainders removed by this execute (see
    /// [`CancelledRemainderV0`]).
    pub cancelled: Vec<CancelledRemainderV0>,
}

/// A removed order, for events (cancel/evict/expire).
#[derive(Clone, Copy, Debug)]
pub struct RemovedOrder {
    pub user: UserRefV0,
    pub order_id: u64,
    pub price: u64,
    pub base_asset_amount: u64,
    pub side: Side,
}

/// What `execute` hands back: where the wire response was written, plus the
/// per-fill detail the execute event carries (the response merges fills by
/// user, so the event's order-level view can't be recovered from it).
pub struct ExecuteOutcome {
    pub response: ResponsePointerV0,
    pub fills: Vec<crate::events::FillSlimV0>,
    /// Order culled because its post-fill remainder fell below
    /// `min_order_size`. At most one per execute — a partial fill only
    /// happens when the taker's size runs out, which ends the walk.
    pub cancelled_order_id: Option<u64>,
}

/// Wire form of a removed order — return data of cancel/evict/expire, so
/// velocity can decrement the maker's open-order aggregates. `side` tells
/// velocity whether the remaining size unwinds `open_bids` or `open_asks`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct RemovedOrderV0 {
    pub user: UserRefV0,
    pub order_id: u64,
    pub price: u64,
    pub base_asset_amount: u64,
    pub side: Side,
}

/// A sub-`min_order_size` remainder culled during execute, on the wire so
/// velocity decrements the maker's aggregates (the maker was just filled,
/// so their `User` is always in the loaded set).
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CancelledRemainderV0 {
    pub user: UserRefV0,
    pub order_id: u64,
    pub base_asset_amount: u64,
}

/// `activation_slot` is computed by the instruction handler: placement slot
/// plus the default delay, or a chosen delay clamped to
/// `max_activation_delay_slots`. Zero-delay (attested-flow) placement is
/// velocity policy — the CLOB trusts its `place_authority`.
#[derive(Clone, Copy, Debug)]
pub struct PlaceOrderParams {
    pub side: Side,
    pub price: u64,
    pub base_asset_amount: u64,
    pub user: UserRefV0,
    pub activation_slot: u64,
    pub placed_slot: u64,
    pub max_ts: i64,
}

/// Per-market configuration, set at init (also the init wire args).
/// `base_precision` and `market_index` are immutable afterwards; the rest
/// are updatable via `update_market_v0`.
#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct MarketConfigV0 {
    pub market_index: u16,
    pub base_precision: u64,
    pub order_tick_size: u64,
    pub order_step_size: u64,
    pub min_order_size: u64,
    pub default_activation_delay_slots: u32,
    pub max_activation_delay_slots: u32,
    pub unknown_user_grace_slots: u32,
    pub evict_threshold_per_side: u32,
    pub max_quote_levels: u16,
    pub max_execute_fills: u16,
    pub max_execute_users: u16,
}
