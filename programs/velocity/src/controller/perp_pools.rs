//! Market-level pool accounting: pnl pool ↔ user settles plus the streaming
//! fee sweep (pnl pool → revenue pool / protocol fee pool / AMM fee pool).
//!
//! The pnl pool is where trade-fee value materializes (fees debit the payer's
//! position; tokens arrive as fills settle), so the sweep sources every fee
//! carveout from the pnl pool's surplus over live user claims. The AMM's
//! ledger and token pool are never used as a conduit for non-AMM money — the
//! only AMM-touching step is the tokenization of its own already-booked fee
//! provision.
//!
//! Re-exported from `crate::vlp::amm::controller::*` so `use crate::vlp::amm::controller::*`
//! still resolves these symbols.

use {
    crate::{
        controller::spot_balance::{
            transfer_spot_balance_to_revenue_pool, transfer_spot_balances, update_spot_balances,
        },
        error::{ErrorCode, VelocityResult},
        math::{
            casting::Cast,
            oracle::{is_oracle_valid_for_action, VelocityAction},
            safe_math::SafeMath,
            spot_balance::get_token_amount,
            spot_withdraw::{
                get_max_withdraw_for_market_with_token_amount, validate_spot_balances,
            },
        },
        msg,
        state::{
            events::PerpMarketFeeSweepRecord,
            market_status::MarketStatus,
            oracle_map::OracleMap,
            paused_operations::PerpOperation,
            perp_market::PerpMarket,
            spot_market::{SpotBalance, SpotBalanceType, SpotMarket},
            state::State,
            user::{MarketType, User},
        },
        validate,
    },
    anchor_lang::prelude::*,
    std::cmp::min,
};

/// Materializes accrued pending fees out of the pnl pool. This is the
/// streaming sweep. The sweep drains only the pool's surplus over live user
/// claims. The drains run in seniority order, so the senior claim is paid
/// first when the pool is short:
///   1. `pending_protocol_fee` to `protocol_fee_pool`, which is withdrawable.
///   2. `pending_if_fee` to the quote `SpotMarket.revenue_pool` for insurance.
///      This drain leaves `get_pending_if_fee_floor()` behind. The floor is
///      the first-loss tranche `resolve_perp_bankruptcy` can always reach, so
///      a permissionless sweep cannot drain it ahead of a bankruptcy
///      resolution. A latched bankruptcy holds the whole counter. Before any
///      latch, a percentage of open-interest notional stands.
///   3. `pending_amm_provision` to `amm.fee_pool`. This tokenizes a provision
///      the AMM already booked at fill, so the ledger does not change.
///
/// `force` bypasses the `SettleRevPool` operation pause. It exists for the
/// final sweep on market delisting: that is the last chance to route the
/// protocol carveout to `protocol_fee_pool` before the remaining pnl pool is
/// drained to the revenue pool, so a standing pause must not strand it. The
/// streaming/keeper callers pass `false` and continue to respect the pause.
///
/// Returns `(if_swept, protocol_swept, amm_provision_tokenized)`.
pub fn sweep_market_fees(
    market: &mut PerpMarket,
    spot_market: &mut SpotMarket,
    net_user_pnl: i128,
    now: i64,
    force: bool,
) -> VelocityResult<(u128, u128, u128)> {
    // market can perform withdraw from revenue pool
    market
        .insurance_claim
        .reset_revenue_withdraw_for_new_period(
            spot_market.insurance_fund.last_revenue_settle_ts,
            now,
        )?;

    if (!force && market.is_operation_paused(PerpOperation::SettleRevPool))
        || (market.fee_ledger.pending_protocol_fee == 0
            && market.fee_ledger.pending_if_fee == 0
            && market.fee_ledger.pending_amm_provision == 0)
    {
        return Ok((0, 0, 0));
    }

    let pnl_pool_tokens = get_token_amount(
        market.pnl_pool.balance(),
        spot_market,
        market.pnl_pool.balance_type(),
    )?;

    // Every drain leaves `max(net_user_pnl, 0)`, the floored IF bankruptcy
    // tranche (`min(pending_if_fee, get_pending_if_fee_floor())`), and
    // `pending_revenue_share` backed in the pnl pool. Unbacking any of them
    // strands a bankruptcy promise or temporarily leaves a payable unpaid.
    let reserved_claims: u128 = net_user_pnl
        .max(0)
        .cast::<u128>()?
        .safe_add(market.get_bankruptcy_if_tranche_reservation(force)?)?
        .safe_add(market.pending_revenue_share.cast::<u128>()?)?;
    let mut available_unbuffered: u128 = pnl_pool_tokens.saturating_sub(reserved_claims);

    let protocol_drain = market // Unlike the drains below, this one ignores the buffer.
        .fee_ledger
        .pending_protocol_fee
        .min(available_unbuffered);
    if protocol_drain > 0 {
        transfer_spot_balances(
            protocol_drain.cast()?,
            spot_market,
            &mut market.pnl_pool,
            &mut market.protocol_fee_pool,
        )?;
        market.fee_ledger.consume_pending_protocol(protocol_drain)?;
        available_unbuffered = available_unbuffered.safe_sub(protocol_drain)?;
    }

    let mut available: u128 = // The remaining drains also leave the retention buffer behind.
        available_unbuffered.saturating_sub(market.fee_pool_buffer_target.cast()?);

    // Insurance cut to the revenue pool, buffered and floored. The sweep is
    // permissionless, so a complete drain would let anyone front-run a
    // bankruptcy resolution and push the loss onto the fund. A latched
    // bankruptcy holds the counter. Before latch, a floor sized to
    // open-interest notional at the oracle TWAP applies. `force` drops it.
    let if_drain = market
        .fee_ledger
        .pending_if_fee
        .saturating_sub(market.get_pending_if_fee_floor(force)?)
        .min(available);
    if if_drain > 0 {
        transfer_spot_balance_to_revenue_pool(if_drain, spot_market, &mut market.pnl_pool)?;
        market.fee_ledger.consume_pending_if(if_drain)?;
        available = available.safe_sub(if_drain)?;
    }

    // Tokenizes the AMM's fee provision, buffered. Already booked into
    // `total_fee_minus_distributions` at fill, so this only moves tokens.
    let provision_drain = market.fee_ledger.pending_amm_provision.min(available);
    if provision_drain > 0 {
        transfer_spot_balances(
            provision_drain.cast()?,
            spot_market,
            &mut market.pnl_pool,
            &mut market.amm.fee_pool,
        )?;
        market
            .fee_ledger
            .consume_pending_amm_provision(provision_drain)?;
    }

    if if_drain > 0 || protocol_drain > 0 || provision_drain > 0 {
        emit!(PerpMarketFeeSweepRecord {
            ts: now,
            market_index: market.market_index,
            if_swept: if_drain.cast()?,
            protocol_swept: protocol_drain.cast()?,
            amm_provision_tokenized: provision_drain.cast()?,
        });
    }

    Ok((if_drain, protocol_drain, provision_drain))
}

pub fn update_pool_balances(
    market: &mut PerpMarket,
    spot_market: &mut SpotMarket,
    user_quote_token_amount: i128,
    user_unsettled_pnl: i128,
    net_user_pnl: i128,
    now: i64,
) -> VelocityResult<i128> {
    // market pnl pool pays (what it can to) user_unsettled_pnl and pnl_to_settle_to_amm
    let pnl_pool_token_amount = get_token_amount(
        market.pnl_pool.balance(),
        spot_market,
        market.pnl_pool.balance_type(),
    )?;

    let pnl_to_settle_with_user = if user_unsettled_pnl > 0 {
        min(user_unsettled_pnl, pnl_pool_token_amount.cast::<i128>()?)
    } else {
        // dont settle negative pnl to spot borrows when utilization is high (> 80%)
        let max_withdraw_amount = -get_max_withdraw_for_market_with_token_amount(
            spot_market,
            user_quote_token_amount,
            false,
        )?
        .cast::<i128>()?;

        max_withdraw_amount.max(user_unsettled_pnl)
    };

    let pnl_to_settle_with_market = -(pnl_to_settle_with_user);

    update_spot_balances(
        pnl_to_settle_with_market.unsigned_abs(),
        if pnl_to_settle_with_market >= 0 {
            &SpotBalanceType::Deposit
        } else {
            &SpotBalanceType::Borrow
        },
        spot_market,
        &mut market.pnl_pool,
        false,
    )?;

    // sweep AFTER the user's settle: both draw the pnl pool now, and the
    // sweep must not starve the settle that triggered it. The settle just
    // moved `pnl_to_settle_with_user` out of (or into) aggregate user claims.
    let net_user_pnl_after = net_user_pnl.safe_sub(pnl_to_settle_with_user)?;
    sweep_market_fees(market, spot_market, net_user_pnl_after, now, false)?;

    let _depositors_claim = validate_spot_balances(spot_market)?;

    Ok(pnl_to_settle_with_user)
}

pub fn update_pnl_pool_and_user_balance(
    market: &mut PerpMarket,
    quote_spot_market: &mut SpotMarket,
    user: &mut User,
    unrealized_pnl_with_fee: i128,
) -> VelocityResult<i128> {
    let pnl_to_settle_with_user = if unrealized_pnl_with_fee > 0 {
        unrealized_pnl_with_fee.min(
            get_token_amount(
                market.pnl_pool.scaled_balance,
                quote_spot_market,
                market.pnl_pool.balance_type(),
            )?
            .cast()?,
        )
    } else {
        unrealized_pnl_with_fee
    };

    validate!(
        unrealized_pnl_with_fee == pnl_to_settle_with_user,
        ErrorCode::InsufficientPerpPnlPool,
        "pnl_pool_amount doesnt have enough ({} < {})",
        pnl_to_settle_with_user,
        unrealized_pnl_with_fee
    )?;

    if unrealized_pnl_with_fee == 0 {
        msg!(
            "User has no unsettled pnl for market {}",
            market.market_index
        );
        return Ok(0);
    } else if pnl_to_settle_with_user == 0 {
        msg!(
            "Pnl Pool cannot currently settle with user for market {}",
            market.market_index
        );
        return Ok(0);
    }

    let is_isolated_position = user.get_perp_position(market.market_index)?.is_isolated();
    if is_isolated_position {
        let perp_position = user.force_get_isolated_perp_position_mut(market.market_index)?;
        let perp_position_token_amount =
            perp_position.get_isolated_token_amount(quote_spot_market)?;

        if pnl_to_settle_with_user < 0 {
            validate!(
                perp_position_token_amount >= pnl_to_settle_with_user.unsigned_abs(),
                ErrorCode::InsufficientCollateral,
                "user has insufficient deposit for market {}",
                market.market_index
            )?;
        }

        transfer_spot_balances(
            pnl_to_settle_with_user,
            quote_spot_market,
            &mut market.pnl_pool,
            perp_position,
        )?;
    } else {
        let user_spot_position = user.get_quote_spot_position_mut();

        transfer_spot_balances(
            pnl_to_settle_with_user,
            quote_spot_market,
            &mut market.pnl_pool,
            user_spot_position,
        )?;
    }

    Ok(pnl_to_settle_with_user)
}

/// Returns the price at which a permissionless pnl-pool drain must value `max(net_user_pnl, 0)`.
///
/// Every such drain keeps that amount in the pool to back the positive pnl of other users, so the
/// price decides how much the caller may take. `sweep_market_fees` and
/// `sweep_completed_revenue_share_for_market` both reserve against it, and they must agree. Two
/// prices that value the same claim differently would let one path take value that the other keeps.
///
/// In Settlement, `settle_expired_position` pays users at `expiry_price`, so the reserve uses that
/// price. `expiry_price` does not change after settlement, so it needs no validity check. A live
/// price below `expiry_price` on a net-long market would make the reserve too small, and the
/// expiry claims would later fail with `InsufficientPerpPnlPool`.
///
/// Outside Settlement this returns the live oracle price. It applies the same validity checks that
/// `settle_pnl` applies. A stale or divergent price must not size the reserve.
pub fn get_pnl_pool_drain_reserve_price(
    perp_market: &PerpMarket,
    state: &State,
    oracle_map: &mut OracleMap,
) -> VelocityResult<i64> {
    if perp_market.status == MarketStatus::Settlement {
        return Ok(perp_market.expiry_price);
    }

    let market_index = perp_market.market_index;
    let oracle_price_data = *oracle_map.get_price_data(&perp_market.oracle_id())?;
    let oracle_price = oracle_price_data.price;

    crate::controller::orders::validate_market_within_price_band(perp_market, state, oracle_price)?;

    if !perp_market.amm.is_curve_update_enabled() {
        return Ok(oracle_price);
    }

    if perp_market.is_recent_oracle_valid(oracle_map.slot, &oracle_price_data)? {
        return Ok(oracle_price);
    }

    let (_, oracle_validity) = oracle_map.get_price_data_and_validity(
        MarketType::Perp,
        market_index,
        &perp_market.oracle_id(),
        perp_market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap,
        perp_market.get_max_confidence_interval_multiplier()?,
        0,
        0,
        None,
    )?;

    if is_oracle_valid_for_action(oracle_validity, Some(VelocityAction::SettlePnl))?
        && perp_market.is_price_divergence_ok_for_settle_pnl(oracle_price)?
    {
        return Ok(oracle_price);
    }

    validate!(
        perp_market.market_stats.last_oracle_valid,
        oracle_validity.get_error_code(),
        "Oracle Price detected as invalid ({}) on last perp market update for Market = {}",
        oracle_validity,
        market_index
    )?;

    validate!(
        perp_market.amm.is_fresh_at(oracle_map.slot),
        ErrorCode::AMMNotUpdatedInSameSlot,
        "Market={} AMM must be updated in a prior instruction within same slot (current={} != amm={}, last_oracle_valid={})",
        market_index,
        oracle_map.slot,
        perp_market.amm.last_update_slot(),
        perp_market.market_stats.last_oracle_valid
    )?;

    // Both cached attestations hold, so `is_recent_oracle_valid` can only be false because the
    // samples do not match. A write changed the oracle account after the AMM update in this slot.
    // The cached verdict does not cover the sample this call reads, so the current validity
    // decides.
    msg!(
        "Market={} oracle rewritten after same-slot AMM update; current sample is invalid ({})",
        market_index,
        oracle_validity
    );
    Err(oracle_validity.get_error_code())
}
