use crate::{
    error::VelocityResult,
    math::{casting::Cast, safe_math::SafeMath, spot_balance::get_token_amount},
    state::{
        perp_market_map::PerpMarketMap,
        spot_market::{SpotBalance, SpotBalanceType},
        spot_market_map::SpotMarketMap,
        user::User,
    },
};

#[cfg(test)]
mod tests;

/// Whether the user qualifies as cross-margin bankrupt: it has spot liabilities, no realizable spot
/// assets, and no realizable perp exposure.
///
/// OtterSec #151. A deposit row is measured in tokens, not in `scaled_balance`. A full spot-market
/// socialization floors `cumulative_deposit_interest` at 1. Each wiped depositor keeps a positive
/// scaled row that is worth zero tokens. Such a row used to block bankruptcy admission and PnL
/// liquidation, and stalled the next bad-debt repair. A deposit worth 1 token or more still vetoes.
///
/// OtterSec #145. A positive perp `quote_asset_amount` no longer vetoes on its own. It vetoes only
/// while the market's PnL pool can pay part of it, and only while the estate is net solvent. Each
/// check has its own note below. The unfundable remainder is not ignored, because an unfunded claim
/// is still owed. Both resolvers move that remainder into the market's insurance tranche.
pub fn is_cross_margin_bankrupt(
    user: &User,
    spot_market_map: &SpotMarketMap,
    perp_market_map: &PerpMarketMap,
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

    let quote_spot_market = spot_market_map.get_quote_spot_market()?;
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

        if quote > 0 {
            // #145: a positive claim vetoes only while it is realizable.
            //
            // A pool that cannot pay the claim used to strand a resolvable loss in another market
            // forever. The pool fills only as counterparty losses settle, which can never happen.
            // Until it fills, the claim cannot become a deposit, so the veto never clears.
            //
            // While the pool can pay part of the claim, keep vetoing. That part settles through the
            // ordinary pipeline (settle -> deposit -> `liquidate_perp_pnl_for_deposit`), which uses
            // no insurance. The resolver forfeits the remainder to the insurance tranche.
            let perp_market = perp_market_map.get_ref(&perp_position.market_index)?;
            let pnl_pool_tokens = get_token_amount(
                perp_market.pnl_pool.balance(),
                &quote_spot_market,
                perp_market.pnl_pool.balance_type(),
            )?;

            if pnl_pool_tokens > 0 {
                return Ok(false);
            }
        } else if quote < 0 {
            has_liability = true;
        }

        net_perp_quote = net_perp_quote.safe_add(quote.cast()?)?;
    }

    // #145: admit only a NET insolvent estate. Unpayability alone is not sufficient.
    //
    // Without this gate, +5000 in market A against -1000 in market B is admitted. The resolver then
    // forfeits all 5000 to cover 1000, and confiscates 4000 the user is owed.
    //
    // The sum is exact, not an approximation. Every position that reaches here has
    // `base_asset_amount == 0`, so its whole value is its `quote_asset_amount`. No oracle is needed.
    // The gate also bounds the forfeit: net <= 0 means total claims <= total debt.
    if net_perp_quote > 0 {
        return Ok(false);
    }

    Ok(has_liability)
}

/// Whether the user still holds a spot deposit that can pay part of its bad debt.
///
/// OtterSec #130. The bankruptcy latch records that nothing was left to seize when liquidation set
/// it. Assets can arrive after that. The revenue-share sweep is permissionless, and keeper filler
/// rewards credit the filler with no bankruptcy check. Once the latch is set, `settle_pnl` and
/// `liquidate_spot` both reject the user, and the resolvers read only the liability row. The asset
/// therefore pays nothing, insurance covers the whole debt, and the asset becomes withdrawable when
/// the resolver clears the latch.
///
/// This is much narrower than [`is_cross_margin_bankrupt`]. That predicate also vetoes on an open
/// order or on base exposure. Those conditions mean "not resolvable yet", not "holds an asset". The
/// resolvers are reached with orders open, so the full predicate would block valid resolutions.
///
/// Scope is spot deposits only. `resolve_perp_bankruptcy` sets off a *quote* deposit directly before
/// it draws, so this covers the residue it cannot reach: a *non-quote* deposit. Netting that against
/// a quote debt needs a cross-asset swap, not a balance transfer.
///
/// A positive perp `quote_asset_amount` is not counted here. OtterSec #145 handles it, and forfeits
/// an unfundable claim to the insurance tranche instead of deferring it. Widen this to perp quotes
/// only together with that path, or the two disagree.
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

/// Perp markets whose settled positive claims a bankruptcy resolver may forfeit to the insurance
/// tranche, so a handler can declare them writable before loading its market map (OtterSec #145).
///
/// `extinguish_unfundable_perp_claims` writes to every market in this list: it debits the user's
/// claim and credits the market's `pending_if_fee`. A market passed read-only fails `load_mut` deep
/// inside the resolver, so both resolve handlers derive their writable perp-market set from here.
/// Fundability is deliberately NOT filtered on: a claim that the pool can pay when the transaction
/// is built may be unfundable by the time it lands, so every settled positive claim must be
/// writable. Keeping the position filter here rather than inline in the resolver is what stops the
/// handler's declared set and the resolver's actual writes from drifting apart.
pub fn perp_markets_with_forfeitable_claims(user: &User) -> Vec<u16> {
    user.perp_positions
        .iter()
        .filter(|position| {
            !position.is_isolated()
                && position.quote_asset_amount > 0
                && position.base_asset_amount == 0
                && !position.has_open_order()
        })
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
