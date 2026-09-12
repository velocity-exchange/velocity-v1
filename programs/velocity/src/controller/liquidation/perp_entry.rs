//! The opening steps both perp liquidation paths run.
//!
//! [`super::perp`] hands the position to a liquidator and [`super::perp_fill`]
//! sells it to the book, but they reach the transfer the same way: they refuse
//! a market that may not be liquidated, decide whether the account is a
//! candidate, cancel its orders and re-measure it, refresh the market's AMM,
//! and size the transfer against the margin shortage. Those steps live here so
//! the two paths cannot drift apart.

use super::*;

/// Who a perp liquidation names, before it has an id or a price.
#[derive(Clone, Copy)]
pub(crate) struct PerpLiquidationTarget<'a> {
    pub user_key: &'a Pubkey,
    pub liquidator_key: &'a Pubkey,
    pub market_index: u16,
}

/// The constants that identify one perp liquidation while it runs.
#[derive(Clone, Copy)]
pub(crate) struct PerpLiquidationRun<'a> {
    pub user_key: &'a Pubkey,
    pub liquidator_key: &'a Pubkey,
    pub market_index: u16,
    pub liquidation_id: u16,
    /// The price the transfer values the position at.
    pub oracle_price: i64,
    pub terms: LiquidationTerms,
}

/// A perp liquidation that passed its entry checks and holds the latch.
pub(crate) struct OpenedPerpLiquidation<'a> {
    pub mode: Box<dyn LiquidatePerpMode>,
    pub run: PerpLiquidationRun<'a>,
    /// The picture the liquidation opened on, which the record reports.
    pub margin_calculation: MarginCalculation,
    /// The picture after the order cancels, which the transfer is sized on.
    pub intermediate_margin_calculation: MarginCalculation,
    pub canceled_order_ids: Vec<u32>,
    pub margin_freed: u64,
}

/// What one `LiquidatePerp` record reports beyond the run's identity.
pub(crate) struct PerpLiquidationOutcome {
    pub canceled_order_ids: Vec<u32>,
    pub margin_freed: u64,
    pub liquidate_perp: LiquidatePerpRecord,
}

/// The account after the order cancels, when it is still liquidatable.
pub(crate) struct PerpCancelRecheck {
    pub margin_calculation: MarginCalculation,
    pub margin_freed: u64,
    pub canceled_order_ids: Vec<u32>,
}

/// Refuse a perp market that must not be liquidated right now.
pub(crate) fn validate_perp_market_liquidatable(market: &PerpMarket, now: i64) -> VelocityResult {
    let market_index = market.market_index;

    validate!(
        !market.is_operation_paused(PerpOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        market_index
    )?;

    // OtterSec #149: once `expiry_ts` passes, an expired perp position must only be
    // closed out at the market's committed `expiry_price`, never at the live oracle.
    //
    // Every ordinary user path already refuses past expiry via the same
    // `is_in_settlement(now)` predicate — placing, filling, triggering, transferring and
    // settling all gate on it — but direct permissionless liquidation did not, so it kept
    // valuing and transferring the position at the live oracle for the whole window
    // between `expiry_ts` and a warm admin flipping the status to `Settlement`. A
    // liquidator could take the position at a live price that the fixed settlement price
    // then supersedes, while the owner had no way to act.
    //
    // Scoped precisely to that window. Deliberately NOT `is_in_settlement(now)`, which is
    // also true once the status *is* `Settlement`/`Delisted` — by then `expiry_price` is
    // committed and liquidation during the wind-down is a legitimate way to resolve bad
    // debt (the delisting tests exercise exactly that). What must be refused is only the
    // gap where the market has expired but no settlement price exists yet.
    let expired_awaiting_settlement = market.expiry_ts != 0
        && now >= market.expiry_ts
        && !matches!(
            market.status,
            MarketStatus::Settlement | MarketStatus::Delisted
        );

    validate!(
        !expired_awaiting_settlement,
        ErrorCode::InvalidLiquidation,
        "market {} expired at {} but has no committed expiry price yet; \
         settle_expired_market must run first",
        market_index,
        market.expiry_ts
    )?;

    Ok(())
}

/// Decide whether a perp account still needs liquidating.
pub(crate) fn check_perp_liquidation_entry(
    user: &mut User,
    liquidation_mode: &dyn LiquidatePerpMode,
    margin_calculation: &MarginCalculation,
) -> VelocityResult<LiquidationEntry> {
    let user_is_being_liquidated = liquidation_mode.user_is_being_liquidated(user)?;

    // A CLOB-resident order reserves open_bids/open_asks that inflate the
    // worst-case margin above, but the DLOB cancel below cannot remove it, so
    // the intermediate re-check never runs and a solvent position is
    // liquidated on the inflated figure. Refuse a fresh liquidation until the
    // book orders are reclaimed (force_cancel_clob_orders un-reserves them). An
    // account already in liquidation is not blocked — the entry that inflation
    // could have caused already happened.
    if !user_is_being_liquidated {
        if let Some(clob_market) = user.first_market_with_clob_resident_orders() {
            msg!(
                "user has resting CLOB orders in market {}; force_cancel_clob_orders must run first",
                clob_market
            );
            return Err(ErrorCode::LiquidationConflictsWithClobOrders);
        }
    }

    if !user_is_being_liquidated
        && liquidation_mode.meets_margin_requirements(margin_calculation)?
    {
        msg!("margin calculation: {:?}", margin_calculation);
        return Err(ErrorCode::SufficientCollateral);
    } else if user_is_being_liquidated
        && liquidation_mode.can_exit_liquidation(margin_calculation)?
    {
        liquidation_mode.exit_liquidation(user)?;
        return Ok(LiquidationEntry::Exited);
    }

    Ok(LiquidationEntry::Proceed)
}

/// Refresh the market's AMM and return the price the liquidation values at.
///
/// A market in settlement values at its committed expiry price. Every other
/// market values at the live oracle.
pub(crate) fn refresh_amm_and_read_price(
    maps: &mut AccountMaps,
    market_index: u16,
    state: &State,
    now: i64,
    slot: u64,
) -> VelocityResult<i64> {
    let mut market = maps.perp_market_map.get_ref_mut(&market_index)?;
    let oracle_price_data = maps.oracle_map.get_price_data(&market.oracle_id())?;
    let mm_oracle_price_data = market.get_mm_oracle_price_data(
        *oracle_price_data,
        slot,
        &state.oracle_guard_rails.validity,
        state.slot_clock(),
    )?;

    update_amm_and_check_validity(
        &mut market,
        &mm_oracle_price_data,
        state,
        now,
        slot,
        Some(VelocityAction::Liquidate),
    )?;

    if market.status == MarketStatus::Settlement {
        Ok(market.expiry_price)
    } else {
        Ok(oracle_price_data.price)
    }
}

/// Latch the account, cancel its orders, refresh the market and re-measure.
///
/// `None` reports that the cancels alone cleared the shortage, so the caller
/// returns without moving value.
pub(crate) fn latch_cancel_and_recheck<'a>(
    user: &mut User,
    target: PerpLiquidationTarget<'a>,
    liquidation_mode: &dyn LiquidatePerpMode,
    maps: &mut AccountMaps,
    margin_calculation: &MarginCalculation,
    context: (&LiquidationTerms, &State),
) -> VelocityResult<Option<(PerpLiquidationRun<'a>, PerpCancelRecheck)>> {
    let (terms, state) = context;
    let market_index = target.market_index;

    let liquidation_id = liquidation_mode.enter_liquidation(user, terms.slot)?;

    let position_index = get_position_index(&user.perp_positions, market_index)?;
    validate!(
        user.perp_positions[position_index].is_open_position()
            || user.perp_positions[position_index].has_open_order(),
        ErrorCode::PositionDoesntHaveOpenPositionOrOrders
    )?;

    let (cancel_orders_market_type, cancel_orders_market_index) =
        liquidation_mode.get_cancel_orders_params();
    let canceled_order_ids = orders::cancel_orders(
        user,
        target.user_key,
        Some(target.liquidator_key),
        maps,
        terms.now,
        terms.slot,
        OrderActionExplanation::Liquidation,
        cancel_orders_market_type,
        cancel_orders_market_index,
        None,
        true,
    )?;

    let run = PerpLiquidationRun {
        user_key: target.user_key,
        liquidator_key: target.liquidator_key,
        market_index,
        liquidation_id,
        oracle_price: refresh_amm_and_read_price(maps, market_index, state, terms.now, terms.slot)?,
        terms: *terms,
    };

    let recheck = recheck_after_cancels(
        user,
        maps,
        liquidation_mode,
        &run,
        margin_calculation,
        canceled_order_ids,
    )?;

    Ok(recheck.map(|recheck| (run, recheck)))
}

/// Re-measure the account after its orders are canceled.
///
/// The cancels release the margin the orders reserved, which can be enough on
/// its own. `None` reports that they were: the record is emitted, the latch is
/// released, and the caller returns without moving value.
pub(crate) fn recheck_after_cancels(
    user: &mut User,
    maps: &mut AccountMaps,
    liquidation_mode: &dyn LiquidatePerpMode,
    run: &PerpLiquidationRun,
    margin_calculation: &MarginCalculation,
    canceled_order_ids: Vec<u32>,
) -> VelocityResult<Option<PerpCancelRecheck>> {
    if canceled_order_ids.is_empty() {
        return Ok(Some(PerpCancelRecheck {
            margin_calculation: margin_calculation.clone(),
            margin_freed: 0,
            canceled_order_ids,
        }));
    }

    let intermediate_margin_calculation =
        calculate_margin_requirement_and_total_collateral_and_liability_info(
            user,
            maps,
            run.terms
                .margin_context_tracking(MarketIdentifier::perp(run.market_index))?,
        )?;

    let initial_margin_shortage = liquidation_mode.margin_shortage(margin_calculation)?;
    let new_margin_shortage = liquidation_mode.margin_shortage(&intermediate_margin_calculation)?;

    let margin_freed = initial_margin_shortage
        .saturating_sub(new_margin_shortage)
        .cast::<u64>()?;
    liquidation_mode.increment_free_margin(user, margin_freed)?;

    if liquidation_mode.can_exit_liquidation(&intermediate_margin_calculation)? {
        emit_liquidate_perp_record(
            user,
            liquidation_mode,
            run,
            margin_calculation,
            PerpLiquidationOutcome {
                canceled_order_ids,
                margin_freed,
                liquidate_perp: LiquidatePerpRecord {
                    market_index: run.market_index,
                    oracle_price: run.oracle_price,
                    ..LiquidatePerpRecord::default()
                },
            },
        )?;

        liquidation_mode.exit_liquidation(user)?;
        return Ok(None);
    }

    Ok(Some(PerpCancelRecheck {
        margin_calculation: intermediate_margin_calculation,
        margin_freed,
        canceled_order_ids,
    }))
}

/// Emit one `LiquidatePerp` record.
///
/// `margin_calculation` is the picture the liquidation opened on, so the
/// record reports the shortage that justified it rather than the one it left.
pub(crate) fn emit_liquidate_perp_record(
    user: &User,
    liquidation_mode: &dyn LiquidatePerpMode,
    run: &PerpLiquidationRun,
    margin_calculation: &MarginCalculation,
    outcome: PerpLiquidationOutcome,
) -> VelocityResult {
    let (margin_requirement, total_collateral, bit_flags) =
        liquidation_mode.get_event_fields(margin_calculation)?;

    emit!(LiquidationRecord {
        ts: run.terms.now,
        liquidation_id: run.liquidation_id,
        liquidation_type: LiquidationType::LiquidatePerp,
        user: *run.user_key,
        liquidator: *run.liquidator_key,
        margin_requirement,
        total_collateral,
        bankrupt: liquidation_mode.is_user_bankrupt(user)?,
        canceled_order_ids: outcome.canceled_order_ids,
        margin_freed: outcome.margin_freed,
        liquidate_perp: outcome.liquidate_perp,
        bit_flags,
        ..LiquidationRecord::default()
    });

    Ok(())
}

/// The fee rates a perp liquidation charges, and the size that clears the
/// whole margin shortage.
pub(crate) struct PerpLiquidationSizing {
    /// The position the account holds, unsigned.
    pub user_base_asset_amount: u64,
    /// The liquidator's execution discount, ramped by the grace period.
    pub liquidator_fee: u32,
    /// The insurance fund's share of the insurance-side fee.
    pub if_liquidation_fee: u32,
    /// The protocol's share of the insurance-side fee.
    pub protocol_liquidation_fee: u32,
    /// The base amount that clears the whole shortage, before the step size is
    /// applied. `u64::MAX` reports that no size clears it.
    pub base_asset_amount_to_cover_margin_shortage: u64,
}

impl PerpLiquidationSizing {
    /// Size one perp liquidation against the account's margin shortage.
    ///
    /// The insurance-side budget is computed once with the cap raised to
    /// `if_liquidation_fee + protocol_liquidation_fee`, then split IF-first:
    /// the insurance fund receives exactly what it would have without the
    /// protocol fee, and the protocol only captures margin headroom beyond
    /// that. This keeps the combined fee inside the margin budget, so the
    /// protocol fee can never push a liquidation into spurious bankruptcy.
    ///
    /// The time-adjusted liquidator fee is the basis for both the
    /// insurance-side budget and the base sizing, so it matches the fee the
    /// transfer is actually priced with. Sizing against the un-aged market fee
    /// would under-budget the insurance and protocol fees relative to the
    /// larger execution discount the account pays after the grace period.
    pub fn calculate(
        user: &User,
        maps: &mut AccountMaps,
        run: &PerpLiquidationRun,
        margin_calculation: &MarginCalculation,
        margin_shortage: u128,
    ) -> VelocityResult<Self> {
        let slot_clock = maps.oracle_map.slot_clock;
        let position_index = get_position_index(&user.perp_positions, run.market_index)?;
        let user_base_asset_amount = user.perp_positions[position_index]
            .base_asset_amount
            .unsigned_abs();

        let margin_ratio = maps
            .perp_market_map
            .get_ref(&run.market_index)?
            .get_margin_ratio(
                user_base_asset_amount.cast()?,
                MarginRequirementType::Maintenance,
            )?;

        let margin_ratio_with_buffer = margin_ratio.safe_add(run.terms.margin_buffer_ratio)?;

        let market = maps.perp_market_map.get_ref(&run.market_index)?;
        let quote_spot_market = maps
            .spot_market_map
            .get_ref(&market.quote_spot_market_index)?;
        let quote_oracle_price = maps
            .oracle_map
            .get_price_data(&quote_spot_market.oracle_id())?
            .price;

        let liquidator_fee = get_liquidation_fee(
            market.get_base_liquidator_fee(),
            market.get_max_liquidation_fee()?,
            user.last_active_slot,
            run.terms.slot,
            slot_clock,
        )?;

        let total_if_side_fee = calculate_perp_if_fee(
            margin_calculation.tracked_market_margin_shortage(margin_shortage)?,
            user_base_asset_amount,
            margin_ratio_with_buffer,
            liquidator_fee,
            run.oracle_price,
            quote_oracle_price,
            market
                .if_liquidation_fee
                .safe_add(market.protocol_liquidation_fee)?,
        )?;
        let if_liquidation_fee = total_if_side_fee.min(market.if_liquidation_fee);
        let protocol_liquidation_fee = total_if_side_fee.safe_sub(if_liquidation_fee)?;

        let base_asset_amount_to_cover_margin_shortage =
            calculate_base_asset_amount_to_cover_margin_shortage(
                margin_shortage,
                margin_ratio_with_buffer,
                liquidator_fee,
                total_if_side_fee,
                run.oracle_price,
                quote_oracle_price,
            )?;

        Ok(Self {
            user_base_asset_amount,
            liquidator_fee,
            if_liquidation_fee,
            protocol_liquidation_fee,
            base_asset_amount_to_cover_margin_shortage,
        })
    }
}

/// The largest base amount the time ramp lets this call move.
pub(crate) fn max_base_asset_amount_allowed_to_be_transferred(
    user: &User,
    liquidation_mode: &dyn LiquidatePerpMode,
    maps: &AccountMaps,
    terms: &LiquidationTerms,
    margin_shortage: u128,
    base_asset_amount_to_cover_margin_shortage: u64,
) -> VelocityResult<u64> {
    let max_pct_allowed = liquidation_mode.calculate_max_pct_to_liquidate(
        user,
        margin_shortage,
        terms.slot,
        terms.initial_pct_to_liquidate,
        terms.duration,
        maps.oracle_map.slot_clock,
    )?;

    base_asset_amount_to_cover_margin_shortage
        .cast::<u128>()?
        .saturating_mul(max_pct_allowed)
        .safe_div(LIQUIDATION_PCT_PRECISION)?
        .cast::<u64>()
}

/// Refuse a price that has run too far from the five minute TWAP.
pub(crate) fn validate_oracle_within_twap_band(
    maps: &AccountMaps,
    market_index: u16,
    oracle_price: i64,
    state: &State,
) -> VelocityResult {
    let oracle_price_too_divergent = is_oracle_too_divergent_with_twap_5min(
        oracle_price,
        maps.perp_market_map
            .get_ref(&market_index)?
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min,
        state
            .oracle_guard_rails
            .max_oracle_twap_5min_percent_divergence()
            .cast()?,
    )?;

    validate!(!oracle_price_too_divergent, ErrorCode::PriceBandsBreached)
}

/// One fee rate applied to a quote value, as a debit to the account.
pub(crate) fn fee_debit(quote_asset_amount: u64, rate: u32) -> VelocityResult<i64> {
    Ok(-quote_asset_amount
        .cast::<u128>()?
        .safe_mul(rate.cast()?)?
        .safe_div(LIQUIDATION_FEE_PRECISION_U128)?
        .cast::<i64>()?)
}

/// The smallest transfer that must not leave dust behind.
///
/// A position worth more than fifty dollars may be reduced in part. Anything
/// smaller is taken whole, because a remainder that small is not worth another
/// call.
pub(crate) fn minimum_base_asset_amount(
    user_base_asset_amount: u64,
    oracle_price: i64,
) -> VelocityResult<u64> {
    let base_asset_value =
        calculate_base_asset_value_with_oracle_price(user_base_asset_amount.cast()?, oracle_price)?
            .cast::<u64>()?;

    if base_asset_value > 50 * QUOTE_PRECISION_U64 {
        Ok(0)
    } else {
        Ok(user_base_asset_amount)
    }
}
