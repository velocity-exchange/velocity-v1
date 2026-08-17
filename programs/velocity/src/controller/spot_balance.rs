use {
    crate::{
        error::{ErrorCode, VelocityResult},
        math::{
            casting::Cast,
            constants::{
                FIVE_MINUTE, IF_FACTOR_PRECISION, ONE_HOUR, ONE_MINUTE, QUOTE_SPOT_MARKET_INDEX,
                SPOT_MARKET_TOKEN_TWAP_WINDOW,
            },
            oracle::{
                is_oracle_valid_for_action, oracle_validity, LogMode, OracleValidity,
                VelocityAction,
            },
            safe_math::SafeMath,
            spot_balance::{
                calculate_accumulated_interest, calculate_spot_market_utilization,
                calculate_utilization, get_interest_token_amount, get_spot_balance,
                get_token_amount, InterestAccumulated,
            },
            stats::{calculate_new_twap, calculate_weighted_average},
        },
        msg,
        state::{
            events::{SpotInterestRecord, TransferFeeAndPnlPoolDirection},
            oracle::OraclePriceData,
            paused_operations::SpotOperation,
            perp_market::PoolBalance,
            spot_market::{SpotBalance, SpotBalanceType, SpotMarket},
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
            // Roll before the write, so the anchors take the TWAPs from before
            // this update. The gates read the anchors.
            let SpotMarket {
                historical_oracle_data,
                settled_oracle_twaps,
                ..
            } = &mut *spot_market;
            settled_oracle_twaps.roll(historical_oracle_data, now)?;

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

/// Stamp `last_interest_ts` forward to `now` **without** accruing anything.
///
/// Used for intervals in which no interest is charged to anyone. This is load-bearing, not
/// bookkeeping: `calculate_accumulated_interest` bills the entire `now - last_interest_ts`
/// span at whatever rate prevails when it finally runs, so an interval left un-stamped is
/// billed retroactively to whoever happens to hold debt later (findings #115, #117).
///
/// Never moves the stamp backwards — `now` can trail the stored value (the accrual is also
/// driven from user instructions, whose `now` comes from their own `Clock`).
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
    // dropped rather than deferred. Previously it was left in place and the first accrual
    // after resume applied the whole paused span to whatever balances existed at that
    // moment: a deposit made just before the unpause collected interest for time it was not
    // deposited, and a borrow opened during the pause was charged for time it did not exist
    // (finding #115). A pause means interest does not accrue for that window — not that it
    // accrues and is billed later to a different set of balances.
    if funding_paused || spot_market.is_operation_paused(SpotOperation::UpdateCumulativeInterest) {
        stamp_interest_ts_without_accrual(spot_market, now)?;
        update_spot_market_twap_stats(spot_market, oracle_price_data, now)?;
        return Ok(());
    }

    let InterestAccumulated {
        deposit_interest,
        borrow_interest,
    } = calculate_accumulated_interest(spot_market, now)?;

    // This interval has exactly three possible outcomes, and only two of them appear as
    // branches below. Naming all three here because the third is the absence of action:
    //
    //   COMMIT — bump both indexes, pay both carveouts, stamp the clock.
    //   DROP   — nobody owes anything for this interval (paused above, or zero utilization),
    //            so stamp the clock without accruing. The interval leaves the ledger.
    //   DEFER  — something IS owed but cannot be paid in full yet (the split rounds to
    //            nothing, or a configured carveout would floor to zero). Fall through both
    //            branches, touching nothing: the clock stays put so the same interval is
    //            retried later against a longer span.
    //
    // DROP and DEFER are the load-bearing distinction. Dropping an interval that is owed
    // forgives interest (the shape of #127); deferring an interval that is not owed leaves it
    // on the clock to be billed retroactively to whoever holds debt later (#115, #117).
    if deposit_interest > 0 && borrow_interest > 1 {
        // Explicit lending-gain carveouts (replaces the old single `total_factor`
        // skim). Two independent cuts taken off the deposit-interest gain:
        //   - `if_fee_factor`     -> insurance fund (revenue_pool, staker-owned)
        //   - `protocol_fee_factor`  -> withdrawable protocol fees (protocol_fee_pool)
        // Lenders receive whatever remains.
        let deposit_interest_for_if = deposit_interest
            .safe_mul(spot_market.insurance_fund.if_fee_factor as u128)?
            .safe_div(IF_FACTOR_PRECISION)?;

        let deposit_interest_for_protocol = deposit_interest
            .safe_mul(spot_market.protocol_fee_factor as u128)?
            .safe_div(IF_FACTOR_PRECISION)?;

        let deposit_interest_for_lenders = deposit_interest
            .safe_sub(deposit_interest_for_if)?
            .safe_sub(deposit_interest_for_protocol)?;

        // convert both carveouts to tokens against the SAME pre-credit
        // deposit_balance — crediting the first pool grows deposit_balance,
        // and converting the second cut against the grown balance would
        // skew it above its stated factor (order-dependence)
        //
        // An unconfigured cut is structurally zero, so skip its conversion rather than
        // multiplying and dividing to reach 0. Most markets run with both factors at zero and
        // this function is cranked by nearly every spot-touching instruction.
        let if_token_amount = if spot_market.insurance_fund.if_fee_factor == 0 {
            0
        } else {
            get_interest_token_amount(
                spot_market.deposit_balance,
                spot_market,
                deposit_interest_for_if,
            )?
        };
        let protocol_token_amount = if spot_market.protocol_fee_factor == 0 {
            0
        } else {
            get_interest_token_amount(
                spot_market.deposit_balance,
                spot_market,
                deposit_interest_for_protocol,
            )?
        };

        // A configured carveout must actually reach its pool before the interval is committed.
        //
        // The cuts are withheld from lenders in *index* terms (`deposit_interest_for_lenders`
        // is net of both), but only reach `revenue_pool` / `protocol_fee_pool` if they convert
        // to at least one token: `deposit_balance * cut / 10^(19 - decimals)`. When a cut
        // converted to zero the value was withheld from lenders and credited to nobody — it
        // became unattributed slack in the vault — and `last_interest_ts` advanced anyway, so
        // the interval could never be retried. Cranking this (permissionless) accrual at short
        // enough intervals kept every cut under one token indefinitely, permanently forfeiting
        // the insurance fund's and the protocol's entire share of lending yield (finding #127).
        //
        // Deferring is safe from liveness: the cut grows linearly with the un-stamped interval
        // and the clock only advances on commit, so frequent cranking cannot hold the interval
        // short — every configured cut eventually clears a token. The tradeoff is that accrual
        // lands in coarser steps on very small markets (on a $1M market at a 0.1% factor a cut
        // clears a token in ~16s; on a dust-sized market it can defer for hours). This is the
        // mirror image of the #117 treatment: an interval nobody owes anything for is stamped
        // and dropped, an interval that *is* owed is deferred until it can be paid in full.
        //
        // Exempt `deposit_balance == 0`, the one case where the conversion is structurally zero
        // regardless of how long the interval grows — deferring there would never converge and
        // would leave borrowers uncharged forever.
        let carveouts_payable = spot_market.deposit_balance == 0
            || ((spot_market.insurance_fund.if_fee_factor == 0 || if_token_amount > 0)
                && (spot_market.protocol_fee_factor == 0 || protocol_token_amount > 0));

        if deposit_interest_for_lenders > 0 && carveouts_payable {
            spot_market.cumulative_deposit_interest = spot_market
                .cumulative_deposit_interest
                .safe_add(deposit_interest_for_lenders)?;

            spot_market.cumulative_borrow_interest = spot_market
                .cumulative_borrow_interest
                .safe_add(borrow_interest)?;
            spot_market.last_interest_ts = now.cast()?;

            // IF cut -> revenue_pool (settles to IF vault for stakers)
            if if_token_amount > 0 {
                update_revenue_pool_balances(
                    if_token_amount,
                    &SpotBalanceType::Deposit,
                    spot_market,
                    false,
                )?;
            }

            // protocol cut -> protocol_fee_pool (directly withdrawable)
            if protocol_token_amount > 0 {
                update_protocol_fee_pool_balances(
                    protocol_token_amount,
                    &SpotBalanceType::Deposit,
                    spot_market,
                    false,
                )?;
            }

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
        }
    } else if spot_market.borrow_balance == 0
        || calculate_spot_market_utilization(spot_market)? == 0
    {
        // Nothing is borrowed, so no interest is owed by anyone for this interval — the same
        // condition on which `calculate_accumulated_interest` returns zero. Stamp the clock
        // so the idle span leaves the ledger.
        //
        // `borrow_balance == 0` is checked first purely to avoid the work: it is the case that
        // actually occurs, and it settles the question with one comparison instead of two
        // `get_token_amount` conversions and a division that `calculate_accumulated_interest`
        // already performed a few lines above. The utilization arm is kept because zero
        // utilization is the exact condition that function returns zero on, and it also covers
        // a borrow so small relative to deposits that the ratio floors to zero.
        //
        // Left un-stamped it stayed on the clock for the whole zero-borrow epoch, and the
        // first accrual after a borrow appeared billed that entire span at the newly non-zero
        // rate. Any lender could farm it: deposit into an idle market, wait for the first
        // borrower, crank the accrual, and collect interest the fresh debt never owed
        // (finding #117). Every borrow-creating path cranks this function *before* touching
        // balances, so the stamp is always current at the instant debt appears and a new
        // borrow can only ever be charged from its own creation.
        //
        // Deliberately narrow: only the genuinely-nothing-owed case stamps. When utilization
        // is non-zero but the interval is too short for the split (or for a configured
        // carveout, see above) to clear a unit, the clock is left alone so the accrual is
        // deferred, not forgiven — stamping there would let frequent cranking zero out
        // borrowers' interest, which is the shape of #127.
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

/// Returns the computed [`OracleValidity`] so callers can apply stricter, action-specific
/// handling (e.g. liquidation pricing collateral protectively when the oracle is
/// margin-invalid). The quote spot market skips validity checks and reports `Valid`.
pub fn update_spot_market_and_check_validity(
    spot_market: &mut SpotMarket,
    oracle_price_data: &OraclePriceData,
    validity_guard_rails: &ValidityGuardRails,
    now: i64,
    action: Option<VelocityAction>,
    funding_paused: bool,
) -> VelocityResult<OracleValidity> {
    // update spot market EMAs with new/current data
    update_spot_market_cumulative_interest(
        spot_market,
        Some(oracle_price_data),
        now,
        funding_paused,
    )?;

    if spot_market.market_index == QUOTE_SPOT_MARKET_INDEX {
        return Ok(OracleValidity::Valid);
    }

    // 1 hour EMA
    let risk_ema_price = spot_market.historical_oracle_data.last_oracle_price_twap;

    let oracle_validity = oracle_validity(
        MarketType::Spot,
        spot_market.market_index,
        risk_ema_price,
        oracle_price_data,
        validity_guard_rails,
        spot_market.get_max_confidence_interval_multiplier()?,
        &spot_market.oracle_source,
        LogMode::ExchangeOracle,
        -1,
        false, // exchange-oracle price, never MM-sourced
        0,
    )?;

    validate!(
        is_oracle_valid_for_action(oracle_validity, action)?,
        ErrorCode::InvalidOracle,
        "Invalid Oracle ({:?} vs ema={:?}) for spot market index={} and action={:?}",
        oracle_price_data,
        risk_ema_price,
        spot_market.market_index,
        action
    )?;

    Ok(oracle_validity)
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
