use {
    super::position::get_position_index,
    crate::{
        controller::{
            self, spot_balance::update_spot_balances,
            spot_position::update_spot_balances_and_cumulative_deposits,
        },
        error::{ErrorCode, VelocityResult},
        get_then_update_id,
        instructions::optional_accounts::AccountMaps,
        math::{
            casting::Cast,
            liquidation::is_isolated_margin_being_liquidated,
            margin::{validate_spot_margin_trading, MarginRequirementType},
            safe_math::SafeMath,
            spot_withdraw::{check_deposit_limits, check_withdraw_limits},
        },
        state::{
            events::{DepositDirection, DepositExplanation, DepositRecord},
            margin_calculation::MarginTypeConfig,
            market_status::MarketStatus,
            paused_operations::SpotOperation,
            spot_market::SpotBalanceType,
            state::State,
            user::{User, UserStats},
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[cfg(test)]
mod tests;

pub fn deposit_into_isolated_perp_position<'c: 'info, 'info>(
    user_key: Pubkey,
    user: &mut User,
    maps: &mut AccountMaps,
    slot: u64,
    now: i64,
    state: &State,
    spot_market_index: u16,
    perp_market_index: u16,
    amount: u64,
) -> VelocityResult<()> {
    validate!(
        amount != 0,
        ErrorCode::InsufficientDeposit,
        "deposit amount cant be 0",
    )?;

    validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

    let perp_market = maps.perp_market_map.get_ref(&perp_market_index)?;

    validate!(
        perp_market.quote_spot_market_index == spot_market_index,
        ErrorCode::InvalidIsolatedPerpMarket,
        "perp market quote spot market index ({}) != spot market index ({})",
        perp_market.quote_spot_market_index,
        spot_market_index
    )?;
    drop(perp_market);

    let mut spot_market = maps.spot_market_map.get_ref_mut(&spot_market_index)?;
    let oracle_price_data = *maps.oracle_map.get_price_data(&spot_market.oracle_id())?;

    validate!(
        user.pool_id == spot_market.pool_id,
        ErrorCode::InvalidPoolId,
        "user pool id ({}) != market pool id ({})",
        user.pool_id,
        spot_market.pool_id
    )?;

    validate!(
        !matches!(spot_market.status, MarketStatus::Initialized),
        ErrorCode::MarketBeingInitialized,
        "Market is being initialized"
    )?;

    // Accrue interest, but do not advance the market's oracle TWAPs
    // (OtterSec #134, the same shape as #110 and #111). A refresh drags the
    // stored TWAPs toward the live price, and a check in the same instruction
    // then reads the dragged values.
    controller::spot_balance::update_spot_market_cumulative_interest(
        &mut spot_market,
        None,
        now,
        state.funding_paused()?,
    )?;

    user.increment_total_deposits(
        amount,
        oracle_price_data.price,
        spot_market.get_precision().cast()?,
    )?;

    let total_deposits_after = user.total_deposits;
    let total_withdraws_after = user.total_withdraws;

    {
        let perp_position = user.force_get_isolated_perp_position_mut(perp_market_index)?;

        update_spot_balances(
            amount.cast::<u128>()?,
            &SpotBalanceType::Deposit,
            &mut spot_market,
            perp_position,
            false,
        )?;
    }

    validate!(
        matches!(spot_market.status, MarketStatus::Active),
        ErrorCode::MarketActionPaused,
        "spot_market not active",
    )?;

    // The daily deposit cap counts tokens that enter the spot market vault.
    // This deposit enters the same vault as a cross-margin deposit, so the same
    // cap applies. `handle_deposit` checks it on the cross path. A cap that one
    // instruction can bypass is not a cap. The check does nothing when the
    // market has no cap configured, meaning `max_deposit_bps_per_day == 0`.
    validate!(
        check_deposit_limits(&spot_market)?,
        ErrorCode::DailyDepositLimit,
        "Spot Market {} has hit daily deposit limit (deposits exceed {} bps above 24h twap)",
        spot_market_index,
        spot_market.max_deposit_bps_per_day
    )?;

    drop(spot_market);

    if user.is_isolated_margin_being_liquidated(perp_market_index)? {
        // try to update liquidation status if user is was already being liq'd
        let is_being_liquidated = is_isolated_margin_being_liquidated(
            user,
            maps,
            perp_market_index,
            state.liquidation_margin_buffer_ratio,
        )?;

        if !is_being_liquidated {
            user.exit_isolated_margin_liquidation(perp_market_index)?;
        }
    }

    user.update_last_active_slot(slot);

    let spot_market = &mut maps.spot_market_map.get_ref_mut(&spot_market_index)?;

    let deposit_record_id = get_then_update_id!(spot_market, next_deposit_record_id);
    let oracle_price = oracle_price_data.price;

    let deposit_record = DepositRecord {
        ts: now,
        deposit_record_id,
        user_authority: user.authority,
        user: user_key,
        direction: DepositDirection::Deposit,
        amount,
        oracle_price,
        market_deposit_balance: spot_market.deposit_balance,
        market_withdraw_balance: spot_market.borrow_balance,
        market_cumulative_deposit_interest: spot_market.cumulative_deposit_interest,
        market_cumulative_borrow_interest: spot_market.cumulative_borrow_interest,
        total_deposits_after,
        total_withdraws_after,
        market_index: spot_market_index,
        explanation: DepositExplanation::None,
        transfer_user: None,
        user_token_amount_after: user.get_total_token_amount(spot_market)?,
        signer: None,
    };

    emit!(deposit_record);

    Ok(())
}

/// Moves collateral between the account's cross-margin spot position and its
/// isolated position in the same spot market.
///
/// This function does not check the withdraw limits, and it must not. No token
/// leaves the spot market vault. Both legs are equal and opposite inside one
/// market, so `deposit_balance` and `borrow_balance` end where they started.
/// The TVL check at the end of the function confirms that. The withdraw
/// circuit breaker limits the rate of vault outflow, so it has nothing to
/// measure here.
///
/// The transfer can still change who is eligible for the small-depositor
/// exception. Moving most of a large cross deposit into an isolated position
/// leaves the cross position under the per-account allowance. That does not
/// yield extra funds. The market-level `exception_floor` in
/// `check_withdraw_limits` caps the total exception outflow per market, and
/// `withdraw_from_isolated_perp_position` applies the market-level check to the
/// isolated balance.
pub fn transfer_isolated_perp_position_deposit<'c: 'info, 'info>(
    user: &mut User,
    user_stats: Option<&mut UserStats>,
    maps: &mut AccountMaps,
    slot: u64,
    now: i64,
    spot_market_index: u16,
    perp_market_index: u16,
    amount: i64,
    funding_paused: bool,
) -> VelocityResult<()> {
    validate!(
        amount != 0,
        ErrorCode::DefaultError,
        "transfer amount cant be 0",
    )?;

    validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

    let tvl_before;
    {
        let perp_market = &maps.perp_market_map.get_ref(&perp_market_index)?;
        let spot_market = &mut maps.spot_market_map.get_ref_mut(&spot_market_index)?;

        validate!(
            perp_market.quote_spot_market_index == spot_market_index,
            ErrorCode::InvalidIsolatedPerpMarket,
            "perp market quote spot market index ({}) != spot market index ({})",
            perp_market.quote_spot_market_index,
            spot_market_index
        )?;

        validate!(
            user.pool_id == spot_market.pool_id && user.pool_id == perp_market.pool_id,
            ErrorCode::InvalidPoolId,
            "user pool id ({}) != market pool id ({})",
            user.pool_id,
            spot_market.pool_id
        )?;

        // Accrue interest, but do not advance the market's oracle TWAPs
        // (OtterSec #134, the same shape as #110 and #111).
        //
        // The `MarginRequirementType::Initial` gate later in this flow enables
        // strict pricing. `StrictOraclePrice` bounds are the min and max of the
        // live price and `last_oracle_price_twap_5min`, and a liability is
        // priced at the upper bound. Refreshing that TWAP here drags it toward
        // a temporarily depressed live price and under-values the debt the gate
        // must catch.
        controller::spot_balance::update_spot_market_cumulative_interest(
            spot_market,
            None,
            now,
            funding_paused,
        )?;

        tvl_before = spot_market.get_tvl()?;
    }

    if amount > 0 {
        let mut spot_market = maps.spot_market_map.get_ref_mut(&spot_market_index)?;

        let spot_position_index = user.force_get_spot_position_index(spot_market.market_index)?;
        update_spot_balances_and_cumulative_deposits(
            amount as u128,
            &SpotBalanceType::Borrow,
            &mut spot_market,
            &mut user.spot_positions[spot_position_index],
            false,
            None,
        )?;

        update_spot_balances(
            amount as u128,
            &SpotBalanceType::Deposit,
            &mut spot_market,
            user.force_get_isolated_perp_position_mut(perp_market_index)?,
            false,
        )?;

        drop(spot_market);

        if let Some(_user_stats) = user_stats {
            user.meets_transfer_isolated_position_deposit_margin_requirement(
                maps,
                MarginTypeConfig::CrossMarginOverride {
                    margin_requirement_type: MarginRequirementType::Initial,
                    default_margin_requirement_type: MarginRequirementType::Maintenance,
                },
                true,
                perp_market_index,
            )?;

            validate_spot_margin_trading(user, maps)?;

            if user.is_cross_margin_being_liquidated() {
                user.exit_cross_margin_liquidation();
            }
        } else {
            msg!("Cant transfer isolated position deposit without user stats");
            return Err(ErrorCode::DefaultError);
        }
    } else {
        let mut spot_market = maps.spot_market_map.get_ref_mut(&spot_market_index)?;

        let isolated_perp_position_token_amount = user
            .force_get_isolated_perp_position_mut(perp_market_index)?
            .get_isolated_token_amount(&spot_market)?;

        // i64::MIN is used to transfer the entire isolated position deposit
        let amount = if amount == i64::MIN {
            isolated_perp_position_token_amount
        } else {
            amount.unsigned_abs() as u128
        };

        validate!(
            amount <= isolated_perp_position_token_amount,
            ErrorCode::InsufficientCollateral,
            "user has insufficient deposit for market {}",
            spot_market_index
        )?;

        let spot_position_index = user.force_get_spot_position_index(spot_market.market_index)?;
        update_spot_balances_and_cumulative_deposits(
            amount,
            &SpotBalanceType::Deposit,
            &mut spot_market,
            &mut user.spot_positions[spot_position_index],
            false,
            None,
        )?;

        update_spot_balances(
            amount,
            &SpotBalanceType::Borrow,
            &mut spot_market,
            user.force_get_isolated_perp_position_mut(perp_market_index)?,
            false,
        )?;

        drop(spot_market);

        if let Some(_user_stats) = user_stats {
            user.meets_transfer_isolated_position_deposit_margin_requirement(
                maps,
                MarginTypeConfig::IsolatedPositionOverride {
                    margin_requirement_type: MarginRequirementType::Initial,
                    default_isolated_margin_requirement_type: MarginRequirementType::Maintenance,
                    cross_margin_requirement_type: MarginRequirementType::Maintenance,
                    market_index: perp_market_index,
                },
                false,
                perp_market_index,
            )?;

            if user.is_isolated_margin_being_liquidated(perp_market_index)? {
                user.exit_isolated_margin_liquidation(perp_market_index)?;
            }
        } else if get_position_index(&user.perp_positions, perp_market_index).is_ok() {
            msg!("Cant transfer isolated position deposit without user stats if position is still open");
            return Err(ErrorCode::DefaultError);
        }
    }

    user.update_last_active_slot(slot);

    let spot_market = maps.spot_market_map.get_ref(&spot_market_index)?;

    let tvl_after = spot_market.get_tvl()?;

    validate!(
        tvl_before.safe_sub(tvl_after)? <= 10,
        ErrorCode::DefaultError,
        "Transfer Isolated Perp Position Deposit TVL mismatch: before={}, after={}",
        tvl_before,
        tvl_after
    )?;

    Ok(())
}

pub fn withdraw_from_isolated_perp_position<'c: 'info, 'info>(
    user_key: Pubkey,
    user: &mut User,
    _user_stats: &mut UserStats,
    maps: &mut AccountMaps,
    slot: u64,
    now: i64,
    spot_market_index: u16,
    perp_market_index: u16,
    amount: u64,
    funding_paused: bool,
) -> VelocityResult<()> {
    validate!(
        amount != 0,
        ErrorCode::DefaultError,
        "withdraw amount cant be 0",
    )?;

    validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

    {
        let perp_market = &maps.perp_market_map.get_ref(&perp_market_index)?;

        validate!(
            perp_market.quote_spot_market_index == spot_market_index,
            ErrorCode::InvalidIsolatedPerpMarket,
            "perp market quote spot market index ({}) != spot market index ({})",
            perp_market.quote_spot_market_index,
            spot_market_index
        )?;

        let spot_market = &mut maps.spot_market_map.get_ref_mut(&spot_market_index)?;
        let oracle_price_data = maps.oracle_map.get_price_data(&spot_market.oracle_id())?;

        // Accrue interest, but do not advance the market's oracle TWAPs
        // (OtterSec #134, the same shape as #110 and #111).
        //
        // The `MarginRequirementType::Initial` gate later in this flow enables
        // strict pricing. `StrictOraclePrice` bounds are the min and max of the
        // live price and `last_oracle_price_twap_5min`, and a liability is
        // priced at the upper bound. Refreshing that TWAP here drags it toward
        // a temporarily depressed live price and under-values the debt the gate
        // must catch.
        controller::spot_balance::update_spot_market_cumulative_interest(
            spot_market,
            None,
            now,
            funding_paused,
        )?;

        user.increment_total_withdraws(
            amount,
            oracle_price_data.price,
            spot_market.get_precision().cast()?,
        )?;

        // The isolated position holds a mutable borrow of `user`. The block ends
        // it so the checks below can read `user.authority`.
        {
            let isolated_perp_position =
                user.force_get_isolated_perp_position_mut(perp_market_index)?;

            let isolated_position_token_amount =
                isolated_perp_position.get_isolated_token_amount(spot_market)?;

            validate!(
                amount as u128 <= isolated_position_token_amount,
                ErrorCode::InsufficientCollateral,
                "user has insufficient deposit for market {}",
                spot_market_index
            )?;

            update_spot_balances(
                amount as u128,
                &SpotBalanceType::Borrow,
                spot_market,
                isolated_perp_position,
                true,
            )?;
        }

        // This withdrawal sends real tokens out of the spot market vault, so it
        // must respect the withdraw circuit breaker. The cross-margin withdraw
        // path gets the check from
        // `update_spot_balances_and_cumulative_deposits_with_limits`. This path
        // debits balances directly, so the check is explicit here. Without it,
        // one account of any size defeats the breaker for the whole market.
        //
        // The `user` argument is `None`, which asks for a market-level verdict
        // and denies the small-depositor exception here. There are two reasons.
        // The exception reads the account's cross-margin spot position in this
        // market. That balance is not the balance being withdrawn, so a tiny
        // cross deposit would excuse an arbitrarily large isolated withdrawal.
        // `check_withdraw_limits` also reads the cross position with
        // `get_spot_position_index`, which errors when the account has no cross
        // position in the market. An isolated-only depositor would get
        // `CouldNotFindSpotPosition` instead of a limit verdict. An isolated
        // position carries its own collateral, so it is not the small depositor
        // the exception exists for.
        validate!(
            check_withdraw_limits(spot_market, None, None)?,
            ErrorCode::DailyWithdrawLimit,
            "Spot Market {} has hit daily withdraw limit. Attempted isolated position withdraw of {} by {}",
            spot_market_index,
            amount,
            user.authority
        )?;

        // The admin can stop withdrawals per market, by market status or by the
        // `Withdraw` paused-operation bit. The cross-margin withdraw path
        // applies both gates inside
        // `update_spot_balances_and_cumulative_deposits_with_limits`. This path
        // debits balances directly, so both gates are explicit here. A market
        // that is closed for withdrawals must be closed on every route out of
        // the vault.
        //
        // The admitted status set is the same as the cross path. It admits
        // `Settlement`, so a wound-down market stays exitable, and it rejects
        // `Initialized` and `Delisted`. This traps no isolated collateral. An
        // admin can move a market out of `Delisted` again, because
        // `update_spot_market_status` writes any status without restriction. The
        // holder also keeps a second exit at every status.
        // `transfer_isolated_perp_position_deposit` has no per-market status
        // gate, so the isolated balance can always move to the cross-margin
        // position and then face these same rules. The isolated holder therefore
        // never has fewer exits than a cross-margin holder in the same market.
        validate!(
            matches!(
                spot_market.status,
                MarketStatus::Active | MarketStatus::ReduceOnly | MarketStatus::Settlement
            ),
            ErrorCode::MarketWithdrawPaused,
            "Spot Market {} withdraws are currently paused, market not active or in settlement",
            spot_market_index
        )?;

        validate!(
            !spot_market.is_operation_paused(SpotOperation::Withdraw),
            ErrorCode::MarketWithdrawPaused,
            "Spot Market {} withdraws are currently paused",
            spot_market_index
        )?;
    }

    // This path cranks only the market the isolated collateral leaves, and the
    // account's other borrow markets arrive read-only, so their un-booked
    // interest is missing from the check below (OtterSec #135, the same shape
    // as `handle_withdraw`).
    crate::math::margin::validate_spot_borrow_interest_fresh_for_margin(
        user,
        &maps.spot_market_map,
        now,
    )?;

    user.meets_withdraw_margin_requirement(maps, MarginRequirementType::Initial)?;

    if user.is_isolated_margin_being_liquidated(perp_market_index)? {
        user.exit_isolated_margin_liquidation(perp_market_index)?;
    }

    user.update_last_active_slot(slot);

    let mut spot_market = maps.spot_market_map.get_ref_mut(&spot_market_index)?;
    let oracle_price = maps
        .oracle_map
        .get_price_data(&spot_market.oracle_id())?
        .price;

    let deposit_record_id = get_then_update_id!(spot_market, next_deposit_record_id);
    let deposit_record = DepositRecord {
        ts: now,
        deposit_record_id,
        user_authority: user.authority,
        user: user_key,
        direction: DepositDirection::Withdraw,
        oracle_price,
        amount,
        market_index: spot_market_index,
        market_deposit_balance: spot_market.deposit_balance,
        market_withdraw_balance: spot_market.borrow_balance,
        market_cumulative_deposit_interest: spot_market.cumulative_deposit_interest,
        market_cumulative_borrow_interest: spot_market.cumulative_borrow_interest,
        total_deposits_after: user.total_deposits,
        total_withdraws_after: user.total_withdraws,
        explanation: DepositExplanation::None,
        transfer_user: None,
        user_token_amount_after: user.get_total_token_amount(&spot_market)?,
        signer: None,
    };
    emit!(deposit_record);

    Ok(())
}
