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
/// Never forward outer signer privilege. It propagates through CPI, so a quoter handed
/// the taker's wallet could CPI to the token program and drain it. A quoter that must
/// know who signed reads the instructions sysvar. The single signer is the market's
/// quoter slab, which is how a quoter authenticates velocity as the caller.
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
        /// Mirrors `StableVec`, which the runtime reads as an address, a capacity it
        /// ignores, and a length. Not `StableVec` itself, which owns its allocation and
        /// would hand the allocator a pointer this program still owns.
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

        // SAFETY: `borrowed` has `StableInstruction`'s layout, asserted above. The
        // metas and args it addresses are the caller's and outlive this call. Every
        // meta came from a real `AccountMeta`, so its flag bytes are 0 or 1.
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

/// The three buffers a quoter CPI leg needs, allocated once and reused by every leg.
/// Velocity's heap is 32 KB and never reclaims, so a `Vec` per leg is a `Vec` for the
/// rest of the instruction. `clear` keeps the allocation, and the buffers size themselves
/// from the first leg.
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

/// A velocity user on the quoter wire, in its derivable form: an authority wallet and a
/// sub-account index. Both the `User` and `UserStats` PDAs derive from those, so an
/// off-chain reader reaches every user-derived account from a quoter's state alone.
/// Declared by `quoter-spec`, and this alias keeps velocity's name for it.
pub type ClobUserRefV0 = quoter_spec::UserRefV0;

/// Borsh width of a [`ClobUserRefV0`].
pub const CLOB_USER_REF_BYTES: usize = quoter_spec::UserRefV0::SIZE;
const_assert_eq!(std::mem::size_of::<ClobUserRefV0>(), CLOB_USER_REF_BYTES);

/// The loaded-user set on the quoter wire: a length prefix and that many entries. An
/// empty set means unrestricted, which only callers that settle nothing send. A quoter
/// must skip liquidity whose owner is absent from a non-empty set, because velocity
/// cannot settle a balance change for a `User` it did not load.
pub use quoter_spec::{
    user_set_bytes as quoter_user_set_bytes, UserCapV0 as QuoterUserCapV0,
    UserCapsV0 as QuoterUserCapsV0, USER_CAPS_BYTES as QUOTER_USER_CAPS_BYTES,
    USER_CAPS_CAPACITY as MAX_CONSTRAINED_WIRE_USERS, USER_EXCLUSION_BITMAP_BYTES,
    USER_SET_CAPACITY as MAX_QUOTER_WIRE_USERS, USER_SET_MAX_BYTES as QUOTER_USER_SET_MAX_BYTES,
};

/// Bytes an execute CPI's instruction data takes: the discriminator, the user set, the
/// direction, the size, the caps, the reference price, the taker behind its option tag,
/// then `taker_served_window` and `include_taker_origin_reservations`. A size rather
/// than a constant, because the user set is length-prefixed. One byte short and the
/// `Vec` doubles onto a heap that never reclaims.
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

/// The loaded-user set as velocity holds it: a heap slice, capped at the wire's
/// capacity. Never held by value. A full set of [`MAX_QUOTER_WIRE_USERS`] entries
/// overflows the 4 KB SBF frame on the fill and cross-match entrypoints, which the
/// linker reports as "overflows the maximum allowed frame space".
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
        ErrorCode::PropAmmArgsEncodeFailed
    })?;

    Ok(())
}

/// The request half of the quoter wire, declared by `quoter-spec` beside the responses.
/// The user set is a borrowed slice. Owning one put a copy in this frame per quoter and
/// another in the CPI leg, which overflowed the 4 KB SBF frame at runtime while every
/// host-side test passed.
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

/// One order a CLOB removed as a sub-min remainder of a fill. Velocity holds every
/// removal report to [`PerpPosition::reserved_open_base`], so a book may only free size
/// a user really placed. A removal the owner signed for clamps and logs instead of
/// failing, because a maker must be able to leave a book that reports garbage.
pub use quoter_spec::CancelledRemainderV0;
pub use quoter_spec::CompletedOrderV0;
/// The execute response, read in place out of the quoter's account. Borrows rather than
/// owns, because one fill CPIs every registered quoter and copying the records out would
/// spend heap per quoter that nothing gives back. See [`ResponseLocationV0`].
pub use quoter_spec::ExecuteResponseV0;
/// Returned via return data by `quote_v0` and `execute_v0`. It says where in
/// the quoter's `response_account` the borsh response was written. Declared by
/// `quoter-spec`, which owns every shape on this wire.
pub use quoter_spec::ResponsePointerV0;

/// Who a quoter's `execute_v0` response is allowed to move balances for.
pub enum QuoterSubjects {
    /// A Custom entry fills against exactly one margin account, the entry's `user`. That
    /// user's authority created the entry, so registration is their consent. Velocity
    /// knows it from the registry rather than from anything the quoter says.
    Account(Pubkey),
    /// A book fills against whoever rests on it. Velocity does not walk the book's
    /// arena, which is the book's own state. It holds the response to
    /// [`crate::state::user::PerpPosition::reserved_open_base`] instead, so a book may
    /// only fill or remove size a user really placed, on the side they placed it.
    Book,
}

impl QuoterSubjects {
    /// Whether this quoter may move `user`'s balances. `key` is `user` resolved against
    /// the loaded set.
    ///
    /// Neither the taker nor the protocol `User` is ever a subject. A quoter that could
    /// name either would net that position against itself at a price of its choosing.
    /// Both are excluded by identity here rather than by never being loaded, because the
    /// cross cranks load the protocol user on purpose.
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

/// Find one of the fill's tail accounts by key. A scan rather than a map, because the
/// tail is a couple of dozen accounts and building an index costs more than searching.
/// A `BTreeMap` would also allocate on a 32 KB heap that never reclaims.
pub fn find_account<'a, 'info>(
    accounts: &'a [AccountInfo<'info>],
    key: &Pubkey,
) -> Option<&'a AccountInfo<'info>> {
    accounts.iter().find(|info| info.key == key)
}

/// Where a quoter left its response, handed back so the caller creates the account
/// borrow. The guard cannot outlive the function that takes it, so the executor returns
/// a location and the fill holds the guard for as long as it reads. Returning the
/// records would copy them onto a 32 KB heap that never reclaims.
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
            ErrorCode::PropAmmResponseAccountBorrowConflict
        })
    }

    /// The response bytes the quoter wrote, checked against the account length.
    /// The quoter reports the pointer, so a length past the end is an error
    /// rather than a panic.
    fn bytes<'a>(&self, data: &'a [u8]) -> crate::error::VelocityResult<&'a [u8]> {
        data.get(self.start..self.end).ok_or_else(|| {
            msg!("prop amm response pointer out of bounds");
            ErrorCode::InvalidQuoterResponse
        })
    }

    /// Read the execute response in place out of a guard taken by
    /// [`Self::borrow`].
    pub fn execute_response<'a>(
        &self,
        data: &'a [u8],
    ) -> crate::error::VelocityResult<ExecuteResponseV0<'a>> {
        ExecuteResponseV0::parse(self.bytes(data)?).map_err(|_| {
            msg!("prop amm quoter returned an undecodable execute response");
            ErrorCode::InvalidQuoterResponse
        })
    }

    /// Read the quote response in place.
    pub fn quote_response<'a>(
        &self,
        data: &'a [u8],
    ) -> crate::error::VelocityResult<QuoteResponseV0<'a>> {
        QuoteResponseV0::parse(self.bytes(data)?).map_err(|_| {
            msg!("prop amm quoter returned an undecodable quote response");
            ErrorCode::InvalidQuoterResponse
        })
    }

    /// The rows behind a ladder, read in place out of a guard taken by
    /// [`Self::borrow`].
    pub fn l3_response<'a>(
        &self,
        data: &'a [u8],
    ) -> crate::error::VelocityResult<L3ResponseV0<'a>> {
        L3ResponseV0::parse(self.bytes(data)?).map_err(|_| {
            msg!("prop amm quoter returned an undecodable l3 response");
            ErrorCode::InvalidQuoterResponse
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

/// The part of a quoted ladder a reader can use. The router's cursor and its level
/// validation both stop at [`crate::math::router::MAX_LEVELS_PER_BOOK`], and a market
/// may set `max_quote_levels` high enough that the discarded tail is kilobytes.
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

    /// The registry entry of quoter `index`. Failure messages name this rather than the
    /// index, because one quoter program serves many entries and only the entry key says
    /// which maker to hold responsible.
    fn quoter_key(&self, index: usize) -> Pubkey;

    /// How far from oracle a fill on quoter `index` may price, in MARGIN_PRECISION
    /// units. Defaults to the market's own band, which is what an executor carrying no
    /// registry entry can say. See [`QuoterConfigV0::oracle_band`].
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

    /// CPI a whole-side cancel on quoter `index` for one of its makers. Only book-backed
    /// entries honour it, and a `Custom` quoter answers `None`. The caller clears a maker
    /// the fill proved may not rest risk-increasing orders.
    fn cancel_all(
        &mut self,
        _index: usize,
        _user: ClobUserRefV0,
        _sides: ClobCancelSides,
    ) -> crate::error::VelocityResult<Option<ClobCancelAllOutcomeV0>> {
        Ok(None)
    }

    /// CPI `execute_v0` on quoter `index` with the routed allocation. The response is
    /// untrusted. The router pass validates overfill, checks the executed price against
    /// the quoted prefix, and checks every balance change lands on a permitted
    /// subject.
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
        Err(ErrorCode::ImpossibleFill)
    }
}

pub use quoter_spec::UserBalanceChangeV0;

impl QuoterConfigV0 {
    /// Shared gate on both CPI legs. The entry takes new flow, and takes it for the
    /// market the caller is filling. Nothing about the CPI itself carries the market, so
    /// without the second check an entry vetted for one perp market could settle balance
    /// changes against positions it was never approved to touch.
    fn gate_for_market(&self, market_index: u16) -> Result<()> {
        validate!(
            self.is_active,
            ErrorCode::InvalidQuoterConfig,
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
                ErrorCode::QuoterCpiAccountMissing
            })?;

            infos.push(info.clone());
        }

        // CPI needs the callee program's account info too.
        let program_info = find_account(accounts, &self.program_id).ok_or_else(|| {
            msg!("quoter program account missing from account map");
            ErrorCode::QuoterCpiAccountMissing
        })?;

        infos.push(program_info.clone());

        // `QUOTER_CPI_DATA_MAX` counts every byte the args serializer writes,
        // so a leg that does not fit means that constant is wrong. Fail here
        // rather than let the `Vec` double and leak the buffer it grew out of.
        let args_len = quoter_spec::args_size(args).map_err(|_| {
            msg!("prop amm failed to size cpi args");
            ErrorCode::PropAmmArgsEncodeFailed
        })?;
        let data_len = discriminator.len().saturating_add(args_len);
        validate!(
            data_len <= QUOTER_CPI_DATA_MAX,
            ErrorCode::QuoterCpiArgsTooLarge,
            "prop amm cpi args are {} bytes, above the {} the wire allows",
            data_len,
            QUOTER_CPI_DATA_MAX
        )?;

        instruction.data.clear();
        instruction.data.extend_from_slice(discriminator);
        quoter_spec::write_args(&mut instruction.data, args).map_err(|_| {
            msg!("prop amm failed to serialize cpi args");
            ErrorCode::PropAmmArgsEncodeFailed
        })?;

        // Signed as the market's slab, the identity every quoter authenticates velocity
        // by. The seeds derive from the config's own market, so a slab for a different
        // market fails the runtime's signer check instead of signing.
        let market = self.market.to_le_bytes();
        let seeds = get_quoter_slab_signer_seeds(&market, &slab_bump);
        invoke_quoter_signed(instruction, infos, &[&seeds])?;

        // The payload lives in the quoter's response account and return data carries
        // only a pointer, so a response is not bound by the 1024-byte cap. Return data is
        // last-writer-wins, so requiring the writer to be `program_id` stops a read of a
        // pointer set by a program the quoter called.
        let (writer, pointer_data) = get_return_data().ok_or_else(|| {
            msg!("prop amm quoter set no return data");
            ErrorCode::InvalidQuoterResponse
        })?;

        validate!(
            writer == self.program_id,
            ErrorCode::InvalidQuoterResponse,
            "prop amm return data written by {} instead of quoter program",
            writer
        )?;

        let pointer =
            ResponsePointerV0::deserialize(&mut pointer_data.as_slice()).map_err(|_| {
                msg!("prop amm quoter returned undecodable response pointer");
                ErrorCode::InvalidQuoterResponse
            })?;

        let response_info = find_account(accounts, &self.response_account).ok_or_else(|| {
            msg!("prop amm response account missing from account map");
            ErrorCode::QuoterCpiAccountMissing
        })?;

        // Only the quoter program can have written an account it owns.
        validate!(
            *response_info.owner == self.program_id,
            ErrorCode::InvalidQuoterResponse,
            "prop amm response account not owned by quoter program"
        )?;

        let data = response_info
            .try_borrow_data()
            .map_err(|_| ErrorCode::PropAmmResponseAccountBorrowConflict)?;
        let start = pointer.offset as usize;
        let end = start
            .checked_add(pointer.len as usize)
            .ok_or(ErrorCode::MathError)?;
        validate!(
            end <= data.len(),
            ErrorCode::InvalidQuoterResponse,
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
