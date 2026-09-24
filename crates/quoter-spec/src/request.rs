//! What a quoter is called with. A quote promises what `execute_v0` will
//! deliver, so both take the same user set and caps and spend them the same
//! way.

use {
    crate::{DirectionV0, SpecError, UserRefV0},
    wincode::{SchemaRead, SchemaWrite},
};

/// Most users a call may name. The bound is the account-lock limit of the
/// transaction that carries the set, and what [`UserCapsV0`] can address with
/// one bit per slot.
pub const USER_SET_CAPACITY: usize = 48;

/// Users that can carry a partial cap on one call. A user with no room takes
/// one bit of [`UserCapsV0::excluded`] instead, so every user in the set can be
/// excluded at once.
pub const USER_CAPS_CAPACITY: usize = 8;

/// Bytes of bitmap for one bit per user in the set.
pub const USER_EXCLUSION_BITMAP_BYTES: usize = USER_SET_CAPACITY.div_ceil(8);

/// What one named user may still lose on the swept side, in quote. The quoter
/// has each fill price and the caller does not, so the cap is not in base. The
/// quoter charges `base * |price - reference_price| / BASE_PRECISION` where the
/// fill moves against the owner. Zero belongs in the exclusion bitmap.
#[repr(C)]
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct UserCapV0 {
    /// `u64::MAX` is unbounded. It bounds depth that was margin-reserved at
    /// placement. Base cannot express that bound, because a fill at a price in
    /// the owner's favour costs nothing per base.
    pub quote_cap: u64,
    /// The most base this user may give up on the swept side. `u64::MAX` is
    /// unbounded. A book enforces it at match time and will not fill a reduce-only
    /// order whose owner has no cap here, so for a book this field is a trust
    /// boundary. Every other quoter reads it as advisory.
    pub base_cap: u64,
    /// Index into the accompanying user set.
    pub index: u8,
}

/// Per-user room, parallel to the caller's user set. A user absent from all of
/// this is unconstrained. The quoter skips an excluded user's orders, and an
/// exclusion never fails the call. Not a trust boundary: the caller's own
/// post-fill checks still refuse the fill.
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

/// Encoded width of a [`UserCapsV0`].
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
    /// overflows. The rest take the slots, tightest first. A cap that does not
    /// fit becomes an exclusion, because a drop would offer the user's whole
    /// resting depth, which is the reading the cap exists to correct.
    ///
    /// Refuses an index past [`USER_SET_CAPACITY`] or an index named twice.
    /// Neither names one user, so no bound can be applied to it.
    pub fn from_caps(caps: impl IntoIterator<Item = UserCapV0>) -> Result<Self, SpecError> {
        let mut set = Self::EMPTY;
        let mut named = [0u8; USER_EXCLUSION_BITMAP_BYTES];
        let mut partial: [UserCapV0; USER_CAPS_CAPACITY] =
            [UserCapV0::default(); USER_CAPS_CAPACITY];
        let mut partial_len = 0usize;
        for cap in caps {
            let index = cap.index as usize;
            if index >= USER_SET_CAPACITY || named[index / 8] & (1 << (index % 8)) != 0 {
                return Err(SpecError::InvalidCapIndex);
            }

            named[index / 8] |= 1 << (index % 8);

            if cap.quote_cap == 0 {
                set.exclude(cap.index as usize);
                continue;
            }

            // `base_cap` alone still needs a slot. It is the reduce-only clamp
            // the book cannot reconstruct.
            if cap.quote_cap == u64::MAX && cap.base_cap == u64::MAX {
                continue;
            }

            let mut slot = partial_len;
            while slot > 0 && partial[slot - 1].quote_cap > cap.quote_cap {
                slot -= 1;
            }

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
        Ok(set)
    }
}

/// Encoded width of a user set of `len` entries: `len` refs behind a four-byte
/// count, which is what a borsh sequence writes. Empty means unrestricted.
pub const fn user_set_bytes(len: usize) -> usize {
    4 + len * UserRefV0::SIZE
}

/// Widest a user set can be on the wire.
pub const USER_SET_MAX_BYTES: usize = user_set_bytes(USER_SET_CAPACITY);

/// Whether a received user set is within what the wire allows. The exclusion
/// bitmap holds one bit per slot up to [`USER_SET_CAPACITY`], so an oversized
/// set would present an excluded user as settleable.
pub fn user_set_within_capacity(users: &[UserRefV0]) -> bool {
    users.len() <= USER_SET_CAPACITY
}

/// Arguments to `quote_l3_v0`: how much of a side to describe. No user set and
/// no caps, because the question is what rests on the book, not what this
/// caller may settle.
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct L3ArgsV0 {
    pub direction: DirectionV0,
    /// Stop once this much base is described. Zero describes the side up to
    /// `max_rows`.
    pub size: u64,
    /// Stop after this many rows, whatever `size` is left.
    pub max_rows: u16,
    /// Same contract as [`QuoteArgsV0::include_taker_origin_reservations`].
    pub include_taker_origin_reservations: bool,
}

/// Arguments to `quote_v0`: what a taker wants, and who the caller can settle
/// against. `users` is a slice into the caller's instruction data, so a quoter
/// walks it without a copy in a 4 KB frame. It leads the struct to keep its
/// offset even, and a field added ahead of it must preserve that.
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
    /// Budgets are spent and bands are checked against it. It does not bound
    /// what a quoter may fill at. `None` is a read that settles nothing.
    pub reference_price: Option<u64>,
    /// The taker's own user, whose resting liquidity is skipped
    /// unconditionally (self-trade prevention).
    pub taker: Option<UserRefV0>,
    /// The worst price the caller will fill at, in PRICE_PRECISION. Zero means
    /// no bound. Not a trust boundary: a quoter that ignores it returns levels
    /// the caller drops, and never widens what it may fill.
    pub limit_price: u64,
    /// Whether the taker's flow served a protection window before this call:
    /// the swift hold, or the book's activation delay. The caller asserts it. A
    /// quoter that serves only protected flow refuses when this is false.
    pub taker_served_window: bool,
    /// Fill the depth that a taker-origin order reserves. Only the crank that
    /// settles the cross sets it. A quoter that reserves no depth ignores it.
    pub include_taker_origin_reservations: bool,
}

/// Arguments to `execute_v0`: commit a fill. [`QuoteArgsV0`] without the price
/// bound, because `size` is depth the caller chose off the ladder. A quoter may
/// fill less than `size`, and what it filled is the sum of its balance changes.
/// Every field must carry the value its quote carried.
#[derive(Clone, Copy, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct ExecuteArgsV0<'a> {
    /// Same contract as [`QuoteArgsV0::users`], and it leads for the same
    /// reason.
    pub users: &'a [UserRefV0],
    pub direction: DirectionV0,
    pub size: u64,
    pub caps: UserCapsV0,
    /// The mark the quote was taken against. A different price makes a quoter
    /// that spends budgets pass over a different set of orders than it quoted.
    /// A fill always has a mark, so a quoter may refuse `None` here.
    pub reference_price: Option<u64>,
    pub taker: Option<UserRefV0>,
    pub taker_served_window: bool,
    pub include_taker_origin_reservations: bool,
}

/// The framing of the request half. It is borsh-compatible, which is what a v2
/// program's instruction dispatch reads. The responses use wincode's own
/// configuration, whose length prefix is eight bytes, not four.
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

/// Bytes `args` takes on the wire. A caller reserves this before it writes,
/// because a `Vec` that doubles into place on a heap that never reclaims leaks
/// every intermediate buffer.
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
