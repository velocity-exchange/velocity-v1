//! The generic quoter CPI: the one interface every source answers on.
//!
//! [`QuoterConfigV0::quote_in_place`] and [`QuoterConfigV0::execute`] CPI a
//! registered quoter program with the request shapes `quoter-spec` declares.
//! Both read the response in place out of the quoter's response account
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
/// Never forward outer signer privilege. Signer status propagates through CPI,
/// so a quoter handed the taker's wallet as a signer could CPI to the system
/// program or the token program and drain it. A quoter that must know who
/// signed the outer transaction reads the instructions sysvar instead. The
/// `flow_authority` attestation works that way.
///
/// The single signer is the market's quoter slab, which is velocity signing as
/// itself. A registered slot for that key is how a quoter authenticates that
/// velocity, and not an arbitrary caller, is invoking it. See `crate::signer`
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
/// its own length. That is one copy of the account metas and one copy of the
/// args on every leg, on a heap the runtime never gives back.
/// [`QuoterCpiScratch`] exists to avoid that copy and cannot reach it from the
/// caller's side.
///
/// The runtime reads the instruction as two `(address, capacity, length)`
/// triples and a program id, and it never reads a capacity. It turns each
/// address and length into a slice and bounds-checks the slice against the
/// VM's memory map. Addresses that point at the buffers the scratch already
/// holds say the same thing without the copy.
///
/// The aliasing probe that [`invoke_signed`] runs first is kept. The probe
/// stops a callee writing an account whose data this program still holds
/// borrowed. Dropping it is what makes the SDK's own unchecked variant unsafe.
fn invoke_quoter_signed(
    instruction: &Instruction,
    infos: &[AccountInfo],
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    // The aliasing probe, copied from `invoke_signed` unchanged.
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
        /// This is not `StableVec` itself. That type owns its allocation and
        /// frees it on drop, so building one over borrowed memory would hand
        /// the allocator a pointer this program still owns.
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
/// is a `Vec` for the rest of the instruction. One leg at capacity wants its
/// account metas, its `AccountInfo`s, and up to [`QUOTER_CPI_DATA_MAX`] bytes
/// of args. A fill runs two legs per entry across up to
/// [`super::MAX_ROUTE_QUOTERS`] entries, which is more scaffolding than the
/// heap holds. The pass then aborts out of memory well before the route reaches
/// the size the account-lock budget allows.
///
/// The buffers are only ever cleared and refilled. `clear` keeps the
/// allocation, so the whole fill pays for one set.
///
/// They start empty and take their size from the first leg that uses them.
/// Reserving the widest a leg may be is close to unreachable. The wire allows
/// [`MAX_QUOTER_WIRE_USERS`] users, but a transaction's account locks cap a
/// real route far below that. A registered CPI surface is also a handful of
/// accounts rather than the [`super::MAX_QUOTER_ACCOUNTS`] the array holds. A
/// pass that calls no quoter then pays nothing at all.
pub struct QuoterCpiScratch<'info> {
    /// Carries the metas and the args. `Instruction` owns both, and the CPI
    /// leg takes an `&Instruction`, so reusing them means reusing the
    /// instruction they live in.
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

/// Declared by `quoter-spec`, the same crate that declares the responses.
/// Velocity writes these bytes and a quoter reads them, so a second
/// declaration here would be a second thing to keep in step. The aliases keep
/// velocity's names.
pub use quoter_spec::{DirectionV0 as Direction, SideV0 as ClobSide};

/// What velocity reads into the wire's direction and side beyond their shape.
///
/// A foreign type takes no inherent impl. A trait keeps every call site
/// reading as it did.
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

/// A velocity user on the quoter wire, in its derivable form: an authority
/// wallet and a sub-account index.
///
/// The `User` PDA and the `UserStats` PDA both derive from those two fields.
/// An off-chain reader such as a relay resolver staging a crank therefore
/// reaches every user-derived account from a quoter's state alone. A stored
/// `User` key reaches nothing, because its authority lives inside account data
/// the reader cannot load. Velocity resolves refs against its loaded users by
/// field match and never by PDA derivation, so the hot path pays nothing for
/// this.
///
/// Declared by `quoter-spec`, which every program on this wire compiles
/// against. `Pubkey` and the v2 programs' `Address` are the same type, so the
/// declaration needs no per-program restatement. The alias keeps velocity's
/// name for it.
pub type ClobUserRefV0 = quoter_spec::UserRefV0;

/// Borsh width of a [`ClobUserRefV0`].
pub const CLOB_USER_REF_BYTES: usize = quoter_spec::UserRefV0::SIZE;
const_assert_eq!(std::mem::size_of::<ClobUserRefV0>(), CLOB_USER_REF_BYTES);

/// The loaded-user set on the quoter wire: a length prefix and that many
/// entries.
///
/// An empty set means unrestricted. Only callers that settle nothing send one,
/// which is the quote view and cross discovery. A quoter reading a non-empty
/// set must skip liquidity whose owner is absent from it. Velocity cannot
/// settle a balance change for a `User` it did not load, so it refuses such a
/// fill whole.
///
/// Declared by `quoter-spec` beside the responses. The aliases keep velocity's
/// names for them.
pub use quoter_spec::{
    user_set_bytes as quoter_user_set_bytes, UserCapV0 as QuoterUserCapV0,
    UserCapsV0 as QuoterUserCapsV0, USER_CAPS_BYTES as QUOTER_USER_CAPS_BYTES,
    USER_CAPS_CAPACITY as MAX_CONSTRAINED_WIRE_USERS, USER_EXCLUSION_BITMAP_BYTES,
    USER_SET_CAPACITY as MAX_QUOTER_WIRE_USERS, USER_SET_MAX_BYTES as QUOTER_USER_SET_MAX_BYTES,
};

/// Bytes an execute CPI's instruction data takes. The wire order is the
/// discriminator, the user set, the direction, the size, the caps, the
/// reference price, and the taker behind its option tag. The two trailing
/// bytes are `taker_served_window` and `consume_reservation`.
///
/// A size rather than a constant, because the user set is length-prefixed. The
/// caller reserves exactly this much in one shot. Every field the args
/// serializer writes has to be counted here. One byte short and the `Vec`
/// doubles, and on this heap that means the fill runs out of memory rather
/// than slowing down.
pub const fn quoter_cpi_data_len(users: usize, taker: bool) -> usize {
    8 + 1
        + 8
        + quoter_user_set_bytes(users)
        + QUOTER_USER_CAPS_BYTES
        + 8
        + 1
        + if taker { CLOB_USER_REF_BYTES } else { 0 }
        + 2
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
/// wire's capacity.
///
/// Velocity never holds the set by value. A full set is
/// [`MAX_QUOTER_WIRE_USERS`] entries of [`CLOB_USER_REF_BYTES`] bytes. One of
/// those in an SBF frame overflows the 4 KB limit on the fill and cross-match
/// entrypoints. The linker reports "overflows the maximum allowed frame space"
/// at build time. At runtime it is an access violation several frames deep.
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

/// The request half of the quoter wire, declared by `quoter-spec`. That crate
/// declares the responses too, so the bytes velocity writes and the bytes a
/// quoter reads come from one declaration.
///
/// Both carry the user set as a borrowed slice. A full set is
/// [`MAX_QUOTER_WIRE_USERS`] entries of [`CLOB_USER_REF_BYTES`] bytes, and an
/// SBF stack frame is 4 KB. Owning one put a copy in this frame per quoter and
/// another inside the CPI leg, which overflowed the fill path at runtime while
/// every host-side test passed. The quoter reads it in place too, straight out
/// of its instruction data.
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
    /// levels themselves are not owned here. A route quotes up to
    /// [`super::MAX_ROUTE_QUOTERS`] books, and one pool for all of them is one
    /// allocation rather than one per book.
    pub levels: core::ops::Range<usize>,
    /// `price == 0` when the quoter reached everything it was asked for.
    pub withheld: PriceLevel,
}

/// One order a CLOB removed as a sub-min remainder of a fill.
///
/// `base_asset_amount` releases a maker's margin reservation, and the
/// completed-order ids beside it decrement their open-order counts. That maker
/// is an ordinary velocity user resting on the book, not the quoter's own
/// account. The subject rule therefore bounds whose orders an entry may
/// remove. It does not bound whether the entry described what it removed.
///
/// The bound on the description is the reservation itself. Velocity wrote
/// `open_bids` and `open_asks` when the order was placed, under its owner's
/// signature, and holds every removal report to
/// [`PerpPosition::reserved_open_base`]. A report above what the user has
/// resting fails rather than freeing the margin behind orders that still rest.
/// The same bound covers `evict_worst_v0` and `remove_expired_v0`, which drive
/// identical unwinding, and the fill path's balance changes.
///
/// The one lenient path is a removal the owner signed for, meaning velocity's
/// `cancel_order_v1` and its sweeps. Those run against books that may be dead
/// or de-listed, so an over-report clamps and logs instead of failing. A maker
/// must always be able to leave a book that reports garbage.
///
/// The reservation does not bound which of a user's orders a report names. An
/// id for an order still live on the book frees a placed trigger's shadow
/// while the book keeps the size. The size still has to be one the user really
/// placed.
pub use quoter_spec::CancelledRemainderV0;
pub use quoter_spec::CompletedOrderV0;
/// The execute response, read in place out of the quoter's account.
///
/// Borrows rather than owns. Velocity's heap is 32 KB and never reclaims, and
/// one fill CPIs every registered quoter, so copying the records out would
/// spend heap per quoter per fill that nothing gives back. The caller holds
/// the account guard for as long as it reads. See [`ResponseLocationV0`].
pub use quoter_spec::ExecuteResponseV0;
/// Returned via return data by `quote_v0` and `execute_v0`. It says where in
/// the quoter's `response_account` the borsh response was written. Declared by
/// `quoter-spec`, which owns every shape on this wire.
pub use quoter_spec::ResponsePointerV0;

/// Who a quoter's `execute_v0` response is allowed to move balances for.
pub enum QuoterSubjects {
    /// A Custom entry fills against exactly one margin account, the entry's
    /// `user`. That user's authority created the entry, so registration is
    /// that user's consent. Nothing else the entry names is settleable.
    /// Velocity knows that from the registry rather than from anything the
    /// quoter says.
    Account(Pubkey),
    /// A book fills against whoever rests on it, and velocity takes its word
    /// for who that is.
    ///
    /// Velocity does not check the response against the book's arena. The
    /// arena is the book program's own state, so a book that wanted to name a
    /// stranger would write the stranger into a node first. Such a check runs
    /// the program against itself. It buys no safety and costs a walk of the
    /// swept prefix on every leg.
    ///
    /// What bounds a book instead is the record velocity wrote itself. Every
    /// CLOB order reserved `open_bids` or `open_asks` on its owner's position
    /// at placement, under that owner's signature, and no external program can
    /// write those. So a response is held to
    /// [`crate::state::user::PerpPosition::reserved_open_base`]. A book may
    /// only fill or remove size a user really placed, on the side they placed
    /// it. That bound needs nothing from the book's own state, which is why it
    /// stands where the arena walk did not. It covers the removal reports on
    /// the same terms. See [`super::ClobRemovedOrderV0`].
    ///
    /// Three further limits hold. The response may only name users the
    /// transaction already carries. Every balance change is held to the quoted
    /// prices and to the oracle band. Every user it touches is margin-checked
    /// after the fill.
    ///
    /// Two things the reservation does not bound. It records size and side but
    /// not price, so an order a user did place can still be filled anywhere
    /// inside the oracle band. The exposure is the band's width across the size
    /// they posted. `open_bids` also does not separate a book's reservation
    /// from the DLOB's on the same market and side, so a book's report can
    /// consume reservation a DLOB order made. Splitting them needs sixteen more
    /// bytes on [`crate::state::user::PerpPosition`], which has no spare bytes.
    Book,
}

impl QuoterSubjects {
    /// Whether this quoter may move `user`'s balances. `key` is `user`
    /// resolved against the loaded set, which is how the Custom case compares
    /// against the entry's `user` field.
    ///
    /// The `taker` is never a subject, whatever the entry type. A quoter that
    /// could name the taker would net the taker's position against itself at a
    /// price of its choosing. The wire asks every quoter to skip the taker.
    /// This is the rule rather than the request, and it lives here so no caller
    /// can apply the type check without it.
    ///
    /// `protocol_authority` is `State::signer`. The protocol `User` is never a
    /// subject either. It is the inventory-free taker the cranks fill through,
    /// so a quoter that could name it would move a position onto protocol funds
    /// at a price of its own choosing. It is excluded by identity rather than by
    /// never being loaded, because the cross cranks load it on purpose.
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

/// Find one of the fill's tail accounts by key.
///
/// A scan rather than a map. The tail is a couple of dozen accounts, and one
/// fill looks up a handful per quoter CPI, so building an index costs more than
/// searching. A `BTreeMap` also pays that cost in allocations on a 32 KB heap
/// that never reclaims.
pub fn find_account<'a, 'info>(
    accounts: &'a [AccountInfo<'info>],
    key: &Pubkey,
) -> Option<&'a AccountInfo<'info>> {
    accounts.iter().find(|info| info.key == key)
}

/// Where a quoter left its response, handed back so the caller creates the
/// account borrow.
///
/// The response is read in place out of the quoter's account, and the borrow
/// guard cannot outlive the function that takes it. So the executor validates
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

/// The part of a quoted ladder a reader can use.
///
/// The router's cursor and its level validation both stop at
/// [`crate::math::router::MAX_LEVELS_PER_BOOK`], so a reader that took a deeper
/// ladder would never look at the tail. A market may set `max_quote_levels`
/// high enough that the discarded tail is kilobytes per book.
pub fn usable_levels(levels: &[PriceLevel]) -> &[PriceLevel] {
    &levels[..levels.len().min(crate::math::router::MAX_LEVELS_PER_BOOK)]
}

/// The quoter legs of a router fill, supplied by the fill entrypoint.
///
/// The controller works over account maps and cannot CPI itself. The entrypoint
/// holds the `AccountInfo`s, so the entrypoint supplies this. Every `index`
/// addresses the same entry the caller quoted into
/// [`crate::math::router::RouterLeg::books`].
pub trait ExternalQuoterExecutor<'info> {
    /// Registry type of quoter `index`. It decides whether the quoter's fills
    /// carry velocity-side resting-order aggregates to unwind. A CLOB order is
    /// margin-reserved at placement. Custom PropAMM depth is not.
    fn quoter_type(&self, index: usize) -> QuoterType;

    /// The `User` quoter `index` quotes for, which is `QuoterV0::user`. The
    /// pre-execute clamp sizes Custom books against that margin account.
    fn quoter_user(&self, index: usize) -> Pubkey;

    /// The registry entry of quoter `index`.
    ///
    /// Failure messages name this rather than the index. An off-chain router
    /// reading the logs of a failed fill knows the program from the runtime's
    /// CPI brackets, but one quoter program serves many entries, so only the
    /// entry key says which maker to hold responsible.
    fn quoter_key(&self, index: usize) -> Pubkey;

    /// How far from oracle a fill on quoter `index` may price, in
    /// MARGIN_PRECISION units. See [`QuoterConfigV0::oracle_band`].
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
    /// Only book-backed entries can honour this. A `Custom` quoter has no
    /// orders and answers `None`. The caller uses it to clear a maker the fill
    /// has proven may not rest risk-increasing orders, once the fill it would
    /// have broken has settled.
    fn cancel_all(
        &mut self,
        _index: usize,
        _user: ClobUserRefV0,
        _sides: ClobCancelSides,
    ) -> crate::error::VelocityResult<Option<ClobCancelAllOutcomeV0>> {
        Ok(None)
    }

    /// CPI `execute_v0` on quoter `index` with the routed allocation.
    ///
    /// The response is untrusted. Before it settles anything, the router pass
    /// validates overfill, checks the executed price against the quoted prefix,
    /// and checks that every balance change lands on a permitted subject.
    fn execute(
        &mut self,
        index: usize,
        direction: Direction,
        size: u64,
    ) -> crate::error::VelocityResult<ResponseLocationV0<'info>>;
}

/// Executor for a router fill that carries no external quoter accounts.
///
/// Quoting produced no external books, so no external allocation is reachable.
/// Executing one is an error rather than a skip.
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
    /// Shared gate on both CPI legs. The entry takes new flow, and it takes
    /// that flow for the market the caller is filling.
    ///
    /// An entry is registered per `(market, program, user)`, and nothing about
    /// the CPI itself carries the market. Without the second check, an entry
    /// vetted for one perp market could be quoted into another. It would then
    /// settle its balance changes against positions it was never approved to
    /// touch.
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

    /// CPI `quote_v0` and leave the ladder where the quoter wrote it.
    ///
    /// A reader that consumes the levels before the next CPI pays no heap for
    /// the book.
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
    /// Carries the quote leg's accounts. The two legs read the same state, and
    /// a second registered list would be a second thing an admin has to vet
    /// and keep in step.
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

    /// CPI `execute_v0` on the quoter program. It commits a fill and returns
    /// the balance changes velocity must apply.
    ///
    /// The quoter is untrusted. The caller must validate the returned changes
    /// against the quoted levels and against margin before it applies them.
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

    /// Shared CPI leg. It forwards the registered accounts, sends the
    /// discriminator followed by the borsh args, and locates the borsh response
    /// in the quoter's response account at the pointer returned via return
    /// data.
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

        // `QUOTER_CPI_DATA_MAX` counts every byte the args serializer writes,
        // so a leg that does not fit means that constant is wrong. Fail here
        // rather than let the `Vec` double and leak the buffer it grew out of.
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

        // Signed as the market's slab, the one identity every quoter on the
        // market authenticates velocity by. See `crate::signer` for why the
        // shared key is safe to hand a quoter. The seeds derive from the
        // config's own market, so a slab for a different market fails the
        // runtime's signer check instead of signing.
        let market = self.market.to_le_bytes();
        let seeds = get_quoter_slab_signer_seeds(&market, &slab_bump);
        invoke_quoter_signed(instruction, infos, &[&seeds])?;

        // The payload lives in the quoter's response account, and return data
        // carries only a pointer into it. A response is therefore not bound by
        // the 1024-byte return-data cap. Return data is last-writer-wins within
        // the transaction. Requiring the writer to be `program_id` stops a read
        // of a pointer set by a program the quoter called.
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
