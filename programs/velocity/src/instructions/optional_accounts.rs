use {
    crate::{
        error::{
            ErrorCode::{self, UnableToLoadOracle},
            VelocityResult,
        },
        math::{safe_unwrap::SafeUnwrap, time::SlotClock},
        msg,
        state::{
            load_ref::load_ref_mut,
            oracle::PrelaunchOracle,
            oracle_map::OracleMap,
            perp_market::PerpMarket,
            perp_market_map::{MarketSet, PerpMarketMap},
            revenue_share::{
                RevenueShareEscrow, RevenueShareEscrowLoader, RevenueShareEscrowZeroCopyMut,
                RevenueShareOrder, RevenueShareOrderBitFlag,
            },
            spot_market_map::SpotMarketMap,
            state::{OracleGuardRails, State},
            traits::Size,
            user::{MarketType, User, UserStats},
        },
        validate, OracleSource,
    },
    anchor_lang::{
        accounts::account::Account,
        prelude::{AccountInfo, AccountLoader, Interface, InterfaceAccount, Pubkey},
        Discriminator,
    },
    anchor_spl::{
        token::TokenAccount,
        token_interface::{Mint, TokenInterface},
    },
    arrayref::array_ref,
    solana_program::account_info::next_account_info,
    std::{cell::RefMut, convert::TryFrom, iter::Peekable, ops::Deref, slice::Iter},
};

pub struct AccountMaps<'a> {
    pub perp_market_map: PerpMarketMap<'a>,
    pub spot_market_map: SpotMarketMap<'a>,
    pub oracle_map: OracleMap<'a>,
}

impl<'a> AccountMaps<'a> {
    /// Builds the bundle from maps the caller already loaded.
    ///
    /// [`load_maps`] reads all three from one account list, which is what an
    /// instruction handler has. A caller that loads them separately, or that
    /// names the perp market as its own account, builds the bundle here.
    pub fn new(
        perp_market_map: PerpMarketMap<'a>,
        spot_market_map: SpotMarketMap<'a>,
        oracle_map: OracleMap<'a>,
    ) -> Self {
        Self {
            perp_market_map,
            spot_market_map,
            oracle_map,
        }
    }
}

pub fn load_maps<'a, 'b>(
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'a>>>,
    writable_perp_markets: &'b MarketSet,
    writable_spot_markets: &'b MarketSet,
    slot: u64,
    slot_clock: SlotClock,
    oracle_guard_rails: Option<OracleGuardRails>,
) -> VelocityResult<AccountMaps<'a>> {
    let oracle_map = OracleMap::load(account_info_iter, slot, slot_clock, oracle_guard_rails)?;
    let spot_market_map = SpotMarketMap::load(writable_spot_markets, account_info_iter)?;
    let perp_market_map = PerpMarketMap::load(writable_perp_markets, account_info_iter)?;

    for perp_market_index in writable_perp_markets.iter() {
        update_prelaunch_oracle(
            perp_market_map.get_ref(perp_market_index)?.deref(),
            &oracle_map,
            slot,
        )?;
    }

    Ok(AccountMaps {
        perp_market_map,
        spot_market_map,
        oracle_map,
    })
}

pub fn update_prelaunch_oracle(
    perp_market: &PerpMarket,
    oracle_map: &OracleMap,
    slot: u64,
) -> VelocityResult {
    if perp_market.oracle_source != OracleSource::Prelaunch {
        return Ok(());
    }

    let oracle_account_info = oracle_map.get_account_info(&perp_market.oracle)?;

    let mut oracle: RefMut<PrelaunchOracle> =
        load_ref_mut(&oracle_account_info).or(Err(UnableToLoadOracle))?;

    oracle.update(perp_market, slot)?;

    Ok(())
}

#[allow(clippy::type_complexity)]
pub fn get_referrer_and_referrer_stats<'a>(
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'a>>>,
) -> VelocityResult<(
    Option<AccountLoader<'a, User>>,
    Option<AccountLoader<'a, UserStats>>,
)> {
    let referrer_account_info = account_info_iter.peek();

    if referrer_account_info.is_none() {
        return Ok((None, None));
    }

    let referrer_account_info = referrer_account_info.safe_unwrap()?;
    let data = referrer_account_info.try_borrow_data().map_err(|e| {
        msg!("{:?}", e);
        ErrorCode::CouldNotDeserializeReferrer
    })?;

    if data.len() < User::SIZE {
        return Ok((None, None));
    }

    let user_discriminator: &[u8] = User::DISCRIMINATOR;
    let account_discriminator = &data[..8];
    if account_discriminator != user_discriminator {
        return Ok((None, None));
    }

    let referrer_account_info = next_account_info(account_info_iter).safe_unwrap()?;

    validate!(
        referrer_account_info.is_writable,
        ErrorCode::ReferrerMustBeWritable
    )?;

    let referrer: AccountLoader<User> = AccountLoader::try_from(referrer_account_info)
        .or(Err(ErrorCode::CouldNotDeserializeReferrer))?;

    let referrer_stats_account_info = account_info_iter.peek();
    if referrer_stats_account_info.is_none() {
        return Ok((None, None));
    }

    let referrer_stats_account_info = referrer_stats_account_info.safe_unwrap()?;
    let data = referrer_stats_account_info.try_borrow_data().map_err(|e| {
        msg!("{:?}", e);
        ErrorCode::CouldNotDeserializeReferrerStats
    })?;

    if data.len() < UserStats::SIZE {
        return Ok((None, None));
    }

    let user_stats_discriminator: &[u8] = UserStats::DISCRIMINATOR;
    let account_discriminator = &data[..8];
    if account_discriminator != user_stats_discriminator {
        return Ok((None, None));
    }

    let referrer_stats_account_info = next_account_info(account_info_iter).safe_unwrap()?;

    validate!(
        referrer_stats_account_info.is_writable,
        ErrorCode::ReferrerMustBeWritable
    )?;

    let referrer_stats: AccountLoader<UserStats> =
        AccountLoader::try_from(referrer_stats_account_info)
            .or(Err(ErrorCode::CouldNotDeserializeReferrerStats))?;

    Ok((Some(referrer), Some(referrer_stats)))
}

pub fn get_whitelist_token<'a>(
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'a>>>,
) -> VelocityResult<Account<'a, TokenAccount>> {
    let token_account_info = account_info_iter.peek();
    validate!(
        token_account_info.is_some(),
        ErrorCode::InvalidWhitelistToken,
        "Could not find whitelist token"
    )?;

    let token_account_info = token_account_info.safe_unwrap()?;
    let whitelist_token: Account<TokenAccount> =
        Account::try_from(token_account_info).map_err(|e| {
            msg!("Unable to deserialize whitelist token");
            msg!("{:?}", e);
            ErrorCode::InvalidWhitelistToken
        })?;

    Ok(whitelist_token)
}

pub fn get_token_interface<'a>(
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'a>>>,
) -> VelocityResult<Option<Interface<'a, TokenInterface>>> {
    let token_interface_account_info = account_info_iter.peek();
    if token_interface_account_info.is_none() {
        return Ok(None);
    }

    let token_interface_account_info = account_info_iter.next().safe_unwrap()?;
    let token_interface: Interface<TokenInterface> =
        Interface::try_from(token_interface_account_info).map_err(|e| {
            msg!("Unable to deserialize token interface");
            msg!("{:?}", e);
            ErrorCode::DefaultError
        })?;

    Ok(Some(token_interface))
}

pub fn get_token_mint<'a>(
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'a>>>,
) -> VelocityResult<Option<InterfaceAccount<'a, Mint>>> {
    let mint_account_info = account_info_iter.peek();
    if mint_account_info.is_none() {
        return Ok(None);
    }

    let mint_account_info = account_info_iter.next().safe_unwrap()?;

    match InterfaceAccount::try_from(mint_account_info) {
        Ok(mint) => Ok(Some(mint)),
        Err(_) => Ok(None),
    }
}

pub fn get_revenue_share_escrow_account<'a>(
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'a>>>,
    expected_authority: &Pubkey,
) -> VelocityResult<Option<RevenueShareEscrowZeroCopyMut<'a>>> {
    let account_info = account_info_iter.peek();
    if account_info.is_none() {
        return Ok(None);
    }

    let account_info = account_info.safe_unwrap()?;

    // Check size and discriminator without borrowing
    if account_info.data_len() < 80 {
        return Ok(None);
    }

    let discriminator: &[u8] = RevenueShareEscrow::DISCRIMINATOR;
    let borrowed_data = account_info.data.borrow();
    let account_discriminator = array_ref![&borrowed_data, 0, 8];
    if account_discriminator != discriminator {
        return Ok(None);
    }

    let account_info = account_info_iter.next().safe_unwrap()?;

    drop(borrowed_data);
    let escrow: RevenueShareEscrowZeroCopyMut<'a> = account_info.load_zc_mut()?;

    validate!(
        escrow.fixed.authority == *expected_authority,
        ErrorCode::RevenueShareEscrowAuthorityMismatch,
        "invalid RevenueShareEscrow authority"
    )?;

    Ok(Some(escrow))
}

/// Reads the referrer's `UserStats`, which follows a referred taker's
/// `RevenueShareEscrow` in remaining accounts, and returns its stored
/// Accelerated status. The account stays read-only so a popular referrer does
/// not take a writable lock on every referee fill.
///
/// The account is optional. It is consumed only when it is present, is a
/// `UserStats`, and belongs to the escrow's referrer. Anything else leaves the
/// iterator untouched and returns the standard rate. The account selects a
/// reward rate and nothing else, so a missing one does not fail the fill.
pub fn get_referrer_accelerated_status<'a>(
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'a>>>,
    escrow: Option<&RevenueShareEscrowZeroCopyMut<'a>>,
) -> VelocityResult<bool> {
    let Some(referrer) = escrow.and_then(|escrow| escrow.get_referrer()) else {
        return Ok(false);
    };

    let Some(account_info) = account_info_iter.peek() else {
        return Ok(false);
    };

    // Check the owner, the discriminator and the authority before consuming, so
    // an account that does not match stays in the iterator for the group that
    // owns it.
    if account_info.owner != &crate::ID || account_info.data_len() < 8 + 32 {
        return Ok(false);
    }

    {
        let borrowed_data = account_info.data.borrow();
        if array_ref![&borrowed_data, 0, 8] != UserStats::DISCRIMINATOR {
            return Ok(false);
        }
        if array_ref![&borrowed_data, 8, 32] != &referrer.to_bytes() {
            return Ok(false);
        }
    }

    let account_info = account_info_iter.next().safe_unwrap()?;

    let referrer_stats: AccountLoader<UserStats> = AccountLoader::try_from(account_info)
        .or(Err(ErrorCode::CouldNotDeserializeReferrerStats))?;
    let referrer_stats = referrer_stats
        .load()
        .or(Err(ErrorCode::UnableToLoadUserStatsAccount))?;

    Ok(referrer_stats.is_accelerated_referrer())
}

/// Loads `count` read-only `User` accounts of `escrow_authority` from the front of the
/// remaining-account iterator. The caller runs `RevenueShareEscrow::revoke_completed_orders` on
/// each one.
///
/// `revoke_completed_orders` matches a row by `user.sub_account_id`. The order list of one
/// sub-account says nothing about a row of a different sub-account. A scan across sub-accounts
/// would complete a live row that holds fees and clear it too early (OtterSec #82). The caller
/// must therefore supply each sub-account.
///
/// These accounts must be read-only. That also marks the end of the group.
/// `load_revenue_share_map` reads the next group and requires writable `User`
/// accounts, because it credits them. A wrong `count` therefore fails. A value
/// that is too high rejects a writable beneficiary here. A value that is too low
/// fails the map loader with `UserWrongMutability`.
pub fn load_escrow_owner_sub_accounts<'a>(
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'a>>>,
    escrow_authority: &Pubkey,
    count: u8,
) -> VelocityResult<Vec<AccountLoader<'a, User>>> {
    let mut loaders = Vec::with_capacity(count as usize);

    for _ in 0..count {
        let account_info = account_info_iter.next().safe_unwrap()?;

        validate!(
            !account_info.is_writable,
            ErrorCode::UserWrongMutability,
            "escrow owner sub-account {} must be read-only",
            account_info.key
        )?;

        let authority = {
            let data = account_info
                .try_borrow_data()
                .or(Err(ErrorCode::CouldNotLoadUserData))?;
            if data.len() < User::SIZE || &data[..8] != User::DISCRIMINATOR {
                return Err(ErrorCode::CouldNotLoadUserData);
            }
            Pubkey::from(*array_ref![data, 8, 32])
        };

        validate!(
            authority == *escrow_authority,
            ErrorCode::RevenueShareEscrowAuthorityMismatch,
            "escrow owner sub-account {} belongs to {}, not escrow authority {}",
            account_info.key,
            authority,
            escrow_authority
        )?;

        loaders.push(AccountLoader::try_from(account_info).or(Err(ErrorCode::InvalidUserAccount))?);
    }

    Ok(loaders)
}

/// Validates that a builder referenced by an order may collect the requested fee.
///
/// Returns `Ok(None)` when builder codes are disabled or the order carries no builder fields.
/// When a builder fee is present it validates the escrow ownership, that the builder is not
/// revoked, and that the requested fee does not exceed the builder's max, returning the fee in
/// tenths of a bps. Errors if a builder fee is requested but the escrow has not been loaded.
pub fn validate_builder_fee(
    escrow: Option<&mut RevenueShareEscrowZeroCopyMut>,
    user_authority: &Pubkey,
    builder_idx: Option<u8>,
    builder_fee_tenth_bps: Option<u16>,
    state: &State,
) -> VelocityResult<Option<u16>> {
    if !state.builder_codes_enabled() {
        return Ok(None);
    }
    let (builder_idx, builder_fee) = match (builder_idx, builder_fee_tenth_bps) {
        (Some(idx), Some(fee)) => (idx, fee),
        _ => return Ok(None),
    };

    // A global ceiling on the builder fee, separate from the builder's own
    // `max_fee_tenth_bps`, which approval accepts with no ceiling. It bounds the
    // value one fill can route to a builder, so the builder fee cannot move an
    // amount the taker could not withdraw under initial margin (OtterSec #83).
    validate!(
        builder_fee <= crate::math::constants::MAX_BUILDER_FEE_TENTH_BPS,
        ErrorCode::InvalidBuilderFee,
        "builder fee {} exceeds global max {} (tenth-bps)",
        builder_fee,
        crate::math::constants::MAX_BUILDER_FEE_TENTH_BPS
    )?;

    let escrow = match escrow {
        Some(escrow) => escrow,
        None => {
            validate!(
                false,
                ErrorCode::UnableToLoadRevenueShareAccount,
                "Order has builder fee but no escrow account found"
            )?;
            unreachable!()
        }
    };

    validate!(
        escrow.fixed.authority == *user_authority,
        ErrorCode::InvalidUserAccount,
        "RevenueShareEscrow account must be owned by taker",
    )?;

    let builder = escrow.get_approved_builder_mut(builder_idx)?;

    if builder.is_revoked() {
        return Err(ErrorCode::BuilderRevoked);
    }

    if builder_fee > builder.max_fee_tenth_bps {
        return Err(ErrorCode::InvalidBuilderFee);
    }

    Ok(Some(builder_fee))
}

/// Move-based convenience wrapper around [`validate_builder_fee`] for single-order callers.
///
/// Takes ownership of the loaded escrow and returns it together with the validated builder fee
/// when the order carries a valid builder code, or `(None, None)` otherwise.
pub fn validate_and_load_builder<'a>(
    mut escrow: Option<RevenueShareEscrowZeroCopyMut<'a>>,
    user_authority: &Pubkey,
    builder_idx: Option<u8>,
    builder_fee_tenth_bps: Option<u16>,
    state: &State,
) -> VelocityResult<(Option<RevenueShareEscrowZeroCopyMut<'a>>, Option<u16>)> {
    let builder_fee_bps = validate_builder_fee(
        escrow.as_mut(),
        user_authority,
        builder_idx,
        builder_fee_tenth_bps,
        state,
    )?;
    match builder_fee_bps {
        Some(_) => Ok((escrow, builder_fee_bps)),
        None => Ok((None, None)),
    }
}

/// Adds a [`RevenueShareOrder`] to the escrow for the order about to be placed and returns a
/// mutable reference to it, suitable to pass to `controller::orders::place_perp_trigger_order` as the
/// `rev_share_order` argument. Returns `Ok(None)` when the order carries no builder code
/// (`builder_idx`/`builder_fee_bps` is `None`), when there is no escrow, or when the escrow's
/// order list is full.
///
/// Gating on the builder fee here (rather than only on the escrow being present) is important:
/// a referred user has an escrow even for orders without a builder code, and we must not create
/// a spurious builder order for those — the escrow is still loaded so the fill can accrue the
/// referrer's revenue share.
pub fn add_builder_order<'a, 'b>(
    escrow: &'b mut Option<RevenueShareEscrowZeroCopyMut<'a>>,
    user: &User,
    builder_idx: Option<u8>,
    builder_fee_bps: Option<u16>,
    order_id: u32,
    market_index: u16,
) -> VelocityResult<Option<&'b mut RevenueShareOrder>> {
    let (builder_idx, builder_fee_bps) = match (builder_idx, builder_fee_bps) {
        (Some(idx), Some(fee)) => (idx, fee),
        _ => return Ok(None),
    };

    // A requested builder fee must reserve a row. An absent or full escrow that
    // silently returned `Ok(None)` would drop the `HasBuilder` bit and let a
    // taker dodge the fee by zero-sizing or filling the order list, so placement
    // is rejected instead (OtterSec #82); `validate_builder_fee` is the first guard.
    let escrow = escrow
        .as_mut()
        .ok_or(ErrorCode::UnableToLoadRevenueShareAccount)?;

    let new_order_index = user
        .orders
        .iter()
        .position(|order| order.is_available())
        .ok_or(ErrorCode::MaxNumberOfOrders)?;

    // `add_order` returns `RevenueShareEscrowOrdersAccountFull` when no slot is
    // free. The error propagates instead of turning into a no-builder placement.
    let order_idx = escrow.add_order(RevenueShareOrder::new(
        builder_idx,
        user.sub_account_id,
        order_id,
        builder_fee_bps,
        MarketType::Perp,
        market_index,
        RevenueShareOrderBitFlag::Open as u8,
        new_order_index as u8,
    ))?;
    Ok(escrow.get_order_mut(order_idx).ok())
}

/// The compute budget this transaction asked for. The first value is the price
/// per compute unit, in micro-lamports. The second is the unit limit.
///
/// Both are ordinary instructions to the compute-budget program, so this reads
/// them back off the instructions sysvar. A crank that reimburses what a turner
/// spent needs the price, because the priority fee is `price * units` and
/// nothing else onchain records it.
///
/// An absent instruction reads as zero. That matches the runtime for the price,
/// because no price set means no priority fee. It does not match for the limit,
/// because the runtime applies a default. A caller reimbursed against a zero
/// limit gets nothing rather than too much, which is the safe direction. Every
/// caller that wants reimbursement states its limit.
pub fn tx_compute_budget(instructions_sysvar: &AccountInfo) -> VelocityResult<(u64, u32)> {
    use {
        solana_program::sysvar::instructions::load_instruction_at_checked, std::convert::TryInto,
    };

    /// `ComputeBudget111111111111111111111111111111`.
    const COMPUTE_BUDGET_ID: Pubkey =
        solana_program::pubkey!("ComputeBudget111111111111111111111111111111");
    /// `SetComputeUnitLimit(u32)`.
    const SET_UNIT_LIMIT: u8 = 2;
    /// `SetComputeUnitPrice(u64)`, in micro-lamports per compute unit.
    const SET_UNIT_PRICE: u8 = 3;

    let (mut price, mut limit) = (0u64, 0u32);
    let mut index = 0usize;
    while let Ok(instruction) = load_instruction_at_checked(index, instructions_sysvar) {
        index += 1;
        if instruction.program_id != COMPUTE_BUDGET_ID {
            continue;
        }

        match instruction.data.split_first() {
            Some((&SET_UNIT_PRICE, rest)) if rest.len() >= 8 => {
                price = u64::from_le_bytes(rest[..8].try_into().unwrap());
            }
            Some((&SET_UNIT_LIMIT, rest)) if rest.len() >= 4 => {
                limit = u32::from_le_bytes(rest[..4].try_into().unwrap());
            }
            _ => {}
        }
    }

    Ok((price, limit))
}

/// How many instructions in this transaction claim the same whole-transaction
/// reimbursement as the one running now.
///
/// The transaction pays the priority fee once, and [`tx_compute_budget`] reports
/// that one figure. A crank that reimburses against it divides by the number of
/// peers doing the same, else a transaction batching N of them collects N times
/// one fee. The count matches on the discriminator, so an unrelated velocity
/// instruction does not dilute the share. The result is never zero: the asking
/// instruction is itself a claimant, and an unreadable sysvar answers one.
pub fn tx_reimbursement_claimants(
    instructions_sysvar: &AccountInfo,
    discriminator: &[u8],
) -> VelocityResult<u32> {
    use solana_program::sysvar::instructions::load_instruction_at_checked;
    let mut claimants = 0u32;
    let mut index = 0usize;
    while let Ok(instruction) = load_instruction_at_checked(index, instructions_sysvar) {
        index += 1;
        if instruction.program_id == crate::ID
            && instruction.data.len() >= discriminator.len()
            && &instruction.data[..discriminator.len()] == discriminator
        {
            claimants = claimants.saturating_add(1);
        }
    }

    Ok(claimants.max(1))
}

/// Distinct accounts this transaction locks.
///
/// The runtime caps a transaction at 64 account locks, the scarce resource on
/// a router fill (a CLOB maker costs two, a custom quoter costs five), so this
/// is one of two counts telling a caller whether it had room for an omitted
/// maker. The count is an upper bound: a writable meta can name any pubkey,
/// even one with no account, so a caller can inflate it for 32 bytes per key.
/// The other count, of locks velocity verified itself, cannot be inflated;
/// `withheld_obligation` takes the smaller of the two. The count covers the
/// whole transaction, since a force-cancel ahead of a fill locks accounts
/// too, and the scan is capped at 64.
pub fn tx_writable_lock_count(instructions_sysvar: &AccountInfo) -> VelocityResult<usize> {
    use solana_program::sysvar::instructions::load_instruction_at_checked;
    const CAP: usize = 64;
    let mut seen = [Pubkey::default(); CAP];
    let mut count = 0usize;
    let mut index = 0usize;
    // Count only writable and signer accounts. A read-only lock is shared, so
    // read-only keys are free to append. A writable lock is contended, but
    // only against other users of the same account, so a key nobody else
    // touches is nearly free as well. Program ids are read-only, so they do
    // not count either.
    while let Ok(instruction) = load_instruction_at_checked(index, instructions_sysvar) {
        index += 1;
        for meta in instruction.accounts.iter() {
            if !(meta.is_writable || meta.is_signer) {
                continue;
            }
            if seen[..count].contains(&meta.pubkey) {
                continue;
            }
            if count == CAP {
                return Ok(CAP);
            }

            seen[count] = meta.pubkey;
            count += 1;
        }
    }

    Ok(count)
}
