//! The generic quoter CPI: the one interface every source answers on.
//!
//! [`QuoterConfigV0::quote`]/[`QuoterConfigV0::execute`] CPI a registered
//! quoter program with the request shapes `quoter-spec` declares, and read
//! the response in place out of the quoter's response account
//! ([`ResponseLocationV0`]). [`ExternalQuoterExecutor`] is the trait the
//! router fill drives those legs through.

use {
    super::{
        get_quoter_slab_signer_seeds, AmmAccountMeta, ClobCancelAllOutcomeV0, ClobCancelSides,
        QuoterConfigV0, QuoterSlabV0, QuoterType,
    },
    crate::{error::ErrorCode, msg, validate},
    anchor_lang::prelude::*,
    solana_program::{
        instruction::{AccountMeta, Instruction},
        program::{get_return_data, invoke_signed},
    },
    static_assertions::const_assert_eq,
};

/// Account metas for one quoter CPI leg.
///
/// NEVER forward outer signer privilege. Signer status propagates through CPI,
/// so a quoter handed the taker's wallet as a signer could CPI to the
/// system/token program and drain it. Quoters that need to know who signed the
/// outer transaction (e.g. the `flow_authority` attestation) introspect the
/// instructions sysvar instead.
///
/// The single signer is the market's quoter slab (velocity signing as
/// itself) — a registered slot for it is how a quoter authenticates that
/// velocity, not an arbitrary caller, is invoking it. See `crate::signer`
/// for why forwarding that signature to another quoter completes nothing.
pub(super) fn write_quoter_account_metas<'a>(
    into: &mut Vec<AccountMeta>,
    registered: impl Iterator<Item = &'a AmmAccountMeta>,
    slab: &Pubkey,
) {
    into.clear();
    into.extend(registered.map(|meta| AccountMeta {
        pubkey: meta.pubkey,
        is_signer: meta.pubkey == *slab,
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

/// Bytes an execute CPI's instruction data takes: discriminator, direction,
/// size, the user set, the caps, the reference price, the taker behind its
/// option tag, and the quoter's own base room.
///
/// A size rather than a constant, because the user set is length-prefixed.
/// The caller reserves exactly this much in one shot. Every field the args
/// serializer writes has to be counted here: one byte short and the `Vec`
/// doubles, which on this heap means the fill runs out of memory rather than
/// slowing down. The two single bytes are `taker_served_window` and
/// `consume_reservation`; the last eight are `self_base_room`.
pub const fn quoter_cpi_data_len(users: usize, taker: bool) -> usize {
    8 + 1
        + 8
        + quoter_user_set_bytes(users)
        + QUOTER_USER_CAPS_BYTES
        + 8
        + 1
        + if taker { CLOB_USER_REF_BYTES } else { 0 }
        + 2
        + 8
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
    /// MARGIN_PRECISION units — see [`QuoterConfigV0::oracle_band`].
    ///
    /// Defaults to the market's own band, which is what an executor that
    /// carries no registry entry can say.
    fn oracle_band(&self, _index: usize, market_margin_ratio_initial: u32) -> u32 {
        market_margin_ratio_initial
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

impl QuoterConfigV0 {
    /// Shared gate on both CPI legs: the entry takes new flow, and it takes
    /// it for the market the caller is filling. An entry is registered per
    /// `(market, program, user)`, and nothing about the CPI itself carries the
    /// market — so without the second check an entry vetted for one perp
    /// market could be quoted into another, settling its balance changes
    /// against positions it was never approved to touch.
    fn gate_for_market(&self, market_index: u16) -> Result<()> {
        validate!(
            self.is_active,
            ErrorCode::DefaultError,
            "quoter is not active"
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
        slab: &AccountLoader<'info, QuoterSlabV0>,
        accounts: &[AccountInfo<'info>],
        scratch: &mut QuoterCpiScratch<'info>,
        // Where the quoted levels are appended. See `QuotedLadderV0::levels`.
        out: &mut Vec<PriceLevel>,
    ) -> Result<QuotedLadderV0> {
        let located = self.quote_in_place(market_index, args, slab, accounts, scratch)?;
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
        slab: &AccountLoader<'info, QuoterSlabV0>,
        accounts: &[AccountInfo<'info>],
        scratch: &mut QuoterCpiScratch<'info>,
    ) -> Result<ResponseLocationV0<'info>> {
        self.gate_for_market(market_index)?;
        self.invoke_quoter(
            &self.quote_v0_discriminator,
            self.quote_leg_indexes(),
            &args,
            slab,
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
        slab: &AccountLoader<'info, QuoterSlabV0>,
        accounts: &[AccountInfo<'info>],
        scratch: &mut QuoterCpiScratch<'info>,
    ) -> Result<Option<ResponseLocationV0<'info>>> {
        if self.quote_l3_v0_discriminator == [0u8; 8] {
            return Ok(None);
        }
        self.gate_for_market(market_index)?;
        self.invoke_quoter(
            &self.quote_l3_v0_discriminator,
            self.quote_leg_indexes(),
            &args,
            slab,
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
        slab: &AccountLoader<'info, QuoterSlabV0>,
        accounts: &[AccountInfo<'info>],
        scratch: &mut QuoterCpiScratch<'info>,
    ) -> Result<ResponseLocationV0<'info>> {
        self.gate_for_market(market_index)?;
        self.invoke_quoter(
            &self.execute_v0_discriminator,
            self.execute_leg_indexes(),
            &args,
            slab,
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
        leg_indexes: &[u8],
        args: &A,
        slab: &AccountLoader<'info, QuoterSlabV0>,
        accounts: &[AccountInfo<'info>],
        scratch: &mut QuoterCpiScratch<'info>,
    ) -> Result<ResponseLocationV0<'info>> {
        // The bump is in the slab's own header, so no caller plumbs it. A
        // read borrow, so a caller may hold the slot region while this runs.
        let slab_bump = slab.load()?.bump;
        let slab_info: &AccountInfo<'info> = slab.as_ref();
        let QuoterCpiScratch { instruction, infos } = scratch;
        instruction.program_id = self.program_id;
        write_quoter_account_metas(
            &mut instruction.accounts,
            self.leg_metas(leg_indexes)?,
            slab_info.key,
        );

        infos.clear();
        for meta in instruction.accounts.iter() {
            // The slab is a named account of the outer instruction, not part
            // of the account tail the registered lists resolve against.
            if meta.pubkey == *slab_info.key {
                infos.push(slab_info.clone());
                continue;
            }
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

        // Signed as the market's slab — the one identity every quoter on the
        // market authenticates velocity by. See `crate::signer` for why the
        // shared key is safe to hand a quoter. The seeds derive from the
        // config's own market, so a slab for a different market fails the
        // runtime's signer check instead of signing.
        let market = self.market.to_le_bytes();
        let seeds = get_quoter_slab_signer_seeds(&market, &slab_bump);
        invoke_quoter_signed(instruction, infos, &[&seeds])?;

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
