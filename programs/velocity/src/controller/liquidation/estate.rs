//! What the bankrupt estate owns and owes before anyone else pays.
//!
//! A bankruptcy resolver draws on the revenue pool, the insurance fund and
//! the surviving depositors. Before it may, the estate must realize what it
//! holds and give up what it cannot: its claims on the markets' pnl pools,
//! its quote deposit, and the positive claims no pool can fund. This module
//! holds those four moves, and the flag that freezes a market fee sweep while
//! a debt in it is unresolved.

use super::*;

/// Settle what each market's PnL pool can pay of the estate's positive claims into its quote
/// deposit, bounded by `max_recovery`, and return the total (OtterSec #130 / #145).
///
/// This is the `settle_pnl` move the estate is barred from making. A claim on a market whose pool
/// holds tokens is an asset the estate owns and can reach: `update_pool_balances` pays a positive
/// claim out of the pool's raw balance, first come first served, and `settle_pnl` lets any keeper
/// make that call for a user who is being liquidated. Pool excess is not the yardstick, because it
/// governs only who may settle for a *healthy* user.
///
/// Bankruptcy admission no longer vetoes on a funded pool, so nothing else forces that recovery
/// before the tranches open. Without this pass insurance would cover a debt the estate could pay
/// itself, which is OtterSec #130's finding in a new place.
///
/// `max_recovery` is the debt this call is about to cover, less what the estate's existing quote
/// deposit already covers. Taking more would drain a pool the estate has no further claim against,
/// and dilute that market's other claimants for nothing.
///
/// The recovered tokens land in the quote deposit, where the caller applies them:
/// `resolve_perp_bankruptcy` sets them off against its market's debt, and `resolve_spot_bankruptcy`
/// hands the account back to ordinary liquidation, which seizes the deposit against the borrow.
pub(crate) fn recover_perp_claims_from_pnl_pools(
    user: &mut User,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    max_recovery: u128,
    now: i64,
    funding_paused: bool,
) -> VelocityResult<u128> {
    if max_recovery == 0 {
        return Ok(0);
    }

    let quote_spot_market = &mut spot_market_map.get_quote_spot_market_mut()?;

    // Accrue before converting the pool and the deposit through the index. Pass `None`: only the
    // interest index matters here, interest accrual does not depend on the oracle, and feeding a
    // price would stamp `historical_oracle_data` as a side effect of a bankruptcy resolution.
    update_spot_market_cumulative_interest(quote_spot_market, None, now, funding_paused)?;
    let mut remaining = max_recovery;
    let mut total_recovered: u128 = 0;

    // One pass over the same list the handler declared writable, from the same filter the forfeit
    // uses, so neither can write to a market the handler did not declare.
    for market_index in perp_markets_with_forfeitable_claims(user) {
        if remaining == 0 {
            break;
        }

        let index = get_position_index(&user.perp_positions, market_index)?;
        let claim = user.perp_positions[index]
            .quote_asset_amount
            .cast::<u128>()?;

        let mut perp_market = perp_market_map.get_ref_mut(&market_index)?;

        let pnl_pool_tokens = get_token_amount(
            perp_market.pnl_pool.balance(),
            quote_spot_market,
            perp_market.pnl_pool.balance_type(),
        )?;

        let recovered = claim.min(pnl_pool_tokens).min(remaining);
        if recovered == 0 {
            continue;
        }

        transfer_spot_balances(
            recovered.cast()?,
            quote_spot_market,
            &mut perp_market.pnl_pool,
            user.get_quote_spot_position_mut(),
        )?;

        update_quote_asset_amount(
            &mut user.perp_positions[index],
            &mut perp_market,
            -recovered.cast::<i64>()?,
        )?;

        // Parity with `settle_pnl`, which stamps the same counter when it moves this value.
        update_settled_pnl(user, index, recovered.cast::<i64>()?)?;

        msg!(
            "perp market {} bankruptcy: recovered {} of claim from the pnl pool",
            market_index,
            recovered
        );

        remaining = remaining.safe_sub(recovered)?;
        total_recovered = total_recovered.safe_add(recovered)?;
    }

    Ok(total_recovered)
}

/// Book the user's quote debt in `market_index` against that market's
/// `pending_bankruptcy_claims`, so the fee sweep withholds the whole
/// `pending_if_fee` until the debt resolves. Without it, the sweep — which is
/// permissionless, and also runs inline on every pnl settle — can drain the
/// first-loss tranche between the latch and the resolution, and the loss falls
/// through to the shared insurance fund or into socialization.
///
/// Call this at every point that latches a user bankrupt while `market_index`
/// is writable. It is idempotent: the position flag records the booking, so a
/// repeated latch counts the debt once. `update_quote_asset_amount` releases
/// the booking when the quote debt is gone.
///
/// Only a SETTLED debt is booked: `base_asset_amount == 0` and
/// `quote_asset_amount < 0`. That is exactly what `resolve_perp_bankruptcy`
/// can absorb, and it is what makes the release condition sound — a position
/// that still holds base can carry a negative quote through ordinary trading
/// (a partly closed short does), and its quote swings either way on the next
/// fill. Booking one would let an ordinary fill release the freeze. Both
/// bankruptcy predicates already require a zero base, so this only restates
/// the admission rule locally instead of trusting each call site to hold it.
///
/// A cross-margin latch can leave a debt in a market the latching instruction
/// did not declare writable, which cannot be booked here. The standing
/// `get_bankruptcy_if_floor()` tranche covers that market instead.
pub(crate) fn flag_perp_bankruptcy_claim(
    user: &mut User,
    market_index: u16,
    perp_market_map: &PerpMarketMap,
) -> VelocityResult<()> {
    let Ok(position_index) = get_position_index(&user.perp_positions, market_index) else {
        return Ok(());
    };

    let position = &mut user.perp_positions[position_index];
    if position.has_bankruptcy_claim()
        || position.base_asset_amount != 0
        || position.quote_asset_amount >= 0
    {
        return Ok(());
    }

    position.set_bankruptcy_claim();
    perp_market_map
        .get_ref_mut(&market_index)?
        .increment_pending_bankruptcy_claims();

    Ok(())
}

/// Forfeit the estate's unfundable positive perp claims to their markets' insurance tranches, and
/// return the total (OtterSec #145).
///
/// A positive `quote_asset_amount` on a zero-base position is a claim on that market's PnL pool. When
/// the pool cannot pay it, the claim is unfunded but still owed. `is_cross_margin_bankrupt` lets such
/// a claim through, because a permanent veto strands a resolvable loss in another market forever.
///
/// `recover_perp_claims_from_pnl_pools` runs first and takes everything the pools can pay, so what
/// reaches this function is what nobody can realize.
///
/// The claim must not simply be ignored. The resolver draws the full per-market debt, the re-derive
/// then finds the account perp-solvent, and the latch clears. The user keeps a live claim that
/// insurance has already paid for.
///
/// So the creditor moves instead of the obligation vanishing. This zeroes the user's claim and adds
/// the same amount to the market's `pending_if_fee`.
///
/// The swap is equity-neutral. Zeroing the claim lowers `market.quote_asset_amount`, and therefore
/// `net_user_pnl`, which raises the market's excess. The `pending_if_fee` credit lowers the excess by
/// the same amount. No new state is needed, and no inter-market receivable: `pending_if_fee` is
/// already a claim on future PnL-pool inflows, which is what the user's claim was.
///
/// The insurance tranche of the claim's own market is credited, not the market that bore the loss.
/// That is what makes the swap equity-neutral for the claim's market, and `pending_if_fee` is the
/// only account with the right meaning. Both markets share one quote insurance vault, so the
/// compensation reaches the same fund. Only per-market tranche accounting shifts.
///
/// The insurance fund receives a claim, not cash. This does not reduce the draw for the bankruptcy in
/// progress. The fund is paid later, if the pool fills, through `pending_if_fee` ->
/// `sweep_market_fees` -> revenue pool -> `settle_revenue_to_insurance_fund`.
///
/// `max_forfeit` bounds the total taken: the loss this call is about to cover with other people's
/// money. A resolver may never take more from the estate than the payout it is funding, whatever the
/// estate holds and however stale its latch is.
///
/// That bound has to be derived here rather than inherited from bankruptcy admission, where the
/// estate had to be net insolvent. Admission is a fact about the past. The bankruptcy latch is a
/// snapshot, nothing re-derives it, and credits keep arriving after it is set (OtterSec #130), so by
/// the time a resolver runs the estate can be solvent again and an admission-time bound is worthless.
/// A bound taken from the loss in front of the resolver does not decay, and it holds for credit
/// routes that do not exist yet.
pub(crate) fn extinguish_unfundable_perp_claims(
    user: &mut User,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    max_forfeit: u128,
) -> VelocityResult<u128> {
    let quote_spot_market = spot_market_map.get_quote_spot_market()?;
    let mut remaining = max_forfeit;
    let mut total_forfeited: u128 = 0;

    // One pass over the same list the handler declared writable. Both derive it from
    // `is_settled_positive_claim`, so this loop cannot write to a market the handler did not declare.
    for market_index in perp_markets_with_forfeitable_claims(user) {
        if remaining == 0 {
            break;
        }

        let index = get_position_index(&user.perp_positions, market_index)?;
        let position = user.perp_positions[index];

        // Recompute what the pool cannot pay, under a READ borrow. The recovery pass above already
        // drained it to the extent the debt allowed, so this is normally the whole remaining claim.
        // Taking the write borrow only once something is actually forfeited keeps a no-op pass from
        // touching write access it does not use.
        let forfeited = {
            let perp_market = perp_market_map.get_ref(&market_index)?;
            let pnl_pool_tokens = get_token_amount(
                perp_market.pnl_pool.balance(),
                &quote_spot_market,
                perp_market.pnl_pool.balance_type(),
            )?;

            position
                .quote_asset_amount
                .cast::<u128>()?
                .saturating_sub(pnl_pool_tokens)
                .min(remaining)
        };

        if forfeited == 0 {
            continue;
        }

        let mut perp_market = perp_market_map.get_ref_mut(&market_index)?;

        update_quote_asset_amount(
            &mut user.perp_positions[index],
            &mut perp_market,
            -forfeited.cast::<i64>()?,
        )?;
        perp_market
            .fee_ledger
            .accrue_forfeited_claim_to_if(forfeited)?;

        msg!(
            "perp market {} bankruptcy: forfeited {} of unfundable claim to the insurance tranche",
            market_index,
            forfeited
        );

        remaining = remaining.safe_sub(forfeited)?;
        total_forfeited = total_forfeited.safe_add(forfeited)?;
    }

    Ok(total_forfeited)
}

/// Set off the user's quote deposit against this perp market's bad debt, and return the amount
/// applied (OtterSec #130).
///
/// The bankruptcy latch records that nothing was left to seize when liquidation set it. Assets can
/// arrive after that: the revenue-share sweep is permissionless, and keeper filler rewards credit the
/// filler with no bankruptcy check. Once the latch is set, every route that could pay the debt is
/// closed. `settle_pnl` rejects a bankrupt user, `liquidate_spot` rejects a bankrupt user, and the
/// resolver reads only the liability row. The credit therefore paid nothing, insurance covered the
/// whole debt, and the credit became withdrawable when the resolver cleared the latch.
///
/// This performs the `settle_pnl` move that a bankrupt user cannot make. Tokens go from the quote
/// deposit to the market's `pnl_pool`, and the perp debt falls by the same amount. Those tokens land
/// where tranche 2 would have put insurance money, so every tranche below sees the net debt and the
/// draw falls one for one. The quote market is token-neutral, so the handler's vault-amount assertion
/// still holds.
///
/// `min(deposit, |debt|)` bounds this, so it never takes more than is owed. A guard at the credit's
/// source could not do the same: the sweep cannot compute what the account owes, because that spans
/// every perp market and every spot borrow, and non-quote borrows need oracles.
pub(crate) fn apply_quote_deposit_setoff_for_perp_bankruptcy(
    market_index: u16,
    user: &mut User,
    maps: &mut AccountMaps,
    now: i64,
    funding_paused: bool,
) -> VelocityResult<u128> {
    let position_index = get_position_index(&user.perp_positions, market_index)?;

    // An isolated position is walled off from cross collateral. Its resolver never reads cross
    // deposits, and must not start now.
    if user.perp_positions[position_index].is_isolated() {
        return Ok(0);
    }

    let debt = user.perp_positions[position_index].quote_asset_amount;
    if debt >= 0 {
        return Ok(0);
    }

    let quote_spot_market = &mut maps.spot_market_map.get_quote_spot_market_mut()?;
    let oracle_price_data = maps
        .oracle_map
        .get_price_data(&quote_spot_market.oracle_id())?;
    update_spot_market_cumulative_interest(
        quote_spot_market,
        Some(oracle_price_data),
        now,
        funding_paused,
    )?;

    let quote_position = user.get_quote_spot_position();
    if quote_position.balance_type != SpotBalanceType::Deposit {
        return Ok(0);
    }

    let setoff = quote_position
        .get_token_amount(quote_spot_market)?
        .min(debt.unsigned_abs().cast()?);

    if setoff == 0 {
        return Ok(0);
    }

    let mut perp_market = maps.perp_market_map.get_ref_mut(&market_index)?;

    transfer_spot_balances(
        setoff.cast()?,
        quote_spot_market,
        user.get_quote_spot_position_mut(),
        &mut perp_market.pnl_pool,
    )?;

    update_quote_asset_amount(
        &mut user.perp_positions[position_index],
        &mut perp_market,
        setoff.cast()?,
    )?;

    // Parity with `settle_pnl`, which stamps the same counter when it moves this value.
    update_settled_pnl(user, position_index, -setoff.cast::<i64>()?)?;

    msg!(
        "perp market {} bankruptcy: set off {} of quote deposit against bad debt",
        market_index,
        setoff
    );

    Ok(setoff)
}
