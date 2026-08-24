use {
    crate::{
        controller::{
            spot_balance::{update_spot_balances, update_spot_market_cumulative_interest},
            token::send_from_program_vault,
        },
        emit,
        error::{ErrorCode, VelocityResult},
        math::{
            casting::Cast,
            constants::{
                MAX_APR_PER_REVENUE_SETTLE_TO_INSURANCE_FUND_VAULT, ONE_YEAR, PERCENTAGE_PRECISION,
                QUOTE_SPOT_MARKET_INDEX,
                SHARE_OF_REVENUE_ALLOCATED_TO_INSURANCE_FUND_VAULT_DENOMINATOR,
                SHARE_OF_REVENUE_ALLOCATED_TO_INSURANCE_FUND_VAULT_NUMERATOR,
            },
            helpers::{get_proportion_u128, on_the_hour_update},
            insurance::{
                calculate_if_shares_lost, calculate_rebase_info,
                deposit_amount_and_shares_for_if_stake, if_shares_to_vault_amount,
            },
            safe_math::SafeMath,
            spot_balance::{get_spot_balance, get_token_amount},
            spot_withdraw::validate_spot_market_vault_amount,
        },
        msg,
        state::{
            events::{InsuranceFundRecord, InsuranceFundStakeRecord, StakeAction},
            insurance_fund_stake::InsuranceFundStake,
            paused_operations::SpotOperation,
            perp_market::PerpMarket,
            spot_market::{SpotBalanceType, SpotMarket},
            state::State,
            user::UserStats,
        },
        validate,
        vlp::amm::math::amm::calculate_net_user_pnl,
    },
    anchor_lang::prelude::*,
    anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface},
    std::{iter::Peekable, slice::Iter},
};

#[cfg(test)]
mod tests;

/// Value of the insurance fund: the cash in its vault plus the revenue that is
/// allocated to it but still held in the spot vault.
///
/// Shares are priced off this rather than the vault balance alone, so a transfer
/// that a withdraw pause holds back does not move the share price.
pub fn get_insurance_fund_nav(
    insurance_vault_amount: u64,
    spot_market: &SpotMarket,
) -> VelocityResult<u64> {
    insurance_vault_amount.safe_add(
        spot_market
            .get_insurance_fund_revenue_receivable()?
            .cast()?,
    )
}

/// Lower the revenue-settle cap base to the fund value that an outflow leaves
/// behind.
///
/// `if_last_settle_vault_amount` is the lowest value the fund held since the last
/// revenue settle. Every path that moves value out of the fund must call this.
/// Without it, a dip inside a period is invisible at the next settle: a draw takes
/// the fund to 100, a donation puts it back to 1000, and `min(live, snapshot)` reads
/// 1000 again. The donation then lifts the cap without staying in the fund for a
/// period.
///
/// `insurance_vault_amount` is the vault balance before the outflow. The subtraction
/// saturates rather than errors. A cap base of `0` only settles less revenue for the
/// rest of the period. An error would revert a bankruptcy or deficit resolution.
pub fn record_insurance_fund_outflow(
    spot_market: &mut SpotMarket,
    insurance_vault_amount: u64,
    outflow_amount: u64,
) -> VelocityResult {
    let effective_balance_after =
        get_insurance_fund_nav(insurance_vault_amount, spot_market)?.saturating_sub(outflow_amount);
    spot_market.if_last_settle_vault_amount = spot_market
        .if_last_settle_vault_amount
        .min(effective_balance_after);

    Ok(())
}

pub fn update_user_stats_if_stake_amount(
    if_stake_amount_delta: i64,
    insurance_vault_amount: u64,
    insurance_fund_stake: &mut InsuranceFundStake,
    user_stats: &mut UserStats,
    spot_market: &mut SpotMarket,
) -> VelocityResult {
    if spot_market.market_index != QUOTE_SPOT_MARKET_INDEX {
        return Ok(());
    }

    let insurance_fund_nav = get_insurance_fund_nav(insurance_vault_amount, spot_market)?;
    let if_stake_amount = if if_stake_amount_delta >= 0 {
        if_shares_to_vault_amount(
            insurance_fund_stake.checked_if_shares(spot_market)?,
            spot_market.insurance_fund.total_shares,
            insurance_fund_nav.safe_add(if_stake_amount_delta.unsigned_abs())?,
        )?
    } else {
        if_shares_to_vault_amount(
            insurance_fund_stake.checked_if_shares(spot_market)?,
            spot_market.insurance_fund.total_shares,
            insurance_fund_nav.safe_sub(if_stake_amount_delta.unsigned_abs())?,
        )?
    };

    user_stats.if_staked_quote_asset_amount = if_stake_amount;

    Ok(())
}

/// Stake into the insurance fund, returning the amount actually staked — the caller
/// transfers that, not `requested_amount`. See `deposit_amount_and_shares_for_if_stake`:
/// only the portion of the request that prices to whole shares is taken, so a deposit
/// can never be partly forfeited to existing shareholders as rounding.
pub fn add_insurance_fund_stake(
    requested_amount: u64,
    insurance_vault_amount: u64,
    insurance_fund_stake: &mut InsuranceFundStake,
    user_stats: &mut UserStats,
    spot_market: &mut SpotMarket,
    now: i64,
    admin_deposit: bool,
) -> VelocityResult<u64> {
    let insurance_fund_nav = get_insurance_fund_nav(insurance_vault_amount, spot_market)?;
    validate!(
        !(insurance_fund_nav == 0 && spot_market.insurance_fund.total_shares != 0),
        ErrorCode::InvalidIFForNewStakes,
        "Insurance Fund balance should be non-zero for new stakers to enter"
    )?;

    // No-staker bootstrap guard: if the vault was funded while no shares exist
    // (fees settled or tokens sent before any staker), seed `total_shares` 1:1
    // with the vault so the first staker mints `amount` shares at price ~1
    // instead of `amount * 0 / vault == 0` (which would forfeit their deposit).
    // The seeded shares are protocol-owned, permanent, non-withdrawable ballast.
    if spot_market.insurance_fund.total_shares == 0 && insurance_fund_nav > 0 {
        spot_market.insurance_fund.total_shares = insurance_fund_nav.cast()?;
    }

    apply_rebase_to_insurance_fund(insurance_fund_nav, spot_market)?;
    apply_rebase_to_insurance_fund_stake(insurance_fund_stake, spot_market)?;

    let if_shares_before = insurance_fund_stake.checked_if_shares(spot_market)?;
    let total_if_shares_before = spot_market.insurance_fund.total_shares;
    let user_if_shares_before = spot_market.insurance_fund.user_shares;

    // `amount` is the share-aligned portion of the request; the remainder is left in the
    // depositor's token account rather than transferred, so no part of a deposit accrues
    // to existing shareholders as rounding. Shares are priced off the pre-transfer vault
    // balance, which an attacker can inflate by donating into the vault — pricing the
    // deposit exactly is what makes that inflation unprofitable.
    let (amount, n_shares) = deposit_amount_and_shares_for_if_stake(
        requested_amount,
        spot_market.insurance_fund.total_shares,
        insurance_fund_nav,
    )?;

    // A request below the price of a single share buys nothing at all; reject it rather
    // than accept a deposit of zero. Mirrors the `n_shares > 0` guard the request-remove
    // path already enforces.
    validate!(
        n_shares > 0,
        ErrorCode::IFDepositMintsZeroShares,
        "deposit of {} is below the price of one IF share (vault {}, total_shares {})",
        requested_amount,
        insurance_fund_nav,
        spot_market.insurance_fund.total_shares
    )?;

    // reset cost basis if no shares
    insurance_fund_stake.cost_basis = if if_shares_before == 0 {
        amount.cast()?
    } else {
        insurance_fund_stake.cost_basis.safe_add(amount.cast()?)?
    };

    insurance_fund_stake.increase_if_shares(n_shares, spot_market)?;

    spot_market.insurance_fund.total_shares =
        spot_market.insurance_fund.total_shares.safe_add(n_shares)?;

    spot_market.insurance_fund.user_shares =
        spot_market.insurance_fund.user_shares.safe_add(n_shares)?;

    update_user_stats_if_stake_amount(
        amount.cast()?,
        insurance_vault_amount,
        insurance_fund_stake,
        user_stats,
        spot_market,
    )?;

    let if_shares_after = insurance_fund_stake.checked_if_shares(spot_market)?;

    emit!(InsuranceFundStakeRecord {
        ts: now,
        user_authority: user_stats.authority,
        action: if admin_deposit {
            StakeAction::AdminDeposit
        } else {
            StakeAction::Stake
        },
        amount,
        market_index: spot_market.market_index,
        insurance_vault_amount_before: insurance_vault_amount,
        if_shares_before,
        user_if_shares_before,
        total_if_shares_before,
        if_shares_after,
        total_if_shares_after: spot_market.insurance_fund.total_shares,
        user_if_shares_after: spot_market.insurance_fund.user_shares,
    });

    Ok(amount)
}

pub fn apply_rebase_to_insurance_fund(
    insurance_fund_vault_balance: u64,
    spot_market: &mut SpotMarket,
) -> VelocityResult {
    if insurance_fund_vault_balance != 0
        && insurance_fund_vault_balance.cast::<u128>()? < spot_market.insurance_fund.total_shares
    {
        let (expo_diff, rebase_divisor) = calculate_rebase_info(
            spot_market.insurance_fund.total_shares,
            insurance_fund_vault_balance,
        )?;

        spot_market.insurance_fund.total_shares = spot_market
            .insurance_fund
            .total_shares
            .safe_div(rebase_divisor)?;
        spot_market.insurance_fund.user_shares = spot_market
            .insurance_fund
            .user_shares
            .safe_div(rebase_divisor)?;
        spot_market.insurance_fund.shares_base = spot_market
            .insurance_fund
            .shares_base
            .safe_add(expo_diff.cast::<u128>()?)?;

        msg!("rebasing insurance fund: expo_diff={}", expo_diff);
    }

    if insurance_fund_vault_balance != 0 && spot_market.insurance_fund.total_shares == 0 {
        spot_market.insurance_fund.total_shares = insurance_fund_vault_balance.cast::<u128>()?;
    }

    Ok(())
}

pub fn apply_rebase_to_insurance_fund_stake(
    insurance_fund_stake: &mut InsuranceFundStake,
    spot_market: &mut SpotMarket,
) -> VelocityResult {
    if spot_market.insurance_fund.shares_base != insurance_fund_stake.if_base {
        validate!(
            spot_market.insurance_fund.shares_base > insurance_fund_stake.if_base,
            ErrorCode::InvalidIFRebase,
            "Rebase expo out of bounds"
        )?;

        let expo_diff = (spot_market.insurance_fund.shares_base - insurance_fund_stake.if_base)
            .cast::<u32>()?;

        let rebase_divisor = 10_u128.pow(expo_diff);

        msg!(
            "rebasing insurance fund stake: base: {} -> {} ",
            insurance_fund_stake.if_base,
            spot_market.insurance_fund.shares_base,
        );

        insurance_fund_stake.if_base = spot_market.insurance_fund.shares_base;

        let old_if_shares = insurance_fund_stake.unchecked_if_shares();
        let new_if_shares = old_if_shares.safe_div(rebase_divisor)?;

        msg!(
            "rebasing insurance fund stake: shares -> {} ",
            new_if_shares
        );

        insurance_fund_stake.update_if_shares(new_if_shares, spot_market)?;

        insurance_fund_stake.last_withdraw_request_shares = insurance_fund_stake
            .last_withdraw_request_shares
            .safe_div(rebase_divisor)?;
    }

    Ok(())
}

pub fn request_remove_insurance_fund_stake(
    n_shares: u128,
    insurance_vault_amount: u64,
    insurance_fund_stake: &mut InsuranceFundStake,
    user_stats: &mut UserStats,
    spot_market: &mut SpotMarket,
    now: i64,
) -> VelocityResult {
    msg!("n_shares {}", n_shares);
    insurance_fund_stake.last_withdraw_request_shares = n_shares;

    let insurance_fund_nav = get_insurance_fund_nav(insurance_vault_amount, spot_market)?;
    apply_rebase_to_insurance_fund(insurance_fund_nav, spot_market)?;
    apply_rebase_to_insurance_fund_stake(insurance_fund_stake, spot_market)?;

    let if_shares_before = insurance_fund_stake.checked_if_shares(spot_market)?;
    let total_if_shares_before = spot_market.insurance_fund.total_shares;
    let user_if_shares_before = spot_market.insurance_fund.user_shares;

    validate!(
        insurance_fund_stake.last_withdraw_request_shares
            <= insurance_fund_stake.checked_if_shares(spot_market)?,
        ErrorCode::InvalidInsuranceUnstakeSize,
        "last_withdraw_request_shares exceeds if_shares {} > {}",
        insurance_fund_stake.last_withdraw_request_shares,
        insurance_fund_stake.checked_if_shares(spot_market)?
    )?;

    validate!(
        insurance_fund_stake.if_base == spot_market.insurance_fund.shares_base,
        ErrorCode::InvalidIFRebase,
        "if stake base != spot market base"
    )?;

    insurance_fund_stake.last_withdraw_request_value = if_shares_to_vault_amount(
        insurance_fund_stake.last_withdraw_request_shares,
        spot_market.insurance_fund.total_shares,
        insurance_fund_nav,
    )?
    .min(insurance_fund_nav.saturating_sub(1));

    validate!(
        insurance_fund_stake.last_withdraw_request_value == 0
            || insurance_fund_stake.last_withdraw_request_value < insurance_fund_nav,
        ErrorCode::InvalidIFUnstakeSize,
        "Requested withdraw value is not below Insurance Fund balance"
    )?;

    let if_shares_after = insurance_fund_stake.checked_if_shares(spot_market)?;

    update_user_stats_if_stake_amount(
        0,
        insurance_vault_amount,
        insurance_fund_stake,
        user_stats,
        spot_market,
    )?;

    emit!(InsuranceFundStakeRecord {
        ts: now,
        user_authority: user_stats.authority,
        action: StakeAction::UnstakeRequest,
        amount: insurance_fund_stake.last_withdraw_request_value,
        market_index: spot_market.market_index,
        insurance_vault_amount_before: insurance_vault_amount,
        if_shares_before,
        user_if_shares_before,
        total_if_shares_before,
        if_shares_after,
        total_if_shares_after: spot_market.insurance_fund.total_shares,
        user_if_shares_after: spot_market.insurance_fund.user_shares,
    });

    insurance_fund_stake.last_withdraw_request_ts = now;

    Ok(())
}

/// Cancel a pending unstake request, modeled as **withdraw-and-restake at the current
/// active share price** (finding #30). The staker's `n_shares` requested shares are
/// treated as if they were withdrawn (paying out the value frozen at request time) and
/// immediately re-staked at the price prevailing now: any appreciation accrued during the
/// escrow window is forfeited to the remaining stakers (`if_shares_lost`), while a cancel
/// with no appreciation leaves the stake untouched. See `calculate_if_shares_lost` for the
/// exact share math and why bounding the withdraw leg by the request-time snapshot makes
/// this donation-immune without reading `if_last_settle_vault_amount`.
pub fn cancel_request_remove_insurance_fund_stake(
    insurance_vault_amount: u64,
    insurance_fund_stake: &mut InsuranceFundStake,
    user_stats: &mut UserStats,
    spot_market: &mut SpotMarket,
    now: i64,
) -> VelocityResult {
    let insurance_fund_nav = get_insurance_fund_nav(insurance_vault_amount, spot_market)?;
    apply_rebase_to_insurance_fund(insurance_fund_nav, spot_market)?;
    apply_rebase_to_insurance_fund_stake(insurance_fund_stake, spot_market)?;

    let if_shares_before = insurance_fund_stake.checked_if_shares(spot_market)?;
    let total_if_shares_before = spot_market.insurance_fund.total_shares;
    let user_if_shares_before = spot_market.insurance_fund.user_shares;

    validate!(
        insurance_fund_stake.if_base == spot_market.insurance_fund.shares_base,
        ErrorCode::InvalidIFRebase,
        "if stake base != spot market base"
    )?;

    // NOTE: we intentionally do NOT re-check `last_withdraw_request_shares != 0`
    // here (after the rebase above). A market-level IF rebase floors a small
    // pending request to zero (`last_withdraw_request_shares / rebase_divisor`),
    // so a post-rebase `!= 0` guard would reject the cancel and permanently
    // strand the stake: `remove` also rejects the zeroed request, and `add` /
    // re-`request` are blocked by the still-in-progress request. The genuine
    // "no request in progress" case is already rejected by the pre-rebase guard
    // in `handle_cancel_request_remove_insurance_fund_stake`. When the request
    // has floored to zero, cancel is a no-op on shares (`calculate_if_shares_lost`
    // returns 0), returns the intact rebased stake to active, and abandons only
    // the dust `last_withdraw_request_value`.
    //
    // Shares forfeited = requested shares minus the shares a withdraw-then-restake at the
    // current active price (from the live vault balance) would leave. Priced off the live
    // balance is safe here: the restake value is bounded by the request-time snapshot, so a
    // raw donation cannot manufacture extractable forfeiture (see `calculate_if_shares_lost`).
    let if_shares_lost =
        calculate_if_shares_lost(insurance_fund_stake, spot_market, insurance_fund_nav)?;

    insurance_fund_stake.decrease_if_shares(if_shares_lost, spot_market)?;

    spot_market.insurance_fund.total_shares = spot_market
        .insurance_fund
        .total_shares
        .safe_sub(if_shares_lost)?;

    spot_market.insurance_fund.user_shares = spot_market
        .insurance_fund
        .user_shares
        .safe_sub(if_shares_lost)?;

    let if_shares_after = insurance_fund_stake.checked_if_shares(spot_market)?;

    update_user_stats_if_stake_amount(
        0,
        insurance_vault_amount,
        insurance_fund_stake,
        user_stats,
        spot_market,
    )?;

    emit!(InsuranceFundStakeRecord {
        ts: now,
        user_authority: user_stats.authority,
        action: StakeAction::UnstakeCancelRequest,
        amount: 0,
        market_index: spot_market.market_index,
        insurance_vault_amount_before: insurance_vault_amount,
        if_shares_before,
        user_if_shares_before,
        total_if_shares_before,
        if_shares_after,
        total_if_shares_after: spot_market.insurance_fund.total_shares,
        user_if_shares_after: spot_market.insurance_fund.user_shares,
    });

    insurance_fund_stake.last_withdraw_request_shares = 0;
    insurance_fund_stake.last_withdraw_request_value = 0;
    insurance_fund_stake.last_withdraw_request_ts = now;

    Ok(())
}

pub fn remove_insurance_fund_stake(
    insurance_vault_amount: u64,
    insurance_fund_stake: &mut InsuranceFundStake,
    user_stats: &mut UserStats,
    spot_market: &mut SpotMarket,
    now: i64,
) -> VelocityResult<u64> {
    let time_since_withdraw_request =
        now.safe_sub(insurance_fund_stake.last_withdraw_request_ts)?;

    validate!(
        time_since_withdraw_request >= spot_market.insurance_fund.unstaking_period,
        ErrorCode::TryingToRemoveLiquidityTooFast
    )?;

    let insurance_fund_nav = get_insurance_fund_nav(insurance_vault_amount, spot_market)?;
    apply_rebase_to_insurance_fund(insurance_fund_nav, spot_market)?;
    apply_rebase_to_insurance_fund_stake(insurance_fund_stake, spot_market)?;

    let if_shares_before = insurance_fund_stake.checked_if_shares(spot_market)?;
    let total_if_shares_before = spot_market.insurance_fund.total_shares;
    let user_if_shares_before = spot_market.insurance_fund.user_shares;

    let n_shares = insurance_fund_stake.last_withdraw_request_shares;

    validate!(
        n_shares > 0,
        ErrorCode::InvalidIFUnstake,
        "Must submit withdraw request and wait the escrow period"
    )?;

    validate!(
        if_shares_before >= n_shares,
        ErrorCode::InsufficientIFShares
    )?;

    let amount = if_shares_to_vault_amount(
        n_shares,
        spot_market.insurance_fund.total_shares,
        insurance_fund_nav,
    )?;

    let _if_shares_lost =
        calculate_if_shares_lost(insurance_fund_stake, spot_market, insurance_fund_nav)?;

    let withdraw_amount = amount.min(insurance_fund_stake.last_withdraw_request_value);

    // Only cash can leave the fund. Part of it may be revenue that is allocated
    // to the fund but still held in the spot vault, and that part cannot pay an
    // unstake.
    //
    // Refuse rather than pay what cash there is. A partial payout would let a
    // staker take cash and leave the allocated revenue to the stakers who stay,
    // and every loss draw spends that revenue before it touches cash. Paying
    // the cash share of the claim does not fix this: the caller repeats the
    // call, each time taking a share of a smaller pot, until the cash is gone.
    //
    // The instruction settles allocated revenue into the vault before it prices
    // the payout, so this can only fire while a pause blocks that transfer. The
    // staker unstakes once the pause lifts.
    validate!(
        withdraw_amount == 0 || withdraw_amount < insurance_vault_amount,
        ErrorCode::InvalidIFUnstakeSize,
        "insurance fund vault has insufficient cash for withdrawal"
    )?;

    insurance_fund_stake.decrease_if_shares(n_shares, spot_market)?;

    insurance_fund_stake.cost_basis = insurance_fund_stake
        .cost_basis
        .safe_sub(withdraw_amount.cast()?)?;

    spot_market.insurance_fund.total_shares =
        spot_market.insurance_fund.total_shares.safe_sub(n_shares)?;

    spot_market.insurance_fund.user_shares =
        spot_market.insurance_fund.user_shares.safe_sub(n_shares)?;

    record_insurance_fund_outflow(spot_market, insurance_vault_amount, withdraw_amount)?;

    // reset insurance_fund_stake withdraw request info
    insurance_fund_stake.last_withdraw_request_shares = 0;
    insurance_fund_stake.last_withdraw_request_value = 0;
    insurance_fund_stake.last_withdraw_request_ts = now;

    let if_shares_after = insurance_fund_stake.checked_if_shares(spot_market)?;

    update_user_stats_if_stake_amount(
        -(withdraw_amount.cast()?),
        insurance_vault_amount,
        insurance_fund_stake,
        user_stats,
        spot_market,
    )?;

    emit!(InsuranceFundStakeRecord {
        ts: now,
        user_authority: user_stats.authority,
        action: StakeAction::Unstake,
        amount: withdraw_amount,
        market_index: spot_market.market_index,
        insurance_vault_amount_before: insurance_vault_amount,
        if_shares_before,
        user_if_shares_before,
        total_if_shares_before,
        if_shares_after,
        total_if_shares_after: spot_market.insurance_fund.total_shares,
        user_if_shares_after: spot_market.insurance_fund.user_shares,
    });

    Ok(withdraw_amount)
}

pub fn attempt_settle_revenue_to_insurance_fund<'info>(
    spot_market_vault: &InterfaceAccount<'info, TokenAccount>,
    insurance_fund_vault: &InterfaceAccount<'info, TokenAccount>,
    spot_market: &mut SpotMarket,
    now: i64,
    token_program: &Interface<'info, TokenInterface>,
    velocity_signer: &AccountInfo<'info>,
    state: &State,
    mint: &Option<InterfaceAccount<'info, Mint>>,
    remaining_accounts: Option<&mut Peekable<Iter<'info, AccountInfo<'info>>>>,
) -> Result<()> {
    let valid_revenue_settle_time = if spot_market.insurance_fund.revenue_settle_period > 0 {
        let time_until_next_update = on_the_hour_update(
            now,
            spot_market.insurance_fund.last_revenue_settle_ts,
            spot_market.insurance_fund.revenue_settle_period,
        )?;

        time_until_next_update == 0
    } else {
        false
    };

    let transfer_paused =
        state.withdraw_paused()? || spot_market.is_operation_paused(SpotOperation::Withdraw);
    let has_receivable = spot_market.insurance_fund_revenue_receivable_scaled > 0;

    if !valid_revenue_settle_time && !has_receivable {
        return Ok(());
    }

    if valid_revenue_settle_time {
        book_revenue_to_insurance_fund(
            spot_market_vault.amount,
            insurance_fund_vault.amount,
            spot_market,
            now,
            false,
            state.funding_paused()?,
        )?;
    } else {
        update_spot_market_cumulative_interest(spot_market, None, now, state.funding_paused()?)?;
    }

    if transfer_paused {
        return Ok(());
    }

    let token_amount = settle_insurance_fund_revenue_receivable(spot_market)?;
    if token_amount > 0 {
        msg!(
            "Spot market_index={} sending {} to insurance_fund_vault",
            spot_market.market_index,
            token_amount
        );

        send_from_program_vault(
            token_program,
            spot_market_vault,
            insurance_fund_vault,
            velocity_signer,
            state.signer_nonce,
            token_amount,
            mint,
            remaining_accounts,
        )?;
    }

    Ok(())
}

fn book_revenue_to_insurance_fund(
    spot_market_vault_amount: u64,
    insurance_vault_amount: u64,
    spot_market: &mut SpotMarket,
    now: i64,
    check_invariants: bool,
    funding_paused: bool,
) -> VelocityResult<u64> {
    update_spot_market_cumulative_interest(spot_market, None, now, funding_paused)?;

    if spot_market.insurance_fund.revenue_settle_period == 0 {
        // revenue pool not configured to settle, ending early
        return Ok(0);
    }

    let depositors_claim =
        validate_spot_market_vault_amount(spot_market, spot_market_vault_amount)?;

    let mut token_amount = get_token_amount(
        spot_market.revenue_pool.scaled_balance,
        spot_market,
        &SpotBalanceType::Deposit,
    )?;

    let existing_receivable = spot_market.get_insurance_fund_revenue_receivable()?;
    let unreserved_depositors_claim = depositors_claim
        .max(0)
        .cast::<u128>()?
        .saturating_sub(existing_receivable);

    if unreserved_depositors_claim < token_amount {
        // only allow half of withdraw available when utilization is high
        token_amount = unreserved_depositors_claim.safe_div(2)?;
    }

    if spot_market.insurance_fund.user_shares > 0 {
        // Size the APR cap off the balance the fund held for the whole period, not off
        // the live vault alone. `insurance_vault_amount` is the raw token-account
        // balance, which anyone can inflate with a direct SPL transfer right before a
        // settle to lift the cap toward the 1/10-of-revenue-pool bound.
        // `if_last_settle_vault_amount` is the lowest balance the vault held since the
        // last settle, so it predates any such transfer and it already carries every
        // dip in between. The `min` of the two counts only capital that was present
        // throughout, which a pre-settle donation is not. Capital that does span a full
        // period already belongs to the stakers pro rata, so counting it is correct. A
        // `0` snapshot (never settled, or settled on an empty vault) gives a `0` cap for
        // one period; the snapshot written below then heals it.
        let insurance_fund_nav = get_insurance_fund_nav(insurance_vault_amount, spot_market)?;
        let cap_vault_amount = insurance_fund_nav.min(spot_market.if_last_settle_vault_amount);

        // only allow MAX_APR_PER_REVENUE_SETTLE_TO_INSURANCE_FUND_VAULT or 1/10th of revenue pool to be settled
        let capped_apr_amount = cap_vault_amount
            .cast::<u128>()?
            .safe_mul(MAX_APR_PER_REVENUE_SETTLE_TO_INSURANCE_FUND_VAULT)?
            .safe_div(PERCENTAGE_PRECISION)?
            .safe_div(
                ONE_YEAR
                    .safe_div(spot_market.insurance_fund.revenue_settle_period.cast()?)?
                    .max(1),
            )?;
        let capped_token_pct_amount = token_amount.safe_div(10)?;
        token_amount = capped_token_pct_amount.min(capped_apr_amount);
    }

    let mut insurance_fund_token_amount = get_proportion_u128(
        token_amount,
        SHARE_OF_REVENUE_ALLOCATED_TO_INSURANCE_FUND_VAULT_NUMERATOR,
        SHARE_OF_REVENUE_ALLOCATED_TO_INSURANCE_FUND_VAULT_DENOMINATOR,
    )?
    .cast::<u64>()?;

    // True when the cap above collapsed to `0` only because this market has no
    // snapshot yet. Read it before the write below replaces the snapshot.
    let cap_base_was_unset =
        spot_market.insurance_fund.user_shares > 0 && spot_market.if_last_settle_vault_amount == 0;

    let insurance_fund_nav_before = get_insurance_fund_nav(insurance_vault_amount, spot_market)?;

    // Move the scaled claim itself from the revenue pool to the receivable. Both
    // are deposit claims inside `deposit_balance`, so the market total does not
    // change and no pool dust is left behind. Round the debit up so the revenue
    // pool never keeps a claim it already gave away, and cap it at the pool
    // balance so the last settle empties the pool exactly.
    //
    // The token value of the moved claim is what the fund can later draw, and
    // reading a token amount back off a scaled balance floors. Report that value
    // rather than the amount the proportion math asked for, or the settle would
    // promise a token the claim cannot pay.
    if insurance_fund_token_amount > 0 {
        // When the booked amount covers the pool's whole token value, take the
        // pool balance itself. Rounding a token amount back into scaled space
        // would leave a few scaled units the pool can never spend.
        let revenue_pool_token_amount = get_token_amount(
            spot_market.revenue_pool.scaled_balance,
            spot_market,
            &SpotBalanceType::Deposit,
        )?;
        let balance_delta = if revenue_pool_token_amount <= insurance_fund_token_amount.cast()? {
            spot_market.revenue_pool.scaled_balance
        } else {
            get_spot_balance(
                insurance_fund_token_amount.cast()?,
                spot_market,
                &SpotBalanceType::Deposit,
                true,
            )?
        };

        spot_market.revenue_pool.scaled_balance = spot_market
            .revenue_pool
            .scaled_balance
            .safe_sub(balance_delta)?;
        spot_market.insurance_fund_revenue_receivable_scaled = spot_market
            .insurance_fund_revenue_receivable_scaled
            .safe_add(balance_delta)?;

        insurance_fund_token_amount =
            get_token_amount(balance_delta, spot_market, &SpotBalanceType::Deposit)?.cast()?;
    }

    // `NoRevenueToSettleToIF` tells the keeper that the settle was pointless. The settle
    // that seeds the snapshot is expected to move nothing, so let it through — an error
    // reverts the whole instruction, so the market would never get a snapshot and would
    // stay capped at zero forever.
    if check_invariants && !cap_base_was_unset {
        validate!(
            insurance_fund_token_amount != 0
                || spot_market.insurance_fund_revenue_receivable_scaled > 0,
            ErrorCode::NoRevenueToSettleToIF,
            "no amount to settle to insurance fund"
        )?;
    }

    spot_market.insurance_fund.last_revenue_settle_ts = now;

    // The insurance fund is staker-owned: once stakers exist, the entire settled
    // amount accrues to them as share-price appreciation (no protocol shares
    // minted per-settle).
    //
    // Bootstrap (no-staker backstop): while `total_shares == 0`, the fund still
    // collects fees as a backstop, but there are no shares to back the vault.
    // Left unhandled, the first staker would mint `amount * 0 / vault == 0`
    // shares and lose their deposit. So when there are no shares, seed
    // `total_shares` 1:1 with the post-settle vault balance. These are
    // protocol-owned (`total_shares > user_shares == 0`), permanent, and NOT
    // withdrawable (the protocol-share withdraw/transfer ixs are removed) — pure
    // backstop ballast at share price ~1. Fires only during the no-staker phase.
    let total_if_shares_before = spot_market.insurance_fund.total_shares;
    if total_if_shares_before == 0 {
        spot_market.insurance_fund.total_shares = insurance_fund_nav_before
            .safe_add(insurance_fund_token_amount)?
            .cast()?;
    }

    spot_market.if_last_settle_vault_amount =
        insurance_fund_nav_before.safe_add(insurance_fund_token_amount)?;

    emit!(InsuranceFundRecord {
        ts: now,
        spot_market_index: spot_market.market_index,
        perp_market_index: 0, // todo: make option?
        amount: insurance_fund_token_amount.cast()?,

        user_if_factor: spot_market.insurance_fund.if_fee_factor,
        total_if_factor: spot_market.insurance_fund.if_fee_factor,
        vault_amount_before: spot_market_vault_amount,
        insurance_vault_amount_before: insurance_vault_amount,
        total_if_shares_before,
        total_if_shares_after: spot_market.insurance_fund.total_shares,
    });

    insurance_fund_token_amount.cast()
}

pub fn settle_insurance_fund_revenue_receivable(
    spot_market: &mut SpotMarket,
) -> VelocityResult<u64> {
    release_insurance_fund_revenue_receivable(
        spot_market,
        spot_market.insurance_fund_revenue_receivable_scaled,
    )?
    .cast()
}

/// Removes `balance_delta` of the allocated claim from the market total and
/// reports the token value it released.
fn release_insurance_fund_revenue_receivable(
    spot_market: &mut SpotMarket,
    balance_delta: u128,
) -> VelocityResult<u128> {
    if balance_delta == 0 {
        return Ok(0);
    }

    let amount = get_token_amount(balance_delta, spot_market, &SpotBalanceType::Deposit)?;

    spot_market.deposit_balance = spot_market.deposit_balance.safe_sub(balance_delta)?;
    spot_market.insurance_fund_revenue_receivable_scaled = spot_market
        .insurance_fund_revenue_receivable_scaled
        .safe_sub(balance_delta)?;

    Ok(amount)
}

/// Cancels an allocated IF claim against tokens leaving the spot vault or bad
/// spot debt. Both cases remove the claim from the market's total deposits.
pub fn consume_insurance_fund_revenue_receivable(
    spot_market: &mut SpotMarket,
    max_amount: u128,
) -> VelocityResult<u128> {
    if max_amount == 0 {
        return Ok(0);
    }

    // A cap that covers the whole claim releases all of it, so no scaled dust
    // stays behind to accrue interest for nobody. Otherwise release the scaled
    // part the cap pays for, rounded down so the tokens released stay at or
    // below `max_amount`.
    let balance_delta = if spot_market.get_insurance_fund_revenue_receivable()? <= max_amount {
        spot_market.insurance_fund_revenue_receivable_scaled
    } else {
        get_spot_balance(max_amount, spot_market, &SpotBalanceType::Deposit, false)?
    };

    release_insurance_fund_revenue_receivable(spot_market, balance_delta)
}

/// Reclassifies allocated IF revenue into a perp PnL pool without moving tokens
/// or changing total spot deposits.
pub fn transfer_insurance_fund_revenue_receivable_to_pool(
    spot_market: &mut SpotMarket,
    destination: &mut crate::state::perp_market::PoolBalance,
    total_payment: u128,
) -> VelocityResult<u128> {
    if total_payment == 0 {
        return Ok(0);
    }

    let receivable_payment =
        total_payment.min(spot_market.get_insurance_fund_revenue_receivable()?);

    // Credit the destination once for the combined receivable and vault payment.
    // Splitting the credit would apply scaled balance rounding twice.
    update_spot_balances(
        total_payment,
        &SpotBalanceType::Deposit,
        spot_market,
        destination,
        false,
    )?;

    // The receivable is already included in aggregate deposits. Only the vault
    // portion is new backing, so remove the receivable portion added above. The
    // claim moves in scaled space, so the amount the destination gained and the
    // amount the receivable gave up describe the same value.
    if receivable_payment > 0 {
        let balance_delta = get_spot_balance(
            receivable_payment,
            spot_market,
            &SpotBalanceType::Deposit,
            true,
        )?
        .min(spot_market.insurance_fund_revenue_receivable_scaled);

        spot_market.deposit_balance = spot_market.deposit_balance.safe_sub(balance_delta)?;
        spot_market.insurance_fund_revenue_receivable_scaled = spot_market
            .insurance_fund_revenue_receivable_scaled
            .safe_sub(balance_delta)?;
    }

    Ok(receivable_payment)
}

pub fn settle_revenue_to_insurance_fund(
    spot_market_vault_amount: u64,
    insurance_vault_amount: u64,
    spot_market: &mut SpotMarket,
    now: i64,
    check_invariants: bool,
    funding_paused: bool,
) -> VelocityResult<u64> {
    book_revenue_to_insurance_fund(
        spot_market_vault_amount,
        insurance_vault_amount,
        spot_market,
        now,
        check_invariants,
        funding_paused,
    )?;

    settle_insurance_fund_revenue_receivable(spot_market)
}

pub fn resolve_perp_pnl_deficit(
    vault_amount: u64,
    insurance_vault_amount: u64,
    spot_market: &mut SpotMarket,
    market: &mut PerpMarket,
    now: i64,
    funding_paused: bool,
) -> VelocityResult<u64> {
    validate!(
        market.amm.is_underwater(),
        ErrorCode::NoAmmPerpPnlDeficit,
        "market.amm.total_fee_minus_distributions={} must be negative",
        market.amm.total_fee_minus_distributions
    )?;

    // Accrue the quote market's cumulative interest to `now` BEFORE valuing the
    // pnl pool. `get_token_amount` scales `pnl_pool.scaled_balance` by
    // `cumulative_deposit_interest`, so a stale (un-accrued) index understates
    // the pool. The sufficiency gate below rejects an IF draw whenever the pool
    // already covers `net_user_pnl`; sizing that gate off a stale-low pool would
    // draw from the insurance fund even when a current-interest pool suffices.
    update_spot_market_cumulative_interest(spot_market, None, now, funding_paused)?;

    let pnl_pool_token_amount = get_token_amount(
        market.pnl_pool.scaled_balance,
        spot_market,
        &SpotBalanceType::Deposit,
    )?;

    let net_user_pnl = calculate_net_user_pnl(
        &market.amm,
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min,
        market.quote_asset_amount,
        market.net_unsettled_funding_pnl,
    )?;

    validate!(
        pnl_pool_token_amount.cast::<i128>()? < net_user_pnl,
        ErrorCode::SufficientPerpPnlPool,
        "pnl_pool_token_amount >= net_user_pnl ({} >= {})",
        pnl_pool_token_amount,
        net_user_pnl
    )?;

    let total_if_shares_before = spot_market.insurance_fund.total_shares;

    let excess_user_pnl_imbalance = if market.unrealized_pnl_max_imbalance > 0 {
        let net_unsettled_pnl = calculate_net_user_pnl(
            &market.amm,
            market.market_stats.historical_oracle_data.last_oracle_price,
            market.quote_asset_amount,
            market.net_unsettled_funding_pnl,
        )?;

        net_unsettled_pnl.safe_sub(market.unrealized_pnl_max_imbalance.cast()?)?
    } else {
        0
    };

    validate!(
        excess_user_pnl_imbalance > 0,
        ErrorCode::PerpPnlDeficitBelowThreshold,
        "No excess_user_pnl_imbalance({}) to settle",
        excess_user_pnl_imbalance
    )?;

    // A new revenue-settle period may have opened (e.g. the caller just ran
    // attempt_settle_revenue_to_insurance_fund) without a fee sweep in between.
    // Refresh the per-period counter here so the cap reflects the current
    // period rather than the prior one's exhausted value.
    market
        .insurance_claim
        .reset_revenue_withdraw_for_new_period(
            spot_market.insurance_fund.last_revenue_settle_ts,
            now,
        )?;

    let max_revenue_withdraw_per_period = market
        .insurance_claim
        .max_revenue_withdraw_per_period
        .cast::<i128>()?
        .safe_sub(
            market
                .insurance_claim
                .revenue_withdraw_since_last_settle
                .cast()?,
        )?
        .cast::<i128>()?;
    validate!(
        max_revenue_withdraw_per_period > 0,
        ErrorCode::MaxRevenueWithdrawPerPeriodReached,
        "max_revenue_withdraw_per_period={} as already been reached",
        max_revenue_withdraw_per_period
    )?;

    let max_insurance_withdraw = market
        .insurance_claim
        .quote_max_insurance
        .safe_sub(market.insurance_claim.quote_settled_insurance)?
        .cast::<i128>()?;

    validate!(
        max_insurance_withdraw > 0,
        ErrorCode::MaxIFWithdrawReached,
        "max_insurance_withdraw={}/{} as already been reached",
        market.insurance_claim.quote_settled_insurance,
        market.insurance_claim.quote_max_insurance,
    )?;

    let available_if_capital = spot_market
        .get_insurance_fund_revenue_receivable()?
        .cast::<i128>()?
        .safe_add(insurance_vault_amount.saturating_sub(1).cast()?)?;
    let insurance_withdraw = excess_user_pnl_imbalance
        .min(max_revenue_withdraw_per_period)
        .min(max_insurance_withdraw)
        .min(available_if_capital);

    validate!(
        insurance_withdraw > 0,
        ErrorCode::NoIFWithdrawAvailable,
        "No available funds for insurance_withdraw({}) for user_pnl_imbalance={}",
        insurance_withdraw,
        excess_user_pnl_imbalance
    )?;

    <crate::vlp::amm::AMM as crate::vlp::amm::quoter::AmmContract>::record_credit(
        &mut market.amm,
        insurance_withdraw.cast::<u64>()?,
    )?;

    market.insurance_claim.revenue_withdraw_since_last_settle = market
        .insurance_claim
        .revenue_withdraw_since_last_settle
        .safe_add(insurance_withdraw.cast()?)?;

    market.insurance_claim.quote_settled_insurance = market
        .insurance_claim
        .quote_settled_insurance
        .safe_add(insurance_withdraw.cast()?)?;

    validate!(
        market.insurance_claim.quote_settled_insurance
            <= market.insurance_claim.quote_max_insurance,
        ErrorCode::MaxIFWithdrawReached,
        "quote_settled_insurance breached its max {}/{}",
        market.insurance_claim.quote_settled_insurance,
        market.insurance_claim.quote_max_insurance,
    )?;

    market.insurance_claim.last_revenue_withdraw_ts = now;

    let receivable_payment = transfer_insurance_fund_revenue_receivable_to_pool(
        spot_market,
        &mut market.pnl_pool,
        insurance_withdraw.cast()?,
    )?;
    let insurance_vault_payment = insurance_withdraw
        .cast::<u128>()?
        .safe_sub(receivable_payment)?;

    emit!(InsuranceFundRecord {
        ts: now,
        spot_market_index: spot_market.market_index,
        perp_market_index: market.market_index,
        amount: -insurance_withdraw.cast()?,
        user_if_factor: spot_market.insurance_fund.if_fee_factor,
        total_if_factor: spot_market.insurance_fund.if_fee_factor,
        vault_amount_before: vault_amount,
        insurance_vault_amount_before: insurance_vault_amount,
        total_if_shares_before,
        total_if_shares_after: spot_market.insurance_fund.total_shares,
    });

    insurance_vault_payment.cast()
}
