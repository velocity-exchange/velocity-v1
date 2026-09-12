//! Retiring a market and draining its pools.
//!
//! An expiry sets the timestamp a market stops trading at.
//! [`handle_settle_expired_market`] then prices the settlement, and
//! [`handle_settle_expired_market_pools_to_revenue_pool`] moves what is left to
//! the revenue pool and delists the market. Every user claim must be wound down
//! first, which is what the validations in that handler prove.

use super::*;

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_expiry(
    ctx: Context<AdminUpdateSpotMarket>,
    expiry_ts: i64,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("updating spot market {} expiry", spot_market.market_index);
    let now = Clock::get()?.unix_timestamp;

    validate!(
        now < expiry_ts,
        ErrorCode::DefaultError,
        "Market expiry ts must later than current clock timestamp"
    )?;

    msg!(
        "spot_market.status {:?} -> {:?}",
        spot_market.status,
        MarketStatus::ReduceOnly
    );
    msg!(
        "spot_market.expiry_ts {} -> {}",
        spot_market.expiry_ts,
        expiry_ts
    );

    // automatically enter reduce only
    spot_market.status = MarketStatus::ReduceOnly;
    spot_market.expiry_ts = expiry_ts;

    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_expiry(
    ctx: Context<AdminUpdatePerpMarket>,
    expiry_ts: i64,
) -> Result<()> {
    let clock: Clock = Clock::get()?;
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("updating perp market {} expiry", perp_market.market_index);

    validate!(
        clock.unix_timestamp < expiry_ts,
        ErrorCode::DefaultError,
        "Market expiry ts must later than current clock timestamp"
    )?;

    msg!(
        "perp_market.status {:?} -> {:?}",
        perp_market.status,
        MarketStatus::ReduceOnly
    );
    msg!(
        "perp_market.expiry_ts {} -> {}",
        perp_market.expiry_ts,
        expiry_ts
    );

    // automatically enter reduce only
    perp_market.status = MarketStatus::ReduceOnly;
    perp_market.expiry_ts = expiry_ts;

    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_settle_expired_market_pools_to_revenue_pool(
    ctx: Context<SettleExpiredMarketPoolsToRevenuePool>,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    let spot_market: &mut std::cell::RefMut<'_, SpotMarket> =
        &mut load_mut!(ctx.accounts.spot_market)?;
    let state = ctx.accounts.state.load()?;

    msg!(
        "settling expired market pools to revenue pool for perp market {}",
        perp_market.market_index
    );

    msg!(
        "settling expired market pools to revenue pool for spot market {}",
        spot_market.market_index
    );

    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    controller::spot_balance::update_spot_market_cumulative_interest(
        spot_market,
        None,
        now,
        state.funding_paused()?,
    )?;

    validate!(
        spot_market.market_index == QUOTE_SPOT_MARKET_INDEX,
        ErrorCode::DefaultError,
        "spot_market must be perp market's quote asset"
    )?;

    validate!(
        perp_market.status == MarketStatus::Settlement,
        ErrorCode::DefaultError,
        "Market must in Settlement"
    )?;

    validate_perp_market_wound_down(perp_market)?;

    validate_delist_escrow_elapsed(perp_market, &state, now)?;

    validate_revenue_share_settled(perp_market)?;

    drain_market_pools_to_revenue_pool(perp_market, spot_market, now)?;

    perp_market.status = MarketStatus::Delisted;

    Ok(())
}

/// Holds a delist to a market that carries no user position.
///
/// The pools go to the revenue pool with nothing reserved, so every user claim
/// on them must already be zero. Base and quote must be balanced, the AMM must
/// hold no base, and no bankruptcy claim may still be open. A latched bankrupt
/// nets against another user's unsettled claim in the quote sum, so the claim
/// counter is checked on its own.
///
/// Two permissionless paths take the counter to zero. `resolve_perp_bankruptcy`
/// absorbs the debt through the bankruptcy waterfall. `settle_pnl` releases the
/// claim once the position's quote reaches zero. The market stays in Settlement
/// while they run.
fn validate_perp_market_wound_down(perp_market: &PerpMarket) -> Result<()> {
    validate!(
        perp_market.base_asset_amount_long == 0
            && perp_market.base_asset_amount_short == 0
            && perp_market.number_of_users_with_base == 0,
        ErrorCode::DefaultError,
        "outstanding base_asset_amounts must be balanced {} {} {}",
        perp_market.base_asset_amount_long,
        perp_market.base_asset_amount_short,
        perp_market.number_of_users_with_base
    )?;

    validate!(
        crate::vlp::amm::math::amm::calculate_net_user_cost_basis(
            perp_market.quote_asset_amount,
            perp_market.net_unsettled_funding_pnl,
        )? == 0,
        ErrorCode::DefaultError,
        "outstanding quote_asset_amounts must be balanced"
    )?;

    validate!(
        perp_market.pending_bankruptcy_claims == 0,
        ErrorCode::DefaultError,
        "perp market {} still holds {} unresolved bankruptcy claims; resolve them before delisting",
        perp_market.market_index,
        perp_market.pending_bankruptcy_claims
    )?;

    // With user base, AMM base, and net user cost basis all wound down,
    // net_user_pnl is identically 0 — no live user claim remains on the pnl
    // pool. This is what lets the final sweep below (and the full pnl-pool
    // drain to the revenue pool) reserve nothing for users without consulting
    // an oracle.
    validate!(
        perp_market.amm.base_asset_amount_with_amm == 0,
        ErrorCode::DefaultError,
        "amm base_asset_amount_with_amm must be balanced ({})",
        perp_market.amm.base_asset_amount_with_amm
    )?;

    Ok(())
}

/// Holds a delist until the escrow period after expiry has passed.
///
/// The period gives every user time to settle. A `settlement_duration` of zero
/// is an unconfigured exchange, not a zero wait.
fn validate_delist_escrow_elapsed(perp_market: &PerpMarket, state: &State, now: i64) -> Result<()> {
    // block when settlement_duration is default/unconfigured
    validate!(
        state.settlement_duration != 0,
        ErrorCode::DefaultError,
        "invalid state.settlement_duration (is 0)"
    )?;

    let escrow_period_before_transfer = state.escrow_period_before_transfer()?;

    validate!(
        now > perp_market
            .expiry_ts
            .safe_add(escrow_period_before_transfer)?,
        ErrorCode::DefaultError,
        "must be escrow_period_before_transfer={} after market.expiry_ts",
        escrow_period_before_transfer
    )?;

    Ok(())
}

/// Holds a delist until the market owes no builder or referrer fee.
///
/// The expiry solver values a winner's claim against `pnl_pool` less
/// `pending_revenue_share`, so the pool still holds the tokens the counter
/// names. A delist with a non-zero counter would give those earned fees of
/// third parties to the revenue pool.
///
/// The rule has no time limit, because every row has an end state.
/// `settle_revenue_share` pays a payable row, and in Settlement it needs no
/// help from the escrow owner. `forfeit_revenue_share_order` writes off a row
/// that the program cannot pay. Anyone can call both.
///
/// The admin holds one exception. While the `BuilderCodes` feature bit is off,
/// `settle_revenue_share` fails and the sweep skips builder rows. A row that
/// the pool can pay is then neither payable nor forfeitable, and this check
/// holds. The admin enables the bit again to close the market, so this is an
/// order of operations and not a way for another party to block a delist.
fn validate_revenue_share_settled(perp_market: &PerpMarket) -> Result<()> {
    validate!(
        perp_market.pending_revenue_share == 0,
        ErrorCode::UnsettledRevenueShareOnDelist,
        "perp market {} still owes {} of builder/referrer revenue share; run settle_revenue_share for every escrow still owed, and forfeit_revenue_share_order for any row that provably cannot be paid",
        perp_market.market_index,
        perp_market.pending_revenue_share
    )?;

    Ok(())
}

/// Moves what is left of a wound-down market to the revenue pool.
///
/// The fee sweep runs first, because the pnl pool still holds the un-swept fee
/// value. Without it the protocol fee carveout would reach the revenue pool and
/// the insurance fund instead of the withdrawable protocol fee pool, and no
/// sweep can run once the market is Delisted. Net user PnL is zero by the checks
/// above, so the whole surplus is available and `force = true` overrides a
/// standing sweep pause. This is the last sweep the market ever gets.
fn drain_market_pools_to_revenue_pool(
    perp_market: &mut PerpMarket,
    spot_market: &mut SpotMarket,
    now: i64,
) -> Result<()> {
    controller::perp_pools::sweep_market_fees(perp_market, spot_market, 0, now, true)?;

    let fee_pool_token_amount = perp_market.amm.fee_pool_token_amount(spot_market)?;
    let pnl_pool_token_amount = get_token_amount(
        perp_market.pnl_pool.scaled_balance,
        spot_market,
        &SpotBalanceType::Deposit,
    )?;

    <crate::vlp::amm::AMM as crate::vlp::amm::quoter::AmmContract>::withdraw_from_fee_pool(
        &mut perp_market.amm,
        fee_pool_token_amount,
        spot_market,
        false,
    )?;

    controller::spot_balance::update_spot_balances(
        pnl_pool_token_amount,
        &SpotBalanceType::Borrow,
        spot_market,
        &mut perp_market.pnl_pool,
        false,
    )?;

    controller::spot_balance::update_revenue_pool_balances(
        pnl_pool_token_amount.safe_add(fee_pool_token_amount)?,
        &SpotBalanceType::Deposit,
        spot_market,
        false,
    )?;

    math::spot_withdraw::validate_spot_balances(spot_market)?;

    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_pnl_pool<'c: 'info, 'info>(
    ctx: Context<'info, UpdatePerpMarketPnlPool<'info>>,
    amount: u64,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;

    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;

    controller::spot_balance::update_spot_balances(
        amount.cast::<u128>()?,
        &SpotBalanceType::Deposit,
        spot_market,
        &mut perp_market.pnl_pool,
        false,
    )?;

    validate_spot_market_vault_amount(spot_market, ctx.accounts.spot_market_vault.amount)?;

    msg!(
        "updating perp market {} pnl pool with amount {}",
        perp_market.market_index,
        amount
    );

    Ok(())
}

pub fn handle_settle_expired_market<'c: 'info, 'info>(
    ctx: Context<'info, AdminUpdatePerpMarket<'info>>,
    market_index: u16,
) -> Result<()> {
    let clock = Clock::get()?;
    let _now = clock.unix_timestamp;
    let state = ctx.accounts.state.load()?;

    let mut maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &get_writable_perp_market_set(market_index),
        &get_writable_spot_market_set(QUOTE_SPOT_MARKET_INDEX),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    // Refresh PerpMarket-level oracle stats only — settle_expired_market
    // reads `market.market_stats.historical_oracle_data` for the expiry
    // price, not AMM peg or reserves. The AMM refresh that used to fire
    // here was cargo-cult.
    {
        let mut perp_market = maps.perp_market_map.get_ref_mut(&market_index)?;
        let oracle_price_data = maps.oracle_map.get_price_data(&perp_market.oracle_id())?;
        let mm_oracle_price_data = perp_market.get_mm_oracle_price_data(
            *oracle_price_data,
            clock.slot,
            &state.oracle_guard_rails.validity,
            state.slot_clock(),
        )?;
        let validity = crate::vlp::amm::refresh::compute_amm_refresh_validity(
            &perp_market,
            &mm_oracle_price_data,
            &state,
            clock.slot,
        )?;
        perp_market.update_oracle_derived_stats(
            &mm_oracle_price_data,
            validity,
            clock.unix_timestamp,
            clock.slot,
            state.slot_clock(),
        )?;
    }

    crate::vlp::amm::refresh::settle_expired_market(market_index, &mut maps, &state, &clock)?;

    Ok(())
}

#[derive(Accounts)]
pub struct SettleExpiredMarketPoolsToRevenuePool<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(
        seeds = [b"spot_market", 0_u16.to_le_bytes().as_ref()],
        bump,
        mut
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
}

#[derive(Accounts)]
pub struct UpdatePerpMarketPnlPool<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(
        seeds = [b"spot_market", 0_u16.to_le_bytes().as_ref()],
        bump,
        mut
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), 0_u16.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
}
