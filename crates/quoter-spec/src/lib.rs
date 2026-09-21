// Wire format for velocity's quoter interface. It is the contract between
// velocity (the router) and any program registered as a quoter: the CLOB, the
// midpoint, and third-party PropAMMs.
//
// # One declaration, three programs
//
// This crate is the whole interface. It holds the arguments a quoter is called
// with ([`QuoteArgsV0`], [`ExecuteArgsV0`] and the types they carry) and the
// responses it must produce. Reading it should be enough to implement one.
//
// `quote_v0` and `execute_v0` answer across a program boundary in bytes.
// Velocity writes the arguments and reads the responses. The quoter does the
// reverse. One declaration per program pins nothing against the other
// declarations. A field added on one side and forgotten on another then gives
// two self-consistent programs that disagree about the bytes between them. The
// disagreement lands on a value transfer, where a misread `base_size` moves
// the wrong amount of a user's collateral. The types live here and all three
// programs use them.
//
// # The signer a quoter is called with is shared
//
// Velocity signs `quote_v0` and `execute_v0` as the market's quoter-slab PDA.
// That key is the same for every quoter approved on the market, and a CPI
// callee inherits the signer status of what it was handed. So a quoter holds,
// live inside its own call, the key velocity authenticates with at every
// other quoter on that market.
//
// The presence of the key does not mean velocity is the immediate caller.
// Another quoter on the same market may forward it. That is the one place
// this interface departs from a dedicated authority, where the inference
// would be sound.
//
// The forwarding is useless because velocity refuses to approve a quoter whose
// registered account list names another approved quoter's response account. It
// applies that check in both directions and across the whole market. A
// forwarded call therefore cannot name the account its callee needs, as long
// as the callee needs it.
//
// So the rule for an implementer is to keep any authority gated on this key on
// the account the quoter writes its response to. An instruction cannot then
// read the authority without taking the response account, and no other quoter
// on the market can name it. Velocity's own quoters are built this way. The
// book stores `place_authority` on its market account and the midpoint stores
// `execute_authority` on its quoter account, and each writes its response
// there. An implementer who stores the authority elsewhere can gate an
// instruction on this key without naming the response account, and another
// quoter on the same market can then complete that call.
//
// The key is derived per market, so it authenticates nothing at a quoter
// registered on a different market.
//
// # The responses are read in place
//
// A response is plain data in the quoter's account, and velocity reads it
// there: fixed-width records, little-endian, no length-prefixed nesting and no
// deserialization step. Compute units are not the only reason. Velocity's heap
// is 32 KB and never reclaims, and one fill CPIs every registered quoter
// twice. A response that decodes into `Vec`s spends heap per quoter per fill
// that nothing returns.
//
// Every record is `#[repr(C)]` and free of implicit padding, which is what
// both `bytemuck::Pod` and wincode's zero-copy rules require. The `Pod`
// derives below enforce it. A field reordered into a layout with a padding
// hole stops compiling rather than changing the wire. Field order is therefore
// part of the contract. The `u64` fields lead so the 34-byte [`UserRefV0`]
// cannot push one out of alignment, and each record carries explicit tail
// padding to a multiple of its alignment.
//
// # Framing
//
// Each section of a response is a count followed by that many fixed-width
// records. The sections are contiguous and come in declaration order. The
// caller owns anything past the last one.
//
// A quoter streams a response rather than serializing one. The records come
// out of a book walk that does not know a section's count until the walk ends.
// The streaming writers are [`QuoteWriter`] and [`ExecuteWriter`], and they
// live here for the reason the records do. A section a quoter forgets to write
// is the same disagreement as a field read at the wrong offset. The sections
// are `finish`'s parameters, so a new one stops every quoter compiling.
//
// An execute response is [`UserBalanceChangeV0`], then
// [`CancelledRemainderV0`], then [`CompletedOrderV0`], then
// [`PartiallyFilledOrderV0`]. Completed order ids are their own section rather
// than a list inside each change. A quoter aggregates repeated fills into one
// record per user as it goes, so ids for a user arrive interleaved with other
// users' fills. Naming the change from the id makes appending one an O(1)
// write at the tail instead of a shift of everything after it.
//
// # Addresses
//
// Velocity names the address type `Pubkey` and the v2 programs name it
// `Address`. It is one type, because solana-pubkey re-exports `Address as
// Pubkey` and solana-address 1.x is a shim over 2.x. It is spelled `Pubkey`
// here because anchor's IDL derive recognizes it by that token rather than by
// the type it resolves to, and velocity is the consumer that runs
// `anchor idl build`.

// The v2 IdlType derive emits `anchor_lang::`. Point it at the fork when the
// v2 IDL build is on. Inert (feature undefined) in the v1 crate.
#[cfg(feature = "idl-build-v2")]
extern crate anchor_lang_v2 as anchor_lang;

// Re-exported so a consumer can write a response without taking its own
// wincode dependency. The framing is this crate's to define, so the encoder is
// too.
pub use wincode;
pub mod write;
pub use write::{ExecuteWriter, L3Writer, QuoteWriter};
use {
    bytemuck::{Pod, Zeroable},
    core::mem::MaybeUninit,
    solana_address::Address as Pubkey,
    wincode::{
        config::{ConfigCore, ZeroCopy},
        error::{ReadResult, WriteResult},
        io::{Reader, Writer},
        SchemaRead, SchemaWrite, TypeMeta,
    },
};

/// The all-zero key. A quoter uses it to mean "no key set", which its own
/// validation then refuses where a real key is required.
pub const ZERO_ADDRESS: Pubkey = Pubkey::new_from_array([0u8; 32]);

/// A velocity user in derivable form: the wallet and sub-account index that both
/// the `User` and `UserStats` PDAs derive from. An off-chain reader can reach every
/// user-derived account from a quoter's state alone. A stored `User` key cannot,
/// because its authority lives in account data the reader cannot load.
#[repr(C)]
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Pod, Zeroable)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct UserRefV0 {
    pub authority: Pubkey,
    pub sub_account_id: u16,
}

// The wincode schema below is written out rather than derived. The derive needs
// `Address` to carry wincode's own traits, which it does only from
// solana-address 2.7. Litesvm and the agave RPC crates both hold this crate's
// trees to 2.6. The key is 32 opaque bytes on the wire either way, so the
// schema delegates to `[u8; 32]` and `u16` and produces the same bytes the
// derive would. `the_user_ref_schema_matches_its_byte_form` pins that.

/// `Address` is a newtype over `[u8; 32]`, so the key's schema is the array's.
type AuthoritySchema = [u8; 32];

/// The shape the derive computes for a `#[repr(C)]` struct: the fields' sizes
/// summed, zero-copy when every field is zero-copy and the sum leaves no
/// padding. `u16` is dynamic under a varint integer encoding, and this follows
/// it there.
const fn user_ref_type_meta(fields: [TypeMeta; 2]) -> TypeMeta {
    match TypeMeta::join_types(fields) {
        TypeMeta::Static {
            size, zero_copy, ..
        } => TypeMeta::Static {
            size,
            zero_copy: zero_copy && size == core::mem::size_of::<UserRefV0>(),
        },
        TypeMeta::Dynamic => TypeMeta::Dynamic,
    }
}

// SAFETY: `write` emits the key's 32 bytes then the sub-account index, which is
// what `TYPE_META` sizes. `TYPE_META` claims zero-copy only when both fields are
// zero-copy and their sizes sum to `size_of::<UserRefV0>()`, which is the
// derive's own test for padding.
unsafe impl<C: ConfigCore> SchemaWrite<C> for UserRefV0 {
    type Src = Self;

    const TYPE_META: TypeMeta = user_ref_type_meta([
        <AuthoritySchema as SchemaWrite<C>>::TYPE_META,
        <u16 as SchemaWrite<C>>::TYPE_META,
    ]);

    fn size_of(src: &Self::Src) -> WriteResult<usize> {
        if let TypeMeta::Static { size, .. } = <Self as SchemaWrite<C>>::TYPE_META {
            return Ok(size);
        }

        Ok(
            <AuthoritySchema as SchemaWrite<C>>::size_of(src.authority.as_array())?
                + <u16 as SchemaWrite<C>>::size_of(&src.sub_account_id)?,
        )
    }

    fn write(mut writer: impl Writer, src: &Self::Src) -> WriteResult<()> {
        <AuthoritySchema as SchemaWrite<C>>::write(
            Writer::by_ref(&mut writer),
            src.authority.as_array(),
        )?;

        <u16 as SchemaWrite<C>>::write(writer, &src.sub_account_id)
    }
}

// SAFETY: `read` consumes the same two fields `write` emits, in the same order,
// and initializes `dst` only on success.
unsafe impl<'de, C: ConfigCore> SchemaRead<'de, C> for UserRefV0 {
    type Dst = Self;

    const TYPE_META: TypeMeta = user_ref_type_meta([
        <AuthoritySchema as SchemaRead<'de, C>>::TYPE_META,
        <u16 as SchemaRead<'de, C>>::TYPE_META,
    ]);

    fn read(mut reader: impl Reader<'de>, dst: &mut MaybeUninit<Self::Dst>) -> ReadResult<()> {
        let authority = <AuthoritySchema as SchemaRead<'de, C>>::get(Reader::by_ref(&mut reader))?;
        let sub_account_id = <u16 as SchemaRead<'de, C>>::get(reader)?;
        dst.write(Self {
            authority: Pubkey::new_from_array(authority),
            sub_account_id,
        });

        Ok(())
    }
}

// SAFETY: the struct is `Pod`, so it has no invalid bit patterns, and its two
// fields are themselves zero-copy under any configuration that makes the
// integer encoding zero-copy. This is the same impl the derive emits for a
// `#[repr(C)]` struct whose fields are all zero-copy.
unsafe impl<C: ConfigCore> ZeroCopy<C> for UserRefV0 where u16: ZeroCopy<C> {}

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

/// One user's share of an executed fill. The sign follows the taker's direction,
/// not this user's. `base_size` is subtracted when the taker went long and added
/// when it went short. `quote_size` moves the opposite way.
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

/// One order a quoter removed as a sub-minimum remainder of a fill.
///
/// A completed order was consumed. This one was removed because what remained
/// of it fell under the market's minimum. Both unwind the maker's aggregates,
/// but only this one carries a size to release.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Pod, Zeroable, SchemaRead, SchemaWrite)]
#[wincode(assert_zero_copy)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct CancelledRemainderV0 {
    pub order_id: u64,
    pub base_asset_amount: u64,
    /// The price the removed order was resting at. The caller reports this one
    /// removal itself, and there is at most one per execute.
    pub price: u64,
    /// The caller's own id for this order, carried back so the removal lands
    /// on the order the caller knows. See [`CompletedOrderV0::client_order_id`].
    pub client_order_id: u32,
    pub user: UserRefV0,
    pub _pad: [u8; 2],
}

/// One resting order a fill fully consumed, naming the balance change it belongs
/// to by index. The reader decrements that user's open-order count and releases any
/// per-order state, so an id for an order still on the book releases live state.
/// Velocity holds a change merged from N orders to N roundings, not one.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Pod, Zeroable, SchemaRead, SchemaWrite)]
#[wincode(assert_zero_copy)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct CompletedOrderV0 {
    pub order_id: u64,
    /// Which entry of [`ExecuteResponseV0::changes`] this order belongs to. Changes
    /// merge by user, so three consumed orders of one maker point at one change. An
    /// index rather than a 34-byte [`UserRefV0`]. [`ExecuteResponseV0::parse`] refuses
    /// an index past the end, which would unwind another user's live margin.
    pub change_index: u32,
    /// The caller's own id for this order, minted when it asked for the placement.
    /// Reporting it lets the caller close its record without holding a map between
    /// the two id spaces. Zero when the caller supplied none.
    pub client_order_id: u32,
}

/// The one order a fill left resting with less size than it found. A balance change
/// merges every order of one maker, so a caller holding per-order state has nothing
/// to apply a partial fill to. At most one exists per execute, because only the last
/// order a best-first walk reached can be partial. `base_filled` is this fill's
/// contribution, not the order's lifetime total.
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

/// Bytes wincode spends on a slice's length prefix. A quoter that streams records
/// writes this prefix itself and backfills the count at the end. The encoding is
/// wincode's, and [`tests::the_length_prefix_is_what_wincode_writes`] pins them
/// together.
pub const LEN_BYTES: usize = 8;

/// The length prefix wincode writes ahead of a slice of `count` records.
#[inline]
pub fn len_prefix(count: usize) -> [u8; LEN_BYTES] {
    (count as u64).to_le_bytes()
}

const _: () = {
    // The records are the wire. A field reordered or widened must fail here
    // rather than change what the other program reads.
    assert!(USER_REF_BYTES == 34);
    assert!(CHANGE_BYTES == 56);
    assert!(CANCELLED_BYTES == 64);
    assert!(COMPLETED_BYTES == 16);
    assert!(PARTIAL_BYTES == 24);
    assert!(PRICE_LEVEL_BYTES == 16);
};

/// What `execute_v0` answers: every balance change the fill produced, every sub-min
/// remainder it removed, and every resting order it consumed. The fields borrow
/// straight out of the quoter's account, so reading one allocates nothing.
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
    /// Read a response out of `bytes`. Validates what makes the bytes readable, and
    /// that every completed order names a balance change that exists. An index past
    /// the end would unwind a live order's margin. What the numbers mean stays the
    /// caller's to check.
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
    /// no single order and this is `None`. It names one when the fill consumed
    /// one order and left nothing resting, or left one order resting and
    /// consumed none. The change and the order are then the same event, and a
    /// reader can file the fill under that order.
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
    /// The best price this quoter could have offered but did not, and the base resting
    /// there, because that liquidity belongs to a user the caller did not load. Zeroed
    /// when nothing was left out. A caller that skipped this and filled elsewhere at a
    /// worse price routed around a competitor rather than running out of room.
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

/// Whether every completed order names a balance change that exists. Both halves of
/// the wire hold it. [`ExecuteResponseV0::parse`] refuses a response that breaks it,
/// and [`ExecuteWriter::finish`] refuses to write one.
pub(crate) fn completed_orders_fit(completed: &[CompletedOrderV0], changes: usize) -> bool {
    completed
        .iter()
        .all(|entry| (entry.change_index as usize) < changes)
}

/// Whether the partial-fill section is one a fill could have produced: at most one
/// record, naming a balance change that exists. A second partial would mean the walk
/// continued past an order it did not finish.
pub(crate) fn partial_orders_fit(partial: &[PartiallyFilledOrderV0], changes: usize) -> bool {
    partial.len() <= 1
        && partial
            .iter()
            .all(|entry| (entry.change_index as usize) < changes)
}

/// Most users a call may name. The bound is the account-lock limit of the transaction
/// that carries the set, and what [`UserCapsV0`] can address with one bit per slot.
pub const USER_SET_CAPACITY: usize = 48;

/// Users that can carry a partial cap on one call. Only a partial cap needs a slot. A
/// user with no room rides [`UserCapsV0::excluded`], one bit each, so every user in the
/// set can be excluded at once. The slots are left for the narrow band of users with
/// room for some of what they rest.
pub const USER_CAPS_CAPACITY: usize = 8;

/// Bytes of bitmap for one bit per user in the set.
pub const USER_EXCLUSION_BITMAP_BYTES: usize = USER_SET_CAPACITY.div_ceil(8);

/// Taker direction, from the taker's perspective.
///
/// Encoded as its discriminant, `Long = 0`, and every program on this wire
/// reads the same declaration. A taker direction inverted across the boundary
/// would fill the wrong side of a book.
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

    /// The wire tag, which is also the byte an event record carries.
    pub const fn tag(self) -> u8 {
        match self {
            DirectionV0::Long => 0,
            DirectionV0::Short => 1,
        }
    }

    /// Whether a level at `price` is past the caller's worst acceptable price.
    /// Zero is no bound. A level exactly at the limit is acceptable, so the
    /// comparison is strict.
    pub fn worse_than_limit(self, price: u64, limit_price: u64) -> bool {
        limit_price != 0
            && match self {
                DirectionV0::Long => price > limit_price,
                DirectionV0::Short => price < limit_price,
            }
    }
}

/// Where in the quoter's response account it wrote the response. Return data of
/// `quote_v0` and `execute_v0`. Every quoter answers with this one shape, declared here
/// rather than per program, because three copies of two `u32`s are three chances to
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

impl ResponsePointerV0 {
    /// Point at the `len` bytes a quoter streamed at `offset`. The offset is
    /// the quoter's own, because each account puts its response region
    /// somewhere different.
    pub fn at(offset: usize, len: usize) -> Self {
        Self {
            offset: offset as u32,
            len: len as u32,
        }
    }
}

/// Which sides a `cancel_all_v0` withdraws. Named sides rather than a pair of bools,
/// because the wire must not express "neither" and leave a maker believing its quotes
/// are gone. What a side means differs by reader. A book walks it as a book side and a
/// caller unwinds it as a position direction. The tags are the part that has to agree.
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

impl CancelSidesV0 {
    /// The wire tag, which is also the byte an event record carries.
    pub const fn tag(self) -> u8 {
        match self {
            CancelSidesV0::Bids => 0,
            CancelSidesV0::Asks => 1,
            CancelSidesV0::Both => 2,
        }
    }

    pub const fn has_bids(self) -> bool {
        matches!(self, Self::Bids | Self::Both)
    }

    pub const fn has_asks(self) -> bool {
        matches!(self, Self::Asks | Self::Both)
    }
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

impl SideV0 {
    /// The wire tag, which is also the byte an event record carries and the
    /// index of this side's per-side arrays in the book.
    pub const fn tag(self) -> u8 {
        match self {
            SideV0::Bid => 0,
            SideV0::Ask => 1,
        }
    }
}

/// Base units in one whole base asset. It is the denominator that turns a base
/// amount and a price difference into a quote amount, and the reason every
/// quoter spends a cap the same way.
pub const BASE_PRECISION: u64 = 1_000_000_000;

/// What one named user may still lose on the side this call sweeps, in quote.
///
/// A quote amount, not base, because the conversion needs the price each order fills
/// at, which the quoter has and the caller does not. The quoter charges
/// `base * |price - reference_price| / BASE_PRECISION` where the fill moves against the
/// owner. `u64::MAX` is unbounded, and zero belongs in the exclusion bitmap.
#[repr(C)]
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct UserCapV0 {
    /// Quote this user may lose filling on the swept side. `u64::MAX` is unbounded.
    /// This bounds depth that was margin-reserved at placement, so filling it moves
    /// collateral rather than the worst case. Base cannot express that bound, because
    /// at a price in the owner's favour the cost per base is zero.
    pub quote_cap: u64,
    /// The most base this user may give up on the swept side. `u64::MAX` is unbounded.
    /// A book is position-blind, so it enforces this at match time and will not fill a
    /// reduce-only order whose owner carries no cap here. That makes the field a trust
    /// boundary, unlike [`Self::quote_cap`]. Every other quoter reads it as advisory.
    pub base_cap: u64,
    /// Index into the accompanying user set.
    pub index: u8,
}

/// Per-user room, parallel to the caller's user set. A user absent from all of this is
/// unconstrained. The quoter skips an excluded user's orders, because depth standing on
/// them would be depth the fill declines. An exclusion never fails the call, unlike
/// absence from the user set.
///
/// Not a trust boundary. A caller's own post-fill checks still refuse the fill.
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
    /// Live entries at the head of `caps`. The tail is undefined.
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
            quote_cap: 0,
            base_cap: u64::MAX,
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

    /// Whether anyone is excluded. An ordinary walk reads this first so it
    /// never pays for a lookup it does not need.
    pub fn any_excluded(&self) -> bool {
        self.excluded.iter().any(|byte| *byte != 0)
    }

    /// Build from per-user budgets. No room becomes a bitmap bit, which never
    /// overflows. The rest take the scarce slots, tightest first, so an
    /// overflow drops the entries with the most room.
    pub fn from_caps(caps: impl IntoIterator<Item = UserCapV0>) -> Self {
        let mut set = Self::EMPTY;
        let mut partial: [UserCapV0; USER_CAPS_CAPACITY] =
            [UserCapV0::default(); USER_CAPS_CAPACITY];
        let mut partial_len = 0usize;
        for cap in caps {
            // No room at all is a bitmap bit, which costs no slot.
            if cap.quote_cap == 0 {
                set.exclude(cap.index as usize);
                continue;
            }

            // An unbounded quote cap alone says nothing, but `base_cap` still needs a
            // slot. It is the reduce-only clamp the book cannot reconstruct. A cap
            // unbounded on both is the only one worth no slot.
            if cap.quote_cap == u64::MAX && cap.base_cap == u64::MAX {
                continue;
            }

            // Insertion sort into a fixed array. The list is eight long and
            // this runs on a frame that cannot afford a heap round trip. The
            // tightest budgets are the ones worth a slot, so a full array
            // evicts its loosest.
            let mut slot = partial_len;
            while slot > 0 && partial[slot - 1].quote_cap > cap.quote_cap {
                slot -= 1;
            }

            // A cap that cannot be carried becomes an exclusion rather than a
            // drop. A drop would offer the user its whole resting depth, which
            // is the reading the cap exists to correct. An exclusion offers
            // none, which costs liquidity and nothing else.
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

/// Encoded width of a user set of `len` entries: `len` refs behind a four-byte count,
/// which is what a borsh sequence writes. Empty means unrestricted. A padded set cost
/// 1,633 bytes per call on a 32 KB heap that never reclaims, and a market with five
/// quoters ran out.
pub const fn user_set_bytes(len: usize) -> usize {
    4 + len * UserRefV0::SIZE
}

/// Widest a user set can be on the wire.
pub const USER_SET_MAX_BYTES: usize = user_set_bytes(USER_SET_CAPACITY);

/// Whether a received user set is within what the wire allows. [`UserCapsV0`] addresses
/// a user by index and the bitmap holds one bit per slot up to [`USER_SET_CAPACITY`],
/// so an oversized set would present an excluded user as settleable.
pub fn user_set_within_capacity(users: &[UserRefV0]) -> bool {
    users.len() <= USER_SET_CAPACITY
}

/// One resting order behind a quoted book, as `quote_l3_v0` reports it. A quoter that
/// holds discrete orders has more to say than its aggregated ladder, and a caller that
/// must carry those users' accounts or draw the book needs the attribution. A quoter
/// with no orders does not implement the leg.
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
    /// The other half of the handle, beside the id it completes. A cancel or a fill
    /// takes both, so a row carrying only the id names an order its reader cannot act
    /// on. Zero when the quoter keeps no arena, which is every non-book quoter.
    pub node_index: u32,
    /// Who this row settles against.
    pub user: UserRefV0,
    /// [`L3_ROW_FLAG_TAKER_ORIGIN`] and its siblings, and room for the next
    /// fact a row has to carry.
    pub flags: u8,
    pub _pad: [u8; 1],
    /// Slot the order was placed in. Its id already orders it against the other
    /// rows, which is what price-time needs. This is elapsed time, which is what
    /// pricing the work of resolving it needs. Zero when the quoter keeps no
    /// such record.
    pub placed_slot: u64,
}

/// The row is an unfilled taker remainder the caller migrated onto the book,
/// not liquidity someone chose to post. It demands liquidity rather than
/// offering it, so depth behind it is not depth a cross can count on.
pub const L3_ROW_FLAG_TAKER_ORIGIN: u8 = 1;

/// This order is big enough to end a fill walk when its owner is absent from the
/// caller's user set. The quoter sets it, so an account-set builder never reimplements
/// the size floor. It says the order can end a walk, not that it will. The book skips an
/// order inside its grace window whatever the size.
pub const L3_ROW_FLAG_BLOCKS_WALK: u8 = 2;

/// The order is reduce-only. It may fill only up to the owner's position in the reduce
/// direction, and its owner carries an authoritative `base_cap`. A caller binds the fill
/// to the cover its own accounting reserved, and stops tracking the owner's reduce-only
/// exposure when the order leaves the book.
pub const L3_ROW_FLAG_REDUCE_ONLY: u8 = 4;

/// A taker-origin order reserves part of this row's size, or all of it. `size` already
/// has the reservation subtracted, and the flag says why the row is short of what the
/// order holds. A caller that settles the cross reads the same book with
/// `include_taker_origin_reservations` and sees the whole size.
pub const L3_ROW_FLAG_RESERVED: u8 = 8;

/// Encoded width of an [`L3RowV0`].
pub const L3_ROW_BYTES: usize = core::mem::size_of::<L3RowV0>();

/// The answer to `quote_l3_v0`: the resting orders behind a quoted book, best price
/// first. Read in place out of the quoter's response account, like every response here.
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct L3ResponseV0<'a> {
    pub rows: &'a [L3RowV0],
    /// The walk stopped on a bound rather than on the end of the book, so there
    /// is depth behind the last row. A caller displaying a book says so. A
    /// caller collecting users knows its list is a prefix.
    pub more: u8,
}

impl<'a> L3ResponseV0<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, SpecError> {
        wincode::deserialize(bytes).map_err(|_| SpecError::Read)
    }
}

/// Arguments to `quote_l3_v0`: how much of a side to describe. No user set and no caps,
/// because the question is what rests on the book rather than what this caller may
/// settle. The filtering a quote applies is the reader's to apply here.
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct L3ArgsV0 {
    pub direction: DirectionV0,
    /// Stop once this much base is described. Zero describes the side up to
    /// `max_rows`.
    pub size: u64,
    /// Stop after this many rows, whatever `size` is left.
    pub max_rows: u16,
    /// Report the depth that a taker-origin order reserves as available. Same
    /// contract as [`QuoteArgsV0::include_taker_origin_reservations`], on the
    /// surface a caller resolves a cross from.
    pub include_taker_origin_reservations: bool,
}

/// Arguments to `quote_v0`: what a taker wants, and who the caller can settle against. A
/// quote promises what `execute_v0` will deliver, so it takes the same `users` and `caps`
/// and spends them the same way. `users` is a slice into the caller's instruction data,
/// so a quoter walks it without standing a copy in a 4 KB frame. It leads the struct to
/// keep its offset even, and a field added ahead of it must preserve that.
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct QuoteArgsV0<'a> {
    /// The loaded-user set, at most [`USER_SET_CAPACITY`] entries. A reader
    /// checks that with [`user_set_within_capacity`], because the count comes
    /// off the wire.
    pub users: &'a [UserRefV0],
    pub direction: DirectionV0,
    /// Base the taker wants filled.
    pub size: u64,
    pub caps: UserCapsV0,
    /// The price the caller marks a filled position at, in PRICE_PRECISION.
    /// Only [`UserCapV0`] budgets are spent against it. It does not bound what a
    /// quoter may fill at.
    pub reference_price: i64,
    /// The taker's own user, whose resting liquidity is skipped
    /// unconditionally (self-trade prevention).
    pub taker: Option<UserRefV0>,
    /// The worst price the caller will fill at, in PRICE_PRECISION. Zero means no
    /// bound. A ladder is walked best price first, so a quoter may stop as soon as a
    /// level is worse. Not a trust boundary. Ignoring it wastes the caller's compute and
    /// returns levels it drops, and never widens what a quoter may fill.
    pub limit_price: u64,
    /// Whether the taker's flow served a protection window before this call: the swift
    /// hold, or the book's activation delay. The caller asserts it, like `users` and
    /// `caps`. A quoter that only serves protected flow refuses when this is false, as
    /// the midpoint does under `require_attested_flow`.
    pub taker_served_window: bool,
    /// Fill the depth that a taker-origin order reserves. The crank that settles the
    /// cross is the one caller that may take it, and it says so here. Unrelated to the
    /// margin a maker reserves at placement. A quoter that reserves no depth ignores
    /// it.
    pub include_taker_origin_reservations: bool,
}

/// Arguments to `execute_v0`: commit a fill. [`QuoteArgsV0`] without the price bound,
/// because `size` is only the depth the caller chose off the ladder, so the walk visits
/// no level a bound would have cut. A quoter may fill less than `size`, and what it
/// filled is whatever its balance changes sum to.
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
    /// Same contract as [`QuoteArgsV0::include_taker_origin_reservations`].
    /// The execute must carry the value its quote carried. Otherwise it walks
    /// a different set of orders than the one it quoted.
    pub include_taker_origin_reservations: bool,
}

/// The framing of the request half. Borsh-compatible, which is what a v2 program's
/// instruction dispatch reads, and named here so the writer and the reader agree by
/// declaration. The responses use wincode's own configuration, whose length prefix is
/// eight bytes rather than four, and reading one for the other shifts every field.
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

/// Bytes `args` takes on the wire. A caller reserves this before it writes, because it
/// builds the buffer on a 32 KB heap that never reclaims, and a `Vec` that doubles into
/// place leaks every intermediate buffer.
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

    /// The request half is read in place, and its framing carries a four-byte
    /// count rather than the responses' eight, so a quoter's dispatch decodes it
    /// borsh-compatibly. The offsets are pinned here because velocity writes
    /// these bytes from another workspace, where the agreement can only be held
    /// as numbers.
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
            include_taker_origin_reservations: false,
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
            include_taker_origin_reservations: false,
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

    /// A row is 72 bytes with no implicit padding, which is what `Pod` and the
    /// in-place read both need.
    #[test]
    fn the_l3_row_is_the_width_the_region_is_sized_from() {
        assert_eq!(L3_ROW_BYTES, 72);
        assert_eq!(L3_ROW_BYTES, 3 * 8 + 4 + UserRefV0::SIZE + 1 + 1 + 8);
    }

    #[test]
    fn the_user_ref_schema_matches_its_byte_form() {
        // `UserRefV0` writes its wincode schema by hand. The two ways this
        // crate states the same 34 bytes must agree, or a quoter's response and
        // velocity's reader disagree on where every field after the ref starts.
        let subject = user(0xAB, 0x0102);
        let encoded = wincode::serialize(&subject).unwrap();

        assert_eq!(encoded.len(), UserRefV0::SIZE);
        assert_eq!(encoded.as_slice(), subject.to_bytes().as_slice());
        assert_eq!(&encoded[..32], subject.authority.as_array());
        assert_eq!(&encoded[32..], &0x0102u16.to_le_bytes());

        let decoded: UserRefV0 = wincode::deserialize(&encoded).unwrap();
        assert_eq!(decoded, subject);

        // The schema reports itself zero-copy and unpadded, which is what lets
        // the records embedding it stay `#[wincode(assert_zero_copy)]`.
        assert_eq!(
            <UserRefV0 as SchemaWrite<wincode::config::DefaultConfig>>::TYPE_META,
            TypeMeta::Static {
                size: UserRefV0::SIZE,
                zero_copy: true,
            }
        );
    }

    /// The writers write the prefix themselves, so what this crate says it is
    /// has to be what wincode writes.
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

    /// Nine constrained users against eight slots. The eight tightest keep their
    /// exact number. The ninth is excluded rather than dropped, because a drop
    /// would read as unconstrained, which is the opposite of what its
    /// `quote_cap` says.
    #[test]
    fn a_budget_that_does_not_fit_becomes_an_exclusion() {
        let caps = (0..9).map(|index| UserCapV0 {
            index,
            quote_cap: 1_000 - index as u64,
            base_cap: u64::MAX,
        });
        let set = UserCapsV0::from_caps(caps);

        assert_eq!(set.len as usize, USER_CAPS_CAPACITY);
        // Index 0 has the loosest quote_cap of the nine, so it is the one evicted.
        assert!(set.is_excluded(0));
        for index in 1..9 {
            assert!(!set.is_excluded(index), "index {index}");
            assert_eq!(
                set.as_slice()
                    .iter()
                    .find(|cap| cap.index == index as u8)
                    .map(|cap| cap.quote_cap),
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
            quote_cap: 0,
            base_cap: u64::MAX,
        }));

        assert_eq!(set.len, 0);
        for index in 0..20 {
            assert!(set.is_excluded(index));
        }
    }

    /// An unbounded `quote_cap` is the same as saying nothing, so it costs
    /// neither a slot nor a bit.
    #[test]
    fn an_unbounded_budget_is_not_carried() {
        let set = UserCapsV0::from_caps((0..20).map(|index| UserCapV0 {
            index,
            quote_cap: u64::MAX,
            base_cap: u64::MAX,
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
