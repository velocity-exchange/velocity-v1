//! The deposit and borrow limits of a spot market.
//!
//! Each handler moves one limit: the margin weights, the interest rate curve,
//! the deposit and borrow ceilings, and the withdraw guards that bound how fast
//! a market can drain.

use super::*;

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_withdraw_guard_threshold(
    ctx: Context<AdminUpdateSpotMarketWithdrawGuardThreshold>,
    withdraw_guard_threshold: u64,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!(
        "updating spot market withdraw guard threshold {}",
        spot_market.market_index
    );

    let oracle_price = get_oracle_price(
        &spot_market.oracle_source,
        &ctx.accounts.oracle,
        Clock::get()?.slot,
    )?
    .price;

    // price the notional cap with the max of the live price and the 5min
    // twap so a momentarily manipulated-down oracle can't let an oversized
    // threshold through
    let strict_oracle_price = StrictOraclePrice::new(
        oracle_price,
        spot_market
            .historical_oracle_data
            .last_oracle_price_twap_5min,
        true,
    );
    strict_oracle_price.validate()?;

    validate_withdraw_guard_threshold(
        withdraw_guard_threshold,
        spot_market.decimals,
        strict_oracle_price.max(),
    )?;

    msg!(
        "spot_market.withdraw_guard_threshold: {:?} -> {:?}",
        spot_market.withdraw_guard_threshold,
        withdraw_guard_threshold
    );
    spot_market.withdraw_guard_threshold = withdraw_guard_threshold;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_margin_weights(
    ctx: Context<AdminUpdateSpotMarket>,
    initial_asset_weight: u32,
    maintenance_asset_weight: u32,
    initial_liability_weight: u32,
    maintenance_liability_weight: u32,
    imf_factor: u32,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    validate_margin_weights(
        spot_market.market_index,
        initial_asset_weight,
        maintenance_asset_weight,
        initial_liability_weight,
        maintenance_liability_weight,
        imf_factor,
    )?;

    msg!(
        "spot_market.initial_asset_weight: {:?} -> {:?}",
        spot_market.initial_asset_weight,
        initial_asset_weight
    );

    msg!(
        "spot_market.maintenance_asset_weight: {:?} -> {:?}",
        spot_market.maintenance_asset_weight,
        maintenance_asset_weight
    );

    msg!(
        "spot_market.initial_liability_weight: {:?} -> {:?}",
        spot_market.initial_liability_weight,
        initial_liability_weight
    );

    msg!(
        "spot_market.maintenance_liability_weight: {:?} -> {:?}",
        spot_market.maintenance_liability_weight,
        maintenance_liability_weight
    );

    msg!(
        "spot_market.imf_factor: {:?} -> {:?}",
        spot_market.imf_factor,
        imf_factor
    );

    spot_market.initial_asset_weight = initial_asset_weight;
    spot_market.maintenance_asset_weight = maintenance_asset_weight;
    spot_market.initial_liability_weight = initial_liability_weight;
    spot_market.maintenance_liability_weight = maintenance_liability_weight;
    spot_market.imf_factor = imf_factor;

    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_borrow_rate(
    ctx: Context<AdminUpdateSpotMarket>,
    optimal_utilization: u32,
    optimal_borrow_rate: u32,
    max_borrow_rate: u32,
    min_borrow_rate: Option<u8>,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    validate_borrow_rate(
        optimal_utilization,
        optimal_borrow_rate,
        max_borrow_rate,
        min_borrow_rate
            .unwrap_or(spot_market.min_borrow_rate)
            .cast::<u32>()?
            * ((PERCENTAGE_PRECISION / 200) as u32),
    )?;

    msg!(
        "spot_market.optimal_utilization: {:?} -> {:?}",
        spot_market.optimal_utilization,
        optimal_utilization
    );

    msg!(
        "spot_market.optimal_borrow_rate: {:?} -> {:?}",
        spot_market.optimal_borrow_rate,
        optimal_borrow_rate
    );

    msg!(
        "spot_market.max_borrow_rate: {:?} -> {:?}",
        spot_market.max_borrow_rate,
        max_borrow_rate
    );

    spot_market.optimal_utilization = optimal_utilization;
    spot_market.optimal_borrow_rate = optimal_borrow_rate;
    spot_market.max_borrow_rate = max_borrow_rate;

    if let Some(min_borrow_rate) = min_borrow_rate {
        msg!(
            "spot_market.min_borrow_rate: {:?} -> {:?}",
            spot_market.min_borrow_rate,
            min_borrow_rate
        );
        spot_market.min_borrow_rate = min_borrow_rate
    }

    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_max_token_deposits(
    ctx: Context<AdminUpdateSpotMarket>,
    max_token_deposits: u64,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    msg!(
        "spot_market.max_token_deposits: {:?} -> {:?}",
        spot_market.max_token_deposits,
        max_token_deposits
    );

    spot_market.max_token_deposits = max_token_deposits;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_withdraw_circuit_breaker(
    ctx: Context<AdminUpdateSpotMarket>,
    withdraw_circuit_breaker_bps: u16,
) -> Result<()> {
    validate!(
        withdraw_circuit_breaker_bps <= BPS_PRECISION as u16,
        ErrorCode::DefaultError,
        "withdraw_circuit_breaker_bps ({} bps) must be <= 100% ({} bps)",
        withdraw_circuit_breaker_bps,
        BPS_PRECISION
    )?;

    // A higher pct loosens the breaker (allows a larger daily withdrawal). The
    // warm admin may only keep or tighten it relative to the 25% default;
    // loosening it past 25% is a riskier change reserved for the cold admin.
    // (`0` is the default-25% sentinel, so it stays within the warm cap.)
    if !check_cold(&ctx.accounts.admin.key(), &ctx.accounts.state)? {
        validate!(
            withdraw_circuit_breaker_bps <= DEFAULT_WITHDRAW_CIRCUIT_BREAKER_BPS,
            ErrorCode::Unauthorized,
            "warm admin cannot set withdraw_circuit_breaker_bps ({}) above the 25% default ({}); requires cold admin",
            withdraw_circuit_breaker_bps,
            DEFAULT_WITHDRAW_CIRCUIT_BREAKER_BPS
        )?;
    }

    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    msg!(
        "spot_market.withdraw_circuit_breaker_bps: {:?} -> {:?}",
        spot_market.withdraw_circuit_breaker_bps,
        withdraw_circuit_breaker_bps
    );

    spot_market.withdraw_circuit_breaker_bps = withdraw_circuit_breaker_bps;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_deposit_cap(
    ctx: Context<AdminUpdateSpotMarket>,
    deposit_guard_threshold: u64,
    max_deposit_bps_per_day: u16,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    msg!(
        "spot_market.deposit_guard_threshold: {:?} -> {:?}",
        spot_market.deposit_guard_threshold,
        deposit_guard_threshold
    );
    msg!(
        "spot_market.max_deposit_bps_per_day: {:?} -> {:?}",
        spot_market.max_deposit_bps_per_day,
        max_deposit_bps_per_day
    );

    spot_market.deposit_guard_threshold = deposit_guard_threshold;
    spot_market.max_deposit_bps_per_day = max_deposit_bps_per_day;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_max_token_borrows(
    ctx: Context<AdminUpdateSpotMarket>,
    max_token_borrows_fraction: u16,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    msg!(
        "spot_market.max_token_borrows_fraction: {:?} -> {:?}",
        spot_market.max_token_borrows_fraction,
        max_token_borrows_fraction
    );

    let current_spot_tokens_borrows: u64 = spot_market.get_borrows()?.cast()?;
    let new_max_token_borrows = spot_market
        .max_token_deposits
        .safe_mul(max_token_borrows_fraction.cast()?)?
        .safe_div(10000)?;

    validate!(
        current_spot_tokens_borrows <= new_max_token_borrows,
        ErrorCode::InvalidSpotMarketInitialization,
        "spot borrows {} > max_token_borrows {}",
        current_spot_tokens_borrows,
        max_token_borrows_fraction
    )?;

    spot_market.max_token_borrows_fraction = max_token_borrows_fraction;
    Ok(())
}

#[access_control(
spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_scale_initial_asset_weight_start(
    ctx: Context<AdminUpdateSpotMarket>,
    scale_initial_asset_weight_start: u64,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    msg!(
        "spot_market.scale_initial_asset_weight_start: {:?} -> {:?}",
        spot_market.scale_initial_asset_weight_start,
        scale_initial_asset_weight_start
    );

    spot_market.scale_initial_asset_weight_start = scale_initial_asset_weight_start;
    Ok(())
}

#[derive(Accounts)]
pub struct AdminUpdateSpotMarketWithdrawGuardThreshold<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        has_one = oracle @ ErrorCode::InvalidOracle,
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    /// CHECK: validated against `spot_market.oracle` by the `has_one` constraint
    pub oracle: UncheckedAccount<'info>,
}
