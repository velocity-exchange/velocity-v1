//! Moving tokens into the protocol from an admin account.
//!
//! [`handle_deposit_into_spot_market_vault`] adds tokens to a spot market vault
//! and raises the cumulative deposit interest, so the gain reaches every lender.
//! [`handle_admin_deposit`] credits one user account instead.

use super::*;

#[access_control(
    deposit_not_paused(&ctx.accounts.state)
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_deposit_into_spot_market_vault<'c: 'info, 'info>(
    ctx: Context<'info, DepositIntoSpotMarketVault<'info>>,
    amount: u64,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;

    validate!(
        !spot_market.is_operation_paused(SpotOperation::Deposit),
        ErrorCode::DefaultError,
        "spot market deposits paused"
    )?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();

    let mint = get_token_mint(remaining_accounts_iter)?;

    msg!(
        "depositing {} into spot market {} vault",
        amount,
        spot_market.market_index
    );

    let raise = raise_cumulative_deposit_interest(spot_market, amount)?;

    controller::token::receive(
        &ctx.accounts.token_program,
        &ctx.accounts.source_vault,
        &ctx.accounts.spot_market_vault,
        &ctx.accounts.admin.to_account_info(),
        amount,
        &mint,
        if spot_market.has_transfer_hook() {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;

    ctx.accounts.spot_market_vault.reload()?;
    validate_spot_market_vault_amount(spot_market, ctx.accounts.spot_market_vault.amount)?;

    spot_market.validate_max_token_deposits_and_borrows(false)?;

    emit!(SpotMarketVaultDepositRecord {
        ts: Clock::get()?.unix_timestamp,
        market_index: spot_market.market_index,
        deposit_balance: spot_market.deposit_balance,
        cumulative_deposit_interest_before: raise.cumulative_deposit_interest_before,
        cumulative_deposit_interest_after: raise.cumulative_deposit_interest_after,
        deposit_token_amount_before: raise.deposit_token_amount_before.cast()?,
        amount
    });

    Ok(())
}

/// What one vault deposit moved the market's deposit index by.
struct CumulativeDepositInterestRaise {
    deposit_token_amount_before: u128,
    cumulative_deposit_interest_before: u128,
    cumulative_deposit_interest_after: u128,
}

/// Raises the market's cumulative deposit interest by the deposited amount.
///
/// The deposit arrives from outside the market, so no lender's scaled balance
/// changes. Raising the index is what spreads the gain across every lender in
/// proportion to what each one holds. Both checks refuse a raise that does not
/// move: a deposit that leaves the index where it was would be lost to the
/// market.
fn raise_cumulative_deposit_interest(
    spot_market: &mut SpotMarket,
    amount: u64,
) -> Result<CumulativeDepositInterestRaise> {
    let deposit_token_amount_before = spot_market.get_deposits()?;

    let deposit_token_amount_after = deposit_token_amount_before.safe_add(amount.cast()?)?;

    validate!(
        deposit_token_amount_after > deposit_token_amount_before,
        ErrorCode::DefaultError,
        "new_deposit_token_amount ({}) <= deposit_token_amount ({})",
        deposit_token_amount_after,
        deposit_token_amount_before
    )?;

    let token_precision = spot_market.get_precision();

    let cumulative_deposit_interest_before = spot_market.cumulative_deposit_interest;

    let cumulative_deposit_interest_after = deposit_token_amount_after
        .safe_mul(SPOT_CUMULATIVE_INTEREST_PRECISION)?
        .safe_div(spot_market.deposit_balance)?
        .safe_mul(SPOT_BALANCE_PRECISION)?
        .safe_div(token_precision.cast()?)?;

    validate!(
        cumulative_deposit_interest_after > cumulative_deposit_interest_before,
        ErrorCode::DefaultError,
        "cumulative_deposit_interest_after ({}) <= cumulative_deposit_interest_before ({})",
        cumulative_deposit_interest_after,
        cumulative_deposit_interest_before
    )?;

    spot_market.cumulative_deposit_interest = cumulative_deposit_interest_after;

    Ok(CumulativeDepositInterestRaise {
        deposit_token_amount_before,
        cumulative_deposit_interest_before,
        cumulative_deposit_interest_after,
    })
}

#[access_control(
    deposit_not_paused(&ctx.accounts.state)
)]
pub fn handle_admin_deposit<'c: 'info, 'info>(
    ctx: Context<'info, AdminDeposit<'info>>,
    market_index: u16,
    amount: u64,
) -> Result<()> {
    let user_key = ctx.accounts.user.key();
    let user = &mut load_mut!(ctx.accounts.user)?;

    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let slot = clock.slot;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        &MarketSet::new(),
        &get_writable_spot_market_set(market_index),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let mint = get_token_mint(remaining_accounts_iter)?;

    if amount == 0 {
        return Err(ErrorCode::InsufficientDeposit.into());
    }

    validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

    let (booked, oracle_price) = book_admin_deposit(
        user,
        &mut maps,
        state.funding_paused()?,
        market_index,
        amount,
        now,
    )?;

    user.update_last_active_slot(slot);

    let spot_market = &mut maps.spot_market_map.get_ref_mut(&market_index)?;
    let user_token_amount_after = user.get_total_token_amount(spot_market)?;

    let details = AdminDepositDetails {
        user_key,
        user_authority: user.authority,
        signer: ctx.accounts.admin.key(),
        booked,
        oracle_price,
        user_token_amount_after,
        now,
    };

    transfer_and_record_admin_deposit(
        AdminDepositTransfer {
            token_program: &ctx.accounts.token_program,
            from: &ctx.accounts.admin_token_account,
            vault: &mut ctx.accounts.spot_market_vault,
            authority: ctx.accounts.admin.as_ref(),
        },
        remaining_accounts_iter,
        spot_market,
        &mint,
        details,
    )
}

/// The token accounts one admin deposit moves value between.
struct AdminDepositTransfer<'a, 'info> {
    token_program: &'a Interface<'info, TokenInterface>,
    from: &'a InterfaceAccount<'info, TokenAccount>,
    vault: &'a mut InterfaceAccount<'info, TokenAccount>,
    authority: &'a AccountInfo<'info>,
}

/// Pulls the deposit from the admin's token account into the market vault and
/// records it.
///
/// The vault is reloaded after the transfer, because the market's own balances
/// must agree with what the vault holds before anything reads either.
fn transfer_and_record_admin_deposit<'info>(
    transfer: AdminDepositTransfer<'_, 'info>,
    remaining_accounts_iter: &mut std::iter::Peekable<std::slice::Iter<'info, AccountInfo<'info>>>,
    spot_market: &mut SpotMarket,
    mint: &Option<InterfaceAccount<'info, Mint>>,
    details: AdminDepositDetails,
) -> Result<()> {
    let AdminDepositTransfer {
        token_program,
        from,
        vault,
        authority,
    } = transfer;

    controller::token::receive(
        token_program,
        from,
        vault,
        authority,
        details.booked.amount,
        mint,
        if spot_market.has_transfer_hook() {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;
    vault.reload()?;
    validate_spot_market_vault_amount(spot_market, vault.amount)?;

    emit_admin_deposit_record(spot_market, details)?;

    spot_market.validate_max_token_deposits_and_borrows(false)?;

    Ok(())
}

/// What one admin deposit booked against the user's position.
struct BookedAdminDeposit {
    /// The amount credited. A reduce-only market caps it at the user's borrow.
    amount: u64,
    total_deposits_after: u64,
    total_withdraws_after: u64,
}

/// Books an admin deposit against the user's spot position.
///
/// The market must be live and in the user's pool. Interest accrues first, so
/// the credit lands against the current index.
fn book_admin_deposit(
    user: &mut User,
    maps: &mut crate::instructions::optional_accounts::AccountMaps,
    funding_paused: bool,
    market_index: u16,
    amount: u64,
    now: i64,
) -> Result<(BookedAdminDeposit, i64)> {
    let spot_market = &mut maps.spot_market_map.get_ref_mut(&market_index)?;
    let oracle_price_data = *maps.oracle_map.get_price_data(&spot_market.oracle_id())?;

    validate!(
        user.pool_id == spot_market.pool_id,
        ErrorCode::InvalidPoolId,
        "user pool id ({}) != market pool id ({})",
        user.pool_id,
        spot_market.pool_id
    )?;

    validate!(
        !matches!(spot_market.status, MarketStatus::Initialized),
        ErrorCode::MarketBeingInitialized,
        "Market is being initialized"
    )?;

    controller::spot_balance::update_spot_market_cumulative_interest(
        spot_market,
        Some(&oracle_price_data),
        now,
        funding_paused,
    )?;

    let position_index = user.force_get_spot_position_index(spot_market.market_index)?;

    // if reduce only, have to compare ix amount to current borrow amount
    let amount = if (spot_market.is_reduce_only())
        && user.spot_positions[position_index].balance_type == SpotBalanceType::Borrow
    {
        user.spot_positions[position_index]
            .get_token_amount(spot_market)?
            .cast::<u64>()?
            .min(amount)
    } else {
        amount
    };

    let total_deposits_after = user.total_deposits;
    let total_withdraws_after = user.total_withdraws;

    credit_spot_position(user, spot_market, position_index, amount)?;

    Ok((
        BookedAdminDeposit {
            amount,
            total_deposits_after,
            total_withdraws_after,
        },
        oracle_price_data.price,
    ))
}

/// Credits one spot position and holds the result to the market's rules.
///
/// A position that rounds to no tokens must hold no balance either. A position
/// that holds a deposit needs a live market.
fn credit_spot_position(
    user: &mut User,
    spot_market: &mut SpotMarket,
    position_index: usize,
    amount: u64,
) -> Result<()> {
    let spot_position = &mut user.spot_positions[position_index];
    controller::spot_position::update_spot_balances_and_cumulative_deposits(
        amount as u128,
        &SpotBalanceType::Deposit,
        spot_market,
        spot_position,
        false,
        None,
    )?;

    let token_amount = spot_position.get_token_amount(spot_market)?;
    if token_amount == 0 {
        validate!(
            spot_position.scaled_balance == 0,
            ErrorCode::InvalidSpotPosition,
            "deposit left user with invalid position. scaled balance = {} token amount = {}",
            spot_position.scaled_balance,
            token_amount
        )?;
    }

    if spot_position.balance_type == SpotBalanceType::Deposit && spot_position.scaled_balance > 0 {
        validate!(
            matches!(spot_market.status, MarketStatus::Active),
            ErrorCode::MarketActionPaused,
            "spot_market not active",
        )?;
    }

    Ok(())
}

/// What one admin deposit records, beyond what the market itself carries.
struct AdminDepositDetails {
    user_key: Pubkey,
    user_authority: Pubkey,
    signer: Pubkey,
    booked: BookedAdminDeposit,
    oracle_price: i64,
    user_token_amount_after: i128,
    now: i64,
}

/// Records an admin deposit against the market it landed in.
fn emit_admin_deposit_record(
    spot_market: &mut SpotMarket,
    details: AdminDepositDetails,
) -> Result<()> {
    let deposit_record_id = get_then_update_id!(spot_market, next_deposit_record_id);
    emit!(DepositRecord {
        ts: details.now,
        deposit_record_id,
        user_authority: details.user_authority,
        user: details.user_key,
        direction: DepositDirection::Deposit,
        amount: details.booked.amount,
        oracle_price: details.oracle_price,
        market_deposit_balance: spot_market.deposit_balance,
        market_withdraw_balance: spot_market.borrow_balance,
        market_cumulative_deposit_interest: spot_market.cumulative_deposit_interest,
        market_cumulative_borrow_interest: spot_market.cumulative_borrow_interest,
        total_deposits_after: details.booked.total_deposits_after,
        total_withdraws_after: details.booked.total_withdraws_after,
        market_index: spot_market.market_index,
        explanation: DepositExplanation::Reward,
        transfer_user: None,
        signer: Some(details.signer),
        user_token_amount_after: details.user_token_amount_after,
    });

    Ok(())
}

#[derive(Accounts)]
pub struct DepositIntoSpotMarketVault<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(constraint = check_hot(&admin.key(), &state, HotRole::VaultDeposit)?)]
    pub admin: Signer<'info>,
    #[account(
        mut,
        token::authority = admin
    )]
    pub source_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = spot_market.load()?.vault == spot_market_vault.key()
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
#[instruction(market_index: u16,)]
pub struct AdminDeposit<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(mut, constraint = check_hot(&admin.key(), &state, HotRole::VaultDeposit)?)]
    pub admin: Signer<'info>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = &spot_market_vault.mint.eq(&admin_token_account.mint),
        token::authority = admin.key()
    )]
    pub admin_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}
