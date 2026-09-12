//! Taking over negative perp pnl in exchange for the account's deposit.
//!
//! The liquidator absorbs the account's loss and takes collateral for it at a
//! discount. The position must be closed first, and the transfer must improve
//! the account, so a trade that only strips collateral is refused.

use super::*;

/// The account's unsettled loss, as the liquidator takes it on.
struct PerpPnlLiability {
    /// The unsettled loss, unsigned.
    unsettled_pnl: u128,
    quote_price: i64,
    quote_decimals: u32,
    /// The tier the market's pnl carries, which bounds what may be liquidated
    /// while riskier liabilities are open.
    contract_tier: ContractTier,
    /// The weight the margin calculation gives the loss.
    weight: u32,
    /// The liquidator's discount, as a multiplier.
    liquidation_multiplier: u32,
}

/// The account after the order cancels, when it is still liquidatable.
struct PnlCancelRecheck {
    margin_calculation: MarginCalculation,
    margin_freed: u64,
}

/// How much of the loss the liquidator takes, and the deposit that pays for
/// it.
struct DepositForPnlTransfer {
    pnl_transfer: u128,
    asset_transfer: u128,
    /// The pnl transfer that would clear the whole shortage.
    pnl_transfer_to_cover_margin_shortage: u128,
}

/// A pnl-for-deposit liquidation that passed its entry checks and holds the
/// latch.
struct OpenedPnlForDeposit {
    mode: Box<dyn LiquidatePerpMode>,
    asset: SpotLiquidationSide,
    pnl_liability: PerpPnlLiability,
    liquidation_id: u16,
    /// The picture the liquidation opened on, which the record reports.
    margin_calculation: MarginCalculation,
    /// The picture after the order cancels, which the transfer is sized on.
    intermediate_margin_calculation: MarginCalculation,
    margin_freed: u64,
    /// True while the market winds down at its expiry price.
    market_in_settlement: bool,
}

/// The tier of the safest liability the account still holds.
#[derive(Clone, Copy)]
struct SafestTiers {
    spot: AssetTier,
    perp: ContractTier,
}

pub fn liquidate_perp_pnl_for_deposit(
    request: LiquidatePerpPnlForDepositRequest,
    parties: &mut LiquidationParties,
    maps: &mut AccountMaps,
    terms: &LiquidationTerms,
    funding_paused: bool,
) -> VelocityResult {
    // liquidator takes over remaining negative perpetual pnl in exchange for a user deposit
    // can only be done once the perpetual position's size is 0
    // blocked when 1) user deposit oracle is deemed invalid
    // or 2) user has outstanding liability with higher tier
    let Some(opened) = open_pnl_for_deposit(request, parties, maps, terms, funding_paused)? else {
        return Ok(());
    };

    complete_pnl_for_deposit(request, parties, maps, terms, opened)
}

/// Run the entry checks, latch the account, cancel its orders and re-measure
/// it.
///
/// `None` reports that the liquidation is already finished: the cancels alone
/// cleared the shortage, the account never needed one, or a riskier liability
/// bars this market's pnl from being liquidated.
fn open_pnl_for_deposit(
    request: LiquidatePerpPnlForDepositRequest,
    parties: &mut LiquidationParties,
    maps: &mut AccountMaps,
    terms: &LiquidationTerms,
    funding_paused: bool,
) -> VelocityResult<Option<OpenedPnlForDeposit>> {
    let liquidation_mode = get_perp_liquidation_mode(parties.user, request.perp_market_index)?;

    let market_in_settlement =
        validate_preconditions(request, parties, maps, liquidation_mode.as_ref())?;
    settle_both_sides_funding(request.perp_market_index, parties, maps, terms.now)?;

    let asset = read_asset_side(
        parties.user,
        maps,
        (request.asset_market_index, liquidation_mode.as_ref()),
        terms.now,
        (terms.slot, funding_paused),
    )?;
    let pnl_liability = read_perp_pnl_liability(parties.user, maps, request.perp_market_index)?;

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        parties.user,
        maps,
        terms.margin_context(),
    )?;

    if check_perp_pnl_entry(parties.user, liquidation_mode.as_ref(), &margin_calculation)?
        == LiquidationEntry::Exited
    {
        return Ok(None);
    }

    let liquidation_id = liquidation_mode.enter_liquidation(parties.user, terms.slot)?;

    let (cancel_orders_market_type, cancel_orders_market_index) =
        liquidation_mode.get_cancel_orders_params();
    let canceled_order_ids = orders::cancel_orders(
        parties.user,
        parties.user_key,
        Some(parties.liquidator_key),
        maps,
        terms.now,
        terms.slot,
        OrderActionExplanation::Liquidation,
        cancel_orders_market_type,
        cancel_orders_market_index,
        None,
        true,
    )?;

    let safest_tiers = read_safest_tiers(parties.user, maps, liquidation_mode.as_ref())?;
    let is_contract_tier_violation = safest_tiers.bar_liquidating(&pnl_liability);

    let Some(recheck) = recheck_after_cancels(
        request,
        parties,
        maps,
        (liquidation_mode.as_ref(), terms),
        (
            &margin_calculation,
            canceled_order_ids,
            liquidation_id,
            asset.price,
        ),
        (is_contract_tier_violation, &pnl_liability, safest_tiers),
    )?
    else {
        return Ok(None);
    };

    if is_contract_tier_violation {
        log_tier_violation(&pnl_liability, safest_tiers, "");
        return Err(ErrorCode::TierViolationLiquidatingPerpPnl);
    }

    Ok(Some(OpenedPnlForDeposit {
        mode: liquidation_mode,
        asset,
        pnl_liability,
        liquidation_id,
        margin_calculation,
        intermediate_margin_calculation: recheck.margin_calculation,
        margin_freed: recheck.margin_freed,
        market_in_settlement,
    }))
}

/// Move the deposit and the loss, then close or re-latch the account.
fn complete_pnl_for_deposit(
    request: LiquidatePerpPnlForDepositRequest,
    parties: &mut LiquidationParties,
    maps: &mut AccountMaps,
    terms: &LiquidationTerms,
    opened: OpenedPnlForDeposit,
) -> VelocityResult {
    let margin_shortage = opened
        .mode
        .margin_shortage(&opened.intermediate_margin_calculation)?;
    validate_transfer_can_help(
        &opened.asset,
        &opened.pnl_liability,
        terms,
        opened.market_in_settlement,
    )?;

    let Some(transfer) = size_transfer(
        request,
        parties.user,
        maps,
        (&opened.asset, &opened.pnl_liability, opened.mode.as_ref()),
        (margin_shortage, terms),
    )?
    else {
        return Ok(());
    };

    validate_transfer_satisfies_limit_price(
        transfer.asset_transfer,
        transfer.pnl_transfer,
        opened.asset.decimals,
        opened.pnl_liability.quote_decimals,
        request.limit_price,
    )?;

    apply_transfer(request, parties, maps, &transfer, opened.mode.as_ref())?;

    let (margin_freed_from_liability, margin_calculation_after) = calculate_margin_freed(
        parties.user,
        maps,
        terms.margin_buffer_ratio,
        margin_shortage,
        Some(opened.mode.as_ref()),
    )?;

    validate_account_not_worsened(
        opened.mode.as_ref(),
        &margin_calculation_after,
        margin_shortage,
        opened.market_in_settlement,
    )?;

    let margin_freed = opened.margin_freed.safe_add(margin_freed_from_liability)?;
    opened
        .mode
        .increment_free_margin(parties.user, margin_freed_from_liability)?;

    close_pnl_for_deposit(
        request,
        parties,
        maps,
        &opened,
        (&transfer, margin_freed, terms.now),
    )
}

/// Release the latch when the transfer cleared the shortage, admit bankruptcy
/// when nothing is left to seize, and record what moved.
fn close_pnl_for_deposit(
    request: LiquidatePerpPnlForDepositRequest,
    parties: &mut LiquidationParties,
    maps: &mut AccountMaps,
    opened: &OpenedPnlForDeposit,
    outcome: (&DepositForPnlTransfer, u64, i64),
) -> VelocityResult {
    let (transfer, margin_freed, now) = outcome;

    if transfer.pnl_transfer >= transfer.pnl_transfer_to_cover_margin_shortage {
        opened.mode.exit_liquidation(parties.user)?;
    } else if opened
        .mode
        .should_user_enter_bankruptcy(parties.user, &maps.spot_market_map)?
    {
        opened.mode.enter_bankruptcy(parties.user)?;
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

    emit_liquidate_perp_pnl_record(
        parties,
        (opened.mode.as_ref(), &opened.margin_calculation),
        opened.liquidation_id,
        (margin_freed, vec![]),
        LiquidatePerpPnlForDepositRecord {
            perp_market_index: request.perp_market_index,
            market_oracle_price,
            pnl_transfer: transfer.pnl_transfer,
            asset_market_index: request.asset_market_index,
            asset_price: opened.asset.price,
            asset_transfer: transfer.asset_transfer,
        },
        now,
    )
}

/// Refuse a liquidation neither account nor market may take part in, and
/// report whether the perp market is winding down.
fn validate_preconditions(
    request: LiquidatePerpPnlForDepositRequest,
    parties: &mut LiquidationParties,
    maps: &AccountMaps,
    liquidation_mode: &dyn LiquidatePerpMode,
) -> VelocityResult<bool> {
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

    validate!(
        !maps
            .spot_market_map
            .get_ref(&request.asset_market_index)?
            .is_operation_paused(SpotOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        request.asset_market_index
    )?;

    let market_in_settlement = {
        let perp_market = maps.perp_market_map.get_ref(&request.perp_market_index)?;

        validate!(
            !perp_market.is_operation_paused(PerpOperation::Liquidation),
            ErrorCode::InvalidLiquidation,
            "Liquidation operation is paused for market {}",
            request.perp_market_index
        )?;

        // Audit #25 scoping: an expired/delisted market (Settlement) winds positions
        // down at the expiry price regardless of margin improvement, so the
        // "shortage must not grow" postcondition below is deliberately skipped there
        // — see the guard for the rationale.
        perp_market.status == MarketStatus::Settlement
    };

    validate_both_sides_have_positions(request, parties, liquidation_mode)?;

    Ok(market_in_settlement)
}

/// Refuse a transfer either side has nowhere to book.
fn validate_both_sides_have_positions(
    request: LiquidatePerpPnlForDepositRequest,
    parties: &mut LiquidationParties,
    liquidation_mode: &dyn LiquidatePerpMode,
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

    liquidation_mode.validate_spot_position(parties.user, request.asset_market_index)?;

    parties
        .liquidator
        .force_get_perp_position_mut(request.perp_market_index)
        .inspect_err(|_e| {
            msg!("Liquidator has no available positions to take on pnl");
        })?;

    parties
        .liquidator
        .force_get_spot_position_mut(request.asset_market_index)
        .inspect_err(|_e| {
            msg!("Liquidator has no available spot balances to take on deposit");
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

/// Read the deposit the liquidator takes collateral from.
fn read_asset_side(
    user: &User,
    maps: &mut AccountMaps,
    market: (u16, &dyn LiquidatePerpMode),
    now: i64,
    clock: (u64, bool),
) -> VelocityResult<SpotLiquidationSide> {
    let (market_index, liquidation_mode) = market;
    let (slot, funding_paused) = clock;
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

    // a margin-invalid (stale/uncertain) deposit oracle may make the account
    // liquidatable, but must not let its collateral be seized at a depressed
    // price: size the transfer at a user-protective price instead
    let token_price =
        if is_oracle_valid_for_action(asset_refresh.validity, Some(VelocityAction::MarginCalc))? {
            asset_price_data.price
        } else {
            calculate_user_protective_asset_price(
                asset_price_data,
                asset_refresh.pre_refresh_twap_5min,
            )?
        };

    Ok(SpotLiquidationSide {
        amount: liquidation_mode.get_spot_token_amount(user, &asset_market)?,
        oracle_price: asset_price_data.price,
        price: token_price,
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

/// Read the unsettled loss the liquidator takes on.
fn read_perp_pnl_liability(
    user: &User,
    maps: &mut AccountMaps,
    perp_market_index: u16,
) -> VelocityResult<PerpPnlLiability> {
    let user_position = user.get_perp_position(perp_market_index)?;

    let base_asset_amount = user_position.base_asset_amount;

    validate!(
        base_asset_amount == 0,
        ErrorCode::InvalidPerpPositionToLiquidate,
        "Cant have open perp position (base_asset_amount: {})",
        base_asset_amount
    )?;

    let unsettled_pnl = user_position.quote_asset_amount.cast::<i128>()?;

    validate!(
        unsettled_pnl < 0,
        ErrorCode::InvalidPerpPositionToLiquidate,
        "Perp position must have negative pnl"
    )?;

    let market = maps.perp_market_map.get_ref(&perp_market_index)?;

    let quote_spot_market = maps
        .spot_market_map
        .get_ref(&market.quote_spot_market_index)?;
    let quote_price = maps
        .oracle_map
        .get_price_data(&quote_spot_market.oracle_id())?
        .price;

    Ok(PerpPnlLiability {
        unsettled_pnl: unsettled_pnl.unsigned_abs(),
        quote_price,
        quote_decimals: 6,
        contract_tier: market.contract_tier,
        weight: SPOT_WEIGHT_PRECISION,
        liquidation_multiplier: calculate_liquidation_multiplier(
            market.liquidator_fee,
            LiquidationMultiplierType::Discount,
        )?,
    })
}

/// Decide whether the account still needs liquidating.
fn check_perp_pnl_entry(
    user: &mut User,
    liquidation_mode: &dyn LiquidatePerpMode,
    margin_calculation: &MarginCalculation,
) -> VelocityResult<LiquidationEntry> {
    let user_is_being_liquidated = liquidation_mode.user_is_being_liquidated(user)?;

    if !user_is_being_liquidated
        && liquidation_mode.meets_margin_requirements(margin_calculation)?
    {
        msg!("margin calculation {:?}", margin_calculation);
        return Err(ErrorCode::SufficientCollateral);
    } else if user_is_being_liquidated
        && liquidation_mode.can_exit_liquidation(margin_calculation)?
    {
        liquidation_mode.exit_liquidation(user)?;
        return Ok(LiquidationEntry::Exited);
    }

    Ok(LiquidationEntry::Proceed)
}

impl SafestTiers {
    /// True while a liability the account holds is safer than the pnl this
    /// call would liquidate.
    fn bar_liquidating(&self, pnl_liability: &PerpPnlLiability) -> bool {
        !pnl_liability
            .contract_tier
            .is_as_safe_as(&self.perp, &self.spot)
    }
}

/// The safest liability the account still holds on each side.
fn read_safest_tiers(
    user: &User,
    maps: &AccountMaps,
    liquidation_mode: &dyn LiquidatePerpMode,
) -> VelocityResult<SafestTiers> {
    let (spot, perp) = liquidation_mode.calculate_user_safest_position_tiers(
        user,
        &maps.perp_market_map,
        &maps.spot_market_map,
    )?;

    Ok(SafestTiers { spot, perp })
}

/// Re-measure the account after its orders are canceled.
///
/// `None` reports that the liquidation is over: either the cancels cleared the
/// shortage, or a riskier liability bars this market's pnl from being
/// liquidated. The record is emitted either way.
fn recheck_after_cancels(
    request: LiquidatePerpPnlForDepositRequest,
    parties: &mut LiquidationParties,
    maps: &mut AccountMaps,
    mode: (&dyn LiquidatePerpMode, &LiquidationTerms),
    entry: (&MarginCalculation, Vec<u32>, u16, i64),
    tiers: (bool, &PerpPnlLiability, SafestTiers),
) -> VelocityResult<Option<PnlCancelRecheck>> {
    let (liquidation_mode, terms) = mode;
    let (margin_calculation, canceled_order_ids, liquidation_id, asset_price) = entry;
    let (is_contract_tier_violation, pnl_liability, safest_tiers) = tiers;

    if canceled_order_ids.is_empty() {
        return Ok(Some(PnlCancelRecheck {
            margin_calculation: margin_calculation.clone(),
            margin_freed: 0,
        }));
    }

    let intermediate_margin_calculation =
        calculate_margin_requirement_and_total_collateral_and_liability_info(
            parties.user,
            maps,
            terms.margin_context(),
        )?;

    let margin_freed = liquidation_mode
        .margin_shortage(margin_calculation)?
        .saturating_sub(liquidation_mode.margin_shortage(&intermediate_margin_calculation)?)
        .cast::<u64>()?;
    liquidation_mode.increment_free_margin(parties.user, margin_freed)?;

    let exiting_liq_territory =
        liquidation_mode.can_exit_liquidation(&intermediate_margin_calculation)?;

    if !exiting_liq_territory && !is_contract_tier_violation {
        return Ok(Some(PnlCancelRecheck {
            margin_calculation: intermediate_margin_calculation,
            margin_freed,
        }));
    }

    let market_oracle_price = read_market_oracle_price(maps, request.perp_market_index)?;

    emit_liquidate_perp_pnl_record(
        parties,
        (liquidation_mode, margin_calculation),
        liquidation_id,
        (margin_freed, canceled_order_ids),
        LiquidatePerpPnlForDepositRecord {
            perp_market_index: request.perp_market_index,
            market_oracle_price,
            pnl_transfer: 0,
            asset_market_index: request.asset_market_index,
            asset_price,
            asset_transfer: 0,
        },
        terms.now,
    )?;

    if exiting_liq_territory {
        liquidation_mode.exit_liquidation(parties.user)?;
    } else if is_contract_tier_violation {
        log_tier_violation(
            pnl_liability,
            safest_tiers,
            "return early after cancel orders: ",
        );
    }

    Ok(None)
}

/// Report the tier that bars this market's pnl from being liquidated.
fn log_tier_violation(pnl_liability: &PerpPnlLiability, safest_tiers: SafestTiers, prefix: &str) {
    msg!(
        "{}liquidating contract tier={:?} pnl is riskier than outstanding {:?} & {:?}",
        prefix,
        pnl_liability.contract_tier,
        safest_tiers.perp,
        safest_tiers.spot
    );
}

/// Refuse a transfer that cannot improve the account at any size.
///
/// Audit #25: the account gives up deposit valued at the asset weight and
/// priced with the liquidator premium, and receives pnl relief valued at the
/// buffered liability weight and priced with the liquidator discount. The
/// margin improvement per unit transferred is therefore constant, and it is
/// positive only while the asset side stays below the liability side. When the
/// asset side reaches the liability side, every transfer size strips more
/// collateral than it frees, so no partial size helps and the call must
/// revert.
///
/// `calculate_liability_transfer_to_cover_margin_shortage` detects the same
/// condition, but reports it as `u128::MAX`. The sizing then reads that
/// sentinel as "no bound" and transfers the largest amount the other caps
/// allow.
///
/// `asset.weight` is the raw maintenance weight. A size-scaled (imf) weight is
/// never higher, so this check errs toward refusing a transfer that would in
/// fact help by a small amount.
///
/// Settlement is exempt: an expired market winds every position down at the
/// expiry price regardless of margin improvement.
fn validate_transfer_can_help(
    asset: &SpotLiquidationSide,
    pnl_liability: &PerpPnlLiability,
    terms: &LiquidationTerms,
    market_in_settlement: bool,
) -> VelocityResult {
    if market_in_settlement {
        return Ok(());
    }

    let pnl_liability_weight_plus_buffer =
        pnl_liability.weight.safe_add(terms.margin_buffer_ratio)?;

    // The extra factor of 10 mirrors the precision scaling in
    // `calculate_liability_transfer_to_cover_margin_shortage`.
    let asset_weight_component = asset
        .weight
        .cast::<u128>()?
        .safe_mul(10)?
        .safe_mul(asset.liquidation_multiplier.cast::<u128>()?)?
        .safe_div(pnl_liability.liquidation_multiplier.cast::<u128>()?)?;
    let pnl_liability_weight_component = pnl_liability_weight_plus_buffer
        .cast::<u128>()?
        .safe_mul(10)?;

    validate!(
        asset_weight_component < pnl_liability_weight_component,
        ErrorCode::LiquidationWorsensAccountHealth,
        "liquidate_perp_pnl_for_deposit cannot improve account health (asset weight component {} >= liability weight component {})",
        asset_weight_component,
        pnl_liability_weight_component
    )
}

/// Size the loss the liquidator takes and the deposit that pays for it.
///
/// `None` reports that the time ramp allows nothing yet, which is not an
/// error: a later call moves what this one may not.
fn size_transfer(
    request: LiquidatePerpPnlForDepositRequest,
    user: &User,
    maps: &AccountMaps,
    sides: (
        &SpotLiquidationSide,
        &PerpPnlLiability,
        &dyn LiquidatePerpMode,
    ),
    shortage: (u128, &LiquidationTerms),
) -> VelocityResult<Option<DepositForPnlTransfer>> {
    let (asset, pnl_liability, liquidation_mode) = sides;
    let (margin_shortage, terms) = shortage;
    let pnl_liability_weight_plus_buffer =
        pnl_liability.weight.safe_add(terms.margin_buffer_ratio)?;

    // Determine what amount of borrow to transfer to reduce margin shortage to 0
    let pnl_transfer_to_cover_margin_shortage =
        calculate_liability_transfer_to_cover_margin_shortage(
            margin_shortage,
            asset.weight,
            asset.liquidation_multiplier,
            pnl_liability_weight_plus_buffer,
            pnl_liability.liquidation_multiplier,
            pnl_liability.quote_decimals,
            pnl_liability.quote_price,
            0, // no if fee
        )?;

    let max_pct_allowed = liquidation_mode.calculate_max_pct_to_liquidate(
        user,
        margin_shortage,
        terms.slot,
        terms.initial_pct_to_liquidate,
        terms.duration,
        maps.oracle_map.slot_clock,
    )?;
    let max_pnl_allowed_to_be_transferred = pnl_transfer_to_cover_margin_shortage
        .saturating_mul(max_pct_allowed)
        .safe_div(LIQUIDATION_PCT_PRECISION)?;

    if max_pnl_allowed_to_be_transferred == 0 {
        msg!("max_pnl_allowed_to_be_transferred == 0");
        return Ok(None);
    }

    // Given the user's deposit amount, how much borrow can be transferred?
    let pnl_transfer_implied_by_asset_amount =
        calculate_liability_transfer_implied_by_asset_amount(
            asset.amount,
            asset.liquidation_multiplier,
            asset.decimals,
            asset.price,
            pnl_liability.liquidation_multiplier,
            pnl_liability.quote_decimals,
            pnl_liability.quote_price,
        )?;

    let minimum_pnl_transfer = if pnl_liability.unsettled_pnl > 10 * QUOTE_PRECISION {
        0_u128
    } else {
        pnl_liability.unsettled_pnl
    };

    let pnl_transfer = request
        .liquidator_max_pnl_transfer
        .min(pnl_liability.unsettled_pnl)
        // want to make sure the pnl_transfer_to_cover_margin_shortage doesn't lead to dust pnl
        .min(max_pnl_allowed_to_be_transferred.max(minimum_pnl_transfer))
        .min(pnl_transfer_implied_by_asset_amount);

    let asset_transfer = asset_transfer_for(
        asset,
        pnl_liability,
        pnl_transfer,
        pnl_transfer_implied_by_asset_amount,
    )?;

    if asset_transfer == 0 || pnl_transfer == 0 {
        log_empty_transfer(
            request,
            pnl_liability.unsettled_pnl,
            pnl_transfer_to_cover_margin_shortage,
            pnl_transfer_implied_by_asset_amount,
            (pnl_transfer, asset_transfer),
        );
        return Err(ErrorCode::InvalidLiquidation);
    }

    Ok(Some(DepositForPnlTransfer {
        pnl_transfer,
        asset_transfer,
        pnl_transfer_to_cover_margin_shortage,
    }))
}

/// The deposit that pays for one pnl transfer.
///
/// Audit #25: every unit seized must be paid for, so this path does not use
/// the round-to-whole-deposit form of the conversion. That form takes up to
/// one dollar of collateral the pnl relief does not cover, which is real
/// value, not rounding.
///
/// The whole deposit still goes when the deposit is what limited the transfer:
/// `pnl_transfer_implied_by_asset_amount` is the pnl the whole deposit buys,
/// and it rounds up, so charging the whole deposit for it never overcharges.
/// The only gap is the base-unit truncation of the two inverse conversions,
/// and taking the deposit to zero avoids stranding that dust in the position.
///
/// The exact form can exceed the deposit by a unit or two through the same
/// truncation, so it is clamped.
fn asset_transfer_for(
    asset: &SpotLiquidationSide,
    pnl_liability: &PerpPnlLiability,
    pnl_transfer: u128,
    pnl_transfer_implied_by_asset_amount: u128,
) -> VelocityResult<u128> {
    if pnl_transfer == pnl_transfer_implied_by_asset_amount {
        return Ok(asset.amount);
    }

    Ok(calculate_asset_transfer_for_liability_transfer_exact(
        asset.liquidation_multiplier,
        asset.decimals,
        asset.price,
        pnl_transfer,
        pnl_liability.liquidation_multiplier,
        pnl_liability.quote_decimals,
        pnl_liability.quote_price,
    )?
    .min(asset.amount))
}

/// Report the inputs that produced a transfer of nothing.
fn log_empty_transfer(
    request: LiquidatePerpPnlForDepositRequest,
    unsettled_pnl: u128,
    pnl_transfer_to_cover_margin_shortage: u128,
    pnl_transfer_implied_by_asset_amount: u128,
    transfers: (u128, u128),
) {
    let (pnl_transfer, asset_transfer) = transfers;
    msg!(
        "asset_market_index {} perp_market_index {}",
        request.asset_market_index,
        request.perp_market_index
    );
    msg!(
        "liquidator_max_pnl_transfer {} unsettled_pnl {} pnl_transfer_to_cover_margin_shortage {}",
        request.liquidator_max_pnl_transfer,
        unsettled_pnl,
        pnl_transfer_to_cover_margin_shortage
    );
    msg!(
        "pnl_transfer_implied_by_asset_amount {} pnl_transfer {} asset_transfer {}",
        pnl_transfer_implied_by_asset_amount,
        pnl_transfer,
        asset_transfer
    );
}

/// Move the deposit to the liquidator and the loss to the account that took it
/// on.
fn apply_transfer(
    request: LiquidatePerpPnlForDepositRequest,
    parties: &mut LiquidationParties,
    maps: &AccountMaps,
    transfer: &DepositForPnlTransfer,
    liquidation_mode: &dyn LiquidatePerpMode,
) -> VelocityResult {
    {
        let mut asset_market = maps
            .spot_market_map
            .get_ref_mut(&request.asset_market_index)?;

        update_spot_balances_and_cumulative_deposits(
            transfer.asset_transfer,
            &SpotBalanceType::Deposit,
            &mut asset_market,
            parties
                .liquidator
                .get_spot_position_mut(request.asset_market_index)?,
            false,
            Some(transfer.asset_transfer),
        )?;

        liquidation_mode.decrease_spot_token_amount(
            parties.user,
            transfer.asset_transfer,
            &mut asset_market,
            Some(transfer.asset_transfer),
        )?;
    }

    let mut perp_market = maps
        .perp_market_map
        .get_ref_mut(&request.perp_market_index)?;
    let liquidator_position = parties
        .liquidator
        .force_get_perp_position_mut(request.perp_market_index)?;
    update_quote_asset_amount(
        liquidator_position,
        &mut perp_market,
        -transfer.pnl_transfer.cast()?,
    )?;

    let user_position = parties
        .user
        .get_perp_position_mut(request.perp_market_index)?;
    update_quote_asset_amount(
        user_position,
        &mut perp_market,
        transfer.pnl_transfer.cast()?,
    )
}

/// Refuse a transfer that grew the account's buffered shortage.
///
/// Audit #25: this path must never worsen the account. The weight check
/// rejects the market parameters that make the transfer loss-making at every
/// size, and `calculate_margin_freed` saturates a negative improvement to
/// zero, so this is the backstop for anything the sizing math does not model.
///
/// The check is exact. It holds no tolerance, because the seizure above pays
/// for every unit it takes. A tolerance here would let a liquidator size each
/// transfer to degrade the account by just under it and repeat the call until
/// the deposit is gone.
///
/// Settlement is exempt: an expired market winds every position down at the
/// expiry price and this path clears the residual expired pnl into the
/// liquidator, which legitimately drives the account to bankruptcy. There is
/// no live risk left to protect, so the check must not block the wind-down.
fn validate_account_not_worsened(
    liquidation_mode: &dyn LiquidatePerpMode,
    margin_calculation_after: &MarginCalculation,
    margin_shortage: u128,
    market_in_settlement: bool,
) -> VelocityResult {
    if market_in_settlement {
        return Ok(());
    }

    let new_margin_shortage = liquidation_mode.margin_shortage(margin_calculation_after)?;
    validate!(
        new_margin_shortage <= margin_shortage,
        ErrorCode::LiquidationWorsensAccountHealth,
        "liquidate_perp_pnl_for_deposit would grow margin shortage ({} -> {}); refusing to worsen account health",
        margin_shortage,
        new_margin_shortage
    )
}

/// The perp market's oracle price, as the record reports it.
fn read_market_oracle_price(maps: &mut AccountMaps, perp_market_index: u16) -> VelocityResult<i64> {
    let market = maps.perp_market_map.get_ref(&perp_market_index)?;
    Ok(maps.oracle_map.get_price_data(&market.oracle_id())?.price)
}

/// Emit one `LiquidatePerpPnlForDeposit` record.
fn emit_liquidate_perp_pnl_record(
    parties: &LiquidationParties,
    mode: (&dyn LiquidatePerpMode, &MarginCalculation),
    liquidation_id: u16,
    outcome: (u64, Vec<u32>),
    liquidate_perp_pnl_for_deposit: LiquidatePerpPnlForDepositRecord,
    now: i64,
) -> VelocityResult {
    let (liquidation_mode, margin_calculation) = mode;
    let (margin_freed, canceled_order_ids) = outcome;
    let (margin_requirement, total_collateral, bit_flags) =
        liquidation_mode.get_event_fields(margin_calculation)?;

    emit!(LiquidationRecord {
        ts: now,
        liquidation_id,
        liquidation_type: LiquidationType::LiquidatePerpPnlForDeposit,
        user: *parties.user_key,
        liquidator: *parties.liquidator_key,
        margin_requirement,
        total_collateral,
        bankrupt: liquidation_mode.is_user_bankrupt(parties.user)?,
        canceled_order_ids,
        margin_freed,
        liquidate_perp_pnl_for_deposit,
        bit_flags,
        ..LiquidationRecord::default()
    });

    Ok(())
}
