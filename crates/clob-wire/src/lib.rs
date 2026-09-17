// The CLOB's instruction arguments and return data.
//
// # One declaration, two programs
//
// Velocity reaches the book by CPI to place, cancel, evict, and reclaim an
// expired order. Every one of those calls is bytes that velocity writes and
// the book reads. Two declarations pin nothing against each other. A field
// reordered on one side gives two self-consistent programs that disagree
// about the bytes between them. The disagreement lands on a placement, where
// a misread `base_asset_amount` rests the wrong size against a real user's
// margin.
//
// [`quoter_spec`] makes the same argument for the quoter interface. This
// crate is its counterpart for the surface a book has beyond that interface.
// Every PropAMM implements `quoter-spec`. These instructions are not part of
// it.
//
// # What is not here
//
// The book's account layout, meaning its header offsets and node arena. A
// caller that needs to know where a field sits inside the market account
// reads the book's memory rather than calling it. This crate exists to
// replace that.
//
// # Serialization
//
// Per-consumer, as in `quoter-spec`. Velocity encodes with anchor's borsh and
// needs the IDL plumbing. The book writes the bytes itself and carries no
// borsh crate, because of its binary size and CU budget. The two encodings
// are byte-compatible, because wincode's configuration here is anchor's
// `BORSH_CONFIG`. One declaration serves both.

// The v2 IdlType derive emits `anchor_lang::`. This points that name at the
// fork when the v2 IDL build is on. The v1 crate never defines the feature,
// so the alias is inert there.
#[cfg(feature = "idl-build-v2")]
extern crate anchor_lang_v2 as anchor_lang;

pub use quoter_spec::{CancelSidesV0, SideV0, UserRefV0};

/// Order handle. The node index is an O(1) hint that the book verifies
/// against the order id. A stale hint, whose node was freed or reused, fails
/// closed rather than acting on whichever order took the slot.
///
/// The `Clob` prefix repeats the crate name on purpose. This is the one type
/// on this wire that reaches velocity's instruction arguments, so it is the
/// one that lands in velocity's IDL. There it sits beside `Order`, `OrderType`
/// and `OrderParams`, where a bare `OrderRefV0` names no program. Anchor takes
/// the declared name, not the alias, so a rename here renames it there.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
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
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct PlaceOrderArgsV0 {
    pub side: SideV0,
    pub price: u64,
    pub base_asset_amount: u64,
    /// `None` takes the market's default activation delay. A value must be at
    /// or under the market's maximum. Zero is allowed. The caller owns
    /// attestation policy, and the book only clamps.
    pub activation_delay_slots: Option<u32>,
    /// Zero means good until cancelled.
    pub max_ts: i64,
    /// The user the order settles against, in derivable form. The book trusts
    /// its `place_authority` for identity. The caller verified control before
    /// the CPI.
    pub user: UserRefV0,
    /// The order is an unfilled taker remainder the caller migrated onto the
    /// book, rather than a quote its owner chose to post. Only the caller can
    /// know that, so the caller declares it instead of the book inferring it.
    /// The flag has two effects. The order cannot be taken while a live
    /// counterparty crosses it. A cross that involves it settles at the
    /// counterparty's price.
    pub taker_origin: bool,
    /// The caller's own id for this order. The book stores it and reports it
    /// back on every answer that names the order, so the caller never holds a
    /// map from the book's ids to its own. The book neither sorts nor
    /// identifies orders by it. Zero means the caller keeps no id.
    pub client_order_id: u32,
    /// Refuse the placement when the order would cross the opposite best
    /// price, instead of resting it crossed.
    ///
    /// A crossed order still fills at its own price, because the cross crank
    /// matches it as a maker. The flag is about whether the order rests at
    /// all. A maker that quotes through the other side has mispriced. It would
    /// rather place nothing than hold a position it did not intend to take.
    pub reject_if_crossed: bool,
    /// The order only reduces its owner's position. The book does not see
    /// positions, so the caller declares this. At match time the book clamps a
    /// fill against a reduce-only order to the owner's `base_cover` cap from
    /// the execute call's user set.
    pub reduce_only: bool,
}

/// `cancel_order_v0` arguments.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
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
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct FillRequestV0 {
    pub order_ref: ClobOrderRefV0,
    pub base_asset_amount: u64,
}

/// `fill_v0` arguments.
///
/// A taker remainder resting here can be the aggressor of a match. The
/// sources it trades against are not all on this book. A quoter or the vAMM
/// may hold the better price, and this program can see neither. So velocity
/// does that matching and reports back which orders filled and by how much.
///
/// A list rather than a single fill, so a transaction that resolves several
/// remainders pays for one call.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct FillArgsV0 {
    pub fills: Vec<FillRequestV0>,
}

/// What one order in a [`FillArgsV0`] came to.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct FilledOrderV0 {
    pub order_id: u64,
    /// The placing caller's own id, so a caller joins this to its own order
    /// without holding a map between the two id spaces.
    pub client_order_id: u32,
    pub base_asset_amount: u64,
    /// Size dropped because the remainder fell under `min_order_size`. The
    /// book will not hold it, so the caller unwinds it from the owner's
    /// reservation.
    pub culled_base_asset_amount: u64,
    /// The order left the book, either filled out or culled.
    pub removed: bool,
}

/// Return data of `fill_v0`.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct FillOutcomeV0 {
    pub filled: Vec<FilledOrderV0>,
}

/// `evict_worst_v0` arguments.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct EvictWorstArgsV0 {
    pub side: SideV0,
}

/// `remove_expired_v0` arguments.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct RemoveExpiredArgsV0 {
    pub order_ref: ClobOrderRefV0,
}

/// `cancel_all_v0` arguments.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct CancelAllArgsV0 {
    /// Whose orders to withdraw, verified against each node.
    pub user: UserRefV0,
    pub sides: CancelSidesV0,
    /// Sweep taker-origin remainders that have not reached their activation
    /// slot as well. Liquidation sets this flag. Every other caller leaves it
    /// clear, and the sweep then skips such an order and reports the call as
    /// not exhaustive.
    pub force: bool,
}

/// Return data of `cancel_order_v0`, `evict_worst_v0` and
/// `remove_expired_v0`: the order that left the book.
///
/// `side` tells the caller whether the remaining size unwinds its bid-side or
/// ask-side reservation.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct RemovedOrderV0 {
    pub user: UserRefV0,
    pub order_id: u64,
    /// The caller's own id for this order, as supplied at placement.
    pub client_order_id: u32,
    pub price: u64,
    pub base_asset_amount: u64,
    pub side: SideV0,
    /// The order was taker-origin. The flag tells a caller which side of the
    /// cross it is resolving demanded liquidity, and therefore which side's
    /// price the match settles at.
    pub taker_origin: bool,
    /// The order was reduce-only. A modify removes the order and rests an
    /// equivalent one, and must carry the flag across. Otherwise the
    /// replacement rests uncapped.
    pub reduce_only: bool,
    /// The expiry the order carried, zero for good until cancelled.
    ///
    /// A modify removes an order and rests an equivalent one. Reporting the
    /// expiry lets that caller carry it across without reading the book. The
    /// removal is the last moment the value is knowable, and reading it off
    /// the node beforehand means knowing where a node keeps it.
    pub max_ts: i64,
}

/// Return data of `cancel_all_v0`. Per-side totals rather than a list of
/// removals, because that is the shape open-order aggregates consume. The
/// caller does one unwind per side and one count, however many orders the
/// sweep took.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct CancelAllOutcomeV0 {
    pub user: UserRefV0,
    pub bid_base_asset_amount: u64,
    pub ask_base_asset_amount: u64,
    pub bid_orders: u32,
    pub ask_orders: u32,
    /// How many of the swept orders on each side were reduce-only. The caller
    /// tracks reduce-only resting orders per user. It disarms exactly this
    /// many when the sweep removes them, without a per-order report.
    pub bid_reduce_only_orders: u32,
    pub ask_reduce_only_orders: u32,
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
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
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
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct CrankAccountV0 {
    pub address: [u8; 32],
    /// 0 = readonly, nonzero = writable.
    pub writable: u8,
}

/// `set_crank_conditions_v0` arguments: who resolves each of the book's own
/// conditions, and the accounts they all take.
///
/// The book owns the wakes. An order expiring, an order activating, a side
/// reaching its cap, and the two sides crossing are all facts about its own
/// account, and it keeps them current as it places and removes orders. The
/// book owns none of the answers. Removing an order releases a maker's margin
/// reservation, pays a reward, and frees a trigger slot, and the book holds
/// none of those. So the program that owns the flow registers what runs, and
/// each condition wakes that program's resolver.
///
/// One account list serves every condition, because the resolvers belong to
/// that one program and read the same state.
///
/// A second call replaces the registration in place, which is how a re-priced
/// crank or a rotated resolver lands. A zero `program` on a resolver
/// deactivates its condition.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
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
/// Relay names the condition that fired by slot index. A program that
/// registers one resolver for several conditions needs the mapping from the
/// argument names above to those indices. That makes the numbering part of
/// this wire rather than the book's private business, and the book asserts
/// its own slots against these.
pub const CRANK_SLOT_EXPIRY: u8 = 0;
pub const CRANK_SLOT_ACTIVATION: u8 = 1;
pub const CRANK_SLOT_CAPACITY: u8 = 2;
pub const CRANK_SLOT_CROSS: u8 = 3;

/// Return data of `set_crank_conditions_v0`: the two regions of the market
/// account a registrant has to point relay at.
///
/// A watch registration names an account, an offset and a length. Reporting
/// them here lets a registrant register a watch without knowing this
/// account's layout. Every other answer on this wire exists for that reason.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct CrankBlockV0 {
    /// Where the condition block starts.
    pub block_offset: u32,
    /// The region that changes whenever either side's best moves.
    ///
    /// A crossing order is always a new best, so a watch here catches every
    /// cross as it appears. The book registers its own watch on this region. A
    /// caller that crosses another source against this book registers a second
    /// watch, because repricing that source writes nothing here.
    pub top_of_book_offset: u32,
    pub top_of_book_len: u32,
}

/// Return data of `order_rules_v0`: what the book requires of an order before
/// it will hold one.
///
/// A caller that builds orders has to satisfy these, and finding out by
/// rejection costs it the transaction. Asking replaces reading them out of
/// the market account's header.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct OrderRulesV0 {
    /// Floor on a resting order's size. A remainder below it cannot rest, and
    /// the book culls one on its own fills. A caller re-placing a partially
    /// filled remainder drops it instead of offering a placement the book
    /// rejects.
    pub min_order_size: u64,
    /// Floor on the size of an order that may end a fill walk when its owner
    /// is not in the caller's user set. The book skips a smaller order at any
    /// age, the same way it skips a too-fresh one.
    ///
    /// A maker sizing a quote needs this value. It is what keeping price
    /// priority costs against a caller that leaves the maker out. A reader
    /// deciding which owners to carry does not need it, because `quote_l3_v0`
    /// flags per row the orders that can end a walk.
    ///
    /// Zero disables the floor, which is what a market that has never set one
    /// reports.
    pub blocking_min_size: u64,
    /// Slots added to the placement slot to get `activation_slot` when the
    /// caller chooses no delay. A caller that compares its own delay against
    /// the book's own reads this value.
    pub default_activation_delay_slots: u32,
    /// Upper bound on a caller-chosen activation delay.
    pub max_activation_delay_slots: u32,
    /// The key the book requires to sign a placement, cancel, evict, expire,
    /// or execute. It is the book's whole trust root. A caller that settles
    /// fills for whoever the book names as a maker pins this to its own
    /// signing PDA, so the book only ever acts under a key the caller
    /// controls. Velocity does that through `QuoterSubjects::Book`. Reporting
    /// the key here keeps the caller from depending on where the book stores
    /// it.
    pub place_authority: [u8; 32],
    /// The book's price and size grid. A caller that migrates an order onto
    /// the book pins these to its market's grid at attach. A remainder aligned
    /// to the market can then always rest, and is never rejected off-tick or
    /// off-step. Such a rejection reverts the whole fill that carried the
    /// remainder.
    pub tick_size: u64,
    pub step_size: u64,
    /// Resting orders on each side right now, bids first.
    ///
    /// A side is full at `arena_capacity / 2`, and the book refuses a
    /// placement onto a full side. A caller that rests a taker's remainder has
    /// to know that before it commits the fill the remainder came out of,
    /// because the refusal reverts that whole fill. With these two numbers the
    /// caller predicts the refusal and fills without resting instead.
    ///
    /// The counts move with every placement and removal, so they are a fact
    /// about the slot this call ran in rather than a rule. They are reported
    /// here because this is the call a caller already makes before it rests.
    pub side_order_counts: [u32; 2],
    /// Order slots the whole arena holds. Half of it is the per-side cap.
    pub arena_capacity: u32,
    /// Count at which the eviction crank may take a side's tail. A side
    /// between this and its cap still accepts placements, and the crank works
    /// it back down.
    pub evict_threshold_per_side: u32,
}

/// One order, as the book describes it to a caller.
///
/// `next_removal_v0`, `next_cross_v0` and `orders_v0` all answer with this one
/// shape. They ask different questions and get back the same thing, because
/// what a caller needs about an order does not depend on why it asked. It
/// needs the handle to act on the order, whose order it is, and the fields it
/// prices or sizes a decision with. One declaration is also the only way the
/// three answers stay in step.
///
/// Every field is stated rather than derived. Deriving one means knowing how a
/// node is laid out, which is what calling the book instead of reading it
/// exists to avoid.
///
/// `order_ref.order_id == 0` means there is no such order. The book hands ids
/// out from one and never reuses one, so no live order carries zero. These
/// answers travel as return data, where a caller reads a fixed width or
/// nothing at all, so an absent order needs a sentinel.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
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
    /// The expiry the order carries, zero for good until cancelled.
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

    /// True when this order rested before `other`.
    ///
    /// The book hands out ids from a counter that only increases and never
    /// reuses a value, so a lower id was placed earlier. The id alone gives
    /// rest-time priority, and no slot has to travel with the order.
    pub fn rested_before(&self, other: &Self) -> bool {
        self.order_ref.order_id < other.order_ref.order_id
    }
}

/// Return data of `next_cross_v0`: the best matchable order on each side.
///
/// Matchable is the book's own predicate, meaning open, activated, and
/// unexpired. A caller comparing the two heads never re-derives it, and never
/// has to know how a node stores an activation slot or an expiry.
///
/// Any cross settles between these two heads. A caller compares their prices
/// to see whether the book crosses itself at all, and reads
/// [`OrderViewV0::taker_origin`] to see which side came to trade. Those two
/// facts are the whole of what it needs to price the match. `quote_l3_v0`
/// already answers the separate question of depth behind the heads.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
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

    /// True when the two heads cross. False when either side is empty.
    pub fn crosses(&self) -> bool {
        self.bid.found() && self.ask.found() && self.bid.price >= self.ask.price
    }
}

/// Most refs one `orders_v0` call may ask about.
///
/// The answer travels as return data, which is capped at 1 KB. An
/// [`OrderViewV0`] is 84 bytes on this wire, so twelve of them plus the
/// sequence length prefix is what fits. A caller with more refs than this asks
/// more than once.
pub const ORDER_VIEW_CEILING: usize = 12;

/// `orders_v0` arguments: which orders to describe.
///
/// A caller holds refs from its own records, or from a client that read the
/// book off chain. Without the book's memory it cannot tell which of them
/// still name a live order, or what those orders hold. Asking replaces
/// reading the arena.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct OrdersArgsV0 {
    /// At most [`ORDER_VIEW_CEILING`] refs.
    pub refs: Vec<ClobOrderRefV0>,
}

/// Return data of `orders_v0`: one [`OrderViewV0`] per requested ref, in the
/// order they were asked for.
///
/// A ref that no longer names a live order comes back as [`OrderViewV0::NONE`]
/// rather than being dropped, so a caller reads the answers against its own
/// list by position. A race with a fill or a crank produces this, and it is
/// not an error.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct OrdersV0 {
    pub orders: Vec<OrderViewV0>,
}

/// Which of the book's own removal cranks a caller is asking about.
///
/// Both belong to the book rather than to the quoter interface. A source with
/// no resting orders has neither. The caller owns the consequence of a
/// removal, meaning a maker's margin reservation, a reward, and a trigger
/// slot. That is why the caller asks rather than the book acting alone.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub enum ClobRemovalKindV0 {
    /// An order past its `max_ts`. Quote and execute already skip these. The
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
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct NextRemovalArgsV0 {
    pub kind: ClobRemovalKindV0,
}
