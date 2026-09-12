//! Moving a spot balance between two subaccounts of one authority.
//!
//! No tokens leave the protocol, so the vault never moves. The source side
//! carries the withdraw margin check and the destination side carries the
//! deposit admission checks, so a transfer can never do what a direct
//! withdrawal and deposit could not.

use super::*;

/// What a transfer moves, and the market state it moves under.
struct TransferTerms {
    market_index: u16,
    amount: u64,
    spot_market_vault_amount: u64,
    now: i64,
    slot: u64,
    funding_paused: bool,
}

/// While the equity breaker is tripped, the only delegate transfer allowed is
/// one that shrinks an existing breach: funds only (no floor movement) into a
/// subaccount below its buffered floor. The debited side is still gated at its
/// own floor plus buffer by the withdraw margin check inside
/// [`transfer_spot_deposit`], so a cure cannot create a new breach. Once the
/// credited side clears its buffered floor this path closes again. The transfer
/// never clears the flag; only the admin reset does.
fn validate_cure_transfer(
    to_user: &User,
    maps: &mut AccountMaps,
    equity_floor_delta: u64,
) -> Result<()> {
    validate!(
        equity_floor_delta == 0,
        ErrorCode::EquityBelowFloor,
        "equity floor breaker is tripped for this authority; floor cannot move"
    )?;

    validate!(
        to_user.equity_floor > 0,
        ErrorCode::EquityBelowFloor,
        "equity floor breaker is tripped for this authority; transfers must cure a floored subaccount"
    )?;

    let (to_user_net_equity, to_user_oracles_valid) = calculate_user_equity(to_user, maps)?;

    // Cure eligibility must not be decided off an invalid price, matching
    // the validity the trip and the reset require of the same metric.
    // Deliberately a blanket reject rather than the bounded metric the
    // floor gates use: this is a standalone instruction (no innocent
    // third party to abort), and failing frozen is the right direction.
    validate!(
        to_user_oracles_valid,
        ErrorCode::InvalidOracle,
        "cannot verify cure transfer with an invalid oracle"
    )?;

    validate!(
        to_user.is_below_buffered_equity_floor(to_user_net_equity),
        ErrorCode::EquityBelowFloor,
        "equity floor breaker is tripped for this authority; transfers must cure a subaccount below its buffered equity floor"
    )?;

    Ok(())
}

/// Carry equity floor along with the funds so the sum of floors across the
/// authority's subaccounts is preserved. The from side is validated against its
/// reduced floor by the withdraw margin check inside [`transfer_spot_deposit`].
/// The to side is validated by [`validate_floor_is_backed`] after the deposit
/// lands, so its increased floor must be backed by real equity.
///
/// Guard (#55): a delegate must not shed floor off a subaccount that is already
/// below the floor being reduced. Without this, an owner could shift the floor
/// off a breached subaccount with a zero-amount transfer and drop it out of
/// breach before the permissionless breaker trips, defusing the pending trip.
/// `from_user` is evaluated against its PRE-reduction floor, because the
/// transfer below reduces it. Deliberately checks the raw floor, not floor plus
/// buffer: its only job is trip defusal, and a subaccount inside the buffer band
/// may still rebalance floor away. Measured as net equity, matching the breaker
/// trip threshold.
fn move_equity_floor(
    from_user: &mut User,
    to_user: &mut User,
    maps: &mut AccountMaps,
    equity_floor_delta: u64,
) -> Result<()> {
    let (from_user_net_equity, from_user_oracles_valid) = calculate_user_equity(from_user, maps)?;

    // Defusal eligibility must not be decided off an invalid price. The
    // counterpart `trip_equity_floor_breaker` requires valid oracles, so
    // without this check a stale-high price lets this guard pass in the
    // same slot the trip reverts. The floor transfer would then drop the
    // subaccount to `equity_floor = 0`, after which `is_below_equity_floor`
    // short-circuits to false until an admin sets a new floor. The oracle
    // therefore only has to be bad for the slot this transfer lands in.
    validate!(
        from_user_oracles_valid,
        ErrorCode::InvalidOracle,
        "cannot verify equity floor transfer with an invalid oracle"
    )?;

    validate!(
        !from_user.is_below_equity_floor(from_user_net_equity),
        ErrorCode::InvalidEquityFloorTransfer,
        "from_user net equity {} is below equity floor {}; cannot reduce floor while breached",
        from_user_net_equity,
        from_user.equity_floor
    )?;

    transfer_equity_floor(from_user, to_user, equity_floor_delta)?;

    Ok(())
}

/// A floor that just grew must be backed by equity that is actually
/// measurable. A stale-high price would otherwise let a floor land on a
/// subaccount that cannot back it.
fn validate_floor_is_backed(to_user: &User, maps: &mut AccountMaps) -> Result<()> {
    let (to_user_net_equity, to_user_oracles_valid) = calculate_user_equity(to_user, maps)?;

    validate!(
        to_user_oracles_valid,
        ErrorCode::InvalidOracle,
        "cannot verify equity floor transfer with an invalid oracle"
    )?;

    validate!(
        !to_user.is_below_buffered_equity_floor(to_user_net_equity),
        ErrorCode::InvalidEquityFloorTransfer,
        "to_user net equity {} does not back new equity floor {} + buffer {}",
        to_user_net_equity,
        to_user.equity_floor,
        to_user.equity_floor_buffer
    )?;

    Ok(())
}

#[access_control(
    deposit_not_paused(&ctx.accounts.state)
    withdraw_not_paused(&ctx.accounts.state)
)]
pub fn handle_transfer_deposit_by_delegate<'c: 'info, 'info>(
    ctx: Context<'info, TransferDepositByDelegate<'info>>,
    market_index: u16,
    amount: u64,
    equity_floor_delta: u64,
) -> anchor_lang::Result<()> {
    let signer_key = ctx.accounts.delegate.key();
    let to_user_key = ctx.accounts.to_user.key();
    let from_user_key = ctx.accounts.from_user.key();

    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;

    let to_user = &mut load_mut!(ctx.accounts.to_user)?;
    let from_user = &mut load_mut!(ctx.accounts.from_user)?;
    let user_stats = load!(ctx.accounts.user_stats)?;

    validate!(
        user_stats.is_delegate_transfer_allowed(),
        ErrorCode::DefaultError,
        "delegate transfer not allowed"
    )?;

    let mut maps = load_one_spot_market_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &state,
        market_index,
        clock.slot,
    )?;

    if user_stats.is_equity_breaker_tripped() {
        validate_cure_transfer(to_user, &mut maps, equity_floor_delta)?;
    }

    if equity_floor_delta > 0 {
        move_equity_floor(from_user, to_user, &mut maps, equity_floor_delta)?;
    }

    transfer_spot_deposit(
        &mut TransferParties {
            from_user,
            to_user,
            from_user_key,
            to_user_key,
            signer: Some(signer_key),
        },
        &TransferTerms {
            market_index,
            amount,
            spot_market_vault_amount: ctx.accounts.spot_market_vault.amount,
            now: clock.unix_timestamp,
            slot: clock.slot,
            funding_paused: state.funding_paused()?,
        },
        &mut maps,
    )?;

    if equity_floor_delta > 0 {
        validate_floor_is_backed(to_user, &mut maps)?;
    }

    Ok(())
}

#[access_control(
    deposit_not_paused(&ctx.accounts.state)
    withdraw_not_paused(&ctx.accounts.state)
)]
pub fn handle_transfer_deposit<'c: 'info, 'info>(
    ctx: Context<'info, TransferDeposit<'info>>,
    market_index: u16,
    amount: u64,
) -> anchor_lang::Result<()> {
    let to_user_key = ctx.accounts.to_user.key();
    let from_user_key = ctx.accounts.from_user.key();

    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;

    let to_user = &mut load_mut!(ctx.accounts.to_user)?;
    let from_user = &mut load_mut!(ctx.accounts.from_user)?;
    let user_stats = &mut load_mut!(ctx.accounts.user_stats)?;

    validate!(
        !user_stats.is_equity_breaker_tripped(),
        ErrorCode::EquityBelowFloor,
        "equity floor breaker is tripped for this authority"
    )?;

    validate!(
        !to_user.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "to_user bankrupt"
    )?;

    validate!(
        !from_user.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "from_user bankrupt"
    )?;

    validate!(
        from_user_key != to_user_key,
        ErrorCode::CantTransferBetweenSameUserAccount,
        "cant transfer between the same user account"
    )?;

    let mut maps = load_one_spot_market_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &state,
        market_index,
        clock.slot,
    )?;

    transfer_spot_deposit(
        &mut TransferParties {
            from_user,
            to_user,
            from_user_key,
            to_user_key,
            signer: None,
        },
        &TransferTerms {
            market_index,
            amount,
            spot_market_vault_amount: ctx.accounts.spot_market_vault.amount,
            now: clock.unix_timestamp,
            slot: clock.slot,
            funding_paused: state.funding_paused()?,
        },
        &mut maps,
    )
}

/// Accrue interest on the transferred market, but do NOT advance its *oracle*
/// TWAPs (OtterSec #134 — the same shape as #110/#111).
///
/// `meets_withdraw_margin_requirement` values the source account through
/// `StrictOraclePrice`, whose bounds are the min and max of the live price and
/// this market's `last_oracle_price_twap_5min`. A liability is priced at the
/// upper bound, so dragging that TWAP down toward a temporarily depressed live
/// price under-values the debt, lets the margin check pass, and frees sibling
/// collateral for withdrawal. That leaves depositor-socialized debt once the
/// oracle recovers.
///
/// Interest accrual and the deposit/borrow/utilization TWAPs still advance.
/// Only the oracle TWAP and its timestamp are left alone, so the next real
/// refresh still weights the full elapsed interval. That TWAP keeps advancing
/// on every other spot path and through the permissionless
/// `update_spot_market_cumulative_interest` crank.
fn accrue_transfer_interest(maps: &mut AccountMaps, terms: &TransferTerms) -> Result<()> {
    let spot_market = &mut maps.spot_market_map.get_ref_mut(&terms.market_index)?;
    controller::spot_balance::update_spot_market_cumulative_interest(
        spot_market,
        None,
        terms.now,
        terms.funding_paused,
    )?;

    Ok(())
}

/// Mirror direct-withdraw's reduce-only cap on the source debit, so an internal
/// transfer cannot open or grow a borrow in a reduce-only spot market.
fn clamp_transfer_to_reduce_only(
    from_user: &mut User,
    maps: &mut AccountMaps,
    terms: &TransferTerms,
) -> Result<u64> {
    if !maps
        .spot_market_map
        .get_ref(&terms.market_index)?
        .is_reduce_only()
    {
        return Ok(terms.amount);
    }

    let position_index = from_user.force_get_spot_position_index(terms.market_index)?;
    validate!(
        from_user.spot_positions[position_index].balance_type == SpotBalanceType::Deposit,
        ErrorCode::ReduceOnlyWithdrawIncreasedRisk
    )?;

    let max_withdrawable_amount =
        calculate_max_withdrawable_amount(terms.market_index, from_user, maps)?;

    let spot_market = &maps.spot_market_map.get_ref(&terms.market_index)?;
    let existing_deposit_amount = from_user.spot_positions[position_index]
        .get_token_amount(spot_market)?
        .cast::<u64>()?;

    Ok(terms
        .amount
        .min(max_withdrawable_amount)
        .min(existing_deposit_amount))
}

/// Debit the source subaccount.
fn debit_transfer_source(
    parties: &mut TransferParties<'_>,
    maps: &mut AccountMaps,
    terms: &TransferTerms,
    amount: u64,
    oracle_price: i64,
) -> Result<()> {
    let spot_market = &mut maps.spot_market_map.get_ref_mut(&terms.market_index)?;

    validate!(
        parties.from_user.pool_id == spot_market.pool_id,
        ErrorCode::InvalidPoolId,
        "user pool id ({}) != market pool id ({})",
        parties.from_user.pool_id,
        spot_market.pool_id
    )?;

    parties.from_user.increment_total_withdraws(
        amount,
        oracle_price,
        spot_market.get_precision().cast()?,
    )?;

    // prevents withdraw when limits hit
    controller::spot_position::update_spot_balances_and_cumulative_deposits_with_limits(
        amount as u128,
        &SpotBalanceType::Borrow,
        spot_market,
        parties.from_user,
    )?;

    Ok(())
}

/// Record the debit of the source subaccount.
fn emit_transfer_source_record(
    parties: &TransferParties<'_>,
    maps: &mut AccountMaps,
    terms: &TransferTerms,
    amount: u64,
    oracle_price: i64,
) -> Result<()> {
    let spot_market = &mut maps.spot_market_map.get_ref_mut(&terms.market_index)?;

    emit_spot_balance_move(
        parties.from_user,
        parties.from_user_key,
        spot_market,
        SpotBalanceMove {
            ts: terms.now,
            direction: DepositDirection::Withdraw,
            amount,
            oracle_price,
            explanation: DepositExplanation::Transfer,
            transfer_user: Some(parties.to_user_key),
            signer: parties.signer,
            total_deposits_after: parties.from_user.total_deposits,
            total_withdraws_after: parties.from_user.total_withdraws,
        },
    )
}

/// Credit the destination position and prove the position it leaves is legal.
fn credit_transfer_position(
    to_user: &mut User,
    spot_market: &mut SpotMarket,
    amount: u64,
) -> Result<()> {
    let to_spot_position = to_user.force_get_spot_position_mut(spot_market.market_index)?;

    controller::spot_position::update_spot_balances_and_cumulative_deposits(
        amount as u128,
        &SpotBalanceType::Deposit,
        spot_market,
        to_spot_position,
        false,
        None,
    )?;

    let token_amount = to_spot_position.get_token_amount(spot_market)?;
    if token_amount == 0 {
        validate!(
            to_spot_position.scaled_balance == 0,
            ErrorCode::InvalidSpotPosition,
            "deposit left to_user with invalid position. scaled balance = {} token amount = {}",
            to_spot_position.scaled_balance,
            token_amount
        )?;
    }

    // Mirror direct-deposit admission: a positive deposit balance is only
    // permitted while the spot market is active.
    if to_spot_position.balance_type == SpotBalanceType::Deposit
        && to_spot_position.scaled_balance > 0
    {
        validate!(
            matches!(spot_market.status, MarketStatus::Active),
            ErrorCode::MarketActionPaused,
            "spot_market not active",
        )?;
    }

    Ok(())
}

/// Credit the destination subaccount, prove the position it leaves is legal,
/// and record the credit.
fn credit_transfer_destination(
    parties: &mut TransferParties<'_>,
    maps: &mut AccountMaps,
    terms: &TransferTerms,
    amount: u64,
    oracle_price: i64,
) -> Result<()> {
    let spot_market = &mut maps.spot_market_map.get_ref_mut(&terms.market_index)?;

    validate!(
        parties.to_user.pool_id == spot_market.pool_id,
        ErrorCode::InvalidPoolId,
        "user pool id ({}) != market pool id ({})",
        parties.to_user.pool_id,
        spot_market.pool_id
    )?;

    parties.to_user.increment_total_deposits(
        amount,
        oracle_price,
        spot_market.get_precision().cast()?,
    )?;

    let total_deposits_after = parties.to_user.total_deposits;
    let total_withdraws_after = parties.to_user.total_withdraws;

    credit_transfer_position(parties.to_user, spot_market, amount)?;

    emit_spot_balance_move(
        parties.to_user,
        parties.to_user_key,
        spot_market,
        SpotBalanceMove {
            ts: terms.now,
            direction: DepositDirection::Deposit,
            amount,
            oracle_price,
            explanation: DepositExplanation::Transfer,
            transfer_user: Some(parties.from_user_key),
            signer: parties.signer,
            total_deposits_after,
            total_withdraws_after,
        },
    )
}

/// Shared core for [`handle_transfer_deposit`] and
/// [`handle_transfer_deposit_by_delegate`].
///
/// Both instructions move an amount of a single spot market between two
/// same-authority subaccounts. They differ only in how the caller is authorized
/// (their preambles) and in the `signer` stamped on the emitted
/// `DepositRecord`s. The balance movement lives here so the two entrypoints can
/// never drift apart. That includes the admission checks that must mirror direct
/// `deposit` and `withdraw`: the reduce-only source cap, the recipient
/// active-status gate, and the `max_token_deposits` cap.
fn transfer_spot_deposit(
    parties: &mut TransferParties<'_>,
    terms: &TransferTerms,
    maps: &mut AccountMaps,
) -> anchor_lang::Result<()> {
    accrue_transfer_interest(maps, terms)?;

    let oracle_price = {
        let spot_market = &maps.spot_market_map.get_ref(&terms.market_index)?;
        maps.oracle_map
            .get_price_data(&spot_market.oracle_id())?
            .price
    };

    let amount = clamp_transfer_to_reduce_only(parties.from_user, maps, terms)?;

    debit_transfer_source(parties, maps, terms, amount, oracle_price)?;

    // OtterSec #135: same shape as `handle_withdraw`. This handler cranks only the
    // market being transferred, and the account's other borrow markets arrive
    // read-only, so their un-booked interest is missing from the check below.
    math::margin::validate_spot_borrow_interest_fresh_for_margin(
        parties.from_user,
        &maps.spot_market_map,
        terms.now,
    )?;

    parties
        .from_user
        .meets_withdraw_margin_requirement(maps, MarginRequirementType::Initial)?;

    validate_spot_margin_trading(parties.from_user, maps)?;

    if parties.from_user.is_cross_margin_being_liquidated() {
        parties.from_user.exit_cross_margin_liquidation();
    }

    parties.from_user.update_last_active_slot(terms.slot);

    emit_transfer_source_record(parties, maps, terms, amount, oracle_price)?;

    credit_transfer_destination(parties, maps, terms, amount, oracle_price)?;

    parties.to_user.update_last_active_slot(terms.slot);

    let spot_market = maps.spot_market_map.get_ref(&terms.market_index)?;
    math::spot_withdraw::validate_spot_market_vault_amount(
        &spot_market,
        terms.spot_market_vault_amount,
    )?;

    // Mirror direct-deposit admission: enforce the aggregate deposit cap after crediting.
    spot_market.validate_max_token_deposits_and_borrows(false)?;

    Ok(())
}

#[derive(Accounts)]
#[instruction(market_index: u16,)]
pub struct TransferDeposit<'info> {
    #[account(
        mut,
        has_one = authority,
    )]
    pub from_user: AccountLoader<'info, User>,
    #[account(
        mut,
        has_one = authority,
    )]
    pub to_user: AccountLoader<'info, User>,
    #[account(
        mut,
        has_one = authority
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(
        seeds = [b"spot_market_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
}

#[derive(Accounts)]
#[instruction(market_index: u16,)]
pub struct TransferDepositByDelegate<'info> {
    #[account(
        mut,
        has_one = delegate,
        constraint = !from_user.load()?.is_bankrupt() @ ErrorCode::UserBankrupt,
    )]
    pub from_user: AccountLoader<'info, User>,
    #[account(
        mut,
        has_one = delegate,
        constraint = to_user.load()?.authority == from_user.load()?.authority,
        constraint = !to_user.load()?.is_bankrupt() @ ErrorCode::UserBankrupt,
        constraint = to_user.key() != from_user.key() @ ErrorCode::CantTransferBetweenSameUserAccount,
    )]
    pub to_user: AccountLoader<'info, User>,
    #[account(
        constraint = is_stats_for_user(&from_user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub delegate: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(
        seeds = [b"spot_market_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
}
