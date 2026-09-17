//! Moving tokens between a token account and a spot market.
//!
//! A deposit credits the account and may repay a borrow. A withdrawal debits
//! the account and may open one, so it carries the margin check. The revenue
//! pool deposit credits the market itself rather than any user.

use super::*;

/// Admit a deposit into a spot market. The pools must match, the market must
/// be past initialization, and its deposits must not be paused.
fn admit_deposit(user: &User, spot_market: &SpotMarket, market_index: u16) -> Result<()> {
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

    validate!(
        !spot_market.is_operation_paused(SpotOperation::Deposit),
        ErrorCode::MarketActionPaused,
        "spot market {} deposits paused",
        market_index
    )?;

    Ok(())
}

/// What a credited deposit tells the rest of the handler.
struct DepositCredit {
    /// The amount credited. A reduce-only deposit is clamped to the borrow it
    /// repays.
    amount: u64,
    /// Whether the position was a borrow before the credit. The record
    /// explains the deposit as a repayment when it was.
    is_borrow_before: bool,
    /// The market deposit level before the credit, so the daily cap can be
    /// gated on real growth.
    deposit_token_amount_before: u128,
    total_deposits_after: u64,
    total_withdraws_after: u64,
}

/// Credit the deposit to the user and prove the position it leaves is legal.
fn credit_deposit(
    user: &mut User,
    spot_market: &mut SpotMarket,
    amount: u64,
    reduce_only: bool,
    oracle_price: i64,
) -> Result<DepositCredit> {
    let position_index = user.force_get_spot_position_index(spot_market.market_index)?;

    let is_borrow_before = user.spot_positions[position_index].is_borrow();

    // Snapshot the market's deposit level so the daily cap below runs only on real growth.
    // This instruction also repays borrows (see `DepositExplanation::RepayBorrow`). A repayment
    // reduces `borrow_balance` and leaves `deposit_balance` alone, so a market-wide level
    // predicate must not block it. OtterSec #118 removed that exit lock from the shared credit
    // path.
    let deposit_token_amount_before = math::spot_balance::get_token_amount(
        spot_market.deposit_balance,
        spot_market,
        &SpotBalanceType::Deposit,
    )?;

    let force_reduce_only = spot_market.is_reduce_only();

    // if reduce only, have to compare ix amount to current borrow amount
    let amount = if (force_reduce_only || reduce_only)
        && user.spot_positions[position_index].balance_type == SpotBalanceType::Borrow
    {
        user.spot_positions[position_index]
            .get_token_amount(spot_market)?
            .cast::<u64>()?
            .min(amount)
    } else {
        amount
    };

    user.increment_total_deposits(amount, oracle_price, spot_market.get_precision().cast()?)?;

    let total_deposits_after = user.total_deposits;
    let total_withdraws_after = user.total_withdraws;

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

    Ok(DepositCredit {
        amount,
        is_borrow_before,
        deposit_token_amount_before,
        total_deposits_after,
        total_withdraws_after,
    })
}

/// The key that signed for a deposit into an account it does not own. Returns
/// `None` when the owner or its delegate signed. Only the mainnet build holds
/// an allowlist of external depositors.
fn external_deposit_signer(authority: Pubkey, user: &User) -> Result<Option<Pubkey>> {
    if authority == user.authority || authority == user.delegate {
        return Ok(None);
    }

    #[cfg(feature = "mainnet-beta")]
    validate!(
        WHITELISTED_EXTERNAL_DEPOSITORS.contains(&authority),
        ErrorCode::DefaultError,
        "Not whitelisted external depositor"
    )?;

    Ok(Some(authority))
}

/// One deposit into a spot market.
struct DepositRequest {
    market_index: u16,
    amount: u64,
    reduce_only: bool,
    now: i64,
    funding_paused: bool,
}

/// Accrue interest on the market, admit the deposit, and credit it. Returns
/// what was credited and the price it was valued at.
fn accrue_and_credit_deposit(
    user: &mut User,
    maps: &mut AccountMaps,
    request: &DepositRequest,
) -> Result<(DepositCredit, i64)> {
    let mut spot_market = maps.spot_market_map.get_ref_mut(&request.market_index)?;
    let oracle_price_data = *maps.oracle_map.get_price_data(&spot_market.oracle_id())?;

    admit_deposit(user, &spot_market, request.market_index)?;

    controller::spot_balance::update_spot_market_cumulative_interest(
        &mut spot_market,
        Some(&oracle_price_data),
        request.now,
        request.funding_paused,
    )?;

    let credit = credit_deposit(
        user,
        &mut spot_market,
        request.amount,
        request.reduce_only,
        oracle_price_data.price,
    )?;

    Ok((credit, oracle_price_data.price))
}

/// What stamps a deposit record beyond the amounts: when the deposit happened,
/// what it was valued at, and who signed for it.
struct DepositStamp {
    ts: i64,
    oracle_price: i64,
    authority: Pubkey,
}

/// Record the credit of a deposit. The record explains the credit as a
/// repayment when the position was a borrow before it.
fn emit_deposit_record(
    user: &User,
    user_key: Pubkey,
    spot_market: &mut SpotMarket,
    credit: &DepositCredit,
    stamp: &DepositStamp,
) -> Result<()> {
    emit_spot_balance_move(
        user,
        user_key,
        spot_market,
        SpotBalanceMove {
            ts: stamp.ts,
            direction: DepositDirection::Deposit,
            amount: credit.amount,
            oracle_price: stamp.oracle_price,
            explanation: if credit.is_borrow_before {
                DepositExplanation::RepayBorrow
            } else {
                DepositExplanation::None
            },
            transfer_user: None,
            signer: external_deposit_signer(stamp.authority, user)?,
            total_deposits_after: credit.total_deposits_after,
            total_withdraws_after: credit.total_withdraws_after,
        },
    )
}

/// Admit a credit into the revenue pool of a spot market. The market must not
/// be in settlement, and its deposits must not be paused: the credit moves
/// tokens into the spot vault, so it obeys the same pause as a direct deposit.
fn admit_revenue_pool_deposit(spot_market: &SpotMarket, now: i64) -> Result<()> {
    validate!(
        !spot_market.is_in_settlement(now),
        ErrorCode::DefaultError,
        "spot market {} not active",
        spot_market.market_index
    )?;

    validate!(
        !spot_market.is_operation_paused(SpotOperation::Deposit),
        ErrorCode::MarketActionPaused,
        "spot market {} deposits paused",
        spot_market.market_index
    )?;

    Ok(())
}

/// Accrue interest on the market a withdrawal debits, and hold its oracle
/// TWAPs at the values the margin check must read.
///
/// A refresh drags both stored TWAPs toward the live price, and the margin
/// check in the same instruction then reads the dragged values (OtterSec #81).
/// Each field feeds a different gate.
///
///   `last_oracle_price_twap` (1h). `TooVolatile` validity compares the live
///   oracle price against it. A refresh that drags it toward the live price
///   lets a too-volatile oracle pass the same-instruction margin check and
///   release vault tokens.
///
///   `last_oracle_price_twap_5min`. `StrictOraclePrice` bounds are the min and
///   max of the live price and this field, and a liability is priced at the
///   upper bound. The margin check runs with `Initial`, which enables strict
///   pricing. Dragging the 5-minute TWAP toward a temporarily depressed live
///   price under-values the debt and admits a withdrawal the pre-refresh value
///   rejects.
///
/// Returns the refreshed values. The caller restores them with
/// [`restore_oracle_twaps`] after the margin check, so the account still
/// persists the up-to-date EMA.
fn accrue_interest_behind_twaps(
    spot_market: &mut SpotMarket,
    oracle_price_data: &OraclePriceData,
    now: i64,
    funding_paused: bool,
) -> Result<(i64, i64)> {
    let pre_refresh = (
        spot_market.historical_oracle_data.last_oracle_price_twap,
        spot_market
            .historical_oracle_data
            .last_oracle_price_twap_5min,
    );

    controller::spot_balance::update_spot_market_cumulative_interest(
        spot_market,
        Some(oracle_price_data),
        now,
        funding_paused,
    )?;

    let refreshed = (
        spot_market.historical_oracle_data.last_oracle_price_twap,
        spot_market
            .historical_oracle_data
            .last_oracle_price_twap_5min,
    );

    restore_oracle_twaps(spot_market, pre_refresh);

    Ok(refreshed)
}

/// Write back the oracle TWAPs that [`accrue_interest_behind_twaps`] held out
/// of the margin check.
fn restore_oracle_twaps(spot_market: &mut SpotMarket, twaps: (i64, i64)) {
    spot_market.historical_oracle_data.last_oracle_price_twap = twaps.0;
    spot_market
        .historical_oracle_data
        .last_oracle_price_twap_5min = twaps.1;
}

/// Accrue interest on the market a withdrawal debits, and report whether the
/// market is reduce-only along with the TWAPs the caller must restore.
fn accrue_withdraw_market(
    maps: &mut AccountMaps,
    market_index: u16,
    now: i64,
    funding_paused: bool,
) -> Result<(bool, (i64, i64))> {
    let spot_market = &mut maps.spot_market_map.get_ref_mut(&market_index)?;
    let oracle_price_data = *maps.oracle_map.get_price_data(&spot_market.oracle_id())?;

    let refreshed =
        accrue_interest_behind_twaps(spot_market, &oracle_price_data, now, funding_paused)?;

    Ok((spot_market.is_reduce_only(), refreshed))
}

/// Prove the account may release value against its other markets.
///
/// OtterSec #135: this handler cranks only the market being withdrawn, so a
/// borrow in any *other* market is valued through its stale stored
/// `cumulative_borrow_interest`. The due interest is missing from the
/// initial-margin check, and those markets arrive read-only so they cannot be
/// refreshed here. Their accrual is required to be recent instead.
fn check_withdraw_margin(user: &mut User, maps: &mut AccountMaps, now: i64) -> Result<()> {
    math::margin::validate_spot_borrow_interest_fresh_for_margin(user, &maps.spot_market_map, now)?;

    user.meets_withdraw_margin_requirement(maps, MarginRequirementType::Initial)?;

    validate_spot_margin_trading(user, maps)?;

    Ok(())
}

/// Record the debit of a withdrawal. The record explains the debit as a borrow
/// when the account ends up owing the market.
fn emit_withdraw_record(
    user: &User,
    user_key: Pubkey,
    spot_market: &mut SpotMarket,
    amount: u64,
    oracle_price: i64,
    now: i64,
) -> Result<()> {
    let is_borrow = user
        .get_spot_position(spot_market.market_index)
        .is_ok_and(|position| position.is_borrow());

    emit_spot_balance_move(
        user,
        user_key,
        spot_market,
        SpotBalanceMove {
            ts: now,
            direction: DepositDirection::Withdraw,
            amount,
            oracle_price,
            explanation: if is_borrow {
                DepositExplanation::Borrow
            } else {
                DepositExplanation::None
            },
            transfer_user: None,
            signer: None,
            total_deposits_after: user.total_deposits,
            total_withdraws_after: user.total_withdraws,
        },
    )
}

/// Clamp a reduce-only withdrawal to the deposit it consumes, then debit it.
/// Returns the amount debited.
fn debit_withdraw(
    user: &mut User,
    maps: &mut AccountMaps,
    market_index: u16,
    amount: u64,
    reduce_only: bool,
) -> Result<u64> {
    let position_index = user.force_get_spot_position_index(market_index)?;

    let amount = if reduce_only {
        validate!(
            user.spot_positions[position_index].balance_type == SpotBalanceType::Deposit,
            ErrorCode::ReduceOnlyWithdrawIncreasedRisk
        )?;

        let max_withdrawable_amount = calculate_max_withdrawable_amount(market_index, user, maps)?;

        let spot_market = &maps.spot_market_map.get_ref(&market_index)?;
        let existing_deposit_amount = user.spot_positions[position_index]
            .get_token_amount(spot_market)?
            .cast::<u64>()?;

        amount
            .min(max_withdrawable_amount)
            .min(existing_deposit_amount)
    } else {
        amount
    };

    let spot_market = &mut maps.spot_market_map.get_ref_mut(&market_index)?;
    let oracle_price_data = maps.oracle_map.get_price_data(&spot_market.oracle_id())?;

    user.increment_total_withdraws(
        amount,
        oracle_price_data.price,
        spot_market.get_precision().cast()?,
    )?;

    // prevents withdraw when limits hit
    controller::spot_position::update_spot_balances_and_cumulative_deposits_with_limits(
        amount as u128,
        &SpotBalanceType::Borrow,
        spot_market,
        user,
    )?;

    Ok(amount)
}

#[access_control(
    deposit_not_paused(&ctx.accounts.state)
)]
pub fn handle_deposit<'c: 'info, 'info>(
    ctx: Context<'info, Deposit<'info>>,
    market_index: u16,
    amount: u64,
    reduce_only: bool,
) -> Result<()> {
    let user_key = ctx.accounts.user.key();
    let user = &mut load_mut!(ctx.accounts.user)?;

    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps =
        load_one_spot_market_maps(remaining_accounts_iter, &state, market_index, clock.slot)?;

    let mint = get_token_mint(remaining_accounts_iter)?;

    if amount == 0 {
        return Err(ErrorCode::InsufficientDeposit.into());
    }

    validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

    let (credit, oracle_price) = accrue_and_credit_deposit(
        user,
        &mut maps,
        &DepositRequest {
            market_index,
            amount,
            reduce_only,
            now,
            funding_paused: state.funding_paused()?,
        },
    )?;

    exit_liquidation_if_healthy(user, &mut maps, state.liquidation_margin_buffer_ratio)?;

    user.update_last_active_slot(clock.slot);

    let spot_market = &mut maps.spot_market_map.get_ref_mut(&market_index)?;

    controller::token::receive(
        &ctx.accounts.token_program,
        &ctx.accounts.user_token_account,
        &ctx.accounts.spot_market_vault,
        &ctx.accounts.authority,
        credit.amount,
        &mint,
        if spot_market.has_transfer_hook() {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;
    ctx.accounts.spot_market_vault.reload()?;

    emit_deposit_record(
        user,
        user_key,
        spot_market,
        &credit,
        &DepositStamp {
            ts: now,
            oracle_price,
            authority: ctx.accounts.authority.key(),
        },
    )?;

    spot_market.validate_max_token_deposits_and_borrows(false)?;

    // The cap runs only on real growth, for the same reason as the shared credit path
    // (OtterSec #118). The cap is a market-wide level predicate. Validating it on every call
    // blocked a borrow repayment whenever the market already sat above its cap. A repayment is
    // one of the actions that brings the level back down, and a liquidatable user needs it.
    math::spot_withdraw::validate_deposit_cap_after_increase(
        spot_market,
        credit.deposit_token_amount_before,
    )?;

    Ok(())
}

#[access_control(
    withdraw_not_paused(&ctx.accounts.state)
)]
pub fn handle_withdraw<'c: 'info, 'info>(
    ctx: Context<'info, Withdraw<'info>>,
    market_index: u16,
    amount: u64,
    reduce_only: bool,
) -> anchor_lang::Result<()> {
    let user_key = ctx.accounts.user.key();
    let user = &mut load_mut!(ctx.accounts.user)?;
    let user_stats = load_mut!(ctx.accounts.user_stats)?;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let state = ctx.accounts.state.load()?;

    validate!(
        !user_stats.is_equity_breaker_tripped(),
        ErrorCode::EquityBelowFloor,
        "equity floor breaker is tripped for this authority"
    )?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps =
        load_one_spot_market_maps(remaining_accounts_iter, &state, market_index, clock.slot)?;

    let mint = get_token_mint(remaining_accounts_iter)?;

    validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

    let (spot_market_is_reduce_only, refreshed_liability_twaps) =
        accrue_withdraw_market(&mut maps, market_index, now, state.funding_paused()?)?;

    let amount = debit_withdraw(
        user,
        &mut maps,
        market_index,
        amount,
        reduce_only || spot_market_is_reduce_only,
    )?;

    check_withdraw_margin(user, &mut maps, now)?;

    {
        let spot_market = &mut maps.spot_market_map.get_ref_mut(&market_index)?;
        restore_oracle_twaps(spot_market, refreshed_liability_twaps);
    }

    if user.is_cross_margin_being_liquidated() {
        user.exit_cross_margin_liquidation();
    }

    user.update_last_active_slot(clock.slot);

    let mut spot_market = maps.spot_market_map.get_ref_mut(&market_index)?;
    let oracle_price = maps
        .oracle_map
        .get_price_data(&spot_market.oracle_id())?
        .price;

    let is_borrow = user
        .get_spot_position(market_index)
        .is_ok_and(|pos| pos.is_borrow());

    emit_withdraw_record(user, user_key, &mut spot_market, amount, oracle_price, now)?;

    controller::token::send_from_program_vault(
        &ctx.accounts.token_program,
        &ctx.accounts.spot_market_vault,
        &ctx.accounts.user_token_account,
        &ctx.accounts.velocity_signer,
        state.signer_nonce,
        amount,
        &mint,
        if spot_market.has_transfer_hook() {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;

    // reload the spot market vault balance so it's up-to-date
    ctx.accounts.spot_market_vault.reload()?;
    math::spot_withdraw::validate_spot_market_vault_amount(
        &spot_market,
        ctx.accounts.spot_market_vault.amount,
    )?;

    spot_market.validate_max_token_deposits_and_borrows(is_borrow)?;

    Ok(())
}

#[access_control(
    deposit_not_paused(&ctx.accounts.state)
)]
pub fn handle_deposit_into_spot_market_revenue_pool<'c: 'info, 'info>(
    ctx: Context<'info, RevenuePoolDeposit<'info>>,
    amount: u64,
) -> Result<()> {
    if amount == 0 {
        return Err(ErrorCode::InsufficientDeposit.into());
    }

    let now = Clock::get()?.unix_timestamp;

    let mut spot_market = load_mut!(ctx.accounts.spot_market)?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();

    let mint = get_token_mint(remaining_accounts_iter)?;

    admit_revenue_pool_deposit(&spot_market, now)?;

    // Refresh cumulative deposit and borrow interest before crediting, like the normal
    // `handle_deposit` path. `update_revenue_pool_balances` converts `amount` into a
    // scaled balance using `cumulative_deposit_interest`. A stale market holds a lower
    // interest value, which mints too large a scaled balance. A later refresh at
    // settlement then revalues it upward, so the revenue pool claims interest that
    // accrued before this deposit existed. No oracle account reaches this instruction,
    // so the refresh passes `None`, like the revenue-settle and pnl-deficit paths.
    controller::spot_balance::update_spot_market_cumulative_interest(
        &mut spot_market,
        None,
        now,
        ctx.accounts.state.load()?.funding_paused()?,
    )?;

    controller::spot_balance::update_revenue_pool_balances(
        amount.cast::<u128>()?,
        &SpotBalanceType::Deposit,
        &mut spot_market,
        false,
    )?;

    controller::token::receive(
        &ctx.accounts.token_program,
        &ctx.accounts.user_token_account,
        &ctx.accounts.spot_market_vault,
        &ctx.accounts.authority,
        amount,
        &mint,
        if spot_market.has_transfer_hook() {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;

    spot_market.validate_max_token_deposits_and_borrows(false)?;
    ctx.accounts.spot_market_vault.reload()?;
    math::spot_withdraw::validate_spot_market_vault_amount(
        &spot_market,
        ctx.accounts.spot_market_vault.amount,
    )?;

    Ok(())
}

#[derive(Accounts)]
#[instruction(market_index: u16,)]
pub struct Deposit<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = &spot_market_vault.mint.eq(&user_token_account.mint),
        token::authority = authority
    )]
    pub user_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct RevenuePoolDeposit<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), spot_market.load()?.market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = &spot_market_vault.mint.eq(&user_token_account.mint),
        token::authority = authority
    )]
    pub user_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
#[instruction(market_index: u16,)]
pub struct Withdraw<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        has_one = authority,
    )]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        has_one = authority
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        constraint = state.load()?.signer.eq(&velocity_signer.key())
    )]
    /// CHECK: forced velocity_signer
    pub velocity_signer: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = &spot_market_vault.mint.eq(&user_token_account.mint)
    )]
    pub user_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}
