//! Handing a failing perp position to a liquidator.
//!
//! The liquidator takes the position at the oracle price less its execution
//! discount, and the insurance fund and the protocol take a cut on top. Both
//! sides book the move as a fill, so the trade appears in the order records
//! the same way any other fill does.

use super::*;

/// The size one transfer moves, and the fees it settles at.
struct PerpTransfer {
    base_asset_amount: u64,
    base_asset_value: u64,
    /// The liquidator's cut, as a debit to the account.
    liquidator_fee: i64,
    /// The insurance fund's cut, as a debit to the account.
    if_fee: i64,
    /// The protocol's cut, as a debit to the account.
    protocol_fee: i64,
    /// The size that would clear the whole shortage.
    base_asset_amount_to_cover_margin_shortage: u64,
}

/// What the position move left for the fill record.
struct PerpTransferPositions {
    user_existing_direction: PositionDirection,
    user_direction_to_close: PositionDirection,
    user_existing_params: Option<(u64, u64)>,
    liquidator_existing_direction: PositionDirection,
    liquidator_existing_params: Option<(u64, u64)>,
    user_position_delta: PositionDelta,
}

/// What the account's side of the transfer left for the fill record.
struct UserSideOfTransfer {
    existing_direction: PositionDirection,
    direction_to_close: PositionDirection,
    existing_params: Option<(u64, u64)>,
}

/// The ids the two sides of one liquidation fill are recorded under.
#[derive(Clone, Copy)]
struct PerpFillIds {
    user_order_id: u32,
    liquidator_order_id: u32,
    fill_record_id: u64,
}

pub fn liquidate_perp(
    request: LiquidatePerpRequest,
    parties: &mut PerpLiquidationParties,
    maps: &mut AccountMaps,
    terms: &LiquidationTerms,
    state: &State,
) -> VelocityResult {
    let Some(opened) = open_perp_liquidation(request, parties, maps, terms, state)? else {
        return Ok(());
    };

    let position_index = get_position_index(&parties.user.perp_positions, request.market_index)?;
    if parties.user.perp_positions[position_index].base_asset_amount == 0 {
        msg!("User has no base asset amount");
        return Ok(());
    }

    let margin_shortage = opened
        .mode
        .margin_shortage(&opened.intermediate_margin_calculation)?;

    let Some(transfer) =
        size_perp_transfer(request, parties.user, maps, &opened, margin_shortage, state)?
    else {
        return Ok(());
    };

    parties
        .user_stats
        .update_taker_volume_30d(transfer.base_asset_value, terms.now)?;
    parties
        .liquidator_stats
        .update_maker_volume_30d(transfer.base_asset_value, terms.now)?;

    let positions = apply_perp_transfer(parties, maps, request.market_index, &transfer)?;

    let (margin_freed_for_perp_position, _) = calculate_margin_freed(
        parties.user,
        maps,
        terms.margin_buffer_ratio,
        margin_shortage,
        Some(opened.mode.as_ref()),
    )?;
    let margin_freed = opened
        .margin_freed
        .safe_add(margin_freed_for_perp_position)?;
    opened
        .mode
        .increment_free_margin(parties.user, margin_freed_for_perp_position)?;

    close_or_latch_perp_account(parties.user, maps, &opened, &transfer)?;

    validate_liquidator_takes_on_risk(
        parties.liquidator,
        maps,
        "Liquidator doesnt have enough collateral to take over perp position",
    )?;

    let ids = next_fill_ids(parties, maps, request.market_index)?;
    emit_perp_fill_records(parties, &opened.run, &transfer, &positions, request, ids)?;

    let outcome = PerpLiquidationOutcome {
        canceled_order_ids: opened.canceled_order_ids,
        margin_freed,
        liquidate_perp: LiquidatePerpRecord {
            market_index: request.market_index,
            oracle_price: opened.run.oracle_price,
            base_asset_amount: positions.user_position_delta.base_asset_amount,
            quote_asset_amount: positions.user_position_delta.quote_asset_amount,
            user_order_id: ids.user_order_id,
            liquidator_order_id: ids.liquidator_order_id,
            fill_record_id: ids.fill_record_id,
            liquidator_fee: transfer.liquidator_fee.abs().cast()?,
            if_fee: transfer.if_fee.abs().cast()?,
            protocol_fee: transfer.protocol_fee.abs().cast()?,
        },
    };
    emit_liquidate_perp_record(
        parties.user,
        opened.mode.as_ref(),
        &opened.run,
        &opened.margin_calculation,
        outcome,
    )
}

/// Run the entry checks, latch the account, cancel its orders and re-measure
/// it.
///
/// `None` reports that the liquidation is already finished: either the account
/// never needed one, or the cancels alone cleared its shortage.
fn open_perp_liquidation<'a>(
    request: LiquidatePerpRequest,
    parties: &mut PerpLiquidationParties<'a>,
    maps: &mut AccountMaps,
    terms: &LiquidationTerms,
    state: &State,
) -> VelocityResult<Option<OpenedPerpLiquidation<'a>>> {
    let market_index = request.market_index;
    let target = PerpLiquidationTarget {
        user_key: parties.user_key,
        liquidator_key: parties.liquidator_key,
        market_index,
    };
    let liquidation_mode = get_perp_liquidation_mode(parties.user, market_index)?;

    validate_preconditions(
        parties,
        maps,
        liquidation_mode.as_ref(),
        market_index,
        terms.now,
    )?;
    settle_both_sides_funding(parties, maps, market_index, terms.now)?;

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        parties.user,
        maps,
        terms.margin_context_tracking(MarketIdentifier::perp(market_index))?,
    )?;

    if check_perp_liquidation_entry(parties.user, liquidation_mode.as_ref(), &margin_calculation)?
        == LiquidationEntry::Exited
    {
        return Ok(None);
    }

    validate_both_sides_have_positions(parties, market_index)?;

    let Some((run, recheck)) = latch_cancel_and_recheck(
        parties.user,
        target,
        liquidation_mode.as_ref(),
        maps,
        &margin_calculation,
        (terms, state),
    )?
    else {
        return Ok(None);
    };

    Ok(Some(OpenedPerpLiquidation {
        mode: liquidation_mode,
        run,
        margin_calculation,
        intermediate_margin_calculation: recheck.margin_calculation,
        canceled_order_ids: recheck.canceled_order_ids,
        margin_freed: recheck.margin_freed,
    }))
}

/// Refuse a liquidation neither account nor market may take part in.
fn validate_preconditions(
    parties: &PerpLiquidationParties,
    maps: &AccountMaps,
    liquidation_mode: &dyn LiquidatePerpMode,
    market_index: u16,
    now: i64,
) -> VelocityResult {
    validate!(
        !liquidation_mode.is_user_bankrupt(parties.user)?,
        ErrorCode::UserBankrupt,
        "user bankrupt",
    )?;

    validate!(
        !parties.liquidator.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "liquidator bankrupt",
    )?;

    validate!(
        parties.liquidator.pool_id == 0,
        ErrorCode::InvalidPoolId,
        "liquidator pool id ({}) != 0",
        parties.liquidator.pool_id
    )?;

    validate_perp_market_liquidatable(&*maps.perp_market_map.get_ref(&market_index)?, now)
}

/// Bring both accounts current on funding before the position moves.
fn settle_both_sides_funding(
    parties: &mut PerpLiquidationParties,
    maps: &AccountMaps,
    market_index: u16,
    now: i64,
) -> VelocityResult {
    settle_funding_payment(
        parties.user,
        parties.user_key,
        maps.perp_market_map.get_ref_mut(&market_index)?.deref_mut(),
        now,
    )?;

    settle_funding_payment(
        parties.liquidator,
        parties.liquidator_key,
        maps.perp_market_map.get_ref_mut(&market_index)?.deref_mut(),
        now,
    )
}

/// Refuse a transfer neither side has room for.
fn validate_both_sides_have_positions(
    parties: &mut PerpLiquidationParties,
    market_index: u16,
) -> VelocityResult {
    parties
        .user
        .get_perp_position(market_index)
        .inspect_err(|_e| {
            msg!(
                "User does not have a position for perp market {}",
                market_index
            );
        })?;

    parties
        .liquidator
        .force_get_perp_position_mut(market_index)
        .inspect_err(|_e| {
            msg!(
                "Liquidator has no available positions to take on perp position in market {}",
                market_index
            );
        })?;

    Ok(())
}

/// Size the transfer and price its fees.
///
/// `None` reports that the time ramp allows nothing yet, which is not an
/// error: a later call moves what this one may not.
fn size_perp_transfer(
    request: LiquidatePerpRequest,
    user: &User,
    maps: &mut AccountMaps,
    opened: &OpenedPerpLiquidation,
    margin_shortage: u128,
    state: &State,
) -> VelocityResult<Option<PerpTransfer>> {
    let market_index = request.market_index;
    let oracle_price = opened.run.oracle_price;
    let step_size = maps.perp_market_map.get_ref(&market_index)?.order_step_size;

    let liquidator_max_base_asset_amount =
        standardize_base_asset_amount(request.liquidator_max_base_asset_amount, step_size)?;

    validate!(
        liquidator_max_base_asset_amount != 0,
        ErrorCode::InvalidBaseAssetAmountForLiquidatePerp,
        "liquidator_max_base_asset_amount must be greater or equal to the step size",
    )?;

    // A market in settlement prices at its committed expiry price, which the
    // live TWAP band does not describe.
    if maps.perp_market_map.get_ref(&market_index)?.status != MarketStatus::Settlement {
        validate_oracle_within_twap_band(maps, market_index, oracle_price, state)?;
    }

    let sizing = PerpLiquidationSizing::calculate(
        user,
        maps,
        &opened.run,
        &opened.intermediate_margin_calculation,
        margin_shortage,
    )?;

    let mut base_asset_amount_to_cover_margin_shortage =
        sizing.base_asset_amount_to_cover_margin_shortage;
    if base_asset_amount_to_cover_margin_shortage != u64::MAX {
        base_asset_amount_to_cover_margin_shortage = standardize_base_asset_amount_ceil(
            base_asset_amount_to_cover_margin_shortage,
            step_size,
        )?;
    }

    let max_allowed = max_base_asset_amount_allowed_to_be_transferred(
        user,
        opened.mode.as_ref(),
        maps,
        &opened.run.terms,
        margin_shortage,
        base_asset_amount_to_cover_margin_shortage,
    )?;

    if max_allowed == 0 {
        msg!("max_base_asset_amount_allowed_to_be_transferred == 0");
        return Ok(None);
    }

    let min_base_asset_amount =
        minimum_base_asset_amount(sizing.user_base_asset_amount, oracle_price)?;

    let base_asset_amount = standardize_base_asset_amount_ceil(
        sizing
            .user_base_asset_amount
            .min(liquidator_max_base_asset_amount)
            .min(max_allowed.max(min_base_asset_amount)),
        step_size,
    )?;

    if let Some(limit_price) = request.limit_price {
        let position_index = get_position_index(&user.perp_positions, market_index)?;
        validate_transfer_price(
            user.perp_positions[position_index].get_direction(),
            oracle_price,
            sizing.liquidator_fee,
            limit_price,
        )?;
    }

    let base_asset_value =
        calculate_base_asset_value_with_oracle_price(base_asset_amount.cast()?, oracle_price)?
            .cast::<u64>()?;

    Ok(Some(PerpTransfer {
        base_asset_amount,
        base_asset_value,
        liquidator_fee: fee_debit(base_asset_value, sizing.liquidator_fee)?,
        if_fee: fee_debit(base_asset_value, sizing.if_liquidation_fee)?,
        protocol_fee: fee_debit(base_asset_value, sizing.protocol_liquidation_fee)?,
        base_asset_amount_to_cover_margin_shortage,
    }))
}

/// Refuse a transfer the liquidator's limit price does not accept.
///
/// The liquidator enters at the oracle price moved by its own fee, in its own
/// favor, so the limit price bounds that price rather than the oracle.
fn validate_transfer_price(
    direction: PositionDirection,
    oracle_price: i64,
    liquidator_fee: u32,
    limit_price: u64,
) -> VelocityResult {
    let oracle_price_u128 = oracle_price.cast::<u128>()?;
    let fee = oracle_price_u128
        .safe_mul(liquidator_fee.cast()?)?
        .safe_div(LIQUIDATION_FEE_PRECISION_U128)?;

    match direction {
        PositionDirection::Long => {
            let transfer_price = oracle_price_u128.safe_sub(fee)?;
            validate!(
                transfer_price <= limit_price.cast()?,
                ErrorCode::LiquidationDoesntSatisfyLimitPrice,
                "limit price ({}) > transfer price ({})",
                limit_price,
                transfer_price
            )
        }
        PositionDirection::Short => {
            let transfer_price = oracle_price_u128.safe_add(fee)?;
            validate!(
                transfer_price >= limit_price.cast()?,
                ErrorCode::LiquidationDoesntSatisfyLimitPrice,
                "limit price ({}) < transfer price ({})",
                limit_price,
                transfer_price
            )
        }
    }
}

/// Move the position from the account to the liquidator, and accrue the fees.
fn apply_perp_transfer(
    parties: &mut PerpLiquidationParties,
    maps: &AccountMaps,
    market_index: u16,
    transfer: &PerpTransfer,
) -> VelocityResult<PerpTransferPositions> {
    let position_index = get_position_index(&parties.user.perp_positions, market_index)?;
    let user_position_delta = get_position_delta_for_fill(
        transfer.base_asset_amount,
        transfer.base_asset_value,
        parties.user.perp_positions[position_index].get_direction_to_close(),
    )?;
    let liquidator_position_delta = get_position_delta_for_fill(
        transfer.base_asset_amount,
        transfer.base_asset_value,
        parties.user.perp_positions[position_index].get_direction(),
    )?;

    let mut market = maps.perp_market_map.get_ref_mut(&market_index)?;

    let user_side =
        reduce_user_position(parties.user, &mut market, &user_position_delta, transfer)?;

    let (liquidator_existing_direction, liquidator_existing_params) = increase_liquidator_position(
        parties.liquidator,
        &mut market,
        &liquidator_position_delta,
        user_side.existing_direction,
        transfer.liquidator_fee,
    )?;

    // both cuts accrue to pending counters, materialized into
    // revenue_pool / protocol_fee_pool by `sweep_market_fees`
    // (total_liquidation_fee remains a lifetime analytics counter)
    market.fee_ledger.accrue_liquidation_fees(
        transfer.if_fee.unsigned_abs().cast()?,
        transfer.protocol_fee.unsigned_abs().cast()?,
    )?;

    Ok(PerpTransferPositions {
        user_existing_direction: user_side.existing_direction,
        user_direction_to_close: user_side.direction_to_close,
        user_existing_params: user_side.existing_params,
        liquidator_existing_direction,
        liquidator_existing_params,
        user_position_delta,
    })
}

/// Take the position off the account and charge it all three fees.
fn reduce_user_position(
    user: &mut User,
    market: &mut PerpMarket,
    delta: &PositionDelta,
    transfer: &PerpTransfer,
) -> VelocityResult<UserSideOfTransfer> {
    let market_index = market.market_index;
    let user_position = user.get_perp_position_mut(market_index)?;
    let existing_direction = user_position.get_direction();
    let direction_to_close = user_position.get_direction_to_close();
    let existing_params =
        user_position.get_existing_position_params_for_order_action(direction_to_close);

    update_position_and_market(user_position, market, delta)?;
    update_quote_asset_and_break_even_amount(user_position, market, transfer.liquidator_fee)?;
    update_quote_asset_and_break_even_amount(user_position, market, transfer.if_fee)?;
    update_quote_asset_and_break_even_amount(user_position, market, transfer.protocol_fee)?;

    validate!(
        is_multiple_of_step_size(
            user_position.base_asset_amount.unsigned_abs(),
            market.order_step_size
        )?,
        ErrorCode::InvalidPerpPosition,
        "base asset amount {} step size {}",
        user_position.base_asset_amount,
        market.order_step_size
    )?;

    Ok(UserSideOfTransfer {
        existing_direction,
        direction_to_close,
        existing_params,
    })
}

/// Put the position on the liquidator and credit it the execution discount.
fn increase_liquidator_position(
    liquidator: &mut User,
    market: &mut PerpMarket,
    delta: &PositionDelta,
    fill_direction: PositionDirection,
    liquidator_fee: i64,
) -> VelocityResult<(PositionDirection, Option<(u64, u64)>)> {
    let market_index = market.market_index;
    let liquidator_position = liquidator.force_get_perp_position_mut(market_index)?;
    let existing_direction = liquidator_position.get_direction();
    let existing_params =
        liquidator_position.get_existing_position_params_for_order_action(fill_direction);

    update_position_and_market(liquidator_position, market, delta)?;
    update_quote_asset_and_break_even_amount(liquidator_position, market, -liquidator_fee)?;

    validate!(
        is_multiple_of_step_size(
            liquidator_position.base_asset_amount.unsigned_abs(),
            market.order_step_size
        )?,
        ErrorCode::InvalidPerpPosition,
        "base asset amount {} step size {}",
        liquidator_position.base_asset_amount,
        market.order_step_size
    )?;

    Ok((existing_direction, existing_params))
}

/// Release the latch when the transfer cleared the shortage, or admit
/// bankruptcy when nothing is left to seize.
fn close_or_latch_perp_account(
    user: &mut User,
    maps: &AccountMaps,
    opened: &OpenedPerpLiquidation,
    transfer: &PerpTransfer,
) -> VelocityResult {
    if transfer.base_asset_amount >= transfer.base_asset_amount_to_cover_margin_shortage {
        opened.mode.exit_liquidation(user)?;
    } else if opened
        .mode
        .should_user_enter_bankruptcy(user, &maps.spot_market_map)?
    {
        opened.mode.enter_bankruptcy(user)?;
        flag_perp_bankruptcy_claim(user, opened.run.market_index, &maps.perp_market_map)?;
    }

    Ok(())
}

/// Claim the next order ids and fill record id for the two sides.
fn next_fill_ids(
    parties: &mut PerpLiquidationParties,
    maps: &AccountMaps,
    market_index: u16,
) -> VelocityResult<PerpFillIds> {
    let user_order_id = get_then_update_id!(parties.user, next_order_id);
    let liquidator_order_id = get_then_update_id!(parties.liquidator, next_order_id);
    let fill_record_id = {
        let mut market = maps.perp_market_map.get_ref_mut(&market_index)?;
        get_then_update_id!(market, next_fill_record_id)
    };

    Ok(PerpFillIds {
        user_order_id,
        liquidator_order_id,
        fill_record_id,
    })
}

/// Record the transfer as a fill of two synthetic orders.
fn emit_perp_fill_records(
    parties: &PerpLiquidationParties,
    run: &PerpLiquidationRun,
    transfer: &PerpTransfer,
    positions: &PerpTransferPositions,
    request: LiquidatePerpRequest,
    ids: PerpFillIds,
) -> VelocityResult {
    let user_order = Order {
        slot: run.terms.slot,
        base_asset_amount: transfer.base_asset_amount,
        order_id: ids.user_order_id,
        market_index: run.market_index,
        status: OrderStatus::Open,
        order_type: OrderType::Market,
        market_type: MarketType::Perp,
        direction: positions.user_direction_to_close,
        existing_position_direction: positions.user_existing_direction,
        ..Order::default()
    };

    emit!(OrderRecord {
        ts: run.terms.now,
        user: *parties.user_key,
        order: user_order
    });

    let liquidator_order = Order {
        slot: run.terms.slot,
        price: request.limit_price.unwrap_or_default(),
        base_asset_amount: transfer.base_asset_amount,
        order_id: ids.liquidator_order_id,
        market_index: run.market_index,
        status: OrderStatus::Open,
        order_type: if request.limit_price.is_some() {
            OrderType::Limit
        } else {
            OrderType::Market
        },
        market_type: MarketType::Perp,
        direction: positions.user_existing_direction,
        existing_position_direction: positions.liquidator_existing_direction,
        ..Order::default()
    };

    emit!(OrderRecord {
        ts: run.terms.now,
        user: *parties.liquidator_key,
        order: liquidator_order
    });

    emit!(build_liquidation_fill_record(
        run, transfer, positions, ids
    )?);

    Ok(())
}

/// The fill record the two synthetic orders match in.
fn build_liquidation_fill_record(
    run: &PerpLiquidationRun,
    transfer: &PerpTransfer,
    positions: &PerpTransferPositions,
    ids: PerpFillIds,
) -> VelocityResult<OrderActionRecord> {
    let (taker_existing_quote_entry_amount, taker_existing_base_asset_amount) =
        calculate_existing_position_fields_for_order_action(
            transfer.base_asset_amount,
            positions.user_existing_params,
        )?;

    let (maker_existing_quote_entry_amount, maker_existing_base_asset_amount) =
        calculate_existing_position_fields_for_order_action(
            transfer.base_asset_amount,
            positions.liquidator_existing_params,
        )?;

    Ok(OrderActionRecord {
        ts: run.terms.now,
        action: OrderAction::Fill,
        action_explanation: OrderActionExplanation::Liquidation,
        market_index: run.market_index,
        market_type: MarketType::Perp,
        filler: None,
        filler_reward: None,
        fill_record_id: Some(ids.fill_record_id),
        base_asset_amount_filled: Some(transfer.base_asset_amount),
        quote_asset_amount_filled: Some(transfer.base_asset_value),
        taker_fee: Some(
            transfer
                .liquidator_fee
                .unsigned_abs()
                .safe_add(transfer.if_fee.unsigned_abs())?
                .safe_add(transfer.protocol_fee.unsigned_abs())?,
        ),
        maker_fee: Some(transfer.liquidator_fee),
        referrer_reward: None,
        quote_asset_amount_surplus: None,
        spot_fulfillment_method_fee: None,
        taker: Some(*run.user_key),
        taker_order_id: Some(ids.user_order_id),
        taker_order_direction: Some(positions.user_direction_to_close),
        taker_order_base_asset_amount: Some(transfer.base_asset_amount),
        taker_order_cumulative_base_asset_amount_filled: Some(transfer.base_asset_amount),
        taker_order_cumulative_quote_asset_amount_filled: Some(transfer.base_asset_value),
        maker: Some(*run.liquidator_key),
        maker_order_id: Some(ids.liquidator_order_id),
        maker_order_direction: Some(positions.user_existing_direction),
        maker_order_base_asset_amount: Some(transfer.base_asset_amount),
        maker_order_cumulative_base_asset_amount_filled: Some(transfer.base_asset_amount),
        maker_order_cumulative_quote_asset_amount_filled: Some(transfer.base_asset_value),
        oracle_price: run.oracle_price,
        bit_flags: 0,
        taker_existing_quote_entry_amount,
        taker_existing_base_asset_amount,
        maker_existing_quote_entry_amount,
        maker_existing_base_asset_amount,
        trigger_price: None,
        builder_idx: None,
        builder_fee: None,
    })
}
