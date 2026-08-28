//! The CLOB's instruction arguments and return data.
//!
//! # One declaration, two programs
//!
//! Velocity reaches the book by CPI — placing, cancelling, evicting,
//! reclaiming an expired order — and every one of those calls is bytes that
//! velocity writes and the book reads. Declaring the shape twice leaves
//! nothing pinning the declarations against each other: a field reordered on
//! one side gives two self-consistent programs that disagree about the bytes
//! between them, and the disagreement lands on a placement, where a misread
//! `base_asset_amount` rests the wrong size against a real user's margin.
//!
//! That is the same argument [`quoter_spec`] makes for the quoter interface,
//! and this crate is its counterpart for the surface a book has *beyond* that
//! interface. The split matters: `quoter-spec` is what every PropAMM
//! implements, and these instructions are not part of it.
//!
//! # What is not here
//!
//! The book's *account layout* — its header offsets and node arena. A caller
//! that needs to know where a field sits inside the market account is reading
//! the book's memory rather than calling it, which is what this crate exists
//! to replace.
//!
//! # Serialization
//!
//! Per-consumer, as in `quoter-spec`: velocity encodes with anchor's borsh and
//! needs the IDL plumbing, while the book writes the bytes itself and carries
//! no borsh crate (its binary size and CU budget are why). The two
//! encodings are byte-compatible — wincode's configuration here is anchor's
//! `BORSH_CONFIG` — so one declaration serves both.

pub use quoter_spec::{CancelSidesV0, SideV0, UserRefV0};

/// Order handle: an O(1) node hint verified against the order id, so a stale
/// hint (node freed or reused) fails closed rather than acting on whichever
/// order took the slot.
///
/// The `Clob` prefix is load-bearing and stutters here on purpose. This is the
/// one type on this wire that reaches velocity's *instruction* arguments, so
/// it is the one that lands in velocity's IDL — beside `Order`, `OrderType`
/// and `OrderParams`, where a bare `OrderRefV0` names no program. Anchor takes
/// the declared name, not the alias, so renaming it here renames it there.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct ClobOrderRefV0 {
    pub node_index: u32,
    pub order_id: u64,
}

/// `place_order_v0` arguments.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct PlaceOrderArgsV0 {
    pub side: SideV0,
    pub price: u64,
    pub base_asset_amount: u64,
    /// `None` = the market's default activation delay. A value must be at or
    /// under the market's maximum. Zero is allowed — the caller owns
    /// attestation policy, the book only clamps.
    pub activation_delay_slots: Option<u32>,
    /// Zero = good until cancelled.
    pub max_ts: i64,
    /// The user the order settles against, in derivable form. The book trusts
    /// its `place_authority` for identity; the caller verified control before
    /// the CPI.
    pub user: UserRefV0,
    /// The order is an unfilled taker remainder the caller migrated onto the
    /// book, not a quote its owner chose to post. Only the caller can know
    /// that, so it is an argument rather than something the book infers. It
    /// changes two things: the order cannot be taken while a live
    /// counterparty crosses it, and a cross involving it settles at the
    /// counterparty's price.
    pub taker_origin: bool,
    /// The caller's own id for this order. The book stores it and reports it
    /// back on every answer that names the order, so the caller never holds a
    /// map from the book's ids to its own. Opaque to the book: it neither
    /// orders nor identifies an order here. Zero means the caller keeps no id.
    pub client_order_id: u32,
    /// Refuse the placement when the order would cross the opposite best
    /// price, instead of resting it crossed.
    ///
    /// A crossed order still fills at its own price — the cross crank matches
    /// it as a maker — so this is not about the fee it pays. It is about the
    /// order resting at all: a maker that quotes through the other side has
    /// mispriced, and would rather place nothing than hold a position it did
    /// not intend to take.
    pub reject_if_crossed: bool,
}

/// `cancel_order_v0` arguments.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CancelOrderArgsV0 {
    pub order_ref: ClobOrderRefV0,
    /// Owner of the order, verified against the node.
    pub user: UserRefV0,
    /// Remove the order even when it is a taker-origin remainder that has not
    /// reached its activation slot. Liquidation sets this flag. Every other
    /// caller leaves it clear, which holds a taker to the auction window its
    /// own order asked for.
    pub force: bool,
}

/// One order the caller filled elsewhere, and by how much.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct FillRequestV0 {
    pub order_ref: ClobOrderRefV0,
    pub base_asset_amount: u64,
}

/// `fill_v0` arguments.
///
/// A taker remainder resting here can be the *aggressor* of a match, and the
/// sources it aggresses against are not all on this book — a quoter or the
/// vAMM may be the better price, and this program cannot see either. So
/// velocity does that matching and reports the result back: these orders
/// filled this much, take it off them.
///
/// A list rather than one, because a transaction that resolves several
/// remainders should pay for one call.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct FillArgsV0 {
    pub fills: Vec<FillRequestV0>,
}

/// What one order in a [`FillArgsV0`] came to.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct FilledOrderV0 {
    pub order_id: u64,
    /// The placing caller's own id, so a caller joins this to its own order
    /// without holding a map between the two id spaces.
    pub client_order_id: u32,
    pub base_asset_amount: u64,
    /// Size dropped because what was left fell under `min_order_size`. The
    /// caller unwinds this from the owner's reservation; the book will not
    /// hold it.
    pub culled_base_asset_amount: u64,
    /// The order left the book — filled out, or culled by the line above.
    pub removed: bool,
}

/// Return data of `fill_v0`.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct FillOutcomeV0 {
    pub filled: Vec<FilledOrderV0>,
}

/// `evict_worst_v0` arguments.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct EvictWorstArgsV0 {
    pub side: SideV0,
}

/// `remove_expired_v0` arguments.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct RemoveExpiredArgsV0 {
    pub order_ref: ClobOrderRefV0,
}

/// `cancel_all_v0` arguments.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CancelAllArgsV0 {
    /// Whose orders to withdraw, verified against each node.
    pub user: UserRefV0,
    pub sides: CancelSidesV0,
    /// Sweep taker-origin remainders that have not reached their activation
    /// slot as well. Liquidation sets this flag. Every other caller leaves it
    /// clear, and the sweep then passes such an order over and reports the
    /// call as not exhaustive.
    pub force: bool,
}

/// Return data of `cancel_order_v0`, `evict_worst_v0` and
/// `remove_expired_v0`: the order that left the book.
///
/// `side` is what tells the caller whether the remaining size unwinds its
/// bid-side or ask-side reservation.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct RemovedOrderV0 {
    pub user: UserRefV0,
    pub order_id: u64,
    /// The caller's own id for this order, as supplied at placement.
    pub client_order_id: u32,
    pub price: u64,
    pub base_asset_amount: u64,
    pub side: SideV0,
    /// The order was taker-origin.
    ///
    /// The only place the book reports the flag, and what tells a caller
    /// which side of a cross it is resolving was demanding liquidity — hence
    /// which side's price the match settles at.
    pub taker_origin: bool,
    /// The expiry the order carried, zero for good-till-cancelled.
    ///
    /// Reported so a caller that removes an order to put an equivalent one
    /// back — a modify — can carry the expiry across without reading the
    /// book. The removal is the only moment the value is still knowable, and
    /// reading it off the node beforehand means knowing where a node keeps
    /// it.
    pub max_ts: i64,
}

/// Return data of `cancel_all_v0`: per-side totals rather than a list of
/// removals, which is the shape open-order aggregates consume — one unwind
/// per side and one count, however many orders the sweep took.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CancelAllOutcomeV0 {
    pub user: UserRefV0,
    pub bid_base_asset_amount: u64,
    pub ask_base_asset_amount: u64,
    pub bid_orders: u32,
    pub ask_orders: u32,
    /// The sweep took every order it was asked for. False means the user still
    /// has resting orders, for one of two reasons: the book stopped at its
    /// per-call cap, or it passed over a taker-origin remainder still inside
    /// its activation window. Repeating the call clears the first. The second
    /// clears itself once the order activates.
    pub exhaustive: bool,
}

/// One resolver registration: which program answers a condition, with which
/// instruction, and what it must pay the keeper that lands the answer.
///
/// The same three fields relay's own `CrankSpecV0` carries. Restated here
/// because they travel as instruction arguments, and this wire is the one
/// declaration velocity and the book share.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CrankResolverV0 {
    pub program: [u8; 32],
    pub disc: [u8; 8],
    pub min_payment: u64,
}

/// One account a registered resolver takes, in the order its instruction
/// expects them.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CrankAccountV0 {
    pub address: [u8; 32],
    /// 0 = readonly, nonzero = writable.
    pub writable: u8,
}

/// `set_crank_conditions_v0` arguments: who resolves each of the book's own
/// conditions, and the accounts they all take.
///
/// The book owns the *wakes* — when an order expires, when one activates,
/// when a side reaches its cap, when the two sides cross are all facts about
/// its own account, and it keeps them current as it places and removes. It
/// owns none of the *answers*: removing an order releases a maker's margin
/// reservation, pays a reward and frees a trigger slot, none of which the
/// book holds. So the program that owns the flow registers what runs, and
/// each condition wakes into that program's resolver.
///
/// One account list serves every condition — the resolvers are that one
/// program's, and they read the same state.
///
/// Re-running replaces the registration in place, which is how a re-priced
/// crank or a rotated resolver lands. Zeroing a resolver's `program`
/// deactivates its condition.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CrankConditionsArgsV0 {
    pub expiry: CrankResolverV0,
    pub activation: CrankResolverV0,
    pub capacity: CrankResolverV0,
    pub cross: CrankResolverV0,
    pub accounts: Vec<CrankAccountV0>,
}

/// Which slot of the book's condition block each of
/// [`CrankConditionsArgsV0`]'s resolvers is written to.
///
/// Relay tells a resolver which condition fired by slot index, so a program
/// registering one resolver for several of them has to know the mapping from
/// the argument names above to those indices. That makes it part of this wire
/// rather than the book's private numbering, and the book asserts its own
/// slots against these.
pub const CRANK_SLOT_EXPIRY: u8 = 0;
pub const CRANK_SLOT_ACTIVATION: u8 = 1;
pub const CRANK_SLOT_CAPACITY: u8 = 2;
pub const CRANK_SLOT_CROSS: u8 = 3;

/// Return data of `set_crank_conditions_v0`: the two regions of the market
/// account a registrant has to point relay at.
///
/// A watch registration names an account, an offset and a length. Reporting
/// them here is what lets a registrant register one without knowing this
/// account's layout — the same reason every other answer on this wire exists.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CrankBlockV0 {
    /// Where the condition block starts.
    pub block_offset: u32,
    /// The region that changes whenever either side's best moves.
    ///
    /// A crossing order is by definition a new best, so a watch here catches
    /// every cross the moment it appears. The book registers its own on this
    /// region; a caller crossing something *else* against this book — whose
    /// own repricing writes nothing here — registers a second one.
    pub top_of_book_offset: u32,
    pub top_of_book_len: u32,
}

/// Return data of `order_rules_v0`: what the book requires of an order before
/// it will hold one.
///
/// A caller that builds orders has to satisfy these, and finding out by
/// rejection costs it the transaction. Asking is what replaces reading them
/// out of the market account's header.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct OrderRulesV0 {
    /// Floor on a resting order's size. A remainder below it cannot rest —
    /// the book culls one on its own fills — so a caller re-placing a
    /// partially-filled remainder drops it instead of offering the book a
    /// placement it will reject.
    pub min_order_size: u64,
    /// Floor on the size of an order that may end a fill walk when its owner
    /// is not in the caller's user set. Below it the order is stepped over at
    /// any age, exactly as a too-fresh one is.
    ///
    /// A maker sizing a quote needs it: this is what it costs to keep price
    /// priority against a caller that leaves the maker out. A reader deciding
    /// which owners to carry does not — `quote_l3_v0` flags the orders that can
    /// end a walk per row, so nothing has to apply this floor itself.
    ///
    /// Zero disables the floor, which is what a market that has never set one
    /// reports.
    pub blocking_min_size: u64,
    /// Slots added to the placement slot to get `activation_slot` when the
    /// caller chooses no delay. A caller that gates on going faster than the
    /// book's own speed bump compares against this.
    pub default_activation_delay_slots: u32,
    /// Upper bound on a caller-chosen activation delay.
    pub max_activation_delay_slots: u32,
    /// The key the book requires to sign a placement, cancel, evict, expire, or
    /// execute — the book's whole trust root. A caller that settles fills for
    /// whoever the book names as a maker (velocity, via `QuoterSubjects::Book`)
    /// pins this to its own signing PDA, so the book only ever acts under a key
    /// the caller controls. Reported here rather than read from the header, so
    /// the caller does not depend on where the book stores it.
    pub place_authority: [u8; 32],
    /// The book's price and size grid. A caller that migrates an order onto the
    /// book pins these to its market's grid at attach, so a remainder aligned
    /// to the market can always rest and is never rejected off-tick or
    /// off-step (which would revert the whole fill that carried it).
    pub tick_size: u64,
    pub step_size: u64,
}

/// One order, as the book describes it to a caller.
///
/// The single shape every read-only answer uses — `next_removal_v0`,
/// `next_cross_v0`, `orders_v0`. They ask different questions and get back the
/// same thing, because what a caller needs about an order does not depend on
/// why it asked: the handle to act on it, whose it is, and the fields it has
/// to price or size a decision with. One declaration is also the only way the
/// three stay in step.
///
/// Every field is stated rather than derived, because deriving any of them
/// means knowing how a node is laid out — which is what calling the book
/// instead of reading it exists to avoid.
///
/// **`order_ref.order_id == 0` means there is no such order.** Ids are handed
/// out from one and never reused, so zero is a value no live order carries.
/// A sentinel rather than an absent value because these answers travel as
/// return data, where a caller reads a fixed width or nothing at all.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct OrderViewV0 {
    pub order_ref: ClobOrderRefV0,
    /// The caller's own id for this order, as supplied at placement.
    pub client_order_id: u32,
    pub user: UserRefV0,
    pub side: SideV0,
    pub price: u64,
    pub base_asset_amount: u64,
    /// Slot the order was placed in. A caller that prices a match against an
    /// auction window needs it.
    pub placed_slot: u64,
    /// The expiry the order carries, zero for good-till-cancelled.
    pub max_ts: i64,
    /// The order is an unfilled taker remainder the caller migrated onto the
    /// book. It demands liquidity rather than offering it, so a cross that
    /// touches one settles at the other side's price.
    pub taker_origin: bool,
}

impl OrderViewV0 {
    /// The answer that means "there is no such order".
    pub const NONE: Self = Self {
        order_ref: ClobOrderRefV0 {
            node_index: 0,
            order_id: 0,
        },
        client_order_id: 0,
        user: UserRefV0::ZERO,
        side: SideV0::Bid,
        price: 0,
        base_asset_amount: 0,
        placed_slot: 0,
        max_ts: 0,
        taker_origin: false,
    };

    /// Whether this names an order at all.
    pub fn found(&self) -> bool {
        self.order_ref.order_id != 0
    }

    /// Did this order rest before `other`?
    ///
    /// Price-time priority between two orders. The book hands out ids from a
    /// counter that only increases and never reuses one, so a lower id was
    /// placed earlier — the id alone is the rest-time order, and no slot has
    /// to travel with it.
    pub fn rested_before(&self, other: &Self) -> bool {
        self.order_ref.order_id < other.order_ref.order_id
    }
}

/// Return data of `next_cross_v0`: the best matchable order on each side.
///
/// "Matchable" is the book's own predicate — open, activated, and unexpired —
/// so a caller comparing the two heads never re-derives it, and never has to
/// know how a node stores an activation slot or an expiry.
///
/// The two heads are what any cross settles between. A caller compares their
/// prices to see whether the book crosses itself at all, and reads
/// [`OrderViewV0::taker_origin`] to see which side came to trade — which is
/// the whole of what it needs to price the match. Depth behind the heads is a
/// separate question, and `quote_l3_v0` already answers it.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct NextCrossV0 {
    pub bid: OrderViewV0,
    pub ask: OrderViewV0,
}

impl NextCrossV0 {
    /// The answer that means "the book has no matchable order on either side".
    pub const NONE: Self = Self {
        bid: OrderViewV0::NONE,
        ask: OrderViewV0::NONE,
    };

    /// Do the two heads cross? False when either side is empty.
    pub fn crosses(&self) -> bool {
        self.bid.found() && self.ask.found() && self.bid.price >= self.ask.price
    }
}

/// Most refs one `orders_v0` call may ask about.
///
/// The answer travels as return data, which is capped at 1 KB. An
/// [`OrderViewV0`] is 80 bytes on this wire, so twelve of them plus the
/// sequence count is what fits. A caller with more refs than this asks more
/// than once.
pub const ORDER_VIEW_CEILING: usize = 12;

/// `orders_v0` arguments: which orders to describe.
///
/// A caller holding refs — from its own records, or from a client that read
/// the book off chain — cannot tell which of them still name a live order, or
/// what those orders hold, without the book's memory. Asking is what replaces
/// reading the arena for it.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct OrdersArgsV0 {
    /// At most [`ORDER_VIEW_CEILING`] refs.
    pub refs: Vec<ClobOrderRefV0>,
}

/// Return data of `orders_v0`: one [`OrderViewV0`] per requested ref, in the
/// order they were asked for.
///
/// A ref that no longer names a live order comes back as [`OrderViewV0::NONE`]
/// rather than being dropped, so a caller reads the answers straight against
/// its own list. That is the expected outcome of a race with a fill or a
/// crank, not an error.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct OrdersV0 {
    pub orders: Vec<OrderViewV0>,
}

/// Which of the book's own removal cranks a caller is asking about.
///
/// Both are the book's business rather than the quoter interface's: a source
/// with no resting orders has neither. What the caller owns is the
/// *consequence* of a removal — a maker's margin reservation, a reward, a
/// trigger slot — which is why it asks rather than the book acting alone.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub enum ClobRemovalKindV0 {
    /// An order past its `max_ts`. Quote and execute already skip these; the
    /// order still holds a node and its owner's reservation until removed.
    Expired,
    /// The worst-priced order on the side that has reached the book's own
    /// eviction threshold. Both the threshold and which side to relieve first
    /// are the book's policy, so a caller asking this never has to know
    /// either. The answer names the side it chose.
    Evictable,
}

/// `next_removal_v0` arguments.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct NextRemovalArgsV0 {
    pub kind: ClobRemovalKindV0,
}
