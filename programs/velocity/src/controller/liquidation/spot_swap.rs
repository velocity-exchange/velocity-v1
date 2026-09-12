//! Repaying a failing borrow through an external swap.
//!
//! The pair runs in one transaction. [`liquidate_spot_with_swap_begin`] bounds
//! the collateral the swap may take out, the caller swaps it for the borrowed
//! asset, and [`liquidate_spot_with_swap_end`] books what came back.
//!
//! Neither half advances either market's *oracle* TWAPs. Both judge the
//! oracles against the TWAPs as they stand, and both price against the same
//! unmoved values. A refresh in `begin` would pull each TWAP toward the live
//! oracle price and then widen the band checks, and it would also be gone from
//! the account by the time `end` reads it. `end` is a separate instruction, so
//! there is nowhere to hold a snapshot across the pair; not moving the value is
//! the fix (OtterSec #109-#112, #134). The deposit, borrow and utilization
//! TWAPs still advance, in `handle_liquidate_spot_with_swap_begin`.

use super::*;

/// The insurance-side rates a swap-backed repayment settles at.
#[derive(Clone, Copy)]
struct SwapLiabilityFees {
    if_liquidation_fee: u32,
    protocol_liquidation_fee: u32,
}

/// Open a swap-backed spot liquidation, and bound the collateral it may take.
pub fn liquidate_spot_with_swap_begin(
    request: LiquidateSpotSwapBeginRequest,
    parties: &mut LiquidationParties,
    maps: &mut AccountMaps,
    terms: &LiquidationTerms,
    state: &State,
) -> VelocityResult {
    validate_begin_preconditions(request, parties, maps)?;

    let mut asset = read_asset_side(
        maps,
        request.asset_market_index,
        terms.slot,
        LogMode::ExchangeOracle,
    )?;
    asset.amount = read_user_deposit_amount(parties.user, maps, request.asset_market_index)?;

    let (liability, liability_fees) = read_liability_side(
        maps,
        request.liability_market_index,
        terms.slot,
        LogMode::ExchangeOracle,
    )?;
    read_user_borrow_amount(parties.user, maps, request.liability_market_index)?;

    validate_lst_oracle_delays(&asset, &liability)?;

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        parties.user,
        maps,
        terms.margin_context_tracking(MarketIdentifier::spot(request.liability_market_index))?,
    )?;

    // A swap that is no longer needed must throw, because the caller's swap is
    // already staged behind this instruction.
    if !parties.user.is_cross_margin_being_liquidated()
        && margin_calculation.meets_cross_margin_requirement()
    {
        msg!("margin calculation: {:?}", margin_calculation);
        return Err(ErrorCode::SufficientCollateral);
    } else if parties.user.is_cross_margin_being_liquidated()
        && margin_calculation.can_exit_cross_margin_liquidation()?
    {
        msg!("margin calculation: {:?}", margin_calculation);
        return Err(ErrorCode::InvalidLiquidation);
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

    let intermediate_margin_calculation = recheck_after_cancels(
        parties,
        maps,
        terms,
        (&margin_calculation, canceled_order_ids),
        (request, liquidation_id, (asset.price, liability.price)),
    )?;

    let max_asset_transfer = bound_swap_amount(
        request,
        parties.user,
        maps,
        &intermediate_margin_calculation,
        (&asset, &liability, liability_fees),
        terms,
    )?;

    validate_swap_amount_in(request, max_asset_transfer, asset.amount)?;
    validate_swap_oracles_within_bands(
        request,
        (asset.oracle_price, liability.oracle_price),
        maps,
        state,
    )
}

/// Close a swap-backed spot liquidation with what the swap actually moved.
pub fn liquidate_spot_with_swap_end(
    request: LiquidateSpotSwapEndRequest,
    user: &mut User,
    user_key: &Pubkey,
    liquidator_key: &Pubkey,
    maps: &mut AccountMaps,
    terms: &LiquidationTerms,
) -> VelocityResult {
    let asset = read_asset_side(maps, request.asset_market_index, terms.slot, LogMode::None)?;
    let (liability, liability_fees) = read_liability_side(
        maps,
        request.liability_market_index,
        terms.slot,
        LogMode::None,
    )?;

    validate_swap_within_liquidation_boundaries(
        request.asset_transfer,
        request.liability_transfer,
        asset.decimals,
        liability.decimals,
        asset.price,
        liability.price,
        asset.liquidation_multiplier,
        liability.liquidation_multiplier,
    )?;

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        maps,
        terms.margin_context_tracking(MarketIdentifier::spot(request.liability_market_index))?,
    )?;

    let liquidation_id = user.enter_cross_margin_liquidation(terms.slot)?;
    let margin_shortage = margin_calculation.cross_margin_margin_shortage()?;

    let (if_fee, protocol_fee) = split_swap_end_fees(
        request,
        &margin_calculation,
        (&asset, &liability, liability_fees),
        terms.margin_buffer_ratio,
    )?;

    apply_swap_end_balances(request, user, maps, (if_fee, protocol_fee))?;

    let (margin_freed_from_liability, margin_calulcation_after) =
        calculate_margin_freed(user, maps, terms.margin_buffer_ratio, margin_shortage, None)?;

    user.increment_margin_freed(margin_freed_from_liability)?;

    if margin_calulcation_after.can_exit_cross_margin_liquidation()? {
        user.exit_cross_margin_liquidation();
    } else if is_cross_margin_bankrupt(user, &maps.spot_market_map)? {
        user.enter_cross_margin_bankruptcy();
    }

    emit!(LiquidationRecord {
        ts: terms.now,
        liquidation_id,
        liquidation_type: LiquidationType::LiquidateSpot,
        user: *user_key,
        liquidator: *liquidator_key,
        margin_requirement: margin_calculation.margin_requirement,
        total_collateral: margin_calculation.total_collateral,
        bankrupt: user.is_cross_margin_bankrupt(),
        margin_freed: margin_freed_from_liability,
        liquidate_spot: LiquidateSpotRecord {
            asset_market_index: request.asset_market_index,
            asset_price: asset.price,
            asset_transfer: request.asset_transfer,
            liability_market_index: request.liability_market_index,
            liability_price: liability.price,
            liability_transfer: request.liability_transfer,
            if_fee: if_fee.cast()?,
            protocol_fee: protocol_fee.cast()?,
        },
        ..LiquidationRecord::default()
    });

    Ok(())
}

/// Refuse a liquidation neither account nor market may take part in.
fn validate_begin_preconditions(
    request: LiquidateSpotSwapBeginRequest,
    parties: &LiquidationParties,
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

    let asset_spot_market = maps.spot_market_map.get_ref(&request.asset_market_index)?;

    validate!(
        !asset_spot_market.is_operation_paused(SpotOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        request.asset_market_index
    )?;

    let liability_spot_market = maps
        .spot_market_map
        .get_ref(&request.liability_market_index)?;

    validate!(
        !liability_spot_market.is_operation_paused(SpotOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        request.liability_market_index
    )?;

    validate!(
        asset_spot_market.pool_id == liability_spot_market.pool_id,
        ErrorCode::InvalidPoolId,
        "asset_spot_market pool id ({}) != liability_spot_market pool id ({})",
        asset_spot_market.pool_id,
        liability_spot_market.pool_id
    )
}

/// Read the deposit the swap takes collateral from, without moving the oracle
/// TWAP.
fn read_asset_side(
    maps: &mut AccountMaps,
    market_index: u16,
    slot: u64,
    log_mode: LogMode,
) -> VelocityResult<SpotLiquidationSide> {
    let slot_clock = maps.oracle_map.slot_clock;
    let asset_market = maps.spot_market_map.get_ref(&market_index)?;
    let (asset_price_data, validity_guard_rails) = maps
        .oracle_map
        .get_price_data_and_guard_rails(&asset_market.oracle_id())?;

    let asset_validity = check_spot_oracle_validity(
        &asset_market,
        asset_price_data,
        validity_guard_rails,
        Some(VelocityAction::Liquidate),
        log_mode,
        slot,
        slot_clock,
    )?;

    let twap_5min = asset_market
        .historical_oracle_data
        .last_oracle_price_twap_5min;

    // a margin-invalid (stale/uncertain) deposit oracle may make the account
    // liquidatable, but must not let its collateral be swapped away at a
    // depressed price: cap the swap at a user-protective price instead
    let asset_price =
        if is_oracle_valid_for_action(asset_validity, Some(VelocityAction::MarginCalc))? {
            asset_price_data.price
        } else {
            calculate_user_protective_asset_price(asset_price_data, twap_5min)?
        };

    Ok(SpotLiquidationSide {
        amount: 0,
        oracle_price: asset_price_data.price,
        price: asset_price,
        pre_refresh_twap_5min: twap_5min,
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

/// The deposit the account holds in the asset market.
fn read_user_deposit_amount(
    user: &User,
    maps: &AccountMaps,
    market_index: u16,
) -> VelocityResult<u128> {
    let asset_market = maps.spot_market_map.get_ref(&market_index)?;
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

    Ok(token_amount)
}

/// The borrow the account holds in the liability market.
fn read_user_borrow_amount(
    user: &User,
    maps: &AccountMaps,
    market_index: u16,
) -> VelocityResult<u128> {
    let liability_market = maps.spot_market_map.get_ref(&market_index)?;
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

    Ok(token_amount)
}

/// Read the borrow the swap repays, without moving the oracle TWAP.
fn read_liability_side(
    maps: &mut AccountMaps,
    market_index: u16,
    slot: u64,
    log_mode: LogMode,
) -> VelocityResult<(SpotLiquidationSide, SwapLiabilityFees)> {
    let slot_clock = maps.oracle_map.slot_clock;
    let liability_market = maps.spot_market_map.get_ref(&market_index)?;
    let (liability_price_data, validity_guard_rails) = maps
        .oracle_map
        .get_price_data_and_guard_rails(&liability_market.oracle_id())?;

    let liability_validity = check_spot_oracle_validity(
        &liability_market,
        liability_price_data,
        validity_guard_rails,
        Some(VelocityAction::Liquidate),
        log_mode,
        slot,
        slot_clock,
    )?;

    let twap_5min = liability_market
        .historical_oracle_data
        .last_oracle_price_twap_5min;

    // the liability side of the exchange rate gets the mirrored protection: a
    // margin-invalid (stale/uncertain) borrow oracle must not overvalue the debt
    // being repaid and inflate the collateral allowed to be swapped for it
    let liability_price =
        if is_oracle_valid_for_action(liability_validity, Some(VelocityAction::MarginCalc))? {
            liability_price_data.price
        } else {
            calculate_user_protective_liability_price(liability_price_data, twap_5min)?
        };

    Ok((
        SpotLiquidationSide {
            amount: 0,
            oracle_price: liability_price_data.price,
            price: liability_price,
            pre_refresh_twap_5min: twap_5min,
            decimals: liability_market.decimals,
            weight: liability_market.maintenance_liability_weight,
            liquidation_multiplier: calculate_liquidation_multiplier(
                liability_market.liquidator_fee,
                LiquidationMultiplierType::Discount,
            )?,
            pool_id: liability_market.pool_id,
            oracle_delay: liability_price_data.delay,
        },
        SwapLiabilityFees {
            if_liquidation_fee: liability_market.if_liquidation_fee,
            protocol_liquidation_fee: liability_market.protocol_liquidation_fee,
        },
    ))
}

/// Re-measure the account after its orders are canceled.
///
/// The record is emitted whatever the outcome, because the cancels already
/// moved the account. A liquidation the cancels resolved must then throw, to
/// stop the swap the caller staged behind this instruction.
fn recheck_after_cancels(
    parties: &mut LiquidationParties,
    maps: &mut AccountMaps,
    terms: &LiquidationTerms,
    entry: (&MarginCalculation, Vec<u32>),
    record: (LiquidateSpotSwapBeginRequest, u16, (i64, i64)),
) -> VelocityResult<MarginCalculation> {
    let (margin_calculation, canceled_order_ids) = entry;
    if canceled_order_ids.is_empty() {
        return Ok(margin_calculation.clone());
    }

    let (request, liquidation_id, (asset_price, liability_price)) = record;
    let (margin_freed, intermediate_margin_calculation) = recheck_cross_margin_after_cancels(
        parties.user,
        maps,
        terms.margin_context_tracking(MarketIdentifier::spot(request.liability_market_index))?,
        margin_calculation.cross_margin_margin_shortage()?,
    )?;

    emit!(LiquidationRecord {
        ts: terms.now,
        liquidation_id,
        liquidation_type: LiquidationType::LiquidateSpot,
        user: *parties.user_key,
        liquidator: *parties.liquidator_key,
        margin_requirement: margin_calculation.margin_requirement,
        total_collateral: margin_calculation.total_collateral,
        bankrupt: parties.user.is_cross_margin_bankrupt(),
        canceled_order_ids,
        margin_freed,
        liquidate_spot: LiquidateSpotRecord {
            asset_market_index: request.asset_market_index,
            asset_price,
            asset_transfer: 0,
            liability_market_index: request.liability_market_index,
            liability_price,
            liability_transfer: 0,
            if_fee: 0,
            protocol_fee: 0,
        },
        ..LiquidationRecord::default()
    });

    // must throw error to stop swap
    if intermediate_margin_calculation.can_exit_cross_margin_liquidation()? {
        return Err(ErrorCode::InvalidLiquidation);
    }

    Ok(intermediate_margin_calculation)
}

/// The most collateral this swap may take out of the account.
///
/// The bound is the time-ramped share of the shortage, not the whole shortage.
/// Deriving it from the full shortage would let this lane seize more
/// collateral in a single swap than the throttle permits, because
/// `swap_amount_in` is only bounded here. The direct path caps its transfer
/// the same way.
///
/// The bound is exact. No headroom is added on top of the throttle: begin and
/// end run in one transaction and read the same oracle prices, so there is no
/// price drift to absorb, and `end` bounds the exchange rate on its own with
/// `validate_swap_within_liquidation_boundaries`. Headroom here only raises
/// the collateral volume the liquidator can seize above the throttle. For the
/// same reason this uses the exact conversion: the round-to-whole-deposit form
/// would lift the bound to the user's entire deposit whenever the throttle
/// lands within one dollar of it.
fn bound_swap_amount(
    request: LiquidateSpotSwapBeginRequest,
    user: &User,
    maps: &AccountMaps,
    margin_calculation: &MarginCalculation,
    sides: (
        &SpotLiquidationSide,
        &SpotLiquidationSide,
        SwapLiabilityFees,
    ),
    terms: &LiquidationTerms,
) -> VelocityResult<u128> {
    let (asset, liability, liability_fees) = sides;
    let margin_shortage = margin_calculation.cross_margin_margin_shortage()?;
    let liability_weight_with_buffer = liability.weight.safe_add(terms.margin_buffer_ratio)?;

    // The borrow reduction the user receives in `liquidate_spot_with_swap_end`
    // is `liability_transfer - if_fee - protocol_fee`, so size the transfer
    // against the combined insurance-side fee. Using only `if_fee` here would
    // under-size the swap and leave the user with less margin relief than
    // intended (matches the combined fee `liquidate_spot` sizes with).
    let liability_total_if_side_fee = liability_fees
        .if_liquidation_fee
        .safe_add(liability_fees.protocol_liquidation_fee)?;

    // Determine what amount of borrow to transfer to reduce margin shortage to 0
    // assume 0 liquidator fee and swap is executed at oracle price.
    // valuation (shortage -> tokens) stays at the raw oracle price, consistent with
    // the margin calculation; only the exchange rate uses the protective price
    let liability_transfer_to_cover_margin_shortage =
        calculate_liability_transfer_to_cover_margin_shortage(
            margin_shortage,
            asset.weight,
            LIQUIDATION_FEE_PRECISION,
            liability_weight_with_buffer,
            LIQUIDATION_FEE_PRECISION,
            liability.decimals,
            liability.oracle_price,
            liability_total_if_side_fee,
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
        return Err(ErrorCode::InvalidLiquidation);
    }

    let max_asset_transfer = calculate_asset_transfer_for_liability_transfer_exact(
        LIQUIDATION_FEE_PRECISION,
        asset.decimals,
        asset.price,
        max_liability_allowed_to_be_transferred,
        LIQUIDATION_FEE_PRECISION,
        liability.decimals,
        liability.price,
    )?
    .min(asset.amount);

    if max_asset_transfer == 0 {
        msg!(
            "asset_market_index {} liability_market_index {}",
            request.asset_market_index,
            request.liability_market_index
        );
        msg!(
            "max_asset_transfer {} liability_transfer_to_cover_margin_shortage {}",
            max_asset_transfer,
            liability_transfer_to_cover_margin_shortage
        );
        msg!(
            "max_liability_allowed_to_be_transferred {} liability_transfer_to_cover_margin_shortage {}",
            max_liability_allowed_to_be_transferred,
            liability_transfer_to_cover_margin_shortage
        );
        msg!("swap_amount_in {}", request.swap_amount_in);
        return Err(ErrorCode::InvalidLiquidation);
    }

    Ok(max_asset_transfer)
}

/// Refuse a swap that takes out more than the bound or more than the deposit.
fn validate_swap_amount_in(
    request: LiquidateSpotSwapBeginRequest,
    max_asset_transfer: u128,
    asset_amount: u128,
) -> VelocityResult {
    validate!(
        max_asset_transfer >= request.swap_amount_in.cast()?,
        ErrorCode::InvalidLiquidation,
        "swap_amount_in larger than max_asset_transfer (swap_amount_in: {}, max_asset_transfer: {})",
        request.swap_amount_in,
        max_asset_transfer
    )?;

    validate!(
        asset_amount >= request.swap_amount_in.cast()?,
        ErrorCode::InvalidLiquidation,
        "swap_amount_in larger than asset_amount (swap_amount_in: {}, asset_amount: {})",
        request.swap_amount_in,
        asset_amount
    )
}

/// Refuse a swap priced off an oracle that has run too far from its five
/// minute TWAP.
fn validate_swap_oracles_within_bands(
    request: LiquidateSpotSwapBeginRequest,
    oracle_prices: (i64, i64),
    maps: &AccountMaps,
    state: &State,
) -> VelocityResult {
    let (asset_oracle_price, liability_oracle_price) = oracle_prices;
    let max_divergence = state
        .oracle_guard_rails
        .max_oracle_twap_5min_percent_divergence()
        .cast()?;

    let liability_oracle_too_divergent = is_oracle_too_divergent_with_twap_5min(
        liability_oracle_price.cast()?,
        maps.spot_market_map
            .get_ref(&request.liability_market_index)?
            .historical_oracle_data
            .last_oracle_price_twap_5min,
        max_divergence,
    )?;

    validate!(
        !liability_oracle_too_divergent,
        ErrorCode::PriceBandsBreached,
        "liability oracle too divergent"
    )?;

    let asset_oracle_too_divergent = is_oracle_too_divergent_with_twap_5min(
        asset_oracle_price.cast()?,
        maps.spot_market_map
            .get_ref(&request.asset_market_index)?
            .historical_oracle_data
            .last_oracle_price_twap_5min,
        max_divergence,
    )?;

    validate!(
        !asset_oracle_too_divergent,
        ErrorCode::PriceBandsBreached,
        "asset oracle too divergent"
    )
}

/// The insurance and protocol cuts of what the swap repaid.
///
/// Audit #51: the insurance-side fee is capped by the account's margin
/// shortage, exactly as the direct path does. Charging the raw rates on the
/// swap-realized borrow relief would route value into the fee pools that the
/// account needs to climb out of its shortage, delivering less borrow relief
/// than the direct path for an equivalent seizure. The basis is the
/// swap-realized `liability_transfer`, and the capped total is split IF-first
/// then protocol.
fn split_swap_end_fees(
    request: LiquidateSpotSwapEndRequest,
    margin_calculation: &MarginCalculation,
    sides: (
        &SpotLiquidationSide,
        &SpotLiquidationSide,
        SwapLiabilityFees,
    ),
    margin_buffer_ratio: u32,
) -> VelocityResult<(u128, u128)> {
    let (asset, liability, liability_fees) = sides;
    let margin_shortage = margin_calculation.cross_margin_margin_shortage()?;
    let liability_weight_with_buffer = liability.weight.safe_add(margin_buffer_ratio)?;

    let total_if_side_fee = calculate_spot_if_fee(
        margin_calculation.tracked_market_margin_shortage(margin_shortage)?,
        request.liability_transfer,
        asset.weight,
        asset.liquidation_multiplier,
        liability_weight_with_buffer,
        liability.liquidation_multiplier,
        liability.decimals,
        liability.price,
        liability_fees
            .if_liquidation_fee
            .safe_add(liability_fees.protocol_liquidation_fee)?,
    )?;

    let liquidation_if_fee = total_if_side_fee.min(liability_fees.if_liquidation_fee);
    let liquidation_protocol_fee = total_if_side_fee.safe_sub(liquidation_if_fee)?;

    Ok((
        fee_on_transfer(request.liability_transfer, liquidation_if_fee)?,
        fee_on_transfer(request.liability_transfer, liquidation_protocol_fee)?,
    ))
}

/// Book the borrow relief the swap bought, and the collateral it spent.
fn apply_swap_end_balances(
    request: LiquidateSpotSwapEndRequest,
    user: &mut User,
    maps: &AccountMaps,
    fees: (u128, u128),
) -> VelocityResult {
    let (if_fee, protocol_fee) = fees;

    {
        let mut liability_market = maps
            .spot_market_map
            .get_ref_mut(&request.liability_market_index)?;

        let user_liability_reduction = request
            .liability_transfer
            .safe_sub(if_fee)?
            .safe_sub(protocol_fee)?;
        update_spot_balances_and_cumulative_deposits(
            user_liability_reduction,
            &SpotBalanceType::Deposit,
            &mut liability_market,
            user.get_spot_position_mut(request.liability_market_index)?,
            false,
            Some(user_liability_reduction),
        )?;

        update_revenue_pool_balances(
            if_fee,
            &SpotBalanceType::Deposit,
            &mut liability_market,
            false,
        )?;
        update_protocol_fee_pool_balances(
            protocol_fee,
            &SpotBalanceType::Deposit,
            &mut liability_market,
            false,
        )?;
    }

    let mut asset_market = maps
        .spot_market_map
        .get_ref_mut(&request.asset_market_index)?;

    update_spot_balances_and_cumulative_deposits(
        request.asset_transfer,
        &SpotBalanceType::Borrow,
        &mut asset_market,
        user.force_get_spot_position_mut(request.asset_market_index)?,
        false,
        Some(request.asset_transfer),
    )
}
