//! Refreshing the AMMs and the AMM cache.

use super::*;

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_update_amms<'c: 'info, 'info>(
    ctx: Context<'info, UpdateAMM<'info>>,
    market_indexes: Vec<u16>,
) -> Result<()> {
    validate!(
        market_indexes.len() <= 5,
        ErrorCode::TooManyMarketsPassed,
        "Too many markets passed, max 5"
    )?;

    // up to ~60k compute units (per amm) worst case

    let clock = Clock::get()?;

    let state = ctx.accounts.state.load()?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        &get_market_set_from_list(market_indexes),
        &MarketSet::new(),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    crate::vlp::amm::refresh::update_amms(
        &mut maps.perp_market_map,
        &mut maps.oracle_map,
        &state,
        &clock,
    )?;

    Ok(())
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn view_amm_liquidity<'c: 'info, 'info>(
    ctx: Context<'info, UpdateAMM<'info>>,
    market_indexes: Vec<u16>,
) -> Result<()> {
    validate!(
        market_indexes.len() <= 5,
        ErrorCode::TooManyMarketsPassed,
        "Too many markets passed, max 5"
    )?;

    // up to ~60k compute units (per amm) worst case

    let clock = Clock::get()?;

    let state = ctx.accounts.state.load()?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let oracle_map = &mut OracleMap::load(
        remaining_accounts_iter,
        clock.slot,
        state.slot_clock(),
        None,
    )?;
    let market_map = &mut PerpMarketMap::load(
        &get_market_set_from_list(market_indexes),
        remaining_accounts_iter,
    )?;

    crate::vlp::amm::refresh::update_amms(market_map, oracle_map, &state, &clock)?;

    for (_key, market_account_loader) in market_map.0.iter_mut() {
        let market = &mut load_mut!(market_account_loader)?;
        let oracle_price_data = &oracle_map.get_price_data(&market.oracle_id())?;

        // `update_amms` above refreshed each AMM's cached spread state; read
        // it back for the dlog.
        let reserve_price = market.amm.reserve_price()?;
        let (bid, ask) = market.amm.bid_ask_price(
            reserve_price,
            market.amm.long_spread,
            market.amm.short_spread,
            market.amm.reference_price_offset,
        )?;

        crate::dlog!(bid, ask, oracle_price_data.price);
    }

    Ok(())
}

pub fn handle_update_amm_cache<'c: 'info, 'info>(
    ctx: Context<'info, UpdateAmmCache<'info>>,
) -> Result<()> {
    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut amm_cache: AccountZeroCopyMut<'_, CacheInfo, _> =
        ctx.accounts.amm_cache.load_zc_mut()?;

    let state = ctx.accounts.state.load()?;
    let quote_market = ctx.accounts.quote_market.load()?;

    let mut maps = load_maps(
        remaining_accounts_iter,
        &MarketSet::new(),
        &MarketSet::new(),
        Clock::get()?.slot,
        state.slot_clock(),
        None,
    )?;
    let slot = Clock::get()?.slot;

    for (_, perp_market_loader) in maps.perp_market_map.0.iter() {
        let perp_market = perp_market_loader.load()?;
        if perp_market.hedge_config.status == 0 {
            continue;
        }

        let oracle_data = *maps.oracle_map.get_price_data(&perp_market.oracle_id())?;
        refresh_cached_market(
            amm_cache.get_for_market_index_mut(perp_market.market_index)?,
            &perp_market,
            &state,
            oracle_data,
            slot,
        )?;

        if !PerpLpOperation::is_operation_paused(
            perp_market.hedge_config.paused_operations,
            PerpLpOperation::TrackAmmRevenue,
        ) {
            amm_cache.update_amount_owed_from_lp_pool(&perp_market, &quote_market)?;
        }
    }

    Ok(())
}

/// Copy one perp market's hedge-relevant fields and oracle reading into the
/// cache row the LP pool reads.
fn refresh_cached_market(
    cached_info: &mut CacheInfo,
    perp_market: &PerpMarket,
    state: &State,
    oracle_data: crate::state::oracle::OraclePriceData,
    slot: u64,
) -> Result<()> {
    validate!(
        perp_market.oracle_id() == cached_info.oracle_id()?,
        ErrorCode::InvalidOracle,
        "oracle id mismatch between amm cache and perp market"
    )?;

    let mm_oracle_price_data = perp_market.get_mm_oracle_price_data(
        oracle_data,
        slot,
        &state.oracle_guard_rails.validity,
        state.slot_clock(),
    )?;

    cached_info.update_perp_market_fields(perp_market)?;
    cached_info.try_update_oracle_info(
        slot,
        &mm_oracle_price_data,
        perp_market,
        &state.oracle_guard_rails,
        state.slot_clock(),
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct UpdateAmmCache<'info> {
    #[account(mut)]
    pub keeper: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    /// CHECK: checked in AmmCacheZeroCopy checks
    #[account(mut)]
    pub amm_cache: UncheckedAccount<'info>,
    #[account(
        owner = crate::ID,
        seeds = [b"spot_market", QUOTE_SPOT_MARKET_INDEX.to_le_bytes().as_ref()],
        bump,
    )]
    pub quote_market: AccountLoader<'info, SpotMarket>,
}

#[derive(Accounts)]
pub struct UpdateAMM<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
}
