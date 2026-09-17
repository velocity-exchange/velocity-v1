use crate::{
    error::VelocityResult,
    math::{casting::Cast, safe_math::SafeMath, spot_balance::get_token_amount},
    state::{
        spot_market::SpotBalanceType,
        spot_market_map::SpotMarketMap,
        user::{PerpPosition, User},
    },
};

#[cfg(test)]
mod tests;

/// Whether a perp position is a settled positive claim: cross-margin, flat, with no live order and a
/// positive quote balance.
///
/// This is the single definition of "claim" for the bankruptcy paths. The recovery pass, the
/// forfeit, and the writable-market set that the handlers declare all call it. A second copy of the
/// filter would let the handler declare one market writable while a resolver writes to another.
pub fn is_settled_positive_claim(position: &PerpPosition) -> bool {
    !position.is_isolated()
        && position.quote_asset_amount > 0
        && position.base_asset_amount == 0
        && !position.has_open_order()
}

/// Whether the user qualifies as cross-margin bankrupt: it has spot liabilities, no realizable spot
/// assets, and no realizable perp exposure.
///
/// A deposit row is measured in tokens, not in `scaled_balance` (OtterSec #151). A full
/// spot-market socialization floors `cumulative_deposit_interest` at 1. Each wiped depositor then
/// keeps a positive scaled row that is worth zero tokens. Such a row used to block bankruptcy
/// admission and PnL liquidation, and it stalled the next bad-debt repair. A deposit worth 1 token
/// or more still vetoes.
///
/// A positive perp `quote_asset_amount` does not veto on its own, and neither does the state of its
/// market's PnL pool (OtterSec #145). The resolvers recover what the pool can pay and forfeit the
/// rest, so a claim cannot strand a resolvable loss in another market whatever the pool holds. Only
/// the net-solvency gate below still speaks to claims.
pub fn is_cross_margin_bankrupt(
    user: &User,
    spot_market_map: &SpotMarketMap,
) -> VelocityResult<bool> {
    // user is bankrupt iff they have spot liabilities, no spot assets, and no perp exposure

    let mut has_liability = false;

    for spot_position in user.spot_positions.iter() {
        if spot_position.scaled_balance == 0 {
            continue;
        }

        match spot_position.balance_type {
            SpotBalanceType::Deposit => {
                // Measure the row in tokens, not in scaled balance (OtterSec #151).
                let spot_market = spot_market_map.get_ref(&spot_position.market_index)?;
                let token_amount = get_token_amount(
                    spot_position.scaled_balance.cast()?,
                    &spot_market,
                    &SpotBalanceType::Deposit,
                )?;
                if token_amount > 0 {
                    return Ok(false);
                }
            }
            SpotBalanceType::Borrow => has_liability = true,
        }
    }

    let mut net_perp_quote: i128 = 0;

    for perp_position in user.perp_positions.iter() {
        // Skip isolated perp positions - they are handled by is_isolated_margin_bankrupt
        if perp_position.is_isolated() {
            continue;
        }

        if perp_position.base_asset_amount != 0 || perp_position.has_open_order() {
            return Ok(false);
        }

        let quote = perp_position.quote_asset_amount;

        if quote < 0 {
            has_liability = true;
        }

        net_perp_quote = net_perp_quote.safe_add(quote.cast()?)?;
    }

    // Admit only a net insolvent estate (OtterSec #145).
    //
    // Without this gate, +5000 in market A against -1000 in market B is admitted, and the account
    // enters a bankruptcy it does not need.
    //
    // The sum is exact, not an approximation. Every position that reaches here has
    // `base_asset_amount == 0`, so its whole value is its `quote_asset_amount`. No oracle is needed.
    //
    // This gate does not bound the forfeit. `extinguish_unfundable_perp_claims` does, against the
    // loss each resolver call covers, because a latched estate reaches a resolver without passing
    // this function again.
    if net_perp_quote > 0 {
        return Ok(false);
    }

    Ok(has_liability)
}

/// Whether the user still holds a spot deposit that can pay part of its bad debt.
///
/// The bankruptcy latch records that nothing was left to seize when liquidation set it (OtterSec
/// #130). Assets can arrive after that. The revenue-share sweep is permissionless, and keeper
/// filler rewards credit the filler with no bankruptcy check. Once the latch is set, `settle_pnl`
/// and `liquidate_spot` both reject the user, and the resolvers read only the liability row. The
/// asset therefore pays nothing, insurance covers the whole debt, and the asset becomes
/// withdrawable when the resolver clears the latch.
///
/// This predicate is much narrower than [`is_cross_margin_bankrupt`]. That one also vetoes on an
/// open order or on base exposure. Those conditions mean the estate is not resolvable yet, rather
/// than that it holds an asset. The resolvers are reached with orders open, so the full predicate
/// would block valid resolutions.
///
/// The scope is spot deposits only, and it stays that way. A perp claim is the other place value
/// can sit on a cross-margin estate. A resolver does not have to hand one back to ordinary
/// liquidation, because it recovers the fundable part into this deposit itself and forfeits the
/// rest. Value that arrives as a perp credit is therefore already handled when this runs.
pub fn has_realizable_spot_assets_for_setoff(
    user: &User,
    spot_market_map: &SpotMarketMap,
) -> VelocityResult<bool> {
    for spot_position in user.spot_positions.iter() {
        if spot_position.scaled_balance == 0
            || spot_position.balance_type != SpotBalanceType::Deposit
        {
            continue;
        }

        // Measured in tokens, not in scaled balance (OtterSec #151). A fully socialized market
        // leaves a positive scaled row whose token value is zero, and that worthless residue
        // must not gate a bad-debt repair.
        let spot_market = spot_market_map.get_ref(&spot_position.market_index)?;
        if get_token_amount(
            spot_position.scaled_balance.cast()?,
            &spot_market,
            &SpotBalanceType::Deposit,
        )? > 0
        {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Whether an isolated position still holds collateral of its own.
///
/// The isolated analogue of [`has_realizable_spot_assets_for_setoff`], and the stale-latch re-check
/// for an isolated bankruptcy. An isolated position is walled off from the cross-margin book, so
/// only its own collateral row can pay its debt.
///
/// The condition matches the collateral test in [`is_isolated_margin_bankrupt`] exactly. A resolver
/// that un-latched on a wider condition than the one that admits the bankruptcy would clear the
/// latch, re-admit, and clear it again on every call.
pub fn has_realizable_isolated_assets(user: &User, market_index: u16) -> VelocityResult<bool> {
    Ok(user
        .get_isolated_perp_position(market_index)?
        .isolated_position_scaled_balance
        > 0)
}

/// Perp markets a bankruptcy resolver may write to beyond the market being resolved, so a handler
/// can declare them writable before loading its market map (OtterSec #145).
///
/// A resolver writes to every market in this list twice over. It settles what the market's PnL pool
/// can pay into the estate's quote deposit, and it forfeits the remainder to the market's
/// `pending_if_fee`. A market passed read-only fails `load_mut` deep inside the resolver, so both
/// resolve handlers derive their writable perp-market set from here.
///
/// The list does not filter on payability. A claim that the pool can pay when the transaction is
/// built may be unfundable by the time it lands, and the reverse also happens. The shared
/// [`is_settled_positive_claim`] filter is what stops the handler's declared set and the resolver's
/// actual writes from drifting apart.
pub fn perp_markets_with_forfeitable_claims(user: &User) -> Vec<u16> {
    user.perp_positions
        .iter()
        .filter(|position| is_settled_positive_claim(position))
        .map(|position| position.market_index)
        .collect()
}

/// Returns true if the user still has an unresolved cross-margin perp
/// bankruptcy: a non-isolated perp position carrying bad debt (no base, no open
/// order, negative unsettled pnl). Used to enforce the deterministic
/// perp-before-spot bankruptcy-resolution precedence (audit #52) so a public
/// caller cannot pick which resolver drains the shared (quote) insurance fund
/// first and thereby shift socialized loss between perp and spot stakeholders.
pub fn has_pending_cross_margin_perp_bankruptcy(user: &User) -> bool {
    user.perp_positions.iter().any(|perp_position| {
        !perp_position.is_isolated()
            && perp_position.base_asset_amount == 0
            && !perp_position.has_open_order()
            && perp_position.quote_asset_amount < 0
    })
}

pub fn is_isolated_margin_bankrupt(user: &User, market_index: u16) -> VelocityResult<bool> {
    let perp_position = user.get_isolated_perp_position(market_index)?;

    if perp_position.isolated_position_scaled_balance > 0 {
        return Ok(false);
    }

    Ok(perp_position.base_asset_amount == 0
        && perp_position.quote_asset_amount < 0
        && !perp_position.has_open_order())
}
