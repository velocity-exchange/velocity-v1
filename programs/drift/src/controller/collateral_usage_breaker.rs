//! Stateful orchestration of the per-`SpotMarket` collateral-usage circuit breaker.
//!
//! The breaker maintains a single invariant for every `(user, spot_market)` pair:
//!
//! ```text
//!     spot_position.collateral_usage_contribution
//!         == if !user.is_collateral_usage_circuit_breaker_exempt()
//!         &&    user.has_any_liability()
//!         &&    spot_position.balance_type == Deposit
//!         &&    spot_position.scaled_balance > 0
//!         {  spot_position.scaled_balance }
//!         else { 0 }
//! ```
//!
//! Any state change that could violate this invariant (spot balance change, perp
//! position open/close, isolated deposit/withdraw, order add/cancel, etc.) calls
//! [`reconcile_user_collateral_usage`] which walks all 8 spot positions, computes
//! the expected contribution for each, and applies the delta to
//! `spot_market.collateral_usage` while keeping the position's warmup timestamps
//! consistent.

use std::cell::RefMut;

use crate::error::DriftResult;
use crate::math::circuit_breaker::split_delta_at_threshold;
use crate::state::perp_market_map::PerpMarketMap;
use crate::state::spot_market::{SpotBalanceType, SpotMarket};
use crate::state::spot_market_map::SpotMarketMap;
use crate::state::user::User;

/// Compute the expected `collateral_usage_contribution` for a single position
/// given the user's current overall state. Reflects the breaker's invariant.
fn expected_contribution(
    user_is_exempt: bool,
    user_has_liability: bool,
    balance_type: SpotBalanceType,
    scaled_balance: u64,
) -> u64 {
    if user_is_exempt
        || !user_has_liability
        || balance_type != SpotBalanceType::Deposit
        || scaled_balance == 0
    {
        0
    } else {
        scaled_balance
    }
}

/// Walk every spot position on `user`, compute the expected contribution to its
/// market's `collateral_usage`, and apply the delta to the market counter.
/// Concurrently keeps each position's `(warmup_start_ts, warmup_end_ts)` in sync
/// (rebasing on increase, clearing on decrease-to-zero).
///
/// Idempotent — a position whose contribution is already correct is a no-op.
///
/// **Lenient on unloaded markets.** If a market the user has a position in is
/// not in the writable set of this instruction, that position is skipped here
/// — its persisted counter / stamp will be stale until the next interaction
/// touches that market with writable access. This is *safe* for hack-mitigation
/// because [`SpotPosition::collateral_usage_weight_discount_factor_with_hypothetical_stamp`]
/// applies a conservative dynamic discount on the read path that does not
/// depend on the persisted stamp. We can't reliably enforce "all markets
/// writable" here without breaking many existing instruction shapes; the
/// hypothetical-stamp logic on margin reads is what carries the security
/// guarantee.
pub fn reconcile_user_collateral_usage(
    spot_market_map: &SpotMarketMap,
    user: &mut User,
    now: i64,
) -> DriftResult<()> {
    reconcile_user_collateral_usage_with_perp_map(spot_market_map, None, user, now)
}

/// Same as [`reconcile_user_collateral_usage`] but also walks isolated perp
/// positions when `perp_market_map` is provided. Isolated deposits (held on
/// `PerpPosition::isolated_position_scaled_balance`) feed the
/// `collateral_usage` counter on the perp's quote spot market. Pass `None`
/// when there's no perp activity to track in this instruction.
pub fn reconcile_user_collateral_usage_with_perp_map(
    spot_market_map: &SpotMarketMap,
    perp_market_map: Option<&PerpMarketMap>,
    user: &mut User,
    now: i64,
) -> DriftResult<()> {
    let user_is_exempt = user.is_collateral_usage_circuit_breaker_exempt();
    let user_has_liability = user.has_any_liability();

    // Spot positions: the user's deposits backed by overall liability (cross).
    for position_index in 0..user.spot_positions.len() {
        let market_index = user.spot_positions[position_index].market_index;
        if user.spot_positions[position_index].is_available()
            && user.spot_positions[position_index].collateral_usage_contribution == 0
        {
            continue;
        }
        let market_ref_mut: RefMut<SpotMarket> = match spot_market_map.get_ref_mut(&market_index) {
            Ok(m) => m,
            Err(_) => continue,
        };
        reconcile_one_position(
            market_ref_mut,
            user_is_exempt,
            user_has_liability,
            &mut user.spot_positions[position_index],
            now,
        )?;
    }

    // Isolated perp positions: the user's isolated quote-token deposits feed
    // the spot market's counter (no per-position warmup discount in v1).
    if let Some(perp_market_map) = perp_market_map {
        for perp_index in 0..user.perp_positions.len() {
            let perp_position = &user.perp_positions[perp_index];
            // Skip if there's nothing to track and nothing to undo.
            if perp_position.isolated_position_scaled_balance == 0
                && perp_position.isolated_collateral_usage_contribution == 0
            {
                continue;
            }
            // Determine the quote spot market for this perp.
            let quote_spot_market_index = match perp_market_map
                .get_ref(&perp_position.market_index)
            {
                Ok(m) => m.quote_spot_market_index,
                Err(_) => continue,
            };
            // Acquire writable handle to the quote spot market.
            let mut spot_market =
                match spot_market_map.get_ref_mut(&quote_spot_market_index) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
            reconcile_isolated_perp_contribution(
                &mut spot_market,
                user_is_exempt,
                &mut user.perp_positions[perp_index],
                now,
            )?;
        }
    }

    Ok(())
}

/// Reconcile the isolated portion of a perp position against its quote spot
/// market. Isolated positions feed the counter but do not receive a warmup
/// discount in v1 — this writes back the contribution to keep the counter
/// consistent for trigger detection.
fn reconcile_isolated_perp_contribution(
    spot_market: &mut SpotMarket,
    user_is_exempt: bool,
    perp_position: &mut crate::state::user::PerpPosition,
    now: i64,
) -> DriftResult<()> {
    let expected = if user_is_exempt {
        0
    } else {
        perp_position.isolated_position_scaled_balance
    };
    let current = perp_position.isolated_collateral_usage_contribution;
    if expected == current {
        return Ok(());
    }

    spot_market.update_collateral_usage_twap(now)?;
    if expected > current {
        let delta = expected - current;
        spot_market.collateral_usage = spot_market.collateral_usage.saturating_add(delta);
    } else {
        let delta = current - expected;
        spot_market.collateral_usage = spot_market.collateral_usage.saturating_sub(delta);
    }
    perp_position.isolated_collateral_usage_contribution = expected;
    Ok(())
}

/// Reconcile a single position against its market. Used internally by
/// `reconcile_user_collateral_usage` and by hooks that already hold a writable
/// market reference (e.g. `update_spot_balances` callers).
pub fn reconcile_one_position(
    mut spot_market: RefMut<SpotMarket>,
    user_is_exempt: bool,
    user_has_liability: bool,
    position: &mut crate::state::user::SpotPosition,
    now: i64,
) -> DriftResult<()> {
    let expected = expected_contribution(
        user_is_exempt,
        user_has_liability,
        position.balance_type,
        position.scaled_balance,
    );
    let current = position.collateral_usage_contribution;

    if expected == current {
        return Ok(());
    }

    // Tick TWAP before mutating the counter so the threshold below uses the
    // pre-mutation snapshot.
    spot_market.update_collateral_usage_twap(now)?;

    if expected > current {
        let delta = expected - current;
        let pre_market_usage = spot_market.collateral_usage;
        let threshold = spot_market.collateral_usage_threshold_amount();
        let (delta_below, delta_above) =
            split_delta_at_threshold(pre_market_usage, delta, threshold);

        // Apply to market counter (saturating — the breaker should not panic the
        // protocol on overflow; any cap would already be enforced by static gates).
        spot_market.collateral_usage = spot_market.collateral_usage.saturating_add(delta);

        // Rebase the position's warmup timestamps to interpolate effective
        // collateral correctly across the increase.
        position.rebase_collateral_usage_warmup_for_increase(
            &spot_market,
            current,
            expected,
            delta_below,
            delta_above,
            now,
        )?;
    } else {
        let delta = current - expected;
        spot_market.collateral_usage = spot_market.collateral_usage.saturating_sub(delta);
        if expected == 0 {
            position.clear_collateral_usage_warmup();
        }
        // Note: partial decreases (current > expected > 0) keep the warmup
        // timestamps untouched — the position is still maturing on its existing
        // schedule, just at a smaller balance.
    }

    position.collateral_usage_contribution = expected;
    Ok(())
}
