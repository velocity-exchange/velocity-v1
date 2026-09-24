//! The generic quoter CPI: the one interface every source answers on.
//!
//! [`QuoterSlotV0::quote_in_place`] and [`QuoterSlotV0::execute`] CPI a
//! registered quoter program with the request shapes `quoter-spec` declares.
//! Both read the response in place out of the quoter's response account
//! ([`ResponseLocationV0`]). [`ExternalQuoterExecutor`] is the trait the
//! router fill drives those legs through.

// The wire's shapes. `quoter-spec` declares them, and every quoter builds against that crate.
pub use quoter_spec::{
    user_set_bytes, CancelledRemainderV0, CompletedOrderV0, DirectionV0, ExecuteArgsV0,
    ExecuteResponseV0, L3ArgsV0, L3ResponseV0, L3RowV0, PriceLevelV0, QuoteArgsV0, QuoteResponseV0,
    ResponsePointerV0, SideV0, UserBalanceChangeV0, UserCapV0, UserCapsV0, UserRefV0,
    L3_ROW_FLAG_BLOCKS_WALK, L3_ROW_FLAG_REDUCE_ONLY, L3_ROW_FLAG_TAKER_ORIGIN, USER_CAPS_BYTES,
    USER_CAPS_CAPACITY, USER_SET_CAPACITY,
};
use {
    super::{get_quoter_slab_signer_seeds, AmmAccountMeta, QuoterSlabV0, QuoterSlotV0, QuoterType},
    crate::{
        controller::position::PositionDirection,
        error::{ErrorCode, VelocityResult},
        math::router::{validate_quoted_levels, MAX_LEVELS_PER_BOOK},
        msg,
        state::user::is_protocol_user_seeds,
        validate,
    },
    anchor_lang::prelude::*,
    quoter_spec::{wincode::SchemaWrite, ArgsConfig},
    solana_program::{
        instruction::{AccountMeta, Instruction},
        program::{get_return_data, invoke_signed},
    },
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

/// Mirrors `StableVec`, which the runtime reads as an address, a capacity it
/// ignores, and a length. Not `StableVec` itself, which owns its allocation and
/// would hand the allocator a pointer this program still owns.
#[allow(dead_code)]
#[repr(C)]
struct BorrowedVec {
    addr: u64,
    cap: u64,
    len: u64,
}

/// Mirrors `StableInstruction`. The asserts below pin the size, the alignment
/// and every field offset of both mirrors on every target. The host build
/// CPIs through [`invoke_signed`], so it only checks the mirrors.
#[allow(dead_code)]
#[repr(C)]
struct BorrowedInstruction {
    accounts: BorrowedVec,
    data: BorrowedVec,
    program_id: Pubkey,
}

const _: () = {
    use {
        core::mem::{align_of, offset_of, size_of},
        solana_program::stable_layout::{
            stable_instruction::StableInstruction, stable_vec::StableVec,
        },
    };

    assert!(size_of::<BorrowedVec>() == size_of::<StableVec<u8>>());
    assert!(align_of::<BorrowedVec>() == align_of::<StableVec<u8>>());
    assert!(offset_of!(BorrowedVec, addr) == offset_of!(StableVec<u8>, addr));
    assert!(offset_of!(BorrowedVec, cap) == offset_of!(StableVec<u8>, cap));
    assert!(offset_of!(BorrowedVec, len) == offset_of!(StableVec<u8>, len));

    assert!(size_of::<BorrowedInstruction>() == size_of::<StableInstruction>());
    assert!(align_of::<BorrowedInstruction>() == align_of::<StableInstruction>());
    assert!(offset_of!(BorrowedInstruction, accounts) == offset_of!(StableInstruction, accounts));
    assert!(offset_of!(BorrowedInstruction, data) == offset_of!(StableInstruction, data));
    assert!(
        offset_of!(BorrowedInstruction, program_id) == offset_of!(StableInstruction, program_id)
    );
};

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

        // SAFETY: `borrowed` has `StableInstruction`'s layout, which the asserts on
        // `BorrowedInstruction` pin. The metas and args it addresses are the caller's
        // and outlive this call. Every meta came from a real `AccountMeta`, so its
        // flag bytes are 0 or 1.
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
            _ => Err(ProgramError::from(result).into()),
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

impl From<DirectionV0> for PositionDirection {
    fn from(direction: DirectionV0) -> Self {
        match direction {
            DirectionV0::Long => PositionDirection::Long,
            DirectionV0::Short => PositionDirection::Short,
        }
    }
}

/// The maker position direction a resting order on this side represents.
impl From<SideV0> for PositionDirection {
    fn from(side: SideV0) -> Self {
        match side {
            SideV0::Bid => PositionDirection::Long,
            SideV0::Ask => PositionDirection::Short,
        }
    }
}

/// The side a resting order of this maker position direction rests on.
impl From<PositionDirection> for SideV0 {
    fn from(direction: PositionDirection) -> Self {
        match direction {
            PositionDirection::Long => SideV0::Bid,
            PositionDirection::Short => SideV0::Ask,
        }
    }
}

/// Bytes an execute CPI's instruction data takes: the discriminator, the user set, the
/// direction, the size, the caps, the reference price and the taker behind their option
/// tags, and two flags. It counts a reference price, which every execute carries. One
/// byte short and the `Vec` doubles onto a heap that never reclaims.
pub const fn quoter_cpi_data_len(users: usize, taker: bool) -> usize {
    8 + 1
        + 8
        + user_set_bytes(users)
        + USER_CAPS_BYTES
        + 1
        + 8
        + 1
        + if taker { UserRefV0::SIZE } else { 0 }
        + 2
}

/// The same for a quote, which also carries the caller's worst acceptable
/// price. Execute is handed a size cut off the ladder rather than a bound, so
/// the two legs are eight bytes apart.
pub const fn quote_cpi_data_len(users: usize, taker: bool) -> usize {
    quoter_cpi_data_len(users, taker) + 8
}

/// Widest either leg can be: a quote with a full user set and a taker.
pub const QUOTER_CPI_DATA_MAX: usize = quote_cpi_data_len(USER_SET_CAPACITY, true);

/// The loaded-user set as velocity holds it: a heap slice, capped at the wire's
/// capacity. Never held by value. A full set of [`USER_SET_CAPACITY`] entries
/// overflows the 4 KB SBF frame on the fill and cross-match entrypoints, which the
/// linker reports as "overflows the maximum allowed frame space".
pub fn quoter_wire_users(
    refs: impl IntoIterator<Item = UserRefV0>,
) -> VelocityResult<Vec<UserRefV0>> {
    let users: Vec<UserRefV0> = refs.into_iter().collect();
    validate!(
        users.len() <= USER_SET_CAPACITY,
        ErrorCode::TooManyQuoterWireUsers,
        "{} loaded users to forward to a quoter exceeds the wire's {}",
        users.len(),
        USER_SET_CAPACITY
    )?;

    Ok(users)
}

/// Write `quote_l3_v0` args in the framing the wire declares, for an
/// off-chain caller that asks a book directly rather than through the router.
pub fn write_l3_args(dst: &mut Vec<u8>, args: &L3ArgsV0) -> VelocityResult<()> {
    quoter_spec::write_args(dst, args).map_err(|_| {
        msg!("could not serialize l3 args");
        ErrorCode::PropAmmArgsEncodeFailed
    })?;

    Ok(())
}

/// What one quoter answered: the ladder it stands behind, and the depth it
/// says it holds at a better price but cannot reach in this transaction.
pub struct QuotedLadderV0 {
    /// Where this ladder's levels landed in the pool the caller passed. The
    /// levels themselves are not owned here. A route quotes up to
    /// [`super::MAX_ROUTE_QUOTERS`] books, and one pool for all of them is one
    /// allocation rather than one per book.
    pub levels: core::ops::Range<usize>,
    /// `price == 0` when the quoter reached everything it was asked for.
    pub withheld: PriceLevelV0,
}

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
        user: &UserRefV0,
        key: &Pubkey,
        taker: &UserRefV0,
        protocol_authority: &Pubkey,
    ) -> bool {
        if user == taker {
            return false;
        }

        if is_protocol_user_seeds(&user.authority, user.sub_account_id, protocol_authority) {
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
    account: AccountInfo<'info>,
    start: usize,
    end: usize,
}

impl<'info> ResponseLocationV0<'info> {
    /// The response `pointer` names in `account`. The quoter reports the pointer, so
    /// a range past the end of the account is an error here rather than at the read.
    pub fn new(account: AccountInfo<'info>, pointer: &ResponsePointerV0) -> VelocityResult<Self> {
        let start = pointer.offset as usize;
        let end = start
            .checked_add(pointer.len as usize)
            .ok_or(ErrorCode::MathError)?;
        let account_len = account
            .try_borrow_data()
            .map_err(|_| ErrorCode::PropAmmResponseAccountBorrowConflict)?
            .len();
        validate!(
            end <= account_len,
            ErrorCode::InvalidQuoterResponse,
            "prop amm response pointer out of bounds"
        )?;

        Ok(Self {
            account,
            start,
            end,
        })
    }

    /// Borrow the response account. The guard lives in the caller's scope,
    /// which is what makes the borrowed view below sound.
    pub fn borrow(&self) -> VelocityResult<core::cell::Ref<'_, &'_ mut [u8]>> {
        self.account.try_borrow_data().map_err(|_| {
            msg!("prop amm response account is already borrowed");
            ErrorCode::PropAmmResponseAccountBorrowConflict
        })
    }

    /// The response bytes the quoter wrote, checked against the account length.
    /// The quoter reports the pointer, so a length past the end is an error
    /// rather than a panic.
    fn bytes<'a>(&self, data: &'a [u8]) -> VelocityResult<&'a [u8]> {
        data.get(self.start..self.end).ok_or_else(|| {
            msg!("prop amm response pointer out of bounds");
            ErrorCode::InvalidQuoterResponse
        })
    }

    /// Read the execute response in place out of a guard taken by
    /// [`Self::borrow`].
    pub fn execute_response<'a>(&self, data: &'a [u8]) -> VelocityResult<ExecuteResponseV0<'a>> {
        ExecuteResponseV0::parse(self.bytes(data)?).map_err(|_| {
            msg!("prop amm quoter returned an undecodable execute response");
            ErrorCode::InvalidQuoterResponse
        })
    }

    /// Read the quote response in place.
    pub fn quote_response<'a>(&self, data: &'a [u8]) -> VelocityResult<QuoteResponseV0<'a>> {
        QuoteResponseV0::parse(self.bytes(data)?).map_err(|_| {
            msg!("prop amm quoter returned an undecodable quote response");
            ErrorCode::InvalidQuoterResponse
        })
    }

    /// The rows behind a ladder, read in place out of a guard taken by
    /// [`Self::borrow`].
    pub fn l3_response<'a>(&self, data: &'a [u8]) -> VelocityResult<L3ResponseV0<'a>> {
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
        direction: DirectionV0,
    ) -> VelocityResult<QuoteResponseV0<'a>> {
        let response = self.quote_response(data)?;
        validate_quoted_levels(direction, response.levels)?;
        Ok(response)
    }
}

/// The part of a quoted ladder a reader can use. The router's cursor and its level
/// validation both stop at [`MAX_LEVELS_PER_BOOK`], and a market
/// may set `max_quote_levels` high enough that the discarded tail is kilobytes.
pub fn usable_levels(levels: &[PriceLevelV0]) -> &[PriceLevelV0] {
    &levels[..levels.len().min(MAX_LEVELS_PER_BOOK)]
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
    /// margin-reserved at placement. Custom PropAMM depth is not. `Custom` for an
    /// index with no entry.
    fn quoter_type(&self, index: usize) -> QuoterType;

    /// The `User` quoter `index` quotes for, which is `QuoterV0::user`. The
    /// pre-execute clamp sizes Custom books against that margin account.
    /// `Pubkey::default()` for an index with no entry, which the idle-maker count relies on.
    fn quoter_user(&self, index: usize) -> Pubkey;

    /// The registry entry of quoter `index`. Failure messages name this rather than the
    /// index, because one quoter program serves many entries and only the entry key says
    /// which maker to hold responsible. `Pubkey::default()` for an index with no entry,
    /// which the account-lock count relies on.
    fn quoter_key(&self, index: usize) -> Pubkey;

    /// How far from oracle a fill on quoter `index` may price, in MARGIN_PRECISION
    /// units. Defaults to the market's own band, which is what an executor carrying no
    /// registry entry can say. See [`super::QuoterConfigV0::oracle_band`].
    fn oracle_band(&self, _index: usize, market_margin_ratio_initial: u32) -> u32 {
        market_margin_ratio_initial
    }

    /// The users quoter `index` may return balance changes for.
    fn subjects(
        &self,
        index: usize,
        direction: DirectionV0,
        size: u64,
    ) -> VelocityResult<QuoterSubjects>;

    /// CPI `execute_v0` on quoter `index` with the routed allocation. The response is
    /// untrusted. The router pass validates overfill, checks the executed price against
    /// the quoted prefix, and checks every balance change lands on a permitted
    /// subject.
    fn execute(
        &mut self,
        index: usize,
        direction: DirectionV0,
        size: u64,
    ) -> VelocityResult<ResponseLocationV0<'info>>;
}

/// Executor for a router fill that carries no external quoter accounts.
///
/// It holds no entry at any index, so each read gives the no-entry answer the trait
/// declares. Quoting produced no external books, so a leg is never reachable, and
/// `subjects` and `execute` are errors rather than a skip.
#[cfg(test)]
pub struct NoExternalQuoters;

#[cfg(test)]
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
        _direction: DirectionV0,
        _size: u64,
    ) -> VelocityResult<QuoterSubjects> {
        msg!("router fill has no external quoter to name subjects for");
        Err(ErrorCode::ImpossibleFill)
    }

    fn execute(
        &mut self,
        _index: usize,
        _direction: DirectionV0,
        _size: u64,
    ) -> VelocityResult<ResponseLocationV0<'info>> {
        msg!("router fill has no external quoter accounts to execute against");
        Err(ErrorCode::ImpossibleFill)
    }
}

/// The CPI legs live on the slab slot. Approval is membership in the market's
/// slab, so a staging entry's config has no way to reach a quoter.
impl QuoterSlotV0 {
    /// Shared gate on both CPI legs. The entry takes new flow, and takes it for the
    /// market the caller is filling. Nothing about the CPI itself carries the market, so
    /// without the second check an entry vetted for one perp market could settle balance
    /// changes against positions it was never approved to touch.
    fn gate_for_market(&self, market_index: u16) -> Result<()> {
        validate!(
            self.config.is_active,
            ErrorCode::InvalidQuoterConfig,
            "quoter is not active"
        )?;
        validate!(
            self.config.market == market_index,
            ErrorCode::InvalidQuoterConfig,
            "quoter entry is for market {}, call is for market {}",
            self.config.market,
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
            QuoterLeg {
                discriminator: &self.config.quote_v0_discriminator,
                account_indexes: self.config.quote_leg_indexes(),
            },
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
    /// caller attributes the ladder to [`super::QuoterConfigV0::user`] in that case, so a
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
        if self.config.quote_l3_v0_discriminator == [0u8; 8] {
            return Ok(None);
        }

        self.gate_for_market(market_index)?;
        self.invoke_quoter(
            QuoterLeg {
                discriminator: &self.config.quote_l3_v0_discriminator,
                account_indexes: self.config.quote_leg_indexes(),
            },
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
            QuoterLeg {
                discriminator: &self.config.execute_v0_discriminator,
                account_indexes: self.config.execute_leg_indexes(),
            },
            &args,
            slab,
            accounts,
            scratch,
        )
    }

    /// Shared CPI leg: build the instruction, CPI it signed as the slab, and
    /// locate the response the quoter wrote.
    fn invoke_quoter<'info, A: SchemaWrite<ArgsConfig, Src = A>>(
        &self,
        leg: QuoterLeg<'_>,
        args: &A,
        slab: &AccountLoader<'info, QuoterSlabV0>,
        accounts: &[AccountInfo<'info>],
        scratch: &mut QuoterCpiScratch<'info>,
    ) -> Result<ResponseLocationV0<'info>> {
        // A read borrow, so a caller may hold the slot region while this runs.
        let slab_bump = slab.load()?.bump;
        self.write_leg_accounts(leg.account_indexes, slab.as_ref(), accounts, scratch)?;
        write_leg_data(leg.discriminator, args, &mut scratch.instruction.data)?;

        // Signed as the market's slab, the identity every quoter authenticates velocity
        // by. The seeds derive from the config's own market, so a slab for a different
        // market fails the runtime's signer check instead of signing.
        let market = self.config.market.to_le_bytes();
        let seeds = get_quoter_slab_signer_seeds(&market, &slab_bump);
        invoke_quoter_signed(&scratch.instruction, &scratch.infos, &[&seeds])?;

        self.locate_response(accounts)
    }

    /// Write the leg's program id, account metas and account infos into `scratch`.
    fn write_leg_accounts<'info>(
        &self,
        account_indexes: &[u8],
        slab_info: &AccountInfo<'info>,
        accounts: &[AccountInfo<'info>],
        scratch: &mut QuoterCpiScratch<'info>,
    ) -> Result<()> {
        let QuoterCpiScratch { instruction, infos } = scratch;
        instruction.program_id = self.config.program_id;
        write_quoter_account_metas(
            &mut instruction.accounts,
            self.config.leg_metas(account_indexes)?,
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
        let program_info = find_account(accounts, &self.config.program_id).ok_or_else(|| {
            msg!("quoter program account missing from account map");
            ErrorCode::QuoterCpiAccountMissing
        })?;

        infos.push(program_info.clone());
        Ok(())
    }

    /// Where the quoter wrote its response, read from the pointer it returned.
    ///
    /// The payload lives in the quoter's response account and return data carries
    /// only a pointer, so a response is not bound by the 1024-byte cap. Return data is
    /// last-writer-wins, so requiring the writer to be `program_id` stops a read of a
    /// pointer set by a program the quoter called.
    fn locate_response<'info>(
        &self,
        accounts: &[AccountInfo<'info>],
    ) -> Result<ResponseLocationV0<'info>> {
        let (writer, pointer_data) = get_return_data().ok_or_else(|| {
            msg!("prop amm quoter set no return data");
            ErrorCode::InvalidQuoterResponse
        })?;

        validate!(
            writer == self.config.program_id,
            ErrorCode::InvalidQuoterResponse,
            "prop amm return data written by {} instead of quoter program",
            writer
        )?;

        let pointer =
            ResponsePointerV0::deserialize(&mut pointer_data.as_slice()).map_err(|_| {
                msg!("prop amm quoter returned undecodable response pointer");
                ErrorCode::InvalidQuoterResponse
            })?;

        let response_info =
            find_account(accounts, &self.config.response_account).ok_or_else(|| {
                msg!("prop amm response account missing from account map");
                ErrorCode::QuoterCpiAccountMissing
            })?;

        // Only the quoter program can have written an account it owns.
        validate!(
            *response_info.owner == self.config.program_id,
            ErrorCode::InvalidQuoterResponse,
            "prop amm response account not owned by quoter program"
        )?;

        Ok(ResponseLocationV0::new(response_info.clone(), &pointer)?)
    }
}

/// One CPI leg's surface on the quoter program: the instruction it calls and
/// the registered accounts it forwards.
struct QuoterLeg<'a> {
    discriminator: &'a [u8; 8],
    account_indexes: &'a [u8],
}

/// Write the discriminator and then the wincode args into the reused `data` buffer.
///
/// `QUOTER_CPI_DATA_MAX` counts every byte the args serializer writes, so a leg that
/// does not fit means that constant is wrong. Fail here rather than let the `Vec`
/// double and leak the buffer it grew out of.
fn write_leg_data<A: SchemaWrite<ArgsConfig, Src = A>>(
    discriminator: &[u8; 8],
    args: &A,
    data: &mut Vec<u8>,
) -> Result<()> {
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

    data.clear();
    data.extend_from_slice(discriminator);
    quoter_spec::write_args(data, args).map_err(|_| {
        msg!("prop amm failed to serialize cpi args");
        ErrorCode::PropAmmArgsEncodeFailed
    })?;

    Ok(())
}
