//! Which oracle prices a market, and how stale its price may be.
//!
//! A market oracle swap reads both the old and the new price and holds the
//! change to a bound, so a bad account cannot move every position at once.
//! [`validated_oracle_swap`] is the one copy of that rule, shared by the spot
//! and the perp handler.
//!
//! Prelaunch oracles are in [`super::prelaunch_oracle`]. The MM oracle crank is
//! in [`super::mm_oracle`].

use super::*;

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_oracle(
    ctx: Context<AdminUpdateSpotMarketOracle>,
    oracle: Pubkey,
    oracle_source: OracleSource,
    skip_invariant_check: bool,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("updating spot market {} oracle", spot_market.market_index);
    let clock = Clock::get()?;

    validate_new_oracle_account(&ctx.accounts.oracle, oracle, oracle_source)?;

    validate!(
        ctx.accounts.old_oracle.key == &spot_market.oracle,
        ErrorCode::DefaultError,
        "old oracle account info ({:?}) and spot market oracle ({:?}) must match",
        ctx.accounts.old_oracle.key,
        spot_market.oracle
    )?;

    // Verify oracle is readable
    let OraclePriceData {
        price: new_oracle_price,
        ..
    } = get_oracle_price(&oracle_source, &ctx.accounts.oracle, clock.slot)?;

    msg!(
        "spot_market.oracle {:?} -> {:?}",
        spot_market.oracle,
        oracle
    );

    msg!(
        "spot_market.oracle_source {:?} -> {:?}",
        spot_market.oracle_source,
        oracle_source
    );

    let OraclePriceData {
        price: old_oracle_price,
        ..
    } = get_oracle_price(
        &spot_market.oracle_source,
        &ctx.accounts.old_oracle,
        clock.slot,
    )?;

    msg!(
        "Oracle Price: {:?} -> {:?}",
        old_oracle_price,
        new_oracle_price
    );

    if !skip_invariant_check {
        validate_oracle_price_change(old_oracle_price, new_oracle_price)?;
    }

    spot_market.oracle = oracle;
    spot_market.oracle_source = oracle_source;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_oracle(
    ctx: Context<AdminUpdatePerpMarketOracle>,
    oracle: Pubkey,
    oracle_source: OracleSource,
    skip_invariant_check: bool,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    let amm_cache = &mut ctx.accounts.amm_cache;
    msg!("perp market {}", perp_market.market_index);

    let clock = Clock::get()?;

    validate_new_oracle_account(&ctx.accounts.oracle, oracle, oracle_source)?;

    validate!(
        ctx.accounts.old_oracle.key == &perp_market.oracle,
        ErrorCode::DefaultError,
        "old oracle account info ({:?}) and perp market oracle ({:?}) must match",
        ctx.accounts.old_oracle.key,
        perp_market.oracle
    )?;

    // Verify new oracle is readable
    let OraclePriceData {
        price: new_oracle_price,
        delay: _oracle_delay,
        ..
    } = get_oracle_price(&oracle_source, &ctx.accounts.oracle, clock.slot)?;

    msg!(
        "perp_market.oracle: {:?} -> {:?}",
        perp_market.oracle,
        oracle
    );

    msg!(
        "perp_market.oracle_source: {:?} -> {:?}",
        perp_market.oracle_source,
        oracle_source
    );

    let OraclePriceData {
        price: old_oracle_price,
        ..
    } = get_oracle_price(
        &perp_market.oracle_source,
        &ctx.accounts.old_oracle,
        clock.slot,
    )?;

    msg!(
        "Oracle Price: {:?} -> {:?}",
        old_oracle_price,
        new_oracle_price
    );

    if !skip_invariant_check {
        validate_oracle_price_change(old_oracle_price, new_oracle_price)?;
    }

    perp_market.oracle = oracle;
    perp_market.oracle_source = oracle_source;

    refresh_amm_cache_oracle(amm_cache, perp_market)
}

/// Holds the account an oracle swap names to the instruction data.
///
/// The source must be one the program still reads, the account must parse as
/// that source, and it must be the account the caller named. A caller that
/// passes the wrong account would otherwise re-price the whole market.
fn validate_new_oracle_account(
    oracle_account: &AccountInfo,
    oracle: Pubkey,
    oracle_source: OracleSource,
) -> Result<()> {
    validate_supported_market_oracle_source(oracle_source)?;

    OracleMap::validate_oracle_account_info(oracle_account)?;

    validate!(
        oracle_account.key == &oracle,
        ErrorCode::DefaultError,
        "oracle account info ({:?}) and ix data ({:?}) must match",
        oracle_account.key,
        oracle
    )?;

    Ok(())
}

/// Holds an oracle swap to a small price change.
///
/// A new feed prices the same asset, so it must agree with the old one to
/// within ten percent. A larger step is the wrong feed, and it would move every
/// position at once.
fn validate_oracle_price_change(old_oracle_price: i64, new_oracle_price: i64) -> Result<()> {
    validate!(
        new_oracle_price > 0,
        ErrorCode::DefaultError,
        "invalid oracle price, must be greater than 0"
    )?;

    let oracle_change_divergence = new_oracle_price
        .safe_sub(old_oracle_price)?
        .safe_mul(PERCENTAGE_PRECISION_I64)?
        .safe_div(old_oracle_price)?;

    validate!(
        oracle_change_divergence.abs() < (PERCENTAGE_PRECISION_I64 / 10),
        ErrorCode::DefaultError,
        "invalid new oracle price, more than 10% divergence"
    )?;

    Ok(())
}

/// Copies the market's new oracle into its AMM cache row. A market with no row
/// keeps none.
fn refresh_amm_cache_oracle(amm_cache: &mut AmmCache, perp_market: &PerpMarket) -> Result<()> {
    if amm_cache
        .cache
        .iter()
        .any(|cache_info| cache_info.market_index == perp_market.market_index)
    {
        amm_cache.update_perp_market_fields(perp_market)?;
    }

    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_oracle_low_risk_slot_delay_override(
    ctx: Context<HotAdminUpdatePerpMarket>,
    oracle_low_risk_slot_delay_override: i8,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    msg!(
        "perp_market.oracle_low_risk_slot_delay_override: {:?} -> {:?}",
        perp_market.oracle_low_risk_slot_delay_override,
        oracle_low_risk_slot_delay_override
    );

    perp_market.oracle_low_risk_slot_delay_override = oracle_low_risk_slot_delay_override;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_oracle_slot_delay_override(
    ctx: Context<HotAdminUpdatePerpMarket>,
    oracle_slot_delay_override: i8,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    msg!(
        "perp_market.oracle_slot_delay_override: {:?} -> {:?}",
        perp_market.oracle_slot_delay_override,
        oracle_slot_delay_override
    );

    perp_market.oracle_slot_delay_override = oracle_slot_delay_override;
    Ok(())
}

pub fn handle_initialize_pyth_lazer_oracle(
    ctx: Context<InitPythLazerOracle>,
    feed_id: u32,
) -> Result<()> {
    let pubkey = ctx.accounts.lazer_oracle.to_account_info().key;
    msg!(
        "Lazer price feed initted {} with feed_id {}",
        pubkey,
        feed_id
    );
    Ok(())
}

pub fn handle_zero_mm_oracle_fields(ctx: Context<HotAdminUpdatePerpMarket>) -> Result<()> {
    let mut perp_market = load_mut!(ctx.accounts.perp_market)?;
    perp_market.market_stats.mm_oracle_price = 0;
    perp_market.market_stats.mm_oracle_sequence_id = 0;
    perp_market.market_stats.mm_oracle_slot = 0;
    Ok(())
}

#[derive(Accounts)]
pub struct AdminUpdateSpotMarketOracle<'info> {
    // cold-only: a lesser admin swapping the oracle could re-price the
    // withdraw guard threshold notional cap (and all margin math) at will
    #[account(constraint = check_cold(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    /// CHECK: checked in `initialize_spot_market`
    pub oracle: UncheckedAccount<'info>,
    /// CHECK: checked in `admin_update_spot_market_oracle` ix constraint
    pub old_oracle: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct AdminUpdatePerpMarketOracle<'info> {
    // cold-only: see AdminUpdateSpotMarketOracle
    #[account(constraint = check_cold(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// CHECK: checked in `admin_update_perp_market_oracle` ix constraint
    pub oracle: UncheckedAccount<'info>,
    /// CHECK: checked in `admin_update_perp_market_oracle` ix constraint
    pub old_oracle: UncheckedAccount<'info>,
    #[account(
        mut,
        seeds = [AMM_POSITIONS_CACHE.as_bytes()],
        bump = amm_cache.bump,
    )]
    pub amm_cache: Box<Account<'info, AmmCache>>,
}

#[derive(Accounts)]
#[instruction(feed_id: u32)]
pub struct InitPythLazerOracle<'info> {
    #[account(mut, constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(init, seeds = [PYTH_LAZER_ORACLE_SEED, &feed_id.to_le_bytes()],
        space=PythLazerOracle::SIZE,
        bump,
        payer=admin
    )]
    pub lazer_oracle: AccountLoader<'info, PythLazerOracle>,
    pub state: AccountLoader<'info, State>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}
