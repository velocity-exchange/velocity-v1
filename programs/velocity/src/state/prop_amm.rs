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
        error::ErrorCode,
        msg,
        signer::{get_clob_authority_seeds, get_quoter_signer_seeds},
        state::traits::Size,
        validate,
    },
    anchor_lang::prelude::*,
    solana_program::{
        instruction::{AccountMeta, Instruction},
        program::{get_return_data, invoke, invoke_signed},
    },
    static_assertions::const_assert_eq,
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

/// Bytes an execute CPI's instruction data takes: discriminator, direction,
/// size, the user set, the caps, the reference price, and the taker behind
/// its option tag.
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

/// The same for a quote, which also carries the caller's worst acceptable
/// price. Execute is handed a size cut off the ladder rather than a bound, so
/// the two legs are eight bytes apart.
pub const fn quote_cpi_data_len(users: usize, taker: bool) -> usize {
    quoter_cpi_data_len(users, taker) + 8
}

/// Widest either leg can be: a quote with a full user set and a taker.
pub const QUOTER_CPI_DATA_MAX: usize = quote_cpi_data_len(MAX_QUOTER_WIRE_USERS, true);

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

/// `cancel_order_v0` args on the CLOB wire.
pub use clob_wire::CancelOrderArgsV0 as ClobCancelOrderArgsV0;
/// Order handle on the CLOB: an O(1) node hint verified against the order id
/// there, so a stale hint fails closed on the CLOB side.
pub use clob_wire::ClobOrderRefV0;
/// `place_order_v0` args on the CLOB wire.
///
/// `taker_origin` marks the order as an unfilled taker remainder velocity
/// migrated onto the book rather than a quote its owner chose to post. Only
/// velocity knows that, which is why the CLOB takes it as an argument. It
/// changes two things on the book — the order cannot be taken while a live
/// counterparty crosses it, and a cross involving it settles at the
/// counterparty's price.
pub use clob_wire::PlaceOrderArgsV0 as ClobPlaceOrderArgsV0;
/// Wire form of an order the CLOB removed — return data of its
/// cancel/evict/expire ixs, so velocity can decrement the maker's
/// open-order aggregates by the remaining size on the right side.
///
/// Its `taker_origin` flag is the only place the CLOB reports that a removed
/// order was a migrated taker remainder, which is what tells velocity which
/// side of a cross was demanding liquidity — hence which side's price the
/// match settles at.
pub use clob_wire::RemovedOrderV0 as ClobRemovedOrderV0;
/// Which sides a `cancel_all_v0` withdraws, on the CLOB wire. Declared by
/// `quoter-spec`; the alias keeps velocity's name for it.
pub use quoter_spec::CancelSidesV0 as ClobCancelSides;
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
/// must be checked against the book's own `quote_l3_v0`, asked before execute
/// consumes the orders, and the same check owed to the removal cranks.
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
pub use clob_wire::CancelAllArgsV0 as ClobCancelAllArgsV0;
/// What the CLOB's `cancel_all_v0` withdrew: per-side totals rather than a list
/// of removals, which is exactly the shape the open-order aggregates consume —
/// one `decrease_open_bids_and_asks` per side and one count, however many
/// orders the sweep took.
pub use clob_wire::CancelAllOutcomeV0 as ClobCancelAllOutcomeV0;

/// What velocity reads into the sweep outcome beyond its shape: which side a
/// maker's position rests on. An inherent impl is not available on a type the
/// wire crate owns, and the direction is velocity's own type — the same reason
/// [`ClobCancelSidesExt`] is a trait.
pub trait ClobCancelAllOutcomeExt {
    fn orders(&self) -> u32;
    fn base_for(&self, direction: crate::controller::position::PositionDirection) -> u64;
    fn orders_for(&self, direction: crate::controller::position::PositionDirection) -> u32;
}

impl ClobCancelAllOutcomeExt for ClobCancelAllOutcomeV0 {
    fn orders(&self) -> u32 {
        self.bid_orders.saturating_add(self.ask_orders)
    }

    /// Base amount withdrawn on the side a maker position of `direction` rests
    /// on — a bid is a long, an ask a short.
    fn base_for(&self, direction: crate::controller::position::PositionDirection) -> u64 {
        match direction {
            crate::controller::position::PositionDirection::Long => self.bid_base_asset_amount,
            crate::controller::position::PositionDirection::Short => self.ask_base_asset_amount,
        }
    }

    /// Orders withdrawn on that same side.
    fn orders_for(&self, direction: crate::controller::position::PositionDirection) -> u32 {
        match direction {
            crate::controller::position::PositionDirection::Long => self.bid_orders,
            crate::controller::position::PositionDirection::Short => self.ask_orders,
        }
    }
}

/// `evict_worst_v0` args on the CLOB wire.
pub use clob_wire::EvictWorstArgsV0 as ClobEvictWorstArgsV0;
/// `order_rules_v0`'s answer: what the book requires of an order before it
/// will hold one. Asked rather than read, so a rule the book moves is a rule
/// velocity still gets right.
pub use clob_wire::OrderRulesV0 as ClobOrderRulesV0;
/// `remove_expired_v0` args on the CLOB wire.
pub use clob_wire::RemoveExpiredArgsV0 as ClobRemoveExpiredArgsV0;
/// `next_removal_v0` args and answer: which order the book would let a caller
/// reclaim, and why. The book decides both — velocity supplies the removal's
/// consequences, not the search.
pub use clob_wire::{ClobRemovalKindV0, NextRemovalArgsV0 as ClobNextRemovalArgsV0};
/// `set_crank_conditions_v0`: who resolves each of the book's own conditions.
/// The book owns the wakes; velocity registers the answers.
pub use clob_wire::{
    CrankAccountV0 as ClobCrankAccountV0, CrankBlockV0 as ClobCrankBlockV0,
    CrankConditionsArgsV0 as ClobCrankConditionsArgsV0, CrankResolverV0 as ClobCrankResolverV0,
};
/// The one shape every read-only CLOB answer describes an order in, and the
/// two answers built from it: the best matchable order on each side, and one
/// view per requested ref.
pub use clob_wire::{
    NextCrossV0 as ClobNextCrossV0, OrderViewV0 as ClobOrderViewV0,
    OrdersArgsV0 as ClobOrdersArgsV0, OrdersV0 as ClobOrdersV0,
    ORDER_VIEW_CEILING as CLOB_ORDER_VIEW_CEILING,
};
/// Which slot of the book's block each registered resolver lands in. Relay
/// names the fired condition by slot, so one resolver serving several of them
/// reads the mapping from here rather than from the book's own numbering.
pub use clob_wire::{
    CRANK_SLOT_ACTIVATION as CLOB_CRANK_SLOT_ACTIVATION,
    CRANK_SLOT_CAPACITY as CLOB_CRANK_SLOT_CAPACITY, CRANK_SLOT_CROSS as CLOB_CRANK_SLOT_CROSS,
    CRANK_SLOT_EXPIRY as CLOB_CRANK_SLOT_EXPIRY,
};

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
pub const CLOB_NEXT_REMOVAL_V0_DISCRIMINATOR: [u8; 8] = [132, 65, 9, 126, 135, 115, 177, 92];
pub const CLOB_NEXT_CROSS_V0_DISCRIMINATOR: [u8; 8] = [234, 191, 102, 36, 183, 233, 127, 48];
pub const CLOB_SET_CRANK_CONDITIONS_V0_DISCRIMINATOR: [u8; 8] = [34, 160, 120, 93, 84, 133, 8, 95];
pub const CLOB_ORDERS_V0_DISCRIMINATOR: [u8; 8] = [124, 117, 208, 33, 202, 209, 58, 199];
pub const CLOB_ORDER_RULES_V0_DISCRIMINATOR: [u8; 8] = [201, 129, 212, 105, 18, 69, 149, 252];

/// The velocity-mediated CLOB CPI surface, bound to one book: the three
/// accounts every call takes plus the signer nonce that lets velocity sign as
/// the book's `place_authority`.
///
/// This and [`ClobReader`] are the only places in the program that speak the
/// CLOB's wire — the discriminators above, the borsh arg encoding, the
/// `invoke_signed` with the fixed `[market (w), quoter_signer (s)]` account
/// pair, and the return-data decode (writer-checked, so a program the CLOB
/// CPI'd into can't spoof the response). Every caller — placement, cancel,
/// the evict/expire cranks, force-cancel — goes through a method here. The
/// split is signing: this half changes the book and signs as its
/// `place_authority`; the reader only asks, and signs nothing.
///
/// Velocity speaks *only* this wire. It holds no copy of the market account's
/// layout, so what the book has to answer is what these methods ask for, and
/// where a field sits inside its account is the book's own business.
///
/// `execute_v0` is deliberately absent: that leg is the *registry* wire every
/// quoter type shares ([`QuoterV0::execute`] → `invoke_quoter`), whose
/// account list is per-entry registered rather than this fixed pair, and it
/// already has exactly one implementation. `quote_v0` and `quote_l3_v0` are
/// absent for the same reason — the book answers for its depth on the
/// interface every source shares.
pub struct ClobMarket<'a, 'info> {
    /// The book account, passed writable.
    pub market: &'a AccountInfo<'info>,
    /// The registered CLOB program.
    pub program: &'a AccountInfo<'info>,
    /// The CLOB place authority PDA — what a book's `place_authority` is set
    /// to. The CLOB gates place/cancel/evict/expire *and* `execute_v0` on that
    /// one field, so this leg and the registry's execute leg for a CLOB entry
    /// necessarily sign as the same key.
    ///
    /// It is its own key rather than the one a third-party quoter is handed.
    /// Signer privilege is inherited by a callee, and this key may place and
    /// cancel on any book for *any* user (`place_order_v0` takes the user as an
    /// argument), so a quoter that received it and also held a book in its
    /// account list could rest unreserved orders on that book or wipe it.
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

    /// Register who resolves the book's own crank conditions. Returns the
    /// account offset the block sits at, which is what a relay watch
    /// registration points at — asked for rather than derived, so velocity
    /// never has to know the market account's layout.
    pub fn set_crank_conditions(
        &self,
        args: ClobCrankConditionsArgsV0,
    ) -> Result<ClobCrankBlockV0> {
        self.invoke(
            &CLOB_SET_CRANK_CONDITIONS_V0_DISCRIMINATOR,
            &args,
            "set crank conditions",
        )
    }

    /// Reclaim the worst order on a side past its soft cap (the CLOB
    /// re-checks the threshold).
    pub fn evict(&self, args: ClobEvictWorstArgsV0) -> Result<ClobRemovedOrderV0> {
        self.invoke(&CLOB_EVICT_WORST_V0_DISCRIMINATOR, &args, "evict")
    }

    /// The read-only half of the same wire, for the questions this call site
    /// asks the book about its own memory rather than telling it to change.
    pub fn reader(&self) -> ClobReader<'a, 'info> {
        ClobReader {
            market: self.market,
            program: self.program,
        }
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
            &[&get_clob_authority_seeds(&self.quoter_signer_nonce)],
        )?;

        clob_response(&self.program.key(), what)
    }
}

/// Decode what a CLOB call left as return data.
///
/// Return data is last-writer-wins within the transaction, so the writer must
/// be the book's own program: otherwise a program the CLOB CPI'd into could
/// dictate the response velocity settles on. `what` only names the call in
/// error messages.
fn clob_response<R: AnchorDeserialize>(program: &Pubkey, what: &str) -> Result<R> {
    let (writer, response) = get_return_data().ok_or_else(|| -> Error {
        msg!("clob {} returned no response", what);
        ErrorCode::DefaultError.into()
    })?;
    validate!(
        writer == *program,
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

/// The read-only leg of the CLOB wire: ask the book what removal work it has.
///
/// Separate from [`ClobMarket`] because it signs nothing. A crank resolver
/// runs under simulation and holds no quoter signer, and the question it asks
/// — which order is expired, which order is past the eviction threshold — is
/// one the book answers about its own memory. Velocity supplies a removal's
/// consequences, not the search for it.
pub struct ClobReader<'a, 'info> {
    pub market: &'a AccountInfo<'info>,
    pub program: &'a AccountInfo<'info>,
}

impl ClobReader<'_, '_> {
    /// The next order of `args.kind` the book would let a caller reclaim, or
    /// [`ClobOrderViewV0::NONE`] when it has none.
    pub fn next_removal(&self, args: ClobNextRemovalArgsV0) -> Result<ClobOrderViewV0> {
        let mut data = Vec::with_capacity(CLOB_CPI_DATA_CAPACITY);
        data.extend_from_slice(&CLOB_NEXT_REMOVAL_V0_DISCRIMINATOR);
        args.serialize(&mut data).map_err(|_| {
            msg!("failed to serialize clob next removal args");
            ErrorCode::DefaultError
        })?;
        self.ask(data, "next removal")
    }

    /// The best matchable order on each side — what a cross would settle
    /// between. Both heads are [`ClobOrderViewV0::NONE`] on an empty book.
    pub fn next_cross(&self) -> Result<ClobNextCrossV0> {
        self.ask(CLOB_NEXT_CROSS_V0_DISCRIMINATOR.to_vec(), "next cross")
    }

    /// What the book requires of an order before it will hold one. Asked
    /// rather than read off the market account, so a rule the book moves is a
    /// rule velocity still gets right.
    pub fn order_rules(&self) -> Result<ClobOrderRulesV0> {
        self.ask(CLOB_ORDER_RULES_V0_DISCRIMINATOR.to_vec(), "order rules")
    }

    /// What the book holds for each of `refs`, in the order given. A ref that
    /// no longer names a live order comes back as [`ClobOrderViewV0::NONE`] —
    /// a fill or a crank got there first, which is the expected outcome of
    /// the race rather than an error.
    pub fn orders(&self, refs: Vec<ClobOrderRefV0>) -> Result<Vec<ClobOrderViewV0>> {
        let mut data = Vec::with_capacity(CLOB_CPI_DATA_CAPACITY);
        data.extend_from_slice(&CLOB_ORDERS_V0_DISCRIMINATOR);
        ClobOrdersArgsV0 { refs }
            .serialize(&mut data)
            .map_err(|_| {
                msg!("failed to serialize clob orders args");
                ErrorCode::DefaultError
            })?;
        self.ask::<ClobOrdersV0>(data, "orders")
            .map(|answer| answer.orders)
    }

    /// One read-only CPI, unsigned: the book answers about its own memory, so
    /// nothing here needs the quoter signer.
    fn ask<R: AnchorDeserialize>(&self, data: Vec<u8>, what: &str) -> Result<R> {
        invoke(
            &Instruction {
                program_id: self.program.key(),
                accounts: vec![AccountMeta::new_readonly(self.market.key(), false)],
                data,
            },
            &[self.market.clone(), self.program.clone()],
        )?;
        clob_response(&self.program.key(), what)
    }
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
    clob: &ClobReader,
    market_index: u16,
    sides: ClobCancelSides,
    swept: &ClobCancelAllOutcomeV0,
) -> Result<u32> {
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
    release_swept_trigger_shadows(user, clob, market_index, sides)?;
    Ok(orders)
}

/// Free the placed-trigger shadows on `sides` whose live orders a bulk sweep
/// took, and report how many were freed.
///
/// Driven off what the *book* still holds rather than off a list of removed
/// ids: a shadow is released exactly when its ref no longer names its order,
/// which is true whether the sweep was capped or not, and is strictly more
/// robust than matching ids — a shadow whose order left the book by any route
/// reads as released here. The scan is over `User.orders`, so it is bounded by
/// that array, not by the book.
///
/// Asked in chunks because the book answers about
/// [`CLOB_ORDER_VIEW_CEILING`] refs per call, and a user may hold more
/// shadows than that.
pub fn release_swept_trigger_shadows(
    user: &mut crate::state::user::User,
    clob: &ClobReader,
    market_index: u16,
    sides: ClobCancelSides,
) -> Result<usize> {
    use crate::state::user::{MarketType, OrderStatus};
    let candidates: Vec<(usize, ClobOrderRefV0)> = user
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
        .map(|(index, order)| {
            let (node_index, order_id) = order.clob_order_ref();
            (
                index,
                ClobOrderRefV0 {
                    node_index,
                    order_id,
                },
            )
        })
        .collect();

    let mut freed = 0usize;
    for chunk in candidates.chunks(CLOB_ORDER_VIEW_CEILING) {
        let views = clob.orders(chunk.iter().map(|(_, r)| *r).collect())?;
        chunk
            .iter()
            .zip(views.iter())
            .filter(|(_, view)| !view.found())
            .for_each(|((index, _), _)| {
                user.orders[*index].status = OrderStatus::Canceled;
                freed += 1;
            });
    }
    Ok(freed)
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
    /// `protocol_authority` is `State::signer`. The protocol `User` is never a
    /// subject either: it is the inventory-free taker the cranks fill through,
    /// so a quoter that could name it would move a position onto protocol
    /// funds at a price of its own choosing. It is excluded by identity rather
    /// than by never being loaded, because the cross cranks load it on purpose.
    pub fn permits(
        &self,
        user: &ClobUserRefV0,
        key: &Pubkey,
        taker: &ClobUserRefV0,
        protocol_authority: &Pubkey,
    ) -> bool {
        if user == taker {
            return false;
        }
        if user.sub_account_id == 0 && user.authority == *protocol_authority {
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
    /// The identity velocity signs this entry's CPI legs as, and its bump.
    ///
    /// A `Clob` entry signs as the book's place authority, because that is the
    /// key `execute_v0` requires; the caller passes it in because anchor already
    /// derived it for the named account and its bump is free there. Every other
    /// entry signs as a key derived from the entry itself, which is what keeps a
    /// quoter's signature from authenticating anywhere but at that quoter — not
    /// at a book, and not at a second quoter the same maker controls.
    pub fn cpi_signer(&self, entry: &Pubkey, clob_authority: (Pubkey, u8)) -> (Pubkey, u8) {
        match self.quoter_type {
            QuoterType::Clob => clob_authority,
            _ => crate::signer::find_quoter_signer(entry),
        }
    }

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
        entry: &Pubkey,
        quoter_signer: &Pubkey,
        quoter_signer_nonce: u8,
        accounts: &[AccountInfo<'info>],
    ) -> Result<QuotedLadderV0> {
        let located = self.quote_in_place(
            market_index,
            args,
            entry,
            quoter_signer,
            quoter_signer_nonce,
            accounts,
        )?;
        let data = located.borrow()?;
        let response = located.checked_quote_response(&data, args.direction)?;
        // The copy a fill earns: the split reads every book at once, and the
        // execute leg then writes the very accounts these levels sit in, so
        // the ladder has to outlive this borrow.
        //
        // Truncated at what a reader can use. The router's cursor and its level
        // validation both stop at `MAX_LEVELS_PER_BOOK`, so a deeper ladder is
        // copied and then never read — and a market may set `max_quote_levels`
        // high enough that the discarded tail is kilobytes per book, on a 32 KB
        // heap that never reclaims.
        let usable = response
            .levels
            .len()
            .min(crate::math::router::MAX_LEVELS_PER_BOOK);
        Ok(QuotedLadderV0 {
            levels: response.levels[..usable].to_vec(),
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
        entry: &Pubkey,
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
            entry,
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
        entry: &Pubkey,
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
            entry,
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
        entry: &Pubkey,
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
            entry,
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
        entry: &Pubkey,
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

        // Signed as whichever identity this entry authenticates velocity by,
        // and the two families are deliberately separate. A `Clob` entry
        // authenticates by the book's place authority, because that is the key
        // its instructions require. Every other entry gets a key derived from
        // its own registry entry, so the signature it receives proves velocity
        // called *it* and proves nothing anywhere else — forwarded to a second
        // quoter it does not authenticate, and it is not any book's authority.
        let clob_seeds;
        let entry_seeds;
        let signer_seeds: &[&[u8]] = match self.quoter_type {
            QuoterType::Clob => {
                clob_seeds = get_clob_authority_seeds(&quoter_signer_nonce);
                &clob_seeds
            }
            _ => {
                entry_seeds = get_quoter_signer_seeds(entry, &quoter_signer_nonce);
                &entry_seeds
            }
        };
        invoke_signed(
            &Instruction {
                program_id: self.program_id,
                accounts: account_metas,
                data,
            },
            &account_infos,
            &[signer_seeds],
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
