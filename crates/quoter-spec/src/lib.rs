//! Wire format for velocity's quoter interface: the contract between velocity
//! (the router) and any program registered as a quoter — the CLOB, the
//! midpoint, and third-party PropAMMs.
//!
//! # One declaration, three programs
//!
//! This crate is the whole interface: the arguments a quoter is called with
//! ([`QuoteArgsV0`], [`ExecuteArgsV0`] and the types they carry) and the
//! responses it must produce. Reading it should be enough to implement one.
//!
//! `quote_v0` and `execute_v0` answer across a program boundary in bytes.
//! Velocity writes the arguments and reads the responses; the quoter does the
//! reverse. Declaring the shape once per program leaves nothing pinning the
//! declarations against each other, so a
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

// Re-exported so a consumer can write a response without taking its own
// wincode dependency — the framing is this crate's to define, so the encoder
// is too.
pub use wincode;
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

    /// How many orders the fill consumed for `change_index`.
    pub fn completed_count(&self, change_index: usize) -> usize {
        self.completed_for(change_index).count()
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

/// Users a quote or execute may fill, and how much room each has left.
///
/// The request half of the wire, declared here for the reason the responses
/// are: velocity writes these bytes and a quoter reads them, and two
/// hand-mirrored declarations are two programs that can drift into
/// self-consistent disagreement. A cap misread as a taker, or a side read off
/// by one, silently turns a skip into a fill.
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

/// Which side an order rests on: a bid makes its owner long, an ask short.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
pub enum SideV0 {
    Bid,
    Ask,
}

/// What one named user may still take on, per side, in base.
///
/// Two numbers rather than one keyed off the call's direction: a maker's room
/// genuinely differs by side — the direction that reduces its position
/// answers to a looser requirement than the one that adds to it — and one set
/// has to serve a cross-match that sweeps both sides of a book in the same
/// transaction.
#[repr(C)]
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, SchemaRead, SchemaWrite)]
pub struct UserCapV0 {
    /// Base this user may take resting on the bid side (going long).
    pub bid_base: u64,
    /// Base this user may take resting on the ask side (going short).
    pub ask_base: u64,
    /// Index into the accompanying user set.
    pub index: u8,
}

/// Per-user room, parallel to the caller's user set.
///
/// A user absent from all of this is unconstrained. A zero cap means their
/// orders are passed over entirely — the caller has said it cannot settle a
/// fill against them, so quoting depth standing on their orders would promise
/// depth the fill declines.
///
/// Distinct from membership of the user set: absent from *that* means the
/// caller's account set is stale and, past the grace window, the whole call
/// fails. A zero cap is a deliberate constraint, not a mistake, and never
/// fails the call.
///
/// **Not a trust boundary.** A quoter that ignores these leaves its caller
/// exactly where it stands without them — the caller's own post-fill checks
/// still refuse the fill. What honouring them buys is that the honest case
/// stops reverting.
#[repr(C)]
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
pub struct UserCapsV0 {
    /// One bit per index in the set: set means no room resting on the bid
    /// side, so pass that user's bids over.
    pub excluded_bid: [u8; USER_EXCLUSION_BITMAP_BYTES],
    /// The same for the ask side.
    pub excluded_ask: [u8; USER_EXCLUSION_BITMAP_BYTES],
    /// Live entries at the head of `caps`; the tail is undefined.
    pub len: u8,
    pub caps: [UserCapV0; USER_CAPS_CAPACITY],
}

/// Encoded width of a [`UserCapsV0`]. One constant rather than an assertion
/// per program, which is the point of declaring the shape once.
pub const USER_CAPS_BYTES: usize =
    2 * USER_EXCLUSION_BITMAP_BYTES + 1 + USER_CAPS_CAPACITY * (8 + 8 + 1);

impl Default for UserCapsV0 {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl UserCapsV0 {
    pub const EMPTY: Self = Self {
        excluded_bid: [0; USER_EXCLUSION_BITMAP_BYTES],
        excluded_ask: [0; USER_EXCLUSION_BITMAP_BYTES],
        len: 0,
        caps: [UserCapV0 {
            index: 0,
            bid_base: 0,
            ask_base: 0,
        }; USER_CAPS_CAPACITY],
    };

    /// The live prefix. `len` crosses a program boundary, so it is clamped
    /// rather than trusted.
    pub fn as_slice(&self) -> &[UserCapV0] {
        &self.caps[..(self.len as usize).min(USER_CAPS_CAPACITY)]
    }

    fn bitmap(&self, side: SideV0) -> &[u8; USER_EXCLUSION_BITMAP_BYTES] {
        match side {
            SideV0::Bid => &self.excluded_bid,
            SideV0::Ask => &self.excluded_ask,
        }
    }

    /// Mark the user at `index` as having no room on `side`.
    pub fn exclude(&mut self, index: usize, side: SideV0) {
        if index >= USER_SET_CAPACITY {
            return;
        }
        let map = match side {
            SideV0::Bid => &mut self.excluded_bid,
            SideV0::Ask => &mut self.excluded_ask,
        };
        map[index / 8] |= 1 << (index % 8);
    }

    /// Whether the user at `index` has no room on `side`. An index past the
    /// set is the caller disagreeing with itself, and constrains nobody.
    pub fn is_excluded(&self, index: usize, side: SideV0) -> bool {
        index < USER_SET_CAPACITY && self.bitmap(side)[index / 8] & (1 << (index % 8)) != 0
    }

    /// Whether anyone is excluded on `side` — the check that keeps an
    /// ordinary walk from paying for a lookup it never needs.
    pub fn any_excluded(&self, side: SideV0) -> bool {
        self.bitmap(side).iter().any(|byte| *byte != 0)
    }

    /// Build from per-user room. No room on a side becomes a bitmap bit,
    /// which never overflows; the rest take the scarce partial slots,
    /// tightest first, so an overflow drops the entries with the most room.
    pub fn from_caps(caps: impl IntoIterator<Item = UserCapV0>) -> Self {
        let mut set = Self::EMPTY;
        let mut partial: [UserCapV0; USER_CAPS_CAPACITY] =
            [UserCapV0::default(); USER_CAPS_CAPACITY];
        let mut partial_len = 0usize;
        for cap in caps {
            if cap.bid_base == 0 {
                set.exclude(cap.index as usize, SideV0::Bid);
            }
            if cap.ask_base == 0 {
                set.exclude(cap.index as usize, SideV0::Ask);
            }
            if cap.bid_base == 0 && cap.ask_base == 0 {
                continue;
            }
            let room = cap.bid_base.saturating_add(cap.ask_base);
            // Insertion sort into a fixed array: the list is eight long and
            // this runs on a frame that cannot afford a heap round trip.
            let mut slot = partial_len.min(USER_CAPS_CAPACITY - 1);
            while slot > 0
                && partial[slot - 1]
                    .bid_base
                    .saturating_add(partial[slot - 1].ask_base)
                    > room
            {
                partial[slot] = partial[slot - 1];
                slot -= 1;
            }
            if slot < USER_CAPS_CAPACITY {
                partial[slot] = cap;
                partial_len = (partial_len + 1).min(USER_CAPS_CAPACITY);
            }
        }
        set.caps = partial;
        set.len = partial_len as u8;
        set
    }
}

/// The loaded-user set a call may settle against.
///
/// Empty means unrestricted, which only a caller that settles nothing (quote
/// discovery) uses. Otherwise it is the set of the caller's loaded users, and
/// liquidity owned by anyone else must be passed over — the caller cannot
/// settle a balance change for a user it did not load, and refuses the whole
/// response if one appears.
///
/// Fixed width so both sides decode it without negotiating a length, and so
/// the size of a request never moves with its contents.
#[repr(C)]
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
pub struct UserSetV0 {
    /// Live entries at the head of `users`; the tail is undefined.
    pub len: u8,
    pub users: [UserRefV0; USER_SET_CAPACITY],
}

/// Encoded width of a [`UserSetV0`].
pub const USER_SET_BYTES: usize = 1 + USER_SET_CAPACITY * UserRefV0::SIZE;

impl Default for UserSetV0 {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl UserSetV0 {
    pub const EMPTY: Self = Self {
        len: 0,
        users: [UserRefV0::ZERO; USER_SET_CAPACITY],
    };

    /// The live prefix. `len` crosses a program boundary, so it is clamped
    /// rather than trusted.
    pub fn as_slice(&self) -> &[UserRefV0] {
        &self.users[..(self.len as usize).min(USER_SET_CAPACITY)]
    }

    /// Whether `user` is in the live prefix.
    pub fn contains(&self, user: &UserRefV0) -> bool {
        self.as_slice().contains(user)
    }

    /// `None` when the set does not fit — a caller with more loaded users
    /// than the wire carries must not silently match against a truncated set.
    pub fn from_refs(refs: &[UserRefV0]) -> Option<Self> {
        if refs.len() > USER_SET_CAPACITY {
            return None;
        }
        let mut set = Self::EMPTY;
        set.len = refs.len() as u8;
        set.users[..refs.len()].copy_from_slice(refs);
        Some(set)
    }
}

/// Arguments to `quote_v0`: what a taker wants, and who the caller can settle
/// against.
///
/// A quote is a promise about what `execute_v0` will deliver, so it is given
/// the same `users` and `caps` and must spend them the same way. A ladder
/// standing on liquidity the matching execute would decline is a ladder its
/// reader cannot route against.
#[repr(C)]
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
pub struct QuoteArgsV0 {
    pub direction: DirectionV0,
    /// Base the taker wants filled.
    pub size: u64,
    pub users: UserSetV0,
    pub caps: UserCapsV0,
    /// The taker's own user, whose resting liquidity is skipped
    /// unconditionally (self-trade prevention).
    pub taker: Option<UserRefV0>,
}

/// Arguments to `execute_v0`: commit a fill.
///
/// The same shape as [`QuoteArgsV0`] because it answers the same question,
/// having committed to it. A quoter may fill less than `size`; what it
/// actually filled is whatever its returned balance changes sum to.
#[repr(C)]
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
pub struct ExecuteArgsV0 {
    pub direction: DirectionV0,
    pub size: u64,
    pub users: UserSetV0,
    pub caps: UserCapsV0,
    pub taker: Option<UserRefV0>,
}
