#[cfg(feature = "velocity-rs")]
use crate::math::constants::PERCENTAGE_PRECISION;
use crate::{
    error::{ErrorCode, VelocityResult},
    math::{
        casting::Cast,
        constants::{
            IF_FACTOR_PRECISION, INTEREST_RATE_SEGMENT_AND_WEIGHTS, ONE_YEAR, SPOT_RATE_PRECISION,
            SPOT_UTILIZATION_PRECISION,
        },
        safe_math::{SafeDivFloor, SafeMath},
    },
    state::{
        oracle::{OraclePriceData, StrictOraclePrice},
        spot_market::{SpotBalanceType, SpotMarket},
        user::SpotPosition,
    },
};

#[cfg(test)]
mod tests;

pub fn get_spot_balance(
    token_amount: u128,
    spot_market: &SpotMarket,
    balance_type: &SpotBalanceType,
    round_up: bool,
) -> VelocityResult<u128> {
    let precision_increase = 10_u128.pow(19_u32.safe_sub(spot_market.decimals)?);

    let cumulative_interest = match balance_type {
        SpotBalanceType::Deposit => spot_market.cumulative_deposit_interest,
        SpotBalanceType::Borrow => spot_market.cumulative_borrow_interest,
    };

    let mut balance = token_amount
        .safe_mul(precision_increase)?
        .safe_div(cumulative_interest)?;

    if round_up && token_amount != 0 {
        balance = balance.safe_add(1)?;
    }

    Ok(balance)
}

pub fn get_token_amount(
    balance: u128,
    spot_market: &SpotMarket,
    balance_type: &SpotBalanceType,
) -> VelocityResult<u128> {
    let precision_decrease = 10_u128.pow(19_u32.safe_sub(spot_market.decimals)?);

    let cumulative_interest = match balance_type {
        SpotBalanceType::Deposit => spot_market.cumulative_deposit_interest,
        SpotBalanceType::Borrow => spot_market.cumulative_borrow_interest,
    };

    let token_amount = match balance_type {
        SpotBalanceType::Deposit => balance
            .safe_mul(cumulative_interest)?
            .safe_div(precision_decrease)?,
        SpotBalanceType::Borrow => balance
            .safe_mul(cumulative_interest)?
            .safe_div_ceil(precision_decrease)?,
    };

    Ok(token_amount)
}

pub fn get_signed_token_amount(
    token_amount: u128,
    balance_type: &SpotBalanceType,
) -> VelocityResult<i128> {
    match balance_type {
        SpotBalanceType::Deposit => token_amount.cast(),
        SpotBalanceType::Borrow => token_amount
            .cast::<i128>()
            .map(|token_amount| -token_amount),
    }
}

pub fn get_interest_token_amount(
    balance: u128,
    spot_market: &SpotMarket,
    interest: u128,
) -> VelocityResult<u128> {
    let precision_decrease = 10_u128.pow(19_u32.safe_sub(spot_market.decimals)?);

    let token_amount = balance.safe_mul(interest)?.safe_div(precision_decrease)?;

    Ok(token_amount)
}

/// How one interval's deposit interest divides between lenders and the two
/// carveout pools, plus the remainders to carry into the next interval.
#[derive(Default)]
pub struct InterestSplit {
    /// Added to `cumulative_deposit_interest`.
    /// precision: SPOT_CUMULATIVE_INTEREST_PRECISION
    pub for_lenders: u128,
    /// Withheld for the insurance fund, before conversion to tokens.
    /// precision: SPOT_CUMULATIVE_INTEREST_PRECISION
    pub for_insurance_fund: u128,
    /// Withheld for the protocol, before conversion to tokens.
    /// precision: SPOT_CUMULATIVE_INTEREST_PRECISION
    pub for_protocol: u128,
    /// New `revenue_pool.pending_interest_split_dust`
    pub carveout_dust: u32,
    /// New `protocol_fee_pool.pending_interest_split_dust`
    pub insurance_fund_dust: u32,
}

/// Divide an interval's deposit interest between lenders, the insurance fund and
/// the protocol. Carry the amounts that do not reach a whole index unit.
///
/// A one-second interval produces only a few index units of `deposit_interest`. A
/// factor below one percent then rounds the carveout to zero. The size of the
/// market does not help, because the factor multiply happens in index space,
/// after the market size divides out. Frequent cranks of this permissionless
/// accrual therefore held every cut under that floor. The insurance fund and the
/// protocol lost their whole share of lending yield. Finding #127 describes this.
/// This function carries each remainder and adds it back on the next interval.
///
/// The function splits twice, in order. The first split separates lenders from
/// the combined carveout. The second split separates the insurance fund from the
/// protocol, inside the amount that the first split withheld.
///
/// That order makes `for_insurance_fund + for_protocol <= deposit_interest` a
/// property of the arithmetic. A clamp does not have to enforce it. Two
/// independent cuts can instead each round up by one unit, take the whole
/// interval, and leave lenders at zero. A zero lender share once stopped the
/// interval from committing, which billed the span against later balances.
/// Findings #115 and #117 describe that result.
///
/// `update_spot_market_if_factor` holds `if_fee_factor + protocol_fee_factor`
/// below `IF_FACTOR_PRECISION`. That bound limits the first split's numerator.
/// The same instruction can lower the pair at any time, so the second split's
/// divisor is not a constant. The carry it stored under a larger divisor is
/// reduced below the divisor in force before it is used.
pub fn split_deposit_interest(
    spot_market: &SpotMarket,
    deposit_interest: u128,
) -> VelocityResult<InterestSplit> {
    let if_factor = spot_market.insurance_fund.if_fee_factor.cast::<u128>()?;
    let combined_factor = if_factor.safe_add(spot_market.protocol_fee_factor.cast::<u128>()?)?;

    let carried_carveout = spot_market
        .revenue_pool
        .pending_interest_split_dust
        .cast::<u128>()?;
    let carried_if = spot_market
        .protocol_fee_pool
        .pending_interest_split_dust
        .cast::<u128>()?;

    // The second split divides by the combined factor, and the admin can lower that
    // factor at any time. A remainder is only valid below the divisor that stored
    // it. Under a smaller divisor the stored remainder raises the insurance fund cut
    // above the withheld amount, and the protocol residual then underflows. The
    // accrual runs first on nearly every spot instruction, so the market would take
    // no deposit, withdrawal, borrow or repayment until the factors went back up.
    //
    // The carry is therefore reduced below the divisor in force. Each change of the
    // factors forfeits less than one index unit. A combined factor of zero drops the
    // carry, because the split that it belongs to no longer exists. The first split
    // needs no reduction, because its divisor is the constant `IF_FACTOR_PRECISION`.
    let carried_if = carried_if.min(combined_factor.saturating_sub(1));

    // Nothing configured and nothing in flight: lenders take the whole interval.
    // Most markets run this way and this runs on nearly every spot instruction.
    if combined_factor == 0 && carried_carveout == 0 && carried_if == 0 {
        return Ok(InterestSplit {
            for_lenders: deposit_interest,
            ..InterestSplit::default()
        });
    }

    // Split one: lenders vs. the carveouts as a whole.
    //
    // `combined_factor < IF_FACTOR_PRECISION` and `carried_carveout <
    // IF_FACTOR_PRECISION` bound the numerator by
    // `(deposit_interest + 1) * (IF_FACTOR_PRECISION - 1)`, so the quotient is at
    // most `deposit_interest` and lenders can never go negative.
    let carveout_numerator = deposit_interest
        .safe_mul(combined_factor)?
        .safe_add(carried_carveout)?;
    let withheld = carveout_numerator.safe_div(IF_FACTOR_PRECISION)?;
    let carveout_dust = carveout_numerator
        .safe_sub(withheld.safe_mul(IF_FACTOR_PRECISION)?)?
        .cast::<u32>()?;

    let for_lenders = deposit_interest.safe_sub(withheld)?;

    // Split two: the insurance fund's share of what was withheld, with the
    // protocol taking the exact residual so the pair always sums back to
    // `withheld`. `if_factor <= combined_factor` and `carried_if <
    // combined_factor` hold the numerator below `(withheld + 1) *
    // combined_factor`, so `for_insurance_fund <= withheld` and the residual never
    // underflows.
    let (for_insurance_fund, insurance_fund_dust) = if combined_factor == 0 {
        // No carveout is configured, so this split has no divisor. The reduction
        // above already dropped the carry.
        (0, 0)
    } else {
        let if_numerator = withheld.safe_mul(if_factor)?.safe_add(carried_if)?;
        let for_insurance_fund = if_numerator.safe_div(combined_factor)?;
        let dust = if_numerator
            .safe_sub(for_insurance_fund.safe_mul(combined_factor)?)?
            .cast::<u32>()?;
        (for_insurance_fund, dust)
    };

    Ok(InterestSplit {
        for_lenders,
        for_insurance_fund,
        for_protocol: withheld.safe_sub(for_insurance_fund)?,
        carveout_dust,
        insurance_fund_dust,
    })
}

/// Convert a withheld carveout to whole tokens. Also return the sub-token
/// remainder for the next interval.
///
/// This is the conversion that [`get_interest_token_amount`] performs. It differs
/// in two ways. It returns the part that the division would discard. It also adds
/// the remainder from earlier intervals before it divides.
///
/// On a small market this division floors to zero even when the index-space cut
/// is not zero. Lenders have already given up the value at that point. A floored
/// cut therefore credits nobody and leaves unattributed slack in the vault.
///
/// `carried_dust` uses the numerator units of that division, which are
/// `token * 10^(19 - decimals)`. It always stays below one token, so the returned
/// remainder does too. See `PoolBalance::pending_interest_dust`.
pub fn get_interest_token_amount_with_dust(
    balance: u128,
    spot_market: &SpotMarket,
    interest: u128,
    carried_dust: u64,
) -> VelocityResult<(u128, u64)> {
    let precision_decrease = 10_u128.pow(19_u32.safe_sub(spot_market.decimals)?);

    let numerator = balance
        .safe_mul(interest)?
        .safe_add(carried_dust.cast::<u128>()?)?;

    let token_amount = numerator.safe_div(precision_decrease)?;
    let dust = numerator
        .safe_sub(token_amount.safe_mul(precision_decrease)?)?
        .cast::<u64>()?;

    Ok((token_amount, dust))
}

pub struct InterestAccumulated {
    pub borrow_interest: u128,
    pub deposit_interest: u128,
}

pub fn calculate_utilization(
    deposit_token_amount: u128,
    borrow_token_amount: u128,
) -> VelocityResult<u128> {
    let utilization = borrow_token_amount
        .safe_mul(SPOT_UTILIZATION_PRECISION)?
        .checked_div(deposit_token_amount)
        .unwrap_or({
            if deposit_token_amount == 0 && borrow_token_amount == 0 {
                0_u128
            } else {
                // if there are borrows without deposits, default to maximum utilization rate
                SPOT_UTILIZATION_PRECISION
            }
        });

    Ok(utilization)
}

pub fn calculate_spot_market_utilization(spot_market: &SpotMarket) -> VelocityResult<u128> {
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
    let utilization = calculate_utilization(deposit_token_amount, borrow_token_amount)?;

    Ok(utilization)
}

pub fn calculate_accumulated_interest(
    spot_market: &SpotMarket,
    now: i64,
) -> VelocityResult<InterestAccumulated> {
    if now <= spot_market.last_interest_ts.cast()? {
        return Ok(InterestAccumulated {
            borrow_interest: 0,
            deposit_interest: 0,
        });
    }

    let utilization = calculate_spot_market_utilization(spot_market)?;

    if utilization == 0 {
        return Ok(InterestAccumulated {
            borrow_interest: 0,
            deposit_interest: 0,
        });
    }

    let borrow_rate = calculate_borrow_rate(spot_market, utilization)?;

    let time_since_last_update = now
        .cast::<u64>()
        .or(Err(ErrorCode::UnableToCastUnixTime))?
        .safe_sub(spot_market.last_interest_ts)?;

    // To save some compute units, have to multiply the rate by the `time_since_last_update` here
    // and then divide out by ONE_YEAR when calculating interest accumulated below
    let modified_borrow_rate = borrow_rate.safe_mul(time_since_last_update as u128)?;

    let modified_deposit_rate = modified_borrow_rate
        .safe_mul(utilization)?
        .safe_div(SPOT_UTILIZATION_PRECISION)?;

    let borrow_interest = spot_market
        .cumulative_borrow_interest
        .safe_mul(modified_borrow_rate)?
        .safe_div(ONE_YEAR)?
        .safe_div(SPOT_RATE_PRECISION)?
        .safe_add(1)?;

    let deposit_interest = spot_market
        .cumulative_deposit_interest
        .safe_mul(modified_deposit_rate)?
        .safe_div(ONE_YEAR)?
        .safe_div(SPOT_RATE_PRECISION)?;

    // Conservation clamp. The deposit side of an interval never receives more tokens than the
    // borrow side pays for it.
    //
    // The two sides are equal by construction. The deposit rate is the borrow rate scaled by
    // `utilization = borrow_tokens / deposit_tokens`. So
    // `deposit_tokens * rate * utilization == borrow_tokens * rate`.
    //
    // `utilization` comes from rounded token amounts, because the borrow side rounds up in
    // `get_token_amount`. The code samples it once at the start of the interval and applies it
    // across the whole span. The interval rate factor then multiplies that small overstatement.
    // On a long interval at a high rate it reaches whole tokens of deposit credit that no
    // borrower paid. Those deposit claims have no backing in the vault.
    //
    // The `check_fee_collection` fixture measures this. On a $1 market at a 2000% optimal rate,
    // a year settled in two cranks credited depositors 5 tokens above the borrower charge. One
    // crank and three or more cranks stay on the safe side. The error therefore appears at
    // particular interval lengths, not at every length.
    //
    // The code scales `deposit_interest` down in proportion. It does not saturate it to the
    // borrow charge, so the credit stays a true share of the amount that borrowers paid.
    let deposit_token_amount_gain =
        get_interest_token_amount(spot_market.deposit_balance, spot_market, deposit_interest)?;
    let borrow_token_amount_gain =
        get_interest_token_amount(spot_market.borrow_balance, spot_market, borrow_interest)?;

    let deposit_interest = if deposit_token_amount_gain > borrow_token_amount_gain {
        deposit_interest
            .safe_mul(borrow_token_amount_gain)?
            .safe_div(deposit_token_amount_gain)?
    } else {
        deposit_interest
    };

    Ok(InterestAccumulated {
        borrow_interest,
        deposit_interest,
    })
}

#[inline(always)]
pub fn calculate_borrow_rate(spot_market: &SpotMarket, utilization: u128) -> VelocityResult<u128> {
    let optimal_util = spot_market.optimal_utilization.cast::<u128>()?;
    let optimal_rate = spot_market.optimal_borrow_rate.cast::<u128>()?;
    let max_rate = spot_market.max_borrow_rate.cast::<u128>()?;
    let min_rate = spot_market.get_min_borrow_rate()?.cast::<u128>()?;

    let weights_divisor = 1000;

    let borrow_rate = if utilization <= optimal_util {
        let slope = optimal_rate
            .safe_mul(SPOT_UTILIZATION_PRECISION)?
            .safe_div(optimal_util)?;
        utilization
            .safe_mul(slope)?
            .safe_div(SPOT_UTILIZATION_PRECISION)?
    } else {
        let total_extra_rate = max_rate.safe_sub(optimal_rate)?;

        let mut rate = optimal_rate;
        let mut prev_util = optimal_util;

        for &(bp, weight) in INTEREST_RATE_SEGMENT_AND_WEIGHTS {
            let segment_start = prev_util;
            let segment_end = bp;
            let segment_range = segment_end.safe_sub(segment_start)?;
            let segment_rate_total = total_extra_rate
                .safe_mul(weight)?
                .safe_div(weights_divisor)?;

            if utilization <= segment_end {
                let partial_util = utilization.safe_sub(segment_start)?;
                let partial_rate = segment_rate_total
                    .safe_mul(partial_util)?
                    .safe_div(segment_range)?;
                rate = rate.safe_add(partial_rate)?;
                break;
            } else {
                rate = rate.safe_add(segment_rate_total)?;
                prev_util = segment_end;
            }
        }

        rate
    };

    Ok(borrow_rate.max(min_rate))
}

#[cfg(feature = "velocity-rs")]
pub fn calculate_deposit_rate(
    spot_market: &SpotMarket,
    utilization: u128,
    borrow_rate: u128,
) -> VelocityResult<u128> {
    // lenders receive the deposit gain net of the IF + protocol carveouts
    let total_carveout = spot_market
        .insurance_fund
        .if_fee_factor
        .safe_add(spot_market.protocol_fee_factor)?;
    borrow_rate
        .safe_mul(PERCENTAGE_PRECISION.safe_sub(total_carveout.cast()?)?)?
        .safe_mul(utilization)?
        .safe_div(SPOT_UTILIZATION_PRECISION)?
        .safe_div(PERCENTAGE_PRECISION)
}

pub fn get_balance_value_and_token_amount(
    spot_position: &SpotPosition,
    spot_market: &SpotMarket,
    oracle_price_data: &OraclePriceData,
) -> VelocityResult<(u128, u128)> {
    let token_amount = spot_position.get_token_amount(spot_market)?;

    let precision_decrease = 10_u128.pow(spot_market.decimals);

    let value = token_amount
        .safe_mul(oracle_price_data.price.cast()?)?
        .safe_div(precision_decrease)?;

    Ok((value, token_amount))
}

pub fn get_strict_token_value(
    token_amount: i128,
    spot_decimals: u32,
    strict_price: &StrictOraclePrice,
) -> VelocityResult<i128> {
    if token_amount == 0 {
        return Ok(0);
    }

    let precision_decrease = 10_i128.pow(spot_decimals);

    let price = if token_amount > 0 {
        strict_price.min()
    } else {
        strict_price.max()
    };

    let token_with_price = token_amount.safe_mul(price.cast()?)?;

    if token_with_price < 0 {
        token_with_price.safe_div_floor(precision_decrease)
    } else {
        token_with_price.safe_div(precision_decrease)
    }
}

pub fn get_token_value(
    token_amount: i128,
    spot_decimals: u32,
    oracle_price: i64,
) -> VelocityResult<i128> {
    if token_amount == 0 {
        return Ok(0);
    }

    let precision_decrease = 10_i128.pow(spot_decimals);
    let token_with_oracle = token_amount.safe_mul(oracle_price.cast()?)?;

    if token_with_oracle < 0 {
        token_with_oracle.safe_div_floor(precision_decrease.abs())
    } else {
        token_with_oracle.safe_div(precision_decrease)
    }
}

pub fn get_balance_value(
    spot_position: &SpotPosition,
    spot_market: &SpotMarket,
    oracle_price_data: &OraclePriceData,
) -> VelocityResult<u128> {
    let (value, _) =
        get_balance_value_and_token_amount(spot_position, spot_market, oracle_price_data)?;
    Ok(value)
}
