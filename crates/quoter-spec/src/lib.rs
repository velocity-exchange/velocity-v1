// Wire format for velocity's quoter interface: the contract between velocity
// (the router) and any program registered as a quoter — the CLOB, the
// midpoint, and third-party PropAMMs.
//
// # One declaration, three programs
//
// This crate is the whole interface: the arguments a quoter is called with
// ([`QuoteArgsV0`], [`ExecuteArgsV0`] and the types they carry) and the
// responses it must produce. Reading it should be enough to implement one.
//
// `quote_v0` and `execute_v0` answer across a program boundary in bytes.
// Velocity writes the arguments and reads the responses; the quoter does the
// reverse. Declaring the shape once per program leaves nothing pinning the
// declarations against each other, so a
// field added on one side and forgotten on another gives two self-consistent
// programs that disagree about the bytes between them — and the disagreement
// lands on a value transfer, where a misread `base_size` moves the wrong
// amount of a user's collateral. The types live here and all three use them.
//
// # The responses are read in place
//
// A response is plain data sitting in the quoter's account, and velocity reads
// it there: fixed-width records, little-endian, no length-prefixed nesting and
// no deserialization step. That is not only a CU question. Velocity's heap is
// 32 KB and never reclaims, and one fill CPIs every registered quoter twice,
// so a response that decodes into `Vec`s spends heap per quoter per fill that
// nothing gives back.
//
// Every record is `#[repr(C)]` and free of implicit padding, which is what
// both `bytemuck::Pod` and wincode's zero-copy rules require. The `Pod`
// derives below are the enforcement: a field reordered into a layout with a
// padding hole stops compiling rather than silently changing the wire. Field
// order is therefore load-bearing — the `u64`s lead so the 34-byte
// [`UserRefV0`] cannot push one out of alignment, and each record carries
// explicit tail padding to a multiple of its alignment.
//
// # Framing
//
// Each response is a header of counts followed by that many fixed-width
// records per section, in declaration order. Sections are contiguous; the
// caller owns anything past the last one.
//
// A quoter does not serialize a response, it streams one: the records come
// out of a book walk that does not know a section's count until it ends. The
// streaming writers are [`QuoteWriter`] and [`ExecuteWriter`], and they live
// here for the reason the records do. A section a quoter forgets to write is
// the same disagreement as a field read at the wrong offset. The sections are
// `finish`'s parameters, so a new one stops every quoter compiling.
//
// An execute response is [`ExecuteHeaderV0`], then [`UserBalanceChangeV0`],
// then [`CancelledRemainderV0`], then [`CompletedOrderV0`], then
// [`PartiallyFilledOrderV0`]. Completed order
// ids are their own section rather than a list inside each change: a quoter
// aggregates repeated fills into one record per user as it goes, so ids for a
// user arrive interleaved with other users' fills. Naming the change from the
// id makes appending one an O(1) write at the tail instead of a shift of
// everything after it.
//
// # Addresses
//
// Velocity names the address type `Pubkey` and the v2 programs name it
// `Address`; it is one type, because solana-pubkey re-exports `Address as
// Pubkey` and solana-address 1.x is a shim over 2.x. It is spelled `Pubkey`
// here because anchor's IDL derive recognizes it by that token rather than by
// the type it resolves to, and velocity is the consumer that runs
// `anchor idl build`.

// Re-exported so a consumer can write a response without taking its own
// wincode dependency — the framing is this crate's to define, so the encoder
// is too.
// The v2 IdlType derive emits `anchor_lang::`; point it at the fork when the
// v2 IDL build is on. Inert (feature undefined) in the v1 crate.
#[cfg(feature = "idl-build-v2")]
extern crate anchor_lang_v2 as anchor_lang;

pub use wincode;
pub mod write;
pub use write::{ExecuteWriter, L3Writer, QuoteWriter};
use {
    bytemuck::{Pod, Zeroable},
    solana_address::Address as Pubkey,
    wincode::{SchemaRead, SchemaWrite},
};

/// A velocity user in derivable form: the wallet and sub-account index that
/// both the `User` and `UserStats` PDAs derive from.
///
/// Stored rather than the `User` key so an off-chain reader — a relay resolver
/// staging a crank — can reach every user-derived account from a quoter's
/// state alone. A stored `User` key is a dead end, because its authority lives
/// inside account data the reader cannot load. Velocity matches refs against
/// its loaded users by field, never by derivation, so the hot path pays
/// nothing for this.
#[repr(C)]
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Pod, Zeroable, SchemaRead, SchemaWrite)]
#[wincode(assert_zero_copy)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct UserRefV0 {
    pub authority: Pubkey,
    pub sub_account_id: u16,
}

impl UserRefV0 {
    pub const SIZE: usize = 34;

    pub const ZERO: Self = Self {
        authority: Pubkey::new_from_array([0u8; 32]),
        sub_account_id: 0,
    };

    /// The wire encoding as a fixed array, for comparing against and writing
    /// into a response region without a heap round-trip.
    #[inline]
    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut bytes = [0u8; Self::SIZE];
        bytes[..32].copy_from_slice(self.authority.as_array());
        bytes[32..].copy_from_slice(&self.sub_account_id.to_le_bytes());
        bytes
    }
}

/// One user's share of an executed fill.
///
/// The sign convention is the taker's direction, not this user's: `base_size`
/// is subtracted from this user when the taker went long — the taker takes
/// base from them — and added when the taker went short. `quote_size` moves
/// the opposite way.
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

/// One order a quoter removed as a sub-min remainder of a fill.
///
/// Distinct from a completed order: a completed order was consumed, this one
/// was culled because what remained of it fell under the market's minimum.
/// Both unwind the maker's aggregates, but only this one carries a size to
/// release.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Pod, Zeroable, SchemaRead, SchemaWrite)]
#[wincode(assert_zero_copy)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct CancelledRemainderV0 {
    pub order_id: u64,
    pub base_asset_amount: u64,
    /// The price the culled order was resting at.
    ///
    /// A cull is the one removal on the fill path the caller has to report
    /// itself, and a removal record that could not state a price would carry a
    /// zero into whatever reads it. There is at most one of these per execute,
    /// so the width is paid once rather than per fill.
    pub price: u64,
    /// The caller's own id for this order, carried back so the removal lands
    /// on the order the caller knows. See [`CompletedOrderV0::client_order_id`].
    pub client_order_id: u32,
    pub user: UserRefV0,
    pub _pad: [u8; 2],
}

/// One resting order a fill fully consumed, naming the balance change it
/// belongs to by index.
///
/// The reader decrements that user's open-order count once per entry and
/// releases any per-order state it keeps against the book, so an id for an
/// order still live on the book frees a live order's shadow.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Pod, Zeroable, SchemaRead, SchemaWrite)]
#[wincode(assert_zero_copy)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct CompletedOrderV0 {
    pub order_id: u64,
    /// Which entry of [`ExecuteResponseV0::changes`] this order belongs to.
    ///
    /// The two lists are not the same length, which is the whole reason this
    /// field exists. Balance changes merge by user — a maker whose three
    /// orders a sweep consumed gets *one* change carrying the summed base and
    /// quote — while a completed entry is per order, so three of them point
    /// at that one change.
    ///
    /// An index rather than a [`UserRefV0`] because a ref is 34 bytes and
    /// this record is 16. Repeating the owner on every consumed order would
    /// cost more than the orders do.
    ///
    /// A quoter fills it in as it walks: when an order is consumed whole, the
    /// index is the position of the change record its owner already has, or
    /// the position of the one about to be written for them.
    ///
    /// What a caller does with it is the reason to get it right. The change
    /// moves the position; these ids do the per-order bookkeeping the change
    /// cannot express — closing out each order, and freeing whatever the
    /// caller keeps per order against the book. Velocity also counts them per
    /// change to widen the rounding it allows: a change merged from N orders
    /// was priced across N levels, so it is held to N roundings rather than
    /// one, and under-reporting them prices the maker's fill outside its own
    /// quote.
    ///
    /// [`ExecuteResponseV0::parse`] refuses an index past the end of
    /// `changes`. It has to: the reader indexes with it, so a dangling one
    /// unwinds whichever record happens to sit there, which is some other
    /// user's live margin.
    pub change_index: u32,
    /// The caller's own id for this order.
    ///
    /// A quoter's order id is its own; the caller has one too, minted when it
    /// asked for the placement, and every record it keeps is filed under that
    /// one. Reporting it here is what lets the caller close its record without
    /// holding a map from one id space to the other. Zero when the caller
    /// supplied none.
    pub client_order_id: u32,
}

/// The one order a fill left resting with less size than it found.
///
/// A completed order tells the caller an order is gone. This tells it an
/// order moved. Both are per-order facts a balance change cannot express: the
/// change merges every order of one maker into one record, so a caller
/// holding per-order state has nothing to apply a partial fill to.
///
/// At most one exists per execute. A best-first walk consumes whole orders
/// until the taker's size runs out, and running out is what ends the walk —
/// so only the last order it reached can be partial, and a partial that fell
/// under the market's minimum is a [`CancelledRemainderV0`] instead.
///
/// `base_filled` is what this order gave to this fill, not what it has given
/// in its life. The caller accumulates.
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
    /// Bounded by [`ExecuteResponseV0::parse`] for the reason a completed
    /// order's index is.
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

/// Bytes wincode spends on a slice's length prefix.
///
/// A quoter that streams its records — writing them as it walks a book,
/// rather than gathering them into a slice it could serialize in one call —
/// has to lay this prefix down itself and backfill the count at the end. The
/// encoding is wincode's, not a choice this crate makes, so
/// [`tests::the_length_prefix_is_what_wincode_writes`] pins the two together.
pub const LEN_BYTES: usize = 8;

/// The length prefix wincode writes ahead of a slice of `count` records.
#[inline]
pub fn len_prefix(count: usize) -> [u8; LEN_BYTES] {
    (count as u64).to_le_bytes()
}

const _: () = {
    // The records are the wire; a field reordered or widened must fail here
    // rather than change what the other program reads.
    assert!(USER_REF_BYTES == 34);
    assert!(CHANGE_BYTES == 56);
    assert!(CANCELLED_BYTES == 64);
    assert!(COMPLETED_BYTES == 16);
    assert!(PARTIAL_BYTES == 24);
    assert!(PRICE_LEVEL_BYTES == 16);
};

/// What `execute_v0` answers: every balance change the fill produced, every
/// sub-min remainder it removed, and every resting order it consumed.
///
/// The fields borrow straight out of the quoter's account — reading one
/// allocates nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct ExecuteResponseV0<'a> {
    pub changes: &'a [UserBalanceChangeV0],
    pub cancelled: &'a [CancelledRemainderV0],
    pub completed: &'a [CompletedOrderV0],
    /// The order the fill left resting smaller, at most one. A slice rather
    /// than an option so every section of this response is framed the same
    /// way and a writer lays them all down with one call.
    pub partial: &'a [PartiallyFilledOrderV0],
}

impl<'a> ExecuteResponseV0<'a> {
    /// Read a response out of `bytes`.
    ///
    /// Validates what makes the bytes readable, and one thing beyond it: every
    /// completed order must name a balance change that exists, because an
    /// index past the end would otherwise unwind whichever record happens to
    /// sit there — a live order's margin. What the numbers *mean* stays the
    /// caller's to check; this cannot know whether a quoter was entitled to
    /// move the amounts it reported.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, SpecError> {
        let response: Self = wincode::deserialize(bytes).map_err(|_| SpecError::Read)?;
        if !completed_orders_fit(response.completed, response.changes.len())
            || !partial_orders_fit(response.partial, response.changes.len())
        {
            return Err(SpecError::DanglingCompletedOrder);
        }
        Ok(response)
    }

    /// The order ids the fill consumed for `change_index`.
    pub fn completed_for(&self, change_index: usize) -> impl Iterator<Item = u64> + '_ {
        self.completed
            .iter()
            .filter(move |entry| entry.change_index as usize == change_index)
            .map(|entry| entry.order_id)
    }

    /// How many orders the fill consumed for `change_index`.
    pub fn completed_count(&self, change_index: usize) -> usize {
        self.completed_for(change_index).count()
    }

    /// The caller's id for the one order behind `change_index`, when there is
    /// exactly one.
    ///
    /// A change merges every order of one maker, so most of the time it names
    /// no single order and this is `None`. When it does name one — one order
    /// consumed and nothing left resting, or one order left resting and none
    /// consumed — the change and the order are the same event, and a reader
    /// can file the fill under that order.
    pub fn sole_client_order_id(&self, change_index: usize) -> Option<u32> {
        let index = change_index as u32;
        let mut ids = self
            .completed
            .iter()
            .filter(|entry| entry.change_index == index)
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
    /// resting there, because that liquidity belongs to a user the caller did
    /// not load. Zeroed when nothing was left out.
    ///
    /// A [`PriceLevelV0`] because that is what it is: one rung the ladder
    /// stops short of. It also keeps the tail of this response one record, so
    /// a quoter writes it the way it writes a rung.
    ///
    /// A caller cannot carry every user a book might hold — a transaction
    /// locks at most 64 accounts and a maker costs two — so a quoter that
    /// refused to answer at all whenever one was missing would make a
    /// fragmented book unfillable. It stops instead, and says where it
    /// stopped.
    ///
    /// That turns depth into the caller's own tradeoff: load more users, win
    /// more of the book. It is also the number that makes the tradeoff
    /// enforceable. A caller that skipped this liquidity and filled elsewhere
    /// at a worse price did not run out of room, it routed around a
    /// competitor, and this field is how its own checks can tell.
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpecError {
    /// The bytes are unreadable as this layout: truncated, or a region whose
    /// start is not aligned for the records it holds. The caller logs the
    /// underlying `wincode` error, which does not survive as a copyable value.
    Read,
    /// A `change_index` names a balance change the response does not contain.
    DanglingCompletedOrder,
    /// The region cannot hold another record of the response being written.
    RegionTooSmall,
    /// The region does not start on the step its records need. A response
    /// written there could not be read in place.
    RegionMisaligned,
    /// The args could not be measured or written. The caller logs the
    /// underlying `wincode` error, which does not survive as a copyable value.
    Write,
}

/// Whether every completed order names a balance change that exists.
///
/// The bound both halves of the wire hold: [`ExecuteResponseV0::parse`]
/// refuses a response that breaks it, and [`ExecuteWriter::finish`] refuses to
/// write one.
pub(crate) fn completed_orders_fit(completed: &[CompletedOrderV0], changes: usize) -> bool {
    completed
        .iter()
        .all(|entry| (entry.change_index as usize) < changes)
}

/// Whether the partial-fill section is one a fill could have produced: at most
/// one record, naming a balance change that exists.
///
/// The count is part of the bound, not a separate check. A second partial
/// would mean the walk continued past an order it did not finish, and a reader
/// that accepted one would apply a fill to an order the quoter never touched.
pub(crate) fn partial_orders_fit(partial: &[PartiallyFilledOrderV0], changes: usize) -> bool {
    partial.len() <= 1
        && partial
            .iter()
            .all(|entry| (entry.change_index as usize) < changes)
}

/// Users a quote or execute may fill, and how much room each has left.
///
/// The request half of the wire, declared here for the reason the responses
/// are: velocity writes these bytes and a quoter reads them, and two
/// hand-mirrored declarations are two programs that can drift into
/// self-consistent disagreement. A cap misread as a taker, or a side read off
/// by one, silently turns a skip into a fill.
///
/// Most a call may name. The bound is the account-lock budget of the
/// transaction that carries the set, and it is also what [`UserCapsV0`] can
/// address: a cap names a user by its index here, and the exclusion bitmap
/// holds one bit per slot up to this number.
pub const USER_SET_CAPACITY: usize = 48;

/// Users that can carry a *partial* cap on one call.
///
/// Only partials need a slot. A user with no room at all rides
/// [`UserCapsV0::excluded_bid`] / `excluded_ask`, one bit each, so every user
/// in the set can be excluded at once — which is what one sharp move
/// produces, and the moment a book most needs to stay usable. What is left
/// here is the narrow band with room for some of what they rest, and an
/// overflow there costs no more than a revert that was already coming.
pub const USER_CAPS_CAPACITY: usize = 8;

/// Bytes of bitmap for one bit per user in the set.
pub const USER_EXCLUSION_BITMAP_BYTES: usize = USER_SET_CAPACITY.div_ceil(8);

/// Taker direction, from the taker's perspective.
///
/// Encoded as its discriminant, `Long = 0`, and every program on this wire
/// reads the same declaration — a taker direction inverted across the
/// boundary would fill the wrong side of a book.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub enum DirectionV0 {
    Long,
    Short,
}

impl DirectionV0 {
    /// The side a taker of this direction consumes.
    pub fn side(self) -> SideV0 {
        match self {
            DirectionV0::Long => SideV0::Ask,
            DirectionV0::Short => SideV0::Bid,
        }
    }
}

/// Where in the quoter's response account it wrote the response. Return data
/// of `quote_v0` and `execute_v0`.
///
/// The one shape a caller reads back from every quoter, whatever it is: the
/// response itself lives in an account the caller then borrows, and this says
/// where to look. Declared here rather than per program for the same reason
/// the rest of the wire is — three copies of two `u32`s are three chances to
/// disagree about which comes first.
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

/// Which sides a `cancel_all_v0` withdraws.
///
/// Named sides rather than a pair of bools, because the wire must not be able
/// to express "neither" — that is a maker believing their quotes are gone
/// when nothing happened.
///
/// What the sides *mean* differs by who is reading: a book walks them as book
/// sides, a caller unwinds them as position directions, a spline reads them as
/// taker directions. Each program adds that reading itself; the tags are the
/// part that has to agree.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub enum CancelSidesV0 {
    Bids,
    Asks,
    Both,
}

/// Which side an order rests on: a bid makes its owner long, an ask short.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub enum SideV0 {
    Bid,
    Ask,
}

/// Base units in one whole base asset. The denominator that turns a base
/// amount and a price difference into a quote amount, and the reason a budget
/// can be spent identically by every quoter.
pub const BASE_PRECISION: u64 = 1_000_000_000;

/// What one named user may still lose on the side this call sweeps, in quote.
///
/// A budget rather than a base amount, because the caller cannot convert the
/// one into the other. What a fill costs a maker is collateral, and the
/// conversion needs the price each order fills at — which the caller does not
/// have and the quoter does. So the caller sends what it knows and the quoter
/// spends it:
///
/// ```text
/// cost = base * |price - reference_price| / BASE_PRECISION
/// ```
///
/// counted only where the fill moves against the owner: an order resting on
/// the bid side costs its owner when it fills *above* the reference, an ask
/// when it fills *below*. An order priced in the owner's favour costs nothing
/// and is filled whole. Once the budget is spent, that user's remaining
/// orders are passed over and the depth behind them is still filled.
///
/// Why this is the cost that matters: a resting order is normally already
/// collateralized against the position it would become, priced at the
/// reference. Filling it converts that reservation into the position it stood
/// for. What the reservation does not price is the owner paying its own limit
/// price for a position marked at the reference, and that difference scales
/// with size.
///
/// One number, not one per side, because a call sweeps one side of the book:
/// [`QuoteArgsV0::direction`] says which, and the budget is for that side.
///
/// `u64::MAX` means unbounded. Zero means the user is excluded outright, and
/// belongs in the bitmap rather than here.
#[repr(C)]
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct UserCapV0 {
    /// Quote this user may lose filling on the swept side.
    pub budget: u64,
    /// The most base the book may fill against this user's **reduce-only**
    /// resting orders on the swept side. `u64::MAX` means no reduce-only cap.
    ///
    /// Unlike [`Self::budget`], this **is** a trust boundary. The book is
    /// position-blind, so a reduce-only order can only be safe if the caller
    /// bounds its fill to the position it may reduce. The book enforces it at
    /// match time and never fills a reduce-only order past it. A reduce-only
    /// order whose owner carries no cap here is not filled at all.
    pub base_cover: u64,
    /// Index into the accompanying user set.
    pub index: u8,
}

/// Per-user room, parallel to the caller's user set.
///
/// A user absent from all of this is unconstrained. An excluded user has
/// their orders passed over entirely — the caller has said it cannot settle a
/// fill against them, so quoting depth standing on their orders would promise
/// depth the fill declines.
///
/// Distinct from membership of the user set: absent from *that* means the
/// caller's account set is stale and, past the grace window, the whole call
/// fails. An exclusion is a deliberate constraint, not a mistake, and never
/// fails the call.
///
/// **Not a trust boundary.** A quoter that ignores these leaves its caller
/// exactly where it stands without them — the caller's own post-fill checks
/// still refuse the fill. What honouring them buys is that the honest case
/// stops reverting. The exclusions are firmer than the budgets: a caller may
/// also refuse a response that names an excluded user, while a budget it
/// cannot reprice is left to those post-fill checks.
#[repr(C)]
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct UserCapsV0 {
    /// One bit per index in the set: set means no room on the swept side, so
    /// pass that user's orders over.
    pub excluded: [u8; USER_EXCLUSION_BITMAP_BYTES],
    /// Live entries at the head of `caps`; the tail is undefined.
    pub len: u8,
    pub caps: [UserCapV0; USER_CAPS_CAPACITY],
}

/// Encoded width of a [`UserCapsV0`]. One constant rather than an assertion
/// per program, which is the point of declaring the shape once.
pub const USER_CAPS_BYTES: usize =
    USER_EXCLUSION_BITMAP_BYTES + 1 + USER_CAPS_CAPACITY * (8 + 8 + 1);

impl Default for UserCapsV0 {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl UserCapsV0 {
    pub const EMPTY: Self = Self {
        excluded: [0; USER_EXCLUSION_BITMAP_BYTES],
        len: 0,
        caps: [UserCapV0 {
            index: 0,
            budget: 0,
            base_cover: u64::MAX,
        }; USER_CAPS_CAPACITY],
    };

    /// The live prefix. `len` crosses a program boundary, so it is clamped
    /// rather than trusted.
    pub fn as_slice(&self) -> &[UserCapV0] {
        &self.caps[..(self.len as usize).min(USER_CAPS_CAPACITY)]
    }

    /// Mark the user at `index` as having no room.
    pub fn exclude(&mut self, index: usize) {
        if index >= USER_SET_CAPACITY {
            return;
        }
        self.excluded[index / 8] |= 1 << (index % 8);
    }

    /// Whether the user at `index` has no room. An index past the set is the
    /// caller disagreeing with itself, and constrains nobody.
    pub fn is_excluded(&self, index: usize) -> bool {
        index < USER_SET_CAPACITY && self.excluded[index / 8] & (1 << (index % 8)) != 0
    }

    /// Whether anyone is excluded — the check that keeps an ordinary walk
    /// from paying for a lookup it never needs.
    pub fn any_excluded(&self) -> bool {
        self.excluded.iter().any(|byte| *byte != 0)
    }

    /// Build from per-user budgets. No room becomes a bitmap bit, which never
    /// overflows; the rest take the scarce slots, tightest first, so an
    /// overflow drops the entries with the most room.
    pub fn from_caps(caps: impl IntoIterator<Item = UserCapV0>) -> Self {
        let mut set = Self::EMPTY;
        let mut partial: [UserCapV0; USER_CAPS_CAPACITY] =
            [UserCapV0::default(); USER_CAPS_CAPACITY];
        let mut partial_len = 0usize;
        for cap in caps {
            // An unbounded budget says nothing and an empty one is a bitmap
            // bit, so neither is worth a slot.
            if cap.budget == 0 {
                set.exclude(cap.index as usize);
                continue;
            }
            // An unbounded budget alone says nothing, but a finite `base_cover`
            // still needs a slot: it is the reduce-only clamp the book cannot
            // reconstruct on its own. A cap unbounded on both is the only one
            // worth no slot.
            if cap.budget == u64::MAX && cap.base_cover == u64::MAX {
                continue;
            }
            // Insertion sort into a fixed array: the list is eight long and
            // this runs on a frame that cannot afford a heap round trip. The
            // tightest budgets are the ones worth a slot, so a full array
            // evicts its loosest.
            let mut slot = partial_len;
            while slot > 0 && partial[slot - 1].budget > cap.budget {
                slot -= 1;
            }
            // A budget that cannot be carried becomes an exclusion rather
            // than a drop. Dropping it would offer the user its whole resting
            // depth, which is the reading the budget exists to correct;
            // excluding it offers none, which costs liquidity and nothing
            // else.
            if partial_len < USER_CAPS_CAPACITY {
                let mut index = partial_len;
                while index > slot {
                    partial[index] = partial[index - 1];
                    index -= 1;
                }
                partial[slot] = cap;
                partial_len += 1;
            } else if slot < USER_CAPS_CAPACITY {
                let evicted = partial[USER_CAPS_CAPACITY - 1];
                let mut index = USER_CAPS_CAPACITY - 1;
                while index > slot {
                    partial[index] = partial[index - 1];
                    index -= 1;
                }
                partial[slot] = cap;
                set.exclude(evicted.index as usize);
            } else {
                set.exclude(cap.index as usize);
            }
        }
        set.caps = partial;
        set.len = partial_len as u8;
        set
    }
}

/// Encoded width of a user set of `len` entries.
///
/// The set a call may settle against is a sequence, not a padded array:
/// `len` refs behind a four-byte count, which is what a borsh sequence
/// writes. Empty means unrestricted, and only a caller that settles nothing
/// (quote discovery) sends that. Otherwise it is the caller's loaded users,
/// and liquidity owned by anyone else must be passed over — the caller cannot
/// settle a balance change for a user it did not load, and refuses the whole
/// response if one appears.
///
/// A padded set cost 1,633 bytes on every call. The caller builds those bytes
/// on a 32 KB heap that never reclaims, once per quoter, so a quote view —
/// which names no users at all — paid the full width for a run of zeros, and
/// a market with five quoters ran out of heap.
pub const fn user_set_bytes(len: usize) -> usize {
    4 + len * UserRefV0::SIZE
}

/// Widest a user set can be on the wire.
pub const USER_SET_MAX_BYTES: usize = user_set_bytes(USER_SET_CAPACITY);

/// Whether a received user set is within what the wire allows.
///
/// A quoter checks this before it reads the set. [`UserCapsV0`] addresses a
/// user by its index in this set, and the exclusion bitmap holds one bit per
/// slot up to [`USER_SET_CAPACITY`]. A user past that has no bit, so an
/// oversized set would present an excluded user as settleable.
pub fn user_set_within_capacity(users: &[UserRefV0]) -> bool {
    users.len() <= USER_SET_CAPACITY
}

/// One resting order behind a quoted book, as `quote_l3_v0` reports it.
///
/// A quoter that holds discrete orders — a book — has more to say than its
/// aggregated ladder: each rung stands on somebody's order, and a caller that
/// has to *carry* those users' accounts, or display the book, needs the
/// attribution. A quoter that has no orders does not implement the leg at
/// all, and its caller attributes the whole ladder to the one user the
/// registry names for it.
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
    /// The other half of the handle, beside the id it completes. An id alone
    /// does not name an order to act on — a cancel or a fill takes both, so a
    /// row carrying only the id described an order its reader could not then
    /// touch. Zero when the quoter keeps no arena, as it is for every quoter
    /// that is not a book.
    pub node_index: u32,
    /// Who this row settles against.
    pub user: UserRefV0,
    /// [`L3_ROW_FLAG_TAKER_ORIGIN`] and its siblings, and room for the next
    /// fact a row has to carry.
    pub flags: u8,
    pub _pad: [u8; 1],
    /// Slot the order was placed in. Its id already orders it against the
    /// other rows, which is what price-time needs; this is elapsed time, which
    /// is what pricing the work of resolving it needs. Zero when the quoter
    /// keeps no such record.
    pub placed_slot: u64,
}

/// The row is an unfilled taker remainder the caller migrated onto the book,
/// not liquidity someone chose to post. It demands liquidity rather than
/// offering it, so depth behind it is not depth a cross can count on.
pub const L3_ROW_FLAG_TAKER_ORIGIN: u8 = 1;

/// This order is big enough to end a fill walk when its owner is absent from
/// the caller's user set, so the depth behind it is unreachable to a caller
/// that does not carry that owner.
///
/// A row without it gates nothing: a caller short of account locks can leave
/// its owner out and still reach everything behind. That is what makes the
/// flag worth carrying — it tells an account-set builder which owners are
/// load-bearing, which is the only reason it needs to know sizes at all.
///
/// The quoter sets it, not the reader. Whatever the rule is — a size floor
/// today — the program that enforces it is the one that reports it, so a
/// reader never reimplements the rule and never falls out of step when it
/// changes. A quoter with one user sets it on every row: its single owner
/// gates all of its depth.
///
/// It says the order *can* end a walk, not that it will. Age also decides:
/// an order inside the book's grace window is passed over whatever its size,
/// and it leaves that window on its own with nothing writing to the book.
pub const L3_ROW_FLAG_BLOCKS_WALK: u8 = 2;

/// The order is reduce-only: it may fill only up to the owner's position in the
/// reduce direction, and its owner carries an authoritative `base_cover` cap.
///
/// A caller that settles a fill against this row must know it is reduce-only,
/// both to bind the fill to the cover its own accounting reserved and to stop
/// tracking the owner's reduce-only exposure once the order leaves the book.
/// The quoter sets it, so the reader never reads the owner's position to guess.
pub const L3_ROW_FLAG_REDUCE_ONLY: u8 = 4;

/// Part of this row's size, or all of it, is claimed by a crossing taker
/// remainder, so the quoter will not sell it to this caller. `size` already
/// has the claim subtracted; the flag says why it is short of what the order
/// holds.
///
/// A book quotes an unfilled taker remainder it was handed like any other
/// resting order, and the remainder claims the depth it crosses so that the
/// improvement reaches the taker rather than whoever lands a transaction
/// first. A claim is transient: it lapses if nothing settles the cross, and
/// the depth is ordinary again.
///
/// A caller that displays depth drops what this marks. A caller that settles
/// the cross reads the same book with `consume_reservation` and sees the whole
/// size.
pub const L3_ROW_FLAG_RESERVED: u8 = 8;

/// Encoded width of an [`L3RowV0`].
pub const L3_ROW_BYTES: usize = core::mem::size_of::<L3RowV0>();

/// The answer to `quote_l3_v0`: the resting orders behind a quoted book, best
/// price first.
///
/// Read in place out of the quoter's response account, like every response
/// here.
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct L3ResponseV0<'a> {
    pub rows: &'a [L3RowV0],
    /// The walk stopped on a bound rather than on the end of the book, so
    /// there is depth behind the last row. A caller displaying a book says
    /// so; a caller collecting users knows its list is a prefix.
    pub more: u8,
}

impl<'a> L3ResponseV0<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, SpecError> {
        wincode::deserialize(bytes).map_err(|_| SpecError::Read)
    }
}

/// Arguments to `quote_l3_v0`: how much of a side to describe.
///
/// No user set and no caps. The question is what rests on the book, not what
/// this caller may settle — a caller asks it precisely because it does not
/// know yet whose accounts to bring — so the filtering a quote applies is the
/// reader's to apply here.
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct L3ArgsV0 {
    pub direction: DirectionV0,
    /// Stop once this much base is described. Zero describes the side up to
    /// `max_rows`.
    pub size: u64,
    /// Stop after this many rows, whatever `size` is left.
    pub max_rows: u16,
    /// Report the depth a crossing taker remainder claims as available. Same
    /// contract as [`QuoteArgsV0::consume_reservation`], on the surface a
    /// caller resolves a cross from.
    pub consume_reservation: bool,
}

/// Arguments to `quote_v0`: what a taker wants, and who the caller can settle
/// against.
///
/// A quote is a promise about what `execute_v0` will deliver, so it is given
/// the same `users` and `caps` and must spend them the same way. A ladder
/// standing on liquidity the matching execute would decline is a ladder its
/// reader cannot route against.
///
/// Read in place, like the responses: `users` is a slice into the caller's
/// instruction data, so a quoter walks the set without allocating and without
/// standing a copy of it in a 4 KB frame. It leads the struct for that to be
/// sound — the count puts the refs four bytes in, which is the two-byte
/// alignment a [`UserRefV0`] reference needs. A field added ahead of it must
/// keep that offset even.
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct QuoteArgsV0<'a> {
    /// The loaded-user set, at most [`USER_SET_CAPACITY`] entries — which a
    /// reader checks with [`user_set_within_capacity`], since the count comes
    /// off the wire.
    pub users: &'a [UserRefV0],
    pub direction: DirectionV0,
    /// Base the taker wants filled.
    pub size: u64,
    pub caps: UserCapsV0,
    /// The price the caller marks a filled position at, in PRICE_PRECISION.
    /// Only [`UserCapV0`] budgets are spent against it; it does not bound
    /// what a quoter may fill at.
    pub reference_price: i64,
    /// The taker's own user, whose resting liquidity is skipped
    /// unconditionally (self-trade prevention).
    pub taker: Option<UserRefV0>,
    /// The worst price the caller will fill at, in PRICE_PRECISION. Zero
    /// means no bound.
    ///
    /// A ladder is walked best price first, so a quoter may stop as soon as a
    /// level is worse than this: the caller discards those levels anyway. The
    /// walk is what a quoter is charged for, and a transaction is charged for
    /// the compute limit it requests, so a bound the quoter ignores is paid
    /// for by whoever sent the transaction.
    ///
    /// **Not a trust boundary**, like the caps above. Honouring it saves the
    /// caller compute; ignoring it wastes the caller's compute and returns
    /// levels the caller drops. It never widens what a quoter may fill.
    pub limit_price: u64,
    /// Whether the taker's flow served a protection window before this call:
    /// the swift hold (an attested transaction), or the book's activation
    /// delay (a protocol crank that fills an order which rested through it).
    /// The caller asserts it, like `users` and `caps` — a quoter already
    /// authenticates the caller, and the caller is the settlement engine. A
    /// quoter that only serves protected flow (the midpoint's
    /// `require_attested_flow`) refuses when this is false.
    pub taker_served_window: bool,
    /// Fill the depth a crossing taker remainder claims.
    ///
    /// A book withholds the depth an unfilled taker remainder crosses, so the
    /// improvement over that remainder's resting price reaches the taker
    /// rather than whoever lands a transaction first. The crank that settles
    /// the cross is the one caller that must reach it, and it says so here.
    ///
    /// The caller asserts it, like `users` and `caps`: the quoter
    /// authenticates the caller, and the caller is the settlement engine. A
    /// quoter that reserves nothing ignores it.
    pub consume_reservation: bool,
}

/// Arguments to `execute_v0`: commit a fill.
///
/// [`QuoteArgsV0`] without the price bound, because it answers the same
/// question having committed to it: `size` is already only the depth the
/// caller chose off the ladder, so the walk that fills it visits no level a
/// bound would have cut. A quoter may fill less than `size`; what it actually
/// filled is whatever its returned balance changes sum to.
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct ExecuteArgsV0<'a> {
    /// Same contract as [`QuoteArgsV0::users`], and it leads for the same
    /// reason.
    pub users: &'a [UserRefV0],
    pub direction: DirectionV0,
    pub size: u64,
    pub caps: UserCapsV0,
    /// The same mark the quote was taken against. A quoter that spends
    /// budgets must be handed the identical price here, or it passes over a
    /// different set of orders than the one it quoted.
    pub reference_price: i64,
    pub taker: Option<UserRefV0>,
    /// Same contract as [`QuoteArgsV0::taker_served_window`]. The execute
    /// must carry the value its quote carried.
    pub taker_served_window: bool,
    /// Same contract as [`QuoteArgsV0::consume_reservation`], and the same
    /// rule: the execute must carry the value its quote carried, or it walks
    /// a different set of orders than the one it quoted.
    pub consume_reservation: bool,
}

/// The framing of the request half.
///
/// Borsh-compatible, which is what a v2 program's instruction dispatch reads
/// (`anchor_lang_v2::BORSH_CONFIG`), and named here so the writer and the
/// reader agree by declaration rather than by coincidence. The responses use
/// wincode's own configuration, whose length prefix is eight bytes wide
/// instead of four — reading one for the other shifts every field behind it.
pub type ArgsConfig = wincode::config::Configuration<
    false,
    { wincode::config::DEFAULT_PREALLOCATION_SIZE_LIMIT },
    wincode::len::FixIntLen<u32>,
    wincode::int_encoding::LittleEndian,
    wincode::int_encoding::FixInt,
    u8,
>;

/// The one value of [`ArgsConfig`].
pub const ARGS_CONFIG: ArgsConfig =
    unsafe { wincode::config::Configuration::new().disable_zero_copy_align_check() };

/// Bytes `args` takes on the wire.
///
/// A caller reserves this before it writes: it builds the buffer on a 32 KB
/// heap that never reclaims, once per quoter per call, and a `Vec` that
/// doubles into place leaks every intermediate buffer.
pub fn args_size<T>(args: &T) -> Result<usize, SpecError>
where
    T: wincode::SchemaWrite<ArgsConfig, Src = T>,
{
    wincode::config::serialized_size(args, ARGS_CONFIG)
        .map(|size| size as usize)
        .map_err(|_| SpecError::Write)
}

/// Append `args` to `dst` in the framing a quoter reads.
pub fn write_args<T>(dst: &mut Vec<u8>, args: &T) -> Result<(), SpecError>
where
    T: wincode::SchemaWrite<ArgsConfig, Src = T>,
{
    wincode::config::serialize_into(dst, args, ARGS_CONFIG).map_err(|_| SpecError::Write)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response region on the 8-byte step a real account gives one. Backed
    /// by `u64`s because the records are cast in place at both ends of the
    /// wire, and a `Vec<u8>` promises no more than byte alignment.
    struct Region(Vec<u64>);

    impl Region {
        fn new(bytes: usize) -> Self {
            Self(vec![0; bytes.div_ceil(LEN_BYTES)])
        }

        fn bytes(&mut self) -> &mut [u8] {
            bytemuck::cast_slice_mut(&mut self.0)
        }
    }

    fn user(seed: u8, sub: u16) -> UserRefV0 {
        UserRefV0 {
            authority: Pubkey::new_from_array([seed; 32]),
            sub_account_id: sub,
        }
    }

    fn changes() -> [UserBalanceChangeV0; 2] {
        [
            UserBalanceChangeV0 {
                base_size: 1_000_000_000,
                quote_size: 101_000_000,
                user: user(7, 3),
                _pad: [0; 6],
            },
            UserBalanceChangeV0 {
                base_size: 5,
                quote_size: 6,
                user: user(8, 0),
                _pad: [0; 6],
            },
        ]
    }

    fn cancelled() -> [CancelledRemainderV0; 1] {
        [CancelledRemainderV0 {
            order_id: 42,
            base_asset_amount: 17,
            price: 101,
            client_order_id: 420,
            user: user(9, 1),
            _pad: [0; 2],
        }]
    }

    fn completed() -> [CompletedOrderV0; 2] {
        [
            CompletedOrderV0 {
                order_id: 9,
                change_index: 0,
                client_order_id: 90,
            },
            CompletedOrderV0 {
                order_id: 10,
                change_index: 0,
                client_order_id: 100,
            },
        ]
    }

    fn partial() -> [PartiallyFilledOrderV0; 1] {
        [PartiallyFilledOrderV0 {
            order_id: 11,
            base_filled: 3,
            client_order_id: 110,
            change_index: 0,
        }]
    }

    #[test]
    fn round_trips_without_copying() {
        let (c, x, d, p) = (changes(), cancelled(), completed(), partial());
        let response = ExecuteResponseV0 {
            changes: &c,
            cancelled: &x,
            completed: &d,
            partial: &p,
        };
        let bytes = wincode::serialize(&response).unwrap();
        let back = ExecuteResponseV0::parse(&bytes).unwrap();
        assert_eq!(back, response);

        // The slices point into the buffer rather than at copies of it.
        let base = bytes.as_ptr() as usize;
        let borrowed = back.changes.as_ptr() as usize;
        assert!(
            borrowed > base && borrowed < base + bytes.len(),
            "changes must borrow from the response buffer"
        );
    }

    #[test]
    fn completed_orders_attach_to_their_change() {
        let (c, x, d, p) = (changes(), cancelled(), completed(), partial());
        let bytes = wincode::serialize(&ExecuteResponseV0 {
            changes: &c,
            cancelled: &x,
            completed: &d,
            partial: &p,
        })
        .unwrap();
        let response = ExecuteResponseV0::parse(&bytes).unwrap();
        assert_eq!(response.completed_for(0).collect::<Vec<_>>(), vec![9, 10]);
        assert_eq!(response.completed_for(1).count(), 0);
    }

    /// An id naming a change that is not there would otherwise unwind whatever
    /// record sits at that index.
    #[test]
    fn a_dangling_completed_order_is_rejected() {
        let (c, x) = (changes(), cancelled());
        let bytes = wincode::serialize(&ExecuteResponseV0 {
            changes: &c,
            cancelled: &x,
            completed: &[CompletedOrderV0 {
                order_id: 1,
                change_index: 9,
                client_order_id: 0,
            }],
            partial: &[],
        })
        .unwrap();
        assert_eq!(
            ExecuteResponseV0::parse(&bytes),
            Err(SpecError::DanglingCompletedOrder)
        );
    }

    #[test]
    fn truncation_is_an_error_not_a_panic() {
        let (c, x, d, p) = (changes(), cancelled(), completed(), partial());
        let bytes = wincode::serialize(&ExecuteResponseV0 {
            changes: &c,
            cancelled: &x,
            completed: &d,
            partial: &p,
        })
        .unwrap();
        for cut in 0..bytes.len() {
            assert!(
                ExecuteResponseV0::parse(&bytes[..cut]).is_err(),
                "truncating to {cut} bytes must be an error"
            );
        }
    }

    #[test]
    fn quote_round_trips() {
        let levels = [
            PriceLevelV0 {
                price: 100_000_000,
                size: 5,
            },
            PriceLevelV0 {
                price: 99_000_000,
                size: 7,
            },
        ];
        let bytes = wincode::serialize(&QuoteResponseV0 {
            levels: &levels,
            withheld: PriceLevelV0::default(),
        })
        .unwrap();
        let response = QuoteResponseV0::parse(&bytes).unwrap();
        assert_eq!(response.levels, levels.as_slice());
    }

    /// The writers and the reader are the two halves of this crate, and this is
    /// what holds them together: bytes a writer produced must equal wincode's
    /// encoding of the same response, and must parse back.
    #[test]
    fn the_execute_writer_writes_what_the_reader_reads() {
        let (c, x, d, p) = (changes(), cancelled(), completed(), partial());
        let mut region = Region::new(512);

        let mut writer = ExecuteWriter::new();
        // Streamed the way a fill produces them: two changes appended, then
        // the first one found again and added into, as a repeat maker does.
        let first = writer.push_change(region.bytes(), c[0]).unwrap();
        writer.push_change(region.bytes(), c[1]).unwrap();
        let found = writer
            .changes(region.bytes())
            .unwrap()
            .iter()
            .position(|change| change.user == c[0].user)
            .unwrap();
        assert_eq!(found as u32, first);
        let record = writer.change_mut(region.bytes(), first).unwrap();
        record.base_size += 3;
        record.quote_size += 4;
        let len = writer.finish(region.bytes(), &x, &d, &p).unwrap();

        let mut merged = c;
        merged[0].base_size += 3;
        merged[0].quote_size += 4;
        let expected = ExecuteResponseV0 {
            changes: &merged,
            cancelled: &x,
            completed: &d,
            partial: &p,
        };
        assert_eq!(
            &region.bytes()[..len],
            wincode::serialize(&expected).unwrap().as_slice()
        );
        assert_eq!(
            ExecuteResponseV0::parse(&region.bytes()[..len]).unwrap(),
            expected
        );
    }

    #[test]
    fn the_quote_writer_writes_what_the_reader_reads() {
        let levels = [
            PriceLevelV0 {
                price: 100,
                size: 5,
            },
            PriceLevelV0 { price: 99, size: 7 },
        ];
        let withheld = PriceLevelV0 {
            price: 98,
            size: 11,
        };
        let mut region = Region::new(256);

        let mut writer = QuoteWriter::new();
        for level in levels {
            writer.push_level(region.bytes(), level).unwrap();
        }
        assert_eq!(writer.levels(), levels.len());
        let len = writer.finish(region.bytes(), withheld).unwrap();

        assert_eq!(
            &region.bytes()[..len],
            wincode::serialize(&QuoteResponseV0 {
                levels: &levels,
                withheld,
            })
            .unwrap()
            .as_slice()
        );
        let response = QuoteResponseV0::parse(&region.bytes()[..len]).unwrap();
        assert_eq!(response.levels, levels.as_slice());
        assert_eq!(response.withheld, withheld);
    }

    /// The writer holds the bound the reader checks, so a quoter fails on its
    /// own bug instead of on the router's rejection of the response.
    #[test]
    fn a_dangling_completed_order_is_refused_at_write_time() {
        let mut region = Region::new(256);
        let mut writer = ExecuteWriter::new();
        writer.push_change(region.bytes(), changes()[0]).unwrap();
        assert_eq!(
            writer.finish(
                region.bytes(),
                &[],
                &[CompletedOrderV0 {
                    order_id: 1,
                    change_index: 1,
                    client_order_id: 0,
                }],
                &[],
            ),
            Err(SpecError::DanglingCompletedOrder)
        );
    }

    /// A region the records cannot be read at is refused rather than written.
    /// The response would be unreadable, and its reader is a CPI away.
    #[test]
    fn a_misaligned_region_is_an_error_not_a_panic() {
        let mut region = Region::new(256);
        let skewed = &mut region.bytes()[1..];
        let mut writer = QuoteWriter::new();
        assert_eq!(
            writer.push_level(skewed, PriceLevelV0 { price: 1, size: 2 }),
            Err(SpecError::RegionMisaligned)
        );
        // And a response with no records at all is refused too, on the report
        // behind the empty ladder.
        assert_eq!(
            QuoteWriter::new().finish(skewed, PriceLevelV0::default()),
            Err(SpecError::RegionMisaligned)
        );
    }

    /// A region too small fails on the record that does not fit, and never
    /// writes past its end.
    #[test]
    fn a_full_region_stops_the_writer() {
        let mut region = Region::new(LEN_BYTES + 2 * PRICE_LEVEL_BYTES);
        // Room for the prefix and one rung, and nothing after them.
        let bytes = &mut region.bytes()[..LEN_BYTES + PRICE_LEVEL_BYTES];
        let mut writer = QuoteWriter::new();
        writer
            .push_level(bytes, PriceLevelV0 { price: 1, size: 2 })
            .unwrap();
        assert_eq!(
            writer.push_level(bytes, PriceLevelV0 { price: 3, size: 4 }),
            Err(SpecError::RegionTooSmall)
        );
        // And the withheld report has nowhere to go either.
        assert_eq!(
            writer.finish(bytes, PriceLevelV0::default()),
            Err(SpecError::RegionTooSmall)
        );
    }

    /// The writers lay the prefix down themselves, so what this crate says it
    /// is has to be what wincode actually writes.
    /// The request half is read in place, and its framing is not the
    /// responses': a four-byte count, so a quoter's dispatch decodes it
    /// borsh-compatibly. The offsets are pinned here because velocity writes
    /// these bytes from another workspace, where the agreement can only be
    /// held as numbers.
    #[test]
    fn the_args_put_the_user_set_first_and_count_it_in_four_bytes() {
        let users = [user(1, 0), user(2, 7)];
        let args = QuoteArgsV0 {
            users: &users,
            direction: DirectionV0::Long,
            size: 12,
            caps: UserCapsV0::EMPTY,
            reference_price: -5,
            taker: Some(user(3, 1)),
            limit_price: 0,
            taker_served_window: true,
            consume_reservation: false,
        };
        let bytes = wincode::config::serialize(&args, ARGS_CONFIG).unwrap();

        assert_eq!(&bytes[..4], &2u32.to_le_bytes());
        assert_eq!(&bytes[4..4 + UserRefV0::SIZE], &user(1, 0).to_bytes());
        let after_set = user_set_bytes(users.len());
        assert_eq!(bytes[after_set], 0, "Long is the zero discriminant");
        assert_eq!(
            &bytes[after_set + 1..after_set + 9],
            &12u64.to_le_bytes(),
            "size follows the direction"
        );
        assert_eq!(bytes.len(), args_size(&args).unwrap());
        assert_eq!(
            bytes.len(),
            after_set + 1 + 8 + USER_CAPS_BYTES + 8 + 1 + UserRefV0::SIZE + 8 + 1 + 1
        );

        // And it reads back as a slice into those bytes, not a copy of them.
        let read: QuoteArgsV0 = wincode::config::deserialize(&bytes, ARGS_CONFIG).unwrap();
        assert_eq!(read, args);
        assert_eq!(read.users.as_ptr() as usize, bytes[4..].as_ptr() as usize);
    }

    /// An empty set is what a quote view sends, and it is the case the heap
    /// cared about.
    #[test]
    fn an_unrestricted_set_costs_four_bytes() {
        let args = ExecuteArgsV0 {
            users: &[],
            direction: DirectionV0::Short,
            size: 1,
            caps: UserCapsV0::EMPTY,
            reference_price: 0,
            taker: None,
            taker_served_window: false,
            consume_reservation: false,
        };
        let bytes = wincode::config::serialize(&args, ARGS_CONFIG).unwrap();
        assert_eq!(&bytes[..4], &0u32.to_le_bytes());
        assert_eq!(bytes.len(), 4 + 1 + 8 + USER_CAPS_BYTES + 8 + 1 + 1 + 1);
        assert_eq!(bytes.len(), args_size(&args).unwrap());

        let read: ExecuteArgsV0 = wincode::config::deserialize(&bytes, ARGS_CONFIG).unwrap();
        assert!(read.users.is_empty());
        assert!(user_set_within_capacity(read.users));
    }

    #[test]
    fn a_set_past_the_capacity_is_refused_by_the_check_a_reader_owes_it() {
        let full = vec![user(1, 0); USER_SET_CAPACITY];
        assert!(user_set_within_capacity(&full));
        assert_eq!(user_set_bytes(full.len()), USER_SET_MAX_BYTES);
        let over = vec![user(1, 0); USER_SET_CAPACITY + 1];
        assert!(!user_set_within_capacity(&over));
    }

    /// The L3 leg is the one place a quoter says who is behind its ladder, so
    /// a row round-trips whole and the reader borrows the rows in place.
    #[test]
    fn an_l3_response_round_trips_through_its_writer() {
        let rows = [
            L3RowV0 {
                price: 100,
                size: 5,
                order_id: 7,
                node_index: 7,
                user: user(1, 0),
                flags: L3_ROW_FLAG_TAKER_ORIGIN,
                _pad: [0; 1],
                placed_slot: 7,
            },
            L3RowV0 {
                price: 101,
                size: 6,
                order_id: 8,
                node_index: 8,
                user: user(2, 3),
                flags: 0,
                _pad: [0; 1],
                placed_slot: 8,
            },
        ];

        let mut region = Region::new(LEN_BYTES + rows.len() * L3_ROW_BYTES + 1);
        let bytes = region.bytes();
        let mut writer = L3Writer::new();
        for row in rows {
            writer.push_row(bytes, row).unwrap();
        }
        assert_eq!(writer.rows(), 2);
        let len = writer.finish(bytes, true).unwrap();
        assert_eq!(len, LEN_BYTES + 2 * L3_ROW_BYTES + 1);

        let response = L3ResponseV0::parse(&bytes[..len]).unwrap();
        assert_eq!(response.rows, &rows[..]);
        assert_eq!(response.more, 1, "the walk stopped on a bound");
        assert_eq!(
            response.rows.as_ptr() as usize,
            bytes[LEN_BYTES..].as_ptr() as usize,
            "rows are read in place"
        );
    }

    /// A row is 64 bytes with no implicit padding — what `Pod` and the
    /// in-place read both need.
    #[test]
    fn the_l3_row_is_the_width_the_region_is_sized_from() {
        assert_eq!(L3_ROW_BYTES, 72);
        assert_eq!(L3_ROW_BYTES, 3 * 8 + 4 + UserRefV0::SIZE + 1 + 1 + 8);
    }

    #[test]
    fn the_length_prefix_is_what_wincode_writes() {
        let levels = [
            PriceLevelV0 { price: 1, size: 2 },
            PriceLevelV0 { price: 3, size: 4 },
        ];
        let bytes = wincode::serialize(&QuoteResponseV0 {
            levels: &levels,
            withheld: PriceLevelV0::default(),
        })
        .unwrap();
        assert_eq!(&bytes[..LEN_BYTES], &len_prefix(levels.len()));
        // The ladder, then the withheld report behind it.
        assert_eq!(
            bytes.len(),
            LEN_BYTES + levels.len() * PRICE_LEVEL_BYTES + 2 * 8
        );

        // And the report round trips from that tail.
        let withheld = PriceLevelV0 { price: 7, size: 9 };
        let bytes = wincode::serialize(&QuoteResponseV0 {
            levels: &levels,
            withheld,
        })
        .unwrap();
        let response = QuoteResponseV0::parse(&bytes).unwrap();
        assert_eq!(response.levels, levels.as_slice());
        assert_eq!(response.withheld, withheld);
    }

    /// Nine constrained users against eight slots. The eight tightest keep
    /// their exact number; the ninth is excluded rather than dropped, because
    /// dropping it would read as "unconstrained" — the opposite of what its
    /// budget says.
    #[test]
    fn a_budget_that_does_not_fit_becomes_an_exclusion() {
        let caps = (0..9).map(|index| UserCapV0 {
            index,
            budget: 1_000 - index as u64,
            base_cover: u64::MAX,
        });
        let set = UserCapsV0::from_caps(caps);

        assert_eq!(set.len as usize, USER_CAPS_CAPACITY);
        // Index 0 has the loosest budget of the nine, so it is the one evicted.
        assert!(set.is_excluded(0));
        for index in 1..9 {
            assert!(!set.is_excluded(index), "index {index}");
            assert_eq!(
                set.as_slice()
                    .iter()
                    .find(|cap| cap.index == index as u8)
                    .map(|cap| cap.budget),
                Some(1_000 - index as u64)
            );
        }
    }

    /// No room never spends a slot. It is the whole point of the bitmap, and
    /// it leaves the eight for users who can still fill.
    #[test]
    fn no_room_costs_no_slot() {
        let set = UserCapsV0::from_caps((0..20).map(|index| UserCapV0 {
            index,
            budget: 0,
            base_cover: u64::MAX,
        }));

        assert_eq!(set.len, 0);
        for index in 0..20 {
            assert!(set.is_excluded(index));
        }
    }

    /// An unbounded budget is the same as saying nothing, so it costs neither
    /// a slot nor a bit.
    #[test]
    fn an_unbounded_budget_is_not_carried() {
        let set = UserCapsV0::from_caps((0..20).map(|index| UserCapV0 {
            index,
            budget: u64::MAX,
            base_cover: u64::MAX,
        }));

        assert_eq!(set.len, 0);
        assert!(!set.any_excluded());
    }

    /// The record layout is the wire. Pin the offsets so a reordered field
    /// fails here rather than redefining what the other program reads.
    #[test]
    fn layout_is_pinned() {
        let change = UserBalanceChangeV0 {
            base_size: 0x0807_0605_0403_0201,
            quote_size: 0x1817_1615_1413_1211,
            user: user(0xAB, 0x0201),
            _pad: [0; 6],
        };
        let bytes = wincode::serialize(&change).unwrap();
        assert_eq!(&bytes[0..8], &0x0807_0605_0403_0201u64.to_le_bytes());
        assert_eq!(&bytes[8..16], &0x1817_1615_1413_1211u64.to_le_bytes());
        assert_eq!(&bytes[16..48], &[0xABu8; 32]);
        assert_eq!(&bytes[48..50], &[0x01, 0x02]);
        assert_eq!(bytes.len(), CHANGE_BYTES);
    }
}
