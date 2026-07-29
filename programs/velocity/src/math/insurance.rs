use crate::{
    error::{ErrorCode, VelocityResult},
    math::{
        casting::Cast,
        constants::PRICE_PRECISION,
        helpers::{get_proportion_u128, log10_iter},
        safe_math::SafeMath,
    },
    msg,
    state::{insurance_fund_stake::InsuranceFundStake, spot_market::SpotMarket},
    validate,
};

#[cfg(test)]
mod tests;

pub fn vault_amount_to_if_shares(
    amount: u64,
    total_if_shares: u128,
    insurance_fund_vault_balance: u64,
) -> VelocityResult<u128> {
    // relative to the entire pool + total amount minted
    let n_shares = if insurance_fund_vault_balance > 0 {
        // assumes total_if_shares != 0 (in most cases) for nice result for user

        get_proportion_u128(
            amount.cast::<u128>()?,
            total_if_shares,
            insurance_fund_vault_balance.cast::<u128>()?,
        )?
    } else {
        // must be case that total_if_shares == 0 for nice result for user
        validate!(
            total_if_shares == 0,
            ErrorCode::InvalidIFSharesDetected,
            "assumes total_if_shares == 0",
        )?;

        amount.cast::<u128>()?
    };

    Ok(n_shares)
}

pub fn if_shares_to_vault_amount(
    n_shares: u128,
    total_if_shares: u128,
    insurance_fund_vault_balance: u64,
) -> VelocityResult<u64> {
    validate!(
        n_shares <= total_if_shares,
        ErrorCode::InvalidIFSharesDetected,
        "n_shares({}) > total_if_shares({})",
        n_shares,
        total_if_shares
    )?;

    let amount = if total_if_shares > 0 {
        get_proportion_u128(
            insurance_fund_vault_balance as u128,
            n_shares,
            total_if_shares,
        )?
        .cast::<u64>()?
    } else {
        0
    };

    Ok(amount)
}

pub fn calculate_rebase_info(
    total_if_shares: u128,
    insurance_fund_vault_balance: u64,
) -> VelocityResult<(u32, u128)> {
    let rebase_divisor_full = total_if_shares
        .safe_div(10)?
        .safe_div(insurance_fund_vault_balance.cast::<u128>()?)?;

    let expo_diff = log10_iter(rebase_divisor_full).cast::<u32>()?;
    let rebase_divisor = 10_u128.pow(expo_diff);

    Ok((expo_diff, rebase_divisor))
}

pub fn calculate_if_shares_lost(
    insurance_fund_stake: &InsuranceFundStake,
    spot_market: &SpotMarket,
    insurance_fund_vault_balance: u64,
) -> VelocityResult<u128> {
    let n_shares = insurance_fund_stake.last_withdraw_request_shares;

    // Forfeiture on unstake-cancel, modeled as **withdraw-and-restake at the current
    // active share price** (finding #30). A cancel is treated as if the staker completed
    // the withdrawal of their `n_shares` requested shares and immediately re-staked the
    // resulting tokens at the price prevailing right now:
    //   * the withdrawal pays out `withdraw_value = min(current value of n_shares,
    //     last_withdraw_request_value)` — the exact payout `remove_insurance_fund_stake`
    //     would give, capped at the value frozen at request time;
    //   * re-staking that `withdraw_value` at the current active price (the pool after
    //     removing `n_shares` and `withdraw_value` tokens) mints `new_n_shares`;
    //   * the staker keeps `new_n_shares` and forfeits `n_shares - new_n_shares` to the
    //     remaining stakers.
    // If the fund appreciated during escrow the current price is higher, so re-staking
    // the frozen value buys back fewer shares and the appreciation is forfeited — this is
    // the anti-free-option property (you cannot request at a low price, watch the fund
    // rise, then cancel and keep the upside for free). If it did not appreciate
    // (`current value <= last_withdraw_request_value`) nothing is forfeited.
    //
    // This is donation-immune without consulting the accounted balance: the withdraw leg
    // is bounded by `last_withdraw_request_value`, snapshotted at request time and never
    // re-read from the live vault. A raw SPL donation inflates the live price, but the
    // extractable forfeiture is capped by that frozen value, and the donation is spread
    // pro-rata across *all* shareholders — so an attacker sandwiching a victim's cancel
    // with a donation always forgoes more on the donated capital (a `(1 - f)` share of
    // the donation, `f` = attacker's share fraction) than they can recapture from the
    // victim's `f`-weighted burn. The attack is unprofitable for any `f < 1`, so pricing
    // the restake off the live balance here is safe.
    let amount = if_shares_to_vault_amount(
        n_shares,
        spot_market.insurance_fund.total_shares,
        insurance_fund_vault_balance,
    )?;

    let if_shares_lost = if amount > insurance_fund_stake.last_withdraw_request_value {
        let new_n_shares = vault_amount_to_if_shares(
            insurance_fund_stake.last_withdraw_request_value,
            spot_market.insurance_fund.total_shares.safe_sub(n_shares)?,
            insurance_fund_vault_balance
                .safe_sub(insurance_fund_stake.last_withdraw_request_value)?,
        )?;

        validate!(
            new_n_shares <= n_shares,
            ErrorCode::InvalidIFSharesDetected,
            "Issue calculating delta if_shares after canceling request {} < {}",
            new_n_shares,
            n_shares
        )?;

        n_shares.safe_sub(new_n_shares)?
    } else {
        0
    };

    Ok(if_shares_lost)
}

pub fn calculate_share_price(
    total_if_shares: u128,
    insurance_fund_vault_balance: u64,
) -> VelocityResult<u64> {
    if total_if_shares > 0 {
        insurance_fund_vault_balance
            .cast::<u128>()?
            .safe_mul(PRICE_PRECISION)?
            .safe_div(total_if_shares)?
            .cast::<u64>()
    } else {
        Ok(0)
    }
}
