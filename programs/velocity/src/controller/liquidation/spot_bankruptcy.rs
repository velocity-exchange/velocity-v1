//! Resolving a bankrupt spot borrow.
//!
//! The estate pays first. What is left runs down three tranches: the market's
//! own unsettled revenue, the staker-owned insurance vault, and then the
//! market's depositors through a cut to the cumulative deposit interest.

use super::*;

/// What the funded tranches paid of one spot bankruptcy.
struct SpotBankruptcyCoverage {
    /// The staker-owned insurance vault, which the record reports.
    if_payment: u128,
    /// What the market's depositors must bear.
    loss_to_socialize: u128,
}

pub fn resolve_spot_bankruptcy(
    market_index: u16,
    parties: &mut LiquidationParties,
    maps: &mut AccountMaps,
    now: i64,
    insurance_fund_vault_balance: u64,
    funding_paused: bool,
) -> VelocityResult<u64> {
    if !parties.user.is_cross_margin_bankrupt()
        && is_cross_margin_bankrupt(parties.user, &maps.spot_market_map)?
    {
        parties.user.enter_cross_margin_bankruptcy();
    }

    validate!(
        parties.user.is_cross_margin_bankrupt(),
        ErrorCode::UserNotBankrupt,
        "user not bankrupt",
    )?;

    recover_estate_claims(parties.user, maps, market_index, now, funding_paused)?;

    // OtterSec #130: assets can arrive after the latch is set, through the permissionless
    // revenue-share sweep or keeper filler rewards. Every route that could apply them to the debt is
    // closed to a bankrupt user, and this resolver reads only the liability row. A stale latch would
    // socialize the whole borrow while the new asset became withdrawable.
    //
    // If a realizable asset is present, clear the latch and return without drawing. Ordinary
    // liquidation then seizes it and re-latches for the real residual. Commit the un-latch instead of
    // erroring, which would wedge both paths. This tests only for assets, not the full predicate.
    // It sits above the #52 check because it draws nothing.
    if has_realizable_spot_assets_for_setoff(parties.user, &maps.spot_market_map)? {
        msg!("stale cross-margin bankruptcy latch (assets present); un-latching without drawing");
        parties.user.exit_cross_margin_bankruptcy();
        return Ok(0);
    }

    validate_preconditions(parties, maps, market_index)?;

    let MarginCalculation {
        margin_requirement,
        total_collateral,
        ..
    } = calculate_margin_requirement_and_total_collateral_and_liability_info(
        parties.user,
        maps,
        MarginContext::standard(MarginRequirementType::Maintenance),
    )?;

    let borrow_amount = read_borrow_amount(parties.user, maps, market_index, now, funding_paused)?;

    // The borrow priced in quote. This is what the tranches below are about to pay off with the
    // revenue pool, the insurance fund and the surviving depositors, so it is also the most the
    // estate can owe them. The counters below record the same value.
    let gross_quote_loss = quote_value_of_borrow(maps, market_index, borrow_amount)?;

    // OtterSec #145: an account can reach this resolver holding unfundable perp claims, because its
    // liability is a spot borrow and the #52 precedence above does not divert it. Wind them up here
    // too, or the tranches cover the borrow and the claim stays live to collect later.
    //
    // Bounded by the borrow this call covers, for the reason given in `resolve_perp_bankruptcy`.
    extinguish_unfundable_perp_claims(
        parties.user,
        &maps.perp_market_map,
        &maps.spot_market_map,
        gross_quote_loss.unsigned_abs(),
    )?;

    let coverage = cover_borrow_from_tranches(
        maps,
        market_index,
        borrow_amount,
        insurance_fund_vault_balance,
    )?;

    let cumulative_deposit_interest_delta =
        calculate_cumulative_deposit_interest_delta_to_resolve_bankruptcy(
            coverage.loss_to_socialize,
            maps.spot_market_map.get_ref(&market_index)?.deref(),
        )?;

    clear_bad_debt(
        parties.user,
        maps,
        market_index,
        (borrow_amount, gross_quote_loss),
        (
            coverage.loss_to_socialize,
            cumulative_deposit_interest_delta,
        ),
    )?;

    // True if a bankrupting liability remains; clears status otherwise.
    let still_bankrupt = is_cross_margin_bankrupt(parties.user, &maps.spot_market_map)?;
    if !still_bankrupt {
        parties.user.exit_cross_margin_bankruptcy();
    }

    emit_spot_bankruptcy_record(
        parties,
        (margin_requirement, total_collateral, still_bankrupt),
        SpotBankruptcyRecord {
            market_index,
            borrow_amount,
            if_payment: coverage.if_payment,
            cumulative_deposit_interest_delta,
        },
        now,
    )?;

    coverage.if_payment.cast()
}

/// Emit one `SpotBankruptcy` record.
fn emit_spot_bankruptcy_record(
    parties: &LiquidationParties,
    margin: (u128, i128, bool),
    spot_bankruptcy: SpotBankruptcyRecord,
    now: i64,
) -> VelocityResult {
    let (margin_requirement, total_collateral, still_bankrupt) = margin;
    let liquidation_id = parties.user.next_liquidation_id.safe_sub(1)?;

    emit!(LiquidationRecord {
        ts: now,
        liquidation_id,
        liquidation_type: LiquidationType::SpotBankruptcy,
        user: *parties.user_key,
        liquidator: *parties.liquidator_key,
        margin_requirement,
        total_collateral,
        bankrupt: still_bankrupt,
        spot_bankruptcy,
        ..LiquidationRecord::default()
    });

    Ok(())
}

/// Turn the estate's perp claims into a quote deposit before anyone else pays.
///
/// OtterSec #130 / #145: this is the same move `resolve_perp_bankruptcy`
/// makes. The borrow here may be in a non-quote market, which quote tokens
/// cannot pay directly, so the recovered value lands in the quote deposit and
/// the stale-latch check hands the account to ordinary liquidation to seize
/// it.
///
/// Bounded by the borrow's quote value at the same price the socialization
/// counters use.
fn recover_estate_claims(
    user: &mut User,
    maps: &mut AccountMaps,
    market_index: u16,
    now: i64,
    funding_paused: bool,
) -> VelocityResult {
    let borrow_quote_value = {
        let spot_market = maps.spot_market_map.get_ref(&market_index)?;
        let spot_position = user.get_spot_position(market_index)?;
        if spot_position.balance_type == SpotBalanceType::Borrow {
            let borrow_amount = spot_position.get_token_amount(spot_market.deref())?;
            let oracle_price_data = maps.oracle_map.get_price_data(&spot_market.oracle_id())?;
            get_token_value(
                borrow_amount.cast()?,
                spot_market.decimals,
                oracle_price_data.price,
            )?
            .unsigned_abs()
        } else {
            0
        }
    };

    recover_perp_claims_from_pnl_pools(
        user,
        &maps.perp_market_map,
        &maps.spot_market_map,
        borrow_quote_value,
        now,
        funding_paused,
    )?;

    Ok(())
}

/// Refuse a resolution neither account nor market may take part in.
///
/// Audit #52: perp bankruptcies must resolve before spot ones.
/// `resolve_perp_bankruptcy` and this resolver both draw from the shared quote
/// insurance fund vault, so a public caller could otherwise pick which
/// resolver spends it first and shift socialized loss between perp and spot
/// stakeholders. The keeper bots already resolve every perp bankruptcy before
/// any spot bankruptcy, so the same order is required on-chain.
fn validate_preconditions(
    parties: &LiquidationParties,
    maps: &AccountMaps,
    market_index: u16,
) -> VelocityResult {
    validate!(
        !has_pending_cross_margin_perp_bankruptcy(parties.user),
        ErrorCode::PerpBankruptcyMustPrecedeSpot,
        "resolve pending perp bankruptcies before spot bankruptcies",
    )?;

    validate!(
        !parties.liquidator.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "liquidator bankrupt",
    )?;

    validate!(
        !parties.liquidator.is_being_liquidated(),
        ErrorCode::UserIsBeingLiquidated,
        "liquidator being liquidated",
    )?;

    validate!(
        !maps
            .spot_market_map
            .get_ref(&market_index)?
            .is_operation_paused(SpotOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        market_index
    )?;

    // validate user and liquidator have spot position balances
    parties
        .user
        .get_spot_position(market_index)
        .map_err(|_| {
            msg!(
                "User does not have a spot balance for market {}",
                market_index
            );
            ErrorCode::CouldNotFindSpotPosition
        })
        .map(|_| ())
}

/// The borrow the tranches must cover, with the market's interest brought
/// current.
///
/// `SpotPosition::get_token_amount` scales the position by
/// `cumulative_borrow_interest`, so a stale index would clear the debt at less
/// than its current value: it would under-draw the revenue-pool and insurance
/// tranches, under-socialize the residual, and forgive the interest accrued
/// since the last touch.
///
/// The refresh passes no oracle price data. Only the interest index matters
/// here, interest accrual does not depend on the oracle, and feeding the price
/// would stamp the market's `historical_oracle_data` as a side effect of a
/// bankruptcy resolution. The sibling `resolve_perp_pnl_deficit` refresh does
/// the same.
fn read_borrow_amount(
    user: &User,
    maps: &AccountMaps,
    market_index: u16,
    now: i64,
    funding_paused: bool,
) -> VelocityResult<u128> {
    {
        let spot_market = &mut maps.spot_market_map.get_ref_mut(&market_index)?;
        update_spot_market_cumulative_interest(spot_market, None, now, funding_paused)?;
    }

    let spot_position = user.get_spot_position(market_index)?;
    validate!(
        spot_position.balance_type == SpotBalanceType::Borrow,
        ErrorCode::UserHasInvalidBorrow
    )?;

    validate!(
        spot_position.scaled_balance > 0,
        ErrorCode::UserHasInvalidBorrow
    )?;

    spot_position.get_token_amount(maps.spot_market_map.get_ref(&market_index)?.deref())
}

/// The borrow priced in quote at the market's oracle.
fn quote_value_of_borrow(
    maps: &mut AccountMaps,
    market_index: u16,
    borrow_amount: u128,
) -> VelocityResult<i128> {
    let spot_market = maps.spot_market_map.get_ref(&market_index)?;
    let oracle_price_data = maps.oracle_map.get_price_data(&spot_market.oracle_id())?;
    get_token_value(
        -borrow_amount.cast()?,
        spot_market.decimals,
        oracle_price_data.price,
    )
}

/// Draw the two funded tranches, and report what each paid.
fn cover_borrow_from_tranches(
    maps: &AccountMaps,
    market_index: u16,
    borrow_amount: u128,
    insurance_fund_vault_balance: u64,
) -> VelocityResult<SpotBankruptcyCoverage> {
    let revenue_pool_payment = draw_revenue_pool_tranche(maps, market_index, borrow_amount)?;

    // Tranche 2: the staker-owned insurance fund vault.
    // subtract 1 so insurance_fund_vault_balance always stays >= 1
    let if_payment = borrow_amount
        .safe_sub(revenue_pool_payment)?
        .min(insurance_fund_vault_balance.saturating_sub(1).cast()?);

    let loss_to_socialize = borrow_amount
        .safe_sub(revenue_pool_payment)?
        .safe_sub(if_payment)?;

    Ok(SpotBankruptcyCoverage {
        if_payment,
        loss_to_socialize,
    })
}

/// Tranche 1: the market's own unsettled insurance revenue (`revenue_pool`) is
/// consumed BEFORE the staker-owned insurance vault and any social loss.
///
/// Counter-only: the pool's tokens already sit in the spot vault, so canceling
/// the pool's deposit claim against the forgiven borrow needs no token
/// movement. Value that would have settled to the insurance vault covers the
/// bad debt directly instead of depositors. Unlike the periodic revenue
/// settle, this draw is not timer-gated or staker-APR-capped: in a bankruptcy
/// the pool is first-loss capital.
fn draw_revenue_pool_tranche(
    maps: &AccountMaps,
    market_index: u16,
    borrow_amount: u128,
) -> VelocityResult<u128> {
    let mut spot_market = maps.spot_market_map.get_ref_mut(&market_index)?;
    let revenue_pool_token_amount = get_token_amount(
        spot_market.revenue_pool.scaled_balance,
        spot_market.deref(),
        &SpotBalanceType::Deposit,
    )?;

    let payment = borrow_amount.min(revenue_pool_token_amount);
    if payment > 0 {
        // counter-only draw, no tokens leave the vault
        update_revenue_pool_balances(payment, &SpotBalanceType::Borrow, &mut spot_market, false)?;
        msg!("bankruptcy revenue pool tranche: {}", payment);
    }

    Ok(payment)
}

/// Zero the borrow, and cut the market's deposit interest by what the
/// depositors bear.
///
/// The user records the gross bad debt. The spot-market counters record only
/// the loss actually borne by depositors, which is what the revenue-pool and
/// insurance payments left.
fn clear_bad_debt(
    user: &mut User,
    maps: &mut AccountMaps,
    market_index: u16,
    debt: (u128, i128),
    socialized: (u128, u128),
) -> VelocityResult {
    let (borrow_amount, gross_quote_loss) = debt;
    let (loss_to_socialize, cumulative_deposit_interest_delta) = socialized;

    let mut spot_market = maps.spot_market_map.get_ref_mut(&market_index)?;
    let oracle_price_data = &maps.oracle_map.get_price_data(&spot_market.oracle_id())?;
    let socialized_quote_loss = get_token_value(
        -loss_to_socialize.cast()?,
        spot_market.decimals,
        oracle_price_data.price,
    )?;
    user.increment_total_socialized_loss(gross_quote_loss.unsigned_abs().cast()?)?;

    let spot_position = user.get_spot_position_mut(market_index)?;
    update_spot_balances_and_cumulative_deposits(
        borrow_amount,
        &SpotBalanceType::Deposit,
        &mut spot_market,
        spot_position,
        false,
        None,
    )?;

    spot_market.cumulative_deposit_interest = spot_market
        .cumulative_deposit_interest
        .safe_sub(cumulative_deposit_interest_delta)?;

    spot_market.total_social_loss = spot_market
        .total_social_loss
        .safe_add(loss_to_socialize.cast()?)?;

    spot_market.total_quote_social_loss = spot_market
        .total_quote_social_loss
        .safe_add(socialized_quote_loss.unsigned_abs().cast()?)?;

    Ok(())
}
