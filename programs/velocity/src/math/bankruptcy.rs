use crate::{
    error::VelocityResult,
    math::{casting::Cast, spot_balance::get_token_amount},
    state::{spot_market::SpotBalanceType, spot_market_map::SpotMarketMap, user::User},
};

#[cfg(test)]
mod tests;

/// Whether a user qualifies to be flagged bankrupt on the cross-margin side:
/// spot liabilities, no realizable spot assets, and no realizable perp exposure.
///
/// OtterSec #151: a deposit row was vetoed on `scaled_balance > 0` alone. A full
/// spot-market socialization floors `cumulative_deposit_interest` at 1 and leaves each
/// wiped depositor's scaled row positive, so the row survives while its *token* amount
/// is zero. That worthless row blocked bankruptcy admission (and PnL liquidation) for a
/// user with unrelated cross-margin debt, stalling the next bad-debt repair. It is now
/// measured in tokens.
///
/// Deliberately narrow: a deposit worth >= 1 token still vetoes, so this only ever
/// admits bankruptcy for a row that genuinely cannot be realized at all.
///
/// **OtterSec #145 is deliberately NOT addressed here.** That finding asks for a
/// positive perp quote to stop vetoing when the market's PnL pool cannot pay it, but
/// payability is the wrong proxy: an empty PnL pool is the normal state for a market
/// whose counterparty losses have not settled yet, so keying off it would declare a
/// user with large, real unrealized profit bankrupt. It needs a net-insolvency notion
/// rather than a per-market pool check, which is a different (and larger) change than
/// this structural predicate — see the tracker.
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
                // #151: measure the row in tokens, not scaled balance.
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

    for perp_position in user.perp_positions.iter() {
        // Skip isolated perp positions - they are handled by is_isolated_margin_bankrupt
        if perp_position.is_isolated() {
            continue;
        }

        if perp_position.base_asset_amount != 0
            || perp_position.quote_asset_amount > 0
            || perp_position.has_open_order()
        {
            return Ok(false);
        }

        if perp_position.quote_asset_amount < 0 {
            has_liability = true;
        }
    }

    Ok(has_liability)
}

/// Whether the user still holds a spot deposit that could be applied to their bad debt.
///
/// OtterSec #130: the bankruptcy latch is a *snapshot* of "nothing left to seize", and the resolvers
/// read only the liability row. Assets can still arrive after the latch is set — the revenue-share
/// sweep is permissionless, and keeper filler rewards credit the filler with no bankruptcy check —
/// and once it is set, `settle_pnl` and `liquidate_spot` both reject the user, so nothing can apply
/// that asset to the debt. Resolving anyway socializes a loss the estate could have covered, and the
/// asset becomes withdrawable as soon as the resolver clears the latch.
///
/// Deliberately much narrower than [`is_cross_margin_bankrupt`]. That predicate also vetoes on an
/// open order or on base exposure, which are "not yet in a resolvable state" conditions rather than
/// realizable assets — the resolvers are legitimately reached with orders still open, so re-deriving
/// the full predicate would block resolutions that have nothing to do with this finding.
///
/// Scoped to **spot deposits** on purpose. A *quote* deposit is set off directly by
/// `resolve_perp_bankruptcy` before it draws; this covers the residue that setoff cannot reach,
/// namely a *non-quote* deposit (netting that against a quote debt would be a cross-asset swap, not
/// a balance transfer). A positive perp `quote_asset_amount` in another market is **not** counted
/// here — that is OtterSec #145's subject, where an *unfundable* claim must be extinguished into the
/// market's insurance tranche rather than merely deferred. Widen this to perp quotes only together
/// with that change, or the two will disagree.
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

        // Measured in tokens, not scaled balance, for the same reason as #151: a fully socialized
        // market leaves a positive scaled row whose token value is zero, and that worthless residue
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
