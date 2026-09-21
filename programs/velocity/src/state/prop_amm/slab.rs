//! The approved set: one market's quoters, in one account.
//!
//! [`QuoterSlabV0`] is a fixed header. The slot region follows it in the
//! account's remaining bytes as back-to-back [`QuoterSlotV0`]s, so capacity is
//! the account's size and grows with it. A fill, `quote_router` and every CLOB
//! instruction resolve slots through the accessors here.

use {
    super::{find_account, QuoterConfigV0, QuoterType},
    crate::{error::ErrorCode, msg, validate},
    anchor_lang::prelude::*,
    static_assertions::const_assert_eq,
};

/// One approved quoter in a market's slab.
#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct QuoterSlotV0 {
    /// The staging [`super::QuoterV0`] this approved copy came from. It is the
    /// quoter's identity everywhere one is named. `Pubkey::default()` marks a
    /// vacant slot.
    pub entry: Pubkey,
    /// Set when the admin pulls approval from a `Clob` slot. The slot quotes
    /// nothing, but the config stays so the removal paths keep working. A maker
    /// must always be able to pull orders off a killed book. A revoked `Custom`
    /// slot is cleared instead, because it has no resting state to unwind.
    pub suspended: bool,
    pub padding: [u8; 7],
    /// The admin-approved copy of the entry's config. Only `is_active`,
    /// `priority` and `max_oracle_deviation_bps` are written between approvals.
    /// Each of the three writes through from its own registry instruction.
    pub config: QuoterConfigV0,
}

const_assert_eq!(std::mem::size_of::<QuoterSlotV0>(), 776);

// The slot region is read straight out of account bytes, so the slot needs
// the bytemuck casts `#[account(zero_copy(unsafe))]` gives an account type.
// The same trust argument applies. The loader checks the owner, so only this
// program writes a slab, and it writes only valid values into the `bool` fields.
unsafe impl bytemuck::Zeroable for QuoterSlotV0 {}
unsafe impl bytemuck::Pod for QuoterSlotV0 {}

impl QuoterSlotV0 {
    pub fn is_vacant(&self) -> bool {
        self.entry == Pubkey::default()
    }

    /// Whether this slot may take new flow: occupied, not suspended by the
    /// admin, and not deactivated by its maker.
    pub fn quotes(&self) -> bool {
        !self.is_vacant() && !self.suspended && self.config.is_active
    }

    pub fn clear(&mut self) {
        *self = QuoterSlotV0::default();
    }
}

/// One market's approved quoters, in one account. This struct is only the fixed
/// header. Read the slot region that follows it through [`QuoterSlabExt::slots`]
/// or [`QuoterSlabExt::slots_mut`]. A vacant slot is all zeroes. Creation through
/// `initialize_quoter_slab` is permissionless, and only the approval flow writes
/// slots.
#[account(zero_copy(unsafe))]
#[derive(Eq, PartialEq, Debug)]
#[repr(C)]
pub struct QuoterSlabV0 {
    /// Perp market this slab serves. It is also in the PDA seeds.
    pub market: u16,
    /// Slots the region holds. Written at creation and when the account grows.
    /// The account must be at least [`QuoterSlabV0::space`] of it.
    pub capacity: u16,
    /// The slab PDA's bump, stored at creation. The slab is the identity
    /// velocity signs every external quoter CPI as. See `crate::signer`.
    /// Signing needs the bump, and a stored byte is cheaper than a derivation
    /// on every leg.
    pub bump: u8,
    pub _pad: [u8; 3],
    /// The market's book, the `Clob` slot's response account. The header holds it so
    /// every accounts struct that names both binds them with `has_one = clob_market`.
    /// It survives a book suspension, because the removal paths must keep reaching a
    /// killed book. `Pubkey::default()` means no book was ever approved.
    pub clob_market: Pubkey,
    /// Header reserve, so future header fields never move the slot region.
    pub padding: [u8; 120],
}

impl Default for QuoterSlabV0 {
    fn default() -> Self {
        QuoterSlabV0 {
            market: 0,
            capacity: 0,
            bump: 0,
            _pad: [0; 3],
            clob_market: Pubkey::default(),
            padding: [0; 120],
        }
    }
}

// Zero-copy layout invariant (see docs/alignment-and-native-offsets.md):
// no u128 fields, size (incl. 8-byte discriminator) ≡ 8 (mod 16).
const_assert_eq!(std::mem::size_of::<QuoterSlabV0>(), 160);
const_assert_eq!((QuoterSlabV0::SLOT_REGION_OFFSET - 8) % 16, 0);
// The slot region must start 8-aligned so its u64 fields are aligned.
const_assert_eq!(QuoterSlabV0::SLOT_REGION_OFFSET % 8, 0);

/// PDA: one slab per perp market.
pub const QUOTER_SLAB_PDA_SEED: &[u8] = b"quoter_slab";

/// Signing seeds for a market's slab, the identity velocity signs every
/// external quoter CPI as. `market` must be the header's market index in
/// little-endian bytes, and `bump` must be the header's stored bump.
pub fn get_quoter_slab_signer_seeds<'a>(market: &'a [u8; 2], bump: &'a u8) -> [&'a [u8]; 3] {
    [QUOTER_SLAB_PDA_SEED, market, bytemuck::bytes_of(bump)]
}

impl QuoterSlabV0 {
    /// Where the slot region starts: the discriminator plus the header.
    pub const SLOT_REGION_OFFSET: usize = 8 + std::mem::size_of::<QuoterSlabV0>();

    /// Account space for a slab holding `capacity` slots.
    pub const fn space(capacity: usize) -> usize {
        Self::SLOT_REGION_OFFSET + capacity * std::mem::size_of::<QuoterSlotV0>()
    }
}

/// Check that the slot region is readable behind the account data. It must be
/// long enough for the header's declared capacity, and the slots must start
/// 8-aligned. The runtime aligns account data, so the alignment check only
/// fails on a malformed host-side harness. Returns the capacity.
fn validate_slab_data(data: &[u8]) -> Result<usize> {
    validate!(
        data.len() >= QuoterSlabV0::SLOT_REGION_OFFSET,
        ErrorCode::InvalidQuoterConfig,
        "quoter slab account is shorter than its header"
    )?;

    let header: &QuoterSlabV0 =
        bytemuck::from_bytes(&data[8..8 + std::mem::size_of::<QuoterSlabV0>()]);
    let capacity = header.capacity as usize;
    validate!(
        data.len() >= QuoterSlabV0::space(capacity),
        ErrorCode::InvalidQuoterConfig,
        "quoter slab declares {} slots but is too short to hold them",
        capacity
    )?;
    validate!(
        (data.as_ptr() as usize + QuoterSlabV0::SLOT_REGION_OFFSET)
            .is_multiple_of(std::mem::align_of::<QuoterSlotV0>()),
        ErrorCode::InvalidQuoterConfig,
        "quoter slab data is not aligned"
    )?;

    Ok(capacity)
}

/// The occupied slots, with their indexes. An index is the stable handle,
/// because a slot never moves while it is occupied.
pub fn occupied_slots(slots: &[QuoterSlotV0]) -> impl Iterator<Item = (usize, &QuoterSlotV0)> {
    slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| !slot.is_vacant())
}

/// The slot holding `entry`'s approved copy.
pub fn slot_for_entry(slots: &[QuoterSlotV0], entry: &Pubkey) -> Option<usize> {
    occupied_slots(slots)
        .find(|(_, slot)| slot.entry == *entry)
        .map(|(index, _)| index)
}

/// A vacant slot for a `Custom` quoter. Slot 0 is reserved for the market's
/// book, so the search starts at 1.
pub fn vacant_slot_index(slots: &[QuoterSlotV0]) -> Option<usize> {
    slots
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, slot)| slot.is_vacant())
        .map(|(index, _)| index)
}

/// The market's book. It sits in slot 0, and nothing else may occupy slot 0.
/// Approval enforces both rules, so the lookup is one read rather than a scan
/// on every book-touching instruction.
pub fn clob_slot_index(slots: &[QuoterSlotV0]) -> Option<usize> {
    slots
        .first()
        .filter(|slot| !slot.is_vacant() && slot.config.quoter_type == QuoterType::Clob)
        .map(|_| 0)
}

/// Quoters one transaction may consult. The account-lock budget sets it. A fill spends
/// roughly 15 locks before its first quoter, and each quoter costs two more that nothing
/// else shares, its program and its response account, against the 64 a transaction can
/// name. Eight leaves room for the maker accounts. Carrying more fails with an error.
pub const MAX_ROUTE_QUOTERS: usize = 8;

/// The slab loader's read surface. It is an extension trait, because the loader
/// is anchor's type and takes no inherent impl here.
pub trait QuoterSlabExt<'info> {
    /// The slot region, read-only.
    fn slots(&self) -> Result<std::cell::Ref<'_, [QuoterSlotV0]>>;

    /// The slot region, writable.
    fn slots_mut(&self) -> Result<std::cell::RefMut<'_, [QuoterSlotV0]>>;

    /// The market's book slot. It is an error when the slab holds no book or serves a
    /// different market, so a substituted slab cannot expose another market's book. It is
    /// not gated on `suspended` or `is_active`, because the removal paths must keep working
    /// on a killed or de-listed book. Callers that add flow gate on [`QuoterSlotV0::quotes`].
    fn clob_slot(&self, market_index: u16) -> Result<std::cell::Ref<'_, QuoterSlotV0>>;

    /// The consulted slots `tail` carries, by slot index. It is every occupied slot
    /// whose response account rides the transaction, in slab order and capped at
    /// [`MAX_ROUTE_QUOTERS`]. Carrying a slot's response account is the intent to
    /// consult it. It returns indexes rather than copies of the config.
    fn consulted_slots(&self, tail: &[AccountInfo<'info>]) -> Result<Vec<usize>>;
}

impl<'info> QuoterSlabExt<'info> for AccountLoader<'info, QuoterSlabV0> {
    fn slots(&self) -> Result<std::cell::Ref<'_, [QuoterSlotV0]>> {
        let info: &AccountInfo = self.as_ref();
        let data = info.try_borrow_data()?;
        let capacity = validate_slab_data(&data)?;
        Ok(std::cell::Ref::map(data, |data| {
            let tail = &data[QuoterSlabV0::SLOT_REGION_OFFSET..];
            bytemuck::cast_slice(&tail[..capacity * std::mem::size_of::<QuoterSlotV0>()])
        }))
    }

    fn slots_mut(&self) -> Result<std::cell::RefMut<'_, [QuoterSlotV0]>> {
        let info: &AccountInfo = self.as_ref();
        validate!(
            info.is_writable,
            ErrorCode::InvalidQuoterConfig,
            "quoter slab is not writable"
        )?;

        let data = info.try_borrow_mut_data()?;
        let capacity = validate_slab_data(&data)?;
        Ok(std::cell::RefMut::map(data, |data| {
            let tail = &mut data[QuoterSlabV0::SLOT_REGION_OFFSET..];
            bytemuck::cast_slice_mut(&mut tail[..capacity * std::mem::size_of::<QuoterSlotV0>()])
        }))
    }

    fn clob_slot(&self, market_index: u16) -> Result<std::cell::Ref<'_, QuoterSlotV0>> {
        let market = self.load()?.market;
        validate!(
            market == market_index,
            ErrorCode::InvalidQuoterConfig,
            "quoter slab is for market {}, call is for market {}",
            market,
            market_index
        )?;

        let slots = self.slots()?;
        let index = clob_slot_index(&slots).ok_or_else(|| {
            msg!("quoter slab for market {} holds no book", market);
            error!(ErrorCode::QuoterNotOnSlab)
        })?;

        Ok(std::cell::Ref::map(slots, |slots| &slots[index]))
    }

    fn consulted_slots(&self, tail: &[AccountInfo<'info>]) -> Result<Vec<usize>> {
        let slots = self.slots()?;
        let mut consulted = Vec::with_capacity(MAX_ROUTE_QUOTERS);
        for (index, slot) in occupied_slots(&slots) {
            if find_account(tail, &slot.config.response_account).is_none() {
                continue;
            }

            validate!(
                consulted.len() < MAX_ROUTE_QUOTERS,
                ErrorCode::TooManyQuotersConsulted,
                "a fill may consult at most {} quoters",
                MAX_ROUTE_QUOTERS
            )?;

            consulted.push(index);
        }

        Ok(consulted)
    }
}
