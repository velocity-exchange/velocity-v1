//! The risk a perp market may carry.
//!
//! Each handler moves one limit: the margin ratios, the IMF factor, the weight
//! on unrealized PnL, the funding period and dead zone, and the ceiling on open
//! interest.

use super::*;

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_margin_ratio(
    ctx: Context<AdminUpdatePerpMarket>,
    margin_ratio_initial: u32,
    margin_ratio_maintenance: u32,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;

    msg!(
        "updating perp market {} margin ratio",
        perp_market.market_index
    );

    perp_market.amm.validate_compatible_with_margin_ratio(
        margin_ratio_initial,
        margin_ratio_maintenance,
        perp_market.liquidator_fee,
        perp_market.if_liquidation_fee,
    )?;

    msg!(
        "perp_market.margin_ratio_initial: {:?} -> {:?}",
        perp_market.margin_ratio_initial,
        margin_ratio_initial
    );

    msg!(
        "perp_market.margin_ratio_maintenance: {:?} -> {:?}",
        perp_market.margin_ratio_maintenance,
        margin_ratio_maintenance
    );

    perp_market.margin_ratio_initial = margin_ratio_initial;
    perp_market.margin_ratio_maintenance = margin_ratio_maintenance;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_funding_period(
    ctx: Context<AdminUpdatePerpMarket>,
    funding_period: i64,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;

    msg!(
        "updating funding period for perp market {}",
        perp_market.market_index
    );

    validate!(funding_period >= 0, ErrorCode::DefaultError)?;

    msg!(
        "perp_market.funding_period: {:?} -> {:?}",
        perp_market.market_stats.funding_period,
        funding_period
    );

    perp_market.market_stats.funding_period = funding_period;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_funding_dead_zone(
    ctx: Context<AdminUpdatePerpMarket>,
    funding_clamp_threshold: u32,
    funding_ramp_slope: u32,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;

    msg!(
        "updating funding dead zone for perp market {}",
        perp_market.market_index
    );

    // threshold is a fraction of the oracle price; keep it well below 100%
    validate!(
        funding_clamp_threshold < BPS_PRECISION,
        ErrorCode::DefaultError
    )?;
    // a zero slope would flatten every premium past the band to the offset
    validate!(funding_ramp_slope > 0, ErrorCode::DefaultError)?;

    msg!(
        "perp_market.funding_clamp_threshold: {:?} -> {:?}",
        perp_market.funding_clamp_threshold,
        funding_clamp_threshold
    );

    msg!(
        "perp_market.funding_ramp_slope: {:?} -> {:?}",
        perp_market.funding_ramp_slope,
        funding_ramp_slope
    );

    perp_market.funding_clamp_threshold = funding_clamp_threshold;
    perp_market.funding_ramp_slope = funding_ramp_slope;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_imf_factor(
    ctx: Context<AdminUpdatePerpMarket>,
    imf_factor: u32,
    unrealized_pnl_imf_factor: u32,
) -> Result<()> {
    validate!(
        imf_factor <= SPOT_IMF_PRECISION,
        ErrorCode::DefaultError,
        "invalid imf factor",
    )?;
    validate!(
        unrealized_pnl_imf_factor <= SPOT_IMF_PRECISION,
        ErrorCode::DefaultError,
        "invalid unrealized pnl imf factor",
    )?;
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    msg!(
        "perp_market.imf_factor: {:?} -> {:?}",
        perp_market.imf_factor,
        imf_factor
    );

    msg!(
        "perp_market.unrealized_pnl_imf_factor: {:?} -> {:?}",
        perp_market.unrealized_pnl_imf_factor,
        unrealized_pnl_imf_factor
    );

    perp_market.imf_factor = imf_factor;
    perp_market.unrealized_pnl_imf_factor = unrealized_pnl_imf_factor;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_unrealized_asset_weight(
    ctx: Context<AdminUpdatePerpMarket>,
    unrealized_initial_asset_weight: u32,
    unrealized_maintenance_asset_weight: u32,
) -> Result<()> {
    validate!(
        unrealized_initial_asset_weight <= SPOT_WEIGHT_PRECISION.cast()?,
        ErrorCode::DefaultError,
        "invalid unrealized_initial_asset_weight",
    )?;
    validate!(
        unrealized_maintenance_asset_weight <= SPOT_WEIGHT_PRECISION.cast()?,
        ErrorCode::DefaultError,
        "invalid unrealized_maintenance_asset_weight",
    )?;
    validate!(
        unrealized_initial_asset_weight <= unrealized_maintenance_asset_weight,
        ErrorCode::DefaultError,
        "must enforce unrealized_initial_asset_weight <= unrealized_maintenance_asset_weight",
    )?;
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    msg!(
        "perp_market.unrealized_initial_asset_weight: {:?} -> {:?}",
        perp_market.unrealized_pnl_initial_asset_weight,
        unrealized_initial_asset_weight
    );

    msg!(
        "perp_market.unrealized_maintenance_asset_weight: {:?} -> {:?}",
        perp_market.unrealized_pnl_maintenance_asset_weight,
        unrealized_maintenance_asset_weight
    );

    perp_market.unrealized_pnl_initial_asset_weight = unrealized_initial_asset_weight;
    perp_market.unrealized_pnl_maintenance_asset_weight = unrealized_maintenance_asset_weight;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_max_open_interest(
    ctx: Context<AdminUpdatePerpMarket>,
    max_open_interest: u128,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    validate!(
        is_multiple_of_step_size(
            max_open_interest.cast::<u64>()?,
            perp_market.order_step_size
        )?,
        ErrorCode::DefaultError,
        "max oi not a multiple of the step size"
    )?;

    msg!(
        "perp_market.max_open_interest: {:?} -> {:?}",
        perp_market.max_open_interest,
        max_open_interest
    );

    perp_market.max_open_interest = max_open_interest;
    Ok(())
}
