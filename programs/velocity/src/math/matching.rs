use crate::{
    controller::position::PositionDirection,
    error::VelocityResult,
    math::{
        casting::Cast,
        constants::{BID_ASK_SPREAD_PRECISION_I128, TEN_BPS_I64},
        safe_math::SafeMath,
    },
};

#[cfg(test)]
mod tests;

pub fn calculate_filler_multiplier_for_matched_orders(
    maker_price: u64,
    maker_direction: PositionDirection,
    oracle_price: i64,
) -> VelocityResult<u64> {
    // percentage oracle_price is above maker_price
    let price_pct_diff = oracle_price
        .safe_sub(maker_price.cast::<i64>()?)?
        .cast::<i128>()?
        .safe_mul(BID_ASK_SPREAD_PRECISION_I128)?
        .safe_div(oracle_price.cast()?)?
        .cast::<i64>()?;

    // offer filler multiplier based on price improvement from reasonable baseline
    // multiplier between 1x and 100x
    let multiplier = match maker_direction {
        PositionDirection::Long => (-price_pct_diff).safe_add(TEN_BPS_I64 * 2)?,
        PositionDirection::Short => price_pct_diff.safe_add(TEN_BPS_I64 * 2)?,
    }
    .clamp(TEN_BPS_I64, TEN_BPS_I64 * 100);

    multiplier.cast()
}
