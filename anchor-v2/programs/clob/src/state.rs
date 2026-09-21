//! CLOB market account layout and the wire types the quoter interface
//! exchanges.
//!
//! The market is a [`Slab`]: `[disc][ClobHeaderV0][len][OrderNodeV0 tail]`.
//! The load derives capacity from the account's data length. Each market
//! picks its arena size at creation and can grow it with realloc.
//!
//! This module is layout only. [`crate::book`] holds the book algorithm over
//! these structs: the free list, the two best-first sorted intrusive lists,
//! and every arena access. The response framing is `quoter-spec`'s, and
//! `quote_v0` and `execute_v0` stream into the region below through its
//! writers.
//!
//! The header is declared here. The order node is declared in `clob-state`,
//! because an off-chain indexer decodes the same nodes to answer which orders
//! a user holds. That crate says why the account is the only place that
//! answer can come from. The coupling costs one number, [`ORDERS_OFFSET`],
//! and the assertion below fails the build when the two disagree.

use {
    anchor_lang::{accounts::Slab, prelude::*},
    relay_spec::RelayBlockV0,
    static_assertions::{const_assert, const_assert_eq},
};

/// Conditions this market hosts, in the fixed slots a resolver addresses them
/// by. The book maintains each one, so none is a best-effort poll.
///
/// An order past its `max_ts`, the earliest any live order carries.
pub const CRANK_EXPIRY: usize = 0;
/// An order that reaches its `activation_slot`. Nothing on chain changes when
/// the slot arrives. It is the slot at which a counterparty lined up against
/// a speed-bumped order expects the match to be possible.
pub const CRANK_ACTIVATION: usize = 1;
/// A side grown to its eviction threshold. Watches this account's own side
/// counts.
pub const CRANK_CAPACITY: usize = 2;
/// The book crossing itself. It watches this account's own side heads. A
/// crossing order is always a new best, so the watch catches every cross the
/// moment it appears.
pub const CRANK_CROSS: usize = 3;
/// Conditions hosted per market.
pub const CRANK_CONDITIONS: usize = 4;

// A caller that registers one resolver for several of these learns which one
// fired by index. The mapping belongs to `clob-wire`, so these assert against
// it.
const_assert_eq!(CRANK_EXPIRY, clob_wire::CRANK_SLOT_EXPIRY as usize);
const_assert_eq!(CRANK_ACTIVATION, clob_wire::CRANK_SLOT_ACTIVATION as usize);
const_assert_eq!(CRANK_CAPACITY, clob_wire::CRANK_SLOT_CAPACITY as usize);
const_assert_eq!(CRANK_CROSS, clob_wire::CRANK_SLOT_CROSS as usize);
/// Accounts a registered resolver takes. The capacity is [`RelayBlockV0`]'s
/// minimum granularity of 8. The book stores whatever list the registering
/// program hands it.
pub const CRANK_RESOLVER_CAPACITY: usize = 8;

pub use quoter_spec::ZERO_ADDRESS;

/// [`ClobHeaderV0::reservation_grace_slots`] a fresh market starts with.
///
/// The window covers the transaction that resolves a cross. It is also the
/// longest the claimed depth stays out of the matchable set.
pub const DEFAULT_RESERVATION_GRACE_SLOTS: u16 = 32;

/// Ceiling on [`ClobHeaderV0::reservation_grace_slots`], enforced by
/// `update_market_v0`. A transaction is invalid more than 150 slots after its
/// blockhash, so a wider grace cannot help a crank land. It only holds the
/// claimed depth out of the matchable set for longer.
pub const RESERVATION_GRACE_SLOTS_CEILING: u16 = 150;

/// Response region size. A response lives in the header, because return data
/// carries only a [`ResponsePointerV0`]. The payload size is therefore not
/// bound by the 1024-byte return-data cap.
pub const RESPONSE_BUFFER_BYTES: usize = {
    let widest = 4 * RESPONSE_LEN_BYTES
        + EXECUTE_FILLS_CEILING as usize * (CHANGE_BYTES + COMPLETED_BYTES)
        + CANCELLED_BYTES
        + PARTIAL_BYTES;
    // The records are read in place, so the region must start on an 8-byte
    // step. The region sits at the end of the header, so its size sets that
    // start.
    widest.next_multiple_of(quoter_spec::LEN_BYTES)
};

// Widths of the response wire types. Each one comes from `quoter-spec`'s
// declaration of the record it measures rather than from a restatement here.
// The ceilings below are arithmetic over them, so a field added to a record
// moves them. `tests::response::wire_widths_match_the_response_types` pins
// every width against wincode's encoding of that record.

/// Byte width of a borsh-framed sequence count (a `Vec`'s length prefix).
pub const COUNT_BYTES: usize = core::mem::size_of::<u32>();

/// Width of a sequence length in the response region, which is wincode's
/// framing rather than borsh's. [`COUNT_BYTES`] is the borsh-framed count the
/// event records carry. The two are separate constants because widening this
/// one once changed an emitted event.
pub const RESPONSE_LEN_BYTES: usize = quoter_spec::LEN_BYTES;

/// Width of a [`UserRefV0`]: 32-byte authority + u16 sub-account.
pub const USER_REF_BYTES: usize = quoter_spec::UserRefV0::SIZE;

/// Encoded width of a [`PriceLevel`].
pub const PRICE_LEVEL_BYTES: usize = quoter_spec::PRICE_LEVEL_BYTES;

/// Width of an order id in an event payload. An event carries borsh framing
/// rather than the response wire's. [`crate::emit`] sizes its buffers from
/// this.
pub const ORDER_ID_BYTES: usize = core::mem::size_of::<u64>();

/// Width of the placing caller's own order id, which is what the bulk id
/// lists in the events carry.
pub const CLIENT_ORDER_ID_BYTES: usize = core::mem::size_of::<u32>();

/// Width of a [`UserBalanceChangeV0`]. It is one fixed stride. The orders a
/// change consumed ride their own section, so a change cannot grow.
pub const CHANGE_BYTES: usize = quoter_spec::CHANGE_BYTES;

/// Width of a [`CancelledRemainderV0`].
pub const CANCELLED_BYTES: usize = quoter_spec::CANCELLED_BYTES;

/// Width of a [`CompletedOrderV0`].
pub const COMPLETED_BYTES: usize = quoter_spec::COMPLETED_BYTES;

/// Width of a [`PartiallyFilledOrderV0`]. There is at most one per execute,
/// so it is a flat addition to the region rather than a per-fill stride.
pub const PARTIAL_BYTES: usize = quoter_spec::PARTIAL_BYTES;

/// Width of a [`RemovedOrderV0`], the return data of `cancel_order_v0`,
/// `evict_worst_v0` and `remove_expired_v0`. Nothing here sizes a buffer from
/// it, because anchor serializes the value. Velocity reads those bytes by
/// offset, so the width is pinned rather than assumed.
pub const REMOVED_ORDER_BYTES: usize = USER_REF_BYTES
    + 3 * core::mem::size_of::<u64>()
    + core::mem::size_of::<u32>()
    // side + taker_origin + reduce_only.
    + 3
    + core::mem::size_of::<i64>();

// Hard ceilings on the per-market response and batch config. The response
// region and the 32KB program heap bound them, and neither varies per market.
// The per-market operating points live on the header. Partial execution is
// the interface contract, so the router sees smaller balance changes.

/// Trailing bytes of a [`QuoteResponseV0`]. The withheld report is one
/// [`PriceLevel`] written after the ladder.
pub const WITHHELD_REPORT_BYTES: usize = PRICE_LEVEL_BYTES;

/// Ceiling on `max_quote_levels`. A [`QuoteResponseV0`] is a count, that many
/// [`PriceLevel`]s, then the withheld report.
pub const QUOTE_LEVELS_CEILING: u16 =
    ((RESPONSE_BUFFER_BYTES - RESPONSE_LEN_BYTES - WITHHELD_REPORT_BYTES) / PRICE_LEVEL_BYTES)
        as u16;

/// Width of an [`L3RowV0`].
pub const L3_ROW_BYTES: usize = quoter_spec::L3_ROW_BYTES;

/// Rows one `quote_l3_v0` may report: a count, that many rows, then the
/// one-byte marker saying whether depth remains. This ceiling must not widen
/// the region, because the market account rides every CPI and costs compute
/// per byte. A caller asks again from where the rows stopped.
pub const L3_ROWS_CEILING: u16 =
    ((RESPONSE_BUFFER_BYTES - RESPONSE_LEN_BYTES - 1) / L3_ROW_BYTES) as u16;

/// Orders one `execute_v0` may consume.
///
/// About 1 KB of extra response region put `crank_cross_match` over the 200k
/// per-instruction compute budget, because that crank CPIs this book twice.
pub const EXECUTE_FILLS_CEILING: u16 = 113;

/// Hard cap on the orders one `cancel_all_v0` removes. It bounds the removal
/// work, the id list the cancel record logs
/// ([`crate::emit::CANCEL_ALL_RECORD_LOG_BYTES`]), and the maker aggregate
/// unwind. [`CancelAllOutcome::exhaustive`] reports whether the call finished.
pub const CANCEL_ALL_ORDERS_CEILING: u16 = 128;

/// Ceiling on `max_execute_users`. It is how many balance changes fit the
/// region once the other three sections have taken their worst case. Every
/// completed order belongs to a fill, so `EXECUTE_FILLS_CEILING` bounds the
/// completed section rather than any count of users.
pub const EXECUTE_USERS_CEILING: u16 = ((RESPONSE_BUFFER_BYTES
    - 2 * RESPONSE_LEN_BYTES
    - CANCELLED_BYTES
    - EXECUTE_FILLS_CEILING as usize * COMPLETED_BYTES)
    / CHANGE_BYTES) as u16;

// The widest response either instruction can produce at the ceilings fits the
// region. `ResponseTooLarge` is therefore unreachable for a market whose
// config the init and update checks accepted.
const_assert!(
    RESPONSE_LEN_BYTES + QUOTE_LEVELS_CEILING as usize * PRICE_LEVEL_BYTES + WITHHELD_REPORT_BYTES
        <= RESPONSE_BUFFER_BYTES
);
const_assert!(
    2 * RESPONSE_LEN_BYTES
        + CANCELLED_BYTES
        + EXECUTE_FILLS_CEILING as usize * COMPLETED_BYTES
        + EXECUTE_USERS_CEILING as usize * CHANGE_BYTES
        <= RESPONSE_BUFFER_BYTES
);

/// Taker direction and book side, declared in `quoter-spec` with the rest of
/// the request half of this wire.
pub use quoter_spec::{DirectionV0 as Direction, SideV0 as Side};

/// What this program reads into the wire's side beyond its shape. A foreign
/// type takes no inherent impl, and a trait keeps every call site reading as
/// it did.
pub trait ClobSideExt {
    fn is_worse_price(self, resting: u64, candidate: u64) -> bool;
    fn side_bit(self) -> u8;
    fn opposite(self) -> Side;
    fn is_crossed_by(self, price: u64, opposite: u64) -> bool;
}

/// The same for the direction.
pub trait ClobDirectionExt {
    fn book_side(self) -> Side;
}

impl ClobDirectionExt for Direction {
    /// The book side this taker direction consumes.
    fn book_side(self) -> Side {
        self.side()
    }
}

impl ClobSideExt for Side {
    /// Whether `resting` is a worse price for this side's makers than
    /// `candidate`. It marks the point where a new order at `candidate` takes
    /// priority. Bids rank high to low and asks rank low to high.
    fn is_worse_price(self, resting: u64, candidate: u64) -> bool {
        match self {
            Side::Bid => resting < candidate,
            Side::Ask => resting > candidate,
        }
    }

    /// The node bit that marks membership of this side. A bid carries no bit,
    /// because a clear `OrderBitFlag::Ask` means bid.
    fn side_bit(self) -> u8 {
        match self {
            Side::Bid => 0,
            Side::Ask => OrderBitFlag::Ask as u8,
        }
    }

    fn opposite(self) -> Side {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }

    /// Whether an order of this side resting at `price` is crossed by an order
    /// on the opposite side at `opposite`. An ask at or below a bid crosses
    /// that bid. A bid at or above an ask crosses that ask.
    fn is_crossed_by(self, price: u64, opposite: u64) -> bool {
        match self {
            Side::Bid => opposite <= price,
            Side::Ask => opposite >= price,
        }
    }
}

/// What the book requires of an order, and the one shape every read-only
/// answer reports an order in. Declared by `clob-wire`.
pub use clob_wire::{OrderRulesV0, OrderViewV0};

/// Describe one order the way every read-only answer does.
///
/// The node to view mapping lives here rather than on the node. The node is
/// layout, and this is what the book chooses to say about it. One function
/// keeps `next_removal_v0`, `next_cross_v0` and `orders_v0` from disagreeing
/// about what an order looks like.
pub fn order_view(node: &OrderNodeV0, node_index: u32) -> OrderViewV0 {
    OrderViewV0 {
        order_ref: clob_wire::ClobOrderRefV0 {
            node_index,
            order_id: node.order_id,
        },

        client_order_id: node.client_order_id,
        user: node.user_ref(),
        side: node.side(),
        price: node.price,
        base_asset_amount: node.base_asset_amount,
        placed_slot: node.placed_slot,
        max_ts: node.max_ts,
        taker_origin: node.is_taker_origin(),
    }
}

#[account]
pub struct ClobHeaderV0 {
    /// Admin able to configure the market.
    pub authority: Address,
    /// The only signer allowed to place, cancel or execute. It is velocity's
    /// quoter CPI signer PDA. Velocity verifies the `User` authority and the
    /// flow-attestation policy, including zero-delay activation, before it
    /// CPIs here.
    pub place_authority: Address,
    /// Prices must be a multiple of this, in PRICE_PRECISION. The check runs
    /// at placement and the stored price does not encode it. It can therefore
    /// change without repricing the resting book.
    pub order_tick_size: u64,
    /// Sizes must be a multiple of this (base precision).
    pub order_step_size: u64,
    /// Floor on order size so every resting order has real capital at risk.
    pub min_order_size: u64,
    /// Floor on the size of an order that may end a walk. Zero disables it.
    ///
    /// A caller carries at most 48 users, so 49 orders at the top of book can
    /// block the depth behind them. The walk steps over a smaller order.
    pub blocking_min_size: u64,
    /// Base units per whole unit. Velocity perps use 1e9 and spot varies.
    /// Immutable after init, because resting order sizes are denominated in
    /// it.
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
    /// Default taker speed bump. These slots are added to the placement slot
    /// to get `activation_slot` when the caller does not choose a delay.
    pub default_activation_delay_slots: u32,
    /// Upper bound on a caller-chosen activation delay (auction flow).
    pub max_activation_delay_slots: u32,
    /// How far the caller's account set is allowed to lag the book.
    ///
    /// The walk skips an order whose owner is absent while its age, measured
    /// from `activation_slot`, is at most this. Past that the order ends the walk.
    pub unknown_user_grace_slots: u32,
    /// Soft cap. `evict_worst` is allowed once a side holds at least this
    /// many orders. Eviction runs through a velocity crank, which keeps the
    /// evicted maker's margin aggregates exact. The buffer up to the per-side
    /// hard cap is what the crank has to work with.
    pub evict_threshold_per_side: u32,
    /// Velocity perp market index this book serves.
    pub market_index: u16,
    /// Per-market response/batch tuning, each bounded by its `*_CEILING`.
    pub max_quote_levels: u16,
    pub max_execute_fills: u16,
    pub max_execute_users: u16,
    /// The earliest `max_ts` any live order carries, or [`i64::MAX`] when no
    /// live order expires. A removal recomputes it only when it took the order
    /// that held the minimum, so the field may be earlier than the truth. It is
    /// never later, because that would leave work nobody is woken for.
    pub next_expiry_ts: i64,
    /// The earliest `activation_slot` any live order carries that has not yet
    /// arrived, or [`u64::MAX`] when none is pending. A passing slot makes the
    /// field stale without any write, so every mutation recomputes it when the
    /// stored slot is no longer in the future.
    pub next_activation_slot: u64,
    /// The relay conditions that wake a turner for this book's own work.
    ///
    /// The book maintains them as it places and removes orders, so no caller
    /// passes a second account. `set_crank_conditions_v0` names the resolvers.
    pub crank: RelayBlockV0<CRANK_CONDITIONS, CRANK_RESOLVER_CAPACITY>,
    /// Reserved bytes. A later field, such as a fee destination or a
    /// paused-operations bitmap, can claim them without moving `response`,
    /// changing the account size, or migrating every live market. The bytes
    /// must stay zero until a field claims them.
    pub padding: [u8; 104],
    /// Oldest taker-origin order on each side, indexed by [`Side`] (bid 0, ask
    /// 1). [`NIL`] when the side holds none. The list is in rest order. A
    /// taker-origin order is a migrated taker remainder that claims depth on
    /// the other side, so a read of a side enumerates it. See [`crate::book`].
    pub taker_origin_head: [u32; 2],
    /// Newest taker-origin order on each side, so an append is O(1).
    pub taker_origin_tail: [u32; 2],
    /// Taker-origin orders on each side. Bounds the claimant hops one read of
    /// a side may take, so a corrupt list cannot spin.
    pub taker_origin_count: [u16; 2],
    /// Slots past its activation slot for which a taker remainder's claim on
    /// the depth it crosses is still honoured. Past the window the depth is
    /// ordinary again, so a crank that never lands cannot hold the top of book
    /// for ever. Zero ends the claim at activation.
    pub reservation_grace_slots: u16,
    /// Keeps `response` on the 8-byte step its records are cast at.
    pub padding1: [u8; 2],
    /// Scratch region that `quote_v0` and `execute_v0` stream their response
    /// into. Return data carries a [`ResponsePointerV0`] that locates it. It is
    /// the last field, so [`RESPONSE_OFFSET`] is the header size minus its
    /// length. Its size holds the region's 8-byte start, so records cast in place.
    pub response: [u8; RESPONSE_BUFFER_BYTES],
}

// Both programs cast the response records onto these bytes, so the region has
// to start on the step they are read at. Solana gives account data an 8-byte
// start, and every record's alignment divides 8, so this offset is the whole
// condition.
const_assert_eq!(RESPONSE_OFFSET % RESPONSE_LEN_BYTES, 0);

/// The market account: header + order-node tail, capacity from data length.
pub type ClobMarketV0 = Slab<ClobHeaderV0, OrderNodeV0>;

/// Account-data offset of the relay block, which is what a `WatchV0`
/// registers at. `set_crank_conditions_v0` reports it, so a registrant learns
/// it by asking rather than by knowing this account's layout.
pub const CRANK_BLOCK_OFFSET: usize = relay_spec::block_offset!(ClobHeaderV0, crank);

const_assert_eq!(CRANK_BLOCK_OFFSET % 8, 0);

/// The region of this account that changes whenever either side's best moves.
/// `best_bid` and `best_ask` are adjacent, so one watched range covers both. A
/// crossing order is always a new best, so a relay watch here catches every
/// cross. `set_crank_conditions_v0` reports the region.
pub const TOP_OF_BOOK_OFFSET: usize = 8 + core::mem::offset_of!(ClobHeaderV0, best_bid);
pub const TOP_OF_BOOK_BYTES: usize = 2 * core::mem::size_of::<u32>();

/// The region that changes whenever a side's order count moves. `bid_count`
/// and `ask_count` are adjacent, so one watched range covers both. The book's
/// own capacity condition watches it.
pub const SIDE_COUNTS_OFFSET: usize = 8 + core::mem::offset_of!(ClobHeaderV0, bid_count);
pub const SIDE_COUNTS_BYTES: usize = 2 * core::mem::size_of::<u32>();

// The pairs are adjacent, which is what lets one watch cover each.
const_assert_eq!(
    TOP_OF_BOOK_OFFSET + core::mem::size_of::<u32>(),
    8 + core::mem::offset_of!(ClobHeaderV0, best_ask)
);
const_assert_eq!(
    SIDE_COUNTS_OFFSET + core::mem::size_of::<u32>(),
    8 + core::mem::offset_of!(ClobHeaderV0, ask_count)
);

/// Account-data offset of the header's `response` region.
pub const RESPONSE_OFFSET: usize = 8 + core::mem::size_of::<ClobHeaderV0>() - RESPONSE_BUFFER_BYTES;

/// The pointer `quote_v0`/`execute_v0` return for a response of `len` bytes.
pub fn response_pointer(len: usize) -> ResponsePointerV0 {
    ResponsePointerV0::at(RESPONSE_OFFSET, len)
}

/// Account-data offset of the order-node tail. It is `[disc][H][len: u32]`
/// padded to the node's 8-byte alignment.
pub const ORDERS_OFFSET: usize = (8 + core::mem::size_of::<ClobHeaderV0>() + 4).next_multiple_of(8);

// An off-chain reader of this account holds the arena's offset as a number,
// and a number cannot follow a header that moves. That is the whole coupling.
// The header stays free to change as long as its size does not change. When
// the size does change, this build fails rather than the reader.
const_assert_eq!(ORDERS_OFFSET, clob_state::ORDERS_OFFSET);

/// The order-node layout, declared in `clob-state` because an off-chain
/// indexer decodes the same bytes. That crate says why it is the one part of
/// this account a reader outside the program is allowed to know. On chain,
/// only this program reads it.
pub use clob_state::{live_orders, OrderBitFlag, OrderNodeV0, NIL, NODE_BYTES};
/// Order handle, declared by `clob-wire`. That crate owns every shape on the
/// instruction surface, so the bytes this program reads and the bytes its
/// caller writes come from one declaration.
pub use clob_wire::ClobOrderRefV0 as OrderRefV0;
/// A velocity user in derivable form: the authority wallet and the sub-account
/// index, which derive the `User` and `UserStats` PDAs. The book stores this
/// rather than the `User` key, so an off-chain reader reaches both from the
/// node alone. A stored key hides the authority inside account data.
pub use quoter_spec::UserRefV0;
/// The request half of this wire, declared in `quoter-spec` alongside the
/// responses. `USER_SET_CAPACITY` comes from the account-lock budget of the
/// forwarding transaction: 64 locks, minus the 15 a router fill spends before
/// its first maker, minus one for the `UserStats` those makers share.
pub use quoter_spec::{
    user_set_within_capacity, UserCapV0, UserCapsV0, BASE_PRECISION, USER_CAPS_BYTES,
    USER_CAPS_CAPACITY, USER_EXCLUSION_BITMAP_BYTES, USER_SET_CAPACITY, USER_SET_MAX_BYTES,
};

/// Declared by `quoter-spec`; the alias keeps this program's name for it.
pub type PriceLevel = quoter_spec::PriceLevelV0;

/// Which sides a `cancel_all_v0` withdraws. Declared by `quoter-spec`.
/// [`CancelSidesExt`] is this program's reading of them.
pub use quoter_spec::CancelSidesV0;
/// Where in the market account the response was written. Declared by
/// `quoter-spec`, which owns every shape on this wire.
pub use quoter_spec::ResponsePointerV0;
/// One user's share of an executed fill. Mirrors velocity's quoter-interface
/// `UserBalanceChangeV0`. `execute` writes this encoding into the response
/// region field by field rather than serializing this struct. The type stays
/// the schema of record, and the response unit tests pin the two together.
pub use quoter_spec::UserBalanceChangeV0;
pub use quoter_spec::{ExecuteResponseV0, L3ArgsV0, L3ResponseV0, L3RowV0, QuoteResponseV0};

/// What the wire's named sides mean to a book. They name the lists to walk.
pub trait CancelSidesExt {
    fn sides(self) -> &'static [Side];
    fn includes(self, side: Side) -> bool;
}

impl CancelSidesExt for CancelSidesV0 {
    /// The sides to walk, in book order.
    fn sides(self) -> &'static [Side] {
        match (self.has_bids(), self.has_asks()) {
            (true, true) => &[Side::Bid, Side::Ask],
            (true, false) => &[Side::Bid],
            (false, _) => &[Side::Ask],
        }
    }

    fn includes(self, side: Side) -> bool {
        match side {
            Side::Bid => self.has_bids(),
            Side::Ask => self.has_asks(),
        }
    }
}

/// What a `cancel_all_v0` withdrew, aggregated per side.
///
/// Velocity unwinds `open_bids` and `open_asks` by a summed base amount, so
/// the whole sweep costs it the same two calls one cancel does. The cancel
/// record's id list carries the per-order detail an indexer needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CancelAllOutcome {
    pub bid_base_asset_amount: u64,
    pub ask_base_asset_amount: u64,
    pub bid_orders: u32,
    pub ask_orders: u32,
    /// Reduce-only orders among those swept on each side. The caller disarms
    /// its per-user reduce-only tracking by this count.
    pub bid_reduce_only_orders: u32,
    pub ask_reduce_only_orders: u32,
    /// Whether the walk finished every requested side rather than stopping at
    /// [`CANCEL_ALL_ORDERS_CEILING`]. False means orders of this user are
    /// still resting and the caller should repeat the call.
    pub exhaustive: bool,
}

impl CancelAllOutcome {
    pub fn orders(&self) -> u32 {
        self.bid_orders.saturating_add(self.ask_orders)
    }
}

/// Wire form of [`CancelAllOutcome`] and the return data of `cancel_all_v0`.
/// The caller unwinds the maker's aggregates in one pass per side. Declared
/// by `clob-wire`.
pub use clob_wire::CancelAllOutcomeV0;

/// A removed order, for events (cancel/evict/expire).
#[derive(Clone, Copy, Debug)]
pub struct RemovedOrder {
    pub user: UserRefV0,
    pub order_id: u64,
    pub client_order_id: u32,
    pub price: u64,
    pub base_asset_amount: u64,
    pub side: Side,
    pub taker_origin: bool,
    pub reduce_only: bool,
    pub max_ts: i64,
}

impl From<RemovedOrder> for RemovedOrderV0 {
    fn from(removed: RemovedOrder) -> Self {
        Self {
            user: removed.user,
            order_id: removed.order_id,
            client_order_id: removed.client_order_id,
            price: removed.price,
            base_asset_amount: removed.base_asset_amount,
            side: removed.side,
            taker_origin: removed.taker_origin,
            reduce_only: removed.reduce_only,
            max_ts: removed.max_ts,
        }
    }
}

/// What `execute` hands back. It names where the wire response was written,
/// and it carries the per-fill detail the execute event needs. The response
/// merges fills by user, so the event's order-level view cannot be recovered
/// from it.
pub struct ExecuteOutcome {
    pub response: ResponsePointerV0,
    pub fills: Vec<crate::events::FillSlimV0>,
    /// Order culled because its post-fill remainder fell below
    /// `min_order_size`. There is at most one per execute, because a partial
    /// fill happens only when the taker's size runs out, which ends the
    /// walk.
    pub cancelled_client_order_id: Option<u32>,
}

/// Most orders one `fill_v0` may report. A transaction that resolves several
/// remainders pays for one call. The execute record's log buffer is sized
/// against this ceiling, because every order in a batch can leave a
/// sub-minimum remainder to cull.
pub const FILL_BATCH_CEILING: usize = 8;

/// Wire form of a removed order. It is the return data of cancel, evict and
/// expire. Declared by `clob-wire`. Its `taker_origin` flag is the only place
/// this program reports a migrated taker remainder. A caller needs the flag to
/// know which side's price the match settles at.
pub use clob_wire::RemovedOrderV0;
/// What one order in a `fill_v0` came to. Declared by `clob-wire`.
pub use clob_wire::{FillArgsV0, FillOutcomeV0, FillRequestV0, FilledOrderV0 as FilledOrder};
/// A remainder below `min_order_size` culled during execute. It travels on
/// the wire so velocity decrements the maker's aggregates. The maker was just
/// filled, so its `User` is always in the loaded set.
pub use quoter_spec::CancelledRemainderV0;
pub use quoter_spec::CompletedOrderV0;
/// The one order a fill left resting smaller than it found it. A merged
/// balance change cannot report that per-order half of a fill. Declared by
/// `quoter-spec`.
pub use quoter_spec::PartiallyFilledOrderV0;

/// The instruction handler computes `activation_slot`. It is the placement
/// slot plus the default delay, or plus a chosen delay clamped to
/// `max_activation_delay_slots`. Zero-delay placement for attested flow is
/// velocity policy. The CLOB trusts its `place_authority`.
#[derive(Clone, Copy, Debug)]
pub struct PlaceOrderParams {
    pub side: Side,
    pub price: u64,
    pub base_asset_amount: u64,
    pub user: UserRefV0,
    pub activation_slot: u64,
    pub placed_slot: u64,
    pub max_ts: i64,
    /// Current unix timestamp. Read only by the `reject_if_crossed` check,
    /// which must tell an expired opposite head from a live one.
    pub now: i64,
    /// Marks the order [`OrderBitFlag::TakerOrigin`].
    pub taker_origin: bool,
    /// Stored on the node and reported back, never read. See
    /// [`OrderNodeV0::client_order_id`].
    pub client_order_id: u32,
    /// Refuse the placement when the order would cross the opposite best,
    /// rather than resting it crossed.
    pub reject_if_crossed: bool,
    /// Marks the order [`OrderBitFlag::ReduceOnly`]. A fill against it is
    /// clamped to the owner's `base_cover` cap at match time.
    pub reduce_only: bool,
}

/// Per-market configuration, set at init. It is also the init wire args.
/// `base_precision` and `market_index` are immutable afterwards.
/// `update_market_v0` updates the rest.
#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build", derive(anchor_lang::IdlType))]
pub struct MarketConfigV0 {
    pub market_index: u16,
    pub base_precision: u64,
    pub order_tick_size: u64,
    pub order_step_size: u64,
    pub min_order_size: u64,
    /// See [`ClobHeaderV0::blocking_min_size`]. Zero disables the floor.
    pub blocking_min_size: u64,
    pub default_activation_delay_slots: u32,
    pub max_activation_delay_slots: u32,
    pub unknown_user_grace_slots: u32,
    pub evict_threshold_per_side: u32,
    pub max_quote_levels: u16,
    pub max_execute_fills: u16,
    pub max_execute_users: u16,
}
