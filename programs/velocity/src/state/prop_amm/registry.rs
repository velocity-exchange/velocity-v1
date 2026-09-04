//! The quoter config and its staging entry.
//!
//! [`QuoterConfigV0`] is everything one quoter is: the CPI surface velocity
//! calls it on and the declarations that bound how it fills. It lives in two
//! places with two meanings — the maker's proposal on the [`QuoterV0`]
//! staging entry here, and the admin-approved copy in a
//! [`super::QuoterSlabV0`] slot, which is the only copy a fill reads.

use {
    crate::{error::ErrorCode, msg, state::traits::Size, validate},
    anchor_lang::prelude::*,
    static_assertions::const_assert_eq,
};

/// Max accounts one quoter can register, shared by both CPI legs: each leg
/// names its accounts as indexes into the one registered list.
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
    /// Default routing priority at registration (lower fills first). Gaps
    /// leave room to slot e.g. the DLOB migration bridge or promoted
    /// quoters; admin-adjustable afterwards.
    pub fn default_priority(self) -> u8 {
        match self {
            QuoterType::Vamm => 0,
            QuoterType::Clob => 10,
            QuoterType::Custom => 20,
        }
    }

    /// Whether a fill unwinds this quoter's makers' open-order aggregates from
    /// its execute response (`completed_orders` and `cancelled`).
    ///
    /// Only a `Clob` does. Its orders are margin-reserved through velocity at
    /// placement, so a fill or cull must decrement those reservations. A
    /// `Custom` quoter's depth is never reserved, so it has nothing to unwind —
    /// and letting one report completions or culls would let it decrement other
    /// loaded users' aggregates, release their trigger slots, and free their
    /// margin. Held as one predicate so the fill path cannot drift from the
    /// registration rule that only velocity's own CLOB is a `Clob`.
    pub fn tracks_maker_aggregates(self) -> bool {
        matches!(self, QuoterType::Clob)
    }
}

#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct AmmAccountMeta {
    pub pubkey: Pubkey,
    /// Whether the account is passed writable to the quoter program.
    /// `is_signer` is intentionally not stored: the only slot a quoter CPI
    /// ever receives signer privilege on is the market's slab, decided by
    /// pubkey match rather than by registration (see
    /// [`super::wire::write_quoter_account_metas`]).
    pub is_writable: bool,
    pub padding: [u8; 7],
}

const_assert_eq!(std::mem::size_of::<AmmAccountMeta>(), 40);

/// One quoter's whole configuration: the CPI surface velocity calls it on,
/// and the declarations that bound how it fills.
///
/// Held in two places with two meanings. On the [`QuoterV0`] staging entry it
/// is the maker's proposal, writable by the entry authority. In a
/// [`QuoterSlabV0`] slot it is the copy the admin approved, which is the only
/// copy a fill reads — so a maker edit never reaches flow until the admin
/// copies it in again.
#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct QuoterConfigV0 {
    /// The slot the approved program was last deployed at, read from its
    /// program-data account when the admin approved this config. Zero when
    /// the program sits on a loader that cannot redeploy it. Meaningful only
    /// in a slab slot; the staging copy holds the last approval's figure.
    ///
    /// Approval does not freeze the program. A maker may upgrade, and the
    /// bounds on a quoter hold either way: a `Custom` entry can move only its
    /// own registered user, at a price held to its own quote and to the taker's
    /// limit, sized inside its own margin. So an upgrade can lose the maker's
    /// money and cannot take anyone else's.
    ///
    /// What it can still do is quote and not deliver, which costs the taker a
    /// fill. That is why the slot is recorded: an off-chain reader compares it
    /// to the live one and knows the code changed, rather than waiting to infer
    /// it from behaviour.
    pub approved_program_slot: u64,
    /// The book's placement rules, mirrored here by the attach
    /// (`update_perp_market_clob_quoter`) so the hot paths read a loaded
    /// field instead of CPI'ing `order_rules_v0`
    /// Zero for non-`Clob` entries and for a book no market has
    /// attached. Changing the book's rules requires re-running the attach:
    /// a stale mirror degrades gracefully (a wrong tick or minimum drops
    /// the remainder to the plain cancel; a stale-zero delay routes an
    /// unattested taker synchronously where it should have rested)
    pub book_tick_size: u64,
    pub book_min_order_size: u64,
    /// For Custom quoters, the User this quoter is allowed to quote for.
    /// That user's authority creates the entry, so creation is consent. For
    /// vAMM, the vAMM user. For CLOB, ignored: execute may return balance
    /// changes for any user with resting orders on the CLOB.
    pub user: Pubkey,
    /// The external program invoked for `quote_v0` / `execute_v0`.
    pub program_id: Pubkey,
    /// Account owned by `program_id` that quote/execute responses are written
    /// into; must be named by both legs' index lists. Responses are read at
    /// the pointer returned via return data, so payloads aren't bound by the
    /// 1024-byte return-data cap.
    ///
    /// For CLOB entries this is the book itself — the CLOB's response region
    /// lives in its market account — which is what lets velocity read the
    /// resting orders an execute may touch without a second registered
    /// account to trust.
    pub response_account: Pubkey,
    /// Manages the staging entry. For Custom quoters this is the quoted
    /// user's authority (enforced at creation, no handoff), so the maker can
    /// always kill their own quoter (`is_active` writes through to the
    /// approved copy); the admin vets the CPI surface by copying it into the
    /// slab.
    pub authority: Pubkey,
    /// Maker-declared reprice region: the account bytes whose change means
    /// "this quoter may quote differently now" (a midpoint's mid region, a
    /// custom AMM's parameter block). Relay cross-discovery conditions wake
    /// on it; `watch_len == 0` means no declaration (poll-only discovery).
    /// Config like everything else here: vetted by the admin at the copy into
    /// the slab — a watch that misses reprices only costs the maker cross
    /// latency, never correctness (the poll is the floor).
    pub watch_account: Pubkey,
    /// Raw instruction discriminators on `program_id`. Stored rather than
    /// derived so non-Anchor programs can participate.
    pub quote_v0_discriminator: [u8; 8],
    pub execute_v0_discriminator: [u8; 8],
    /// The optional third leg: `quote_l3_v0`, which reports the resting
    /// orders behind a ladder and who each belongs to. Zero means the quoter
    /// does not implement it, and a reader attributes the whole ladder to
    /// [`Self::user`] — which is right for every quoter that fills from one
    /// account. A book is the exception, and this is how it says so.
    pub quote_l3_v0_discriminator: [u8; 8],
    /// The one registered CPI account list. Only the first `accounts_count`
    /// entries are live. Each leg forwards a subset, in its own order, named
    /// by the index lists below — one list to vet, and a leg cannot smuggle
    /// an account the other leg's reviewer never saw.
    pub accounts: [AmmAccountMeta; MAX_QUOTER_ACCOUNTS],
    /// Indexes into `accounts` forwarded to `quote_v0` (and `quote_l3_v0`),
    /// in CPI order. Only the first `quote_accounts_count` are live.
    pub quote_account_indexes: [u8; MAX_QUOTER_ACCOUNTS],
    /// Indexes into `accounts` forwarded to `execute_v0`, in CPI order. Only
    /// the first `execute_accounts_count` are live.
    pub execute_account_indexes: [u8; MAX_QUOTER_ACCOUNTS],
    pub watch_offset: u32,
    pub watch_len: u32,
    /// The furthest from oracle a fill on this entry may price, in
    /// MARGIN_PRECISION units, so one unit is one basis point. Zero means the
    /// entry declares nothing and the market's own band stands.
    ///
    /// A maker sets this to cap what its own program can lose if that program
    /// is compromised. Velocity already bounds every external leg by the
    /// market's band, and that band is sized for a market rather than for one
    /// quoter's risk appetite; this is how a quoter asks for a tighter one.
    ///
    /// Unlike the rest of the config it writes through to the approved copy
    /// without re-vetting. The band applies as the smaller of this and the
    /// market's, so no value it can hold is wider than the one the admin
    /// vetted, and a maker tightening it during an incident must not wait.
    ///
    /// `Custom` entries only. A book fills third parties, so a band on one
    /// would let its entry authority revert other people's fills.
    pub max_oracle_deviation_bps: u32,
    pub book_default_activation_delay_slots: u32,
    /// Perp market index this quoter serves.
    pub market: u16,
    pub quoter_type: QuoterType,
    /// The authority's own on/off switch — always settable by the maker, and
    /// written through to the approved copy so a kill takes effect at once.
    pub is_active: bool,
    /// Routing priority: at a price, lower-priority tiers fill first, pro
    /// rata within a tier. Defaults by type (vAMM 0, CLOB 10, Custom 20);
    /// admin-set thereafter — never by the maker.
    pub priority: u8,
    pub accounts_count: u8,
    pub quote_accounts_count: u8,
    pub execute_accounts_count: u8,
}

const_assert_eq!(std::mem::size_of::<QuoterConfigV0>(), 736);

impl QuoterConfigV0 {
    /// The oracle deviation a fill on this entry may reach, in
    /// MARGIN_PRECISION units.
    ///
    /// The market's band is the ceiling. A maker's declaration can only bring
    /// it in, which is what makes the declaration safe to take from the maker
    /// rather than from the admin.
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

    /// One leg's accounts, resolved through its index list. Errors on an
    /// index past `accounts_count` rather than clamping: a wrong index means
    /// the stored config is incoherent, and a CPI whose account list is
    /// silently shorter than registered answers about the wrong thing.
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

/// The staging half of the registry: one entry per (perp market, quoter
/// program, quoted user), created by the quoted user's authority, holding the
/// config that authority proposes. Nothing fills from it — the admin copies
/// it into the market's [`QuoterSlabV0`] (`update_quoter_approved`), and
/// fills read only that copy. A maker edit here therefore never reaches flow
/// until the admin copies again, and the approved copy keeps serving its
/// vetted config in the meantime.
///
/// The entry's address is also the quoter's *identity*: signed routes name
/// it, relay conditions reference it, and
/// its slab slot records it.
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

// `SIZE` is the allocation (discriminator + struct); a literal that drifts
// from the struct allocates short and the loader panics at runtime.
const_assert_eq!(QuoterV0::SIZE, 8 + std::mem::size_of::<QuoterV0>());

/// PDA: one entry per (perp market, quoter program, quoted user).
pub const QUOTER_PDA_SEED: &[u8] = b"quoter";

/// Reject a registered CPI account list that names velocity's vault authority.
///
/// That PDA is the SPL token authority on every `spot_market_vault` and
/// `insurance_fund_vault` and the `User`/`UserStats` authority of the protocol
/// account. Velocity never signs a quoter CPI as it —
/// [`super::wire::write_quoter_account_metas`] only ever marks the market's
/// slab — but a quoter has no legitimate use for the key either, so naming it
/// is refused at registration rather than silently downgraded to a read-only
/// slot.
pub fn validate_quoter_accounts<'a>(pubkeys: impl IntoIterator<Item = &'a Pubkey>) -> Result<()> {
    let vault_authority = crate::state::pdas::velocity_signer();
    pubkeys.into_iter().try_for_each(|pubkey| {
        validate!(
            *pubkey != vault_authority,
            ErrorCode::InvalidQuoterConfig,
            "velocity's vault authority {} cannot be a quoter cpi account",
            vault_authority
        )
    })?;
    Ok(())
}
