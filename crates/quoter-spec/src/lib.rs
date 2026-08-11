//! Wire format for velocity's quoter interface: the contract between velocity
//! (the router) and any program registered as a quoter — the CLOB, the
//! midpoint, and third-party PropAMMs.
//!
//! # One declaration, three programs
//!
//! `quote_v0` and `execute_v0` answer across a program boundary in bytes.
//! Velocity reads them; the quoter produces them. Declaring the shape once per
//! program leaves nothing pinning the declarations against each other, so a
//! field added on one side and forgotten on another gives two self-consistent
//! programs that disagree about the bytes between them — and the disagreement
//! lands on a value transfer, where a misread `base_size` moves the wrong
//! amount of a user's collateral. The types live here and all three use them.
//!
//! # The responses are read in place
//!
//! A response is plain data sitting in the quoter's account, and velocity reads
//! it there: fixed-width records, little-endian, no length-prefixed nesting and
//! no deserialization step. That is not only a CU question. Velocity's heap is
//! 32 KB and never reclaims, and one fill CPIs every registered quoter twice,
//! so a response that decodes into `Vec`s spends heap per quoter per fill that
//! nothing gives back.
//!
//! Every record is `#[repr(C)]` and free of implicit padding, which is what
//! both `bytemuck::Pod` and wincode's zero-copy rules require. The `Pod`
//! derives below are the enforcement: a field reordered into a layout with a
//! padding hole stops compiling rather than silently changing the wire. Field
//! order is therefore load-bearing — the `u64`s lead so the 34-byte
//! [`UserRefV0`] cannot push one out of alignment, and each record carries
//! explicit tail padding to a multiple of its alignment.
//!
//! # Framing
//!
//! Each response is a header of counts followed by that many fixed-width
//! records per section, in declaration order. Sections are contiguous; the
//! caller owns anything past the last one.
//!
//! An execute response is [`ExecuteHeaderV0`], then [`UserBalanceChangeV0`],
//! then [`CancelledRemainderV0`], then [`CompletedOrderV0`]. Completed order
//! ids are their own section rather than a list inside each change: a quoter
//! aggregates repeated fills into one record per user as it goes, so ids for a
//! user arrive interleaved with other users' fills. Naming the change from the
//! id makes appending one an O(1) write at the tail instead of a shift of
//! everything after it.
//!
//! # Addresses
//!
//! Velocity names the address type `Pubkey` and the v2 programs name it
//! `Address`; it is one type, because solana-pubkey re-exports `Address as
//! Pubkey` and solana-address 1.x is a shim over 2.x. It is spelled `Pubkey`
//! here because anchor's IDL derive recognizes it by that token rather than by
//! the type it resolves to, and velocity is the consumer that runs
//! `anchor idl build`.

use {
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
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, SchemaRead, SchemaWrite)]
#[wincode(assert_zero_copy)]
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
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, SchemaRead, SchemaWrite)]
#[wincode(assert_zero_copy)]
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
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, SchemaRead, SchemaWrite)]
#[wincode(assert_zero_copy)]
pub struct CancelledRemainderV0 {
    pub order_id: u64,
    pub base_asset_amount: u64,
    pub user: UserRefV0,
    pub _pad: [u8; 6],
}

/// One resting order a fill fully consumed, naming the balance change it
/// belongs to by index.
///
/// The reader decrements that user's open-order count once per entry and
/// releases any per-order state it keeps against the book, so an id for an
/// order still live on the book frees a live order's shadow.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, SchemaRead, SchemaWrite)]
#[wincode(assert_zero_copy)]
pub struct CompletedOrderV0 {
    pub order_id: u64,
    pub change_index: u32,
    pub _pad: u32,
}

/// One rung of a quoted ladder: `size` available at `price`.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, SchemaRead, SchemaWrite)]
#[wincode(assert_zero_copy)]
pub struct PriceLevelV0 {
    pub price: u64,
    pub size: u64,
}

/// Widths the quoters' own section arithmetic is built from.
pub const USER_REF_BYTES: usize = UserRefV0::SIZE;
pub const CHANGE_BYTES: usize = core::mem::size_of::<UserBalanceChangeV0>();
pub const CANCELLED_BYTES: usize = core::mem::size_of::<CancelledRemainderV0>();
pub const COMPLETED_BYTES: usize = core::mem::size_of::<CompletedOrderV0>();
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
    assert!(CANCELLED_BYTES == 56);
    assert!(COMPLETED_BYTES == 16);
    assert!(PRICE_LEVEL_BYTES == 16);
};

/// What `execute_v0` answers: every balance change the fill produced, every
/// sub-min remainder it removed, and every resting order it consumed.
///
/// The fields borrow straight out of the quoter's account — reading one
/// allocates nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, SchemaRead, SchemaWrite)]
pub struct ExecuteResponseV0<'a> {
    pub changes: &'a [UserBalanceChangeV0],
    pub cancelled: &'a [CancelledRemainderV0],
    pub completed: &'a [CompletedOrderV0],
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
        if response
            .completed
            .iter()
            .any(|entry| entry.change_index as usize >= response.changes.len())
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

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.cancelled.is_empty() && self.completed.is_empty()
    }
}

/// What `quote_v0` answers: the ladder the quoter is standing behind.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, SchemaRead, SchemaWrite)]
pub struct QuoteResponseV0<'a> {
    pub levels: &'a [PriceLevelV0],
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
            user: user(9, 1),
            _pad: [0; 6],
        }]
    }

    fn completed() -> [CompletedOrderV0; 2] {
        [
            CompletedOrderV0 {
                order_id: 9,
                change_index: 0,
                _pad: 0,
            },
            CompletedOrderV0 {
                order_id: 10,
                change_index: 0,
                _pad: 0,
            },
        ]
    }

    #[test]
    fn round_trips_without_copying() {
        let (c, x, d) = (changes(), cancelled(), completed());
        let response = ExecuteResponseV0 {
            changes: &c,
            cancelled: &x,
            completed: &d,
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
        let (c, x, d) = (changes(), cancelled(), completed());
        let bytes = wincode::serialize(&ExecuteResponseV0 {
            changes: &c,
            cancelled: &x,
            completed: &d,
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
                _pad: 0,
            }],
        })
        .unwrap();
        assert_eq!(
            ExecuteResponseV0::parse(&bytes),
            Err(SpecError::DanglingCompletedOrder)
        );
    }

    #[test]
    fn truncation_is_an_error_not_a_panic() {
        let (c, x, d) = (changes(), cancelled(), completed());
        let bytes = wincode::serialize(&ExecuteResponseV0 {
            changes: &c,
            cancelled: &x,
            completed: &d,
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
        let bytes = wincode::serialize(&QuoteResponseV0 { levels: &levels }).unwrap();
        let response = QuoteResponseV0::parse(&bytes).unwrap();
        assert_eq!(response.levels, levels.as_slice());
    }

    /// A streaming writer lays the prefix down itself, so what this crate
    /// says it is has to be what wincode actually writes.
    #[test]
    fn the_length_prefix_is_what_wincode_writes() {
        let levels = [
            PriceLevelV0 { price: 1, size: 2 },
            PriceLevelV0 { price: 3, size: 4 },
        ];
        let bytes = wincode::serialize(&QuoteResponseV0 { levels: &levels }).unwrap();
        assert_eq!(&bytes[..LEN_BYTES], &len_prefix(levels.len()));
        assert_eq!(bytes.len(), LEN_BYTES + levels.len() * PRICE_LEVEL_BYTES);
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
