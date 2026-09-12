//! Resolving a bankrupt perp position.
//!
//! The estate pays first. What is left runs down four tranches in a fixed
//! order: the market's in-transit insurance fees, the shared insurance vault,
//! the AMM's fee provision, and then the surviving open interest through a
//! funding-rate bump. The debt is cleared either way, so the order decides who
//! pays for it and never whether it is paid.

use super::*;

/// What the funded tranches paid of one perp bankruptcy.
struct PerpBankruptcyCoverage {
    /// The shared insurance vault, which the record reports.
    if_payment: u128,
    /// What the surviving open interest must bear.
    loss_to_socialize: i128,
}

pub fn resolve_perp_bankruptcy(
    market_index: u16,
    parties: &mut LiquidationParties,
    maps: &mut AccountMaps,
    now: i64,
    insurance_fund_vault_balance: u64,
    funding_paused: bool,
) -> VelocityResult<u64> {
    let liquidation_mode = get_perp_liquidation_mode(parties.user, market_index)?;

    if !liquidation_mode.is_user_bankrupt(parties.user)?
        && liquidation_mode.should_user_enter_bankruptcy(parties.user, &maps.spot_market_map)?
    {
        liquidation_mode.enter_bankruptcy(parties.user)?;
    }

    validate_preconditions(parties, maps, liquidation_mode.as_ref(), market_index)?;

    // Hold the index across the recovery and the setoff below. A position with no base, no quote and
    // no order does not match a lookup by market index, and the setoff can leave this position in
    // exactly that state, so the reads after it must go through the index.
    let position_index = get_position_index(&parties.user.perp_positions, market_index)
        .inspect_err(|_e| {
            msg!(
                "User does not have a position for perp market {}",
                market_index
            );
        })?;

    let setoff = realize_estate_assets(
        parties.user,
        maps,
        (market_index, position_index),
        now,
        funding_paused,
    )?;

    if unlatch_stale_bankruptcy(parties.user, maps, liquidation_mode.as_ref(), setoff)? {
        return Ok(0);
    }

    let Some(loss) = read_resolvable_loss(
        parties.user,
        maps,
        liquidation_mode.as_ref(),
        (market_index, position_index),
    )?
    else {
        return Ok(0);
    };

    // OtterSec #145: wind up the estate's unfundable claims, so the account cannot keep one after
    // other people's money covers its debt.
    //
    // This MUST sit below the un-latch above. Forfeiting is irreversible, and the un-latch path hands
    // the account back to ordinary liquidation, which may cover the whole debt out of the seized
    // asset — leaving no bankruptcy, no draw, and a forfeit that bought nothing.
    //
    // It sits below the `loss` read because `loss` is what bounds it. This call pays off `loss` with
    // the revenue pool, the insurance fund and the surviving depositors, so `loss` is exactly the
    // amount of other people's money the estate is about to consume, and the most it can owe them.
    // `resolve_spot_bankruptcy` bounds its own forfeit the same way, against the borrow it covers.
    extinguish_unfundable_perp_claims(
        parties.user,
        &maps.perp_market_map,
        &maps.spot_market_map,
        loss.unsigned_abs(),
    )?;

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        parties.user,
        maps,
        MarginContext::standard(MarginRequirementType::Maintenance),
    )?;

    let coverage = cover_loss_from_tranches(
        maps,
        market_index,
        loss,
        insurance_fund_vault_balance,
        now,
        funding_paused,
    )?;

    close_perp_bankruptcy(
        parties,
        maps,
        liquidation_mode.as_ref(),
        (&margin_calculation, &coverage, loss),
        (market_index, now),
    )?;

    coverage.if_payment.cast()
}

/// Spread what the tranches left, clear the debt, and record the resolution.
fn close_perp_bankruptcy(
    parties: &mut LiquidationParties,
    maps: &mut AccountMaps,
    liquidation_mode: &dyn LiquidatePerpMode,
    outcome: (&MarginCalculation, &PerpBankruptcyCoverage, i128),
    market: (u16, i64),
) -> VelocityResult {
    let (margin_calculation, coverage, loss) = outcome;
    let (market_index, now) = market;

    let cumulative_funding_rate_delta = socialize_remaining_loss(maps, market_index, coverage)?;
    clear_bad_debt(parties.user, maps, market_index)?;

    // True if a bankrupting liability remains; clears status otherwise.
    let still_bankrupt =
        liquidation_mode.should_user_enter_bankruptcy(parties.user, &maps.spot_market_map)?;
    if !still_bankrupt {
        liquidation_mode.exit_bankruptcy(parties.user)?;
    }

    emit_perp_bankruptcy_record(
        parties,
        liquidation_mode,
        margin_calculation,
        still_bankrupt,
        PerpBankruptcyRecord {
            market_index,
            if_payment: coverage.if_payment,
            pnl: loss,
            clawback_user: None,
            clawback_user_payment: None,
            cumulative_funding_rate_delta,
        },
        now,
    )
}

/// Refuse a resolution neither account nor market may take part in.
fn validate_preconditions(
    parties: &LiquidationParties,
    maps: &AccountMaps,
    liquidation_mode: &dyn LiquidatePerpMode,
    market_index: u16,
) -> VelocityResult {
    validate!(
        liquidation_mode.is_user_bankrupt(parties.user)?,
        ErrorCode::UserNotBankrupt,
        "user not bankrupt",
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
            .perp_market_map
            .get_ref(&market_index)?
            .is_operation_paused(PerpOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        market_index
    )
}

/// Turn what the estate holds into payment of this market's debt, and return
/// what the quote deposit covered.
///
/// OtterSec #130 / #145: the estate's own claims become cash before anyone
/// else's money is drawn. The recovery is bounded by the part of this market's
/// debt the existing quote deposit does not already cover, so it never drains
/// a pool further than the debt reaches.
///
/// An isolated position is walled off from cross collateral, and a cross claim
/// settles into the cross quote deposit, so it can never pay this debt. The
/// cap stays at zero for that mode, the same way the setoff returns early.
///
/// The setoff is safe to run ahead of the stale-latch check: it debits a
/// deposit and credits the debt by the same amount, which is exactly the
/// `settle_pnl` the user is barred from making, so it leaves the estate no
/// worse off even when the account is handed back to ordinary liquidation.
fn realize_estate_assets(
    user: &mut User,
    maps: &mut AccountMaps,
    position: (u16, usize),
    now: i64,
    funding_paused: bool,
) -> VelocityResult<u128> {
    let (market_index, position_index) = position;

    let debt_to_cover = {
        let position = user.perp_positions[position_index];
        if position.is_isolated() {
            0
        } else {
            position
                .quote_asset_amount
                .min(0)
                .unsigned_abs()
                .cast::<u128>()?
        }
    };
    let quote_deposit_on_hand = {
        let quote_spot_market = maps.spot_market_map.get_quote_spot_market()?;
        let quote_position = user.get_quote_spot_position();
        if quote_position.balance_type == SpotBalanceType::Deposit {
            quote_position.get_token_amount(&quote_spot_market)?
        } else {
            0
        }
    };

    recover_perp_claims_from_pnl_pools(
        user,
        &maps.perp_market_map,
        &maps.spot_market_map,
        debt_to_cover.saturating_sub(quote_deposit_on_hand),
        now,
        funding_paused,
    )?;

    apply_quote_deposit_setoff_for_perp_bankruptcy(market_index, user, maps, now, funding_paused)
}

/// Release a bankruptcy latch whose premise no longer holds.
///
/// OtterSec #130 fallback, for what the setoff cannot reach: a credit in a
/// NON-QUOTE deposit. Netting that against a quote debt needs a cross-asset
/// swap, not a balance transfer.
///
/// If such an asset remains, the latch is stale. It is cleared and nothing is
/// drawn. Ordinary liquidation rejects a latched user, so it becomes legal
/// again, seizes the asset, and re-latches for the real residual.
///
/// This tests only for realizable assets, not the full predicate. That one
/// also vetoes on an open order or base exposure, which the resolvers are
/// reached with. The mode decides which assets can reach this debt: an
/// isolated position is walled off from the cross-margin book.
///
/// The un-latch is committed rather than raised as an error. An error leaves
/// the bit set and wedges both paths. Nothing is drawn here, so this cannot
/// reorder insurance spending against the perp-before-spot precedence.
fn unlatch_stale_bankruptcy(
    user: &mut User,
    maps: &AccountMaps,
    liquidation_mode: &dyn LiquidatePerpMode,
    setoff: u128,
) -> VelocityResult<bool> {
    if !liquidation_mode.has_realizable_assets(user, &maps.spot_market_map)? {
        return Ok(false);
    }

    msg!(
        "stale bankruptcy latch (assets present after setoff of {}); un-latching without drawing",
        setoff
    );
    liquidation_mode.exit_bankruptcy(user)?;
    Ok(true)
}

/// The bad debt this resolution must cover.
///
/// `None` reports that the setoff already cleared it. Nothing is left to
/// resolve here, and the account keeps its latch only if a liability elsewhere
/// still justifies one.
///
/// Admission is re-derived on the same rule as the tail of the resolver. The
/// recovery pass caps what it draws at this debt, so a claim big enough to
/// cover the debt makes a deposit equal to the debt the designed outcome, and
/// the setoff then zeroes both. Without the re-derive the latch survives on an
/// estate that owes nothing, and no path can clear it: a deposit rejects a
/// bankrupt user, ordinary liquidation rejects a latched one, and a second
/// call to this resolver reaches this same return.
fn read_resolvable_loss(
    user: &mut User,
    maps: &AccountMaps,
    liquidation_mode: &dyn LiquidatePerpMode,
    position: (u16, usize),
) -> VelocityResult<Option<i128>> {
    let (market_index, position_index) = position;
    let loss = user.perp_positions[position_index]
        .quote_asset_amount
        .cast::<i128>()?;

    if loss == 0 {
        msg!(
            "perp market {} bad debt fully covered by setoff; nothing to resolve",
            market_index
        );

        if !liquidation_mode.should_user_enter_bankruptcy(user, &maps.spot_market_map)? {
            liquidation_mode.exit_bankruptcy(user)?;
        }

        return Ok(None);
    }

    validate!(
        loss < 0,
        ErrorCode::InvalidPerpPositionToLiquidate,
        "user must have negative pnl"
    )?;

    Ok(Some(loss))
}

/// Draw the three funded tranches, and report what each paid.
fn cover_loss_from_tranches(
    maps: &mut AccountMaps,
    market_index: u16,
    loss: i128,
    insurance_fund_vault_balance: u64,
    now: i64,
    funding_paused: bool,
) -> VelocityResult<PerpBankruptcyCoverage> {
    let pending_if_payment = draw_pending_if_tranche(maps, market_index, loss)?;
    let loss_after_pending = loss.safe_add(pending_if_payment.cast::<i128>()?)?;

    let if_payment = draw_insurance_vault_tranche(
        maps,
        market_index,
        loss_after_pending,
        insurance_fund_vault_balance,
        now,
        funding_paused,
    )?;

    let losses_remaining: i128 = loss_after_pending.safe_add(if_payment.cast::<i128>()?)?;
    validate!(
        losses_remaining <= 0,
        ErrorCode::InvalidPerpPositionToLiquidate,
        "losses_remaining must be non-positive"
    )?;

    let amm_tranche_payment = claw_back_amm_provision(maps, market_index, losses_remaining)?;

    let loss_to_socialize = losses_remaining.safe_add(amm_tranche_payment)?;
    validate!(
        loss_to_socialize <= 0,
        ErrorCode::InvalidPerpPositionToLiquidate,
        "loss_to_socialize must be non-positive"
    )?;

    Ok(PerpBankruptcyCoverage {
        if_payment,
        loss_to_socialize,
    })
}

/// Tranche 1: the market's own in-transit insurance fees (`pending_if_fee`)
/// are consumed BEFORE the shared IF vault is tapped.
///
/// Counter-only: the pending claim and the forgiven loss are both claims on
/// future pnl-pool inflows, so canceling one against the other needs no token
/// movement. The fee value that would have swept to the revenue pool stays in
/// the pnl pool backing the counterparties this spares from socialization.
fn draw_pending_if_tranche(
    maps: &AccountMaps,
    market_index: u16,
    loss: i128,
) -> VelocityResult<u128> {
    let mut perp_market = maps.perp_market_map.get_ref_mut(&market_index)?;

    let pending_if_payment = loss
        .unsigned_abs()
        .min(perp_market.fee_ledger.pending_if_fee);

    if pending_if_payment > 0 {
        perp_market
            .fee_ledger
            .consume_pending_if(pending_if_payment)?;
        msg!("bankruptcy pending_if_fee tranche: {}", pending_if_payment);
    }

    Ok(pending_if_payment)
}

/// Tranche 2: the shared insurance fund vault.
///
/// One lamport is left behind, so the vault's deposit always stays at or above
/// one.
fn draw_insurance_vault_tranche(
    maps: &mut AccountMaps,
    market_index: u16,
    loss_after_pending: i128,
    insurance_fund_vault_balance: u64,
    now: i64,
    funding_paused: bool,
) -> VelocityResult<u128> {
    let mut perp_market = maps.perp_market_map.get_ref_mut(&market_index)?;
    let max_insurance_withdraw = perp_market
        .insurance_claim
        .quote_max_insurance
        .safe_sub(perp_market.insurance_claim.quote_settled_insurance)?
        .cast::<u128>()?;

    let if_payment = loss_after_pending
        .unsigned_abs()
        .min(insurance_fund_vault_balance.saturating_sub(1).cast()?)
        .min(max_insurance_withdraw);

    perp_market.insurance_claim.quote_settled_insurance = perp_market
        .insurance_claim
        .quote_settled_insurance
        .safe_add(if_payment.cast()?)?;

    // move if payment to pnl pool
    let spot_market = &mut maps.spot_market_map.get_ref_mut(&QUOTE_SPOT_MARKET_INDEX)?;
    let oracle_price_data = maps.oracle_map.get_price_data(&spot_market.oracle_id())?;
    update_spot_market_cumulative_interest(
        spot_market,
        Some(oracle_price_data),
        now,
        funding_paused,
    )?;

    update_spot_balances(
        if_payment,
        &SpotBalanceType::Deposit,
        spot_market,
        &mut perp_market.pnl_pool,
        false,
    )?;

    Ok(if_payment)
}

/// Tranche 3: claw back the AMM's fee provision.
///
/// This is the backstop of LAST resort, capped at `amm_protocol_fees_received`
/// (cumulative provision granted through the amm fee cut, net of prior
/// clawbacks). The AMM's own spread and trading capital beyond the provision
/// is never tapped, and neither is the external LP pool. Two phases run:
///
/// 3a. the not-yet-tokenized provision (`pending_amm_provision`) is
///     counter-only, like tranche 1. Its token backing still sits in the pnl
///     pool, where it now backs the spared counterparties instead.
/// 3b. the tokenized remainder moves real tokens from the AMM fee pool to the
///     pnl pool, capped by what the fee pool actually holds.
///
/// Both phases debit the AMM's books through `record_amm_pnl`: the provision
/// was booked into `total_fee_minus_distributions` at fill, and the dent to
/// `net_revenue_since_last_funding` lets the drawdown breaker see the hit.
fn claw_back_amm_provision(
    maps: &mut AccountMaps,
    market_index: u16,
    losses_remaining: i128,
) -> VelocityResult<i128> {
    if losses_remaining >= 0 {
        return Ok(0);
    }

    let mut perp_market = maps.perp_market_map.get_ref_mut(&market_index)?;
    // reborrow through the RefMut so disjoint field borrows split
    let perp_market = &mut *perp_market;
    let spot_market = &mut maps.spot_market_map.get_ref_mut(&QUOTE_SPOT_MARKET_INDEX)?;

    let clawback_budget: u128 = losses_remaining
        .unsigned_abs()
        .min(perp_market.fee_ledger.amm_protocol_fees_received);

    // 3a. untokenized provision: counter-only
    let untokenized = clawback_budget.min(perp_market.fee_ledger.pending_amm_provision);
    if untokenized > 0 {
        perp_market
            .fee_ledger
            .consume_pending_amm_provision(untokenized)?;
        perp_market.fee_ledger.consume_amm_backstop(untokenized)?;
        <crate::vlp::amm::AMM as crate::vlp::amm::quoter::AmmContract>::record_amm_pnl(
            &mut perp_market.amm,
            -untokenized.cast::<i128>()?,
        )?;
        msg!(
            "bankruptcy amm provision tranche (untokenized): {}",
            untokenized
        );
    }

    // 3b. tokenized provision: fee-pool tokens move to the pnl pool
    let fee_pool_tokens: u128 = get_fee_pool_tokens(&perp_market.amm, spot_market)?
        .max(0)
        .cast()?;
    let tokenized = clawback_budget.safe_sub(untokenized)?.min(fee_pool_tokens);
    if tokenized > 0 {
        transfer_spot_balances(
            tokenized.cast()?,
            spot_market,
            &mut perp_market.amm.fee_pool,
            &mut perp_market.pnl_pool,
        )?;
        perp_market.fee_ledger.consume_amm_backstop(tokenized)?;
        <crate::vlp::amm::AMM as crate::vlp::amm::quoter::AmmContract>::record_amm_pnl(
            &mut perp_market.amm,
            -tokenized.cast::<i128>()?,
        )?;
        msg!(
            "bankruptcy amm provision tranche (tokenized): {}",
            tokenized
        );
    }

    untokenized.safe_add(tokenized)?.cast()
}

/// Tranche 4: spread what the funded tranches left across the surviving open
/// interest, and return the funding-rate delta that carries it.
///
/// Only a socialized loss needs a delta. With full coverage the helper is
/// skipped: it requires nonzero open interest, so a fully-covered bankruptcy
/// in a market with zero open interest would otherwise revert the whole atomic
/// resolution and leave the account bankrupt despite sufficient coverage.
fn socialize_remaining_loss(
    maps: &mut AccountMaps,
    market_index: u16,
    coverage: &PerpBankruptcyCoverage,
) -> VelocityResult<i128> {
    if coverage.loss_to_socialize >= 0 {
        return Ok(0);
    }

    let cumulative_funding_rate_delta = calculate_funding_rate_deltas_to_resolve_bankruptcy(
        coverage.loss_to_socialize,
        maps.perp_market_map.get_ref(&market_index)?.deref(),
    )?;

    let mut market = maps.perp_market_map.get_ref_mut(&market_index)?;

    market.total_social_loss = market
        .total_social_loss
        .safe_add(coverage.loss_to_socialize.unsigned_abs())?;

    settle_amm_funding_before_bump(&mut market)?;

    // Socialize the loss across surviving open interest via an asymmetric
    // cum-rate bump (longs and shorts both owe funding covering the loss).
    market.cumulative_funding_rate_long = market
        .cumulative_funding_rate_long
        .safe_add(cumulative_funding_rate_delta)?;

    market.cumulative_funding_rate_short = market
        .cumulative_funding_rate_short
        .safe_sub(cumulative_funding_rate_delta)?;

    // The cum-rate bump makes surviving positions owe `loss_to_socialize`
    // in aggregate funding. Record it the same way funding accrual does
    // (net_unsettled_funding_pnl -= protocol funding revenue): without this
    // the obligation is absent from net_unsettled until users settle, and
    // calculate_net_user_pnl overstates aggregate user PnL by the socialized
    // loss in the meantime.
    market.net_unsettled_funding_pnl = market
        .net_unsettled_funding_pnl
        .safe_add(coverage.loss_to_socialize.cast()?)?;

    // Now advance the AMM stamp PAST the socialization bump. The AMM's
    // genuine funding was just settled above, so this only excludes the
    // socialization delta from the AMM's next funding payment — the
    // socialized loss is borne by surviving USER open interest, not the AMM.
    // Leaving the stamp behind would instead credit the AMM phantom funding
    // on the bump (both legs resolve positive for a balanced book),
    // manufacturing `total_fee_minus_distributions` (≈ D·G1/G0, able to
    // exceed the socialized loss D as gross OI grows) that is then payable to
    // survivors or spendable as curve budget (OtterSec #89).
    market.amm.last_cumulative_funding_rate_long =
        market.cumulative_funding_rate_long.cast::<i64>()?;
    market.amm.last_cumulative_funding_rate_short =
        market.cumulative_funding_rate_short.cast::<i64>()?;

    Ok(cumulative_funding_rate_delta)
}

/// Pay the AMM the funding it genuinely accrued, before the socialization bump
/// moves the cumulative rates.
///
/// This is the same payment the `FundingUpdated` quoter handler applies, so no
/// accrued AMM funding is dropped when the AMM stamp advances past the bump.
/// `calculate_amm_funding_payment` pays the AMM `(cumulative_funding_rate −
/// amm.last_cumulative_funding_rate) × −net_position` per leg, and here the
/// deltas are only what has genuinely accrued.
///
/// Today the market cum rates and the AMM stamp only ever advance together in
/// `update_funding_rate`, so on entry this payment is zero. Applying it
/// explicitly rather than assuming the invariant keeps the AMM's books correct
/// if another writer of the cum rates is ever added.
fn settle_amm_funding_before_bump(market: &mut PerpMarket) -> VelocityResult {
    let amm_funding_payment = crate::math::funding::calculate_amm_funding_payment(
        market.base_asset_amount_long,
        market.base_asset_amount_short,
        market.cumulative_funding_rate_long,
        market.cumulative_funding_rate_short,
        market.amm.last_cumulative_funding_rate_long,
        market.amm.last_cumulative_funding_rate_short,
    )?;

    <crate::vlp::amm::AMM as crate::vlp::amm::quoter::AmmContract>::record_amm_pnl(
        &mut market.amm,
        amm_funding_payment,
    )
}

/// Zero the position's quote debt, and record what the account lost.
fn clear_bad_debt(user: &mut User, maps: &AccountMaps, market_index: u16) -> VelocityResult {
    let mut market = maps.perp_market_map.get_ref_mut(&market_index)?;
    let position_index = get_position_index(&user.perp_positions, market_index)?;
    let quote_asset_amount = user.perp_positions[position_index].quote_asset_amount;
    update_quote_asset_amount(
        &mut user.perp_positions[position_index],
        &mut market,
        -quote_asset_amount,
    )?;

    user.increment_total_socialized_loss(quote_asset_amount.unsigned_abs())
}

/// Emit one `PerpBankruptcy` record.
fn emit_perp_bankruptcy_record(
    parties: &LiquidationParties,
    liquidation_mode: &dyn LiquidatePerpMode,
    margin_calculation: &MarginCalculation,
    still_bankrupt: bool,
    perp_bankruptcy: PerpBankruptcyRecord,
    now: i64,
) -> VelocityResult {
    let liquidation_id = parties.user.next_liquidation_id.safe_sub(1)?;

    let (margin_requirement, total_collateral, bit_flags) =
        liquidation_mode.get_event_fields(margin_calculation)?;

    emit!(LiquidationRecord {
        ts: now,
        liquidation_id,
        liquidation_type: LiquidationType::PerpBankruptcy,
        user: *parties.user_key,
        liquidator: *parties.liquidator_key,
        margin_requirement,
        total_collateral,
        bankrupt: still_bankrupt,
        perp_bankruptcy,
        bit_flags,
        ..LiquidationRecord::default()
    });

    Ok(())
}
