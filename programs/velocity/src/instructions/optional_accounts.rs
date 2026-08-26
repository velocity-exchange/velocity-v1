use {
    crate::{
        error::{
            ErrorCode::{self, UnableToLoadOracle},
            VelocityResult,
        },
        math::safe_unwrap::SafeUnwrap,
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

pub fn load_maps<'a, 'b>(
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'a>>>,
    writable_perp_markets: &'b MarketSet,
    writable_spot_markets: &'b MarketSet,
    slot: u64,
    oracle_guard_rails: Option<OracleGuardRails>,
) -> VelocityResult<AccountMaps<'a>> {
    let oracle_map = OracleMap::load(account_info_iter, slot, oracle_guard_rails)?;
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

pub fn get_maker_and_maker_stats<'a>(
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'a>>>,
) -> VelocityResult<(AccountLoader<'a, User>, AccountLoader<'a, UserStats>)> {
    let maker_account_info =
        next_account_info(account_info_iter).or(Err(ErrorCode::MakerNotFound))?;

    validate!(
        maker_account_info.is_writable,
        ErrorCode::MakerMustBeWritable
    )?;

    let maker: AccountLoader<User> =
        AccountLoader::try_from(maker_account_info).or(Err(ErrorCode::CouldNotDeserializeMaker))?;

    let maker_stats_account_info =
        next_account_info(account_info_iter).or(Err(ErrorCode::MakerStatsNotFound))?;

    validate!(
        maker_stats_account_info.is_writable,
        ErrorCode::MakerStatsMustBeWritable
    )?;

    let maker_stats: AccountLoader<UserStats> =
        AccountLoader::try_from(maker_stats_account_info)
            .or(Err(ErrorCode::CouldNotDeserializeMakerStats))?;

    Ok((maker, maker_stats))
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
    if token_account_info.is_none() {
        msg!("Could not find whitelist token");
        return Err(ErrorCode::InvalidWhitelistToken);
    }

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

/// Loads `count` read-only `User` accounts of `escrow_authority` from the front of the
/// remaining-account iterator. The caller runs `RevenueShareEscrow::revoke_completed_orders` on
/// each one.
///
/// `revoke_completed_orders` matches a row by `user.sub_account_id`. The order list of one
/// sub-account says nothing about a row of a different sub-account. A scan across sub-accounts
/// would complete a live row that holds fees and clear it too early (OtterSec #82). The caller
/// must therefore supply each sub-account.
///
/// These accounts must be read-only. This also marks the end of the group.
/// `load_revenue_share_map` reads the next group and requires writable `User` accounts, because it
/// credits them. A wrong `count` therefore fails: a value that is too high rejects a writable
/// beneficiary here, and a value that is too low fails the map loader with `UserWrongMutability`.
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

    // Global ceiling on the builder fee, independent of the builder's own
    // configured `max_fee_tenth_bps` (accepted at approval with no ceiling).
    // Bounds how much value a fill can route to a builder so the fee rail can't
    // move collateral-significant amounts a taker couldn't withdraw under
    // initial margin (OtterSec #83).
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
/// mutable reference to it, suitable to pass to `controller::orders::place_perp_order` as the
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
        // No builder requested — nothing to attach.
        _ => return Ok(None),
    };
    // A builder fee WAS requested, so a row MUST be reserved. Previously an
    // absent or full escrow returned `Ok(None)`, dropping the `HasBuilder` bit
    // so the fill silently charged no builder fee — a taker could dodge the fee
    // by zero-sizing or filling their escrow's order list. Reject the placement
    // instead of silently downgrading it (OtterSec #82). `validate_builder_fee`
    // already errors on an absent escrow when a fee is requested, so the `None`
    // case here is defensive.
    let escrow = escrow
        .as_mut()
        .ok_or(ErrorCode::UnableToLoadRevenueShareAccount)?;

    let new_order_index = user
        .orders
        .iter()
        .position(|order| order.is_available())
        .ok_or(ErrorCode::MaxNumberOfOrders)?;

    // `add_order` returns `RevenueShareEscrowOrdersAccountFull` when no slot is
    // free; propagate it rather than swallowing it into a no-builder placement.
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

/// Whether the running transaction is co-signed by `signer`, read off the
/// instructions sysvar. The signer's signature carries no funds authority
/// by itself — it is a marker other checks introspect (the flow-authority
/// attestation): the signature proves the transaction passed through the
/// key's holder, and instruction data can't forge a signer flag.
/// The compute budget this transaction asked for: the price it set per
/// compute unit, in micro-lamports, and the unit limit it requested.
///
/// Both are ordinary instructions to the compute-budget program, so the
/// transaction states them and this reads them back. A crank that reimburses
/// what a turner spent needs the price, because the priority fee is
/// `price × units` and nothing else on chain records it.
///
/// Absent instructions read as zero. For the price that matches the runtime —
/// no price set is no priority fee. For the limit it does not: the runtime
/// applies a default. A caller reimbursed against this therefore gets nothing
/// rather than something, which is the safe direction, and every caller that
/// wants reimbursing states its limit.
pub fn tx_compute_budget(instructions_sysvar: &AccountInfo) -> VelocityResult<(u64, u32)> {
    use {
        solana_program::sysvar::instructions::load_instruction_at_checked, std::convert::TryInto,
    };
    /// `ComputeBudget111111111111111111111111111111`.
    const COMPUTE_BUDGET_ID: Pubkey =
        solana_program::pubkey!("ComputeBudget111111111111111111111111111111");
    /// `SetComputeUnitLimit(u32)`.
    const SET_UNIT_LIMIT: u8 = 2;
    /// `SetComputeUnitPrice(u64)` — micro-lamports per compute unit.
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

/// How many instructions in this transaction will claim the same
/// whole-transaction reimbursement as the one running now.
///
/// [`tx_compute_budget`] reports a figure the transaction paid *once*: the
/// priority fee covers the transaction, not each instruction in it. A crank that
/// reimburses against it therefore has to divide by the number of peers that
/// will do the same, or a transaction batching N of them collects N times one
/// fee. Counted by discriminator so an unrelated velocity instruction sharing
/// the transaction does not dilute the share.
///
/// Never zero: the instruction asking is itself one of the claimants, and a
/// sysvar that cannot be read at all answers one rather than dividing by zero.
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

pub fn tx_co_signed_by(instructions_sysvar: &AccountInfo, signer: &Pubkey) -> VelocityResult<bool> {
    use solana_program::sysvar::instructions::load_instruction_at_checked;
    let mut index = 0usize;
    while let Ok(instruction) = load_instruction_at_checked(index, instructions_sysvar) {
        if instruction
            .accounts
            .iter()
            .any(|meta| meta.is_signer && meta.pubkey == *signer)
        {
            return Ok(true);
        }
        index += 1;
    }
    Ok(false)
}
