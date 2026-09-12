//! Booking spot lending interest, and halting a short market.
//!
//! Interest accrues lazily: nothing is owed until somebody reads a balance, so
//! these cranks exist to keep a market's stored figures current for readers
//! that do not settle anything themselves.

use super::*;

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
    exchange_not_paused(&ctx.accounts.state)
    valid_oracle_for_spot_market(&ctx.accounts.oracle, &ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_cumulative_interest(
    ctx: Context<UpdateSpotMarketCumulativeInterest>,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let clock_slot = clock.slot;

    let mut oracle_map = OracleMap::load_one(
        &ctx.accounts.oracle,
        clock_slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let oracle_price_data = oracle_map.get_price_data(&spot_market.oracle_id())?;

    controller::spot_balance::update_spot_market_cumulative_interest(
        spot_market,
        Some(oracle_price_data),
        now,
        state.funding_paused()?,
    )?;

    math::spot_withdraw::validate_spot_market_vault_amount(
        spot_market,
        ctx.accounts.spot_market_vault.amount,
    )?;

    Ok(())
}

/// Permissionless batch refresh: book the lending interest of several spot markets in one
/// instruction. Markets and their indexes arrive through `remaining_accounts`, and each one goes
/// through the same `update_spot_market_cumulative_interest` as the single-market crank above.
///
/// Written for a caller that must value several markets in one transaction, such as a program
/// that prices a share against the markets a user holds. The single-market crank stays the
/// instruction that keeps a market's oracle EMA fresh.
///
/// This instruction moves no tokens and passes no oracle, which is why it drops two of the crank's
/// guards and its spot-vault assertion, and keeps the third:
///
/// - No oracle means `update_spot_market_twap_stats` leaves `historical_oracle_data` alone. A
///   caller that reads a market's oracle TWAP after this call therefore reads a value this call
///   did not move, and no caller can pick the sampling instant of an oracle EMA.
/// - A market status of `Delisted` is not rejected. `deposit` and `force_delete_user` already
///   book interest on a delisted market, so refusing here would block callers without stopping
///   the accrual.
/// - `exchange_not_paused` is kept. A full halt sets every `ExchangeStatus` bit, `FundingPaused`
///   included, so no interest can accrue and the only work left is stamping the clock and the
///   balance TWAPs. Those TWAPs size the withdraw and borrow circuit breakers, and a halt freezes
///   them for a reason. Without this guard a caller could re-baseline a breaker mid-halt, or stamp
///   the halted interval away so nobody is charged for it.
/// - The spot vault holds the same tokens after this call as before it, and booking interest can
///   only lower the depositors' claim, never raise it. Asserting the vault invariant here would
///   let one market that is already short abort the refresh of every other market in the batch.
#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_refresh_spot_market_interest<'c: 'info, 'info>(
    ctx: Context<'info, RefreshSpotMarketInterest<'info>>,
    args: RefreshSpotMarketInterestArgs,
) -> Result<()> {
    let RefreshSpotMarketInterestArgs { market_indexes } = args;
    // A user holds eight spot positions, and every perp market quotes the same spot market
    // (`initialize_perp_market` hardcodes it and no setter exists), so ten markets cover every
    // market one user's equity can read. The cap keeps one call inside a compute budget.
    validate!(
        market_indexes.len() <= 16,
        ErrorCode::DefaultError,
        "too many markets passed, max 16, got {}",
        market_indexes.len()
    )?;

    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;

    let writable_spot_markets = get_writable_spot_market_set_from_many(market_indexes);

    let maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &writable_spot_markets,
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    controller::spot_balance::refresh_spot_market_interest(
        &maps.spot_market_map,
        None,
        &writable_spot_markets,
        clock.unix_timestamp,
        state.funding_paused()?,
    )?;

    Ok(())
}

pub fn handle_pause_spot_market_deposit_withdraw(
    ctx: Context<PauseSpotMarketDepositWithdraw>,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;

    let result =
        validate_spot_market_vault_amount(spot_market, ctx.accounts.spot_market_vault.amount);

    validate!(
        matches!(result, Err(ErrorCode::SpotMarketVaultInvariantViolated)),
        ErrorCode::DefaultError,
        "spot market vault amount is valid"
    )?;

    spot_market.paused_operations |= SpotOperation::Deposit as u8;
    spot_market.paused_operations |= SpotOperation::Withdraw as u8;

    Ok(())
}

#[derive(Accounts)]
pub struct UpdateSpotMarketCumulativeInterest<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    /// CHECK: checked in `update_spot_market_cumulative_interest` ix constraint
    pub oracle: UncheckedAccount<'info>,
    #[account(
        seeds = [b"spot_market_vault".as_ref(), spot_market.load()?.market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
}

/// The markets to refresh arrive as writable spot market accounts in `remaining_accounts`.
/// `SpotMarketMap` reads each market's index out of the account it loads, so a market is refreshed
/// only when its own account is passed.
#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct RefreshSpotMarketInterestArgs {
    pub market_indexes: Vec<u16>,
}

#[derive(Accounts)]
pub struct RefreshSpotMarketInterest<'info> {
    pub state: AccountLoader<'info, State>,
}

#[derive(Accounts)]
pub struct PauseSpotMarketDepositWithdraw<'info> {
    pub state: AccountLoader<'info, State>,
    pub keeper: Signer<'info>,
    #[account(mut)]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        seeds = [b"spot_market_vault".as_ref(), spot_market.load()?.market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
}
