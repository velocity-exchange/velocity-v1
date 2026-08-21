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
    std::convert::TryInto,
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
    /// The optional third leg: `quote_l3_v0`, which reports the resting
    /// orders behind a ladder and who each belongs to. Zero means the quoter
    /// does not implement it, and a reader attributes the whole ladder to
    /// [`Self::user`] — which is right for every quoter that fills from one
    /// account. A book is the exception, and this is how it says so.
    pub quote_l3_v0_discriminator: [u8; 8],
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
    /// Room for the next field, so adding one does not move the account's
    /// size or its alignment invariant.
    pub padding: [u8; 8],
}

// Zero-copy layout invariant (see docs/alignment-and-native-offsets.md):
// no u128 fields, size (incl. 8-byte discriminator) ≡ 8 (mod 16).
const_assert_eq!(std::mem::size_of::<QuoterV0>(), 2768);
const_assert_eq!((QuoterV0::SIZE - 8) % 16, 0);

impl Size for QuoterV0 {
    const SIZE: usize = 2776;
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

/// The request half of the quoter wire, declared in `quoter-spec` alongside
/// the responses: velocity writes these bytes and a quoter reads them, so a
/// second declaration here would be a second thing to keep in step. The
/// aliases keep velocity's names.
pub use quoter_spec::{DirectionV0 as Direction, SideV0 as ClobSide};

/// What velocity reads into the wire's direction and side beyond their shape.
/// An inherent impl is not available on a foreign type, and a trait keeps
/// every call site reading as it did.
pub trait WireDirectionExt {
    fn to_position_direction(self) -> crate::controller::position::PositionDirection;
}

impl WireDirectionExt for Direction {
    fn to_position_direction(self) -> crate::controller::position::PositionDirection {
        match self {
            Direction::Long => crate::controller::position::PositionDirection::Long,
            Direction::Short => crate::controller::position::PositionDirection::Short,
        }
    }
}

impl WireDirectionExt for ClobSide {
    /// The maker position direction a resting order on this side represents.
    fn to_position_direction(self) -> crate::controller::position::PositionDirection {
        match self {
            ClobSide::Bid => crate::controller::position::PositionDirection::Long,
            ClobSide::Ask => crate::controller::position::PositionDirection::Short,
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
///
/// Declared by `quoter-spec`, which every program on this wire compiles
/// against; `Pubkey` and the v2 programs' `Address` are the same type, so the
/// declaration needs no per-program restatement. The alias keeps velocity's
/// name for it.
pub type ClobUserRefV0 = quoter_spec::UserRefV0;

/// Borsh width of a [`ClobUserRefV0`].
pub const CLOB_USER_REF_BYTES: usize = quoter_spec::UserRefV0::SIZE;
const_assert_eq!(std::mem::size_of::<ClobUserRefV0>(), CLOB_USER_REF_BYTES);

/// The loaded-user set on the quoter wire: a length prefix and that many
/// entries.
///
/// An empty set means unrestricted, and is only used by callers that settle
/// nothing (the quote view, cross discovery). A quoter reading a non-empty
/// set must skip liquidity whose owner is absent from it: velocity cannot
/// settle a balance change for a `User` it did not load, so such a fill is
/// refused wholesale.
///
/// The request half of the quoter wire is declared in `quoter-spec`, the same
/// place the responses are: velocity writes these bytes and a quoter reads
/// them, so a second declaration here would be a second thing to keep in
/// step. The aliases keep velocity's names for them.
pub use quoter_spec::{
    user_set_bytes as quoter_user_set_bytes, UserCapV0 as QuoterUserCapV0,
    UserCapsV0 as QuoterUserCapsV0, USER_CAPS_BYTES as QUOTER_USER_CAPS_BYTES,
    USER_CAPS_CAPACITY as MAX_CONSTRAINED_WIRE_USERS, USER_EXCLUSION_BITMAP_BYTES,
    USER_SET_CAPACITY as MAX_QUOTER_WIRE_USERS, USER_SET_MAX_BYTES as QUOTER_USER_SET_MAX_BYTES,
};

/// Upper bound on a CLOB CPI's instruction data: discriminator plus the widest
/// args on that interface, which is `place` — a side, two `u64`s, an optional
/// delay, a timestamp, a user ref and the taker-origin flag.
///
/// Reserved in one shot for the same reason [`quoter_cpi_data_len`] is: a
/// `Vec` that starts at the discriminator and doubles into place leaks every
/// intermediate buffer, and velocity's bump allocator never reclaims. This path
/// runs on every order placed on the book and every removal crank, so it is the
/// busier of the two.
pub const CLOB_CPI_DATA_CAPACITY: usize = 8 + 1 + 8 + 8 + 5 + 8 + CLOB_USER_REF_BYTES + 1;

/// Bytes a quote or execute CPI's instruction data takes: discriminator,
/// direction, size, the user set, the caps, the reference price, and the
/// taker behind its option tag.
///
/// A size rather than a constant, because the user set is length-prefixed.
/// The caller reserves exactly this much in one shot. Every field the args
/// serializer writes has to be counted here: one byte short and the `Vec`
/// doubles, which on this heap means the fill runs out of memory rather than
/// slowing down.
pub const fn quoter_cpi_data_len(users: usize, taker: bool) -> usize {
    8 + 1
        + 8
        + quoter_user_set_bytes(users)
        + QUOTER_USER_CAPS_BYTES
        + 8
        + 1
        + if taker { CLOB_USER_REF_BYTES } else { 0 }
}

/// Widest one quote or execute CPI can be: a full user set and a taker.
pub const QUOTER_CPI_DATA_MAX: usize = quoter_cpi_data_len(MAX_QUOTER_WIRE_USERS, true);

/// The loaded-user set as velocity holds it: a heap slice, capped at the
/// wire's capacity, written to the wire by [`QuoterUserSetRef`].
///
/// Velocity never holds the set by value. A full one is 1,633 bytes, and one
/// of those lands in a 4 KB SBF frame: the fill and cross-match entrypoints
/// overflow it, the linker says so at build time ("overflows the maximum
/// allowed frame space"), and at runtime it is an access violation several
/// frames deep.
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

/// The quote response, read in place for the same reason
/// [`ExecuteResponseV0`] is.
pub use quoter_spec::QuoteResponseV0;
/// Write `quote_l3_v0` args in the framing the wire declares, for an
/// off-chain caller that asks a book directly rather than through the router.
pub fn write_l3_args(dst: &mut Vec<u8>, args: &L3ArgsV0) -> crate::error::VelocityResult<()> {
    quoter_spec::write_args(dst, args).map_err(|_| {
        msg!("could not serialize l3 args");
        ErrorCode::DefaultError
    })?;
    Ok(())
}

/// The request half of the quoter wire, declared by `quoter-spec` — the same
/// crate that declares the responses, so the bytes velocity writes and the
/// bytes a quoter reads come from one declaration.
///
/// Both carry the user set as a borrowed slice. A full set is 1,633 bytes and
/// an SBF stack frame is 4 KB, so owning one put a copy in this frame per
/// quoter and another inside the CPI leg, which overflowed the fill path at
/// runtime while every host-side test passed. The quoter reads it in place
/// too, straight out of its instruction data.
pub use quoter_spec::{
    ExecuteArgsV0, L3ArgsV0, L3ResponseV0, L3RowV0, QuoteArgsV0, L3_ROW_FLAG_TAKER_ORIGIN,
};

/// Declared by `quoter-spec`; the alias keeps velocity's name for it.
pub type PriceLevel = quoter_spec::PriceLevelV0;

/// What one quoter answered: the ladder it stands behind, and the depth it
/// says it holds at a better price but cannot reach in this transaction.
pub struct QuotedLadderV0 {
    pub levels: Vec<PriceLevel>,
    /// `price == 0` when the quoter reached everything it was asked for.
    pub withheld: PriceLevel,
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
pub use quoter_spec::CancelledRemainderV0;
pub use quoter_spec::CompletedOrderV0;
/// The execute response, read in place out of the quoter's account.
///
/// Borrows rather than owns: velocity's heap is 32 KB and never reclaims, and
/// one fill CPIs every registered quoter, so copying the records out would
/// spend heap per quoter per fill that nothing gives back. The caller holds
/// the account guard (see [`ResponseLocationV0`]) for as long as it reads.
pub use quoter_spec::ExecuteResponseV0;
/// Returned via return data by `quote_v0`/`execute_v0`: where in the quoter's
/// `response_account` the borsh response was written. Declared by
/// `quoter-spec`, which owns every shape on this wire.
pub use quoter_spec::ResponsePointerV0;

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

/// Which sides a `cancel_all_v0` withdraws, on the CLOB wire. Declared by
/// `quoter-spec`; the alias keeps velocity's name for it.
pub use quoter_spec::CancelSidesV0 as ClobCancelSides;

/// What the wire's named sides mean to velocity: the maker positions they
/// unwind.
pub trait ClobCancelSidesExt {
    /// The maker position directions the named sides represent — a resting bid
    /// is a long, a resting ask a short. What the aggregate unwind iterates.
    fn directions(self) -> &'static [crate::controller::position::PositionDirection];
    fn includes(self, direction: crate::controller::position::PositionDirection) -> bool;
}

impl ClobCancelSidesExt for ClobCancelSides {
    fn directions(self) -> &'static [crate::controller::position::PositionDirection] {
        use crate::controller::position::PositionDirection;
        match self {
            ClobCancelSides::Bids => &[PositionDirection::Long],
            ClobCancelSides::Asks => &[PositionDirection::Short],
            ClobCancelSides::Both => &[PositionDirection::Long, PositionDirection::Short],
        }
    }

    fn includes(self, direction: crate::controller::position::PositionDirection) -> bool {
        use crate::controller::position::PositionDirection;
        matches!(
            (self, direction),
            (ClobCancelSides::Both, _)
                | (ClobCancelSides::Bids, PositionDirection::Long)
                | (ClobCancelSides::Asks, PositionDirection::Short)
        )
    }
}

/// `cancel_all_v0` args on the CLOB wire.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct ClobCancelAllArgsV0 {
    /// Whose orders to withdraw (verified against each node on the CLOB side).
    pub user: ClobUserRefV0,
    pub sides: ClobCancelSides,
}

/// What the CLOB's `cancel_all_v0` withdrew: per-side totals rather than a list
/// of removals, which is exactly the shape the open-order aggregates consume —
/// one `decrease_open_bids_and_asks` per side and one count, however many
/// orders the sweep took.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct ClobCancelAllOutcomeV0 {
    pub user: ClobUserRefV0,
    pub bid_base_asset_amount: u64,
    pub ask_base_asset_amount: u64,
    pub bid_orders: u32,
    pub ask_orders: u32,
    /// Whether the CLOB finished the requested sides rather than stopping at
    /// its per-call cap. False means this user still has resting orders and the
    /// caller should repeat the instruction.
    pub exhaustive: bool,
}

impl ClobCancelAllOutcomeV0 {
    pub fn orders(&self) -> u32 {
        self.bid_orders.saturating_add(self.ask_orders)
    }

    /// Base amount withdrawn on the side a maker position of `direction` rests
    /// on — a bid is a long, an ask a short.
    pub fn base_for(&self, direction: crate::controller::position::PositionDirection) -> u64 {
        match direction {
            crate::controller::position::PositionDirection::Long => self.bid_base_asset_amount,
            crate::controller::position::PositionDirection::Short => self.ask_base_asset_amount,
        }
    }

    /// Orders withdrawn on that same side.
    pub fn orders_for(&self, direction: crate::controller::position::PositionDirection) -> u32 {
        match direction {
            crate::controller::position::PositionDirection::Long => self.bid_orders,
            crate::controller::position::PositionDirection::Short => self.ask_orders,
        }
    }
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
pub const CLOB_CANCEL_ALL_V0_DISCRIMINATOR: [u8; 8] = [212, 11, 203, 11, 184, 40, 88, 95];
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

    /// Pull every order this user holds on the named sides in one CPI; returns
    /// the per-side totals to unwind their aggregates by. The CLOB caps how many
    /// it takes per call and says so in
    /// [`ClobCancelAllOutcomeV0::exhaustive`] — the totals always describe
    /// exactly what that call removed, so repeating it is safe.
    pub fn cancel_all(&self, args: ClobCancelAllArgsV0) -> Result<ClobCancelAllOutcomeV0> {
        self.invoke(&CLOB_CANCEL_ALL_V0_DISCRIMINATOR, &args, "cancel all")
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

    /// The book's floor on a resting order's size, read off its header bytes.
    /// A remainder below it cannot rest — the book culls one on its own fills
    /// — so a caller re-placing a partially-crossed remainder has to drop it
    /// instead of offering the book a placement it will reject.
    pub fn min_order_size(&self) -> Result<u64> {
        let data = self.market.try_borrow_data()?;
        let bytes = data
            .get(CLOB_MIN_ORDER_SIZE_OFFSET..CLOB_MIN_ORDER_SIZE_OFFSET + 8)
            .ok_or_else(|| -> Error {
                msg!("clob market account is too short to hold its header");
                ErrorCode::DefaultError.into()
            })?;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
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
        let mut data = Vec::with_capacity(CLOB_CPI_DATA_CAPACITY);
        data.extend_from_slice(discriminator);
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
// take the answer as a hint.
//
// The layout is `clob-spec`'s, not this file's. The book asserts its own
// header and node against that crate, so a field that moves there fails to
// compile rather than misparsing here — which would read as an empty or
// nonsense book rather than as an error. The aliases below keep velocity's
// names for what the crate declares. Every offset is an *account-data* offset
// (anchor's 8-byte discriminator included).
pub use clob_spec::{
    node as read_clob_node, u16_at as read_clob_u16, u32_at as read_clob_u32,
    u64_at as read_clob_u64, ASK_COUNT_OFFSET as CLOB_ASK_COUNT_OFFSET,
    BEST_ASK_OFFSET as CLOB_BEST_ASK_OFFSET, BEST_BID_OFFSET as CLOB_BEST_BID_OFFSET,
    BID_COUNT_OFFSET as CLOB_BID_COUNT_OFFSET,
    DEFAULT_ACTIVATION_DELAY_OFFSET as CLOB_DEFAULT_ACTIVATION_DELAY_OFFSET,
    EVICT_THRESHOLD_OFFSET as CLOB_EVICT_THRESHOLD_OFFSET,
    MARKET_INDEX_OFFSET as CLOB_MARKET_INDEX_OFFSET,
    MIN_ORDER_SIZE_OFFSET as CLOB_MIN_ORDER_SIZE_OFFSET, NIL as CLOB_NIL,
    NODE_BYTES as CLOB_NODE_LEN, ORDERS_OFFSET as CLOB_ORDERS_OFFSET,
    WORST_ASK_OFFSET as CLOB_WORST_ASK_OFFSET, WORST_BID_OFFSET as CLOB_WORST_BID_OFFSET,
};
/// One arena slot, as the book declares it. The cranks read whole nodes now
/// rather than a hand-decoded subset of one.
pub use clob_spec::{OrderBitFlag as ClobOrderBitFlag, OrderNodeV0 as ClobNodeView};

/// `OrderBitFlag::Open` — set on a live order, clear on a free node.
pub const CLOB_ORDER_BIT_FLAG_OPEN: u8 = ClobOrderBitFlag::Open as u8;
/// `OrderBitFlag::TakerOrigin` — the order is a migrated taker remainder, so
/// in a cross it is the aggressor and the match settles at the counterparty's
/// price. Velocity sets it at migration and reads it back here to *find* a
/// cross to resolve; the authoritative report is `ClobRemovedOrderV0`, which
/// is what the resolution checks before settling.
pub const CLOB_ORDER_BIT_FLAG_TAKER_ORIGIN: u8 = ClobOrderBitFlag::TakerOrigin as u8;

/// Node-arena capacity implied by the account's length.
pub fn clob_node_capacity(data_len: usize) -> usize {
    clob_spec::capacity(data_len)
}

/// One pass over the arena for both wake hints: the minimum expiry over
/// live orders (`i64::MAX` when none expires) and the minimum *future*
/// activation slot (`u64::MAX` when nothing is pending) — what a landing
/// crank repairs the expire and cross-activation conditions to.
pub fn clob_hint_scan(data: &[u8], current_slot: u64) -> (i64, u64) {
    (0..clob_node_capacity(data.len()) as u32)
        .filter_map(|i| read_clob_node(data, i))
        .filter(|node| node.is_open())
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
            .filter(|node| node.is_open() && node.max_ts != 0 && node.max_ts <= now)
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
    /// A migrated taker remainder, which the CLOB passes over while a
    /// counterparty crosses it. Its base is therefore not depth a caller can
    /// count on execute consuming — see the sweep accounting in
    /// [`clob_resting_prefix`].
    pub is_taker_origin: bool,
}

/// The resting run as price levels, for a caller that has no quote to bind
/// against and must take the book itself as the quote.
///
/// That caller is a cross crank: its account list carries only the execute
/// leg, so there is no `quote_v0` response to hold the fill to, and the run
/// velocity reads off the book stands in as one. It is not a check of the
/// book against velocity — the arena is the book's own state — but of the
/// executed notional against the prices those orders rest at, which is what
/// makes "the counterparty's price" a fact the crank can be held to.
#[allow(clippy::too_many_arguments)]
pub fn clob_resting_levels(
    data: &[u8],
    side: ClobSide,
    size: u64,
    users: &[ClobUserRefV0],
    caps: &QuoterUserCapsV0,
    taker: &ClobUserRefV0,
    slot: u64,
    now: i64,
) -> Vec<PriceLevel> {
    clob_resting_prefix(data, side, size, users, caps, taker, slot, now)
        .into_iter()
        .map(|order| PriceLevel {
            price: order.price,
            size: order.base_asset_amount,
        })
        .collect()
}

/// First-allocation size for a resting-prefix read. Not a cap — a deeper sweep
/// still grows past it.
const CLOB_RESTING_PREFIX_RESERVE: usize = 16;

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
#[allow(clippy::too_many_arguments)]
pub fn clob_resting_prefix(
    data: &[u8],
    side: ClobSide,
    size: u64,
    // The set forwarded to the quoter; empty = unrestricted.
    users: &[ClobUserRefV0],
    // The caps forwarded with it. Only the exclusions are applied here, and
    // that is what makes an exclusion enforced rather than requested: an
    // excluded user is absent from the permitted-subject set, so a book that
    // filled them anyway returns a change for a user velocity refuses. The
    // budgets are advisory and the post-fill checks answer for them.
    caps: &QuoterUserCapsV0,
    taker: &ClobUserRefV0,
    slot: u64,
    now: i64,
) -> Vec<ClobRestingOrderV0> {
    let head_offset = match side {
        ClobSide::Bid => CLOB_BEST_BID_OFFSET,
        ClobSide::Ask => CLOB_BEST_ASK_OFFSET,
    };
    // Reserved for a typical sweep rather than the deepest one. Starting empty
    // means a deep prefix doubles its way up and leaks every intermediate on a
    // heap that never reclaims; reserving the fill ceiling instead would spend
    // ~6 KB of a 32 KB heap per quoter for a case that rarely happens. A
    // handful of orders is the common sweep, and this covers it in one
    // allocation.
    let mut prefix = Vec::with_capacity(CLOB_RESTING_PREFIX_RESERVE);
    // Exclusions only. A partial budget is left to the book: this walk has to
    // stay a *superset* of what execute fills, and spending a budget here
    // could retire a user's room before the book retires it, which would stop
    // the walk short of a maker execute really fills. Ignoring one only makes
    // the walk go deeper.
    let any_excluded = caps.any_excluded();
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
        // An excluded user is passed over here exactly as the book passes it
        // over, so it never reaches the permitted set.
        let excluded = any_excluded
            && users
                .iter()
                .position(|named| *named == user)
                .is_some_and(|index| caps.is_excluded(index));
        if node.is_matchable(slot, now) && settleable && user != *taker && !excluded {
            prefix.push(ClobRestingOrderV0 {
                user,
                price: node.price,
                base_asset_amount: node.base_asset_amount,
                is_taker_origin: node.is_taker_origin(),
            });
            // A taker-origin order's base does not count toward the sweep.
            //
            // The CLOB skips one whose counterparty crosses it — that is the
            // protection that keeps a migrated taker remainder from being
            // taken at its own limit — so an execute sized against this walk
            // reaches *past* it into deeper depth. If its base were counted,
            // the walk would stop at it and the maker actually filled would
            // be absent from the permitted-subject set, and the response
            // would be refused as `QuoterSubjectNotPermitted`.
            //
            // Its user stays in the set, and the walk deliberately does not
            // re-derive whether the CLOB would gate this particular order:
            // not counting the base makes this a superset of what execute can
            // fill under either outcome, which is all the subject check needs,
            // and it keeps the gate's rule in one program instead of two that
            // can drift apart.
            if !node.is_taker_origin() {
                swept = swept.saturating_add(node.base_asset_amount);
            }
        }
        index = node.next;
    }
    prefix
}

/// Unwind one user's reserve for everything a bulk sweep took, and report how
/// many orders that was.
///
/// One `decrease_open_bids_and_asks` per side by the summed base the sweep
/// reported: identical arithmetic to N per-order unwinds — each placement
/// reserved its own amount, so the sum can never exceed what is reserved — at
/// a cost that does not grow with the count. Both directions regardless of
/// which sides were asked for, so the reserve moves by exactly what left the
/// book rather than by what the caller intended to take.
pub fn unwind_swept_orders(
    user: &mut crate::state::user::User,
    book: &[u8],
    market_index: u16,
    sides: ClobCancelSides,
    swept: &ClobCancelAllOutcomeV0,
) -> crate::error::VelocityResult<u32> {
    use crate::controller::position::{
        decrease_open_bids_and_asks, get_position_index, PositionDirection,
    };
    let position_index = get_position_index(&user.perp_positions, market_index)?;
    for direction in [PositionDirection::Long, PositionDirection::Short] {
        decrease_open_bids_and_asks(
            &mut user.perp_positions[position_index],
            &direction,
            swept.base_for(direction),
            true,
        )?;
    }
    let orders = swept.orders();
    user.perp_positions[position_index].open_orders = user.perp_positions[position_index]
        .open_orders
        .saturating_sub(orders.min(u8::MAX as u32) as u8);
    (0..orders).for_each(|_| user.decrement_open_orders(false));
    release_swept_trigger_shadows(user, book, market_index, sides);
    Ok(orders)
}

/// Free the placed-trigger shadows on `sides` whose live orders a bulk sweep
/// took, and report how many were freed.
///
/// Driven off the *book* rather than off a list of removed ids: a shadow is
/// released exactly when the node it points at no longer holds its order,
/// which is true whether the sweep was capped or not, and is strictly more
/// robust than matching ids — a shadow whose order left the book by any route
/// reads as released here. The scan is over `User.orders`, so it is bounded by
/// that array, not by the book.
pub fn release_swept_trigger_shadows(
    user: &mut crate::state::user::User,
    book: &[u8],
    market_index: u16,
    sides: ClobCancelSides,
) -> usize {
    use crate::state::user::{MarketType, OrderStatus};
    let stale: Vec<usize> = user
        .orders
        .iter()
        .enumerate()
        .filter(|(_, order)| {
            order.status == OrderStatus::Open
                && order.is_placed_on_clob()
                && order.market_type == MarketType::Perp
                && order.market_index == market_index
                && sides.includes(order.direction)
        })
        .filter(|(_, order)| {
            let (node_index, clob_order_id) = order.clob_order_ref();
            // No live node with this id: the sweep took it. An unreadable node
            // index counts as gone for the same reason the CLOB treats an
            // out-of-range hint as stale rather than as corruption.
            !read_clob_node(book, node_index)
                .is_some_and(|node| node.is_open() && node.order_id == clob_order_id)
        })
        .map(|(index, _)| index)
        .collect();
    stale
        .iter()
        .for_each(|index| user.orders[*index].status = OrderStatus::Canceled);
    stale.len()
}

/// Who a quoter's `execute_v0` response is allowed to move balances for.
pub enum QuoterSubjects {
    /// A Custom entry fills against exactly one margin account — the entry's
    /// `user`, whose authority created the entry, so registration is that
    /// user's consent. Nothing else it names is settleable, and velocity
    /// knows that from the registry rather than from anything the quoter
    /// says.
    Account(Pubkey),
    /// A book fills against whoever rests on it, and velocity takes its word
    /// for who that is.
    ///
    /// It used to read the book's arena and hold the response to the users
    /// resting there. That check was the program against itself: the arena is
    /// the book program's own state, so a book that wanted to name a stranger
    /// would write the stranger into a node first. It bought no safety, it
    /// cost a walk of the swept prefix on every leg, and it was the last of
    /// velocity's CLOB-specific logic on the fill path.
    ///
    /// What actually bounds a book, then: the admin approves the program
    /// behind a `Clob` entry — and approving a third party's is what would
    /// make this an exposure, exactly as it would for the removal reports
    /// velocity already takes on faith (see [`ClobRemovedOrderV0`]) — the
    /// response may only name users the transaction already carries, every
    /// balance change is held to the quoted prices, and every user it touches
    /// is margin-checked after the fill.
    Book,
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
            QuoterSubjects::Book => true,
        }
    }
}

/// Execute leg for external CPI quoters, threaded into the router pass by the
/// fill entrypoint — the controller works over account maps and can't CPI
/// itself, so the entrypoint (which holds the `AccountInfo`s) supplies this.
/// `index` addresses the same book order the caller quoted into
/// `RouterFillInputs::books`.
/// Find one of the fill's tail accounts by key.
///
/// A scan, not a map: the tail is a couple of dozen accounts and one fill looks
/// up a handful per quoter CPI, so building an index costs more than searching
/// — and a `BTreeMap` costs it in allocations on a 32 KB heap that never
/// reclaims.
pub fn find_account<'a, 'info>(
    accounts: &'a [AccountInfo<'info>],
    key: &Pubkey,
) -> Option<&'a AccountInfo<'info>> {
    accounts.iter().find(|info| info.key == key)
}

/// Where a quoter left its response, handed back so the *caller* creates the
/// account borrow.
///
/// The response is read in place out of the quoter's account, and the borrow
/// guard cannot outlive the function that takes it — so the executor validates
/// the pointer and returns its location, and the fill that consumes the
/// response holds the guard for exactly as long as it reads. Returning the
/// records instead would mean copying them onto a 32 KB heap that never
/// reclaims, once per quoter per fill.
pub struct ResponseLocationV0<'info> {
    pub account: AccountInfo<'info>,
    pub start: usize,
    pub end: usize,
}

impl<'info> ResponseLocationV0<'info> {
    /// Borrow the response account. The guard lives in the caller's scope,
    /// which is what makes the borrowed view below sound.
    pub fn borrow(&self) -> crate::error::VelocityResult<std::cell::Ref<'_, &'_ mut [u8]>> {
        self.account.try_borrow_data().map_err(|_| {
            msg!("prop amm response account is already borrowed");
            ErrorCode::DefaultError
        })
    }

    /// Read the execute response in place out of a guard taken by
    /// [`Self::borrow`].
    pub fn execute_response<'a>(
        &self,
        data: &'a [u8],
    ) -> crate::error::VelocityResult<ExecuteResponseV0<'a>> {
        let bytes = data.get(self.start..self.end).ok_or_else(|| {
            msg!("prop amm response pointer out of bounds");
            ErrorCode::DefaultError
        })?;
        ExecuteResponseV0::parse(bytes).map_err(|_| {
            msg!("prop amm quoter returned an undecodable execute response");
            ErrorCode::DefaultError
        })
    }

    /// Read the quote response in place.
    pub fn quote_response<'a>(
        &self,
        data: &'a [u8],
    ) -> crate::error::VelocityResult<QuoteResponseV0<'a>> {
        let bytes = data.get(self.start..self.end).ok_or_else(|| {
            msg!("prop amm response pointer out of bounds");
            ErrorCode::DefaultError
        })?;
        QuoteResponseV0::parse(bytes).map_err(|_| {
            msg!("prop amm quoter returned an undecodable quote response");
            ErrorCode::DefaultError
        })
    }

    /// The rows behind a ladder, read in place out of a guard taken by
    /// [`Self::borrow`].
    pub fn l3_response<'a>(
        &self,
        data: &'a [u8],
    ) -> crate::error::VelocityResult<L3ResponseV0<'a>> {
        let bytes = data.get(self.start..self.end).ok_or_else(|| {
            msg!("prop amm response pointer out of bounds");
            ErrorCode::DefaultError
        })?;
        L3ResponseV0::parse(bytes).map_err(|_| {
            msg!("prop amm quoter returned an undecodable l3 response");
            ErrorCode::DefaultError
        })
    }

    /// The quote response, checked against the level contract.
    ///
    /// Every reader owes the ladder this check before it routes against it,
    /// so it lives beside the read rather than in each caller.
    pub fn checked_quote_response<'a>(
        &self,
        data: &'a [u8],
        direction: Direction,
    ) -> crate::error::VelocityResult<QuoteResponseV0<'a>> {
        let response = self.quote_response(data)?;
        crate::math::router::validate_quoted_levels(direction, response.levels)?;
        Ok(response)
    }
}

pub trait ExternalQuoterExecutor<'info> {
    /// Registry type of quoter `index` — decides whether its fills carry
    /// velocity-side resting-order aggregates to unwind (CLOB orders are
    /// margin-reserved at placement; Custom PropAMM depth is not).
    fn quoter_type(&self, index: usize) -> QuoterType;

    /// The `User` quoter `index` quotes for (`QuoterV0::user`) — the margin
    /// account the pre-execute clamp sizes Custom books against.
    fn quoter_user(&self, index: usize) -> Pubkey;

    /// The registry entry of quoter `index`.
    ///
    /// Failure messages name this rather than the index. An off-chain router
    /// reading the logs of a failed fill knows the program from the runtime's
    /// CPI brackets, but one quoter program serves many entries, so only the
    /// entry key says which maker to hold responsible.
    fn quoter_key(&self, index: usize) -> Pubkey;

    /// The prices quoter `index`'s liquidity rests at, for a caller with no
    /// quote leg to bind its fill against — the cross cranks, whose account
    /// list carries only the execute surface.
    ///
    /// Must be called before [`Self::execute`]: it reads state execute is
    /// about to consume. `None` when velocity cannot read the quoter's
    /// liquidity, which is every quoter that is not a book.
    fn resting_levels(
        &self,
        _index: usize,
        _direction: Direction,
        _size: u64,
    ) -> crate::error::VelocityResult<Option<Vec<PriceLevel>>> {
        Ok(None)
    }

    /// The users quoter `index` may return balance changes for.
    fn subjects(
        &self,
        index: usize,
        direction: Direction,
        size: u64,
    ) -> crate::error::VelocityResult<QuoterSubjects>;

    /// Lend quoter `index`'s book bytes to `f`, answering whether there was a
    /// book to lend. The borrow cannot outlive the call, so the caller gets a
    /// closure rather than a guard — the same reason a quoter's response is
    /// read in place.
    fn with_book(
        &self,
        _index: usize,
        _f: &mut dyn FnMut(&[u8]),
    ) -> crate::error::VelocityResult<bool> {
        Ok(false)
    }

    /// CPI a whole-side cancel on quoter `index` for one of its makers.
    ///
    /// Only book-backed entries can honour this; a `Custom` quoter has no
    /// orders and answers `None`. Used to clear a maker the fill has proven
    /// may not rest risk-increasing orders, once the fill it would have
    /// broken has settled.
    fn cancel_all(
        &mut self,
        _index: usize,
        _user: ClobUserRefV0,
        _sides: ClobCancelSides,
    ) -> crate::error::VelocityResult<Option<ClobCancelAllOutcomeV0>> {
        Ok(None)
    }

    /// CPI `execute_v0` on quoter `index` with the routed allocation. The
    /// response is untrusted: the router pass validates overfill, the
    /// executed price against the quoted prefix, and that every balance
    /// change lands on a permitted subject before settling anything.
    fn execute(
        &mut self,
        index: usize,
        direction: Direction,
        size: u64,
    ) -> crate::error::VelocityResult<ResponseLocationV0<'info>>;
}

/// Executor for a router fill carrying no external quoter accounts: quoting
/// produced no external books, so any external allocation is unreachable —
/// executing one is an error, not a silent skip.
pub struct NoExternalQuoters;

impl<'info> ExternalQuoterExecutor<'info> for NoExternalQuoters {
    fn quoter_type(&self, _index: usize) -> QuoterType {
        QuoterType::Custom
    }

    fn quoter_user(&self, _index: usize) -> Pubkey {
        Pubkey::default()
    }

    fn quoter_key(&self, _index: usize) -> Pubkey {
        Pubkey::default()
    }

    fn subjects(
        &self,
        _index: usize,
        _direction: Direction,
        _size: u64,
    ) -> crate::error::VelocityResult<QuoterSubjects> {
        Ok(QuoterSubjects::Book)
    }

    fn execute(
        &mut self,
        _index: usize,
        _direction: Direction,
        _size: u64,
    ) -> crate::error::VelocityResult<ResponseLocationV0<'info>> {
        msg!("router fill has no external quoter accounts to execute against");
        Err(ErrorCode::DefaultError)
    }
}

pub use quoter_spec::UserBalanceChangeV0;

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
        accounts: &[AccountInfo<'info>],
    ) -> Result<QuotedLadderV0> {
        let located = self.quote_in_place(
            market_index,
            args,
            quoter_signer,
            quoter_signer_nonce,
            accounts,
        )?;
        let data = located.borrow()?;
        let response = located.checked_quote_response(&data, args.direction)?;
        // The copy a fill earns: the split reads every book at once, and the
        // execute leg then writes the very accounts these levels sit in, so
        // the ladder has to outlive this borrow.
        Ok(QuotedLadderV0 {
            levels: response.levels.to_vec(),
            withheld: response.withheld,
        })
    }

    /// CPI `quote_v0` and leave the ladder where the quoter wrote it.
    ///
    /// A reader that consumes the levels before the next CPI takes this and
    /// pays no heap for the book. [`Self::quote`] is the same call for a
    /// caller that needs the ladder to outlive the borrow.
    pub fn quote_in_place<'info>(
        &self,
        market_index: u16,
        args: QuoteArgsV0<'_>,
        quoter_signer: &Pubkey,
        quoter_signer_nonce: u8,
        accounts: &[AccountInfo<'info>],
    ) -> Result<ResponseLocationV0<'info>> {
        self.gate_for_market(market_index)?;
        self.invoke_quoter(
            &self.quote_v0_discriminator,
            &self.quote_accounts,
            self.quote_accounts_count,
            &args,
            quoter_signer,
            quoter_signer_nonce,
            accounts,
        )
    }

    /// CPI `quote_l3_v0` and leave the rows where the quoter wrote them.
    ///
    /// `None` when the entry declares no leg, which is every quoter whose
    /// ladder stands on the one account the registry names for it. The
    /// caller attributes the ladder to [`Self::user`] in that case, so a
    /// missing leg costs nothing but per-order detail.
    ///
    /// Carries the quote leg's accounts: the two legs read the same state,
    /// and a second registered list would be a second thing an admin has to
    /// vet and keep in step.
    pub fn quote_l3<'info>(
        &self,
        market_index: u16,
        args: L3ArgsV0,
        quoter_signer: &Pubkey,
        quoter_signer_nonce: u8,
        accounts: &[AccountInfo<'info>],
    ) -> Result<Option<ResponseLocationV0<'info>>> {
        if self.quote_l3_v0_discriminator == [0u8; 8] {
            return Ok(None);
        }
        self.gate_for_market(market_index)?;
        self.invoke_quoter(
            &self.quote_l3_v0_discriminator,
            &self.quote_accounts,
            self.quote_accounts_count,
            &args,
            quoter_signer,
            quoter_signer_nonce,
            accounts,
        )
        .map(Some)
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
        accounts: &[AccountInfo<'info>],
    ) -> Result<ResponseLocationV0<'info>> {
        self.gate_for_market(market_index)?;
        self.invoke_quoter(
            &self.execute_v0_discriminator,
            &self.execute_accounts,
            self.execute_accounts_count,
            &args,
            quoter_signer,
            quoter_signer_nonce,
            accounts,
        )
    }

    /// Shared CPI leg: forward the registered accounts, send
    /// `discriminator ++ borsh(args)`, and decode the borsh response from the
    /// quoter's response account at the pointer returned via return data.
    fn invoke_quoter<
        'info,
        A: quoter_spec::wincode::SchemaWrite<quoter_spec::ArgsConfig, Src = A>,
    >(
        &self,
        discriminator: &[u8; 8],
        registered: &[AmmAccountMeta],
        count: u8,
        args: &A,
        quoter_signer: &Pubkey,
        quoter_signer_nonce: u8,
        accounts: &[AccountInfo<'info>],
    ) -> Result<ResponseLocationV0<'info>> {
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
            let info = find_account(accounts, &meta.pubkey).ok_or_else(|| {
                msg!("prop amm account {} missing from account map", meta.pubkey);
                ErrorCode::DefaultError
            })?;
            account_infos.push(info.clone());
        }
        // CPI needs the callee program's account info too.
        let program_info = find_account(accounts, &self.program_id).ok_or_else(|| {
            msg!("quoter program account missing from account map");
            ErrorCode::DefaultError
        })?;
        account_infos.push(program_info.clone());

        // Sized exactly, once, by the schema that writes the bytes. A `Vec`
        // that grows into place by doubling leaks every intermediate buffer:
        // velocity's bump allocator never reclaims, and one fill CPIs every
        // registered quoter twice. Growing rather than reserving exhausted
        // the 32 KB heap outright.
        let args_len = quoter_spec::args_size(args).map_err(|_| {
            msg!("prop amm failed to size cpi args");
            ErrorCode::DefaultError
        })?;
        let mut data = Vec::with_capacity(discriminator.len() + args_len);
        data.extend_from_slice(discriminator);
        quoter_spec::write_args(&mut data, args).map_err(|_| {
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

        let response_info = find_account(accounts, &self.response_account).ok_or_else(|| {
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

        drop(data);
        Ok(ResponseLocationV0 {
            account: response_info.clone(),
            start,
            end,
        })
    }
}
