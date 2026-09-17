//! CLOB market account layout and the wire types the quoter interface
//! exchanges. Design doc: "PropAMM + Order Flow Design".
//!
//! The market is a [`Slab`]: `[disc][ClobHeaderV0][len][OrderNodeV0 tail]`.
//! Capacity is derived from the account's data length at load, so each
//! market picks its arena size at creation (and can grow via realloc).
//!
//! This module is layout only. The book algorithm over these structs — free
//! list, the two best-first sorted intrusive lists, and every arena access —
//! lives in [`crate::book`]. The response framing is `quoter-spec`'s, and
//! `quote_v0`/`execute_v0` stream into the region below through its writers.
//!
//! The header is declared here; the order node is declared in `clob-state`,
//! because an off-chain indexer decodes the same nodes to answer which orders
//! a user holds. That crate says why the account is the only place that answer
//! can come from. The coupling it costs is one number — [`ORDERS_OFFSET`] —
//! and the assertion below is what keeps the two honest.

use {
    anchor_lang::{accounts::Slab, prelude::*},
    relay_spec::RelayBlockV0,
    static_assertions::{const_assert, const_assert_eq},
};

/// Conditions this market hosts, in the fixed slots a resolver addresses them
/// by. Each is a fact about the book that a turner has to be woken for.
///
/// The book keeps every wake current itself. There is no fallback poll here:
/// a poll exists to cover a hint whose maintenance is best-effort, and these
/// are maintained by the same code that changes what they describe.
///
/// An order past its `max_ts`. Wakes at the earliest one any live order
/// carries.
pub const CRANK_EXPIRY: usize = 0;
/// An order reaching its `activation_slot`. Nothing on chain changes when it
/// arrives, but it is exactly when a counterparty lined up against a
/// speed-bumped order expects the match to be possible.
pub const CRANK_ACTIVATION: usize = 1;
/// A side grown to its eviction threshold. Watches this account's own side
/// counts.
pub const CRANK_CAPACITY: usize = 2;
/// The book crossing itself. Watches this account's own side heads — a
/// crossing order is by definition a new best, so the watch catches every
/// cross the moment it appears.
pub const CRANK_CROSS: usize = 3;
/// Conditions hosted per market.
pub const CRANK_CONDITIONS: usize = 4;

// A caller registering one resolver for several of these is told which fired
// by index, so the mapping is `clob-wire`'s and these are asserted against it.
const_assert_eq!(CRANK_EXPIRY, clob_wire::CRANK_SLOT_EXPIRY as usize);
const_assert_eq!(CRANK_ACTIVATION, clob_wire::CRANK_SLOT_ACTIVATION as usize);
const_assert_eq!(CRANK_CAPACITY, clob_wire::CRANK_SLOT_CAPACITY as usize);
const_assert_eq!(CRANK_CROSS, clob_wire::CRANK_SLOT_CROSS as usize);
/// Accounts a registered resolver takes. The capacity is [`RelayBlockV0`]'s
/// minimum granularity of 8; the book stores whatever list the registering
/// program hands it.
pub const CRANK_RESOLVER_CAPACITY: usize = 8;

pub const ZERO_ADDRESS: Address = Address::new_from_array([0u8; 32]);

/// [`ClobHeaderV0::reservation_grace_slots`] a fresh market starts with.
///
/// Two seconds of slots. It has to cover the transaction that resolves a
/// cross — the crank is woken by a relay condition the moment the cross
/// appears — and it is the longest the claimed depth stays out of the
/// matchable set when that crank never lands.
pub const DEFAULT_RESERVATION_GRACE_SLOTS: u16 = 32;

/// Ceiling on [`ClobHeaderV0::reservation_grace_slots`], enforced by
/// `update_market_v0`.
///
/// The window only has to cover the crank transaction that resolves the
/// cross. A transaction is invalid more than 150 slots after its blockhash. A
/// crank that misses that window must be sent again with a fresh blockhash,
/// so a wider grace does not help it land. It only holds the claimed depth
/// out of the matchable set for longer.
pub const RESERVATION_GRACE_SLOTS_CEILING: u16 = 150;

/// Response region size. Responses live in the header (quoter interface:
/// return data carries only a [`ResponsePointerV0`]), so payload size is not
/// bound by the 1024-byte return-data cap.
pub const RESPONSE_BUFFER_BYTES: usize = {
    let widest = 4 * RESPONSE_LEN_BYTES
        + EXECUTE_FILLS_CEILING as usize * (CHANGE_BYTES + COMPLETED_BYTES)
        + CANCELLED_BYTES
        + PARTIAL_BYTES;
    // The region must start on an 8-byte step for its records to be read in
    // place, and it sits at the end of the header, so its size carries that.
    widest.next_multiple_of(quoter_spec::LEN_BYTES)
};

// Widths of the response wire types, each taken from `quoter-spec`'s
// declaration of the record it measures rather than restated here. The
// ceilings below are arithmetic over them, so a field added to a record moves
// them, and `tests::response::wire_widths_match_the_response_types` pins every
// one against wincode's encoding of that record.

/// Byte width of a borsh-framed sequence count (a `Vec`'s length prefix).
pub const COUNT_BYTES: usize = core::mem::size_of::<u32>();

/// Width of a sequence length in the *response region*, which is wincode's
/// framing rather than borsh's. Distinct from [`COUNT_BYTES`]: that one is the
/// borsh-framed count the event records carry, and widening it here silently
/// changed an emitted event before the two were separated.
pub const RESPONSE_LEN_BYTES: usize = quoter_spec::LEN_BYTES;

/// Width of a [`UserRefV0`]: 32-byte authority + u16 sub-account.
pub const USER_REF_BYTES: usize = quoter_spec::UserRefV0::SIZE;

/// Encoded width of a [`PriceLevel`].
pub const PRICE_LEVEL_BYTES: usize = quoter_spec::PRICE_LEVEL_BYTES;

/// Width of an order id in an event payload. Events carry borsh framing, not
/// the response wire's — [`crate::emit`] sizes its buffers from this.
pub const ORDER_ID_BYTES: usize = core::mem::size_of::<u64>();

/// Width of the placing caller's own order id, which is what the bulk id
/// lists in the events carry.
pub const CLIENT_ORDER_ID_BYTES: usize = core::mem::size_of::<u32>();

/// Width of a [`UserBalanceChangeV0`]. One fixed stride: the orders a change
/// consumed ride their own section, so a change cannot grow.
pub const CHANGE_BYTES: usize = quoter_spec::CHANGE_BYTES;

/// Width of a [`CancelledRemainderV0`].
pub const CANCELLED_BYTES: usize = quoter_spec::CANCELLED_BYTES;

/// Width of a [`CompletedOrderV0`].
pub const COMPLETED_BYTES: usize = quoter_spec::COMPLETED_BYTES;

/// Width of a [`PartiallyFilledOrderV0`]. One per execute at most, so it is a
/// flat addition to the region rather than a per-fill stride.
pub const PARTIAL_BYTES: usize = quoter_spec::PARTIAL_BYTES;

/// Width of a [`RemovedOrderV0`] — the return data of
/// `cancel_order_v0`/`evict_worst_v0`/`remove_expired_v0`. Not used to size
/// anything here (anchor serializes the value), but velocity reads those bytes
/// by offset, so the width is pinned rather than assumed.
pub const REMOVED_ORDER_BYTES: usize = USER_REF_BYTES
    + 3 * core::mem::size_of::<u64>()
    + core::mem::size_of::<u32>()
    // side + taker_origin + reduce_only.
    + 3
    + core::mem::size_of::<i64>();

// Hard ceilings on the per-market response/batch config — bound by the
// response region and the 32KB program heap, which don't vary per market.
// The per-market operating points live on the header. Partial execution is
// the interface contract; the router sees smaller balance changes.

/// Trailing bytes of a [`QuoteResponseV0`]: the withheld report, which is one
/// [`PriceLevel`] written after the ladder.
pub const WITHHELD_REPORT_BYTES: usize = PRICE_LEVEL_BYTES;

/// Ceiling on `max_quote_levels`: a [`QuoteResponseV0`] is a count, that many
/// [`PriceLevel`]s, then the withheld report.
pub const QUOTE_LEVELS_CEILING: u16 =
    ((RESPONSE_BUFFER_BYTES - RESPONSE_LEN_BYTES - WITHHELD_REPORT_BYTES) / PRICE_LEVEL_BYTES)
        as u16;

/// Width of an [`L3RowV0`].
pub const L3_ROW_BYTES: usize = quoter_spec::L3_ROW_BYTES;

/// Rows one `quote_l3_v0` may report: a count, that many rows, then the
/// one-byte marker saying whether depth remains.
///
/// Derived from the region rather than chosen, and deliberately not allowed
/// to widen it: the market account rides every CPI and the runtime charges
/// compute per byte of it, so paying for a bigger region on every fill to
/// describe a deeper book once is the wrong trade. A caller wanting more of
/// the book asks again from where this stopped.
pub const L3_ROWS_CEILING: u16 =
    ((RESPONSE_BUFFER_BYTES - RESPONSE_LEN_BYTES - 1) / L3_ROW_BYTES) as u16;

/// Orders one `execute_v0` may consume.
///
/// Held where the response region stays near its original size rather than
/// raised to fit the widest response: the market account rides every CPI, and
/// the runtime charges compute per byte of it, so ~1 KB of extra region put
/// `crank_cross_match` — which CPIs this book twice — over the 200k
/// per-instruction budget. Trading 15 fills off one execute is cheaper than
/// paying for the region on every crank.
pub const EXECUTE_FILLS_CEILING: u16 = 113;

/// Hard cap on the orders one `cancel_all_v0` removes. Bounds three things at
/// once: the removal work in a single call, the id list the cancel record logs
/// ([`crate::emit::CANCEL_ALL_RECORD_LOG_BYTES`]), and how far a maker's aggregate unwind
/// can drift from one instruction. A maker holding more than this cancels in
/// repeated calls — the instruction reports whether it finished (see
/// [`CancelAllOutcome::exhaustive`]).
pub const CANCEL_ALL_ORDERS_CEILING: u16 = 128;

/// Ceiling on `max_execute_users`: how many balance changes fit the region
/// once the other three sections have taken their worst case.
///
/// Every completed order belongs to a fill, so the completed section is
/// bounded by `EXECUTE_FILLS_CEILING` rather than by anything about users, and
/// reserving it here — rather than dividing the region by the change width
/// alone — is what makes a market configured *at* this ceiling unable to
/// overrun the region, whatever the book holds.
pub const EXECUTE_USERS_CEILING: u16 = ((RESPONSE_BUFFER_BYTES
    - 2 * RESPONSE_LEN_BYTES
    - CANCELLED_BYTES
    - EXECUTE_FILLS_CEILING as usize * COMPLETED_BYTES)
    / CHANGE_BYTES) as u16;

// The widest response either instruction can produce at the ceilings fits the
// region, so `ResponseTooLarge` is unreachable for a market whose config the
// init/update checks accepted.
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

/// What this program reads into the wire's side beyond its shape. An inherent
/// impl is not available on a foreign type, and a trait keeps every call site
/// reading as it did.
pub trait ClobSideExt {
    fn to_u8(self) -> u8;
    fn is_worse_price(self, resting: u64, candidate: u64) -> bool;
    fn side_bit(self) -> u8;
    fn opposite(self) -> Side;
    fn is_crossed_by(self, price: u64, opposite: u64) -> bool;
}

/// The same for the direction.
pub trait ClobDirectionExt {
    fn book_side(self) -> Side;
    fn to_u8(self) -> u8;
}

impl ClobDirectionExt for Direction {
    /// The book side this taker direction consumes.
    fn book_side(self) -> Side {
        self.side()
    }

    fn to_u8(self) -> u8 {
        match self {
            Direction::Long => 0,
            Direction::Short => 1,
        }
    }
}

impl ClobSideExt for Side {
    fn to_u8(self) -> u8 {
        match self {
            Side::Bid => 0,
            Side::Ask => 1,
        }
    }

    /// Whether `resting` is a worse price for this side's makers than
    /// `candidate` — i.e. the point a new order at `candidate` cuts in front
    /// of. Bids rank high-to-low, asks low-to-high.
    fn is_worse_price(self, resting: u64, candidate: u64) -> bool {
        match self {
            Side::Bid => resting < candidate,
            Side::Ask => resting > candidate,
        }
    }

    /// The node bit that marks membership of this side (bids carry none —
    /// `OrderBitFlag::Ask` clear means bid).
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
    /// on the opposite side at `opposite`: a bid is crossed by an ask at or
    /// below it, an ask by a bid at or above it.
    fn is_crossed_by(self, price: u64, opposite: u64) -> bool {
        match self {
            Side::Bid => opposite <= price,
            Side::Ask => opposite >= price,
        }
    }
}

/// Flags on a node's `bit_flags` byte, declared with the node itself.
/// The one shape every read-only answer reports an order in.
pub use clob_wire::{OrderRulesV0, OrderViewV0};

/// Describe one order the way every read-only answer does.
///
/// The node → view mapping lives here rather than on the node itself: the
/// node is layout, and this is what the book chooses to say about it. One
/// function so `next_removal_v0`, `next_cross_v0` and `orders_v0` cannot
/// disagree about what an order looks like.
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
    /// Only signer allowed to place/cancel/execute (velocity's quoter CPI
    /// signer PDA; velocity verifies `User` authority and flow-attestation
    /// policy — including zero-delay activation — before CPI'ing here).
    pub place_authority: Address,
    /// Prices must be a multiple of this (PRICE_PRECISION). Enforced at
    /// placement, not baked into the stored representation, so it can be
    /// changed without repricing the resting book.
    pub order_tick_size: u64,
    /// Sizes must be a multiple of this (base precision).
    pub order_step_size: u64,
    /// Floor on order size so every resting order has real capital at risk.
    pub min_order_size: u64,
    /// Floor on the size of an order that may end a walk.
    ///
    /// An order whose owner the caller did not carry ends the walk once it is
    /// past `unknown_user_grace_slots`, and the depth behind it goes untraded.
    /// That is what stops a caller filling around the maker who would have won.
    /// It is also a blocking right, and a right that costs only
    /// `min_order_size` can be bought in bulk: a caller can carry at most 48
    /// users, so 49 orders at the top of book on 49 fresh sub-accounts make the
    /// depth behind them unreachable for everyone, for rent.
    ///
    /// An order below this floor is stepped over instead, at any age, exactly
    /// as a too-fresh order is. So the right now costs 49 times this size,
    /// posted at the top of book and exposed to being filled, which is market
    /// making rather than rent.
    ///
    /// What a maker gives up below the floor is stated and bounded: price
    /// priority against a caller that did not carry it. At or above the floor
    /// that priority is guaranteed; below it, a maker relies on being carried,
    /// and a maker that is carried fills normally either way.
    ///
    /// Zero disables the floor, which is what every market reads out of
    /// reserved bytes, so the behaviour is unchanged until an admin sets it.
    /// There is no upper bound: raising it is the response to someone buying
    /// blocking rights in bulk. Set it above the real book and no order can
    /// end a walk, which hands every caller the freedom to fill around any
    /// maker it left out.
    pub blocking_min_size: u64,
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
    /// Fills race the tx's fixed account set: quote and execute take the set
    /// of users the caller can settle, and an order whose owner is absent is
    /// skipped while its age is at most this many slots — the caller cannot
    /// be expected to have heard of it yet — and ends the walk once its age
    /// passes that. The window is therefore this many slots plus the slot the
    /// order became matchable in. Age runs from `activation_slot`, the slot
    /// the order first became visible to any reader of this book, not from
    /// when it was placed.
    ///
    /// So this is how far the caller's account set is allowed to lag the
    /// book. At or below that age a new maker costs the caller nothing; past
    /// it, a maker the caller did not bring is where its fill stops, and the
    /// book reports the depth behind as withheld.
    ///
    /// Sizing it is a question about how the callers of this market build
    /// their account sets. A set assembled from a live subscription can lag
    /// by a slot or two; one assembled from an address lookup table cannot
    /// name a maker until the table has been extended and that extension has
    /// landed, which is longer. Too small and every fresh quote stops fills
    /// at the top of book; too large and a caller can leave out a maker it
    /// did know about, and the depth behind that maker goes untraded rather
    /// than to a worse price.
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
    /// The earliest `max_ts` any live order carries, or [`i64::MAX`] when no
    /// live order expires.
    ///
    /// The book keeps it so a caller does not have to walk the arena to learn
    /// when its next expiry crank is due. Maintained conservatively: a
    /// placement folds its own expiry in, and a removal recomputes only when
    /// it took the order that held the minimum. It may therefore be *earlier*
    /// than the truth for as long as it takes the next removal to notice,
    /// which costs a caller a simulation that finds nothing. It is never
    /// later, which would be work nobody is woken for.
    pub next_expiry_ts: i64,
    /// The earliest `activation_slot` any live order carries that has not yet
    /// arrived, or [`u64::MAX`] when none is pending.
    ///
    /// Unlike the expiry this one goes stale on its own: a slot the chain
    /// passes turns a pending activation into an arrived one with nothing
    /// writing to the book. Every mutation therefore recomputes it if the
    /// stored slot is no longer in the future, and a book nothing writes to
    /// is covered by its caller's fallback poll rather than by this field.
    pub next_activation_slot: u64,
    /// The relay conditions that wake a turner for this book's own work.
    ///
    /// Expiry, activation, a side at its soft cap and a crossed book are all
    /// facts about this account, so the wakes that watch for them live on it
    /// and the book maintains them as it places and removes. Nothing else has
    /// to be passed a second account to keep them fresh, and a hint cannot go
    /// stale because a caller omitted one.
    ///
    /// What the book does *not* decide is who resolves them: each condition
    /// carries a [`relay_spec::CrankSpecV0`] naming the resolver program, its
    /// discriminator and the payment floor, written by
    /// `set_crank_conditions_v0`. Removing an order has consequences the book
    /// does not hold — a maker's margin, a reward, a trigger slot — so the
    /// program that owns the flow says what runs.
    pub crank: RelayBlockV0<CRANK_CONDITIONS, CRANK_RESOLVER_CAPACITY>,
    /// Growth room: reserved bytes so a later field (a fee destination, a
    /// second authority, a paused-operations bitmap) can be added without
    /// moving `response`, changing the account size, or migrating every live
    /// market. Must stay zero until claimed.
    pub padding: [u8; 104],
    /// Oldest taker-origin order on each side, indexed by [`Side`] (bid 0,
    /// ask 1). [`NIL`] when the side holds none.
    ///
    /// A taker-origin order is a migrated taker remainder, and it claims the
    /// depth it crosses on the other side (see `crate::book`'s reservation).
    /// Every read of a side therefore has to enumerate the remainders resting
    /// on the opposite one, so they are threaded on their own list rather than
    /// found by walking a price-sorted side.
    ///
    /// The list is in rest order, which costs nothing to keep: `next_order_id`
    /// only increases, so the newest taker-origin order always has the highest
    /// id and appending it at the tail is both O(1) and already sorted.
    pub taker_origin_head: [u32; 2],
    /// Newest taker-origin order on each side, so an append is O(1).
    pub taker_origin_tail: [u32; 2],
    /// Taker-origin orders on each side. Bounds the claimant hops one read of
    /// a side may take, so a corrupt list cannot spin.
    pub taker_origin_count: [u16; 2],
    /// Slots past its activation slot for which a taker remainder's claim on
    /// the depth it crosses is still honoured.
    ///
    /// The claim hides that depth from every caller but the crank that owes
    /// the taker its improvement, so a crank that never lands would hold the
    /// top of book indefinitely. Past this window the claim stops being
    /// honoured and the depth is ordinary again. Zero means a claim ends the
    /// slot the remainder activates.
    ///
    /// `update_market_v0` writes it, bounded by
    /// [`RESERVATION_GRACE_SLOTS_CEILING`]. A fresh market starts at
    /// [`DEFAULT_RESERVATION_GRACE_SLOTS`].
    pub reservation_grace_slots: u16,
    /// Keeps `response` on the 8-byte step its records are cast at.
    pub padding1: [u8; 2],
    /// Scratch region `quote_v0`/`execute_v0` stream their response into;
    /// return data carries a [`ResponsePointerV0`] locating it. Last field, so
    /// [`RESPONSE_OFFSET`] is the header size minus its length.
    ///
    /// Its size carries the region's 8-byte start (see
    /// [`RESPONSE_BUFFER_BYTES`]), which is what lets both programs cast the
    /// records in place instead of copying them field by field.
    pub response: [u8; RESPONSE_BUFFER_BYTES],
}

// Both programs cast the response records onto these bytes, so the region has
// to start on the step they are read at. Solana gives account data an 8-byte
// start, and every record's alignment divides 8, so this offset is the whole
// condition.
const_assert_eq!(RESPONSE_OFFSET % RESPONSE_LEN_BYTES, 0);

/// The market account: header + order-node tail, capacity from data length.
pub type ClobMarketV0 = Slab<ClobHeaderV0, OrderNodeV0>;

/// Account-data offset of the header's `response` region.
/// Account-data offset of the relay block — what a `WatchV0` registers at.
/// Reported by `set_crank_conditions_v0` so a registrant learns it by asking
/// rather than by knowing this account's layout.
pub const CRANK_BLOCK_OFFSET: usize = relay_spec::block_offset!(ClobHeaderV0, crank);

const_assert_eq!(CRANK_BLOCK_OFFSET % 8, 0);

/// The region of this account that changes whenever either side's best moves:
/// `best_bid` and `best_ask`, adjacent, as one watched range.
///
/// A crossing order is by definition a new best, so a relay watch here
/// catches every cross the moment it appears. Reported by
/// `set_crank_conditions_v0` so a caller registering one never has to know
/// where the heads sit.
pub const TOP_OF_BOOK_OFFSET: usize = 8 + core::mem::offset_of!(ClobHeaderV0, best_bid);
pub const TOP_OF_BOOK_BYTES: usize = 2 * core::mem::size_of::<u32>();

/// The region that changes whenever a side's order count moves: `bid_count`
/// and `ask_count`, adjacent, as one watched range. What the book's own
/// capacity condition watches.
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

pub const RESPONSE_OFFSET: usize = 8 + core::mem::size_of::<ClobHeaderV0>() - RESPONSE_BUFFER_BYTES;

/// The pointer `quote_v0`/`execute_v0` return for a response of `len` bytes.
pub fn response_pointer(len: usize) -> ResponsePointerV0 {
    ResponsePointerV0 {
        offset: RESPONSE_OFFSET as u32,
        len: len as u32,
    }
}

/// Account-data offset of the order-node tail: `[disc][H][len: u32]` padded
/// to the node's 8-byte alignment.
pub const ORDERS_OFFSET: usize = (8 + core::mem::size_of::<ClobHeaderV0>() + 4).next_multiple_of(8);

// An off-chain reader of this account has the arena's offset as a number, and
// a number cannot follow a header that moves. This is the whole coupling: the
// header stays free to change as long as it does not change size, and if it
// ever does, this build fails rather than the reader.
const_assert_eq!(ORDERS_OFFSET, clob_state::ORDERS_OFFSET);

/// The order-node layout, declared in `clob-state` because an off-chain
/// indexer decodes the same bytes — see that crate for why it is the one part
/// of this account a reader outside the program is allowed to know. Nothing on
/// chain reads it but this program.
pub use clob_state::{live_orders, OrderBitFlag, OrderNodeV0, NIL, NODE_BYTES};
/// Order handle, declared by `clob-wire` — the crate that owns every shape
/// on the instruction surface, so the bytes this program reads and the bytes
/// its caller writes come from one declaration.
pub use clob_wire::ClobOrderRefV0 as OrderRefV0;
/// A velocity user in its *derivable* form: authority wallet + sub-account
/// index. Both the `User` PDA (`["user", authority, sub_account_id]`) and
/// the `UserStats` PDA (`["user_stats", authority]`) derive from it, which
/// is why the book stores this rather than the `User` account key — an
/// off-chain reader (a relay resolver staging a crank) can reach every
/// user-derived account from the node alone, where a stored `User` key is a
/// dead end (its authority lives inside account data the reader can't
/// load).
pub use quoter_spec::UserRefV0;
/// The request half of this wire, declared in `quoter-spec` alongside the
/// responses — one declaration both programs read, rather than a shape each
/// restates and a width each asserts.
///
/// `USER_SET_CAPACITY` is derived from the account-lock budget of the
/// transaction that forwards the set: 64 locks, minus the 15 a router fill
/// spends before its first maker, minus one for the `UserStats` those makers
/// share in the best case.
pub use quoter_spec::{
    user_set_within_capacity, UserCapV0, UserCapsV0, BASE_PRECISION, USER_CAPS_BYTES,
    USER_CAPS_CAPACITY, USER_EXCLUSION_BITMAP_BYTES, USER_SET_CAPACITY, USER_SET_MAX_BYTES,
};

/// Declared by `quoter-spec`; the alias keeps this program's name for it.
pub type PriceLevel = quoter_spec::PriceLevelV0;

/// Which sides a `cancel_all_v0` withdraws. Declared by `quoter-spec`; this
/// program's reading of them is [`CancelSidesExt`].
pub use quoter_spec::CancelSidesV0;
/// Where in the market account the response was written. Declared by
/// `quoter-spec`, which owns every shape on this wire.
pub use quoter_spec::ResponsePointerV0;
/// One user's share of an executed fill. Mirrors velocity's quoter-interface
/// `UserBalanceChangeV0`.
///
/// `execute` writes this encoding into the response region field by field
/// (see [`crate::book`]) rather than serializing this struct — the type
/// remains the schema of record for that layout, and the response unit
/// tests pin the two against each other.
pub use quoter_spec::UserBalanceChangeV0;
pub use quoter_spec::{ExecuteResponseV0, L3ArgsV0, L3ResponseV0, L3RowV0, QuoteResponseV0};

/// What the wire's named sides mean to a book: the lists to walk.
pub trait CancelSidesExt {
    fn sides(self) -> &'static [Side];
    fn includes(self, side: Side) -> bool;
}

impl CancelSidesExt for CancelSidesV0 {
    /// The sides to walk, in book order.
    fn sides(self) -> &'static [Side] {
        match self {
            CancelSidesV0::Bids => &[Side::Bid],
            CancelSidesV0::Asks => &[Side::Ask],
            CancelSidesV0::Both => &[Side::Bid, Side::Ask],
        }
    }

    fn includes(self, side: Side) -> bool {
        matches!(
            (self, side),
            (CancelSidesV0::Both, _)
                | (CancelSidesV0::Bids, Side::Bid)
                | (CancelSidesV0::Asks, Side::Ask)
        )
    }
}

/// What a `cancel_all_v0` withdrew, aggregated per side.
///
/// Per-side totals rather than a list of removals: velocity unwinds
/// `open_bids`/`open_asks` by a summed base amount and the open-order counts by
/// a count, so the whole sweep costs it the same two calls one cancel does.
/// The per-order detail an indexer needs rides the cancel record's id list
/// instead of return data.
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

/// Wire form of [`CancelAllOutcome`] — return data of `cancel_all_v0`, so
/// the caller can unwind the maker's aggregates in one pass per side.
/// Declared by `clob-wire`.
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

/// What `execute` hands back: where the wire response was written, plus the
/// per-fill detail the execute event carries (the response merges fills by
/// user, so the event's order-level view can't be recovered from it).
pub struct ExecuteOutcome {
    pub response: ResponsePointerV0,
    pub fills: Vec<crate::events::FillSlimV0>,
    /// Order culled because its post-fill remainder fell below
    /// `min_order_size`. At most one per execute — a partial fill only
    /// happens when the taker's size runs out, which ends the walk.
    pub cancelled_client_order_id: Option<u32>,
}

/// Most orders one `fill_v0` may report. A transaction that resolves several
/// remainders pays for one call; the ceiling is what the execute record's log
/// buffer is sized against, since every order in a batch can leave a
/// sub-minimum remainder to cull.
pub const FILL_BATCH_CEILING: usize = 8;

/// Wire form of a removed order — return data of cancel/evict/expire.
/// Declared by `clob-wire`.
///
/// Its `taker_origin` flag is the only place this program reports that an
/// order was a migrated taker remainder. A taker-origin cross cannot go
/// through an ordinary `execute_v0` at all: `crate::book`'s reservation
/// withholds both the remainder and the depth it crosses. So a caller resolves
/// one by taking the counterparty's side with `execute_v0` and
/// `consume_reservation`, then lifting the taker-origin order off the book
/// with `cancel_order_v0`, which returns this. Without the flag the caller
/// cannot tell which of the two removed orders was demanding liquidity, and so
/// cannot know which side's price the match settles at.
pub use clob_wire::RemovedOrderV0;
/// What one order in a `fill_v0` came to. Declared by `clob-wire`.
pub use clob_wire::{FillArgsV0, FillOutcomeV0, FillRequestV0, FilledOrderV0 as FilledOrder};
/// A sub-`min_order_size` remainder culled during execute, on the wire so
/// velocity decrements the maker's aggregates (the maker was just filled,
/// so their `User` is always in the loaded set).
pub use quoter_spec::CancelledRemainderV0;
pub use quoter_spec::CompletedOrderV0;
/// The one order a fill left resting smaller than it found it — the per-order
/// half of a fill a merged balance change cannot report. Declared by
/// `quoter-spec`.
pub use quoter_spec::PartiallyFilledOrderV0;

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
    /// Marks the order [`OrderBitFlag::ReduceOnly`]: a fill against it is
    /// clamped to the owner's `base_cover` cap at match time.
    pub reduce_only: bool,
}

/// Per-market configuration, set at init (also the init wire args).
/// `base_precision` and `market_index` are immutable afterwards; the rest
/// are updatable via `update_market_v0`.
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
