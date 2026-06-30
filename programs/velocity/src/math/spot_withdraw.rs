use crate::msg;

use crate::error::{ErrorCode, VelocityResult};
use crate::math::casting::Cast;
use crate::math::safe_math::SafeMath;

use crate::math::spot_balance::get_token_amount;
use crate::state::spot_market::{SpotBalance, SpotBalanceType, SpotMarket};
use crate::state::user::User;
use crate::validate;

use super::constants::{
    PERCENTAGE_PRECISION, PERCENTAGE_PRECISION_U32, SPOT_UTILIZATION_PRECISION,
};

/// Default withdraw circuit-breaker size when a market has not configured one
/// (i.e. `withdraw_circuit_breaker_pct == 0`): 25% of the 24h deposit TWAP.
/// Keeps markets created before the field existed on prior behavior.
pub const DEFAULT_WITHDRAW_CIRCUIT_BREAKER_PCT: u32 = PERCENTAGE_PRECISION_U32 / 4;

pub fn calculate_min_deposit_token_amount(
    deposit_token_twap: u128,
    withdraw_guard_threshold: u128,
    withdraw_circuit_breaker_pct: u32,
) -> VelocityResult<u128> {
    // minimum required deposit amount after withdrawal
    // minimum deposit amount lower of (100% - breaker pct) of TWAP or withdrawal guard threshold below TWAP
    // for high withdrawal guard threshold, minimum deposit amount is 0

    // `0` is treated as the default 25% so existing on-chain markets (whose
    // field reads 0 from old padding) keep the prior behavior rather than a
    // 0% breaker, which would forbid all withdrawals.
    let breaker_pct = if withdraw_circuit_breaker_pct == 0 {
        DEFAULT_WITHDRAW_CIRCUIT_BREAKER_PCT
    } else {
        withdraw_circuit_breaker_pct
    };

    let max_drop = deposit_token_twap
        .safe_mul(breaker_pct.cast()?)?
        .safe_div(PERCENTAGE_PRECISION)?;

    let min_deposit_token = deposit_token_twap
        .safe_sub(max_drop.max(withdraw_guard_threshold.min(deposit_token_twap)))?;

    Ok(min_deposit_token)
}

pub fn calculate_max_deposit_token_amount(
    deposit_token_twap: u128,
    deposit_guard_threshold: u128,
    max_deposit_pct_per_day: u32,
) -> VelocityResult<u128> {
    // maximum permitted deposit token amount after a deposit
    // mirror of `calculate_min_deposit_token_amount` on the deposit side:
    // allows growth up to `max_deposit_pct_per_day` above the 24h deposit TWAP,
    // but never restricts below the deposit guard threshold.
    // disabled (no cap) when `max_deposit_pct_per_day == 0`.
    if max_deposit_pct_per_day == 0 {
        return Ok(u128::MAX);
    }

    let max_increase = deposit_token_twap
        .safe_mul(max_deposit_pct_per_day.cast()?)?
        .safe_div(PERCENTAGE_PRECISION)?;

    let max_deposit_token = deposit_token_twap
        .safe_add(max_increase)?
        .max(deposit_guard_threshold);

    Ok(max_deposit_token)
}

pub fn check_deposit_limits(spot_market: &SpotMarket) -> VelocityResult<bool> {
    // checks the resulting market deposit level against the daily deposit cap.
    // disabled (always valid) when `max_deposit_pct_per_day == 0`.
    if spot_market.max_deposit_pct_per_day == 0 {
        return Ok(true);
    }

    let deposit_token_amount = get_token_amount(
        spot_market.deposit_balance,
        spot_market,
        &SpotBalanceType::Deposit,
    )?;

    let max_deposit_token = calculate_max_deposit_token_amount(
        spot_market.deposit_token_twap.cast()?,
        spot_market.deposit_guard_threshold.cast()?,
        spot_market.max_deposit_pct_per_day,
    )?;

    Ok(deposit_token_amount <= max_deposit_token)
}

pub fn calculate_max_borrow_token_amount(
    deposit_token_amount: u128,
    deposit_token_twap: u128,
    borrow_token_twap: u128,
    withdraw_guard_threshold: u128,
    max_token_borrows: u128,
    pool_id: u8,
) -> VelocityResult<u128> {
    // maximum permitted borrows after withdrawal
    // allows at least up to the withdraw_guard_threshold

    let lesser_deposit_amount = deposit_token_amount.min(deposit_token_twap);

    let max_borrow_token = if pool_id == 0 {
        // main pool between ~30-92.5% utilization with friction on twap in 20% increments

        withdraw_guard_threshold
            .max(
                (lesser_deposit_amount / 3)
                    .max(borrow_token_twap.safe_add(lesser_deposit_amount / 5)?)
                    .min(lesser_deposit_amount.safe_sub(lesser_deposit_amount / 14)?),
            )
            .min(max_token_borrows)
    } else {
        // isolated pools between 50-95% utilization with friction on twap in 33% increments
        withdraw_guard_threshold
            .max(
                (lesser_deposit_amount / 2)
                    .max(borrow_token_twap.safe_add(lesser_deposit_amount / 3)?)
                    .min(lesser_deposit_amount.safe_sub(lesser_deposit_amount / 20)?),
            )
            .min(max_token_borrows)
    };

    Ok(max_borrow_token)
}

pub fn check_user_exception_to_withdraw_limits(
    spot_market: &SpotMarket,
    user: Option<&User>,
    token_amount_withdrawn: Option<u128>,
) -> VelocityResult<bool> {
    // allow a smaller user in a market to bypass and withdraw their principal
    let mut valid_user_withdraw = false;
    if let Some(user) = user {
        let spot_position = user.get_spot_position(spot_market.market_index)?;
        let net_deposits = user
            .total_deposits
            .cast::<i128>()?
            .safe_sub(user.total_withdraws.cast::<i128>()?)?;
        msg!(
            "net_deposits={}({}-{})",
            net_deposits,
            user.total_deposits,
            user.total_withdraws
        );
        if net_deposits >= 0
            && spot_position.cumulative_deposits >= 0
            && spot_position.balance_type == SpotBalanceType::Deposit
        {
            if let Some(token_amount_withdrawn) = token_amount_withdrawn {
                let user_deposit_token_amount = get_token_amount(
                    spot_position.scaled_balance.cast::<u128>()?,
                    spot_market,
                    &spot_position.balance_type,
                )?;

                if user_deposit_token_amount.safe_add(token_amount_withdrawn)?
                    < spot_market
                        .withdraw_guard_threshold
                        .cast::<u128>()?
                        .safe_div(10)?
                {
                    valid_user_withdraw = true;
                }
            }
        }
    }

    Ok(valid_user_withdraw)
}

pub fn calculate_token_utilization_limits(
    deposit_token_amount: u128,
    borrow_token_amount: u128,
    spot_market: &SpotMarket,
) -> VelocityResult<(u128, u128)> {
    // Calculates the allowable minimum deposit and maximum borrow amounts after withdrawal based on market utilization.
    // First, it determines a maximum withdrawal utilization from the market's target and historic utilization.
    // Then, it deduces corresponding deposit/borrow amounts.
    // Note: For deposit sizes below the guard threshold, withdrawals aren't blocked.

    let max_withdraw_utilization: u128 = spot_market.optimal_utilization.cast::<u128>()?.max(
        spot_market.utilization_twap.cast::<u128>()?.safe_add(
            SPOT_UTILIZATION_PRECISION.saturating_sub(spot_market.utilization_twap.cast()?) / 2,
        )?,
    );

    let mut min_deposit_tokens_for_utilization = borrow_token_amount
        .safe_mul(SPOT_UTILIZATION_PRECISION)?
        .safe_div(max_withdraw_utilization)?;

    // dont block withdraws for deposit sizes below guard threshold
    min_deposit_tokens_for_utilization = min_deposit_tokens_for_utilization
        .min(deposit_token_amount.saturating_sub(spot_market.withdraw_guard_threshold.cast()?));

    let mut max_borrow_tokens_for_utilization = max_withdraw_utilization
        .safe_mul(deposit_token_amount)?
        .safe_div(SPOT_UTILIZATION_PRECISION)?;

    // dont block borrows for sizes below guard threshold
    max_borrow_tokens_for_utilization =
        max_borrow_tokens_for_utilization.max(spot_market.withdraw_guard_threshold.cast()?);

    Ok((
        min_deposit_tokens_for_utilization,
        max_borrow_tokens_for_utilization,
    ))
}

pub fn check_withdraw_limits(
    spot_market: &SpotMarket,
    user: Option<&User>,
    token_amount_withdrawn: Option<u128>,
) -> VelocityResult<bool> {
    // calculates min/max deposit/borrow amounts permitted for immediate withdraw
    // takes the stricter of absolute caps on level changes and utilization changes vs 24hr moving averrages
    let deposit_token_amount = get_token_amount(
        spot_market.deposit_balance,
        spot_market,
        &SpotBalanceType::Deposit,
    )?;
    let borrow_token_amount = get_token_amount(
        spot_market.borrow_balance,
        spot_market,
        &SpotBalanceType::Borrow,
    )?;

    let max_token_borrows: u128 = if spot_market.max_token_borrows_fraction > 0 {
        spot_market
            .max_token_deposits
            .safe_mul(spot_market.max_token_borrows_fraction.cast()?)?
            .safe_div(10000)?
            .cast()?
    } else {
        u128::MAX
    };

    let max_borrow_token_for_twap = calculate_max_borrow_token_amount(
        deposit_token_amount,
        spot_market.deposit_token_twap.cast()?,
        spot_market.borrow_token_twap.cast()?,
        spot_market.withdraw_guard_threshold.cast()?,
        max_token_borrows,
        spot_market.pool_id,
    )?;

    let (min_deposit_token_for_utilization, max_borrow_token_for_utilization) =
        calculate_token_utilization_limits(deposit_token_amount, borrow_token_amount, spot_market)?;

    let max_borrow_token = max_borrow_token_for_twap.min(max_borrow_token_for_utilization);

    let min_deposit_token_for_twap = calculate_min_deposit_token_amount(
        spot_market.deposit_token_twap.cast()?,
        spot_market.withdraw_guard_threshold.cast()?,
        spot_market.withdraw_circuit_breaker_pct,
    )?;

    let min_deposit_token = min_deposit_token_for_twap.max(min_deposit_token_for_utilization);

    // for resulting deposit or ZERO, check if deposits above minimum
    // for resulting borrow, check both deposit and borrow constraints
    let valid_global_withdrawal = if let Some(user) = user {
        let spot_position_index = user.get_spot_position_index(spot_market.market_index)?;
        if user.spot_positions[spot_position_index].balance_type() == &SpotBalanceType::Borrow {
            borrow_token_amount <= max_borrow_token && deposit_token_amount >= min_deposit_token
        } else {
            deposit_token_amount >= min_deposit_token
        }
    } else {
        deposit_token_amount >= min_deposit_token && borrow_token_amount <= max_borrow_token
    };

    let valid_withdrawal = if !valid_global_withdrawal {
        msg!(
            "withdraw_guard_threshold={:?}",
            spot_market.withdraw_guard_threshold
        );
        msg!("min_deposit_token={:?}", min_deposit_token);
        msg!("deposit_token_amount={:?}", deposit_token_amount);
        msg!("max_borrow_token={:?}", max_borrow_token);
        msg!("borrow_token_amount={:?}", borrow_token_amount);

        check_user_exception_to_withdraw_limits(spot_market, user, token_amount_withdrawn)?
    } else {
        true
    };

    Ok(valid_withdrawal)
}

pub fn get_max_withdraw_for_market_with_token_amount(
    spot_market: &SpotMarket,
    token_amount: i128,
    is_leaving_velocity: bool,
) -> VelocityResult<u128> {
    let deposit_token_amount = get_token_amount(
        spot_market.deposit_balance,
        spot_market,
        &SpotBalanceType::Deposit,
    )?;

    let borrow_token_amount = get_token_amount(
        spot_market.borrow_balance,
        spot_market,
        &SpotBalanceType::Borrow,
    )?;

    // if leaving velocity, need to consider utilization limits
    let (min_deposit_token_for_utilization, max_borrow_token_for_utilization) =
        if is_leaving_velocity {
            calculate_token_utilization_limits(
                deposit_token_amount,
                borrow_token_amount,
                spot_market,
            )?
        } else {
            (0, u128::MAX)
        };

    let mut max_withdraw_amount = 0_u128;
    if token_amount > 0 {
        let min_deposit_token_for_twap = calculate_min_deposit_token_amount(
            spot_market.deposit_token_twap.cast()?,
            spot_market.withdraw_guard_threshold.cast()?,
            spot_market.withdraw_circuit_breaker_pct,
        )?;
        let min_deposit_token = min_deposit_token_for_twap.max(min_deposit_token_for_utilization);
        let withdraw_limit = deposit_token_amount.saturating_sub(min_deposit_token);

        let token_amount = token_amount.unsigned_abs();
        if withdraw_limit <= token_amount && is_leaving_velocity {
            return Ok(withdraw_limit);
        }

        max_withdraw_amount = token_amount;
    }

    let max_token_borrows: u128 = if spot_market.max_token_borrows_fraction > 0 {
        spot_market
            .max_token_deposits
            .safe_mul(spot_market.max_token_borrows_fraction.cast()?)?
            .safe_div(10000)?
            .cast()?
    } else {
        u128::MAX
    };

    let max_borrow_token_for_twap = calculate_max_borrow_token_amount(
        deposit_token_amount,
        spot_market.deposit_token_twap.cast()?,
        spot_market.borrow_token_twap.cast()?,
        spot_market.withdraw_guard_threshold.cast()?,
        max_token_borrows,
        spot_market.pool_id,
    )?;

    let max_borrow_token = max_borrow_token_for_twap.min(max_borrow_token_for_utilization);

    let mut borrow_limit = max_borrow_token
        .saturating_sub(borrow_token_amount)
        .min(deposit_token_amount.saturating_sub(borrow_token_amount));

    if spot_market.max_token_borrows_fraction > 0 {
        // min with max allowed borrows
        let borrows = spot_market.get_borrows()?;
        let max_token_borrows = spot_market
            .max_token_deposits
            .safe_mul(spot_market.max_token_borrows_fraction.cast()?)?
            .safe_div(10000)?
            .cast::<u128>()?;
        borrow_limit = borrow_limit.min(max_token_borrows.saturating_sub(borrows));
    }

    max_withdraw_amount.safe_add(borrow_limit)
}

pub fn validate_spot_balances(spot_market: &SpotMarket) -> VelocityResult<i64> {
    let depositors_amount: u64 = get_token_amount(
        spot_market.deposit_balance,
        spot_market,
        &SpotBalanceType::Deposit,
    )?
    .cast()?;
    let borrowers_amount: u64 = get_token_amount(
        spot_market.borrow_balance,
        spot_market,
        &SpotBalanceType::Borrow,
    )?
    .cast()?;

    let revenue_amount: u64 = get_token_amount(
        spot_market.revenue_pool.scaled_balance,
        spot_market,
        &SpotBalanceType::Deposit,
    )?
    .cast()?;

    let protocol_fee_amount: u64 = get_token_amount(
        spot_market.protocol_fee_pool.scaled_balance,
        spot_market,
        &SpotBalanceType::Deposit,
    )?
    .cast()?;

    let depositors_claim = depositors_amount
        .cast::<i64>()?
        .safe_sub(borrowers_amount.cast()?)?;

    // revenue_pool and protocol_fee_pool are Deposit-type balances counted
    // INSIDE deposit_balance (crediting a pool also credits the market
    // total), so these disjoint subsets summed can never exceed the total.
    // This is a corruption tripwire (a pool credited without the total, or
    // the total debited without the pool), not an economic cap — depositor
    // protection is the vault check (`validate_spot_market_vault_amount`)
    // and the withdraw paths. Perp-market pools also live inside
    // deposit_balance but are not visible from the spot account alone, so
    // this check is necessarily partial.
    validate!(
        revenue_amount.safe_add(protocol_fee_amount)? <= depositors_amount,
        ErrorCode::SpotMarketVaultInvariantViolated,
        "revenue_amount={} + protocol_fee_amount={} greater than depositors_amount={} (depositors_claim={}, spot_market.deposit_balance={})",
        revenue_amount,
        protocol_fee_amount,
        depositors_amount,
        depositors_claim,
        spot_market.deposit_balance
    )?;

    Ok(depositors_claim)
}

pub fn validate_spot_market_vault_amount(
    spot_market: &SpotMarket,
    vault_amount: u64,
) -> VelocityResult<i64> {
    let depositors_claim = validate_spot_balances(spot_market)?;

    validate!(
        vault_amount.cast::<i64>()? >= depositors_claim,
        ErrorCode::SpotMarketVaultInvariantViolated,
        "spot market vault ={} holds less than remaining depositor claims = {}",
        vault_amount,
        depositors_claim
    )?;

    Ok(depositors_claim)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::constants::QUOTE_PRECISION;

    #[test]
    fn min_deposit_zero_pct_defaults_to_25_percent() {
        // 0 must reproduce the legacy 25% drop (min deposit = 75% of twap).
        let twap = 100 * QUOTE_PRECISION;
        let min = calculate_min_deposit_token_amount(twap, 0, 0).unwrap();
        assert_eq!(min, twap - twap / 4);
    }

    #[test]
    fn min_deposit_configurable_pct_tightens() {
        let twap = 100 * QUOTE_PRECISION;
        // 10% breaker => can only withdraw 10%, min deposit = 90% of twap.
        let pct = PERCENTAGE_PRECISION_U32 / 10;
        let min = calculate_min_deposit_token_amount(twap, 0, pct).unwrap();
        assert_eq!(min, twap - twap / 10);
    }

    #[test]
    fn min_deposit_guard_threshold_loosens_when_below_breaker() {
        let twap = 100 * QUOTE_PRECISION;
        // guard threshold (40%) > breaker drop (25%) => allow draining down by 40%.
        let guard = 40 * QUOTE_PRECISION;
        let min = calculate_min_deposit_token_amount(twap, guard, 0).unwrap();
        assert_eq!(min, twap - guard);
    }

    #[test]
    fn max_deposit_disabled_when_pct_zero() {
        let twap = 100 * QUOTE_PRECISION;
        let max = calculate_max_deposit_token_amount(twap, 0, 0).unwrap();
        assert_eq!(max, u128::MAX);
    }

    #[test]
    fn max_deposit_caps_growth_above_twap() {
        let twap = 100 * QUOTE_PRECISION;
        // 20%/day => resulting deposits capped at 120% of twap.
        let pct = PERCENTAGE_PRECISION_U32 / 5;
        let max = calculate_max_deposit_token_amount(twap, 0, pct).unwrap();
        assert_eq!(max, twap + twap / 5);
    }

    #[test]
    fn max_deposit_never_below_guard_threshold() {
        let twap = 10 * QUOTE_PRECISION;
        // small twap but a high guard threshold => deposits allowed up to threshold.
        let guard = 1_000 * QUOTE_PRECISION;
        let pct = PERCENTAGE_PRECISION_U32 / 5;
        let max = calculate_max_deposit_token_amount(twap, guard, pct).unwrap();
        assert_eq!(max, guard);
    }
}
