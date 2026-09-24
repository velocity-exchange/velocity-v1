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

/// The CLOB program, `BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU`. Velocity
/// refuses a quoter config that names any other program. The CLOB asserts its
/// own `declare_id!` against these bytes, so a divergence fails that build.
pub const CLOB_PROGRAM_ID: solana_address::Address = solana_address::Address::new_from_array([
    154, 89, 161, 4, 195, 203, 187, 235, 187, 150, 76, 246, 47, 233, 123, 46, 134, 65, 53, 146, 67,
    69, 241, 41, 109, 179, 123, 27, 35, 234, 1, 163,
]);

/// Compile-time equality against [`CLOB_PROGRAM_ID`]. Array `PartialEq` is not
/// const, so the bytes are compared one at a time.
pub const fn is_clob_program_id(bytes: [u8; 32]) -> bool {
    let id = CLOB_PROGRAM_ID.to_bytes();
    let mut index = 0;
    while index < 32 {
        if bytes[index] != id[index] {
            return false;
        }
        index += 1;
    }

    true
}

/// Order handle. The node index is an O(1) hint the book verifies against the order
/// id, so a stale hint fails closed rather than acting on whichever order took the
/// slot. The `Clob` prefix is deliberate: this is the one type here that lands in
/// velocity's IDL, beside `Order` and `OrderParams`, where `OrderRefV0` names no
/// program.
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
    /// The order is an unfilled taker remainder the caller migrated onto the book,
    /// which only the caller can know. The order cannot be taken while a live
    /// counterparty crosses it, and a cross settles at the counterparty's price.
    pub taker_origin: bool,
    /// The caller's own id for this order. The book stores it and reports it
    /// back on every answer that names the order, so the caller never holds a
    /// map from the book's ids to its own. The book neither sorts nor
    /// identifies orders by it. Zero means the caller keeps no id.
    pub client_order_id: u32,
    /// Refuse the placement when the order would cross the opposite best price, rather
    /// than resting it crossed. A crossed order still fills at its own price, so this
    /// is about whether the order rests at all. A maker that quotes through the other
    /// side has mispriced and would rather place nothing.
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

/// `fill_v0` arguments. A taker remainder resting here can aggress against sources
/// this program cannot see, such as a quoter or the vAMM, so velocity does that
/// matching and reports back. A list rather than a single fill, so a transaction that
/// resolves several remainders pays for one call.
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
    /// The expiry the order carried, zero for good until cancelled. A modify removes an
    /// order and rests an equivalent one, and this lets the caller carry the expiry
    /// across without reading the book. Removal is the last moment it is knowable.
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
    /// The sweep took every order it was asked for. False means the book stopped at its
    /// per-call cap, which repeating the call clears, or passed over a taker-origin
    /// remainder whose claim the book still honours, which clears itself.
    pub exhaustive: bool,
}

/// One resolver registration: which program answers a condition, with which
/// instruction, and what it must pay the keeper that lands the answer. The same three
/// fields relay's `CrankSpecV0` carries, restated because they travel as instruction
/// arguments on the one wire velocity and the book share.
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

/// `set_crank_conditions_v0` arguments: who resolves each of the book's own conditions,
/// and the accounts they all take.
///
/// The book owns the wakes but none of the answers, so the program that owns the flow
/// registers what runs. One account list serves every condition. A second call replaces
/// the registration, and a zero `program` deactivates a condition.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct CrankConditionsArgsV0 {
    pub expiry: CrankResolverV0,
    pub activation: CrankResolverV0,
    pub capacity: CrankResolverV0,
    pub cross: CrankResolverV0,
    pub accounts: Vec<CrankAccountV0>,
}

/// Which slot of the book's condition block each of [`CrankConditionsArgsV0`]'s
/// resolvers is written to. Relay names the condition that fired by slot index, so the
/// numbering is part of this wire rather than the book's private business. The book
/// asserts its own slots against these.
pub const CRANK_SLOT_EXPIRY: u8 = 0;
pub const CRANK_SLOT_ACTIVATION: u8 = 1;
pub const CRANK_SLOT_CAPACITY: u8 = 2;
pub const CRANK_SLOT_CROSS: u8 = 3;

/// Return data of `set_crank_conditions_v0`: the two regions of the market account a
/// registrant has to point relay at. A watch names an account, an offset and a length,
/// so reporting them lets a registrant watch without knowing this account's layout.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct CrankBlockV0 {
    /// Where the condition block starts.
    pub block_offset: u32,
    /// The region that changes whenever either side's best moves. A crossing order is
    /// always a new best, so a watch here catches every cross. A caller that crosses
    /// another source against this book needs a second watch, because repricing that
    /// source writes nothing here.
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
    /// Floor on the size of an order that may end a fill walk when its owner is not in
    /// the caller's user set. The book skips a smaller order at any age. A maker sizing
    /// a quote needs this, because it is what keeping price priority costs against a
    /// caller that leaves the maker out. Zero disables the floor.
    pub blocking_min_size: u64,
    /// Slots added to the placement slot to get `activation_slot` when the
    /// caller chooses no delay. A caller that compares its own delay against
    /// the book's own reads this value.
    pub default_activation_delay_slots: u32,
    /// Upper bound on a caller-chosen activation delay.
    pub max_activation_delay_slots: u32,
    /// The key the book requires to sign a placement, cancel, evict, expire or execute,
    /// and the book's whole trust root. A caller pins it to its own signing PDA, so the
    /// book only ever acts under a key the caller controls.
    pub place_authority: [u8; 32],
    /// The book's price and size grid. A caller that migrates an order pins these to
    /// its market's grid at attach, so an aligned remainder always rests. An off-tick
    /// rejection would revert the whole fill that carried it.
    pub tick_size: u64,
    pub step_size: u64,
    /// Resting orders on each side right now, bids first. A side is full at
    /// `arena_capacity / 2` and the book refuses a placement onto a full side, so a
    /// caller predicts the refusal and fills without resting. The counts are a fact
    /// about the slot this call ran in, not a rule.
    pub side_order_counts: [u32; 2],
    /// Order slots the whole arena holds. Half of it is the per-side cap.
    pub arena_capacity: u32,
    /// Count at which the eviction crank may take a side's tail. A side
    /// between this and its cap still accepts placements, and the crank works
    /// it back down.
    pub evict_threshold_per_side: u32,
}

/// One order, as the book describes it to a caller. `next_removal_v0`, `next_cross_v0`
/// and `orders_v0` all answer with this shape, which is the only way the three stay in
/// step. Every field is stated rather than derived, because deriving one means knowing
/// how a node is laid out.
///
/// `order_ref.order_id == 0` means there is no such order. Ids start at one and are
/// never reused, and return data is a fixed width, so an absent order needs a
/// sentinel.
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

    /// True when this order rested before `other`. Ids come from a counter that only
    /// increases, so a lower id was placed earlier and no timestamp has to travel with
    /// the order.
    pub fn rested_before(&self, other: &Self) -> bool {
        self.order_ref.order_id < other.order_ref.order_id
    }
}

/// Return data of `next_cross_v0`: the best matchable order on each side. Matchable is
/// the book's own predicate, meaning open, activated and unexpired, so a caller never
/// re-derives it. Any cross settles between these two heads. A caller compares their
/// prices and reads [`OrderViewV0::taker_origin`] to see which side came to trade.
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

/// Most refs one `orders_v0` call may ask about. Return data is capped at 1 KB and an
/// [`OrderViewV0`] is 84 bytes, so twelve plus the length prefix is what fits.
pub const ORDER_VIEW_CEILING: usize = 12;

/// `orders_v0` arguments: which orders to describe. A caller holds refs from its own
/// records or from a client that read the book off chain, and cannot tell which still
/// name a live order. Asking replaces reading the arena.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct OrdersArgsV0 {
    /// At most [`ORDER_VIEW_CEILING`] refs.
    pub refs: Vec<ClobOrderRefV0>,
}

/// Return data of `orders_v0`: one [`OrderViewV0`] per requested ref, in the order they
/// were asked for. A ref that no longer names a live order comes back as
/// [`OrderViewV0::NONE`] rather than being dropped, so a caller reads the answers by
/// position. A race with a fill or a crank produces this, and it is not an error.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct OrdersV0 {
    pub orders: Vec<OrderViewV0>,
}

/// Which of the book's own removal cranks a caller is asking about. Both belong to the
/// book rather than the quoter interface, and a source with no resting orders has
/// neither. The caller asks rather than the book acting alone, because the caller owns
/// the consequence: a maker's margin, a reward, and a trigger slot.
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
    /// The worst-priced order, other than a bound taker remainder, on the side
    /// that has reached the book's own eviction threshold. Both the threshold
    /// and which side to relieve first are the book's policy, so a caller
    /// asking this never has to know either. The answer names the side it chose.
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

/// Anchor-default instruction discriminators, `sha256("global:<name>")[..8]`,
/// of the calls velocity makes into the book. Anchor derives them from the
/// handler name, so a rename on the book's side is silent here. The CLOB
/// asserts every one of them against its own derived value in a unit test.
pub mod discriminator {
    pub const PLACE_ORDER_V0: [u8; 8] = [100, 204, 57, 226, 245, 228, 61, 187];
    pub const CANCEL_ORDER_V0: [u8; 8] = [70, 91, 225, 16, 228, 203, 124, 174];
    pub const FILL_V0: [u8; 8] = [66, 113, 11, 94, 94, 23, 154, 137];
    pub const CANCEL_ALL_V0: [u8; 8] = [212, 11, 203, 11, 184, 40, 88, 95];
    pub const EVICT_WORST_V0: [u8; 8] = [106, 60, 27, 129, 80, 27, 37, 73];
    pub const REMOVE_EXPIRED_V0: [u8; 8] = [241, 135, 215, 18, 254, 107, 179, 119];
    pub const NEXT_REMOVAL_V0: [u8; 8] = [132, 65, 9, 126, 135, 115, 177, 92];
    pub const SET_CRANK_CONDITIONS_V0: [u8; 8] = [34, 160, 120, 93, 84, 133, 8, 95];
    pub const ORDERS_V0: [u8; 8] = [124, 117, 208, 33, 202, 209, 58, 199];
    pub const ORDER_RULES_V0: [u8; 8] = [201, 129, 212, 105, 18, 69, 149, 252];
}
