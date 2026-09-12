//! Repaying a failing borrow out of the account's deposit.
//!
//! The liquidator repays part of the borrow and takes collateral for it at a
//! discount. The insurance fund and the protocol take a cut of the repayment,
//! bounded by the shortage so the cut can never deepen it.

use super::*;

/// The account after the order cancels, when it is still liquidatable.
struct SpotCancelRecheck {
    margin_calculation: MarginCalculation,
    margin_freed: u64,
}

/// A spot liquidation that passed its entry checks and holds the latch.
struct OpenedSpotLiquidation<'a> {
    asset: SpotLiquidationSide,
    liability: SpotLiquidationSide,
    record: SpotLiquidationRecordKeys<'a>,
    /// The picture the liquidation opened on, which the record reports.
    margin_calculation: MarginCalculation,
    /// The picture after the order cancels, which the transfer is sized on.
    intermediate_margin_calculation: MarginCalculation,
    margin_freed: u64,
}

/// How much of the borrow this call repays, and what the account pays for it.
struct SpotTransfer {
    liability_transfer: u128,
    asset_transfer: u128,
    /// The insurance fund's cut of the repayment.
    if_fee: u128,
    /// The protocol's cut of the repayment.
    protocol_fee: u128,
    /// The repayment that would clear the whole shortage.
    liability_transfer_to_cover_margin_shortage: u128,
}

pub fn liquidate_spot(
    request: LiquidateSpotRequest,
    parties: &mut LiquidationParties,
    maps: &mut AccountMaps,
    terms: &LiquidationTerms,
    state: &State,
) -> VelocityResult {
    let Some(opened) = open_spot_liquidation(request, parties, maps, terms, state)? else {
        return Ok(());
    };

    complete_spot_liquidation(request, parties, maps, terms, opened, state)
}

/// Run the entry checks, latch the account, cancel its orders and re-measure
/// it.
///
/// `None` reports that the liquidation is already finished: the cancels alone
/// cleared the shortage, or the account never needed one.
fn open_spot_liquidation<'a>(
    request: LiquidateSpotRequest,
    parties: &mut LiquidationParties<'a>,
    maps: &mut AccountMaps,
    terms: &LiquidationTerms,
    state: &State,
) -> VelocityResult<Option<OpenedSpotLiquidation<'a>>> {
    let funding_paused = state.funding_paused()?;

    validate_preconditions(request, parties, maps)?;

    let asset = read_asset_side(
        parties.user,
        maps,
        request.asset_market_index,
        terms.now,
        terms.slot,
        funding_paused,
    )?;
    let liability = read_liability_side(
        parties.user,
        maps,
        request.liability_market_index,
        terms.now,
        terms.slot,
        funding_paused,
    )?;
    validate_lst_oracle_delays(&asset, &liability)?;

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        parties.user,
        maps,
        terms.margin_context_tracking(MarketIdentifier::spot(request.liability_market_index))?,
    )?;

    if check_cross_margin_entry(parties.user, &margin_calculation)? == LiquidationEntry::Exited {
        return Ok(None);
    }

    let record = SpotLiquidationRecordKeys {
        user_key: parties.user_key,
        liquidator_key: parties.liquidator_key,
        request,
        liquidation_id: parties.user.enter_cross_margin_liquidation(terms.slot)?,
        asset_price: asset.price,
        liability_price: liability.price,
        now: terms.now,
    };

    let canceled_order_ids = orders::cancel_orders(
        parties.user,
        parties.user_key,
        Some(parties.liquidator_key),
        maps,
        terms.now,
        terms.slot,
        OrderActionExplanation::Liquidation,
        None,
        None,
        None,
        true,
    )?;

    let Some(recheck) = recheck_spot_after_cancels(
        parties.user,
        maps,
        terms,
        &margin_calculation,
        canceled_order_ids,
        &record,
    )?
    else {
        return Ok(None);
    };

    Ok(Some(OpenedSpotLiquidation {
        asset,
        liability,
        record,
        margin_calculation,
        intermediate_margin_calculation: recheck.margin_calculation,
        margin_freed: recheck.margin_freed,
    }))
}

/// Move the collateral and the borrow, then close or re-latch the account.
fn complete_spot_liquidation(
    request: LiquidateSpotRequest,
    parties: &mut LiquidationParties,
    maps: &mut AccountMaps,
    terms: &LiquidationTerms,
    opened: OpenedSpotLiquidation,
    state: &State,
) -> VelocityResult {
    let margin_shortage = opened
        .intermediate_margin_calculation
        .cross_margin_margin_shortage()?;

    let Some(transfer) = size_spot_transfer(
        request,
        parties.user,
        maps,
        &opened.intermediate_margin_calculation,
        (&opened.asset, &opened.liability),
        terms,
    )?
    else {
        return Ok(());
    };

    validate_bands_and_limit_price(request, &opened.asset, &opened.liability, &transfer, state)?;
    apply_spot_transfer(request, parties, maps, &transfer)?;

    let (margin_freed_from_liability, _) = calculate_margin_freed(
        parties.user,
        maps,
        terms.margin_buffer_ratio,
        margin_shortage,
        None,
    )?;
    let margin_freed = opened.margin_freed.safe_add(margin_freed_from_liability)?;
    parties
        .user
        .increment_margin_freed(margin_freed_from_liability)?;

    if transfer.liability_transfer >= transfer.liability_transfer_to_cover_margin_shortage {
        parties.user.exit_cross_margin_liquidation();
    } else if is_cross_margin_bankrupt(parties.user, &maps.spot_market_map)? {
        parties.user.enter_cross_margin_bankruptcy();
    }

    validate_liquidator_takes_on_risk(
        parties.liquidator,
        maps,
        "Liquidator doesnt have enough collateral to take over borrow",
    )?;

    emit_liquidate_spot_record(
        parties.user,
        &opened.record,
        &opened.margin_calculation,
        margin_freed,
        LiquidateSpotRecord {
            asset_market_index: request.asset_market_index,
            asset_price: opened.record.asset_price,
            asset_transfer: transfer.asset_transfer,
            liability_market_index: request.liability_market_index,
            liability_price: opened.record.liability_price,
            liability_transfer: transfer.liability_transfer,
            if_fee: transfer.if_fee.cast()?,
            protocol_fee: transfer.protocol_fee.cast()?,
        },
    )
}

/// What every `LiquidateSpot` record repeats.
#[derive(Clone, Copy)]
struct SpotLiquidationRecordKeys<'a> {
    user_key: &'a Pubkey,
    liquidator_key: &'a Pubkey,
    request: LiquidateSpotRequest,
    liquidation_id: u16,
    asset_price: i64,
    liability_price: i64,
    now: i64,
}

/// Refuse a liquidation neither account nor market may take part in.
fn validate_preconditions(
    request: LiquidateSpotRequest,
    parties: &mut LiquidationParties,
    maps: &AccountMaps,
) -> VelocityResult {
    validate!(
        !parties.user.is_cross_margin_bankrupt(),
        ErrorCode::UserBankrupt,
        "user bankrupt",
    )?;

    validate!(
        !parties.liquidator.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "liquidator bankrupt",
    )?;

    validate_market_open_to_liquidator(
        maps,
        request.asset_market_index,
        parties.liquidator.pool_id,
        "asset",
    )?;
    validate_market_open_to_liquidator(
        maps,
        request.liability_market_index,
        parties.liquidator.pool_id,
        "liablity",
    )?;

    validate_both_sides_have_balances(request, parties)
}

/// Refuse a spot market that is paused, or that the liquidator's pool cannot
/// reach.
fn validate_market_open_to_liquidator(
    maps: &AccountMaps,
    market_index: u16,
    liquidator_pool_id: u8,
    side: &str,
) -> VelocityResult {
    let spot_market = maps.spot_market_map.get_ref(&market_index)?;

    validate!(
        !spot_market.is_operation_paused(SpotOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        market_index
    )?;

    validate!(
        liquidator_pool_id == spot_market.pool_id,
        ErrorCode::InvalidPoolId,
        "liquidator pool id ({}) != {} spot market pool id ({})",
        liquidator_pool_id,
        side,
        spot_market.pool_id
    )
}

/// Refuse a transfer either side has nowhere to book.
fn validate_both_sides_have_balances(
    request: LiquidateSpotRequest,
    parties: &mut LiquidationParties,
) -> VelocityResult {
    parties
        .user
        .get_spot_position(request.asset_market_index)
        .map_err(|_| {
            msg!(
                "User does not have a spot balance for asset market {}",
                request.asset_market_index
            );
            ErrorCode::CouldNotFindSpotPosition
        })?;

    parties
        .user
        .get_spot_position(request.liability_market_index)
        .map_err(|_| {
            msg!(
                "User does not have a spot balance for liability market {}",
                request.liability_market_index
            );
            ErrorCode::CouldNotFindSpotPosition
        })?;

    parties
        .liquidator
        .force_get_spot_position_mut(request.asset_market_index)
        .inspect_err(|_e| {
            msg!("Liquidator has no available spot balances to take on deposit");
        })?;

    parties
        .liquidator
        .force_get_spot_position_mut(request.liability_market_index)
        .inspect_err(|_e| {
            msg!("Liquidator has no available spot balances to take on borrow");
        })?;

    Ok(())
}

/// Read the deposit the liquidator takes collateral from.
fn read_asset_side(
    user: &User,
    maps: &mut AccountMaps,
    market_index: u16,
    now: i64,
    slot: u64,
    funding_paused: bool,
) -> VelocityResult<SpotLiquidationSide> {
    let slot_clock = maps.oracle_map.slot_clock;
    let mut asset_market = maps.spot_market_map.get_ref_mut(&market_index)?;
    let (asset_price_data, validity_guard_rails) = maps
        .oracle_map
        .get_price_data_and_guard_rails(&asset_market.oracle_id())?;

    let asset_refresh = update_spot_market_and_check_validity(
        &mut asset_market,
        asset_price_data,
        validity_guard_rails,
        now,
        Some(VelocityAction::Liquidate),
        funding_paused,
        slot,
        slot_clock,
    )?;

    let spot_deposit_position = user.get_spot_position(market_index)?;

    validate!(
        spot_deposit_position.balance_type == SpotBalanceType::Deposit,
        ErrorCode::WrongSpotBalanceType,
        "User did not have a deposit for the asset market index"
    )?;

    let token_amount = spot_deposit_position.get_token_amount(&asset_market)?;

    validate!(
        token_amount != 0,
        ErrorCode::InvalidSpotPosition,
        "asset token amount zero for market index = {}",
        market_index
    )?;

    // a margin-invalid (stale/uncertain) deposit oracle may make the account
    // liquidatable, but must not let its collateral be seized at a depressed
    // price: size the transfer at a user-protective price instead
    let asset_price =
        if is_oracle_valid_for_action(asset_refresh.validity, Some(VelocityAction::MarginCalc))? {
            asset_price_data.price
        } else {
            calculate_user_protective_asset_price(
                asset_price_data,
                asset_refresh.pre_refresh_twap_5min,
            )?
        };

    Ok(SpotLiquidationSide {
        amount: token_amount,
        oracle_price: asset_price_data.price,
        price: asset_price,
        pre_refresh_twap_5min: asset_refresh.pre_refresh_twap_5min,
        decimals: asset_market.decimals,
        weight: asset_market.maintenance_asset_weight,
        liquidation_multiplier: calculate_liquidation_multiplier(
            asset_market.liquidator_fee,
            LiquidationMultiplierType::Premium,
        )?,
        pool_id: asset_market.pool_id,
        oracle_delay: asset_price_data.delay,
    })
}

/// Read the borrow the liquidator repays.
fn read_liability_side(
    user: &User,
    maps: &mut AccountMaps,
    market_index: u16,
    now: i64,
    slot: u64,
    funding_paused: bool,
) -> VelocityResult<SpotLiquidationSide> {
    let slot_clock = maps.oracle_map.slot_clock;
    let mut liability_market = maps.spot_market_map.get_ref_mut(&market_index)?;
    let (liability_price_data, validity_guard_rails) = maps
        .oracle_map
        .get_price_data_and_guard_rails(&liability_market.oracle_id())?;

    let liability_refresh = update_spot_market_and_check_validity(
        &mut liability_market,
        liability_price_data,
        validity_guard_rails,
        now,
        Some(VelocityAction::Liquidate),
        funding_paused,
        slot,
        slot_clock,
    )?;

    let spot_position = user.get_spot_position(market_index)?;

    validate!(
        spot_position.balance_type == SpotBalanceType::Borrow,
        ErrorCode::WrongSpotBalanceType,
        "User did not have a borrow for the liability market index"
    )?;

    let token_amount = spot_position.get_token_amount(&liability_market)?;

    validate!(
        token_amount != 0,
        ErrorCode::InvalidSpotPosition,
        "liability token amount zero for market index = {}",
        market_index
    )?;

    // the liability side of the exchange rate gets the mirrored protection: a
    // margin-invalid (stale/uncertain) borrow oracle must not overvalue the debt
    // being repaid and cheapen the collateral received for it
    let liability_price = if is_oracle_valid_for_action(
        liability_refresh.validity,
        Some(VelocityAction::MarginCalc),
    )? {
        liability_price_data.price
    } else {
        calculate_user_protective_liability_price(
            liability_price_data,
            liability_refresh.pre_refresh_twap_5min,
        )?
    };

    Ok(SpotLiquidationSide {
        amount: token_amount,
        oracle_price: liability_price_data.price,
        price: liability_price,
        pre_refresh_twap_5min: liability_refresh.pre_refresh_twap_5min,
        decimals: liability_market.decimals,
        weight: liability_market.maintenance_liability_weight,
        liquidation_multiplier: calculate_liquidation_multiplier(
            liability_market.liquidator_fee,
            LiquidationMultiplierType::Discount,
        )?,
        pool_id: liability_market.pool_id,
        oracle_delay: liability_price_data.delay,
    })
}

/// Re-measure the account after its orders are canceled.
fn recheck_spot_after_cancels(
    user: &mut User,
    maps: &mut AccountMaps,
    terms: &LiquidationTerms,
    margin_calculation: &MarginCalculation,
    canceled_order_ids: Vec<u32>,
    record: &SpotLiquidationRecordKeys,
) -> VelocityResult<Option<SpotCancelRecheck>> {
    if canceled_order_ids.is_empty() {
        return Ok(Some(SpotCancelRecheck {
            margin_calculation: margin_calculation.clone(),
            margin_freed: 0,
        }));
    }

    let (margin_freed, intermediate_margin_calculation) = recheck_cross_margin_after_cancels(
        user,
        maps,
        terms.margin_context_tracking(MarketIdentifier::spot(
            record.request.liability_market_index,
        ))?,
        margin_calculation.cross_margin_margin_shortage()?,
    )?;

    if intermediate_margin_calculation.can_exit_cross_margin_liquidation()? {
        emit_early_exit_record(
            user,
            record,
            margin_calculation,
            margin_freed,
            canceled_order_ids,
        )?;

        user.exit_cross_margin_liquidation();
        return Ok(None);
    }

    Ok(Some(SpotCancelRecheck {
        margin_calculation: intermediate_margin_calculation,
        margin_freed,
    }))
}

/// Emit the record for a liquidation the order cancels alone resolved.
fn emit_early_exit_record(
    user: &User,
    record: &SpotLiquidationRecordKeys,
    margin_calculation: &MarginCalculation,
    margin_freed: u64,
    canceled_order_ids: Vec<u32>,
) -> VelocityResult {
    emit_liquidate_spot_record_with_cancels(
        user,
        record,
        margin_calculation,
        (margin_freed, canceled_order_ids),
        LiquidateSpotRecord {
            asset_market_index: record.request.asset_market_index,
            asset_price: record.asset_price,
            asset_transfer: 0,
            liability_market_index: record.request.liability_market_index,
            liability_price: record.liability_price,
            liability_transfer: 0,
            if_fee: 0,
            protocol_fee: 0,
        },
    )
}

/// Size the repayment and the collateral that pays for it.
///
/// `None` reports that the time ramp allows nothing yet, which is not an
/// error: a later call moves what this one may not.
fn size_spot_transfer(
    request: LiquidateSpotRequest,
    user: &User,
    maps: &AccountMaps,
    margin_calculation: &MarginCalculation,
    sides: (&SpotLiquidationSide, &SpotLiquidationSide),
    terms: &LiquidationTerms,
) -> VelocityResult<Option<SpotTransfer>> {
    let (asset, liability) = sides;
    let margin_shortage = margin_calculation.cross_margin_margin_shortage()?;
    let liability_weight_with_buffer = liability.weight.safe_add(terms.margin_buffer_ratio)?;

    let (liquidation_if_fee, liquidation_protocol_fee, total_if_side_fee) = split_spot_if_side_fee(
        maps,
        request.liability_market_index,
        margin_calculation,
        (asset, liability),
        liability_weight_with_buffer,
    )?;

    // Determine what amount of borrow to transfer to reduce margin shortage to 0
    let liability_transfer_to_cover_margin_shortage =
        calculate_liability_transfer_to_cover_margin_shortage(
            margin_shortage,
            asset.weight,
            asset.liquidation_multiplier,
            liability_weight_with_buffer,
            liability.liquidation_multiplier,
            liability.decimals,
            liability.oracle_price,
            total_if_side_fee,
        )?;

    let max_pct_allowed = calculate_max_pct_to_liquidate(
        user,
        margin_shortage,
        terms.slot,
        terms.initial_pct_to_liquidate,
        terms.duration,
        maps.oracle_map.slot_clock,
    )?;
    let max_liability_allowed_to_be_transferred = liability_transfer_to_cover_margin_shortage
        .saturating_mul(max_pct_allowed)
        .safe_div(LIQUIDATION_PCT_PRECISION)?;

    if max_liability_allowed_to_be_transferred == 0 {
        msg!("max_liability_allowed_to_be_transferred == 0");
        return Ok(None);
    }

    let (liability_transfer, asset_transfer, liability_transfer_implied_by_asset_amount) =
        choose_transfer_sizes(
            request,
            (asset, liability),
            max_liability_allowed_to_be_transferred,
        )?;

    if asset_transfer == 0 || liability_transfer == 0 {
        log_empty_transfer(
            request,
            liability,
            liability_transfer_to_cover_margin_shortage,
            liability_transfer_implied_by_asset_amount,
            (liability_transfer, asset_transfer),
        );
        return Err(ErrorCode::InvalidLiquidation);
    }

    Ok(Some(SpotTransfer {
        liability_transfer,
        asset_transfer,
        if_fee: fee_on_transfer(liability_transfer, liquidation_if_fee)?,
        protocol_fee: fee_on_transfer(liability_transfer, liquidation_protocol_fee)?,
        liability_transfer_to_cover_margin_shortage,
    }))
}

/// The repayment this call makes, the collateral that pays for it, and the
/// repayment the whole deposit could buy.
///
/// The repayment is the smallest of what the liquidator offers, what the
/// account owes, what the time ramp allows and what the deposit can pay for.
/// A borrow too small to leave a useful remainder is repaid whole instead.
fn choose_transfer_sizes(
    request: LiquidateSpotRequest,
    sides: (&SpotLiquidationSide, &SpotLiquidationSide),
    max_liability_allowed_to_be_transferred: u128,
) -> VelocityResult<(u128, u128, u128)> {
    let (asset, liability) = sides;

    // Given the user's deposit amount, how much borrow can be transferred?
    let liability_transfer_implied_by_asset_amount =
        calculate_liability_transfer_implied_by_asset_amount(
            asset.amount,
            asset.liquidation_multiplier,
            asset.decimals,
            asset.price,
            liability.liquidation_multiplier,
            liability.decimals,
            liability.price,
        )?;

    let liability_transfer = request
        .liquidator_max_liability_transfer
        .min(liability.amount)
        // want to make sure the liability_transfer_to_cover_margin_shortage doesn't lead to dust positions
        .min(max_liability_allowed_to_be_transferred.max(minimum_liability_transfer(liability)?))
        .min(liability_transfer_implied_by_asset_amount);

    // Given the borrow amount to transfer, determine how much deposit amount to transfer
    let asset_transfer = calculate_asset_transfer_for_liability_transfer(
        asset.amount,
        asset.liquidation_multiplier,
        asset.decimals,
        asset.price,
        liability_transfer,
        liability.liquidation_multiplier,
        liability.decimals,
        liability.price,
    )?;

    Ok((
        liability_transfer,
        asset_transfer,
        liability_transfer_implied_by_asset_amount,
    ))
}

/// The insurance-side fee budget, split between the fund and the protocol.
///
/// The budget is computed once with the cap raised to the sum of the two
/// rates, then split IF-first: the insurance fund receives exactly what it
/// would have without the protocol fee, and the protocol only captures the
/// margin headroom beyond that.
fn split_spot_if_side_fee(
    maps: &AccountMaps,
    liability_market_index: u16,
    margin_calculation: &MarginCalculation,
    sides: (&SpotLiquidationSide, &SpotLiquidationSide),
    liability_weight_with_buffer: u32,
) -> VelocityResult<(u32, u32, u32)> {
    let (asset, liability) = sides;
    let (liability_if_liquidation_fee, liability_protocol_liquidation_fee) = {
        let liability_market = maps.spot_market_map.get_ref(&liability_market_index)?;
        (
            liability_market.if_liquidation_fee,
            liability_market.protocol_liquidation_fee,
        )
    };

    let margin_shortage = margin_calculation.cross_margin_margin_shortage()?;
    let total_if_side_fee = calculate_spot_if_fee(
        margin_calculation.tracked_market_margin_shortage(margin_shortage)?,
        liability.amount,
        asset.weight,
        asset.liquidation_multiplier,
        liability_weight_with_buffer,
        liability.liquidation_multiplier,
        liability.decimals,
        // valuation (shortage -> tokens/fees) stays at the raw oracle price, consistent
        // with the margin calculation; only the exchange rate uses the protective price
        liability.oracle_price,
        liability_if_liquidation_fee.safe_add(liability_protocol_liquidation_fee)?,
    )?;

    let liquidation_if_fee = total_if_side_fee.min(liability_if_liquidation_fee);
    let liquidation_protocol_fee = total_if_side_fee.safe_sub(liquidation_if_fee)?;

    Ok((
        liquidation_if_fee,
        liquidation_protocol_fee,
        total_if_side_fee,
    ))
}

/// Report the inputs that produced a transfer of nothing.
fn log_empty_transfer(
    request: LiquidateSpotRequest,
    liability: &SpotLiquidationSide,
    liability_transfer_to_cover_margin_shortage: u128,
    liability_transfer_implied_by_asset_amount: u128,
    transfers: (u128, u128),
) {
    let (liability_transfer, asset_transfer) = transfers;
    msg!(
        "asset_market_index {} liability_market_index {}",
        request.asset_market_index,
        request.liability_market_index
    );
    msg!(
        "liquidator_max_liability_transfer {} liability_amount {} liability_transfer_to_cover_margin_shortage {}",
        request.liquidator_max_liability_transfer,
        liability.amount,
        liability_transfer_to_cover_margin_shortage
    );
    msg!(
        "liability_transfer_implied_by_asset_amount {} liability_transfer {} asset_transfer {}",
        liability_transfer_implied_by_asset_amount,
        liability_transfer,
        asset_transfer
    );
}

/// Refuse a transfer priced off a divergent oracle, or worse than the
/// liquidator's limit price.
fn validate_bands_and_limit_price(
    request: LiquidateSpotRequest,
    asset: &SpotLiquidationSide,
    liability: &SpotLiquidationSide,
    transfer: &SpotTransfer,
    state: &State,
) -> VelocityResult {
    // Both bands measure the live oracle against the TWAP as it stood on entry. This
    // instruction already refreshed both TWAPs above, which pulls each one toward the very
    // oracle price the band measures. Reading the fields back lets a divergent oracle widen
    // its own band and pass trivially (OtterSec #109-#112, #134).
    validate_side_within_twap_band(liability, state, "liability")?;
    validate_side_within_twap_band(asset, state, "asset")?;

    validate_transfer_satisfies_limit_price(
        transfer.asset_transfer,
        transfer.liability_transfer,
        asset.decimals,
        liability.decimals,
        request.limit_price,
    )
}

/// Refuse a price that has run too far from the five minute TWAP it entered
/// with.
pub(crate) fn validate_side_within_twap_band(
    side: &SpotLiquidationSide,
    state: &State,
    name: &str,
) -> VelocityResult {
    let too_divergent = is_oracle_too_divergent_with_twap_5min(
        side.oracle_price.cast()?,
        side.pre_refresh_twap_5min,
        state
            .oracle_guard_rails
            .max_oracle_twap_5min_percent_divergence()
            .cast()?,
    )?;

    validate!(
        !too_divergent,
        ErrorCode::PriceBandsBreached,
        "{} oracle too divergent",
        name
    )
}

/// Move the borrow to the liquidator and the collateral to the account that
/// took it on.
fn apply_spot_transfer(
    request: LiquidateSpotRequest,
    parties: &mut LiquidationParties,
    maps: &AccountMaps,
    transfer: &SpotTransfer,
) -> VelocityResult {
    {
        let mut liability_market = maps
            .spot_market_map
            .get_ref_mut(&request.liability_market_index)?;

        let user_liability_reduction = transfer
            .liability_transfer
            .safe_sub(transfer.if_fee)?
            .safe_sub(transfer.protocol_fee)?;
        update_spot_balances_and_cumulative_deposits(
            user_liability_reduction,
            &SpotBalanceType::Deposit,
            &mut liability_market,
            parties
                .user
                .get_spot_position_mut(request.liability_market_index)?,
            false,
            Some(user_liability_reduction),
        )?;

        update_revenue_pool_balances(
            transfer.if_fee,
            &SpotBalanceType::Deposit,
            &mut liability_market,
            false,
        )?;
        update_protocol_fee_pool_balances(
            transfer.protocol_fee,
            &SpotBalanceType::Deposit,
            &mut liability_market,
            false,
        )?;

        update_spot_balances_and_cumulative_deposits(
            transfer.liability_transfer,
            &SpotBalanceType::Borrow,
            &mut liability_market,
            parties
                .liquidator
                .get_spot_position_mut(request.liability_market_index)?,
            false,
            Some(transfer.liability_transfer),
        )?;
    }

    let mut asset_market = maps
        .spot_market_map
        .get_ref_mut(&request.asset_market_index)?;

    update_spot_balances_and_cumulative_deposits(
        transfer.asset_transfer,
        &SpotBalanceType::Deposit,
        &mut asset_market,
        parties
            .liquidator
            .force_get_spot_position_mut(request.asset_market_index)?,
        false,
        Some(transfer.asset_transfer),
    )?;

    update_spot_balances_and_cumulative_deposits(
        transfer.asset_transfer,
        &SpotBalanceType::Borrow,
        &mut asset_market,
        parties
            .user
            .force_get_spot_position_mut(request.asset_market_index)?,
        false,
        Some(transfer.asset_transfer),
    )
}

/// Emit one `LiquidateSpot` record, reporting no canceled orders.
///
/// The completed liquidation reports the cancels it made through the record
/// the cancels themselves emit, so this one leaves the list empty.
fn emit_liquidate_spot_record(
    user: &User,
    record: &SpotLiquidationRecordKeys,
    margin_calculation: &MarginCalculation,
    margin_freed: u64,
    liquidate_spot: LiquidateSpotRecord,
) -> VelocityResult {
    emit_liquidate_spot_record_with_cancels(
        user,
        record,
        margin_calculation,
        (margin_freed, vec![]),
        liquidate_spot,
    )
}

/// Emit one `LiquidateSpot` record.
fn emit_liquidate_spot_record_with_cancels(
    user: &User,
    record: &SpotLiquidationRecordKeys,
    margin_calculation: &MarginCalculation,
    freed: (u64, Vec<u32>),
    liquidate_spot: LiquidateSpotRecord,
) -> VelocityResult {
    let (margin_freed, canceled_order_ids) = freed;

    emit!(LiquidationRecord {
        ts: record.now,
        liquidation_id: record.liquidation_id,
        liquidation_type: LiquidationType::LiquidateSpot,
        user: *record.user_key,
        liquidator: *record.liquidator_key,
        margin_requirement: margin_calculation.margin_requirement,
        total_collateral: margin_calculation.total_collateral,
        bankrupt: user.is_cross_margin_bankrupt(),
        canceled_order_ids,
        margin_freed,
        liquidate_spot,
        ..LiquidationRecord::default()
    });

    Ok(())
}
