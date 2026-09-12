//! Taking over a failing borrow in exchange for positive perp pnl.
//!
//! The liquidator absorbs the account's borrow and receives its unsettled
//! profit at a discount. The position must be closed first, so only the profit
//! is left to hand over, and a borrow whose oracle is not valid is refused.

use super::*;

/// The account's positive perp pnl, as the liquidator buys it.
struct PerpPnlAsset {
    /// The unsettled profit, unsigned.
    pnl: u128,
    quote_price: i64,
    quote_decimals: u32,
    /// The maintenance weight the margin calculation gives the profit.
    weight: u32,
    /// The liquidator's premium, as a multiplier.
    liquidation_multiplier: u32,
}

/// The account after the order cancels, when it is still liquidatable.
struct PnlCancelRecheck {
    margin_calculation: MarginCalculation,
    margin_freed: u64,
}

/// A borrow-for-pnl liquidation that passed its entry checks and holds the
/// latch.
struct OpenedBorrowLiquidation {
    pnl_asset: PerpPnlAsset,
    liability: SpotLiquidationSide,
    liquidation_id: u16,
    /// The picture the liquidation opened on, which the record reports.
    margin_calculation: MarginCalculation,
    /// The picture after the order cancels, which the transfer is sized on.
    intermediate_margin_calculation: MarginCalculation,
    margin_freed: u64,
}

/// How much of the borrow the liquidator takes, and the pnl it pays with.
struct PnlForBorrowTransfer {
    liability_transfer: u128,
    pnl_transfer: u128,
    /// The repayment that would clear the whole shortage.
    liability_transfer_to_cover_margin_shortage: u128,
}

pub fn liquidate_borrow_for_perp_pnl(
    request: LiquidateBorrowForPerpPnlRequest,
    parties: &mut LiquidationParties,
    maps: &mut AccountMaps,
    terms: &LiquidationTerms,
    funding_paused: bool,
) -> VelocityResult {
    // liquidator takes over a user borrow in exchange for that user's positive perpetual pnl
    // can only be done once a user's perpetual position size is 0
    // blocks borrows where oracle is deemed invalid
    let Some(opened) = open_borrow_liquidation(request, parties, maps, terms, funding_paused)?
    else {
        return Ok(());
    };

    complete_borrow_liquidation(request, parties, maps, terms, opened)
}

/// Run the entry checks, latch the account, cancel its orders and re-measure
/// it.
///
/// `None` reports that the liquidation is already finished: the cancels alone
/// cleared the shortage, or the account never needed one.
fn open_borrow_liquidation(
    request: LiquidateBorrowForPerpPnlRequest,
    parties: &mut LiquidationParties,
    maps: &mut AccountMaps,
    terms: &LiquidationTerms,
    funding_paused: bool,
) -> VelocityResult<Option<OpenedBorrowLiquidation>> {
    validate_preconditions(request, parties, maps)?;
    settle_both_sides_funding(request.perp_market_index, parties, maps, terms.now)?;

    let pnl_asset = read_perp_pnl_asset(parties.user, maps, request.perp_market_index)?;
    let liability = read_liability_side(
        parties.user,
        maps,
        request.liability_market_index,
        terms.now,
        (terms.slot, funding_paused),
    )?;

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        parties.user,
        maps,
        terms.margin_context(),
    )?;

    if check_cross_margin_entry(parties.user, &margin_calculation)? == LiquidationEntry::Exited {
        return Ok(None);
    }

    let liquidation_id = parties.user.enter_cross_margin_liquidation(terms.slot)?;

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

    let Some(recheck) = recheck_after_cancels(
        request,
        parties,
        maps,
        terms,
        (&margin_calculation, canceled_order_ids),
        (liquidation_id, liability.price),
    )?
    else {
        return Ok(None);
    };

    Ok(Some(OpenedBorrowLiquidation {
        pnl_asset,
        liability,
        liquidation_id,
        margin_calculation,
        intermediate_margin_calculation: recheck.margin_calculation,
        margin_freed: recheck.margin_freed,
    }))
}

/// Move the borrow and the pnl, then close or re-latch the account.
fn complete_borrow_liquidation(
    request: LiquidateBorrowForPerpPnlRequest,
    parties: &mut LiquidationParties,
    maps: &mut AccountMaps,
    terms: &LiquidationTerms,
    opened: OpenedBorrowLiquidation,
) -> VelocityResult {
    let margin_shortage = opened
        .intermediate_margin_calculation
        .cross_margin_margin_shortage()?;

    let Some(transfer) = size_transfer(
        request,
        parties.user,
        maps,
        (&opened.pnl_asset, &opened.liability),
        (margin_shortage, terms),
    )?
    else {
        return Ok(());
    };

    validate_transfer_satisfies_limit_price(
        transfer.pnl_transfer,
        transfer.liability_transfer,
        opened.pnl_asset.quote_decimals,
        opened.liability.decimals,
        request.limit_price,
    )?;

    apply_transfer(request, parties, maps, &transfer)?;

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
        flag_perp_bankruptcy_claim(
            parties.user,
            request.perp_market_index,
            &maps.perp_market_map,
        )?;
    }

    validate_liquidator_takes_on_risk(
        parties.liquidator,
        maps,
        "Liquidator doesnt have enough collateral to take over borrow",
    )?;

    let market_oracle_price = {
        let market = maps
            .perp_market_map
            .get_ref_mut(&request.perp_market_index)?;
        maps.oracle_map.get_price_data(&market.oracle_id())?.price
    };

    emit_liquidate_borrow_record(
        parties,
        &opened.margin_calculation,
        opened.liquidation_id,
        (margin_freed, vec![]),
        LiquidateBorrowForPerpPnlRecord {
            perp_market_index: request.perp_market_index,
            market_oracle_price,
            pnl_transfer: transfer.pnl_transfer,
            liability_market_index: request.liability_market_index,
            liability_price: opened.liability.price,
            liability_transfer: transfer.liability_transfer,
        },
        terms.now,
    )
}

/// Refuse a liquidation neither account nor market may take part in.
fn validate_preconditions(
    request: LiquidateBorrowForPerpPnlRequest,
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

    validate!(
        parties.liquidator.pool_id == 0,
        ErrorCode::InvalidPoolId,
        "liquidator pool id ({}) != 0",
        parties.liquidator.pool_id
    )?;

    validate!(
        !maps
            .perp_market_map
            .get_ref(&request.perp_market_index)?
            .is_operation_paused(PerpOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for perp market {}",
        request.perp_market_index
    )?;

    validate!(
        !maps
            .spot_market_map
            .get_ref(&request.liability_market_index)?
            .is_operation_paused(SpotOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        request.liability_market_index
    )?;

    validate_both_sides_have_positions(request, parties)
}

/// Refuse a transfer either side has nowhere to book.
fn validate_both_sides_have_positions(
    request: LiquidateBorrowForPerpPnlRequest,
    parties: &mut LiquidationParties,
) -> VelocityResult {
    parties
        .user
        .get_perp_position(request.perp_market_index)
        .inspect_err(|_e| {
            msg!(
                "User does not have a position for perp market {}",
                request.perp_market_index
            );
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
        .force_get_perp_position_mut(request.perp_market_index)
        .inspect_err(|_e| {
            msg!("Liquidator has no available positions to take on pnl");
        })?;

    parties
        .liquidator
        .force_get_spot_position_mut(request.liability_market_index)
        .inspect_err(|_e| {
            msg!("Liquidator has no available spot balances to take on borrow");
        })?;

    Ok(())
}

/// Bring both accounts current on funding before the pnl moves.
fn settle_both_sides_funding(
    perp_market_index: u16,
    parties: &mut LiquidationParties,
    maps: &AccountMaps,
    now: i64,
) -> VelocityResult {
    settle_funding_payment(
        parties.user,
        parties.user_key,
        maps.perp_market_map
            .get_ref_mut(&perp_market_index)?
            .deref_mut(),
        now,
    )?;

    settle_funding_payment(
        parties.liquidator,
        parties.liquidator_key,
        maps.perp_market_map
            .get_ref_mut(&perp_market_index)?
            .deref_mut(),
        now,
    )
}

/// Read the unsettled profit the liquidator buys.
fn read_perp_pnl_asset(
    user: &User,
    maps: &mut AccountMaps,
    perp_market_index: u16,
) -> VelocityResult<PerpPnlAsset> {
    let user_position = user.get_perp_position(perp_market_index)?;

    let base_asset_amount = user_position.base_asset_amount;

    validate!(
        base_asset_amount == 0,
        ErrorCode::InvalidPerpPositionToLiquidate,
        "Cant have open perp position (base_asset_amount: {})",
        base_asset_amount
    )?;

    let pnl = user_position.quote_asset_amount.cast::<i128>()?;

    validate!(
        pnl > 0,
        ErrorCode::InvalidPerpPositionToLiquidate,
        "Perp position must have position pnl"
    )?;

    validate!(
        !user_position.is_isolated(),
        ErrorCode::InvalidPerpPositionToLiquidate,
        "Perp position is an isolated position"
    )?;

    let market = maps.perp_market_map.get_ref(&perp_market_index)?;

    let quote_spot_market = maps
        .spot_market_map
        .get_ref(&market.quote_spot_market_index)?;
    let quote_price = maps
        .oracle_map
        .get_price_data(&quote_spot_market.oracle_id())?
        .price;

    Ok(PerpPnlAsset {
        pnl: pnl.unsigned_abs(),
        quote_price,
        quote_decimals: 6,
        weight: market.get_unrealized_asset_weight(pnl, MarginRequirementType::Maintenance)?,
        liquidation_multiplier: calculate_liquidation_multiplier(
            market.liquidator_fee,
            LiquidationMultiplierType::Premium,
        )?,
    })
}

/// Read the borrow the liquidator takes over.
fn read_liability_side(
    user: &User,
    maps: &mut AccountMaps,
    market_index: u16,
    now: i64,
    clock: (u64, bool),
) -> VelocityResult<SpotLiquidationSide> {
    let (slot, funding_paused) = clock;
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
        "User did not have a borrow for the borrow market index"
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
    // being taken over and cheapen the pnl received for it
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
fn recheck_after_cancels(
    request: LiquidateBorrowForPerpPnlRequest,
    parties: &mut LiquidationParties,
    maps: &mut AccountMaps,
    terms: &LiquidationTerms,
    entry: (&MarginCalculation, Vec<u32>),
    record: (u16, i64),
) -> VelocityResult<Option<PnlCancelRecheck>> {
    let (margin_calculation, canceled_order_ids) = entry;
    if canceled_order_ids.is_empty() {
        return Ok(Some(PnlCancelRecheck {
            margin_calculation: margin_calculation.clone(),
            margin_freed: 0,
        }));
    }

    let (liquidation_id, liability_price) = record;
    let (margin_freed, intermediate_margin_calculation) = recheck_cross_margin_after_cancels(
        parties.user,
        maps,
        terms.margin_context(),
        margin_calculation.cross_margin_margin_shortage()?,
    )?;

    if !intermediate_margin_calculation.can_exit_cross_margin_liquidation()? {
        return Ok(Some(PnlCancelRecheck {
            margin_calculation: intermediate_margin_calculation,
            margin_freed,
        }));
    }

    let market_oracle_price = {
        let market = maps.perp_market_map.get_ref(&request.perp_market_index)?;
        maps.oracle_map.get_price_data(&market.oracle_id())?.price
    };

    emit_liquidate_borrow_record(
        parties,
        margin_calculation,
        liquidation_id,
        (margin_freed, canceled_order_ids),
        LiquidateBorrowForPerpPnlRecord {
            perp_market_index: request.perp_market_index,
            market_oracle_price,
            pnl_transfer: 0,
            liability_market_index: request.liability_market_index,
            liability_price,
            liability_transfer: 0,
        },
        terms.now,
    )?;

    parties.user.exit_cross_margin_liquidation();
    Ok(None)
}

/// Size the borrow the liquidator takes and the pnl that pays for it.
///
/// `None` reports that the time ramp allows nothing yet, which is not an
/// error: a later call moves what this one may not.
fn size_transfer(
    request: LiquidateBorrowForPerpPnlRequest,
    user: &User,
    maps: &AccountMaps,
    sides: (&PerpPnlAsset, &SpotLiquidationSide),
    shortage: (u128, &LiquidationTerms),
) -> VelocityResult<Option<PnlForBorrowTransfer>> {
    let (pnl_asset, liability) = sides;
    let (margin_shortage, terms) = shortage;
    let liability_weight_with_buffer = liability.weight.safe_add(terms.margin_buffer_ratio)?;

    // Determine what amount of borrow to transfer to reduce margin shortage to 0.
    // valuation (shortage -> tokens) stays at the raw oracle price, consistent with
    // the margin calculation; only the exchange rate uses the protective price
    let liability_transfer_to_cover_margin_shortage =
        calculate_liability_transfer_to_cover_margin_shortage(
            margin_shortage,
            pnl_asset.weight,
            pnl_asset.liquidation_multiplier,
            liability_weight_with_buffer,
            liability.liquidation_multiplier,
            liability.decimals,
            liability.oracle_price,
            0,
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

    // Given the user's deposit amount, how much borrow can be transferred?
    let liability_transfer_implied_by_pnl = calculate_liability_transfer_implied_by_asset_amount(
        pnl_asset.pnl,
        pnl_asset.liquidation_multiplier,
        pnl_asset.quote_decimals,
        pnl_asset.quote_price,
        liability.liquidation_multiplier,
        liability.decimals,
        liability.price,
    )?;

    let liability_transfer = request
        .liquidator_max_liability_transfer
        .min(liability.amount)
        // want to make sure the liability_transfer_to_cover_margin_shortage doesn't lead to dust positions
        .min(max_liability_allowed_to_be_transferred.max(minimum_liability_transfer(liability)?))
        .min(liability_transfer_implied_by_pnl);

    // Given the borrow amount to transfer, determine how much deposit amount to transfer
    let pnl_transfer = calculate_asset_transfer_for_liability_transfer(
        pnl_asset.pnl,
        pnl_asset.liquidation_multiplier,
        pnl_asset.quote_decimals,
        pnl_asset.quote_price,
        liability_transfer,
        liability.liquidation_multiplier,
        liability.decimals,
        liability.price,
    )?;

    if liability_transfer == 0 || pnl_transfer == 0 {
        log_empty_transfer(
            request,
            liability.amount,
            liability_transfer_to_cover_margin_shortage,
            liability_transfer_implied_by_pnl,
            (liability_transfer, pnl_transfer),
        );
        return Err(ErrorCode::InvalidLiquidation);
    }

    Ok(Some(PnlForBorrowTransfer {
        liability_transfer,
        pnl_transfer,
        liability_transfer_to_cover_margin_shortage,
    }))
}

/// Report the inputs that produced a transfer of nothing.
fn log_empty_transfer(
    request: LiquidateBorrowForPerpPnlRequest,
    liability_amount: u128,
    liability_transfer_to_cover_margin_shortage: u128,
    liability_transfer_implied_by_pnl: u128,
    transfers: (u128, u128),
) {
    let (liability_transfer, pnl_transfer) = transfers;
    msg!(
        "perp_market_index {} liability_market_index {}",
        request.perp_market_index,
        request.liability_market_index
    );
    msg!(
        "liquidator_max_liability_transfer {} liability_amount {} liability_transfer_to_cover_margin_shortage {}",
        request.liquidator_max_liability_transfer,
        liability_amount,
        liability_transfer_to_cover_margin_shortage
    );
    msg!(
        "liability_transfer_implied_by_pnl {} liability_transfer {} pnl_transfer {}",
        liability_transfer_implied_by_pnl,
        liability_transfer,
        pnl_transfer
    );
}

/// Move the borrow to the liquidator and the pnl to the account that took it
/// on.
fn apply_transfer(
    request: LiquidateBorrowForPerpPnlRequest,
    parties: &mut LiquidationParties,
    maps: &AccountMaps,
    transfer: &PnlForBorrowTransfer,
) -> VelocityResult {
    {
        let mut liability_market = maps
            .spot_market_map
            .get_ref_mut(&request.liability_market_index)?;

        update_spot_balances_and_cumulative_deposits(
            transfer.liability_transfer,
            &SpotBalanceType::Deposit,
            &mut liability_market,
            parties
                .user
                .force_get_spot_position_mut(request.liability_market_index)?,
            false,
            Some(transfer.liability_transfer),
        )?;

        update_spot_balances_and_cumulative_deposits(
            transfer.liability_transfer,
            &SpotBalanceType::Borrow,
            &mut liability_market,
            parties
                .liquidator
                .force_get_spot_position_mut(request.liability_market_index)?,
            false,
            Some(transfer.liability_transfer),
        )?;
    }

    let mut market = maps
        .perp_market_map
        .get_ref_mut(&request.perp_market_index)?;
    let liquidator_position = parties
        .liquidator
        .force_get_perp_position_mut(request.perp_market_index)?;
    update_quote_asset_amount(
        liquidator_position,
        &mut market,
        transfer.pnl_transfer.cast()?,
    )?;

    let user_position = parties
        .user
        .get_perp_position_mut(request.perp_market_index)?;
    update_quote_asset_amount(user_position, &mut market, -transfer.pnl_transfer.cast()?)
}

/// Emit one `LiquidateBorrowForPerpPnl` record.
///
/// A completed liquidation reports no canceled orders. The cancels it made
/// already appear in the record the cancels themselves emit.
fn emit_liquidate_borrow_record(
    parties: &LiquidationParties,
    margin_calculation: &MarginCalculation,
    liquidation_id: u16,
    outcome: (u64, Vec<u32>),
    liquidate_borrow_for_perp_pnl: LiquidateBorrowForPerpPnlRecord,
    now: i64,
) -> VelocityResult {
    let (margin_freed, canceled_order_ids) = outcome;

    emit!(LiquidationRecord {
        ts: now,
        liquidation_id,
        liquidation_type: LiquidationType::LiquidateBorrowForPerpPnl,
        user: *parties.user_key,
        liquidator: *parties.liquidator_key,
        margin_requirement: margin_calculation.margin_requirement,
        total_collateral: margin_calculation.total_collateral,
        bankrupt: parties.user.is_cross_margin_bankrupt(),
        canceled_order_ids,
        margin_freed,
        liquidate_borrow_for_perp_pnl,
        ..LiquidationRecord::default()
    });

    Ok(())
}
