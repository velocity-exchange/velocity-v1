//! One side of a spot exchange, as a liquidation sees it.
//!
//! Every path that swaps one token for another reads the same shape from a
//! spot market: what the account holds there, the two prices the transfer uses
//! and the weight and multiplier that size it. The readers differ, because
//! only some paths advance the market's oracle TWAPs, so each path builds this
//! itself. What they all share is the shape and the small rules that read it.

use super::*;

/// One side of a spot liquidation: what the account holds there, and the
/// prices and weights the transfer is valued and exchanged at.
pub(crate) struct SpotLiquidationSide {
    /// The token amount the account holds, unsigned.
    pub amount: u128,
    /// The raw oracle price. The shortage and the fees are valued with it,
    /// which keeps them consistent with the margin calculation.
    pub oracle_price: i64,
    /// The price the exchange rate uses. It protects the account when the
    /// oracle is not margin-valid.
    pub price: i64,
    /// The five minute TWAP as it stood before this call refreshed it.
    pub pre_refresh_twap_5min: i64,
    pub decimals: u32,
    /// The maintenance weight the margin calculation applies.
    pub weight: u32,
    /// The liquidator's premium or discount, as a multiplier.
    pub liquidation_multiplier: u32,
    pub pool_id: u8,
    pub oracle_delay: i64,
}

/// Refuse a liquid staking pair priced by a delayed oracle.
pub(crate) fn validate_lst_oracle_delays(
    asset: &SpotLiquidationSide,
    liability: &SpotLiquidationSide,
) -> VelocityResult {
    if asset.pool_id != LST_POOL_ID || liability.pool_id != LST_POOL_ID {
        return Ok(());
    }

    validate!(
        asset.oracle_delay == 0 && liability.oracle_delay == 0,
        ErrorCode::InvalidLiquidation,
        "asset oracle delay ({}) != 0 || liability oracle delay ({}) != 0",
        asset.oracle_delay,
        liability.oracle_delay
    )
}

/// The smallest repayment that must not leave dust behind.
///
/// A borrow worth more than ten dollars may be repaid in part. Anything
/// smaller is repaid whole.
pub(crate) fn minimum_liability_transfer(liability: &SpotLiquidationSide) -> VelocityResult<u128> {
    let liability_value = get_token_value(
        liability.amount.cast()?,
        liability.decimals,
        liability.oracle_price,
    )?;

    if liability_value > 10 * QUOTE_PRECISION_I128 {
        Ok(0)
    } else {
        Ok(liability.amount)
    }
}

/// One fee rate applied to a token transfer.
pub(crate) fn fee_on_transfer(transfer: u128, rate: u32) -> VelocityResult<u128> {
    transfer
        .safe_mul(rate.cast()?)?
        .safe_div(LIQUIDATION_FEE_PRECISION_U128)
}
