use crate::{
    controller::position::PositionDelta,
    error::VelocityResult,
    math::{
        casting::Cast,
        constants::{
            AMM_RESERVE_PRECISION_I128, PRICE_TIMES_AMM_TO_QUOTE_PRECISION_RATIO,
            PRICE_TIMES_AMM_TO_QUOTE_PRECISION_RATIO_I128,
        },
        safe_math::SafeMath,
    },
    state::user::PerpPosition,
    vlp::amm::controller::SwapDirection,
};

pub fn calculate_base_asset_value_with_oracle_price(
    base_asset_amount: i128,
    oracle_price: i64,
) -> VelocityResult<u128> {
    if base_asset_amount == 0 {
        return Ok(0);
    }

    let oracle_price = if oracle_price > 0 {
        oracle_price.unsigned_abs()
    } else {
        0
    };

    base_asset_amount
        .unsigned_abs()
        .safe_mul(oracle_price.cast()?)?
        .safe_div(PRICE_TIMES_AMM_TO_QUOTE_PRECISION_RATIO)
}

pub fn calculate_perp_liability_value(
    base_asset_amount: i128,
    oracle_price: i64,
) -> VelocityResult<u128> {
    calculate_base_asset_value_with_oracle_price(base_asset_amount, oracle_price)
}

pub fn calculate_base_asset_value_and_pnl_with_oracle_price(
    market_position: &PerpPosition,
    oracle_price: i64,
) -> VelocityResult<(u128, i128)> {
    calculate_base_asset_value_and_pnl_with_price(market_position, oracle_price, false)
}

/// Same as [`calculate_base_asset_value_and_pnl_with_oracle_price`], but valued at a
/// market's committed `expiry_price`, which is allowed to be **negative**.
///
/// The live-oracle path clamps a non-positive price to zero, because a negative *oracle*
/// print is nonsense and defaulting it to zero is the safe read. A negative `expiry_price`
/// is not nonsense: the expiry solver can legitimately commit one. Applying the oracle
/// clamp to it clipped a long's signed base loss to zero, so margin and equity valued the
/// position as merely worthless instead of underwater — letting the owner withdraw
/// collateral — while `settle_expired_position` (via
/// `calculate_base_asset_value_with_expiry_price`, which never clamped) later booked the
/// real negative value as an unsecured quote borrow. That divergence between the two
/// valuations *was* OtterSec #133.
pub fn calculate_base_asset_value_and_pnl_with_expiry_price(
    market_position: &PerpPosition,
    expiry_price: i64,
) -> VelocityResult<(u128, i128)> {
    calculate_base_asset_value_and_pnl_with_price(market_position, expiry_price, true)
}

fn calculate_base_asset_value_and_pnl_with_price(
    market_position: &PerpPosition,
    price: i64,
    allow_negative_price: bool,
) -> VelocityResult<(u128, i128)> {
    if market_position.base_asset_amount == 0 {
        return Ok((0, market_position.quote_asset_amount.cast()?));
    }

    let oracle_price = if price > 0 {
        price.abs()
    } else if allow_negative_price {
        price
    } else {
        0
    };

    let base_asset_value = market_position
        .base_asset_amount
        .cast::<i128>()?
        .safe_mul(oracle_price.cast()?)?
        .safe_div(AMM_RESERVE_PRECISION_I128)?;

    let pnl = base_asset_value.safe_add(market_position.quote_asset_amount.cast()?)?;

    Ok((base_asset_value.unsigned_abs(), pnl))
}

pub fn calculate_base_asset_value_with_expiry_price(
    market_position: &PerpPosition,
    expiry_price: i64,
) -> VelocityResult<i64> {
    if market_position.base_asset_amount == 0 {
        return Ok(0);
    }

    market_position
        .base_asset_amount
        .cast::<i128>()?
        .safe_mul(expiry_price.cast()?)?
        .safe_div(PRICE_TIMES_AMM_TO_QUOTE_PRECISION_RATIO_I128)?
        .cast::<i64>()
}

pub fn swap_direction_to_close_position(base_asset_amount: i128) -> SwapDirection {
    if base_asset_amount >= 0 {
        SwapDirection::Add
    } else {
        SwapDirection::Remove
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionUpdateType {
    Open,
    Increase,
    Reduce,
    Close,
    Flip,
}
pub fn get_position_update_type(
    position: &PerpPosition,
    delta: &PositionDelta,
) -> VelocityResult<PositionUpdateType> {
    if position.base_asset_amount == 0 {
        return Ok(PositionUpdateType::Open);
    }

    let position_base = position.base_asset_amount;

    let delta_base = delta.base_asset_amount;

    if position_base.signum() == delta_base.signum() {
        Ok(PositionUpdateType::Increase)
    } else if position_base.abs() > delta_base.abs() {
        Ok(PositionUpdateType::Reduce)
    } else if position_base.abs() == delta_base.abs() {
        Ok(PositionUpdateType::Close)
    } else {
        Ok(PositionUpdateType::Flip)
    }
}

pub fn get_new_position_amounts(
    position: &PerpPosition,
    delta: &PositionDelta,
) -> VelocityResult<(i64, i64)> {
    let new_quote_asset_amount = position
        .quote_asset_amount
        .safe_add(delta.quote_asset_amount)?;

    let new_base_asset_amount = position
        .base_asset_amount
        .safe_add(delta.base_asset_amount)?;

    Ok((new_base_asset_amount, new_quote_asset_amount))
}

#[cfg(test)]
mod negative_expiry_price_tests {
    use {
        super::*,
        crate::{
            math::constants::{BASE_PRECISION_I64, PRICE_PRECISION_I64, QUOTE_PRECISION_I64},
            state::user::PerpPosition,
        },
    };

    /// OtterSec #133 — a negative committed `expiry_price` must not be clipped out of the
    /// margin/equity valuation.
    ///
    /// The live-oracle helper clamps a non-positive price to zero (a negative *oracle* print
    /// is nonsense, so zero is the safe read). Applying that clamp to a legitimately
    /// negative `expiry_price` clipped a long's signed base loss to zero, so margin valued
    /// the position as merely worthless rather than underwater — letting the owner withdraw
    /// collateral — while `settle_expired_position` later booked the real negative value as
    /// an unsecured quote borrow.
    #[test]
    fn negative_expiry_price_is_not_clipped_and_matches_settlement() {
        // Long 1 base, paid 10 quote for it.
        let position = PerpPosition {
            base_asset_amount: BASE_PRECISION_I64,
            quote_asset_amount: -10 * QUOTE_PRECISION_I64,
            ..PerpPosition::default()
        };

        let expiry_price = -5 * PRICE_PRECISION_I64;

        // The clamping (live-oracle) helper reports zero base value, so the pnl is merely
        // the cost basis — the position looks worthless, not underwater. This is the
        // pre-fix reading.
        let (clamped_value, clamped_pnl) =
            calculate_base_asset_value_and_pnl_with_oracle_price(&position, expiry_price).unwrap();
        assert_eq!(clamped_value, 0);
        assert_eq!(clamped_pnl, -10 * QUOTE_PRECISION_I64 as i128);

        // The expiry-price helper keeps the sign, so the loss is visible.
        let (signed_value, signed_pnl) =
            calculate_base_asset_value_and_pnl_with_expiry_price(&position, expiry_price).unwrap();
        assert_eq!(signed_value, (5 * QUOTE_PRECISION_I64) as u128);
        assert_eq!(
            signed_pnl,
            -15 * QUOTE_PRECISION_I64 as i128,
            "a long at -$5 with a $10 basis is $15 underwater"
        );
        assert!(
            signed_pnl < clamped_pnl,
            "the clamp hid {} of loss, which is exactly what let collateral leave",
            clamped_pnl - signed_pnl
        );

        // ...and it now agrees with what settlement actually books.
        let settlement_base_value =
            calculate_base_asset_value_with_expiry_price(&position, expiry_price).unwrap();
        assert!(settlement_base_value < 0);
        assert_eq!(
            settlement_base_value as i128 + position.quote_asset_amount as i128,
            signed_pnl,
            "margin must value the position exactly as settle_expired_position will"
        );

        // A positive expiry price is unchanged by the new variant.
        let positive = 7 * PRICE_PRECISION_I64;
        assert_eq!(
            calculate_base_asset_value_and_pnl_with_expiry_price(&position, positive).unwrap(),
            calculate_base_asset_value_and_pnl_with_oracle_price(&position, positive).unwrap(),
            "the two helpers must only diverge for a negative price"
        );
    }
}
