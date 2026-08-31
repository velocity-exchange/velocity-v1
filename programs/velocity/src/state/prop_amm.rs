//! Quoter registry: [`QuoterV0`] entries name an external quoter program
//! (CLOB, Midpoint, custom PropAMMs; the vAMM is in-program) plus the CPI
//! surface velocity needs to call it — discriminators, account lists, and the
//! response account. [`QuoterV0::quote`]/[`QuoterV0::execute`] are the CPI
//! legs the router fill uses. Registration ixs live in
//! `instructions::quoter_registry`.
//!
//! Every CPI out of this module signs as one of velocity's two external-CPI
//! identities — the book's `clob_authority`, or the entry's own
//! `quoter_signer` (see
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
    /// The slot the approved program was last deployed at, read from its
    /// program-data account when the admin approved this entry. Zero when the
    /// program sits on a loader that cannot redeploy it, and therefore has no
    /// such account.
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
    /// it from behaviour. Deliberately not checked during a fill — that would
    /// cost one more account lock per quoter, on the budget that decides how
    /// many quoters a route can hold.
    pub approved_program_slot: u64,
    /// The furthest from oracle a fill on this entry may price, in
    /// MARGIN_PRECISION units, so one unit is one basis point. Zero means the
    /// entry declares nothing and the market's own band stands.
    ///
    /// A maker sets this to cap what its own program can lose if that program
    /// is compromised. Velocity already bounds every external leg by the
    /// market's band, and that band is sized for a market rather than for one
    /// quoter's risk appetite; this is how a quoter asks for a tighter one.
    ///
    /// Unlike the rest of the config it does not reset `is_approved`. The band
    /// applies as the smaller of this and the market's, so no value it can hold
    /// is wider than the one the admin vetted, and a maker tightening it during
    /// an incident must not have to wait for re-vetting.
    ///
    /// `Custom` entries only. A book fills third parties, so a band on one
    /// would let its entry authority revert other people's fills.
    pub max_oracle_deviation_bps: u32,
    pub padding: [u8; 12],
}

// Zero-copy layout invariant (see docs/alignment-and-native-offsets.md):
// no u128 fields, size (incl. 8-byte discriminator) ≡ 8 (mod 16).
const_assert_eq!(std::mem::size_of::<QuoterV0>(), 2784);
const_assert_eq!((QuoterV0::SIZE - 8) % 16, 0);

impl Size for QuoterV0 {
    const SIZE: usize = 2792;
}

impl QuoterV0 {
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
fn write_quoter_account_metas(
    into: &mut Vec<AccountMeta>,
    registered: &[AmmAccountMeta],
    quoter_signer: &Pubkey,
) {
    into.clear();
    into.extend(registered.iter().map(|meta| AccountMeta {
        pubkey: meta.pubkey,
        is_signer: meta.pubkey == *quoter_signer,
        is_writable: meta.is_writable,
    }));
}

/// CPI a quoter without handing the runtime an owned instruction.
///
/// [`invoke_signed`] builds its argument as
/// `StableInstruction::from(instruction.clone())`, and a clone allocates for
/// its own length. That is one copy of the account metas and one of the args on
/// every leg, on a heap the runtime never gives back, which is the cost
/// [`QuoterCpiScratch`] exists to avoid and cannot reach from the caller's side.
///
/// The runtime reads the instruction as a record of three
/// `(address, capacity, length)` triples and never reads a capacity — it
/// translates each address and length into a slice and bounds-checks it against
/// the VM's memory map. Pointing those addresses at the buffers the scratch
/// already holds says the same thing without the copy.
///
/// The aliasing probe [`invoke_signed`] runs first is kept. It is what stops a
/// callee writing an account whose data this program still holds borrowed, and
/// dropping it is what makes the SDK's own unchecked variant unsafe.
fn invoke_quoter_signed(
    instruction: &Instruction,
    infos: &[AccountInfo],
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    // Exactly the check `invoke_signed` runs, for exactly its reason.
    for meta in instruction.accounts.iter() {
        for info in infos.iter() {
            if meta.pubkey == *info.key {
                if meta.is_writable {
                    let _ = info.try_borrow_mut_lamports()?;
                    let _ = info.try_borrow_mut_data()?;
                } else {
                    let _ = info.try_borrow_lamports()?;
                    let _ = info.try_borrow_data()?;
                }
                break;
            }
        }
    }

    #[cfg(target_os = "solana")]
    {
        /// Mirrors `StableVec`, which the runtime reads as an address, a
        /// capacity it ignores, and a length.
        ///
        /// Deliberately not `StableVec` itself: that type owns its allocation
        /// and frees it on drop, so building one over borrowed memory would
        /// hand the allocator a pointer this program still owns.
        #[repr(C)]
        struct BorrowedVec {
            addr: u64,
            cap: u64,
            len: u64,
        }

        /// Mirrors `StableInstruction`. The asserts below pin it to that type,
        /// so an SDK that moves a field fails the build rather than writing
        /// through the wrong offset.
        #[repr(C)]
        struct BorrowedInstruction {
            accounts: BorrowedVec,
            data: BorrowedVec,
            program_id: Pubkey,
        }

        const _: () = {
            use solana_program::stable_layout::stable_instruction::StableInstruction;
            assert!(
                core::mem::size_of::<BorrowedInstruction>()
                    == core::mem::size_of::<StableInstruction>()
            );
            assert!(
                core::mem::align_of::<BorrowedInstruction>()
                    == core::mem::align_of::<StableInstruction>()
            );
        };

        let borrowed = BorrowedInstruction {
            accounts: BorrowedVec {
                addr: instruction.accounts.as_ptr() as u64,
                cap: instruction.accounts.len() as u64,
                len: instruction.accounts.len() as u64,
            },
            data: BorrowedVec {
                addr: instruction.data.as_ptr() as u64,
                cap: instruction.data.len() as u64,
                len: instruction.data.len() as u64,
            },
            program_id: instruction.program_id,
        };

        // SAFETY: `borrowed` has `StableInstruction`'s layout, asserted above.
        // The metas and args it addresses are the caller's and outlive this
        // call. Every meta came from a real `AccountMeta`, so its flag bytes
        // are 0 or 1, which the runtime requires. The infos and seeds are
        // passed as the SDK passes them: a data pointer and a count.
        let result = unsafe {
            solana_cpi::syscalls::sol_invoke_signed_rust(
                &borrowed as *const _ as *const u8,
                infos as *const _ as *const u8,
                infos.len() as u64,
                signer_seeds as *const _ as *const u8,
                signer_seeds.len() as u64,
            )
        };
        match result {
            0 => Ok(()),
            _ => Err(anchor_lang::solana_program::program_error::ProgramError::from(result).into()),
        }
    }

    // Off-chain the syscall does not exist, and the host harnesses run through
    // the stubbed CPI the SDK provides.
    #[cfg(not(target_os = "solana"))]
    {
        invoke_signed(instruction, infos, signer_seeds)?;
        Ok(())
    }
}

/// The three buffers a quoter CPI leg needs, allocated once and reused by every
/// leg of every entry in one instruction.
///
/// Velocity's heap is 32 KB and its allocator never reclaims, so a `Vec` per leg
/// is a `Vec` for the rest of the instruction. At capacity a leg wants ~1.1 KB
/// of account metas, ~1.6 KB of `AccountInfo`s and ~1.8 KB of args, and a fill
/// runs two legs per entry across up to `MAX_ROUTE_QUOTERS` entries — call it
/// 71 KB of scaffolding on a 32 KB heap, which is an out-of-memory abort well
/// before the route reaches the size the account-lock budget allows.
///
/// Only ever cleared and refilled: `clear` keeps the allocation, so the whole
/// fill pays for one set.
///
/// The buffers start empty and take their size from the first leg that uses
/// them. Reserving the widest a leg may be costs about 4.4 KB of the 32 KB
/// heap, and almost none of it is reachable: the wire allows 48 users but a
/// transaction's account locks cap a real route far below that, and a
/// registered CPI surface is a handful of accounts rather than the 32 the array
/// holds. A pass that calls no quoter then pays nothing at all.
///
/// Reserving the maximum would not even buy a single allocation per fill.
/// `invoke_signed` clones the `Instruction` on every leg — `StableInstruction`
/// is built `from(instruction.clone())` — and a clone allocates for its length,
/// not for the capacity it came from. The leg's own copy lands on the heap
/// whatever this holds, so the reservation buys only the building, and the
/// building is what growing from empty already pays for once.
pub struct QuoterCpiScratch<'info> {
    /// Carries the metas and the args, because `Instruction` owns both and
    /// `invoke_signed` wants an `&Instruction` — so reusing them means reusing
    /// the instruction they live in.
    instruction: Instruction,
    infos: Vec<AccountInfo<'info>>,
}

impl<'info> Default for QuoterCpiScratch<'info> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'info> QuoterCpiScratch<'info> {
    pub fn new() -> Self {
        Self {
            instruction: Instruction {
                program_id: Pubkey::default(),
                accounts: Vec::new(),
                data: Vec::new(),
            },
            infos: Vec::new(),
        }
    }
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
/// delay, a timestamp, a user ref, the taker-origin flag and the reduce-only
/// flag.
///
/// Reserved in one shot for the same reason [`quoter_cpi_data_len`] is: a
/// `Vec` that starts at the discriminator and doubles into place leaks every
/// intermediate buffer, and velocity's bump allocator never reclaims. This path
/// runs on every order placed on the book and every removal crank, so it is the
/// busier of the two.
pub const CLOB_CPI_DATA_CAPACITY: usize = 8 + 1 + 8 + 8 + 5 + 8 + CLOB_USER_REF_BYTES + 1 + 1;

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
    ExecuteArgsV0, L3ArgsV0, L3ResponseV0, L3RowV0, QuoteArgsV0, L3_ROW_FLAG_BLOCKS_WALK,
    L3_ROW_FLAG_REDUCE_ONLY, L3_ROW_FLAG_TAKER_ORIGIN,
};

/// Declared by `quoter-spec`; the alias keeps velocity's name for it.
pub type PriceLevel = quoter_spec::PriceLevelV0;

/// What one quoter answered: the ladder it stands behind, and the depth it
/// says it holds at a better price but cannot reach in this transaction.
pub struct QuotedLadderV0 {
    /// Where this ladder's levels landed in the pool the caller passed. The
    /// levels themselves are not owned here: a route quotes up to
    /// `MAX_ROUTE_QUOTERS` books and one pool for all of them is one
    /// allocation rather than one per book.
    pub levels: core::ops::Range<usize>,
    /// `price == 0` when the quoter reached everything it was asked for.
    pub withheld: PriceLevel,
}

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
/// `cancel_order_v0` args on the CLOB wire.
pub use clob_wire::{
    CancelOrderArgsV0 as ClobCancelOrderArgsV0, FillArgsV0 as ClobFillArgsV0,
    FillOutcomeV0 as ClobFillOutcomeV0, FillRequestV0 as ClobFillRequestV0,
    FilledOrderV0 as ClobFilledOrderV0,
};
/// Which sides a `cancel_all_v0` withdraws, on the CLOB wire. Declared by
/// `quoter-spec`; the alias keeps velocity's name for it.
pub use quoter_spec::CancelSidesV0 as ClobCancelSides;
/// One order a CLOB removed as a sub-min remainder of a fill.
///
/// `base_asset_amount` releases a maker's margin reservation and the
/// completed-order ids beside it decrement their open-order counts. That maker
/// is an ordinary velocity user resting on the book, not the quoter's own
/// account, so the subject rule bounds *whose* orders an entry may remove but
/// not whether it described what it removed.
///
/// The bound on the description is the reservation itself. Velocity wrote
/// `open_bids` / `open_asks` when the order was placed, under its owner's
/// signature, and holds every removal report to it
/// ([`PerpPosition::reserved_open_base`]): a report above what the user has
/// resting fails rather than freeing the margin behind orders that still rest.
/// The same bound covers `evict_worst_v0` and `remove_expired_v0`, which drive
/// identical unwinding, and the fill path's balance changes.
///
/// The one place it is deliberately lenient is a removal the owner signed for
/// (`cancel_order_v0` through `cancel_order_v1` and the sweeps). Those run
/// against books that may be dead or de-listed, so an over-report clamps and
/// logs instead of failing: a maker must always be able to leave a book that
/// reports garbage.
///
/// What the reservation does not bound is which of a user's orders a report
/// names. An id for an order still live on the book frees a placed trigger's
/// shadow while the book keeps the size, and the size still has to be one the
/// user really placed.
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
    fn reduce_only_orders(&self) -> u32;
    fn base_for(&self, direction: crate::controller::position::PositionDirection) -> u64;
    fn orders_for(&self, direction: crate::controller::position::PositionDirection) -> u32;
}

impl ClobCancelAllOutcomeExt for ClobCancelAllOutcomeV0 {
    fn orders(&self) -> u32 {
        self.bid_orders.saturating_add(self.ask_orders)
    }

    /// Reduce-only orders the sweep removed, across both sides. The caller
    /// disarms its per-user reduce-only tracking by this many.
    fn reduce_only_orders(&self) -> u32 {
        self.bid_reduce_only_orders
            .saturating_add(self.ask_reduce_only_orders)
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
pub const CLOB_FILL_V0_DISCRIMINATOR: [u8; 8] = [66, 113, 11, 94, 94, 23, 154, 137];
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
/// `invoke_signed` with the fixed `[market (w), clob_authority (s)]` account
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
    pub clob_authority: &'a AccountInfo<'info>,
    pub clob_authority_nonce: u8,
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
        clob_authority: &'a AccountInfo<'info>,
        clob_authority_nonce: u8,
    ) -> Result<Self> {
        quoter.validate_clob_book(market_index, &market.key())?;
        Ok(Self {
            market,
            program,
            clob_authority,
            clob_authority_nonce,
        })
    }

    /// Rest a new order on the book; returns the CLOB's handle for it.
    pub fn place(&self, args: ClobPlaceOrderArgsV0) -> Result<ClobOrderRefV0> {
        self.invoke(&CLOB_PLACE_ORDER_V0_DISCRIMINATOR, &args, "place")
    }

    /// Report fills velocity made against taker remainders resting on this
    /// book, so they shrink in place.
    ///
    /// The mirror of [`Self::execute`]. That one is the book filling its own
    /// orders for a taker; this is velocity telling the book about a fill it
    /// could not have made itself, because a taker remainder aggresses against
    /// quoters and the vAMM as well as against the book.
    ///
    /// In place, so the order keeps its queue position and its id. Cancelling
    /// and re-placing would send a partly-filled remainder to the back of its
    /// own level for a fill that never changed its price.
    pub fn fill(&self, args: ClobFillArgsV0) -> Result<ClobFillOutcomeV0> {
        self.invoke(&CLOB_FILL_V0_DISCRIMINATOR, &args, "fill")
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
                    AccountMeta::new_readonly(self.clob_authority.key(), true),
                ],
                data,
            },
            &[
                self.market.clone(),
                self.clob_authority.clone(),
                self.program.clone(),
            ],
            &[&get_clob_authority_seeds(&self.clob_authority_nonce)],
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
    // The sweep took this many reduce-only orders off the book, so disarm the
    // counter by the same amount.
    user.perp_positions[position_index]
        .disarm_reduce_only_clob_by(swept.reduce_only_orders().min(u16::MAX as u32) as u16);
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
    /// What bounds a book instead is the record velocity wrote itself. Every
    /// CLOB order reserved `open_bids` / `open_asks` on its owner's position at
    /// placement, under that owner's signature, and no external program can
    /// write those. So a response is held to
    /// [`PerpPosition::reserved_open_base`]: a book may only fill or remove
    /// size a user really placed, on the side they placed it. That bound needs
    /// nothing from the book's own state, which is why it stands where the
    /// arena walk did not. It covers the removal reports on the same terms (see
    /// [`ClobRemovedOrderV0`]).
    ///
    /// On top of it: the response may only name users the transaction already
    /// carries, every balance change is held to the quoted prices and to the
    /// oracle band, and every user it touches is margin-checked after the fill.
    ///
    /// Two things it does not bound. The reservation records size and side, not
    /// price, so an order a user did place can still be filled anywhere inside
    /// the oracle band — the leak is the band's width across the size they
    /// posted. And `open_bids` does not separate a book's reservation from the
    /// DLOB's on the same market and side, so a book's report can consume
    /// reservation a DLOB order made. Splitting them needs sixteen bytes on
    /// [`PerpPosition`], which has two.
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

    /// How far from oracle a fill on quoter `index` may price, in
    /// MARGIN_PRECISION units — see [`QuoterV0::oracle_band`].
    ///
    /// Defaults to the market's own band, which is what an executor that
    /// carries no registry entry can say.
    fn oracle_band(&self, _index: usize, market_margin_ratio_initial: u32) -> u32 {
        market_margin_ratio_initial
    }

    /// The prices quoter `index`'s liquidity rests at, for a caller with no
    /// quote leg to bind its fill against — the cross cranks, whose account
    /// list carries only the execute surface.
    ///
    /// Must be called before [`Self::execute`]: it reads state execute is
    /// about to consume. `None` when velocity cannot read the quoter's
    /// liquidity, which is every quoter that is not a book.
    fn resting_levels(
        &mut self,
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
        // A Clob entry's program is pinned at registration to the CLOB velocity
        // wrote. Re-check on the hot path so the book trust here never rests on
        // a stale entry the pin did not cover.
        validate!(
            self.program_id == crate::ids::clob_program::id(),
            ErrorCode::DefaultError,
            "clob quoter runs program {}, not velocity's CLOB",
            self.program_id
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
        scratch: &mut QuoterCpiScratch<'info>,
        // Where the quoted levels are appended. See `QuotedLadderV0::levels`.
        out: &mut Vec<PriceLevel>,
    ) -> Result<QuotedLadderV0> {
        let located = self.quote_in_place(
            market_index,
            args,
            entry,
            quoter_signer,
            quoter_signer_nonce,
            accounts,
            scratch,
        )?;
        let data = located.borrow()?;
        let response = located.checked_quote_response(&data, args.direction)?;
        // The copy a fill earns: the split reads every book at once, and the
        // execute leg then writes the very accounts these levels sit in, so
        // the ladder has to outlive this borrow.
        //
        // Copied into the caller's pool rather than into a list of its own. A
        // route quotes up to `MAX_ROUTE_QUOTERS` books and reads them all
        // together, so one pool holds every ladder for one allocation instead
        // of one per book, on a heap that never reclaims.
        //
        // Truncated at what a reader can use. The router's cursor and its level
        // validation both stop at `MAX_LEVELS_PER_BOOK`, so a deeper ladder is
        // copied and then never read — and a market may set `max_quote_levels`
        // high enough that the discarded tail is kilobytes per book.
        let usable = response
            .levels
            .len()
            .min(crate::math::router::MAX_LEVELS_PER_BOOK);
        let start = out.len();
        out.extend_from_slice(&response.levels[..usable]);
        Ok(QuotedLadderV0 {
            levels: start..out.len(),
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
        scratch: &mut QuoterCpiScratch<'info>,
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
            scratch,
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
        scratch: &mut QuoterCpiScratch<'info>,
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
            scratch,
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
        scratch: &mut QuoterCpiScratch<'info>,
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
            scratch,
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
        scratch: &mut QuoterCpiScratch<'info>,
    ) -> Result<ResponseLocationV0<'info>> {
        validate!(
            (count as usize) <= registered.len(),
            ErrorCode::DefaultError,
            "prop amm accounts_count {} exceeds capacity {}",
            count,
            registered.len()
        )?;
        let QuoterCpiScratch { instruction, infos } = scratch;
        instruction.program_id = self.program_id;
        write_quoter_account_metas(
            &mut instruction.accounts,
            &registered[..count as usize],
            quoter_signer,
        );

        infos.clear();
        for meta in instruction.accounts.iter() {
            let info = find_account(accounts, &meta.pubkey).ok_or_else(|| {
                msg!("prop amm account {} missing from account map", meta.pubkey);
                ErrorCode::DefaultError
            })?;
            infos.push(info.clone());
        }
        // CPI needs the callee program's account info too.
        let program_info = find_account(accounts, &self.program_id).ok_or_else(|| {
            msg!("quoter program account missing from account map");
            ErrorCode::DefaultError
        })?;
        infos.push(program_info.clone());

        // Written into the buffer the scratch already reserved, which is sized
        // at the widest any leg can be. `QUOTER_CPI_DATA_MAX` counts every byte
        // the args serializer writes, so a leg that does not fit means that
        // constant is wrong — say so rather than letting the `Vec` double and
        // leak the buffer it grew out of.
        let args_len = quoter_spec::args_size(args).map_err(|_| {
            msg!("prop amm failed to size cpi args");
            ErrorCode::DefaultError
        })?;
        let data_len = discriminator.len().saturating_add(args_len);
        validate!(
            data_len <= QUOTER_CPI_DATA_MAX,
            ErrorCode::DefaultError,
            "prop amm cpi args are {} bytes, above the {} the wire allows",
            data_len,
            QUOTER_CPI_DATA_MAX
        )?;
        instruction.data.clear();
        instruction.data.extend_from_slice(discriminator);
        quoter_spec::write_args(&mut instruction.data, args).map_err(|_| {
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
        invoke_quoter_signed(instruction, infos, &[signer_seeds])?;

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
