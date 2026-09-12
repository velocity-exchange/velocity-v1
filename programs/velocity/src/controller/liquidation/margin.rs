//! The margin arithmetic every liquidation path repeats.
//!
//! A liquidation opens on a margin shortage and closes when the shortage is
//! gone, so each path measures the account three times: once to decide whether
//! it may act, once after it cancels the account's orders, and once after it
//! moves value. This module holds those measurements, and the keeper entry
//! point that only latches the account without moving anything.

use super::*;

/// What a margin check decided about an account a liquidation is about to
/// touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LiquidationEntry {
    /// The account is liquidatable. The caller continues.
    Proceed,
    /// The account no longer needs liquidation, and the latch is cleared. The
    /// caller returns without moving value.
    Exited,
}

/// Decide whether a cross-margin account still needs liquidating.
///
/// An account that meets its requirement and is not already latched is not a
/// liquidation candidate at all, which is an error. An account that is latched
/// and has climbed back above the buffered requirement leaves the latch here.
pub(crate) fn check_cross_margin_entry(
    user: &mut User,
    margin_calculation: &MarginCalculation,
) -> VelocityResult<LiquidationEntry> {
    let being_liquidated = user.is_cross_margin_being_liquidated();

    if !being_liquidated && margin_calculation.meets_cross_margin_requirement() {
        msg!("margin calculation: {:?}", margin_calculation);
        return Err(ErrorCode::SufficientCollateral);
    }

    if being_liquidated && margin_calculation.can_exit_cross_margin_liquidation()? {
        user.exit_cross_margin_liquidation();
        return Ok(LiquidationEntry::Exited);
    }

    Ok(LiquidationEntry::Proceed)
}

/// Re-measure a cross-margin account after its orders are canceled, and credit
/// the shortage the cancels closed.
///
/// Canceling the orders releases the margin they reserved, which can be enough
/// on its own. The caller reads the returned calculation to find out.
pub(crate) fn recheck_cross_margin_after_cancels(
    user: &mut User,
    maps: &mut AccountMaps,
    margin_context: MarginContext,
    initial_margin_shortage: u128,
) -> VelocityResult<(u64, MarginCalculation)> {
    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        maps,
        margin_context,
    )?;

    let margin_freed = initial_margin_shortage
        .saturating_sub(margin_calculation.cross_margin_margin_shortage()?)
        .cast::<u64>()?;
    user.increment_margin_freed(margin_freed)?;

    Ok((margin_freed, margin_calculation))
}

/// The shortage a completed transfer closed, and the margin picture it left.
pub fn calculate_margin_freed(
    user: &User,
    maps: &mut AccountMaps,
    liquidation_margin_buffer_ratio: u32,
    initial_margin_shortage: u128,
    liquidation_mode: Option<&dyn LiquidatePerpMode>,
) -> VelocityResult<(u64, MarginCalculation)> {
    let margin_calculation_after =
        calculate_margin_requirement_and_total_collateral_and_liability_info(
            user,
            maps,
            MarginContext::liquidation(liquidation_margin_buffer_ratio),
        )?;

    let new_margin_shortage = if let Some(liquidation_mode) = liquidation_mode {
        liquidation_mode.margin_shortage(&margin_calculation_after)?
    } else {
        margin_calculation_after.cross_margin_margin_shortage()?
    };

    let margin_freed = initial_margin_shortage
        .saturating_sub(new_margin_shortage)
        .cast::<u64>()?;

    Ok((margin_freed, margin_calculation_after))
}

/// Refuse a liquidator that cannot carry the risk it takes on.
///
/// The liquidation adds exposure to the liquidator like a risk-increasing
/// fill, so the liquidator subaccount must meet its initial requirement and
/// clear its own buffered equity floor.
pub(crate) fn validate_liquidator_takes_on_risk(
    liquidator: &User,
    maps: &mut AccountMaps,
    message: &str,
) -> VelocityResult {
    validate!(
        meets_initial_margin_requirement(liquidator, maps)?,
        ErrorCode::InsufficientCollateral,
        "{}",
        message
    )?;

    if let Some(liquidator_net_equity) = calculate_net_equity_for_floor(liquidator, maps)? {
        liquidator_net_equity.validate_clears_buffered_floor(liquidator)?;
    }

    Ok(())
}

/// Latch every margin scope of the account that fails its requirement.
///
/// This moves no value. It only opens the liquidation window, which the
/// account's own risk-increasing paths then refuse.
pub fn set_user_status_to_being_liquidated(
    user: &mut User,
    maps: &mut AccountMaps,
    slot: u64,
    state: &State,
) -> VelocityResult {
    validate!(
        !user.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "user bankrupt",
    )?;

    validate!(
        !user.is_being_liquidated(),
        ErrorCode::UserIsBeingLiquidated,
        "user is already being liquidated",
    )?;

    let liquidation_margin_buffer_ratio = state.liquidation_margin_buffer_ratio;
    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        maps,
        MarginContext::liquidation(liquidation_margin_buffer_ratio),
    )?;

    let mut updated_liquidation_status = false;
    if !user.is_cross_margin_being_liquidated()
        && !margin_calculation.meets_cross_margin_requirement()
    {
        updated_liquidation_status = true;
        user.enter_cross_margin_liquidation(slot)?;
    }

    for (market_index, isolated_margin_calculation) in
        margin_calculation.isolated_margin_calculations.iter()
    {
        if !user.is_isolated_margin_being_liquidated(*market_index)?
            && !isolated_margin_calculation.meets_margin_requirement()
        {
            updated_liquidation_status = true;
            user.enter_isolated_margin_liquidation(*market_index, slot)?;
        }
    }

    if !updated_liquidation_status {
        return Err(ErrorCode::SufficientCollateral);
    }

    Ok(())
}
