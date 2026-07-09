use crate::error::{ErrorCode, VelocityResult};
use crate::math::constants::{
    LIQUIDATION_FEE_TO_MARGIN_PRECISION_RATIO, MAX_MARGIN_RATIO, MIN_MARGIN_RATIO,
    SPOT_IMF_PRECISION, SPOT_WEIGHT_PRECISION,
};
use crate::msg;
use crate::validate;

/// Total liquidation fees (`liquidator_fee + if_liquidation_fee`) must fit
/// strictly inside `margin_ratio_maintenance`: both fees are paid out of the
/// liquidated account's remaining equity, which at the liquidation boundary is
/// exactly the maintenance margin. Fees at or above it guarantee bad debt
/// before any adverse price movement during the close.
pub fn validate_margin(
    margin_ratio_initial: u32,
    margin_ratio_maintenance: u32,
    liquidator_fee: u32,
    if_liquidation_fee: u32,
    max_spread: u32,
) -> VelocityResult {
    if !(MIN_MARGIN_RATIO..=MAX_MARGIN_RATIO).contains(&margin_ratio_initial) {
        return Err(ErrorCode::InvalidMarginRatio);
    }

    if !(MIN_MARGIN_RATIO..=MAX_MARGIN_RATIO).contains(&margin_ratio_maintenance) {
        return Err(ErrorCode::InvalidMarginRatio);
    }

    if margin_ratio_initial <= margin_ratio_maintenance {
        return Err(ErrorCode::InvalidMarginRatio);
    }

    let total_liquidation_fee = liquidator_fee.saturating_add(if_liquidation_fee);
    validate!(
        margin_ratio_maintenance * LIQUIDATION_FEE_TO_MARGIN_PRECISION_RATIO
            > total_liquidation_fee,
        ErrorCode::InvalidMarginRatio,
        "margin_ratio_maintenance ({}) must exceed liquidator_fee + if_liquidation_fee ({})",
        margin_ratio_maintenance * LIQUIDATION_FEE_TO_MARGIN_PRECISION_RATIO,
        total_liquidation_fee
    )?;

    validate!(
        margin_ratio_initial * 100 > max_spread,
        ErrorCode::InvalidMarginRatio,
        "margin_ratio_initial ({}) must be greater than max_spread ({}) (or must lower max_spread first)",
        margin_ratio_initial * 100,
        max_spread
    )?;

    Ok(())
}

pub fn validate_margin_weights(
    spot_market_index: u16,
    initial_asset_weight: u32,
    maintenance_asset_weight: u32,
    initial_liability_weight: u32,
    maintenance_liability_weight: u32,
    imf_factor: u32,
) -> VelocityResult {
    if spot_market_index == 0 {
        validate!(
            initial_asset_weight == SPOT_WEIGHT_PRECISION,
            ErrorCode::InvalidSpotMarketInitialization,
            "For quote asset spot market, initial asset weight must be {}",
            SPOT_WEIGHT_PRECISION
        )?;

        validate!(
            maintenance_asset_weight == SPOT_WEIGHT_PRECISION,
            ErrorCode::InvalidSpotMarketInitialization,
            "For quote asset spot market, maintenance asset weight must be {}",
            SPOT_WEIGHT_PRECISION
        )?;

        validate!(
            initial_liability_weight == SPOT_WEIGHT_PRECISION,
            ErrorCode::InvalidSpotMarketInitialization,
            "For quote asset spot market, initial liability weight must be {}",
            SPOT_WEIGHT_PRECISION
        )?;

        validate!(
            maintenance_liability_weight == SPOT_WEIGHT_PRECISION,
            ErrorCode::InvalidSpotMarketInitialization,
            "For quote asset spot market, maintenance liability weight must be {}",
            SPOT_WEIGHT_PRECISION
        )?;
    } else {
        validate!(
            initial_asset_weight <= SPOT_WEIGHT_PRECISION,
            ErrorCode::InvalidSpotMarketInitialization,
            "Initial asset weight must be less than {}",
            SPOT_WEIGHT_PRECISION
        )?;

        validate!(
            initial_asset_weight <= maintenance_asset_weight
                && maintenance_asset_weight > 0
                && maintenance_asset_weight <= SPOT_WEIGHT_PRECISION,
            ErrorCode::InvalidSpotMarketInitialization,
            "Maintenance asset weight must be between 0 {}",
            SPOT_WEIGHT_PRECISION
        )?;

        validate!(
            initial_liability_weight >= SPOT_WEIGHT_PRECISION,
            ErrorCode::InvalidSpotMarketInitialization,
            "Initial liability weight must be greater than {}",
            SPOT_WEIGHT_PRECISION
        )?;

        validate!(
            initial_liability_weight >= maintenance_liability_weight
                && maintenance_liability_weight >= SPOT_WEIGHT_PRECISION,
            ErrorCode::InvalidSpotMarketInitialization,
            "Maintenance liability weight must be greater than {}",
            SPOT_WEIGHT_PRECISION
        )?;
    }

    validate!(
        imf_factor < SPOT_IMF_PRECISION,
        ErrorCode::InvalidSpotMarketInitialization,
        "imf_factor={} must be less than SPOT_IMF_PRECISION={}",
        imf_factor,
        SPOT_IMF_PRECISION,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_SPREAD: u32 = 2500;

    #[test]
    fn total_liquidation_fee_above_maintenance_margin_rejected() {
        // 5% maintenance margin, 1% liquidator + 5% IF fee = 6% total
        assert_eq!(
            validate_margin(1000, 500, 10000, 50000, MAX_SPREAD),
            Err(ErrorCode::InvalidMarginRatio)
        );
    }

    #[test]
    fn total_liquidation_fee_equal_to_maintenance_margin_rejected() {
        // 5% maintenance margin, fees sum to exactly 5%
        assert_eq!(
            validate_margin(1000, 500, 25000, 25000, MAX_SPREAD),
            Err(ErrorCode::InvalidMarginRatio)
        );
    }

    #[test]
    fn total_liquidation_fee_below_maintenance_margin_accepted() {
        // 5% maintenance margin, 1% + 1% fees
        assert!(validate_margin(1000, 500, 10000, 10000, MAX_SPREAD).is_ok());
        // 3% maintenance margin, 0.75% + 0.75% fees
        assert!(validate_margin(500, 300, 7500, 7500, MAX_SPREAD).is_ok());
    }

    #[test]
    fn fee_sum_overflow_saturates_and_rejects() {
        assert_eq!(
            validate_margin(1000, 500, u32::MAX, u32::MAX, MAX_SPREAD),
            Err(ErrorCode::InvalidMarginRatio)
        );
    }
}
