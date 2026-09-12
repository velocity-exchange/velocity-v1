//! What the exchange charges, and what it pays to land a transaction.
//!
//! The exchange-wide handlers replace a whole fee structure, select the promo
//! tier, and re-price the network fee rails. The per-market handlers adjust one
//! market's fees against that structure.

use super::*;

pub fn handle_update_promo_fee_tier(
    ctx: Context<AdminUpdateState>,
    promo_fee_tier: u8,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;

    // validate against the highest populated tier, not the 10-slot array:
    // the tier fn clamps to PERP_FEE_TIER_MAX_INDEX, so anything above it
    // would validate and then silently mean a lower tier. 0 = disabled
    // (no-op floor).
    validate!(
        (promo_fee_tier as usize) <= PERP_FEE_TIER_MAX_INDEX,
        ErrorCode::DefaultError,
        "promo fee tier {} above max populated tier {}",
        promo_fee_tier,
        PERP_FEE_TIER_MAX_INDEX
    )?;

    msg!(
        "state.promo_fee_tier: {:?} -> {:?}",
        state.promo_fee_tier,
        promo_fee_tier
    );

    state.promo_fee_tier = promo_fee_tier;
    Ok(())
}
/// Re-price what the network charges to land a transaction.
///
/// Every relay crank pays its keeper enough to cover the keeper's own
/// transaction, and that cost is a function of the network's fee model. When
/// the model changes — a new rate on requested cost units, a different
/// inclusion fee — this is the one write that moves it.
///
/// Payments already stored on a market's conditions account hold their figures
/// until that market's attach runs again. That fails in the loud direction: a
/// stored floor above what the executor pays stops the crank at
/// `assert_paid_v0`, where a floor below it only overpays.
pub fn handle_update_transaction_fee_rails(
    ctx: Context<AdminUpdateState>,
    rails: TransactionFeeRails,
) -> Result<()> {
    validate!(
        rails.resource_fee_denominator > 0 || rails.resource_fee_numerator == 0,
        ErrorCode::DefaultError,
        "a resource fee rate needs a denominator"
    )?;

    msg!(
        "transaction_fee_rails: {:?} -> {:?}",
        ctx.accounts.state.load()?.transaction_fee_rails,
        rails
    );

    ctx.accounts.state.load_mut()?.transaction_fee_rails = rails;
    Ok(())
}

pub fn handle_update_perp_fee_structure(
    ctx: Context<AdminUpdateState>,
    fee_structure: FeeStructure,
) -> Result<()> {
    validate_fee_structure(&fee_structure)?;

    msg!(
        "perp_fee_structure: {:?} -> {:?}",
        ctx.accounts.state.load()?.perp_fee_structure,
        fee_structure
    );

    ctx.accounts.state.load_mut()?.perp_fee_structure = fee_structure;
    Ok(())
}

pub fn handle_update_spot_fee_structure(
    ctx: Context<AdminUpdateState>,
    fee_structure: FeeStructure,
) -> Result<()> {
    validate_fee_structure(&fee_structure)?;

    msg!(
        "spot_fee_structure: {:?} -> {:?}",
        ctx.accounts.state.load()?.spot_fee_structure,
        fee_structure
    );

    ctx.accounts.state.load_mut()?.spot_fee_structure = fee_structure;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_fee_adjustment(
    ctx: Context<AdminUpdatePerpMarket>,
    fee_adjustment: i16,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    validate!(
        fee_adjustment.unsigned_abs().cast::<u64>()? <= FEE_ADJUSTMENT_MAX,
        ErrorCode::DefaultError,
        "fee adjustment {} greater than max {}",
        fee_adjustment,
        FEE_ADJUSTMENT_MAX
    )?;

    msg!(
        "perp_market.fee_adjustment: {:?} -> {:?}",
        perp_market.fee_adjustment,
        fee_adjustment
    );

    perp_market.fee_adjustment = fee_adjustment;
    Ok(())
}

pub fn handle_update_perp_market_taker_fee_addon(
    ctx: Context<AdminUpdatePerpMarket>,
    taker_fee_addon_tenth_bps: u16,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    validate!(
        taker_fee_addon_tenth_bps <= MAX_TAKER_FEE_ADDON_TENTH_BPS,
        ErrorCode::DefaultError,
        "taker fee addon {} greater than max {}",
        taker_fee_addon_tenth_bps,
        MAX_TAKER_FEE_ADDON_TENTH_BPS
    )?;

    msg!(
        "perp_market.taker_fee_addon_tenth_bps: {:?} -> {:?}",
        perp_market.taker_fee_addon_tenth_bps,
        taker_fee_addon_tenth_bps
    );

    perp_market.taker_fee_addon_tenth_bps = taker_fee_addon_tenth_bps;
    Ok(())
}

pub fn handle_update_perp_market_fee_pool_buffer_target(
    ctx: Context<AdminUpdatePerpMarket>,
    fee_pool_buffer_target: u64,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    msg!(
        "perp_market.fee_pool_buffer_target: {:?} -> {:?}",
        perp_market.fee_pool_buffer_target,
        fee_pool_buffer_target
    );

    perp_market.fee_pool_buffer_target = fee_pool_buffer_target;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_fee_adjustment(
    ctx: Context<AdminUpdateSpotMarket>,
    fee_adjustment: i16,
) -> Result<()> {
    let spot = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot.market_index);

    validate!(
        fee_adjustment.unsigned_abs().cast::<u64>()? <= FEE_ADJUSTMENT_MAX,
        ErrorCode::DefaultError,
        "fee adjustment {} greater than max {}",
        fee_adjustment,
        FEE_ADJUSTMENT_MAX
    )?;

    msg!(
        "spot_market.fee_adjustment: {:?} -> {:?}",
        spot.fee_adjustment,
        fee_adjustment
    );

    spot.fee_adjustment = fee_adjustment;
    Ok(())
}
