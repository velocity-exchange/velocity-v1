//! The quoter config and its staging entry.
//!
//! [`QuoterConfigV0`] is everything one quoter is: the CPI surface velocity
//! calls it on and the declarations that bound how it fills. It lives in two
//! places with two meanings. The [`QuoterV0`] staging entry here holds the
//! maker's proposal. A [`super::QuoterSlabV0`] slot holds the admin-approved
//! copy, which is the only copy a fill reads.

use {
    crate::{error::ErrorCode, msg, state::traits::Size, validate},
    anchor_lang::prelude::*,
    static_assertions::const_assert_eq,
};

/// Most accounts one quoter can register. Both CPI legs share the list, and
/// each leg names its accounts as indexes into it.
pub const MAX_QUOTER_ACCOUNTS: usize = 12;

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum QuoterType {
    Vamm,
    Clob,
    #[default]
    Custom,
}

impl QuoterType {
    /// Default routing priority at registration. A lower priority fills first.
    /// The gaps leave room for later kinds, such as a promoted quoter. The admin can adjust the value afterwards.
    pub fn default_priority(self) -> u8 {
        match self {
            QuoterType::Vamm => 0,
            QuoterType::Clob => 10,
            QuoterType::Custom => 20,
        }
    }

    /// Whether this quoter's depth was already reserved into its owner's
    /// `open_bids` or `open_asks` at placement. Reserved depth bounds a fill by
    /// a per-owner quote budget. Unreserved depth bounds it by initial margin on
    /// the base taken. Only `Clob` is reserved.
    pub fn depth_is_margin_reserved(self) -> bool {
        matches!(self, QuoterType::Clob)
    }

    /// Whether a fill unwinds this quoter's makers' open-order aggregates from
    /// its execute response (`completed_orders` and `cancelled`). There is
    /// something to unwind exactly when something was reserved. An unreserved
    /// quoter could decrement other loaded users' aggregates and free their margin.
    pub fn tracks_maker_aggregates(self) -> bool {
        self.depth_is_margin_reserved()
    }
}

#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct AmmAccountMeta {
    pub pubkey: Pubkey,
    /// Whether the account is passed writable to the quoter program. `is_signer`
    /// is not stored. The market's slab is the only account a quoter CPI ever
    /// receives signer privilege on, and [`super::wire::write_quoter_account_metas`]
    /// decides that by pubkey match rather than by registration.
    pub is_writable: bool,
    pub padding: [u8; 7],
}

const_assert_eq!(std::mem::size_of::<AmmAccountMeta>(), 40);

/// One quoter's whole configuration: the CPI surface velocity calls it on,
/// and the declarations that bound how it fills.
///
/// Held in two places with two meanings. On the [`QuoterV0`] staging entry it
/// is the maker's proposal, writable by the entry authority. In a
/// [`super::QuoterSlabV0`] slot it is the copy the admin approved, which is the
/// only copy a fill reads. A maker edit therefore never reaches flow until the
/// admin copies it in again.
#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct QuoterConfigV0 {
    /// The slot the approved program was last deployed at, read at approval and
    /// meaningful only in a slab slot. Zero when the loader cannot redeploy the
    /// program. Approval does not freeze the program, so an off-chain reader
    /// compares this to the live slot to know the code changed.
    pub approved_program_slot: u64,
    /// The book's placement rules, mirrored here by the attach
    /// (`update_perp_market_clob_quoter`) so the hot paths read a loaded field.
    /// Zero for a non-`Clob` entry and for a book no market has attached. A
    /// stale mirror degrades the remainder handling rather than failing a fill.
    pub book_tick_size: u64,
    pub book_min_order_size: u64,
    /// For a Custom quoter, the User this quoter may quote for. That user's
    /// authority creates the entry, so creation is consent. For the vAMM, the
    /// vAMM user. For a CLOB it is ignored, because execute may return balance
    /// changes for any user with resting orders on the CLOB.
    pub user: Pubkey,
    /// The external program invoked for `quote_v0` and `execute_v0`.
    pub program_id: Pubkey,
    /// Account owned by `program_id` that quote and execute responses are
    /// written into. Both legs' index lists must name it. A response is read at
    /// the pointer returned via return data, so a payload is not bound by the
    /// 1024-byte return-data cap. For a CLOB entry this is the book itself.
    pub response_account: Pubkey,
    /// Manages the staging entry. For a Custom quoter this is the quoted user's
    /// authority, enforced at creation with no handoff. The maker can therefore
    /// always kill their own quoter, because `is_active` writes through to the
    /// approved copy.
    pub authority: Pubkey,
    /// Maker-declared reprice region: the account bytes whose change means the
    /// quoter may quote differently now. Relay cross-discovery conditions wake
    /// on it, and `watch_len == 0` means the maker declared none, which leaves
    /// discovery to the poll. A missed reprice costs cross latency, never correctness.
    pub watch_account: Pubkey,
    /// Raw instruction discriminators on `program_id`. Stored rather than
    /// derived so non-Anchor programs can participate.
    pub quote_v0_discriminator: [u8; 8],
    pub execute_v0_discriminator: [u8; 8],
    /// The optional third leg, `quote_l3_v0`. It reports the resting orders
    /// behind a ladder and who each one belongs to. Zero means the quoter does
    /// not implement it, and a reader attributes the whole ladder to
    /// [`Self::user`]. A book is the exception, and this field is how it says so.
    pub quote_l3_v0_discriminator: [u8; 8],
    /// The one registered CPI account list. Only the first `accounts_count`
    /// entries are live. Each leg forwards a subset, in its own order, named by
    /// the index lists below. That leaves one list to vet, and a leg cannot
    /// smuggle an account the other leg's reviewer never saw.
    pub accounts: [AmmAccountMeta; MAX_QUOTER_ACCOUNTS],
    /// Indexes into `accounts` forwarded to `quote_v0` and `quote_l3_v0`, in
    /// CPI order. Only the first `quote_accounts_count` are live.
    pub quote_account_indexes: [u8; MAX_QUOTER_ACCOUNTS],
    /// Indexes into `accounts` forwarded to `execute_v0`, in CPI order. Only
    /// the first `execute_accounts_count` are live.
    pub execute_account_indexes: [u8; MAX_QUOTER_ACCOUNTS],
    pub watch_offset: u32,
    pub watch_len: u32,
    /// The furthest from oracle a fill on this entry may price, in
    /// MARGIN_PRECISION units. Zero means the market's own band stands. It
    /// writes through to the approved copy without re-vetting, because the band
    /// applies as the smaller of this and the market's. `Custom` entries only.
    pub max_oracle_deviation_bps: u32,
    pub book_default_activation_delay_slots: u32,
    /// Perp market index this quoter serves.
    pub market: u16,
    pub quoter_type: QuoterType,
    /// The authority's own on and off switch. The maker can always set it, and
    /// it writes through to the approved copy so a kill takes effect at once.
    pub is_active: bool,
    /// Routing priority. At one price, a lower-priority tier fills first, pro
    /// rata within a tier. It defaults by type: vAMM 0, CLOB 10, Custom 20.
    /// Only the admin sets it afterwards, never the maker.
    pub priority: u8,
    pub accounts_count: u8,
    pub quote_accounts_count: u8,
    pub execute_accounts_count: u8,
}

const_assert_eq!(std::mem::size_of::<QuoterConfigV0>(), 736);

impl QuoterConfigV0 {
    /// The oracle deviation a fill on this entry may reach, in MARGIN_PRECISION
    /// units. The market's band is the ceiling, so a maker's declaration can
    /// only bring it in.
    pub fn oracle_band(&self, market_margin_ratio_initial: u32) -> u32 {
        match self.max_oracle_deviation_bps {
            0 => market_margin_ratio_initial,
            declared => declared.min(market_margin_ratio_initial),
        }
    }

    /// The live registered account list.
    pub fn registered_accounts(&self) -> &[AmmAccountMeta] {
        &self.accounts[..(self.accounts_count as usize).min(MAX_QUOTER_ACCOUNTS)]
    }

    /// One leg's accounts, resolved through its index list.
    ///
    /// An index past `accounts_count` is an error rather than a clamp. A wrong
    /// index means the stored config is incoherent, and a CPI whose account
    /// list is shorter than registered answers about the wrong thing.
    pub fn leg_metas<'a>(
        &'a self,
        indexes: &'a [u8],
    ) -> Result<impl Iterator<Item = &'a AmmAccountMeta> + 'a> {
        let registered = self.registered_accounts();
        validate!(
            indexes.iter().all(|&i| (i as usize) < registered.len()),
            ErrorCode::InvalidQuoterConfig,
            "quoter leg index past the registered account list"
        )?;

        Ok(indexes.iter().map(move |&i| &registered[i as usize]))
    }

    pub fn quote_leg_indexes(&self) -> &[u8] {
        &self.quote_account_indexes[..(self.quote_accounts_count as usize).min(MAX_QUOTER_ACCOUNTS)]
    }

    pub fn execute_leg_indexes(&self) -> &[u8] {
        &self.execute_account_indexes
            [..(self.execute_accounts_count as usize).min(MAX_QUOTER_ACCOUNTS)]
    }
}

/// The staging half of the registry: one entry per perp market, quoter program
/// and quoted user. Nothing fills from it. `update_quoter_approved` copies it
/// into the market's [`super::QuoterSlabV0`], and a fill reads only that copy.
/// The entry's address is also the quoter's identity.
#[account(zero_copy(unsafe))]
#[derive(Eq, PartialEq, Debug)]
#[repr(C)]
pub struct QuoterV0 {
    pub config: QuoterConfigV0,
    pub padding: [u8; 48],
}

impl Default for QuoterV0 {
    fn default() -> Self {
        QuoterV0 {
            config: QuoterConfigV0::default(),
            padding: [0; 48],
        }
    }
}

// Zero-copy layout invariant (see docs/alignment-and-native-offsets.md):
// no u128 fields, size (incl. 8-byte discriminator) ≡ 8 (mod 16).
const_assert_eq!(std::mem::size_of::<QuoterV0>(), 784);
const_assert_eq!((QuoterV0::SIZE - 8) % 16, 0);

impl Size for QuoterV0 {
    const SIZE: usize = 792;
}

// `SIZE` is the allocation, which is the discriminator plus the struct. A
// literal that drifts from the struct allocates short, and the loader then
// panics at runtime.
const_assert_eq!(QuoterV0::SIZE, 8 + std::mem::size_of::<QuoterV0>());

/// PDA: one entry per (perp market, quoter program, quoted user).
pub const QUOTER_PDA_SEED: &[u8] = b"quoter";

/// Whether a registered CPI account list keeps clear of a market's book. An
/// entry that receives the book account could place, cancel, evict and fill on
/// it with the slab signature its own `execute_v0` holds. Approval and book
/// designation both read this predicate. A market that names no book bars nothing.
pub fn list_stays_off_the_book<'a>(
    registered: impl IntoIterator<Item = &'a Pubkey>,
    book: &Pubkey,
) -> bool {
    *book == Pubkey::default() || registered.into_iter().all(|key| key != book)
}

/// Reject a registered CPI account list that names velocity's vault authority.
///
/// That PDA is the SPL token authority on every `spot_market_vault` and
/// `insurance_fund_vault`, and the `User` and `UserStats` authority of the
/// protocol account. Velocity never signs a quoter CPI as it, because
/// [`super::wire::write_quoter_account_metas`] only ever marks the market's
/// slab. A quoter has no legitimate use for the key either, so registration
/// refuses it rather than downgrading it to a read-only slot.
pub fn validate_quoter_accounts<'a>(
    metas: impl IntoIterator<Item = (&'a Pubkey, bool)>,
    market_index: u16,
) -> Result<()> {
    let vault_authority = crate::state::pdas::velocity_signer();
    let slab = crate::state::pdas::quoter_slab(market_index);
    metas.into_iter().try_for_each(|(pubkey, is_writable)| {
        validate!(
            *pubkey != vault_authority,
            ErrorCode::InvalidQuoterConfig,
            "velocity's vault authority {} cannot be a quoter cpi account",
            vault_authority
        )?;

        // The slab rides every quoter CPI as the signer, and the runtime
        // refuses to lend an account writable that the caller holds read-only.
        // A list that asks for it writable therefore fails at the CPI, for a
        // reason the registrant never sees. Refuse it here instead.
        validate!(
            !(*pubkey == slab && is_writable),
            ErrorCode::InvalidQuoterConfig,
            "the market's quoter slab {} may only be registered read-only",
            slab
        )
    })?;

    Ok(())
}
