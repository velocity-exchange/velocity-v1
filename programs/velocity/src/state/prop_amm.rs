//! Quoter registry: [`QuoterV0`] entries name an external quoter program
//! (CLOB, Midpoint, custom PropAMMs; the vAMM is in-program) plus the CPI
//! surface velocity needs to call it — discriminators, account lists, and the
//! response account. [`QuoterV0::quote`]/[`QuoterV0::execute`] are the CPI
//! legs the router fill uses. Registration ixs live in
//! `instructions::quoter_registry`.
//!
//! Every CPI out of this module signs as `quoter_signer` (see
//! `crate::signer`), never as the vault authority.

use {
    crate::{
        error::ErrorCode, msg, signer::get_quoter_signer_seeds, state::traits::Size, validate,
    },
    anchor_lang::prelude::*,
    solana_program::{
        instruction::{AccountMeta, Instruction},
        program::{get_return_data, invoke_signed},
    },
    static_assertions::const_assert_eq,
    std::{collections::BTreeMap, convert::TryInto},
};

#[cfg(test)]
mod tests;

/// Max accounts that can be registered per CPI leg (quote / execute).
pub const MAX_QUOTER_ACCOUNTS: usize = 32;

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
}

#[account(zero_copy(unsafe))]
#[derive(Eq, PartialEq, Debug, Default)]
#[repr(C)]
pub struct QuoterV0 {
    /// For Custom quoters, the User this quoter is allowed to quote for.
    /// That user's authority creates the entry, so creation is consent. For
    /// vAMM, the vAMM user. For CLOB, ignored: execute may return balance
    /// changes for any user with resting orders on the CLOB.
    pub user: Pubkey,
    /// The external program invoked for `quote_v0` / `execute_v0`.
    pub program_id: Pubkey,
    /// Account owned by `program_id` that quote/execute responses are written
    /// into; must be registered in both account lists. Responses are read at
    /// the pointer returned via return data, so payloads aren't bound by the
    /// 1024-byte return-data cap.
    ///
    /// For CLOB entries this is the book itself — the CLOB's response region
    /// lives in its market account — which is what lets velocity read the
    /// resting orders an execute may touch without a second registered
    /// account to trust. Callers that depend on that re-derive the book's
    /// market index from its bytes rather than assume it.
    pub response_account: Pubkey,
    /// Manages this registry entry. For Custom quoters this is the quoted
    /// user's authority (enforced at creation, no handoff), so the maker can
    /// always kill their own quoter (`is_active`); the admin vets the CPI
    /// surface (`is_approved`), which any config change resets.
    pub authority: Pubkey,
    /// Raw instruction discriminators on `program_id`. Stored rather than
    /// derived so non-Anchor programs can participate.
    pub quote_v0_discriminator: [u8; 8],
    pub execute_v0_discriminator: [u8; 8],
    /// Accounts forwarded to `quote_v0`, in order. Only the first
    /// `quote_accounts_count` entries are live.
    pub quote_accounts: [AmmAccountMeta; MAX_QUOTER_ACCOUNTS],
    /// Accounts forwarded to `execute_v0`, in order. Only the first
    /// `execute_accounts_count` entries are live.
    pub execute_accounts: [AmmAccountMeta; MAX_QUOTER_ACCOUNTS],
    /// Perp market index this quoter serves.
    pub market: u16,
    pub quoter_type: QuoterType,
    /// The authority's own on/off switch — always settable by the maker.
    pub is_active: bool,
    /// Admin vetting of the CPI surface; reset by any config change.
    pub is_approved: bool,
    /// Routing priority: at a price, lower-priority tiers fill first, pro
    /// rata within a tier. Defaults by type (vAMM 0, CLOB 10, Custom 20);
    /// admin-set thereafter — never by the maker.
    pub priority: u8,
    pub quote_accounts_count: u8,
    pub execute_accounts_count: u8,
    /// Maker-declared reprice region: the account bytes whose change means
    /// "this quoter may quote differently now" (a midpoint's mid region, a
    /// custom AMM's parameter block). Relay cross-discovery conditions wake
    /// on it; `watch_len == 0` means no declaration (poll-only discovery).
    /// Config like everything else here: vetted by the admin via the
    /// `is_approved` reset — a watch that misses reprices only costs the
    /// maker cross latency, never correctness (the poll is the floor).
    pub watch_offset: u32,
    pub watch_len: u32,
    pub watch_account: Pubkey,
}

// Zero-copy layout invariant (see docs/alignment-and-native-offsets.md):
// no u128 fields, size (incl. 8-byte discriminator) ≡ 8 (mod 16).
const_assert_eq!(std::mem::size_of::<QuoterV0>(), 2752);
const_assert_eq!((QuoterV0::SIZE - 8) % 16, 0);

impl Size for QuoterV0 {
    const SIZE: usize = 2760;
}

/// PDA: one entry per (perp market, quoter program, quoted user).
pub const QUOTER_PDA_SEED: &[u8] = b"quoter";

/// Which CPI leg an account-list update targets.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub enum QuoterCpiLeg {
    Quote,
    Execute,
}

#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct AmmAccountMeta {
    pub pubkey: Pubkey,
    /// Whether the account is passed writable to the quoter program.
    /// `is_signer` is intentionally not stored: the only slot a quoter CPI
    /// ever receives signer privilege on is `quoter_signer`, decided by
    /// pubkey match rather than by registration (see [`quoter_account_metas`]).
    pub is_writable: bool,
    pub padding: [u8; 7],
}

const_assert_eq!(std::mem::size_of::<AmmAccountMeta>(), 40);

/// Reject a registered CPI account list that names velocity's vault authority.
///
/// That PDA is the SPL token authority on every `spot_market_vault` and
/// `insurance_fund_vault` and the `User`/`UserStats` authority of the protocol
/// account. Velocity never signs a quoter CPI as it — [`quoter_account_metas`]
/// only ever marks `quoter_signer` — but a quoter has no legitimate use for
/// the key either, so naming it is refused at registration rather than
/// silently downgraded to a read-only slot.
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

/// Account metas for one quoter CPI leg.
///
/// NEVER forward outer signer privilege. Signer status propagates through CPI,
/// so a quoter handed the taker's wallet as a signer could CPI to the
/// system/token program and drain it. Quoters that need to know who signed the
/// outer transaction (e.g. the `flow_authority` attestation) introspect the
/// instructions sysvar instead.
///
/// The single signer is `quoter_signer` (velocity signing as itself) — a
/// registered slot for it is how a quoter authenticates that velocity, not an
/// arbitrary caller, is invoking it. That PDA is the authority on nothing, so
/// a quoter that forwards the signature onward gains nothing by it.
fn quoter_account_metas(registered: &[AmmAccountMeta], quoter_signer: &Pubkey) -> Vec<AccountMeta> {
    registered
        .iter()
        .map(|meta| AccountMeta {
            pubkey: meta.pubkey,
            is_signer: meta.pubkey == *quoter_signer,
            is_writable: meta.is_writable,
        })
        .collect()
}

/// Taker direction, from the taker's perspective. Borsh wire encoding
/// (Long = 0, Short = 1) deliberately matches
/// [`crate::controller::position::PositionDirection`], but the CPI ABI gets
/// its own enum so it can never drift with internal refactors.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub enum Direction {
    Long,
    Short,
}

impl Direction {
    /// The book side a taker of this direction consumes.
    pub fn clob_side(self) -> ClobSide {
        match self {
            Direction::Long => ClobSide::Ask,
            Direction::Short => ClobSide::Bid,
        }
    }

    pub fn to_position_direction(self) -> crate::controller::position::PositionDirection {
        match self {
            Direction::Long => crate::controller::position::PositionDirection::Long,
            Direction::Short => crate::controller::position::PositionDirection::Short,
        }
    }
}

/// A velocity user on the quoter wire, in its *derivable* form: authority
/// wallet + sub-account index. Both the `User` PDA and the `UserStats` PDA
/// derive from it, so an off-chain reader (a relay resolver staging a
/// crank) can reach every user-derived account from a quoter's state alone
/// — a stored `User` key is a dead end (its authority lives inside account
/// data the reader can't load). Velocity resolves refs against its loaded
/// users by field match, never by PDA derivation, so the hot path pays
/// nothing for this.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug, Default)]
pub struct ClobUserRefV0 {
    pub authority: Pubkey,
    pub sub_account_id: u16,
}

impl ClobUserRefV0 {
    pub const ZERO: Self = Self {
        authority: Pubkey::new_from_array([0u8; 32]),
        sub_account_id: 0,
    };
}

/// Borsh width of a [`ClobUserRefV0`].
pub const CLOB_USER_REF_BYTES: usize = 34;
const_assert_eq!(std::mem::size_of::<ClobUserRefV0>(), CLOB_USER_REF_BYTES);

/// Capacity of [`QuoterUserSetV0`].
///
/// The set velocity forwards is its loaded maker/referrer map, and every
/// entry there is a distinct `User` account the transaction locked. Solana
/// caps a transaction at 64 account locks, and a router fill spends 15 of
/// them before any maker: the velocity program, `State`, the filler's
/// signer, the filler `User`+`UserStats`, the taker `User`+`UserStats`, the
/// perp market and its oracle, the quote spot market and its oracle, then —
/// for an external quoter to exist at all — its registry entry, its program,
/// its response account and velocity's signer PDA. The remaining 49 locks
/// must also cover at least one `UserStats` (shared across sub-accounts of
/// one authority in the best case), so 48 `User`s is the most a landed fill
/// can carry. A larger set is therefore unreachable, and velocity treats it
/// as an error rather than silently truncating the set a quoter matches
/// against.
pub const MAX_QUOTER_WIRE_USERS: usize = 48;

/// The loaded-user set on the quoter wire: a fixed array plus a live count,
/// so passing it costs no allocation on a path that runs it once per quoter
/// per fill. Fixed width is also what lets both sides of the CPI decode it
/// zero-copy from a layout neither has to negotiate.
///
/// An empty set means unrestricted — what a `None` meant before — and is
/// only used by callers that settle nothing (the quote view, cross
/// discovery). A quoter reading a non-empty set must skip liquidity whose
/// owner is absent from it: velocity cannot settle a balance change for a
/// `User` it did not load, so such a fill is refused wholesale.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct QuoterUserSetV0 {
    /// Live entries at the head of `users`; the tail is undefined.
    pub len: u8,
    pub users: [ClobUserRefV0; MAX_QUOTER_WIRE_USERS],
}

/// Upper bound on a quote/execute CPI's instruction data: discriminator,
/// direction, size, the fixed-width user set, and an optional taker ref.
/// Reserved in one shot so no intermediate buffer is leaked.
pub const QUOTER_CPI_DATA_CAPACITY: usize =
    8 + 1 + 8 + QUOTER_USER_SET_BYTES + 1 + CLOB_USER_REF_BYTES;

/// Encoded width of a [`QuoterUserSetV0`] on the wire. Pinned here and
/// against the CLOB's `UserSetV0` (`anchor-v2`, wincode) — the two must
/// agree byte for byte.
pub const QUOTER_USER_SET_BYTES: usize = 1 + MAX_QUOTER_WIRE_USERS * CLOB_USER_REF_BYTES;

impl Default for QuoterUserSetV0 {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl QuoterUserSetV0 {
    /// Unrestricted: every user the quoter holds liquidity for is fair game.
    pub const EMPTY: Self = Self {
        len: 0,
        users: [ClobUserRefV0::ZERO; MAX_QUOTER_WIRE_USERS],
    };

    pub fn as_slice(&self) -> &[ClobUserRefV0] {
        &self.users[..self.len as usize]
    }

    pub fn contains(&self, user: &ClobUserRefV0) -> bool {
        self.as_slice().contains(user)
    }
}

/// The loaded-user set as velocity holds it: a heap slice, capped at the
/// wire's capacity, serialized to the wire's fixed width by
/// [`QuoterUserSetRef`].
///
/// Velocity deliberately never materializes a [`QuoterUserSetV0`] value. At
/// 1,633 bytes, one lands in a 4 KB SBF frame and the fill and cross-match
/// entrypoints overflow it — the linker says so at build time ("overflows
/// the maximum allowed frame space"), and at runtime it is an access
/// violation several frames deep. The fixed array is a property of the
/// *wire*, which is what lets each quoter decode it in place; it is not a
/// shape velocity has to hold in a register window.
pub fn quoter_wire_users(
    refs: impl IntoIterator<Item = ClobUserRefV0>,
) -> crate::error::VelocityResult<Vec<ClobUserRefV0>> {
    let users: Vec<ClobUserRefV0> = refs.into_iter().collect();
    validate!(
        users.len() <= MAX_QUOTER_WIRE_USERS,
        ErrorCode::TooManyQuoterWireUsers,
        "{} loaded users to forward to a quoter exceeds the wire's {}",
        users.len(),
        MAX_QUOTER_WIRE_USERS
    )?;
    Ok(users)
}

/// A borrowed user set, written to the wire at [`QuoterUserSetV0`]'s exact
/// fixed width: the live count, the live entries, then a zeroed tail. Byte
/// for byte what serializing a `QuoterUserSetV0` produces, with nothing
/// larger than one entry ever on the stack.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct QuoterUserSetRef<'a>(pub &'a [ClobUserRefV0]);

impl QuoterUserSetRef<'_> {
    /// Unrestricted — used by callers that settle nothing (the quote view,
    /// cross discovery).
    pub const EMPTY: Self = Self(&[]);
}

impl AnchorSerialize for QuoterUserSetRef<'_> {
    fn serialize<W: std::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        (self.0.len() as u8).serialize(writer)?;
        for user in self.0 {
            user.serialize(writer)?;
        }
        for _ in self.0.len()..MAX_QUOTER_WIRE_USERS {
            ClobUserRefV0::ZERO.serialize(writer)?;
        }
        Ok(())
    }
}

/// Borrowed, not owned: a [`QuoterUserSetV0`] is 1,633 bytes, and an SBF
/// stack frame is 4 KB. Owning it here put one copy in the caller's frame
/// per quoter plus another inside the CPI leg, which overflowed the fill
/// path's frame at runtime while every host-side test passed. These args are
/// only ever serialized (each quoter decodes its own mirror), so a reference
/// costs nothing on the wire.
///
/// `AnchorSerialize` is written by hand rather than derived: the derive also
/// emits an `IdlBuild` impl under the `idl-build` feature, and anchor's IDL
/// generator rejects a type with a lifetime ("Unsupported generic
/// argument"). These args are outbound CPI data that no client decodes from
/// our IDL, so they belong nowhere in it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct QuoteArgsV0<'a> {
    pub direction: Direction,
    /// Base size the taker wants filled.
    pub size: u64,
    /// `User`s velocity has loaded and can settle balance changes for.
    /// Quoters must not fill anyone else (velocity rejects the response
    /// otherwise). Empty = unrestricted, for off-chain quote discovery.
    pub users: QuoterUserSetRef<'a>,
    /// The taker's `User`: quoters must skip the taker's own resting
    /// liquidity (self-trade prevention) — a balance change for this user
    /// is rejected.
    pub taker: Option<ClobUserRefV0>,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct QuoteResponseV0 {
    /// Levels the quoter will fill at, best price first.
    pub levels: Vec<PriceLevel>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct PriceLevel {
    pub price: u64,
    pub size: u64,
}

/// Returned via return data by `quote_v0`/`execute_v0`: where in the quoter's
/// `response_account` the borsh response was written.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct ResponsePointerV0 {
    pub offset: u32,
    pub len: u32,
}

/// Borrowed, and hand-serialized, for the same reasons as [`QuoteArgsV0`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExecuteArgsV0<'a> {
    pub direction: Direction,
    /// Base size to fill. The quoter may partially fill; the actual fill is
    /// whatever the returned balance changes sum to.
    pub size: u64,
    /// Same contract as [`QuoteArgsV0::users`]; velocity always passes the
    /// loaded set here.
    pub users: QuoterUserSetRef<'a>,
    /// Same contract as [`QuoteArgsV0::taker`].
    pub taker: Option<ClobUserRefV0>,
}

impl AnchorSerialize for QuoteArgsV0<'_> {
    fn serialize<W: std::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        self.direction.serialize(writer)?;
        self.size.serialize(writer)?;
        self.users.serialize(writer)?;
        self.taker.serialize(writer)
    }
}

impl AnchorSerialize for ExecuteArgsV0<'_> {
    fn serialize<W: std::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        self.direction.serialize(writer)?;
        self.size.serialize(writer)?;
        self.users.serialize(writer)?;
        self.taker.serialize(writer)
    }
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct ExecuteResponseV0 {
    pub balance_changes: Vec<UserBalanceChange>,
    /// Sub-min remainders the quoter removed with this fill; velocity
    /// decrements the maker's open-order aggregates (that maker was just
    /// filled, so their `User` is loaded).
    pub cancelled: Vec<CancelledRemainderV0>,
}

/// One order a CLOB removed as a sub-min remainder of a fill.
///
/// **`base_asset_amount` and the completed-order ids beside it are taken on
/// faith, and that is a first-party-code assumption, not a verified one.**
/// They release a maker's margin reservation and decrement their open-order
/// counts, and that maker is an ordinary velocity user resting on the book —
/// not the quoter's own account, so the subject rule bounds *whose* books an
/// entry may touch but not whether it described what it did to them.
/// Over-reporting frees more reservation than the order held, which
/// understates that user's margin requirement; an id for an order still live
/// on the book frees a placed trigger's shadow while the book keeps the size.
///
/// Two things make it acceptable rather than a hole. Only a `QuoterType::Clob`
/// entry reaches this path at all (a Custom quoter's depth is never reserved
/// through velocity, so there is nothing to unwind), and velocity already
/// takes the same numbers on faith from the same program on every removal
/// path — `ClobRemovedOrderV0` out of `cancel_order_v0`, `evict_worst_v0` and
/// `remove_expired_v0` drives the identical unwinding. Verifying only this
/// one would leave four equivalent routes open.
///
/// So the assumption is: **a Clob-typed registry entry runs code we ship.**
/// Nothing in the program enforces that — `is_approved` is admin vetting of a
/// CPI surface, not a program-id allowlist — so approving a third-party CLOB
/// is what would turn this into a real exposure. At that point these amounts
/// must be checked against `clob_resting_prefix`, read before execute
/// consumes the nodes, and the same check owed to the removal cranks.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct CancelledRemainderV0 {
    pub user: ClobUserRefV0,
    pub order_id: u64,
    pub base_asset_amount: u64,
}

/// The CLOB's book side, as encoded on its wire (borsh enum tag).
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub enum ClobSide {
    Bid,
    Ask,
}

impl ClobSide {
    /// The maker position direction a resting order on this side represents.
    pub fn to_position_direction(self) -> crate::controller::position::PositionDirection {
        match self {
            ClobSide::Bid => crate::controller::position::PositionDirection::Long,
            ClobSide::Ask => crate::controller::position::PositionDirection::Short,
        }
    }
}

/// Order handle on the CLOB: an O(1) node hint verified against the order id
/// there, so a stale hint fails closed on the CLOB side.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct ClobOrderRefV0 {
    pub node_index: u32,
    pub order_id: u64,
}

/// Wire form of an order the CLOB removed — return data of its
/// cancel/evict/expire ixs, so velocity can decrement the maker's
/// open-order aggregates by the remaining size on the right side.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct ClobRemovedOrderV0 {
    pub user: ClobUserRefV0,
    pub order_id: u64,
    pub price: u64,
    pub base_asset_amount: u64,
    pub side: ClobSide,
    /// The removed order was taker-origin — a migrated taker remainder.
    ///
    /// The only place the CLOB reports the flag, and what tells velocity
    /// which side of a cross it is resolving was demanding liquidity, hence
    /// which side's price the match settles at. Trailing, so every offset
    /// before it is unchanged.
    pub taker_origin: bool,
}

/// `place_order_v0` args on the CLOB wire.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct ClobPlaceOrderArgsV0 {
    pub side: ClobSide,
    pub price: u64,
    pub base_asset_amount: u64,
    /// None = the CLOB market's default activation delay. Zero is allowed —
    /// velocity owns attestation policy.
    pub activation_delay_slots: Option<u32>,
    pub max_ts: i64,
    /// The user the order settles against, in derivable form (velocity
    /// verified control before the CPI).
    pub user: ClobUserRefV0,
    /// Mark the order taker-origin on the book: it is an unfilled taker
    /// remainder velocity migrated there, not a quote its owner chose to
    /// post. Only velocity knows that, which is why the CLOB takes it as an
    /// argument. It changes two things on the book — the order cannot be
    /// taken while a live counterparty crosses it (so the improvement cannot
    /// be won by landing a transaction at the activation slot), and a cross
    /// involving it settles at the counterparty's price.
    pub taker_origin: bool,
}

/// `cancel_order_v0` args on the CLOB wire.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct ClobCancelOrderArgsV0 {
    pub order_ref: ClobOrderRefV0,
    /// Owner of the order (verified against the node on the CLOB side).
    pub user: ClobUserRefV0,
}

/// `evict_worst_v0` args on the CLOB wire.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct ClobEvictWorstArgsV0 {
    pub side: ClobSide,
}

/// `remove_expired_v0` args on the CLOB wire.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct ClobRemoveExpiredArgsV0 {
    pub order_ref: ClobOrderRefV0,
}

/// Anchor-default discriminators (`sha256("global:<name>")[..8]`) of the CLOB
/// ixs velocity CPIs directly (place/cancel are velocity-mediated and not part
/// of the registry's quote/execute surface, so they aren't stored per entry).
/// Nothing outside [`ClobMarket`] should reference these — it is the one
/// place that speaks this wire.
pub const CLOB_PLACE_ORDER_V0_DISCRIMINATOR: [u8; 8] = [100, 204, 57, 226, 245, 228, 61, 187];
pub const CLOB_CANCEL_ORDER_V0_DISCRIMINATOR: [u8; 8] = [70, 91, 225, 16, 228, 203, 124, 174];
pub const CLOB_EVICT_WORST_V0_DISCRIMINATOR: [u8; 8] = [106, 60, 27, 129, 80, 27, 37, 73];
pub const CLOB_REMOVE_EXPIRED_V0_DISCRIMINATOR: [u8; 8] = [241, 135, 215, 18, 254, 107, 179, 119];

/// The velocity-mediated CLOB CPI surface, bound to one book: the three
/// accounts every call takes plus the signer nonce that lets velocity sign as
/// the book's `place_authority`.
///
/// This is the *only* place in the program that speaks the CLOB's wire — the
/// discriminators above, the borsh arg encoding, `invoke_signed` with the
/// fixed `[market (w), quoter_signer (s)]` account pair, and the
/// return-data decode (writer-checked, so a program the CLOB CPI'd into
/// can't spoof the response). Every caller — placement, cancel, the
/// evict/expire cranks, force-cancel — goes through a method here.
///
/// `execute_v0` is deliberately absent: that leg is the *registry* wire every
/// quoter type shares ([`QuoterV0::execute`] → `invoke_quoter`), whose
/// account list is per-entry registered rather than this fixed pair, and it
/// already has exactly one implementation.
pub struct ClobMarket<'a, 'info> {
    /// The book account, passed writable.
    pub market: &'a AccountInfo<'info>,
    /// The registered CLOB program.
    pub program: &'a AccountInfo<'info>,
    /// The quoter CPI signer PDA — what a book's `place_authority` is set to.
    /// The CLOB gates place/cancel/evict/expire *and* `execute_v0` on that one
    /// field, so this leg and the registry's execute leg necessarily sign as
    /// the same key; that key is `quoter_signer` rather than the vault
    /// authority so no external program ever receives a signature that can
    /// move protocol funds.
    pub quoter_signer: &'a AccountInfo<'info>,
    pub quoter_signer_nonce: u8,
}

impl<'a, 'info> ClobMarket<'a, 'info> {
    /// Bind to the book a CLOB registry entry names. Checks what every
    /// caller needs: the entry is a CLOB, it serves `market_index`, and this
    /// book is one of the accounts the admin vetted onto the entry (so a
    /// caller can't point a valid entry at an arbitrary account it owns).
    ///
    /// Deliberately *not* gated on `is_active`/`is_approved`: those mean
    /// "may take new flow", and the removal paths must keep working on a
    /// killed or de-listed book. Placement applies that gate itself.
    pub fn from_quoter(
        quoter: &QuoterV0,
        market_index: u16,
        market: &'a AccountInfo<'info>,
        program: &'a AccountInfo<'info>,
        quoter_signer: &'a AccountInfo<'info>,
        quoter_signer_nonce: u8,
    ) -> Result<Self> {
        quoter.validate_clob_book(market_index, &market.key())?;
        Ok(Self {
            market,
            program,
            quoter_signer,
            quoter_signer_nonce,
        })
    }

    /// Rest a new order on the book; returns the CLOB's handle for it.
    pub fn place(&self, args: ClobPlaceOrderArgsV0) -> Result<ClobOrderRefV0> {
        self.invoke(&CLOB_PLACE_ORDER_V0_DISCRIMINATOR, &args, "place")
    }

    /// Pull one order off the book; returns what was removed so the caller
    /// can unwind the maker's aggregates by the remaining size.
    pub fn cancel(&self, args: ClobCancelOrderArgsV0) -> Result<ClobRemovedOrderV0> {
        self.invoke(&CLOB_CANCEL_ORDER_V0_DISCRIMINATOR, &args, "cancel")
    }

    /// Reclaim the hinted expired order (the CLOB re-checks that it is due).
    pub fn remove_expired(&self, args: ClobRemoveExpiredArgsV0) -> Result<ClobRemovedOrderV0> {
        self.invoke(
            &CLOB_REMOVE_EXPIRED_V0_DISCRIMINATOR,
            &args,
            "remove expired",
        )
    }

    /// Reclaim the worst order on a side past its soft cap (the CLOB
    /// re-checks the threshold).
    pub fn evict(&self, args: ClobEvictWorstArgsV0) -> Result<ClobRemovedOrderV0> {
        self.invoke(&CLOB_EVICT_WORST_V0_DISCRIMINATOR, &args, "evict")
    }

    /// The book's default activation delay, read straight off its header —
    /// what a placement's `activation_slot` becomes when the caller doesn't
    /// choose a delay. Callers mirror the CLOB's `slot + delay` to maintain
    /// the crank wake hints, and compare a requested delay against it to
    /// decide whether the fast-activation attestation is required.
    pub fn default_activation_delay_slots(&self) -> Result<u32> {
        read_clob_u32(
            &self.market.try_borrow_data()?,
            CLOB_DEFAULT_ACTIVATION_DELAY_OFFSET,
        )
        .ok_or_else(|| {
            msg!("clob market account is too short to hold its header");
            ErrorCode::DefaultError.into()
        })
    }

    /// One CPI: `discriminator ++ borsh(args)` to the book as its
    /// `place_authority`, then decode the response the CLOB left as return
    /// data. `what` only names the call in error messages.
    fn invoke<A: AnchorSerialize, R: AnchorDeserialize>(
        &self,
        discriminator: &[u8; 8],
        args: &A,
        what: &str,
    ) -> Result<R> {
        let mut data = discriminator.to_vec();
        args.serialize(&mut data).map_err(|_| {
            msg!("failed to serialize clob {} args", what);
            ErrorCode::DefaultError
        })?;
        invoke_signed(
            &Instruction {
                program_id: self.program.key(),
                accounts: vec![
                    AccountMeta::new(self.market.key(), false),
                    AccountMeta::new_readonly(self.quoter_signer.key(), true),
                ],
                data,
            },
            &[
                self.market.clone(),
                self.quoter_signer.clone(),
                self.program.clone(),
            ],
            &[&get_quoter_signer_seeds(&self.quoter_signer_nonce)],
        )?;

        // Return data is last-writer-wins within the transaction, so require
        // the writer to be the book's own program: otherwise a program the
        // CLOB CPI'd into could dictate the response velocity settles on.
        let (writer, response) = get_return_data().ok_or_else(|| -> Error {
            msg!("clob {} returned no response", what);
            ErrorCode::DefaultError.into()
        })?;
        validate!(
            writer == self.program.key(),
            ErrorCode::DefaultError,
            "clob {} return data written by {} instead of the book's program",
            what,
            writer
        )?;
        R::deserialize(&mut response.as_slice()).map_err(|_| {
            msg!("clob {} returned an undecodable response", what);
            ErrorCode::DefaultError.into()
        })
    }
}

// --- CLOB account byte layout ---
//
// The crank resolvers (and the executor's expiry-hint repair) read the CLOB
// market account's bytes directly: there is no CLOB instruction that answers
// "which order is the tail" or "which order is expired", and the removal ixs
// take the answer as a hint. These offsets mirror `ClobHeaderV0`/`OrderNodeV0`
// in `anchor-v2/programs/clob/src/state.rs` — that program lives in a separate
// workspace (anchor v2), so the layout can't be imported and is pinned here
// instead; the litesvm crank tests exercise these reads against the real .so,
// so a layout drift fails them. Every value is an *account-data* offset
// (anchor's 8-byte discriminator included).

/// `ClobHeaderV0.best_bid` / `.best_ask` — the side heads. The cross
/// condition's change-watch covers both u32s in one 8-byte window: a
/// crossing order is by definition better than the opposite side's best, so
/// it always lands as a new best and moves one of these.
pub const CLOB_BEST_BID_OFFSET: usize = 112;
pub const CLOB_BEST_ASK_OFFSET: usize = 116;
/// `ClobHeaderV0.worst_bid` / `.worst_ask` — the side tails, what
/// `evict_worst_v0` removes.
pub const CLOB_WORST_BID_OFFSET: usize = 120;
pub const CLOB_WORST_ASK_OFFSET: usize = 124;
/// `ClobHeaderV0.bid_count` — first of the two adjacent u32 counts the evict
/// condition's change-watch covers (`bid_count` then `ask_count`).
pub const CLOB_BID_COUNT_OFFSET: usize = 136;
pub const CLOB_ASK_COUNT_OFFSET: usize = 140;
/// `ClobHeaderV0.default_activation_delay_slots` — what a placement's
/// `activation_slot` becomes when the caller doesn't choose a delay;
/// velocity mirrors the CLOB's `slot + delay` computation to maintain the
/// activation wake hint.
pub const CLOB_DEFAULT_ACTIVATION_DELAY_OFFSET: usize = 144;
/// `ClobHeaderV0.evict_threshold_per_side` — the soft cap.
pub const CLOB_EVICT_THRESHOLD_OFFSET: usize = 156;
/// `ClobHeaderV0.market_index`.
pub const CLOB_MARKET_INDEX_OFFSET: usize = 160;
/// Start of the `OrderNodeV0` arena: `[disc][header][len: u32]` padded to the
/// node's 8-byte alignment. The CLOB const-asserts its header at 8480, and
/// this must move with it — reading the arena at a stale offset silently
/// misparses every node, which reads as an empty or nonsense book rather
/// than as an error.
pub const CLOB_ORDERS_OFFSET: usize = 8496;
/// `size_of::<OrderNodeV0>()`.
pub const CLOB_NODE_LEN: usize = 96;
/// The CLOB's list terminator.
pub const CLOB_NIL: u32 = u32::MAX;
/// `OrderBitFlag::Open` — set on a live order, clear on a free node.
pub const CLOB_ORDER_BIT_FLAG_OPEN: u8 = 1;

/// The slice of an `OrderNodeV0` the cranks care about, copied out of the
/// account bytes.
#[derive(Clone, Copy, Debug)]
pub struct ClobNodeView {
    pub authority: Pubkey,
    pub price: u64,
    pub base_asset_amount: u64,
    /// First slot the order may match.
    pub activation_slot: u64,
    pub max_ts: i64,
    pub order_id: u64,
    /// Next node away from the best of book ([`CLOB_NIL`] at the tail).
    pub next: u32,
    pub sub_account_id: u16,
    pub is_open: bool,
}

impl ClobNodeView {
    /// Live and matchable right now: open, activated, not expired.
    pub fn is_matchable(&self, slot: u64, now: i64) -> bool {
        self.is_open && self.activation_slot <= slot && !(self.max_ts != 0 && self.max_ts < now)
    }

    pub fn user_ref(&self) -> ClobUserRefV0 {
        ClobUserRefV0 {
            authority: self.authority,
            sub_account_id: self.sub_account_id,
        }
    }
}

/// Read a u32 header field at an account-data offset.
pub fn read_clob_u32(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// Read a u16 header field at an account-data offset. `market_index` and the
/// per-market tuning fields are u16 and packed adjacently, so reading one as
/// a u32 silently picks up the next.
pub fn read_clob_u16(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        data.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

/// Node-arena capacity implied by the account's length.
pub fn clob_node_capacity(data_len: usize) -> usize {
    data_len.saturating_sub(CLOB_ORDERS_OFFSET) / CLOB_NODE_LEN
}

/// Read node `index` out of a CLOB market account's data. `None` past the
/// arena.
pub fn read_clob_node(data: &[u8], index: u32) -> Option<ClobNodeView> {
    let start = CLOB_ORDERS_OFFSET + (index as usize).checked_mul(CLOB_NODE_LEN)?;
    let node = data.get(start..start + CLOB_NODE_LEN)?;
    Some(ClobNodeView {
        authority: Pubkey::new_from_array(node[..32].try_into().ok()?),
        price: u64::from_le_bytes(node[32..40].try_into().ok()?),
        base_asset_amount: u64::from_le_bytes(node[40..48].try_into().ok()?),
        activation_slot: u64::from_le_bytes(node[48..56].try_into().ok()?),
        max_ts: i64::from_le_bytes(node[56..64].try_into().ok()?),
        order_id: u64::from_le_bytes(node[64..72].try_into().ok()?),
        next: u32::from_le_bytes(node[84..88].try_into().ok()?),
        sub_account_id: u16::from_le_bytes(node[90..92].try_into().ok()?),
        is_open: node[88] & CLOB_ORDER_BIT_FLAG_OPEN != 0,
    })
}

/// One pass over the arena for both wake hints: the minimum expiry over
/// live orders (`i64::MAX` when none expires) and the minimum *future*
/// activation slot (`u64::MAX` when nothing is pending) — what a landing
/// crank repairs the expire and cross-activation conditions to.
pub fn clob_hint_scan(data: &[u8], current_slot: u64) -> (i64, u64) {
    (0..clob_node_capacity(data.len()) as u32)
        .filter_map(|i| read_clob_node(data, i))
        .filter(|node| node.is_open)
        .fold((i64::MAX, u64::MAX), |(min_ts, min_slot), node| {
            (
                if node.max_ts != 0 {
                    min_ts.min(node.max_ts)
                } else {
                    min_ts
                },
                if node.activation_slot > current_slot {
                    min_slot.min(node.activation_slot)
                } else {
                    min_slot
                },
            )
        })
}

/// The first live order expired at `now`, with its node index — the expire
/// resolver's work discovery.
pub fn clob_find_expired(data: &[u8], now: i64) -> Option<(u32, ClobNodeView)> {
    (0..clob_node_capacity(data.len()) as u32).find_map(|i| {
        read_clob_node(data, i)
            .filter(|node| node.is_open && node.max_ts != 0 && node.max_ts <= now)
            .map(|node| (i, node))
    })
}

/// One resting order a CLOB execute could sweep: whose it is, and the price
/// it rests at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClobRestingOrderV0 {
    pub user: ClobUserRefV0,
    pub price: u64,
    pub base_asset_amount: u64,
}

/// The best-first run of orders on `side` that a taker of `size` would sweep,
/// read straight off the book's bytes.
///
/// This is velocity's own answer to "whose liquidity is on this book, at what
/// price" — the thing a CLOB entry's response must stay inside, and the only
/// way to get it that doesn't take the quoter's word for it. It must be read
/// *before* execute runs: execute removes the nodes it fills.
///
/// The skip rules mirror the ones the CLOB applies inside its own sweep
/// (`is_matchable` there): unactivated, expired, unsettleable and self-trade
/// orders are passed over rather than counted, because execute passes over
/// them too and keeps going — counting them would end this walk early and
/// leave a maker execute really does fill outside the permitted set. The walk
/// is bounded by the arena's capacity, so a corrupted link list terminates.
pub fn clob_resting_prefix(
    data: &[u8],
    side: ClobSide,
    size: u64,
    // The set forwarded to the quoter; empty = unrestricted.
    users: &[ClobUserRefV0],
    taker: &ClobUserRefV0,
    slot: u64,
    now: i64,
) -> Vec<ClobRestingOrderV0> {
    let head_offset = match side {
        ClobSide::Bid => CLOB_BEST_BID_OFFSET,
        ClobSide::Ask => CLOB_BEST_ASK_OFFSET,
    };
    let mut prefix = Vec::new();
    let mut index = match read_clob_u32(data, head_offset) {
        Some(index) => index,
        None => return prefix,
    };
    let mut swept = 0u64;
    for _ in 0..clob_node_capacity(data.len()) {
        if index == CLOB_NIL || swept >= size {
            break;
        }
        let Some(node) = read_clob_node(data, index) else {
            break;
        };
        let user = node.user_ref();
        let settleable = users.is_empty() || users.contains(&user);
        if node.is_matchable(slot, now) && settleable && user != *taker {
            prefix.push(ClobRestingOrderV0 {
                user,
                price: node.price,
                base_asset_amount: node.base_asset_amount,
            });
            swept = swept.saturating_add(node.base_asset_amount);
        }
        index = node.next;
    }
    prefix
}

/// Who a quoter's `execute_v0` response is allowed to move balances for.
/// Every registry type answers this from its own state, never from the
/// response: a quoter that could name any loaded user could mint a position
/// onto another quoter's maker, or onto the taker, at a price of its choosing.
pub enum QuoterSubjects {
    /// A Custom entry fills against exactly one margin account — the entry's
    /// `user`, whose authority created the entry, so registration is that
    /// user's consent. Nothing else it names is settleable.
    Account(Pubkey),
    /// A CLOB entry's makers are velocity users resting on *its* book, so its
    /// permitted set isn't declarable on the entry; it is read off the book
    /// (best-first, before execute consumes it), which also yields the prices
    /// those orders rest at.
    Book(Vec<ClobRestingOrderV0>),
}

impl QuoterSubjects {
    /// Whether this quoter may move `user`'s balances. `key` is `user`
    /// resolved against the loaded set, which is how the Custom case compares
    /// against the entry's `user` field.
    ///
    /// The `taker` is never a subject, whatever the entry type: a quoter that
    /// could name the taker would net the taker's position against itself at a
    /// price of its choosing. The wire tells every quoter to skip the taker;
    /// this is the rule rather than the request, and it lives here so no
    /// caller can apply the type check without it.
    pub fn permits(&self, user: &ClobUserRefV0, key: &Pubkey, taker: &ClobUserRefV0) -> bool {
        if user == taker {
            return false;
        }
        match self {
            QuoterSubjects::Account(quoted) => quoted == key,
            QuoterSubjects::Book(resting) => resting.iter().any(|order| &order.user == user),
        }
    }

    /// The resting run as price levels, for callers that have no quote to bind
    /// against and must take the book itself as the quote (the cross-match
    /// crank, whose account list carries only the execute leg). `None` for
    /// entry types whose liquidity velocity cannot read.
    pub fn as_levels(&self) -> Option<Vec<PriceLevel>> {
        match self {
            QuoterSubjects::Account(_) => None,
            QuoterSubjects::Book(resting) => Some(
                resting
                    .iter()
                    .map(|order| PriceLevel {
                        price: order.price,
                        size: order.base_asset_amount,
                    })
                    .collect(),
            ),
        }
    }
}

/// Execute leg for external CPI quoters, threaded into the router pass by the
/// fill entrypoint — the controller works over account maps and can't CPI
/// itself, so the entrypoint (which holds the `AccountInfo`s) supplies this.
/// `index` addresses the same book order the caller quoted into
/// `RouterFillInputs::books`.
pub trait ExternalQuoterExecutor {
    /// Registry type of quoter `index` — decides whether its fills carry
    /// velocity-side resting-order aggregates to unwind (CLOB orders are
    /// margin-reserved at placement; Custom PropAMM depth is not).
    fn quoter_type(&self, index: usize) -> QuoterType;

    /// The `User` quoter `index` quotes for (`QuoterV0::user`) — the margin
    /// account the pre-execute clamp sizes Custom books against.
    fn quoter_user(&self, index: usize) -> Pubkey;

    /// The users quoter `index` may return balance changes for on a fill of
    /// `direction`/`size`. Must be called before [`Self::execute`]: for a
    /// book-backed quoter the answer lives in state execute is about to
    /// consume.
    fn subjects(
        &self,
        index: usize,
        direction: Direction,
        size: u64,
    ) -> crate::error::VelocityResult<QuoterSubjects>;

    /// CPI `execute_v0` on quoter `index` with the routed allocation. The
    /// response is untrusted: the router pass validates overfill, the
    /// executed price against the quoted prefix, and that every balance
    /// change lands on a permitted subject before settling anything.
    fn execute(
        &mut self,
        index: usize,
        direction: Direction,
        size: u64,
    ) -> crate::error::VelocityResult<ExecuteResponseV0>;
}

/// Executor for a router fill carrying no external quoter accounts: quoting
/// produced no external books, so any external allocation is unreachable —
/// executing one is an error, not a silent skip.
pub struct NoExternalQuoters;

impl ExternalQuoterExecutor for NoExternalQuoters {
    fn quoter_type(&self, _index: usize) -> QuoterType {
        QuoterType::Custom
    }

    fn quoter_user(&self, _index: usize) -> Pubkey {
        Pubkey::default()
    }

    fn subjects(
        &self,
        _index: usize,
        _direction: Direction,
        _size: u64,
    ) -> crate::error::VelocityResult<QuoterSubjects> {
        Ok(QuoterSubjects::Book(vec![]))
    }

    fn execute(
        &mut self,
        _index: usize,
        _direction: Direction,
        _size: u64,
    ) -> crate::error::VelocityResult<ExecuteResponseV0> {
        msg!("router fill has no external quoter accounts to execute against");
        Err(ErrorCode::DefaultError)
    }
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct UserBalanceChange {
    /// The user the change applies to, in derivable form; velocity resolves
    /// it against its loaded users.
    pub user: ClobUserRefV0,
    /// Will be subtracted if direction was long (taker is taking base from
    /// this user). Will be added if direction was short (taker is adding base
    /// to this user).
    pub base_size: u64,
    /// Will be added if direction was long (taker is paying quote to this
    /// user). Will be subtracted if direction was short (taker is taking
    /// quote from this user).
    pub quote_size: u64,
    /// Resting orders of this user the fill fully consumed (and the quoter
    /// removed), by id. Velocity decrements the user's open-order count by
    /// the length and releases any placed trigger slot shadowing one of
    /// these ids; sub-min culls ride the separate `cancelled` vec because
    /// their remainders also need unwinding.
    pub completed_order_ids: Vec<u64>,
}

impl QuoterV0 {
    /// This entry really is the CLOB serving `market_index`, and `book` is one
    /// of the accounts the admin vetted onto it — so a caller can't point an
    /// otherwise-valid entry at an arbitrary account it happens to own.
    ///
    /// Deliberately says nothing about `is_active`/`is_approved`: those mean
    /// "may take new flow", and the removal paths must keep working on a
    /// killed or de-listed book. Callers that add flow gate on them too.
    pub fn validate_clob_book(&self, market_index: u16, book: &Pubkey) -> Result<()> {
        validate!(
            self.quoter_type == QuoterType::Clob,
            ErrorCode::DefaultError,
            "quoter entry is not a CLOB"
        )?;
        validate!(
            self.market == market_index,
            ErrorCode::DefaultError,
            "quoter entry is for market {}, call is for market {}",
            self.market,
            market_index
        )?;
        let registered = &self.execute_accounts[..self.execute_accounts_count as usize];
        validate!(
            registered.iter().any(|meta| &meta.pubkey == book),
            ErrorCode::DefaultError,
            "clob market is not registered on the quoter entry"
        )?;
        Ok(())
    }

    /// Shared gate on both CPI legs: the entry takes new flow, and it takes
    /// it for the market the caller is filling. An entry is registered per
    /// `(market, program, user)`, and nothing about the CPI itself carries the
    /// market — so without the second check an entry vetted for one perp
    /// market could be quoted into another, settling its balance changes
    /// against positions it was never approved to touch.
    fn gate_for_market(&self, market_index: u16) -> Result<()> {
        validate!(
            self.is_active && self.is_approved,
            ErrorCode::DefaultError,
            "quoter is not active and approved"
        )?;
        validate!(
            self.market == market_index,
            ErrorCode::InvalidQuoterConfig,
            "quoter entry is for market {}, call is for market {}",
            self.market,
            market_index
        )?;
        Ok(())
    }

    /// CPI `quote_v0` on the quoter program and return its price levels,
    /// checked against the level contract before any of it is routed.
    ///
    /// `account_map` is the caller's remaining-accounts index (pubkey →
    /// AccountInfo); every registered quote account must be present or the
    /// call errors — silently dropping one would misalign the CPI account
    /// list against the quoter's expectations.
    pub fn quote<'info>(
        &self,
        market_index: u16,
        args: QuoteArgsV0<'_>,
        quoter_signer: &Pubkey,
        quoter_signer_nonce: u8,
        account_map: &BTreeMap<Pubkey, AccountInfo<'info>>,
    ) -> Result<Vec<PriceLevel>> {
        self.gate_for_market(market_index)?;
        let response: QuoteResponseV0 = self.invoke_quoter(
            &self.quote_v0_discriminator,
            &self.quote_accounts,
            self.quote_accounts_count,
            &args,
            quoter_signer,
            quoter_signer_nonce,
            account_map,
        )?;
        crate::math::router::validate_quoted_levels(args.direction, &response.levels)?;
        Ok(response.levels)
    }

    /// CPI `execute_v0` on the quoter program: commit a fill and return the
    /// balance changes velocity must apply. Callers are responsible for
    /// validating the returned changes against the quoted levels (and margin)
    /// before applying them — the quoter is untrusted.
    pub fn execute<'info>(
        &self,
        market_index: u16,
        args: ExecuteArgsV0<'_>,
        quoter_signer: &Pubkey,
        quoter_signer_nonce: u8,
        account_map: &BTreeMap<Pubkey, AccountInfo<'info>>,
    ) -> Result<ExecuteResponseV0> {
        self.gate_for_market(market_index)?;
        self.invoke_quoter(
            &self.execute_v0_discriminator,
            &self.execute_accounts,
            self.execute_accounts_count,
            &args,
            quoter_signer,
            quoter_signer_nonce,
            account_map,
        )
    }

    /// Shared CPI leg: forward the registered accounts, send
    /// `discriminator ++ borsh(args)`, and decode the borsh response from the
    /// quoter's response account at the pointer returned via return data.
    fn invoke_quoter<'info, A: AnchorSerialize, R: AnchorDeserialize>(
        &self,
        discriminator: &[u8; 8],
        registered: &[AmmAccountMeta],
        count: u8,
        args: &A,
        quoter_signer: &Pubkey,
        quoter_signer_nonce: u8,
        account_map: &BTreeMap<Pubkey, AccountInfo<'info>>,
    ) -> Result<R> {
        validate!(
            (count as usize) <= registered.len(),
            ErrorCode::DefaultError,
            "prop amm accounts_count {} exceeds capacity {}",
            count,
            registered.len()
        )?;
        let account_metas = quoter_account_metas(&registered[..count as usize], quoter_signer);

        let mut account_infos = Vec::with_capacity(account_metas.len() + 1);
        for meta in &account_metas {
            let info = account_map.get(&meta.pubkey).ok_or_else(|| {
                msg!("prop amm account {} missing from account map", meta.pubkey);
                ErrorCode::DefaultError
            })?;
            account_infos.push(info.clone());
        }
        // CPI needs the callee program's account info too.
        let program_info = account_map.get(&self.program_id).ok_or_else(|| {
            msg!("quoter program account missing from account map");
            ErrorCode::DefaultError
        })?;
        account_infos.push(program_info.clone());

        // Sized exactly, once. The fixed-width user set makes these args
        // ~1.7 KB, and a `Vec` that grows into that by doubling leaks every
        // intermediate buffer: velocity's bump allocator never reclaims, and
        // one fill CPIs every registered quoter twice. Growing rather than
        // reserving exhausted the 32 KB heap outright.
        let mut data = Vec::with_capacity(QUOTER_CPI_DATA_CAPACITY);
        data.extend_from_slice(discriminator);
        args.serialize(&mut data).map_err(|_| {
            msg!("prop amm failed to serialize cpi args");
            ErrorCode::DefaultError
        })?;

        invoke_signed(
            &Instruction {
                program_id: self.program_id,
                accounts: account_metas,
                data,
            },
            &account_infos,
            &[&get_quoter_signer_seeds(&quoter_signer_nonce)],
        )?;

        // The payload lives in the quoter's response account; return data
        // carries only a pointer into it, so responses aren't bound by the
        // 1024-byte return-data cap. Return data is last-writer-wins within
        // the transaction; requiring the writer to be `program_id` guards
        // against reading a pointer set by a program the quoter CPI'd into.
        let (writer, pointer_data) = get_return_data().ok_or_else(|| {
            msg!("prop amm quoter set no return data");
            ErrorCode::DefaultError
        })?;
        validate!(
            writer == self.program_id,
            ErrorCode::DefaultError,
            "prop amm return data written by {} instead of quoter program",
            writer
        )?;
        let pointer =
            ResponsePointerV0::deserialize(&mut pointer_data.as_slice()).map_err(|_| {
                msg!("prop amm quoter returned undecodable response pointer");
                ErrorCode::DefaultError
            })?;

        let response_info = account_map.get(&self.response_account).ok_or_else(|| {
            msg!("prop amm response account missing from account map");
            ErrorCode::DefaultError
        })?;
        // Only the quoter program can have written an account it owns.
        validate!(
            *response_info.owner == self.program_id,
            ErrorCode::DefaultError,
            "prop amm response account not owned by quoter program"
        )?;
        let data = response_info
            .try_borrow_data()
            .map_err(|_| ErrorCode::DefaultError)?;
        let start = pointer.offset as usize;
        let end = start
            .checked_add(pointer.len as usize)
            .ok_or(ErrorCode::DefaultError)?;
        validate!(
            end <= data.len(),
            ErrorCode::DefaultError,
            "prop amm response pointer out of bounds"
        )?;

        // `deserialize` (not `try_from_slice`) so trailing bytes are
        // tolerated — lets a quoter append response fields without breaking
        // older velocity builds.
        R::deserialize(&mut &data[start..end]).map_err(|_| {
            msg!("prop amm quoter returned undecodable response");
            ErrorCode::DefaultError.into()
        })
    }
}
