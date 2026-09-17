use {
    crate::{
        error::{ErrorCode, VelocityResult},
        math::{
            casting::Cast,
            constants::{
                FIVE_MINUTE, ONE_HOUR, ONE_MINUTE, QUOTE_SPOT_MARKET_INDEX,
                SPOT_MARKET_TOKEN_TWAP_WINDOW,
            },
            oracle::{
                is_oracle_valid_for_action, oracle_validity, LogMode, OracleValidity,
                VelocityAction,
            },
            safe_math::SafeMath,
            spot_balance::{
                calculate_accumulated_interest, calculate_spot_market_utilization,
                calculate_utilization, get_interest_token_amount_with_dust, get_spot_balance,
                get_token_amount, split_deposit_interest, InterestAccumulated,
            },
            stats::{calculate_new_twap, calculate_weighted_average},
            time::SlotClock,
        },
        msg,
        state::{
            events::{SpotInterestRecord, TransferFeeAndPnlPoolDirection},
            oracle::OraclePriceData,
            oracle_map::OracleMap,
            paused_operations::SpotOperation,
            perp_market::PoolBalance,
            spot_market::{SpotBalance, SpotBalanceType, SpotMarket},
            spot_market_map::{SpotMarketMap, SpotMarketSet},
            state::ValidityGuardRails,
            user::MarketType,
        },
        validate,
        vlp::amm::math::amm::sanitize_new_price,
    },
    anchor_lang::prelude::*,
    std::cmp::max,
};

#[cfg(test)]
mod tests;

pub fn update_spot_market_twap_stats(
    spot_market: &mut SpotMarket,
    oracle_price_data: Option<&OraclePriceData>,
    now: i64,
) -> VelocityResult {
    let since_last = max(0_i64, now.safe_sub(spot_market.last_twap_ts.cast()?)?);
    let from_start = max(1_i64, SPOT_MARKET_TOKEN_TWAP_WINDOW.safe_sub(since_last)?);

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

    spot_market.deposit_token_twap = calculate_weighted_average(
        deposit_token_amount.cast()?,
        spot_market.deposit_token_twap.cast()?,
        since_last,
        from_start,
        None,
    )?
    .cast()?;

    spot_market.borrow_token_twap = calculate_weighted_average(
        borrow_token_amount.cast()?,
        spot_market.borrow_token_twap.cast()?,
        since_last,
        from_start,
        None,
    )?
    .cast()?;

    let utilization = calculate_utilization(deposit_token_amount, borrow_token_amount)?;

    spot_market.utilization_twap = calculate_weighted_average(
        utilization.cast()?,
        spot_market.utilization_twap.cast()?,
        since_last,
        from_start,
        None,
    )?
    .cast()?;

    if let Some(oracle_price_data) = oracle_price_data {
        let sanitize_clamp_denominator = spot_market.get_sanitize_clamp_denominator()?;

        let capped_oracle_update_price: i64 = sanitize_new_price(
            oracle_price_data.price,
            spot_market.historical_oracle_data.last_oracle_price_twap,
            sanitize_clamp_denominator,
        )?;

        let oracle_price_twap = calculate_new_twap(
            capped_oracle_update_price,
            now,
            spot_market.historical_oracle_data.last_oracle_price_twap,
            spot_market.historical_oracle_data.last_oracle_price_twap_ts,
            ONE_HOUR,
        )?;

        let oracle_price_twap_5min = calculate_new_twap(
            capped_oracle_update_price,
            now,
            spot_market
                .historical_oracle_data
                .last_oracle_price_twap_5min,
            spot_market.historical_oracle_data.last_oracle_price_twap_ts,
            FIVE_MINUTE as i64,
        )?;

        spot_market.historical_oracle_data.last_oracle_price = oracle_price_data.price;
        spot_market.historical_oracle_data.last_oracle_conf = oracle_price_data.confidence;
        spot_market.historical_oracle_data.last_oracle_delay = oracle_price_data.delay;

        if oracle_price_twap != spot_market.historical_oracle_data.last_oracle_price_twap
            || since_last >= (ONE_MINUTE as i64)
        {
            spot_market.historical_oracle_data.last_oracle_price_twap = oracle_price_twap;
            spot_market
                .historical_oracle_data
                .last_oracle_price_twap_5min = oracle_price_twap_5min;
            spot_market.historical_oracle_data.last_oracle_price_twap_ts = now;
        }
    }

    spot_market.last_twap_ts = now.cast()?;

    Ok(())
}

/// Stamps `last_interest_ts` forward to `now` and accrues nothing.
///
/// Use this for an interval that charges nobody interest.
/// `calculate_accumulated_interest` bills the whole `now - last_interest_ts` span at the rate
/// that applies when it runs. An interval left un-stamped is therefore billed later to
/// whoever holds debt at that time (OtterSec #115 and #117).
///
/// The stamp never moves backwards. `now` can trail the stored value, because user
/// instructions also drive the accrual and each one reads its own `Clock`.
fn stamp_interest_ts_without_accrual(spot_market: &mut SpotMarket, now: i64) -> VelocityResult {
    if now.cast::<u64>()? > spot_market.last_interest_ts {
        spot_market.last_interest_ts = now.cast()?;
    }

    Ok(())
}

pub fn update_spot_market_cumulative_interest(
    spot_market: &mut SpotMarket,
    oracle_price_data: Option<&OraclePriceData>,
    now: i64,
    funding_paused: bool,
) -> VelocityResult {
    // Freeze interest accrual when the exchange-wide funding pause
    // (`State::funding_paused`) is set or this market's
    // `UpdateCumulativeInterest` operation is paused. `funding_paused` is
    // threaded in from callers because the global flag lives on `State`, which
    // this controller does not load. TWAP stats still advance so oracle EMAs
    // stay fresh, mirroring the dedicated `update_spot_market_cumulative_interest`
    // crank.
    //
    // The clock is stamped forward as the pause is observed, so the paused interval is
    // dropped rather than deferred. A deferred span would reach the first accrual after
    // the resume and apply to the balances that exist then. A deposit made just before
    // the unpause would collect interest for time it was not deposited, and a borrow
    // opened during the pause would pay for time it did not exist (OtterSec #115). A pause
    // means interest does not accrue for that window.
    if funding_paused || spot_market.is_operation_paused(SpotOperation::UpdateCumulativeInterest) {
        stamp_interest_ts_without_accrual(spot_market, now)?;
        update_spot_market_twap_stats(spot_market, oracle_price_data, now)?;
        return Ok(());
    }

    let InterestAccumulated {
        deposit_interest,
        borrow_interest,
    } = calculate_accumulated_interest(spot_market, now)?;

    // Interest commits on the interval that it belongs to, once it is large enough to
    // represent on both indexes.
    //
    // `calculate_accumulated_interest` bills the whole span since `last_interest_ts` at the
    // rate that applies when it runs. It commits the span with an index move, and the index
    // credits every balance that exists at that moment. Balances change between cranks, and
    // every spot instruction that moves balances cranks this function first. An un-stamped
    // span is therefore billed against later balances. A deposit made in the gap earns
    // interest for time before the deposit, and a borrow opened in the gap pays interest for
    // time before the borrow (OtterSec #115 and #117).
    //
    // Three outcomes follow. The commit arm moves both indexes, pays both carveouts, carries
    // the remainders, and stamps the clock. The drop arm stamps the clock without accrual,
    // because nobody owes anything for the interval. The defer arm leaves the clock where it
    // stands and retries the span on the next crank.
    //
    // The defer arm covers an interval that does not reach a whole index unit on both sides.
    // `borrow_interest` is 1 when the borrow side floors to zero, and `deposit_interest` is 0
    // when the lender side floors to zero. A stamp would forgive that interest, and frequent
    // cranks of this permissionless accrual would then hold every interval under the floor. A
    // span therefore survives only while it stays under the floor, and it commits on the first
    // crank that clears both sides.
    if deposit_interest > 0 && borrow_interest > 1 {
        // The deposit-interest gain divides three ways. `if_fee_factor` goes to the insurance
        // fund through `revenue_pool`. `protocol_fee_factor` goes to withdrawable protocol
        // fees through `protocol_fee_pool`. Lenders receive the rest.
        //
        // `split_deposit_interest` carries the index-space remainders, so a share too small to
        // round to a whole index unit is delayed instead of lost. It also guarantees that the
        // two cuts never sum past the gain, so lenders never fall below zero and the split
        // cannot block the commit.
        let split = split_deposit_interest(spot_market, deposit_interest)?;

        // Both cuts convert to tokens against the same `deposit_balance`, before either pool is
        // credited. A credit to the first pool raises `deposit_balance`. A conversion of the
        // second cut against the raised balance would exceed its stated factor.
        //
        // The conversion can floor to zero on a small market, even when the index-space cut is
        // not zero. The value is already withheld from lenders at that point, so a floored cut
        // credits nobody. Each pool therefore carries its own token-space remainder.
        let (if_token_amount, if_token_dust) = get_interest_token_amount_with_dust(
            spot_market.deposit_balance,
            spot_market,
            split.for_insurance_fund,
            spot_market.revenue_pool.pending_interest_dust,
        )?;
        let (protocol_token_amount, protocol_token_dust) = get_interest_token_amount_with_dust(
            spot_market.deposit_balance,
            spot_market,
            split.for_protocol,
            spot_market.protocol_fee_pool.pending_interest_dust,
        )?;

        spot_market.cumulative_deposit_interest = spot_market
            .cumulative_deposit_interest
            .safe_add(split.for_lenders)?;

        spot_market.cumulative_borrow_interest = spot_market
            .cumulative_borrow_interest
            .safe_add(borrow_interest)?;
        spot_market.last_interest_ts = now.cast()?;

        // The insurance fund cut settles to the IF vault for stakers.
        if if_token_amount > 0 {
            update_revenue_pool_balances(
                if_token_amount,
                &SpotBalanceType::Deposit,
                spot_market,
                false,
            )?;
        }
        spot_market.revenue_pool.pending_interest_split_dust = split.carveout_dust;
        spot_market.revenue_pool.pending_interest_dust = if_token_dust;

        // The protocol cut is directly withdrawable.
        if protocol_token_amount > 0 {
            update_protocol_fee_pool_balances(
                protocol_token_amount,
                &SpotBalanceType::Deposit,
                spot_market,
                false,
            )?;
        }
        spot_market.protocol_fee_pool.pending_interest_split_dust = split.insurance_fund_dust;
        spot_market.protocol_fee_pool.pending_interest_dust = protocol_token_dust;

        emit!(SpotInterestRecord {
            ts: now,
            market_index: spot_market.market_index,
            deposit_balance: spot_market.deposit_balance,
            cumulative_deposit_interest: spot_market.cumulative_deposit_interest,
            borrow_balance: spot_market.borrow_balance,
            cumulative_borrow_interest: spot_market.cumulative_borrow_interest,
            optimal_utilization: spot_market.optimal_utilization,
            optimal_borrow_rate: spot_market.optimal_borrow_rate,
            max_borrow_rate: spot_market.max_borrow_rate,
        });
    } else if spot_market.borrow_balance == 0
        || calculate_spot_market_utilization(spot_market)? == 0
    {
        // Nobody borrows, so nobody owes interest for this interval. This is the same
        // condition that makes `calculate_accumulated_interest` return zero. Stamp the clock
        // to remove the idle span from the ledger.
        //
        // `borrow_balance == 0` is tested first because it is one comparison, and it answers
        // the common case. The utilization test needs two `get_token_amount` conversions and
        // a division. It stays because zero utilization is the exact condition that returns
        // zero, and it also covers a borrow so small next to deposits that the ratio floors
        // to zero.
        //
        // An un-stamped idle span would stay on the clock for the whole zero-borrow period,
        // and the first accrual after a borrow would bill that whole span at the new rate.
        // Any lender could farm that. The lender deposits into an idle market, waits for the
        // first borrower, cranks the accrual, and collects interest the new debt never owed
        // (OtterSec #117). Every path that creates a borrow cranks this function before it
        // changes balances, so the stamp is current when debt appears and a new borrow pays
        // only from its own creation.
        //
        // This branch stays narrow. It stamps only an interval that nobody owes anything for.
        stamp_interest_ts_without_accrual(spot_market, now)?;
    }

    update_spot_market_twap_stats(spot_market, oracle_price_data, now)?;

    Ok(())
}

/// Move tokens in/out of a spot market's `revenue_pool` (a Deposit-type claim
/// counted inside `deposit_balance`).
///
/// `is_leaving_velocity` must be true when the Borrow direction corresponds to
/// tokens physically exiting the spot vault (the revenue sweep into the IF
/// vault). It forces the ledger debit to round **up**, so `deposit_balance`'s
/// token value drops by at least the amount transferred out — otherwise the
/// floor-rounded share debit reduces the recorded depositor claim by less than
/// the tokens that left, pushing the vault below `depositors_claim` by the
/// rounding residue and tripping `validate_spot_market_vault_amount`. Mirrors
/// `update_protocol_fee_pool_balances`. Pass false for pure internal moves
/// (e.g. revenue_pool <-> another spot balance in the same vault) where no
/// tokens leave and net `deposit_balance` is unchanged, and for Deposit-side
/// credits where the flag is inert.
pub fn update_revenue_pool_balances(
    token_amount: u128,
    update_direction: &SpotBalanceType,
    spot_market: &mut SpotMarket,
    is_leaving_velocity: bool,
) -> VelocityResult {
    let mut spot_balance = spot_market.revenue_pool;
    update_spot_balances(
        token_amount,
        update_direction,
        spot_market,
        &mut spot_balance,
        is_leaving_velocity,
    )?;
    spot_market.revenue_pool = spot_balance;

    Ok(())
}

/// Move tokens in/out of a spot market's `protocol_fee_pool` (the directly
/// withdrawable protocol-fee claim). Mirrors `update_revenue_pool_balances`.
/// The pool is a protocol-owned Deposit-type claim inside the spot vault —
/// counted in `deposit_balance` like `revenue_pool`, but owned by the protocol
/// (not users) and never part of the insurance backstop.
/// `is_leaving_velocity` should be true when the Borrow direction corresponds to
/// tokens exiting the protocol (the recipient-wallet withdrawal).
pub fn update_protocol_fee_pool_balances(
    token_amount: u128,
    update_direction: &SpotBalanceType,
    spot_market: &mut SpotMarket,
    is_leaving_velocity: bool,
) -> VelocityResult {
    let mut spot_balance = spot_market.protocol_fee_pool;
    update_spot_balances(
        token_amount,
        update_direction,
        spot_market,
        &mut spot_balance,
        is_leaving_velocity,
    )?;
    spot_market.protocol_fee_pool = spot_balance;

    Ok(())
}

pub fn update_spot_balances(
    mut token_amount: u128,
    update_direction: &SpotBalanceType,
    spot_market: &mut SpotMarket,
    spot_balance: &mut dyn SpotBalance,
    is_leaving_velocity: bool,
) -> VelocityResult {
    let increase_user_existing_balance = update_direction == spot_balance.balance_type();
    if increase_user_existing_balance {
        let round_up = spot_balance.balance_type() == &SpotBalanceType::Borrow;
        let balance_delta =
            get_spot_balance(token_amount, spot_market, update_direction, round_up)?;
        spot_balance.increase_balance(balance_delta)?;
        increase_spot_balance(balance_delta, spot_market, update_direction)?;
    } else {
        let current_token_amount = get_token_amount(
            spot_balance.balance(),
            spot_market,
            spot_balance.balance_type(),
        )?;

        let reduce_user_existing_balance = current_token_amount != 0;
        if reduce_user_existing_balance {
            // determine how much to reduce balance based on size of current token amount
            let (token_delta, balance_delta) = if current_token_amount > token_amount {
                let round_up =
                    is_leaving_velocity || spot_balance.balance_type() == &SpotBalanceType::Borrow;
                let balance_delta = get_spot_balance(
                    token_amount,
                    spot_market,
                    spot_balance.balance_type(),
                    round_up,
                )?;
                (token_amount, balance_delta)
            } else {
                (current_token_amount, spot_balance.balance())
            };

            decrease_spot_balance(balance_delta, spot_market, spot_balance.balance_type())?;
            spot_balance.decrease_balance(balance_delta)?;
            token_amount = token_amount.safe_sub(token_delta)?;
        }

        if token_amount > 0 {
            spot_balance.update_balance_type(*update_direction)?;
            let round_up = update_direction == &SpotBalanceType::Borrow;
            let balance_delta =
                get_spot_balance(token_amount, spot_market, update_direction, round_up)?;
            spot_balance.increase_balance(balance_delta)?;
            increase_spot_balance(balance_delta, spot_market, update_direction)?;
        }
    }

    if is_leaving_velocity && update_direction == &SpotBalanceType::Borrow {
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

        validate!(
            deposit_token_amount >= borrow_token_amount,
            ErrorCode::SpotMarketInsufficientDeposits,
            "Spot Market has insufficent deposits to complete withdraw: deposits ({}) borrows ({})",
            deposit_token_amount,
            borrow_token_amount
        )?;
    }

    Ok(())
}

pub fn transfer_spot_balances(
    token_amount: i128,
    spot_market: &mut SpotMarket,
    from_spot_balance: &mut dyn SpotBalance,
    to_spot_balance: &mut dyn SpotBalance,
) -> VelocityResult {
    validate!(
        from_spot_balance.market_index() == to_spot_balance.market_index(),
        ErrorCode::UnequalMarketIndexForSpotTransfer,
        "transfer market indexes arent equal",
    )?;

    if token_amount == 0 {
        return Ok(());
    }

    if from_spot_balance.balance_type() == &SpotBalanceType::Deposit {
        validate!(
            spot_market.deposit_balance >= from_spot_balance.balance(),
            ErrorCode::InvalidSpotMarketState,
            "spot_market.deposit_balance={} lower than individual spot balance={}",
            spot_market.deposit_balance,
            from_spot_balance.balance()
        )?;
    }

    update_spot_balances(
        token_amount.unsigned_abs(),
        if token_amount < 0 {
            &SpotBalanceType::Deposit
        } else {
            &SpotBalanceType::Borrow
        },
        spot_market,
        from_spot_balance,
        false,
    )?;

    update_spot_balances(
        token_amount.unsigned_abs(),
        if token_amount < 0 {
            &SpotBalanceType::Borrow
        } else {
            &SpotBalanceType::Deposit
        },
        spot_market,
        to_spot_balance,
        false,
    )?;

    Ok(())
}

pub fn transfer_revenue_pool_to_spot_balance(
    token_amount: u128,
    spot_market: &mut SpotMarket,
    to_spot_balance: &mut dyn SpotBalance,
) -> VelocityResult {
    validate!(
        to_spot_balance.market_index() == spot_market.market_index,
        ErrorCode::UnequalMarketIndexForSpotTransfer,
        "transfer market indexes arent equal",
    )?;

    // Internal move within the same vault (revenue_pool -> another spot
    // balance); no tokens leave, so floor rounding is fine.
    update_revenue_pool_balances(token_amount, &SpotBalanceType::Borrow, spot_market, false)?;

    update_spot_balances(
        token_amount,
        &SpotBalanceType::Deposit,
        spot_market,
        to_spot_balance,
        false,
    )?;

    Ok(())
}

pub fn transfer_spot_balance_to_revenue_pool(
    token_amount: u128,
    spot_market: &mut SpotMarket,
    from_spot_balance: &mut dyn SpotBalance,
) -> VelocityResult {
    validate!(
        from_spot_balance.market_index() == spot_market.market_index,
        ErrorCode::UnequalMarketIndexForSpotTransfer,
        "transfer market indexes arent equal",
    )?;

    update_spot_balances(
        token_amount,
        &SpotBalanceType::Borrow,
        spot_market,
        from_spot_balance,
        false,
    )?;

    update_revenue_pool_balances(token_amount, &SpotBalanceType::Deposit, spot_market, false)?;

    Ok(())
}

/// Outcome of [`update_spot_market_and_check_validity`], carrying both the verdict and the
/// TWAP the verdict was reached against.
#[derive(Clone, Copy, Debug)]
pub struct SpotMarketOracleRefresh {
    /// Computed validity, so callers can apply stricter, action-specific handling (e.g.
    /// liquidation pricing collateral protectively when the oracle is margin-invalid). The
    /// quote spot market skips validity checks and reports `Valid`.
    pub validity: OracleValidity,
    /// The 5-minute oracle TWAP as it stood before the refresh advanced it. A caller that
    /// bounds a price against this TWAP must read this snapshot and not the field, for the
    /// same reason the verdict is computed first (OtterSec #109 to #112, and #134).
    pub pre_refresh_twap_5min: i64,
}

/// Judges the oracle against the TWAPs as they stand, and does not advance them. The quote
/// spot market has no oracle to judge and reports `Valid`.
///
/// Use this when the caller reads an oracle TWAP later in the same transaction and must not
/// move it first. Use [`update_spot_market_and_check_validity`] when the caller also owns the
/// refresh.
pub fn check_spot_oracle_validity(
    spot_market: &SpotMarket,
    oracle_price_data: &OraclePriceData,
    validity_guard_rails: &ValidityGuardRails,
    action: Option<VelocityAction>,
    log_mode: LogMode,
    current_slot: u64,
    slot_clock: SlotClock,
) -> VelocityResult<OracleValidity> {
    if spot_market.market_index == QUOTE_SPOT_MARKET_INDEX {
        return Ok(OracleValidity::Valid);
    }

    // 1 hour EMA
    let risk_ema_price = spot_market.historical_oracle_data.last_oracle_price_twap;

    let validity = oracle_validity(
        MarketType::Spot,
        spot_market.market_index,
        risk_ema_price,
        oracle_price_data,
        validity_guard_rails,
        spot_market.get_max_confidence_interval_multiplier()?,
        &spot_market.oracle_source,
        log_mode,
        -1,
        false, // exchange-oracle price, never MM-sourced
        0,
        current_slot,
        slot_clock,
    )?;

    validate!(
        is_oracle_valid_for_action(validity, action)?,
        ErrorCode::InvalidOracle,
        "Invalid Oracle ({:?} vs ema={:?}) for spot market index={} and action={:?}",
        oracle_price_data,
        risk_ema_price,
        spot_market.market_index,
        action
    )?;

    Ok(validity)
}

/// Judges the oracle against the TWAPs as they stand on entry, then advances them.
///
/// An instruction must not relax a gate that reads a value it just moved. The refresh drags
/// both oracle TWAPs toward the live price, so a gate that reads them afterwards lets a
/// too-volatile or depressed oracle pass the check meant to stop it. The direct spot
/// liquidation lane does advance these TWAPs and does gate on them, so the refresh stays and
/// runs after the gate. Its gates read `pre_refresh_twap_5min`.
pub fn update_spot_market_and_check_validity(
    spot_market: &mut SpotMarket,
    oracle_price_data: &OraclePriceData,
    validity_guard_rails: &ValidityGuardRails,
    now: i64,
    action: Option<VelocityAction>,
    funding_paused: bool,
    current_slot: u64,
    slot_clock: SlotClock,
) -> VelocityResult<SpotMarketOracleRefresh> {
    let pre_refresh_twap_5min = spot_market
        .historical_oracle_data
        .last_oracle_price_twap_5min;

    let validity = check_spot_oracle_validity(
        spot_market,
        oracle_price_data,
        validity_guard_rails,
        action,
        LogMode::ExchangeOracle,
        current_slot,
        slot_clock,
    )?;

    // update spot market EMAs with new/current data
    update_spot_market_cumulative_interest(
        spot_market,
        Some(oracle_price_data),
        now,
        funding_paused,
    )?;

    Ok(SpotMarketOracleRefresh {
        validity,
        pre_refresh_twap_5min,
    })
}

/// Advances the lending-interest indexes of several spot markets in one pass.
///
/// Every market goes through [`update_spot_market_cumulative_interest`], so a caller that
/// refreshes many markets gets the same per-market treatment as a caller that refreshes one.
/// Pass the same set that loaded `spot_market_map`. Each index must be writable in the map, and
/// the map rejects a repeated index at load, so no market is refreshed twice.
///
/// `oracle_map` is optional, and the choice belongs to the caller. Pass `None` when the same
/// instruction later reads a market's oracle TWAP. [`update_spot_market_twap_stats`] pulls
/// `last_oracle_price_twap` toward the live price, so a later check against that TWAP measures
/// against a value this call moved. Pass `Some` only from an instruction that consumes no oracle
/// TWAP of its own.
pub fn refresh_spot_market_interest(
    spot_market_map: &SpotMarketMap,
    mut oracle_map: Option<&mut OracleMap>,
    market_indexes: &SpotMarketSet,
    now: i64,
    funding_paused: bool,
) -> VelocityResult {
    market_indexes.iter().try_for_each(|market_index| {
        let spot_market = &mut spot_market_map.get_ref_mut(market_index)?;

        // Copy the price out so the oracle map is free again before the refresh. The map is
        // borrowed through an `Option` across loop iterations, and holding the reference would
        // keep that borrow alive for the whole body.
        let oracle_price_data = match &mut oracle_map {
            Some(oracle_map) => Some(*oracle_map.get_price_data(&spot_market.oracle_id())?),
            None => None,
        };

        update_spot_market_cumulative_interest(
            spot_market,
            oracle_price_data.as_ref(),
            now,
            funding_paused,
        )
    })
}

fn increase_spot_balance(
    delta: u128,
    spot_market: &mut SpotMarket,
    balance_type: &SpotBalanceType,
) -> VelocityResult {
    match balance_type {
        SpotBalanceType::Deposit => {
            spot_market.deposit_balance = spot_market.deposit_balance.safe_add(delta)?
        }
        SpotBalanceType::Borrow => {
            spot_market.borrow_balance = spot_market.borrow_balance.safe_add(delta)?
        }
    }

    Ok(())
}

fn decrease_spot_balance(
    delta: u128,
    spot_market: &mut SpotMarket,
    balance_type: &SpotBalanceType,
) -> VelocityResult {
    match balance_type {
        SpotBalanceType::Deposit => {
            spot_market.deposit_balance = spot_market.deposit_balance.safe_sub(delta)?
        }
        SpotBalanceType::Borrow => {
            spot_market.borrow_balance = spot_market.borrow_balance.safe_sub(delta)?
        }
    }

    Ok(())
}

pub fn execute_transfer_between_pools(
    amount: u64,
    spot_market: &mut SpotMarket,
    fee_pool: &mut PoolBalance,
    pnl_pool: &mut PoolBalance,
    fee_pool_market_index: u16,
    pnl_pool_market_index: u16,
    direction: TransferFeeAndPnlPoolDirection,
) -> Result<()> {
    let (source_name, dest_name, source_index, dest_index, source_token_amount) = match direction {
        TransferFeeAndPnlPoolDirection::FeeToPnlPool => (
            "fee",
            "pnl",
            fee_pool_market_index,
            pnl_pool_market_index,
            get_token_amount(
                fee_pool.scaled_balance,
                spot_market,
                &SpotBalanceType::Deposit,
            )?,
        ),
        TransferFeeAndPnlPoolDirection::PnlToFeePool => (
            "pnl",
            "fee",
            pnl_pool_market_index,
            fee_pool_market_index,
            get_token_amount(
                pnl_pool.scaled_balance,
                spot_market,
                &SpotBalanceType::Deposit,
            )?,
        ),
    };

    validate!(
        amount.cast::<u128>()? <= source_token_amount,
        ErrorCode::DefaultError,
        "insufficient {} pool balance: {} < {}",
        source_name,
        source_token_amount,
        amount,
    )?;

    msg!(
        "transferring {} from perp market {} {} pool -> perp market {} {} pool",
        amount,
        source_index,
        source_name,
        dest_index,
        dest_name,
    );

    match direction {
        TransferFeeAndPnlPoolDirection::FeeToPnlPool => {
            transfer_spot_balances(amount.cast::<i128>()?, spot_market, fee_pool, pnl_pool)?;
        }
        TransferFeeAndPnlPoolDirection::PnlToFeePool => {
            transfer_spot_balances(amount.cast::<i128>()?, spot_market, pnl_pool, fee_pool)?;
        }
    }

    msg!(
        "transferred {} fee_pool(market {}) token_amount: {} pnl_pool(market {}) token_amount: {}",
        amount,
        fee_pool_market_index,
        get_token_amount(
            fee_pool.scaled_balance,
            spot_market,
            &SpotBalanceType::Deposit
        )?,
        pnl_pool_market_index,
        get_token_amount(
            pnl_pool.scaled_balance,
            spot_market,
            &SpotBalanceType::Deposit
        )?,
    );

    Ok(())
}
