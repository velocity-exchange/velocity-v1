use {
    crate::{
        controller::spot_balance,
        math::{casting::Cast, safe_math::SafeMath, spot_balance::get_token_amount},
        state::{
            events::{emit_stack, RevenueShareSettleRecord},
            paused_operations::PerpOperation,
            perp_market_map::PerpMarketMap,
            revenue_share::{RevenueShareEscrowZeroCopyMut, RevenueShareOrder},
            revenue_share_map::RevenueShareMap,
            spot_market::SpotBalance,
            spot_market_map::SpotMarketMap,
            traits::Size,
            user::MarketType,
        },
        vlp::amm::math::amm::calculate_net_user_pnl,
    },
    anchor_lang::prelude::*,
};

#[cfg(test)]
mod tests;

/// Runs through the user's RevenueShareEscrow account and sweeps any accrued fees to the corresponding
/// builders and referrer.
///
/// `oracle_price` is the market's oracle price, already validity-gated in-slot by the `settle_pnl`
/// that runs immediately before this sweep at every call site. It is used to value
/// `net_user_pnl` so builder/referrer payouts can only draw the PnL pool's excess *over* live
/// positive user claims — never the tokens backing a third party's positive PnL.
pub fn sweep_completed_revenue_share_for_market<'a>(
    market_index: u16,
    revenue_share_escrow: &mut RevenueShareEscrowZeroCopyMut,
    perp_market_map: &PerpMarketMap<'a>,
    spot_market_map: &SpotMarketMap<'a>,
    revenue_share_map: &RevenueShareMap<'a>,
    now_ts: i64,
    oracle_price: i64,
    builder_codes_feature_enabled: bool,
    funding_paused: bool,
) -> crate::error::VelocityResult<()> {
    let perp_market = &mut perp_market_map.get_ref_mut(&market_index)?;

    // This is a revenue routing path out of the perp market's pnl pool, the
    // same conduit `sweep_market_fees` drains. Respect the market's
    // `SettleRevPool` pause so a paused market can't have its pnl pool swept to
    // builders/referrers while the direct fee sweep is halted.
    if perp_market.is_operation_paused(PerpOperation::SettleRevPool) {
        return Ok(());
    }

    let quote_spot_market = &mut spot_market_map.get_quote_spot_market_mut()?;

    spot_balance::update_spot_market_cumulative_interest(
        quote_spot_market,
        None,
        now_ts,
        funding_paused,
    )?;

    // Amount this permissionless sweep must leave in the PnL pool, mirroring
    // the reservation the protocol fee sweep applies
    // (`controller::perp_pools::sweep_market_fees`):
    //   * `max(net_user_pnl, 0)`: the PnL pool backs users' positive unsettled
    //     PnL, so paying revenue share out of it would leave a third party's
    //     settlement short (audit #48).
    //   * the floored IF bankruptcy tranche (`min(pending_if_fee,
    //     get_bankruptcy_if_floor())`): `resolve_perp_bankruptcy` consumes
    //     `pending_if_fee` counter-only, so a revenue-share payout must not
    //     drain the tokens backing the standing tranche the #245 floor
    //     promises either (same class as audit #53 on the protocol sweep).
    // This sweep does NOT reserve `pending_revenue_share` — it is the payer of
    // that claim, and decrements the counter as it pays below.
    let reserved_user_claims: u128 = calculate_net_user_pnl(
        &perp_market.amm,
        oracle_price,
        perp_market.quote_asset_amount,
        perp_market.net_unsettled_funding_pnl,
    )?
    .max(0)
    .cast::<u128>()?
    .safe_add(perp_market.get_bankruptcy_if_tranche_reservation(false)?)?;

    let orders_len = revenue_share_escrow.orders_len();
    for i in 0..orders_len {
        let (
            is_completed,
            is_referral_order,
            order_market_type,
            order_market_index,
            fees_accrued,
            builder_idx,
        ) = {
            let ord_ro = match revenue_share_escrow.get_order(i) {
                Ok(o) => o,
                Err(_) => {
                    continue;
                }
            };
            (
                ord_ro.is_completed(),
                ord_ro.is_referral_order(),
                ord_ro.market_type,
                ord_ro.market_index,
                ord_ro.fees_accrued,
                ord_ro.builder_idx,
            )
        };

        if is_referral_order {
            if fees_accrued == 0
                || !(order_market_type == MarketType::Perp && order_market_index == market_index)
            {
                continue;
            }
        } else if !(is_completed
            && order_market_type == MarketType::Perp
            && order_market_index == market_index
            && fees_accrued > 0)
        {
            continue;
        }

        let pnl_pool_token_amount = get_token_amount(
            perp_market.pnl_pool.scaled_balance,
            quote_spot_market,
            perp_market.pnl_pool.balance_type(),
        )?;

        // Only the PnL pool's excess over live positive user claims is available to
        // pay revenue share; the reserved portion must stay to back user settlements.
        let available_for_sweep = pnl_pool_token_amount.saturating_sub(reserved_user_claims);

        if available_for_sweep < fees_accrued as u128 {
            msg!(
                "market {} PNL pool has insufficient available balance to sweep fees for builder. pnl_pool_token_amount: {}, reserved_user_claims: {}, available: {}, fees_accrued: {}",
                market_index,
                pnl_pool_token_amount,
                reserved_user_claims,
                available_for_sweep,
                fees_accrued
            );
            break;
        }

        if is_referral_order {
            let referrer_authority =
                if let Some(referrer_authority) = revenue_share_escrow.get_referrer() {
                    referrer_authority
                } else {
                    continue;
                };

            let referrer_user = revenue_share_map.get_user_ref_mut(&referrer_authority);
            let referrer_rev_share =
                revenue_share_map.get_revenue_share_account_mut(&referrer_authority);

            if let (Ok(mut referrer_user), Ok(mut referrer_rev_share)) =
                (referrer_user, referrer_rev_share)
            {
                // A vault-owned beneficiary must never receive revenue share into
                // its NAV-priced User: the reward would enter vault equity at this
                // attacker-controlled sweep time and mis-split depositor value —
                // late-entrant dilution, stranded withdrawer, or a donation that
                // burns a canceller's claim (OtterSec #91/#92/#93). There is no
                // legitimate flow where a vault earns revenue share, so forfeit the
                // reward to the market's pnl pool: drain the liability counter and
                // clear the row without transferring.
                if referrer_user.is_vault_owned() {
                    perp_market.settle_pending_revenue_share(fees_accrued)?;
                    if let Ok(builder_order) = revenue_share_escrow.get_order_mut(i) {
                        builder_order.fees_accrued = 0;
                    }
                    continue;
                }

                spot_balance::transfer_spot_balances(
                    fees_accrued as i128,
                    quote_spot_market,
                    &mut perp_market.pnl_pool,
                    referrer_user.get_quote_spot_position_mut(),
                )?;

                perp_market.settle_pending_revenue_share(fees_accrued)?;

                referrer_rev_share.total_referrer_rewards = referrer_rev_share
                    .total_referrer_rewards
                    .safe_add(fees_accrued)?;

                emit_stack::<_, { RevenueShareSettleRecord::SIZE }>(RevenueShareSettleRecord {
                    ts: now_ts,
                    builder: None,
                    referrer: Some(referrer_authority),
                    fee_settled: fees_accrued,
                    market_index: order_market_index,
                    market_type: order_market_type,
                    builder_total_referrer_rewards: referrer_rev_share.total_referrer_rewards,
                    builder_total_builder_rewards: referrer_rev_share.total_builder_rewards,
                    builder_sub_account_id: referrer_user.sub_account_id,
                })?;

                // zero out the order
                if let Ok(builder_order) = revenue_share_escrow.get_order_mut(i) {
                    builder_order.fees_accrued = 0;
                }
            }
        } else if builder_codes_feature_enabled {
            let builder_authority = match revenue_share_escrow
                .get_approved_builder_mut(builder_idx)
                .map(|builder| builder.authority)
            {
                Ok(auth) => auth,
                Err(_) => {
                    msg!("failed to get approved_builder from escrow account");
                    continue;
                }
            };

            let builder_user = revenue_share_map.get_user_ref_mut(&builder_authority);
            let builder_rev_share =
                revenue_share_map.get_revenue_share_account_mut(&builder_authority);

            if let (Ok(mut builder_user), Ok(mut builder_revenue_share)) =
                (builder_user, builder_rev_share)
            {
                // See the referral branch: a vault-owned beneficiary is never
                // credited (OtterSec #91/#92/#93). Forfeit to the pnl pool — drain
                // the liability counter and clear the row without transferring.
                if builder_user.is_vault_owned() {
                    perp_market.settle_pending_revenue_share(fees_accrued)?;
                    if let Ok(builder_order) = revenue_share_escrow.get_order_mut(i) {
                        *builder_order = RevenueShareOrder::default();
                    }
                    continue;
                }

                spot_balance::transfer_spot_balances(
                    fees_accrued as i128,
                    quote_spot_market,
                    &mut perp_market.pnl_pool,
                    builder_user.get_quote_spot_position_mut(),
                )?;

                perp_market.settle_pending_revenue_share(fees_accrued)?;

                builder_revenue_share.total_builder_rewards = builder_revenue_share
                    .total_builder_rewards
                    .safe_add(fees_accrued)?;

                emit_stack::<_, { RevenueShareSettleRecord::SIZE }>(RevenueShareSettleRecord {
                    ts: now_ts,
                    builder: Some(builder_authority),
                    referrer: None,
                    fee_settled: fees_accrued,
                    market_index: order_market_index,
                    market_type: order_market_type,
                    builder_total_referrer_rewards: builder_revenue_share.total_referrer_rewards,
                    builder_total_builder_rewards: builder_revenue_share.total_builder_rewards,
                    builder_sub_account_id: builder_user.sub_account_id,
                })?;

                // remove order
                if let Ok(builder_order) = revenue_share_escrow.get_order_mut(i) {
                    *builder_order = RevenueShareOrder::default();
                }
            } else {
                msg!(
                    "Builder user or builder not found for builder authority: {}",
                    builder_authority
                );
            }
        } else {
            msg!("Builder codes nor builder referral feature is not enabled");
        }
    }

    Ok(())
}
