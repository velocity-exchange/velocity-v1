//! Selling a failing perp position to the book.
//!
//! The account places a forced order at the oracle price less the liquidator's
//! execution discount, and the fill takes whatever liquidity answers it. The
//! caller is only the filler, so it takes on no position of its own, and the
//! insurance fund and the protocol charge their cut on what actually filled.

use super::*;

/// The forced order the liquidation places, and the rates its fill settles at.
struct FillLiquidationOrder {
    params: OrderParams,
    /// The direction the account already holds, which the order closes.
    existing_direction: PositionDirection,
    if_liquidation_fee: u32,
    protocol_liquidation_fee: u32,
}

/// What the forced order filled, and what the fill owes.
struct FilledLiquidation {
    order_id: u32,
    fill_record_id: u64,
    base_asset_amount: u64,
    quote_asset_amount: u64,
    existing_direction: PositionDirection,
    if_liquidation_fee: u32,
    protocol_liquidation_fee: u32,
}

/// Reduce a failing perp position by filling it against the book.
///
/// Returns the quote value the liquidation actually filled, or zero when
/// nothing was liquidated. The crank prices its keeper payment against it: a
/// liquidation is worth landing in proportion to what it recovers, and that is
/// the one figure a caller cannot inflate.
pub fn liquidate_perp_with_fill<'info>(
    market_index: u16,
    accounts: &PerpFillLiquidationAccounts<'_, 'info>,
    maps: &mut AccountMaps,
    clock: &Clock,
    state: &State,
) -> VelocityResult<u64> {
    let terms = LiquidationTerms::from_state(state, clock.unix_timestamp, clock.slot);
    let target = PerpLiquidationTarget {
        user_key: accounts.user_key,
        liquidator_key: accounts.liquidator_key,
        market_index,
    };

    let mut user = load_mut!(accounts.user)?;
    let mut liquidator = load_mut!(accounts.liquidator)?;

    let Some(opened) =
        open_fill_liquidation(&mut user, &mut liquidator, target, maps, &terms, state)?
    else {
        return Ok(0);
    };

    let position_index = get_position_index(&user.perp_positions, market_index)?;
    if user.perp_positions[position_index].base_asset_amount == 0 {
        msg!("User has no base asset amount");
        return Ok(0);
    }

    let margin_shortage = opened
        .mode
        .margin_shortage(&opened.intermediate_margin_calculation)?;

    let Some(order) = size_fill_order(&user, maps, &opened, margin_shortage, state)? else {
        return Ok(0);
    };

    let order_id = user.next_order_id;
    let fill_record_id = maps
        .perp_market_map
        .get_ref(&market_index)?
        .next_fill_record_id;
    place_perp_order(
        state,
        &mut user,
        *accounts.user_key,
        maps,
        clock,
        order.params,
        PlaceOrderOptions::default().explanation(OrderActionExplanation::Liquidation),
        &mut None,
    )?;

    drop(user);
    drop(liquidator);

    let (fill_base_asset_amount, fill_quote_asset_amount) =
        fill_forced_order(order_id, accounts, maps, clock, state)?;

    let mut user = load_mut!(accounts.user)?;
    retire_unfilled_remainder(&mut user, order_id, accounts, maps, clock)?;

    // no fill
    if fill_base_asset_amount == 0 {
        return Err(ErrorCode::LiquidationOrderFailedToFill);
    }

    let filled = FilledLiquidation {
        order_id,
        fill_record_id,
        base_asset_amount: fill_base_asset_amount,
        quote_asset_amount: fill_quote_asset_amount,
        existing_direction: order.existing_direction,
        if_liquidation_fee: order.if_liquidation_fee,
        protocol_liquidation_fee: order.protocol_liquidation_fee,
    };
    settle_filled_liquidation(&mut user, maps, opened, filled, margin_shortage)?;

    Ok(fill_quote_asset_amount)
}

/// Run the entry checks, latch the account, cancel its orders and re-measure
/// it.
///
/// `None` reports that the liquidation is already finished: either the account
/// never needed one, or the cancels alone cleared its shortage.
fn open_fill_liquidation<'a>(
    user: &mut User,
    liquidator: &mut User,
    target: PerpLiquidationTarget<'a>,
    maps: &mut AccountMaps,
    terms: &LiquidationTerms,
    state: &State,
) -> VelocityResult<Option<OpenedPerpLiquidation<'a>>> {
    let market_index = target.market_index;
    let liquidation_mode = get_perp_liquidation_mode(user, market_index)?;

    validate_preconditions(
        user,
        liquidator,
        liquidation_mode.as_ref(),
        maps,
        market_index,
        terms.now,
    )?;
    settle_both_sides_funding(user, liquidator, target, maps, terms.now)?;

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        maps,
        terms.margin_context_tracking(MarketIdentifier::perp(market_index))?,
    )?;

    if check_perp_liquidation_entry(user, liquidation_mode.as_ref(), &margin_calculation)?
        == LiquidationEntry::Exited
    {
        return Ok(None);
    }

    user.get_perp_position(market_index).inspect_err(|_e| {
        msg!(
            "User does not have a position for perp market {}",
            market_index
        );
    })?;

    let Some((run, recheck)) = latch_cancel_and_recheck(
        user,
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

/// Bring both accounts current on funding before the position moves.
fn settle_both_sides_funding(
    user: &mut User,
    liquidator: &mut User,
    target: PerpLiquidationTarget,
    maps: &AccountMaps,
    now: i64,
) -> VelocityResult {
    let market_index = target.market_index;

    settle_funding_payment(
        user,
        target.user_key,
        maps.perp_market_map.get_ref_mut(&market_index)?.deref_mut(),
        now,
    )?;

    settle_funding_payment(
        liquidator,
        target.liquidator_key,
        maps.perp_market_map.get_ref_mut(&market_index)?.deref_mut(),
        now,
    )
}

/// Refuse a liquidation neither account nor market may take part in.
fn validate_preconditions(
    user: &User,
    liquidator: &User,
    liquidation_mode: &dyn LiquidatePerpMode,
    maps: &AccountMaps,
    market_index: u16,
    now: i64,
) -> VelocityResult {
    validate!(
        !liquidation_mode.is_user_bankrupt(user)?,
        ErrorCode::UserBankrupt,
        "user bankrupt",
    )?;

    validate!(
        liquidator.pool_id == 0,
        ErrorCode::InvalidPoolId,
        "liquidator pool id ({}) != 0",
        liquidator.pool_id
    )?;

    validate!(
        !liquidator.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "liquidator bankrupt",
    )?;

    validate_perp_market_liquidatable(&*maps.perp_market_map.get_ref(&market_index)?, now)
}

/// Size the forced order and price the fees its fill will owe.
///
/// `None` reports that the time ramp allows nothing yet, which is not an
/// error: a later call moves what this one may not.
fn size_fill_order(
    user: &User,
    maps: &mut AccountMaps,
    opened: &OpenedPerpLiquidation,
    margin_shortage: u128,
    state: &State,
) -> VelocityResult<Option<FillLiquidationOrder>> {
    let market_index = opened.run.market_index;
    let oracle_price = opened.run.oracle_price;

    validate_oracle_within_twap_band(maps, market_index, oracle_price, state)?;

    let sizing = PerpLiquidationSizing::calculate(
        user,
        maps,
        &opened.run,
        &opened.intermediate_margin_calculation,
        margin_shortage,
    )?;

    let step_size = maps.perp_market_map.get_ref(&market_index)?.order_step_size;
    let base_asset_amount_to_cover_margin_shortage = standardize_base_asset_amount_ceil(
        sizing.base_asset_amount_to_cover_margin_shortage,
        step_size,
    )?;

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
            .min(max_allowed.max(min_base_asset_amount)),
        step_size,
    )?;

    let position_index = get_position_index(&user.perp_positions, market_index)?;
    let existing_direction = user.perp_positions[position_index].get_direction();

    Ok(Some(FillLiquidationOrder {
        params: get_liquidation_order_params(
            market_index,
            existing_direction,
            base_asset_amount,
            oracle_price,
            sizing.liquidator_fee,
        )?,
        existing_direction,
        if_liquidation_fee: sizing.if_liquidation_fee,
        protocol_liquidation_fee: sizing.protocol_liquidation_fee,
    }))
}

/// Fill the forced order against the resting book.
fn fill_forced_order<'info>(
    order_id: u32,
    accounts: &PerpFillLiquidationAccounts<'_, 'info>,
    maps: &mut AccountMaps,
    clock: &Clock,
    state: &State,
) -> VelocityResult<(u64, u64)> {
    fill_perp_order_without_external_books(
        order_id,
        state,
        accounts.user,
        accounts.user_stats,
        maps,
        accounts.liquidator,
        accounts.liquidator_stats,
        accounts.makers_and_referrer,
        accounts.makers_and_referrer_stats,
        clock,
        FillMode::Liquidation,
        &mut None,
        false,
    )
}

/// Cancel whatever the forced order did not fill.
fn retire_unfilled_remainder<'info>(
    user: &mut User,
    order_id: u32,
    accounts: &PerpFillLiquidationAccounts<'_, 'info>,
    maps: &mut AccountMaps,
    clock: &Clock,
) -> VelocityResult {
    let Ok(order_index) = user.get_order_index(order_id) else {
        return Ok(());
    };

    cancel_order(
        order_index,
        user,
        accounts.user_key,
        maps,
        clock.unix_timestamp,
        clock.slot,
        OrderActionExplanation::None,
        Some(accounts.liquidator_key),
        0,
        false,
    )
}

/// Charge the insurance-side fees on what filled, then close the liquidation.
fn settle_filled_liquidation(
    user: &mut User,
    maps: &mut AccountMaps,
    opened: OpenedPerpLiquidation,
    filled: FilledLiquidation,
    margin_shortage: u128,
) -> VelocityResult {
    let market_index = opened.run.market_index;
    let if_fee = fee_debit(filled.quote_asset_amount, filled.if_liquidation_fee)?;
    let protocol_fee = fee_debit(filled.quote_asset_amount, filled.protocol_liquidation_fee)?;

    charge_fill_fees(user, maps, market_index, if_fee, protocol_fee)?;

    let (margin_freed_for_perp_position, margin_calculation_after) = calculate_margin_freed(
        user,
        maps,
        opened.run.terms.margin_buffer_ratio,
        margin_shortage,
        Some(opened.mode.as_ref()),
    )?;

    let margin_freed = opened
        .margin_freed
        .safe_add(margin_freed_for_perp_position)?;
    opened
        .mode
        .increment_free_margin(user, margin_freed_for_perp_position)?;

    if opened
        .mode
        .can_exit_liquidation(&margin_calculation_after)?
    {
        opened.mode.exit_liquidation(user)?;
    } else if opened
        .mode
        .should_user_enter_bankruptcy(user, &maps.spot_market_map)?
    {
        opened.mode.enter_bankruptcy(user)?;
        flag_perp_bankruptcy_claim(user, market_index, &maps.perp_market_map)?;
    }

    let user_position_delta = get_position_delta_for_fill(
        filled.base_asset_amount,
        filled.quote_asset_amount,
        filled.existing_direction,
    )?;

    let outcome = PerpLiquidationOutcome {
        canceled_order_ids: opened.canceled_order_ids,
        margin_freed,
        liquidate_perp: LiquidatePerpRecord {
            market_index,
            oracle_price: opened.run.oracle_price,
            base_asset_amount: user_position_delta.base_asset_amount,
            quote_asset_amount: user_position_delta.quote_asset_amount,
            user_order_id: filled.order_id,
            liquidator_order_id: 0,
            fill_record_id: filled.fill_record_id,
            liquidator_fee: 0,
            if_fee: if_fee.abs().cast()?,
            protocol_fee: protocol_fee.abs().cast()?,
        },
    };
    emit_liquidate_perp_record(
        user,
        opened.mode.as_ref(),
        &opened.run,
        &opened.margin_calculation,
        outcome,
    )
}

/// Debit the account the insurance and protocol cuts, and accrue them.
fn charge_fill_fees(
    user: &mut User,
    maps: &AccountMaps,
    market_index: u16,
    if_fee: i64,
    protocol_fee: i64,
) -> VelocityResult {
    let mut market = maps.perp_market_map.get_ref_mut(&market_index)?;

    let user_position = user.get_perp_position_mut(market_index)?;
    update_quote_asset_and_break_even_amount(user_position, &mut market, if_fee)?;
    update_quote_asset_and_break_even_amount(user_position, &mut market, protocol_fee)?;

    market.fee_ledger.accrue_liquidation_fees(
        if_fee.unsigned_abs().cast()?,
        protocol_fee.unsigned_abs().cast()?,
    )
}
