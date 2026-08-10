//! Wire format for velocity's quoter interface: the contract between velocity
//! (the router) and any program registered as a quoter — the CLOB, the
//! midpoint, and third-party PropAMMs.
//!
//! # Why this is a crate rather than a struct in each program
//!
//! The response to `execute_v0` crosses a program boundary in bytes. Velocity
//! decodes it; the quoter produces it. Before this crate the shape was
//! declared three times — once per program — and each side had tests pinning
//! its own encoding against its own declaration. Nothing pinned the three
//! against each other, so a field added on one side and forgotten on another
//! would produce two self-consistent programs that disagree about the bytes
//! between them. The failure is silent and lands on a value transfer: a
//! misread `base_size` moves the wrong amount of a user's collateral.
//!
//! One declaration lives here and all three programs use it. The reference
//! codec below stays because the v2 programs do not serialize these structs —
//! they write the bytes incrementally — so each is held to the layout by a
//! conformance test, and a divergence fails a test instead of a fill.
//!
//! # The address type is shared, not restated
//!
//! Velocity names it `Pubkey`, the v2 programs name it `Address`, and it is one
//! struct: solana-pubkey re-exports `Address as Pubkey`, and solana-address 1.x
//! is a shim over 2.x, so both trees land on the same `solana_address::Address`.
//! That is why these types can be the declaration each program uses instead of
//! a shape each one restates — a `Pubkey` satisfies these fields with no
//! conversion.
//!
//! # The encoding is borsh, written out by hand
//!
//! Velocity decodes with borsh, so borsh is what the bytes are. The CLOB
//! writes them without a borsh dependency, incrementally and out of order —
//! it aggregates repeated fills into one entry per user, patching counts and
//! adding into sizes already written. That writer cannot be replaced by
//! "serialize this struct", so this crate does not try to. It defines the
//! layout and a reference codec; the incremental writers stay where they are
//! and are held to this by conformance tests.
//!
//! Integers are little-endian. A `Vec<T>` is a `u32` length followed by that
//! many elements. No padding, no alignment, no discriminator — the caller
//! owns framing.

// Named `Pubkey` deliberately. It is `solana_address::Address` either way — but
// anchor's IDL derive recognizes the address type by the *token* in the field,
// not by the type it resolves to, so a field spelled `Address` makes
// `anchor idl build` try to generate an IDL type for it and fail. Velocity is
// the only consumer that runs that build, and this is the spelling it needs.
use solana_address::Address as Pubkey;

/// Every method below is `#[inline]` because these types cross a crate
/// boundary into the CLOB's per-fill loop, and without the hint the calls stop
/// being inlined there: `execute` over 50 orders measures 44,015 CU without it
/// against 42,870 with it.
///
/// The user a balance change applies to, in the derivable form velocity
/// resolves against its loaded users.
///
/// 34 bytes: 32-byte authority, then a little-endian `u16` sub-account id.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[cfg_attr(
    feature = "wincode-derive",
    derive(wincode::SchemaRead, wincode::SchemaWrite)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
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

    /// The borsh encoding as a fixed array, for comparing against and writing
    /// into a response region without a heap round-trip.
    #[inline]
    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut bytes = [0u8; Self::SIZE];
        bytes[..32].copy_from_slice(self.authority.as_array());
        bytes[32..].copy_from_slice(&self.sub_account_id.to_le_bytes());
        bytes
    }

    #[inline]
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.authority.as_array());
        out.extend_from_slice(&self.sub_account_id.to_le_bytes());
    }

    #[inline]
    pub fn decode(input: &[u8]) -> Result<(Self, usize), SpecError> {
        let authority = Pubkey::new_from_array(take_array::<32>(input, 0)?);
        let sub_account_id = u16::from_le_bytes(take_array::<2>(input, 32)?);
        Ok((
            Self {
                authority,
                sub_account_id,
            },
            Self::SIZE,
        ))
    }
}

/// One user's share of an executed fill.
///
/// Sign convention is the taker's direction, not this user's: `base_size` is
/// subtracted from this user when the taker went long (the taker takes base
/// from them) and added when the taker went short. `quote_size` moves the
/// opposite way.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[cfg_attr(
    feature = "wincode-derive",
    derive(wincode::SchemaRead, wincode::SchemaWrite)
)]
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct UserBalanceChangeV0 {
    pub user: UserRefV0,
    pub base_size: u64,
    pub quote_size: u64,
    /// Resting orders of this user the fill fully consumed, by id. The reader
    /// decrements the user's open-order count by the length and releases any
    /// per-order state it keeps against the book, so a stale id here frees a
    /// live order's shadow.
    pub completed_order_ids: Vec<u64>,
}

impl UserBalanceChangeV0 {
    #[inline]
    pub fn encode(&self, out: &mut Vec<u8>) {
        self.user.encode(out);
        out.extend_from_slice(&self.base_size.to_le_bytes());
        out.extend_from_slice(&self.quote_size.to_le_bytes());
        encode_u64_vec(&self.completed_order_ids, out);
    }

    #[inline]
    pub fn decode(input: &[u8]) -> Result<(Self, usize), SpecError> {
        let (user, mut off) = UserRefV0::decode(input)?;
        let base_size = u64::from_le_bytes(take_array::<8>(input, off)?);
        off += 8;
        let quote_size = u64::from_le_bytes(take_array::<8>(input, off)?);
        off += 8;
        let (completed_order_ids, used) = decode_u64_vec(input, off)?;
        Ok((
            Self {
                user,
                base_size,
                quote_size,
                completed_order_ids,
            },
            off + used,
        ))
    }
}

/// One order the quoter removed as a sub-min remainder of a fill.
///
/// Distinct from a completed order id: a completed order was consumed, this
/// one was culled because what was left of it fell under the market's minimum.
/// Both unwind the maker's aggregates, but only this one carries a size to
/// release.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[cfg_attr(
    feature = "wincode-derive",
    derive(wincode::SchemaRead, wincode::SchemaWrite)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CancelledRemainderV0 {
    pub user: UserRefV0,
    pub order_id: u64,
    pub base_asset_amount: u64,
}

impl CancelledRemainderV0 {
    pub const SIZE: usize = UserRefV0::SIZE + 16;

    #[inline]
    pub fn encode(&self, out: &mut Vec<u8>) {
        self.user.encode(out);
        out.extend_from_slice(&self.order_id.to_le_bytes());
        out.extend_from_slice(&self.base_asset_amount.to_le_bytes());
    }

    #[inline]
    pub fn decode(input: &[u8]) -> Result<(Self, usize), SpecError> {
        let (user, mut off) = UserRefV0::decode(input)?;
        let order_id = u64::from_le_bytes(take_array::<8>(input, off)?);
        off += 8;
        let base_asset_amount = u64::from_le_bytes(take_array::<8>(input, off)?);
        Ok((
            Self {
                user,
                order_id,
                base_asset_amount,
            },
            Self::SIZE,
        ))
    }
}

/// What `execute_v0` returns: every balance change the fill produced, and
/// every sub-min remainder it removed on the way.
#[cfg_attr(
    feature = "anchor-derive",
    derive(anchor_lang::AnchorSerialize, anchor_lang::AnchorDeserialize)
)]
#[cfg_attr(
    feature = "wincode-derive",
    derive(wincode::SchemaRead, wincode::SchemaWrite)
)]
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ExecuteResponseV0 {
    pub balance_changes: Vec<UserBalanceChangeV0>,
    pub cancelled: Vec<CancelledRemainderV0>,
}

impl ExecuteResponseV0 {
    #[inline]
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.balance_changes.len() as u32).to_le_bytes());
        for change in &self.balance_changes {
            change.encode(out);
        }
        out.extend_from_slice(&(self.cancelled.len() as u32).to_le_bytes());
        for cancelled in &self.cancelled {
            cancelled.encode(out);
        }
    }

    /// Decode a whole response, returning it with the number of bytes read.
    /// Trailing bytes are the caller's business — velocity's response region
    /// is a fixed window that is not fully written.
    #[inline]
    pub fn decode(input: &[u8]) -> Result<(Self, usize), SpecError> {
        let mut off = 0usize;
        let count = u32::from_le_bytes(take_array::<4>(input, off)?) as usize;
        off += 4;
        let mut balance_changes = Vec::with_capacity(count.min(MAX_PREALLOC));
        for _ in 0..count {
            let (change, used) = UserBalanceChangeV0::decode(rest(input, off)?)?;
            balance_changes.push(change);
            off += used;
        }
        let count = u32::from_le_bytes(take_array::<4>(input, off)?) as usize;
        off += 4;
        let mut cancelled = Vec::with_capacity(count.min(MAX_PREALLOC));
        for _ in 0..count {
            let (entry, used) = CancelledRemainderV0::decode(rest(input, off)?)?;
            cancelled.push(entry);
            off += used;
        }
        Ok((
            Self {
                balance_changes,
                cancelled,
            },
            off,
        ))
    }
}

/// Cap on what a length prefix may reserve before any of it is read. A
/// corrupt or hostile count must not turn into an allocation of its own size:
/// on-chain the heap is 32 KB and never reclaims, so one bad length would
/// exhaust it before the decode failed.
const MAX_PREALLOC: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpecError {
    /// The buffer ended inside a field.
    Truncated,
}

fn take_array<const N: usize>(input: &[u8], offset: usize) -> Result<[u8; N], SpecError> {
    let end = offset.checked_add(N).ok_or(SpecError::Truncated)?;
    let slice = input.get(offset..end).ok_or(SpecError::Truncated)?;
    let mut out = [0u8; N];
    out.copy_from_slice(slice);
    Ok(out)
}

fn rest(input: &[u8], offset: usize) -> Result<&[u8], SpecError> {
    input.get(offset..).ok_or(SpecError::Truncated)
}

fn encode_u64_vec(values: &[u64], out: &mut Vec<u8>) {
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
}

fn decode_u64_vec(input: &[u8], offset: usize) -> Result<(Vec<u64>, usize), SpecError> {
    let count = u32::from_le_bytes(take_array::<4>(input, offset)?) as usize;
    let mut values = Vec::with_capacity(count.min(MAX_PREALLOC));
    for i in 0..count {
        values.push(u64::from_le_bytes(take_array::<8>(
            input,
            offset + 4 + i * 8,
        )?));
    }
    Ok((values, 4 + count * 8))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ExecuteResponseV0 {
        ExecuteResponseV0 {
            balance_changes: vec![
                UserBalanceChangeV0 {
                    user: UserRefV0 {
                        authority: Pubkey::new_from_array([7u8; 32]),
                        sub_account_id: 3,
                    },
                    base_size: 1_000_000_000,
                    quote_size: 101_000_000,
                    completed_order_ids: vec![9, 10],
                },
                UserBalanceChangeV0 {
                    user: UserRefV0 {
                        authority: Pubkey::new_from_array([8u8; 32]),
                        sub_account_id: 0,
                    },
                    base_size: 5,
                    quote_size: 6,
                    completed_order_ids: vec![],
                },
            ],
            cancelled: vec![CancelledRemainderV0 {
                user: UserRefV0 {
                    authority: Pubkey::new_from_array([9u8; 32]),
                    sub_account_id: 1,
                },
                order_id: 42,
                base_asset_amount: 17,
            }],
        }
    }

    #[test]
    fn round_trips() {
        let mut bytes = Vec::new();
        sample().encode(&mut bytes);
        let (decoded, used) = ExecuteResponseV0::decode(&bytes).unwrap();
        assert_eq!(decoded, sample());
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn trailing_bytes_are_the_callers_business() {
        let mut bytes = Vec::new();
        sample().encode(&mut bytes);
        let padded = [bytes.clone(), vec![0xAA; 64]].concat();
        let (decoded, used) = ExecuteResponseV0::decode(&padded).unwrap();
        assert_eq!(decoded, sample());
        assert_eq!(used, bytes.len(), "stops at the end of the payload");
    }

    /// The layout is the contract; pin the byte offsets so a field reordered
    /// here fails rather than silently redefining the wire.
    #[test]
    fn layout_is_pinned() {
        let change = UserBalanceChangeV0 {
            user: UserRefV0 {
                authority: Pubkey::new_from_array([1u8; 32]),
                sub_account_id: 0x0201,
            },
            base_size: 0x0807_0605_0403_0201,
            quote_size: 0x1817_1615_1413_1211,
            completed_order_ids: vec![0xFF],
        };
        let mut bytes = Vec::new();
        change.encode(&mut bytes);
        assert_eq!(&bytes[0..32], &[1u8; 32]);
        assert_eq!(&bytes[32..34], &[0x01, 0x02]);
        assert_eq!(&bytes[34..42], &0x0807_0605_0403_0201u64.to_le_bytes());
        assert_eq!(&bytes[42..50], &0x1817_1615_1413_1211u64.to_le_bytes());
        assert_eq!(&bytes[50..54], &1u32.to_le_bytes());
        assert_eq!(&bytes[54..62], &0xFFu64.to_le_bytes());
        assert_eq!(bytes.len(), 62);
    }

    #[test]
    fn truncation_is_an_error_not_a_panic() {
        let mut bytes = Vec::new();
        sample().encode(&mut bytes);
        for cut in 0..bytes.len() {
            assert_eq!(
                ExecuteResponseV0::decode(&bytes[..cut]),
                Err(SpecError::Truncated),
                "truncating to {cut} bytes must be an error"
            );
        }
    }

    /// A hostile length prefix must not allocate its own size before any of
    /// the elements behind it have been read.
    #[test]
    fn absurd_length_prefix_fails_without_allocating() {
        let bytes = u32::MAX.to_le_bytes();
        assert_eq!(ExecuteResponseV0::decode(&bytes), Err(SpecError::Truncated));
    }
}
