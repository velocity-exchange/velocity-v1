use {
    super::constants::{BPS_PRECISION, SPOT_UTILIZATION_PRECISION},
    crate::{
        error::{ErrorCode, VelocityResult},
        math::{casting::Cast, safe_math::SafeMath, spot_balance::get_token_amount},
        msg,
        state::{
            spot_market::{SpotBalance, SpotBalanceType, SpotMarket},
            user::User,
        },
        validate,
    },
};

/// Default withdraw circuit-breaker size when a market has not configured one
/// (i.e. `withdraw_circuit_breaker_bps == 0`): 25% of the 24h deposit TWAP.
/// Keeps markets created before the field existed on prior behavior.
/// precision: basis points (10_000 = 100%)
pub const DEFAULT_WITHDRAW_CIRCUIT_BREAKER_BPS: u16 = (BPS_PRECISION / 4) as u16;

pub fn calculate_min_deposit_token_amount(
    deposit_token_twap: u128,
    withdraw_guard_threshold: u128,
    withdraw_circuit_breaker_bps: u16,
) -> VelocityResult<u128> {
    // minimum required deposit amount after withdrawal
    // minimum deposit amount lower of (100% - breaker pct) of TWAP or withdrawal guard threshold below TWAP
    // for high withdrawal guard threshold, minimum deposit amount is 0

    // `0` is treated as the default 25% so existing on-chain markets (whose
    // field reads 0 from old padding) keep the prior behavior rather than a
    // 0% breaker, which would forbid all withdrawals.
    let breaker_pct = if withdraw_circuit_breaker_bps == 0 {
        DEFAULT_WITHDRAW_CIRCUIT_BREAKER_BPS
    } else {
        withdraw_circuit_breaker_bps
    };

    let max_drop = deposit_token_twap
        .safe_mul(breaker_pct.cast()?)?
        .safe_div(BPS_PRECISION.cast()?)?;

    let min_deposit_token = deposit_token_twap
        .safe_sub(max_drop.max(withdraw_guard_threshold.min(deposit_token_twap)))?;

    Ok(min_deposit_token)
}

pub fn calculate_max_deposit_token_amount(
    deposit_token_twap: u128,
    deposit_guard_threshold: u128,
    max_deposit_bps_per_day: u16,
) -> VelocityResult<u128> {
    // maximum permitted deposit token amount after a deposit
    // mirror of `calculate_min_deposit_token_amount` on the deposit side:
    // allows growth up to `max_deposit_bps_per_day` above the 24h deposit TWAP,
    // but never restricts below the deposit guard threshold.
    // disabled (no cap) when `max_deposit_bps_per_day == 0`.
    if max_deposit_bps_per_day == 0 {
        return Ok(u128::MAX);
    }

    let max_increase = deposit_token_twap
        .safe_mul(max_deposit_bps_per_day.cast()?)?
        .safe_div(BPS_PRECISION.cast()?)?;

    let max_deposit_token = deposit_token_twap
        .safe_add(max_increase)?
        .max(deposit_guard_threshold);

    Ok(max_deposit_token)
}

pub fn check_deposit_limits(spot_market: &SpotMarket) -> VelocityResult<bool> {
    // checks the resulting market deposit level against the daily deposit cap.
    // disabled (always valid) when `max_deposit_bps_per_day == 0`.
    if spot_market.max_deposit_bps_per_day == 0 {
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
        spot_market.max_deposit_bps_per_day,
    )?;

    Ok(deposit_token_amount <= max_deposit_token)
}

/// Enforce the daily deposit cap across a state transition, but only when the market's deposit
/// level actually **grew**.
///
/// `check_deposit_limits` is a level predicate over the whole market ("are total deposits within
/// the cap?"), so validating it unconditionally turns a deposit *rate limit* into a market-wide
/// lock: once the level sits above the cap for any reason, every caller of the shared credit path
/// reverts with `DailyDepositLimit` — including withdrawals and borrow repayments, which lower or
/// leave the deposit level untouched and are precisely the actions that bring a market back under
/// its cap. Liquidation does not route through that path, so the lock is one-sided: users cannot
/// exit or repay while they remain liquidatable (finding #118).
///
/// Gating on growth keeps the cap throttling exactly the operations it is meant to throttle. Pass
/// the market's deposit token amount from before the balance update.
pub fn validate_deposit_cap_after_increase(
    spot_market: &SpotMarket,
    deposit_token_amount_before: u128,
) -> VelocityResult {
    let deposit_token_amount_after = get_token_amount(
        spot_market.deposit_balance,
        spot_market,
        &SpotBalanceType::Deposit,
    )?;

    if deposit_token_amount_after <= deposit_token_amount_before {
        return Ok(());
    }

    validate!(
        check_deposit_limits(spot_market)?,
        ErrorCode::DailyDepositLimit,
        "Spot Market {} has hit daily deposit limit (deposits exceed {} bps above 24h twap)",
        spot_market.market_index,
        spot_market.max_deposit_bps_per_day
    )?;

    Ok(())
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

/// Tests if one account qualifies for an exception to the market withdraw
/// limits. The account qualifies when it holds a deposit, has never net
/// withdrawn more than it net deposited, and held less than one tenth of
/// `withdraw_guard_threshold` in this market before the withdrawal.
///
/// This is an eligibility filter only. The result is per account, so it carries
/// no information about how much the market can afford to release. Callers must
/// combine it with a market-level budget. `check_withdraw_limits` does this with
/// `exception_floor`. A caller that grants a bypass on this predicate alone lets
/// an attacker split one deposit across many accounts and drain the market.
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
        spot_market.withdraw_circuit_breaker_bps,
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

        // The market-level check failed. A small depositor can still get an
        // exception. `check_user_exception_to_withdraw_limits` is the
        // eligibility filter for that exception. It is a per-account predicate,
        // so it must not decide the withdrawal on its own. An attacker splits
        // one large deposit across many accounts. Every account then satisfies
        // the per-account predicate, and the cohort drains the whole market past
        // a tripped breaker. Rent on the extra accounts is refundable, so the
        // cost of the split is near zero.
        //
        // `exception_floor` adds the missing market-level budget. The whole
        // eligible cohort can take the market down by one
        // `withdraw_guard_threshold` below the breaker floor. It can take no
        // more, no matter how many accounts join.
        //
        // The relaxation is one `withdraw_guard_threshold` for two reasons.
        // First, that field already sizes this carve-out. The per-account
        // predicate derives its own allowance from it. Second, it is the only
        // field in this subsystem with an enforced absolute notional cap.
        // `validate_withdraw_guard_threshold` in `validation/spot_market.rs`
        // rejects a value above `MAX_WITHDRAW_GUARD_THRESHOLD_NOTIONAL`, which
        // is $10k. Both `initialize_spot_market` and
        // `update_withdraw_guard_threshold` run that check. Total exception
        // outflow per market per TWAP window is therefore bounded at $10k
        // notional. The budget also regenerates at the same rate as the breaker.
        // As the deposit TWAP decays, `min_deposit_token` falls, and
        // `exception_floor` falls with it.
        //
        // Do not reach for `withdraw_circuit_breaker_bps` to tune this. That
        // field only feeds `calculate_min_deposit_token_amount`. Before this
        // bound existed the exception ignored the field completely, so a change
        // from 2500 bps to 1 bp moved the sybil yield by zero. Change
        // `withdraw_guard_threshold` to size this carve-out.
        //
        // One property of the surrounding code limits the attack to a prepared
        // attacker. A large position cannot be split after the breaker trips.
        // Every path that can shrink a position below the per-account allowance
        // runs this same check. `transfer_deposit`, `transfer_pools` and
        // `end_swap` all go through
        // `update_spot_balances_and_cumulative_deposits_with_limits`. Spot DLOB
        // fills are disabled. The split must happen in advance.
        let exception_floor =
            min_deposit_token.saturating_sub(spot_market.withdraw_guard_threshold.cast::<u128>()?);

        msg!("exception_floor={:?}", exception_floor);

        check_user_exception_to_withdraw_limits(spot_market, user, token_amount_withdrawn)?
            && deposit_token_amount >= exception_floor
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
    let insurance_fund_revenue_receivable =
        get_insurance_fund_revenue_receivable_token_amount(spot_market)?;

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
            spot_market.withdraw_circuit_breaker_bps,
        )?;
        let min_deposit_token = min_deposit_token_for_twap.max(min_deposit_token_for_utilization);
        let withdraw_limit = deposit_token_amount.saturating_sub(min_deposit_token);

        let token_amount = token_amount.unsigned_abs();
        if withdraw_limit <= token_amount && is_leaving_velocity {
            let unreserved_liquidity = deposit_token_amount
                .saturating_sub(borrow_token_amount)
                .saturating_sub(insurance_fund_revenue_receivable);
            return Ok(withdraw_limit.min(unreserved_liquidity));
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

    let max_withdraw_and_borrow = max_withdraw_amount.safe_add(borrow_limit)?;

    if is_leaving_velocity && insurance_fund_revenue_receivable > 0 {
        let unreserved_liquidity = deposit_token_amount
            .saturating_sub(borrow_token_amount)
            .saturating_sub(insurance_fund_revenue_receivable);
        Ok(max_withdraw_and_borrow.min(unreserved_liquidity))
    } else {
        Ok(max_withdraw_and_borrow)
    }
}

pub fn get_insurance_fund_revenue_receivable_token_amount(
    spot_market: &SpotMarket,
) -> VelocityResult<u128> {
    Ok(spot_market.insurance_fund_revenue_receivable as u128)
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

    let insurance_fund_revenue_receivable: u64 =
        get_insurance_fund_revenue_receivable_token_amount(spot_market)?.cast()?;

    let depositors_claim = depositors_amount
        .cast::<i64>()?
        .safe_sub(borrowers_amount.cast()?)?;

    // These pools are Deposit-type balances counted
    // INSIDE deposit_balance (crediting a pool also credits the market
    // total), so these disjoint subsets summed can never exceed the total.
    // This is a corruption tripwire (a pool credited without the total, or
    // the total debited without the pool), not an economic cap — depositor
    // protection is the vault check (`validate_spot_market_vault_amount`)
    // and the withdraw paths. Perp-market pools also live inside
    // deposit_balance but are not visible from the spot account alone, so
    // this check is necessarily partial.
    validate!(
        revenue_amount
            .safe_add(protocol_fee_amount)?
            .safe_add(insurance_fund_revenue_receivable)?
            <= depositors_amount,
        ErrorCode::SpotMarketVaultInvariantViolated,
        "revenue_amount={} + protocol_fee_amount={} + insurance_fund_revenue_receivable={} greater than depositors_amount={} (depositors_claim={}, spot_market.deposit_balance={})",
        revenue_amount,
        protocol_fee_amount,
        insurance_fund_revenue_receivable,
        depositors_amount,
        depositors_claim,
        spot_market.deposit_balance
    )?;

    validate!(
        depositors_claim >= insurance_fund_revenue_receivable.cast::<i64>()?,
        ErrorCode::SpotMarketVaultInvariantViolated,
        "depositors_claim={} lower than reserved insurance fund revenue receivable={}",
        depositors_claim,
        insurance_fund_revenue_receivable
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

    let insurance_fund_revenue_receivable =
        get_insurance_fund_revenue_receivable_token_amount(spot_market)?.cast::<u64>()?;
    validate!(
        vault_amount >= insurance_fund_revenue_receivable,
        ErrorCode::SpotMarketVaultInvariantViolated,
        "spot market vault={} lower than reserved insurance fund revenue receivable={}",
        vault_amount,
        insurance_fund_revenue_receivable
    )?;

    Ok(depositors_claim)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            math::constants::{
                MAX_WITHDRAW_GUARD_THRESHOLD_NOTIONAL, QUOTE_PRECISION, QUOTE_PRECISION_U64,
                SPOT_BALANCE_PRECISION, SPOT_CUMULATIVE_INTEREST_PRECISION,
            },
            state::user::SpotPosition,
        },
    };

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
        let pct = (BPS_PRECISION / 10) as u16; // 1000 bps = 10%
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
        let pct = (BPS_PRECISION / 5) as u16; // 2000 bps = 20%
        let max = calculate_max_deposit_token_amount(twap, 0, pct).unwrap();
        assert_eq!(max, twap + twap / 5);
    }

    #[test]
    fn max_deposit_never_below_guard_threshold() {
        let twap = 10 * QUOTE_PRECISION;
        // small twap but a high guard threshold => deposits allowed up to threshold.
        let guard = 1_000 * QUOTE_PRECISION;
        let pct = (BPS_PRECISION / 5) as u16; // 2000 bps = 20%
        let max = calculate_max_deposit_token_amount(twap, guard, pct).unwrap();
        assert_eq!(max, guard);
    }

    #[test]
    fn insurance_fund_revenue_receivable_is_reserved_from_withdrawals_and_borrows() {
        let mut market = SpotMarket {
            market_index: 0,
            decimals: 6,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            deposit_balance: scaled(1_000 * QUOTE_PRECISION),
            borrow_balance: scaled(100 * QUOTE_PRECISION),
            insurance_fund_revenue_receivable: (200 * QUOTE_PRECISION) as u64,
            ..SpotMarket::default()
        };

        let max_withdraw = get_max_withdraw_for_market_with_token_amount(
            &market,
            (900 * QUOTE_PRECISION) as i128,
            true,
        )
        .unwrap();
        let deposits =
            get_token_amount(market.deposit_balance, &market, &SpotBalanceType::Deposit).unwrap();
        let borrows =
            get_token_amount(market.borrow_balance, &market, &SpotBalanceType::Borrow).unwrap();
        let receivable = get_insurance_fund_revenue_receivable_token_amount(&market).unwrap();
        assert_eq!(
            max_withdraw,
            deposits.saturating_sub(borrows).saturating_sub(receivable)
        );

        market.borrow_balance = scaled(801 * QUOTE_PRECISION);
        assert!(validate_spot_balances(&market).is_err());

        market.borrow_balance = scaled(800 * QUOTE_PRECISION);
        assert!(validate_spot_balances(&market).is_ok());
        assert!(validate_spot_market_vault_amount(&market, (200 * QUOTE_PRECISION) as u64).is_ok());
        assert!(
            validate_spot_market_vault_amount(&market, (200 * QUOTE_PRECISION - 1) as u64).is_err()
        );
    }

    // Fixture for the withdraw-limit exception budget.
    //
    // The numbers are the shipped quote-market (USDT) config in
    // `deploy-scripts/params/relaunch-spot-markets.json`. The market holds
    // 500_000 USDT and the breaker is the default 2500 bps.

    /// Quote-market `withdraw_guard_threshold`, 9_500 tokens at 6 decimals.
    const GUARD: u64 = 9_500 * QUOTE_PRECISION_U64;
    /// 24h deposit TWAP for the market, 500_000 tokens.
    const TWAP: u64 = 500_000 * QUOTE_PRECISION_U64;
    /// Per-account allowance in `check_user_exception_to_withdraw_limits`, 950 tokens.
    const PER_ACCOUNT_ALLOWANCE: u128 = (GUARD / 10) as u128;

    /// Converts a token amount to a scaled balance. The fixture uses 6 decimals
    /// and cumulative interest at precision, so the ratio is constant.
    fn scaled(token_amount: u128) -> u128 {
        token_amount * (SPOT_BALANCE_PRECISION / QUOTE_PRECISION)
    }

    fn breaker_market(deposit_token_amount: u128) -> SpotMarket {
        SpotMarket {
            market_index: 0,
            decimals: 6,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            deposit_balance: scaled(deposit_token_amount),
            borrow_balance: 0,
            deposit_token_twap: TWAP,
            withdraw_guard_threshold: GUARD,
            withdraw_circuit_breaker_bps: 2_500,
            ..SpotMarket::default()
        }
    }

    /// Builds an account that qualifies for the exception. `remaining` is the
    /// balance left after the withdrawal. `lifetime` is the amount the account
    /// ever deposited. Net deposits are zero, which is the strongest position an
    /// honest depositor can be in.
    fn eligible_account(remaining: u128, lifetime: u128) -> User {
        let mut user = User {
            total_deposits: lifetime as u64,
            total_withdraws: lifetime as u64,
            ..User::default()
        };
        user.spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: scaled(remaining) as u64,
            cumulative_deposits: remaining as i64,
            ..SpotPosition::default()
        };
        user
    }

    /// Runs one full-balance withdrawal in program order. The program debits the
    /// balances first and calls `check_withdraw_limits` after. A rejected
    /// withdrawal aborts the transaction, so the debit is rolled back here.
    ///
    /// Returns `(eligible, allowed)`. `eligible` is the per-account predicate on
    /// its own. `allowed` is the full decision.
    fn try_withdraw_all(market: &mut SpotMarket, amount: u128) -> (bool, bool) {
        let user = eligible_account(0, amount);
        let debit = scaled(amount);

        market.deposit_balance -= debit;
        let eligible =
            check_user_exception_to_withdraw_limits(market, Some(&user), Some(amount)).unwrap();
        let allowed = check_withdraw_limits(market, Some(&user), Some(amount)).unwrap();
        if !allowed {
            market.deposit_balance += debit;
        }

        (eligible, allowed)
    }

    #[test]
    fn withdraw_exception_still_frees_a_small_depositor() {
        // The breaker floor for this market. The TWAP is 500_000 USDT and the
        // breaker is 2500 bps, so deposits may not fall below 375_000 USDT.
        let floor = calculate_min_deposit_token_amount(TWAP as u128, GUARD as u128, 2_500).unwrap();
        assert_eq!(floor, 375_000 * QUOTE_PRECISION);

        // The market sits exactly on the floor. The market-level check passes at
        // the floor and fails one token unit below it, so any withdrawal from
        // here needs the exception.
        let mut market = breaker_market(floor);
        assert!(check_withdraw_limits(&market, None, None).unwrap());
        assert!(!check_withdraw_limits(&breaker_market(floor - 1), None, None).unwrap());

        // A 500 USDT depositor is below the 950 USDT per-account allowance and
        // still exits in full.
        let amount = 500 * QUOTE_PRECISION;
        assert!(amount < PER_ACCOUNT_ALLOWANCE);
        let (eligible, allowed) = try_withdraw_all(&mut market, amount);
        assert!(eligible);
        assert!(allowed);

        let remaining =
            get_token_amount(market.deposit_balance, &market, &SpotBalanceType::Deposit).unwrap();
        assert_eq!(remaining, floor - amount);
    }

    #[test]
    fn withdraw_exception_cohort_cannot_exceed_one_guard_threshold() {
        // The guard threshold is the whole exception budget for the market. An
        // admin cannot raise it above 10_000 USDT of notional, because
        // `validate_withdraw_guard_threshold` rejects that on both
        // `initialize_spot_market` and `update_withdraw_guard_threshold`.
        assert!(GUARD as u128 <= MAX_WITHDRAW_GUARD_THRESHOLD_NOTIONAL);

        let floor = calculate_min_deposit_token_amount(TWAP as u128, GUARD as u128, 2_500).unwrap();
        let mut market = breaker_market(floor);

        // 600 prepared accounts, each holding 900 USDT. Every one is below the
        // 950 USDT per-account allowance, so every one passes the eligibility
        // predicate. They want 540_000 USDT in total.
        let per_account = 900 * QUOTE_PRECISION;
        assert!(per_account < PER_ACCOUNT_ALLOWANCE);
        let cohort_size = 600_u128;
        let cohort_demand = per_account * cohort_size;
        assert!(cohort_demand > GUARD as u128);

        let mut extracted = 0_u128;
        let mut accounts_paid = 0_u128;
        let mut rejected_but_eligible = 0_u128;

        for _ in 0..cohort_size {
            let (eligible, allowed) = try_withdraw_all(&mut market, per_account);
            assert!(eligible, "every account in the cohort must stay eligible");
            if allowed {
                extracted += per_account;
                accounts_paid += 1;
            } else {
                // Eligibility alone did not pay this account. The market-level
                // budget is what stopped it.
                rejected_but_eligible += 1;
            }
        }

        // Ten accounts drained 9_000 USDT. The eleventh would have taken the
        // market 9_900 USDT below the floor, which is more than one guard
        // threshold, so it and every account after it got nothing.
        assert_eq!(accounts_paid, 10);
        assert_eq!(extracted, 9_000 * QUOTE_PRECISION);
        assert_eq!(rejected_but_eligible, cohort_size - 10);

        // The money bound. Sybil yield is capped at one guard threshold per
        // market per TWAP window, not at one allowance per account.
        assert!(extracted <= GUARD as u128);
        assert!(extracted < cohort_demand);

        let remaining =
            get_token_amount(market.deposit_balance, &market, &SpotBalanceType::Deposit).unwrap();
        assert_eq!(remaining, floor - extracted);
        assert_eq!(remaining, floor - GUARD as u128 + 500 * QUOTE_PRECISION);
        assert!(remaining >= floor - GUARD as u128);
    }
}
