use crate::{
    error::{ErrorCode, VelocityResult},
    math::{
        casting::Cast,
        constants::{
            AMM_RESERVE_PRECISION_I128, BASE_PRECISION,
            FUNDING_RATE_TO_QUOTE_PRECISION_PRECISION_RATIO, LIQUIDATION_FEE_INCREASE_PER_PERIOD,
            LIQUIDATION_FEE_PRECISION, LIQUIDATION_FEE_PRECISION_U128,
            LIQUIDATION_FEE_TO_MARGIN_PRECISION_RATIO, LIQUIDATION_PCT_PRECISION, PRICE_PRECISION,
            PRICE_TIMES_AMM_TO_QUOTE_PRECISION_RATIO, QUOTE_PRECISION, SPOT_WEIGHT_PRECISION_U128,
        },
        margin::calculate_margin_requirement_and_total_collateral_and_liability_info,
        safe_math::SafeMath,
        spot_balance::get_token_amount,
        spot_swap::calculate_swap_price,
        time::{Millis, SlotClock},
    },
    msg,
    state::{
        margin_calculation::MarginContext,
        oracle::OraclePriceData,
        oracle_map::OracleMap,
        perp_market::PerpMarket,
        perp_market_map::PerpMarketMap,
        spot_market::{SpotBalanceType, SpotMarket},
        spot_market_map::SpotMarketMap,
        user::{OrderType, User},
    },
    validate, MarketType, OrderParams, PositionDirection,
};

/// Grace before the liquidation fee starts ramping (~10 minutes).
pub const LIQUIDATION_FEE_ADJUST_GRACE_PERIOD: Millis = Millis::from_secs(600);

#[cfg(test)]
mod tests;

pub fn calculate_base_asset_amount_to_cover_margin_shortage(
    margin_shortage: u128,
    margin_ratio: u32,
    liquidation_fee: u32,
    if_liquidation_fee: u32,
    oracle_price: i64,
    quote_oracle_price: i64,
) -> VelocityResult<u64> {
    let margin_ratio = margin_ratio.safe_mul(LIQUIDATION_FEE_TO_MARGIN_PRECISION_RATIO)?;

    if oracle_price == 0 || margin_ratio <= liquidation_fee {
        return Ok(u64::MAX);
    }

    margin_shortage
        .safe_mul(PRICE_TIMES_AMM_TO_QUOTE_PRECISION_RATIO)?
        .safe_div(
            oracle_price
                .cast::<u128>()?
                .safe_mul(quote_oracle_price.cast()?)?
                .safe_div(PRICE_PRECISION)?
                .safe_mul(margin_ratio.safe_sub(liquidation_fee)?.cast()?)?
                .safe_div(LIQUIDATION_FEE_PRECISION_U128)?
                .safe_sub(
                    oracle_price
                        .cast::<u128>()?
                        .safe_mul(if_liquidation_fee.cast()?)?
                        .safe_div(LIQUIDATION_FEE_PRECISION_U128)?,
                )?,
        )?
        .cast()
}

pub fn calculate_liability_transfer_to_cover_margin_shortage(
    margin_shortage: u128,
    asset_weight: u32,
    asset_liquidation_multiplier: u32,
    liability_weight: u32,
    liability_liquidation_multiplier: u32,
    liability_decimals: u32,
    liability_price: i64,
    if_liquidation_fee: u32,
) -> VelocityResult<u128> {
    // If unsettled pnl asset weight is 1 and quote asset is 1, this calculation breaks
    if asset_weight >= liability_weight {
        return Ok(u128::MAX);
    }

    let (numerator_scale, denominator_scale) = if liability_decimals > 6 {
        (10_u128.pow(liability_decimals - 6), 1)
    } else {
        (1, 10_u128.pow(6 - liability_decimals))
    };

    let liability_weight_component = liability_weight.cast::<u128>()?.safe_mul(10)?; // multiply market weights by extra 10 to increase precision

    let asset_weight_component = asset_weight
        .cast::<u128>()?
        .safe_mul(10)?
        .safe_mul(asset_liquidation_multiplier.cast()?)?
        .safe_div(liability_liquidation_multiplier.cast()?)?;

    if asset_weight_component >= liability_weight_component {
        return Ok(u128::MAX);
    }

    margin_shortage
        .safe_mul(numerator_scale)?
        .safe_mul(PRICE_PRECISION * SPOT_WEIGHT_PRECISION_U128 * 10)?
        .safe_div(
            liability_price
                .cast::<u128>()?
                .safe_mul(liability_weight_component.safe_sub(asset_weight_component)?)?
                .safe_sub(
                    liability_price
                        .cast::<u128>()?
                        .safe_mul(if_liquidation_fee.cast()?)?
                        .safe_div(LIQUIDATION_FEE_PRECISION_U128)?
                        .safe_mul(liability_weight.cast()?)?
                        .safe_mul(10)?,
                )?,
        )?
        .safe_div(denominator_scale)
        .map(|x| x.max(1))
}

/// User-protective price for seizing a collateral (deposit) asset whose oracle is
/// margin-invalid (`StaleForMargin`/`TooUncertain`) but still acceptable for
/// `VelocityAction::Liquidate`. Pricing the seizure at
/// `max(oracle, 5min twap, oracle + confidence)` preserves the invariant that a stale or
/// uncertain oracle can make an account liquidatable but cannot cheapen its collateral.
pub fn calculate_user_protective_asset_price(
    oracle_price_data: &OraclePriceData,
    last_oracle_price_twap_5min: i64,
) -> VelocityResult<i64> {
    let confidence_adjusted_high_price = oracle_price_data
        .price
        .safe_add(oracle_price_data.confidence.cast::<i64>()?)?;

    Ok(oracle_price_data
        .price
        .max(last_oracle_price_twap_5min)
        .max(confidence_adjusted_high_price))
}

/// Liability-side counterpart of [`calculate_user_protective_asset_price`]: a margin-invalid
/// (stale/uncertain) borrow oracle must not overvalue the liability being repaid, since the
/// exchange rate `liability_price / asset_price` cheapens the user's collateral from either
/// side. Prices the repayment at `min(oracle, 5min twap, oracle - confidence)`, floored at 1
/// to keep the exchange-rate math well-defined.
pub fn calculate_user_protective_liability_price(
    oracle_price_data: &OraclePriceData,
    last_oracle_price_twap_5min: i64,
) -> VelocityResult<i64> {
    let confidence_adjusted_low_price = oracle_price_data
        .price
        .saturating_sub(oracle_price_data.confidence.cast::<i64>()?);

    Ok(oracle_price_data
        .price
        .min(last_oracle_price_twap_5min)
        .min(confidence_adjusted_low_price)
        .max(1))
}

pub fn calculate_liability_transfer_implied_by_asset_amount(
    asset_amount: u128,
    asset_liquidation_multiplier: u32,
    asset_decimals: u32,
    asset_price: i64,
    liability_liquidation_multiplier: u32,
    liability_decimals: u32,
    liability_price: i64,
) -> VelocityResult<u128> {
    let (numerator_scale, denominator_scale) = if liability_decimals > asset_decimals {
        (10_u128.pow(liability_decimals - asset_decimals), 1)
    } else {
        (1, 10_u128.pow(asset_decimals - liability_decimals))
    };

    asset_amount
        .safe_mul(numerator_scale)?
        .safe_mul(asset_price.cast()?)?
        .safe_mul(liability_liquidation_multiplier.cast()?)?
        .safe_div_ceil(
            liability_price
                .cast::<u128>()?
                .safe_mul(asset_liquidation_multiplier.cast()?)?,
        )?
        .safe_div_ceil(denominator_scale)
}

/// The asset amount that pays for `liability_amount` at the liquidation exchange
/// rate, with no rounding to the user's whole deposit. Use this where the result
/// is a bound on how much collateral may be taken, or where the caller must know
/// that every unit seized is paid for.
/// `calculate_asset_transfer_for_liability_transfer` wraps this with the
/// round-to-whole-deposit step.
pub fn calculate_asset_transfer_for_liability_transfer_exact(
    asset_liquidation_multiplier: u32,
    asset_decimals: u32,
    asset_price: i64,
    liability_amount: u128,
    liability_liquidation_multiplier: u32,
    liability_decimals: u32,
    liability_price: i64,
) -> VelocityResult<u128> {
    let (numerator_scale, denominator_scale) = if asset_decimals > liability_decimals {
        (10_u128.pow(asset_decimals - liability_decimals), 1)
    } else {
        (1, 10_u128.pow(liability_decimals - asset_decimals))
    };

    Ok(liability_amount
        .safe_mul(numerator_scale)?
        .safe_mul(liability_price.cast()?)?
        .safe_mul(asset_liquidation_multiplier.cast()?)?
        .safe_div(
            asset_price
                .cast::<u128>()?
                .safe_mul(liability_liquidation_multiplier.cast()?)?,
        )?
        .safe_div(denominator_scale)?
        .max(1))
}

/// The exact asset amount, rounded to the user's whole deposit when the two are
/// within $1 of each other. The round-up leaves no dust deposit behind, but it
/// takes up to $1 of collateral that `liability_amount` does not pay for. A
/// caller that cannot give that value away must use
/// `calculate_asset_transfer_for_liability_transfer_exact` instead.
pub fn calculate_asset_transfer_for_liability_transfer(
    asset_amount: u128,
    asset_liquidation_multiplier: u32,
    asset_decimals: u32,
    asset_price: i64,
    liability_amount: u128,
    liability_liquidation_multiplier: u32,
    liability_decimals: u32,
    liability_price: i64,
) -> VelocityResult<u128> {
    let mut asset_transfer = calculate_asset_transfer_for_liability_transfer_exact(
        asset_liquidation_multiplier,
        asset_decimals,
        asset_price,
        liability_amount,
        liability_liquidation_multiplier,
        liability_decimals,
        liability_price,
    )?;

    // Need to check if asset_transfer should be rounded to asset amount
    let (asset_value_numerator_scale, asset_value_denominator_scale) = if asset_decimals > 6 {
        (10_u128.pow(asset_decimals - 6), 1)
    } else {
        (1, 10_u128.pow(6 - asset_decimals))
    };

    let asset_delta = asset_transfer.abs_diff(asset_amount);

    let asset_value_delta = asset_delta
        .safe_mul(asset_price.cast()?)?
        .safe_div(PRICE_PRECISION)?
        .safe_mul(asset_value_numerator_scale)?
        .safe_div(asset_value_denominator_scale)?;

    if asset_value_delta < QUOTE_PRECISION {
        asset_transfer = asset_amount;
    }

    Ok(asset_transfer)
}

pub fn is_cross_margin_being_liquidated(
    user: &User,
    market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    liquidation_margin_buffer_ratio: u32,
) -> VelocityResult<bool> {
    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        market_map,
        spot_market_map,
        oracle_map,
        MarginContext::liquidation(liquidation_margin_buffer_ratio),
    )?;

    let is_being_liquidated = !margin_calculation.can_exit_cross_margin_liquidation()?;

    Ok(is_being_liquidated)
}

pub fn validate_user_not_being_liquidated(
    user: &mut User,
    market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    liquidation_margin_buffer_ratio: u32,
) -> VelocityResult {
    if !user.is_being_liquidated() {
        return Ok(());
    }

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        market_map,
        spot_market_map,
        oracle_map,
        MarginContext::liquidation(liquidation_margin_buffer_ratio),
    )?;

    // Cross-margin and isolated liquidation states are independent; a user can
    // hold both at once. Check each separately and only return success when no
    // liquidation state remains, otherwise clearing the cross flag would bypass
    // a still-active isolated liquidation.
    if user.is_cross_margin_being_liquidated() {
        if margin_calculation.can_exit_cross_margin_liquidation()? {
            user.exit_cross_margin_liquidation();
        } else {
            return Err(ErrorCode::UserIsBeingLiquidated);
        }
    }

    let isolated_positions_being_liquidated = user
        .perp_positions
        .iter()
        .filter(|position| position.is_isolated() && position.is_being_liquidated())
        .map(|position| position.market_index)
        .collect::<Vec<_>>();

    for perp_market_index in isolated_positions_being_liquidated {
        if margin_calculation.can_exit_isolated_margin_liquidation(perp_market_index)? {
            user.exit_isolated_margin_liquidation(perp_market_index)?;
        } else {
            return Err(ErrorCode::UserIsBeingLiquidated);
        }
    }

    Ok(())
}

pub fn is_isolated_margin_being_liquidated(
    user: &User,
    market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    perp_market_index: u16,
    liquidation_margin_buffer_ratio: u32,
) -> VelocityResult<bool> {
    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        market_map,
        spot_market_map,
        oracle_map,
        MarginContext::liquidation(liquidation_margin_buffer_ratio),
    )?;

    let is_being_liquidated =
        !margin_calculation.can_exit_isolated_margin_liquidation(perp_market_index)?;

    Ok(is_being_liquidated)
}

pub enum LiquidationMultiplierType {
    Discount,
    Premium,
}

pub fn calculate_liquidation_multiplier(
    liquidation_fee: u32,
    multiplier_type: LiquidationMultiplierType,
) -> VelocityResult<u32> {
    match multiplier_type {
        LiquidationMultiplierType::Premium => LIQUIDATION_FEE_PRECISION.safe_add(liquidation_fee),
        LiquidationMultiplierType::Discount => LIQUIDATION_FEE_PRECISION.safe_sub(liquidation_fee),
    }
}

pub fn calculate_funding_rate_deltas_to_resolve_bankruptcy(
    loss: i128,
    market: &PerpMarket,
) -> VelocityResult<i128> {
    let total_base_asset_amount = market
        .base_asset_amount_long
        .abs()
        .safe_add(market.base_asset_amount_short.abs())?;

    validate!(
        total_base_asset_amount != 0,
        ErrorCode::CantResolvePerpBankruptcy,
        "Cant resolve perp bankruptcy when total base asset amount is 0"
    )?;

    loss.abs()
        .safe_mul(AMM_RESERVE_PRECISION_I128)?
        .safe_div_ceil(total_base_asset_amount)?
        .safe_mul(FUNDING_RATE_TO_QUOTE_PRECISION_PRECISION_RATIO.cast()?)
}

pub fn calculate_cumulative_deposit_interest_delta_to_resolve_bankruptcy(
    borrow: u128,
    spot_market: &SpotMarket,
) -> VelocityResult<u128> {
    let total_deposits = get_token_amount(
        spot_market.deposit_balance,
        spot_market,
        &SpotBalanceType::Deposit,
    )?;

    // No depositors to haircut: nothing to socialize against.
    if total_deposits == 0 {
        return Ok(0);
    }

    let delta = spot_market
        .cumulative_deposit_interest
        .safe_mul(borrow)?
        .safe_div_ceil(total_deposits)?;

    // When the loss meets or exceeds total deposits, cap the haircut so
    // cumulative_deposit_interest stays >= 1: depositors are wiped out
    // (balances redeem for ~0 tokens) but the interest never underflows and
    // balance conversions, which divide by it, stay well-defined.
    Ok(delta.min(spot_market.cumulative_deposit_interest.saturating_sub(1)))
}

pub fn validate_transfer_satisfies_limit_price(
    asset_transfer: u128,
    liability_transfer: u128,
    asset_decimals: u32,
    liability_decimals: u32,
    limit_price: Option<u64>,
) -> VelocityResult {
    let limit_price = match limit_price {
        Some(limit_price) => limit_price,
        None => return Ok(()),
    };

    let swap_price = calculate_swap_price(
        asset_transfer,
        liability_transfer,
        asset_decimals,
        liability_decimals,
    )?;

    validate!(
        swap_price >= limit_price.cast()?,
        ErrorCode::LiquidationDoesntSatisfyLimitPrice,
        "transfer price transfer_price ({}/1000000) < limit price ({}/1000000)",
        swap_price,
        limit_price
    )
}

pub fn calculate_max_pct_to_liquidate(
    user: &User,
    margin_shortage: u128,
    slot: u64,
    initial_pct_to_liquidate: u128,
    liquidation_duration: Millis,
    slot_clock: SlotClock,
) -> VelocityResult<u128> {
    // if margin shortage is tiny, accelerate liquidation
    if margin_shortage < 50 * QUOTE_PRECISION {
        return Ok(LIQUIDATION_PCT_PRECISION);
    }

    // The ramp is the ratio of elapsed wall-clock time to the configured
    // liquidation window. Elapsed time is integrated per slot-duration regime,
    // so an interval spanning an IBRL transition ramps at the same wall-clock
    // rate on both sides. Identity with the historical slot ratio at 400ms.
    let elapsed_ms = slot_clock.elapsed(user.last_active_slot, slot).as_ms();
    let duration_ms = liquidation_duration.as_ms();

    let pct_freeable = (elapsed_ms as u128)
        .safe_mul(LIQUIDATION_PCT_PRECISION)?
        .safe_div(duration_ms as u128) // ~1 minute at the onchain default
        .unwrap_or(LIQUIDATION_PCT_PRECISION) // if divide by zero, default to 100%
        .safe_add(initial_pct_to_liquidate)?
        .min(LIQUIDATION_PCT_PRECISION);

    let total_margin_shortage = margin_shortage.safe_add(user.liquidation_margin_freed.cast()?)?;
    let max_margin_freed = total_margin_shortage
        .safe_mul(pct_freeable)?
        .safe_div(LIQUIDATION_PCT_PRECISION)?;
    let margin_freeable = max_margin_freed.saturating_sub(user.liquidation_margin_freed.cast()?);

    margin_freeable
        .safe_mul(LIQUIDATION_PCT_PRECISION)?
        .safe_div(margin_shortage)
}

pub fn calculate_perp_if_fee(
    margin_shortage: u128,
    user_base_asset_amount: u64,
    margin_ratio: u32,
    liquidator_fee: u32,
    oracle_price: i64,
    quote_oracle_price: i64,
    max_if_liquidation_fee: u32,
) -> VelocityResult<u32> {
    let margin_ratio = margin_ratio.safe_mul(LIQUIDATION_FEE_TO_MARGIN_PRECISION_RATIO)?;

    if oracle_price == 0
        || quote_oracle_price == 0
        || margin_ratio <= liquidator_fee
        || user_base_asset_amount == 0
    {
        return Ok(0);
    }

    let price = oracle_price
        .cast::<u128>()?
        .safe_mul(quote_oracle_price.cast()?)?
        .safe_div(PRICE_PRECISION)?;

    // margin ratio - liquidator fee - (margin shortage / (user base asset amount * price))
    let implied_if_fee = margin_ratio
        .saturating_sub(liquidator_fee)
        .saturating_sub(
            margin_shortage
                .safe_mul(BASE_PRECISION)?
                .safe_div(user_base_asset_amount.cast()?)?
                .safe_mul(PRICE_PRECISION)?
                .safe_div(price)?
                .cast::<u32>()
                .unwrap_or(u32::MAX),
        )
        // multiply by 95% to avoid situation where fee leads to deposits == negative pnl
        // leading to bankruptcy
        .safe_mul(19)?
        .safe_div(20)?;

    Ok(max_if_liquidation_fee.min(implied_if_fee))
}

pub fn calculate_spot_if_fee(
    margin_shortage: u128,
    token_amount: u128,
    asset_weight: u32,
    asset_liquidation_multiplier: u32,
    liability_weight: u32,
    liability_liquidation_multiplier: u32,
    liability_decimals: u32,
    liability_price: i64,
    max_if_fee: u32,
) -> VelocityResult<u32> {
    if asset_weight >= liability_weight
        || liability_price == 0
        || token_amount == 0
        || liability_liquidation_multiplier == 0
    {
        return Ok(0);
    }

    let token_precision = 10_u128.pow(liability_decimals);

    let liability_weight = liability_weight
        .cast::<u128>()?
        .safe_mul(LIQUIDATION_FEE_PRECISION_U128 / SPOT_WEIGHT_PRECISION_U128)?;
    let asset_weight = asset_weight
        .cast::<u128>()?
        .safe_mul(LIQUIDATION_FEE_PRECISION_U128 / SPOT_WEIGHT_PRECISION_U128)?;

    let implied_if_fee = liability_weight
        .saturating_sub(
            asset_weight
                .safe_mul(asset_liquidation_multiplier.cast()?)?
                .safe_div(liability_liquidation_multiplier.cast()?)?,
        )
        .saturating_sub(
            margin_shortage
                .safe_mul(LIQUIDATION_FEE_PRECISION_U128)?
                .safe_mul(token_precision)?
                .safe_div(token_amount)?
                .safe_div(liability_price.cast()?)?, // price and quote precision the same
        )
        .safe_mul(LIQUIDATION_FEE_PRECISION_U128)?
        .safe_div(liability_weight)?
        .cast::<u32>()
        .unwrap_or(u32::MAX);

    Ok(max_if_fee.min(implied_if_fee))
}

pub fn get_liquidation_order_params(
    market_index: u16,
    existing_direction: PositionDirection,
    base_asset_amount: u64,
    oracle_price: i64,
    liquidation_fee: u32,
) -> VelocityResult<OrderParams> {
    let direction = existing_direction.opposite();

    let oracle_price_u128 = oracle_price.abs().cast::<u128>()?;
    let limit_price = match direction {
        PositionDirection::Long => oracle_price_u128
            .safe_add(
                oracle_price_u128
                    .safe_mul(liquidation_fee.cast()?)?
                    .safe_div(LIQUIDATION_FEE_PRECISION_U128)?,
            )?
            .cast::<u64>()?,
        PositionDirection::Short => oracle_price_u128
            .safe_sub(
                oracle_price_u128
                    .safe_mul(liquidation_fee.cast()?)?
                    .safe_div(LIQUIDATION_FEE_PRECISION_U128)?,
            )?
            .cast::<u64>()?,
    };

    let order_params = OrderParams {
        market_index,
        direction,
        price: limit_price,
        order_type: OrderType::Limit,
        market_type: MarketType::Perp,
        base_asset_amount,
        reduce_only: true,
        ..OrderParams::default()
    };

    Ok(order_params)
}

pub fn get_liquidation_fee(
    base_liquidation_fee: u32,
    max_liquidation_fee: u32,
    last_active_user_slot: u64,
    current_slot: u64,
    slot_clock: SlotClock,
) -> VelocityResult<u32> {
    // The fee ramps per whole 400ms period of elapsed time past the grace
    // window (the rate's historical calibration). Floor on the period count:
    // the fee escalates marginally later, favoring the user. Elapsed time is
    // integrated per slot-duration regime.
    let elapsed = slot_clock.elapsed(last_active_user_slot, current_slot);
    if elapsed < LIQUIDATION_FEE_ADJUST_GRACE_PERIOD {
        return Ok(base_liquidation_fee);
    }

    let liquidation_fee = base_liquidation_fee.saturating_add(
        elapsed
            .div_periods(Millis::UNIT)
            .safe_mul(LIQUIDATION_FEE_INCREASE_PER_PERIOD.cast::<u64>()?)?
            .cast::<u32>()
            .unwrap_or(u32::MAX),
    );
    Ok(liquidation_fee.min(max_liquidation_fee))
}

pub fn validate_swap_within_liquidation_boundaries(
    asset_transfer: u128,
    liability_transfer: u128,
    asset_decimals: u32,
    liability_decimals: u32,
    asset_price: i64,
    liability_price: i64,
    asset_liquidation_multiplier: u32,
    liability_liquidation_multiplier: u32,
) -> VelocityResult {
    let asset_precision = 10_u128.pow(asset_decimals);
    let liability_precision = 10_u128.pow(liability_decimals);

    let swap_price = liability_transfer
        .safe_mul(PRICE_PRECISION)?
        .safe_div(liability_precision)?
        .safe_mul(asset_precision)?
        .safe_div(asset_transfer)?;

    let worst_case_price = asset_price
        .cast::<u128>()?
        .safe_mul(PRICE_PRECISION)?
        .safe_mul(liability_liquidation_multiplier.cast()?)?
        .safe_div(liability_price.cast()?)?
        .safe_div(asset_liquidation_multiplier.cast()?)?;

    validate!(
        swap_price >= worst_case_price,
        ErrorCode::InvalidLiquidation,
        "swap price ({}/1000000) < worst case price ({}/1000000)",
        swap_price,
        worst_case_price
    )?;

    Ok(())
}
