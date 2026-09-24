// Wire format for velocity's quoter interface. It is the contract between
// velocity (the router) and any program registered as a quoter: the CLOB, the
// midpoint, and third-party PropAMMs. All three programs use these types, so a
// field added on one side cannot be forgotten on another.
//
// `request` holds the arguments a quoter is called with. `response` holds what
// it answers, and `write` holds the writers a quoter streams an answer with.
//
// Velocity signs every quoter call as the market's quoter-slab PDA, and every
// quoter on that market receives the same signer. A quoter must keep any
// authority gated on that key on its response account. Velocity refuses to
// approve a quoter whose accounts name another quoter's response account.
//
// This file is also the source of `quoter-spec-v2` through `include!`, so it
// cannot carry `//!` docs or `#![...]` attributes.

// The v2 IdlType derive emits `anchor_lang::`. Point it at the fork when the
// v2 IDL build is on.
#[cfg(feature = "idl-build-v2")]
extern crate anchor_lang_v2 as anchor_lang;

// Re-exported so a consumer can write a response without its own wincode
// dependency.
pub use wincode;
mod request;
mod response;
#[cfg(test)]
mod tests;
pub mod write;

// Spelled `Pubkey` because anchor's IDL derive recognizes the address type by
// that token. It is the same type as the v2 programs' `Address`.
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
pub use {
    request::{
        args_size, user_set_bytes, user_set_within_capacity, write_args, ArgsConfig, ExecuteArgsV0,
        L3ArgsV0, QuoteArgsV0, UserCapV0, UserCapsV0, ARGS_CONFIG, USER_CAPS_BYTES,
        USER_CAPS_CAPACITY, USER_EXCLUSION_BITMAP_BYTES, USER_SET_CAPACITY, USER_SET_MAX_BYTES,
    },
    response::{
        len_prefix, CancelledRemainderV0, ChangeOrders, CompletedOrderV0, ExecuteResponseV0,
        L3ResponseV0, L3RowV0, PartiallyFilledOrderV0, PriceLevelV0, QuoteResponseV0,
        ResponsePointerV0, UserBalanceChangeV0, CANCELLED_BYTES, CHANGE_BYTES, COMPLETED_BYTES,
        L3_ROW_BYTES, L3_ROW_FLAG_BLOCKS_WALK, L3_ROW_FLAG_REDUCE_ONLY, L3_ROW_FLAG_RESERVED,
        L3_ROW_FLAG_TAKER_ORIGIN, LEN_BYTES, PARTIAL_BYTES, PRICE_LEVEL_BYTES, USER_REF_BYTES,
    },
    write::{ExecuteWriter, L3Writer, QuoteWriter},
};

/// The all-zero key. A quoter uses it to mean "no key set", which its own
/// validation then refuses where a real key is required.
pub const ZERO_ADDRESS: Pubkey = Pubkey::new_from_array([0u8; 32]);

/// Base units in one whole base asset. It turns a base amount and a price
/// difference into a quote amount, so every quoter spends a cap the same way.
pub const BASE_PRECISION: u64 = 1_000_000_000;

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
    /// A writer was asked for a balance change it has not written, or for more
    /// changes than a `change_index` can name.
    ChangeIndexOutOfRange,
    /// A response offset or length does not fit the `u32` a pointer carries.
    PointerOverflow,
    /// A cap names an index past the user set, or an index another cap names.
    InvalidCapIndex,
}

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

// The derive needs `Address` to carry wincode's traits, which only solana-address
// 2.7 gives, and litesvm and the agave RPC crates hold these trees to 2.6. So the
// schema delegates to `[u8; 32]` and `u16`, and a test pins it against `to_bytes`.
// TODO: derive `SchemaRead` and `SchemaWrite` once solana-address 2.7 can be taken.
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
// fields are zero-copy under any configuration that makes the integer encoding
// zero-copy. The derive emits the same impl for such a `#[repr(C)]` struct.
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

/// Taker direction, from the taker's perspective. Encoded as its discriminant,
/// `Long = 0`. A direction inverted across the boundary fills the wrong side.
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
    /// Zero is no bound. A level exactly at the limit is acceptable.
    pub fn worse_than_limit(self, price: u64, limit_price: u64) -> bool {
        limit_price != 0
            && match self {
                DirectionV0::Long => price > limit_price,
                DirectionV0::Short => price < limit_price,
            }
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

/// Which sides a `cancel_all_v0` withdraws. The wire cannot express "neither",
/// which would leave a maker believing its quotes are gone. A book reads a side
/// as a book side and a caller as a position direction. Only the tags must agree.
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
