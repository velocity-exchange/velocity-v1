//! Sweeping a perp market's accrued fee carveouts out of its pnl pool.

use super::*;

/// Permissionless streaming sweep: materialize a perp market's accrued
/// pending fee carveouts out of the pnl pool — `pending_protocol_fee` to the
/// market's `protocol_fee_pool` (buffer-exempt, runs first), then
/// `pending_if_fee` to the quote spot market's `revenue_pool` and
/// `pending_amm_provision` tokenized into `amm.fee_pool` (both leave
/// `fee_pool_buffer_target` behind). Every drain reserves
/// `max(net_user_pnl, 0)` so user claims stay backed. The same sweep runs
/// inline on every pnl settle (`update_pool_balances`); this instruction lets
/// keepers run it on demand without settling anyone's pnl.
#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_sweep_perp_market_fees(
    ctx: Context<SweepPerpMarketFees>,
    perp_market_index: u16,
) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let state = ctx.accounts.state.load()?;

    // account identities (market index, quote spot market, oracle) are
    // enforced by the SweepPerpMarketFees constraints
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;

    let mut oracle_map = OracleMap::load_one(
        &ctx.accounts.oracle,
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    // The reserve keeps the live user claim in the pnl pool. `get_pnl_pool_drain_reserve_price`
    // picks the price and validates the oracle. The revenue-share sweep uses the same function, so
    // both drains value the same claim at the same price.
    let reserve_price = controller::perp_pools::get_pnl_pool_drain_reserve_price(
        perp_market,
        &state,
        &mut oracle_map,
    )?;

    controller::spot_balance::update_spot_market_cumulative_interest(
        spot_market,
        None,
        now,
        state.funding_paused()?,
    )?;

    let net_user_pnl = calculate_net_user_pnl(
        &perp_market.amm,
        reserve_price,
        perp_market.quote_asset_amount,
        perp_market.net_unsettled_funding_pnl,
    )?;

    let (if_swept, protocol_swept, amm_provision_tokenized) =
        controller::perp_pools::sweep_market_fees(
            perp_market,
            spot_market,
            net_user_pnl,
            now,
            false,
        )?;

    msg!(
        "swept perp market {} fees: if={} protocol={} amm_provision_tokenized={}",
        perp_market_index,
        if_swept,
        protocol_swept,
        amm_provision_tokenized
    );

    Ok(())
}

#[derive(Accounts)]
#[instruction(perp_market_index: u16,)]
pub struct SweepPerpMarketFees<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        seeds = [b"perp_market", perp_market_index.to_le_bytes().as_ref()],
        bump,
        has_one = oracle @ ErrorCode::InvalidOracle,
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// The perp market's quote spot market (enforced by the PDA derivation)
    #[account(
        mut,
        seeds = [b"spot_market", perp_market.load()?.quote_spot_market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    /// CHECK: must be `perp_market.oracle` (enforced by `has_one` above)
    pub oracle: UncheckedAccount<'info>,
}
