use {
    crate::{
        controller::spot_balance,
        error::{ErrorCode, VelocityResult},
        math::{casting::Cast, safe_math::SafeMath, spot_balance::get_token_amount},
        state::{
            events::{emit_stack, RevenueShareSettleRecord},
            market_status::MarketStatus,
            paused_operations::PerpOperation,
            perp_market::PerpMarket,
            perp_market_map::PerpMarketMap,
            revenue_share::{RevenueShareEscrowZeroCopyMut, RevenueShareOrder},
            revenue_share_map::RevenueShareMap,
            spot_market::{SpotBalance, SpotBalanceType, SpotMarket},
            spot_market_map::SpotMarketMap,
            traits::Size,
            user::MarketType,
        },
        validate,
        vlp::amm::math::amm::{calculate_net_user_cost_basis, calculate_net_user_pnl},
    },
    anchor_lang::prelude::*,
};

#[cfg(test)]
mod tests;

/// Pays the accrued builder and referrer fees in one escrow out of one market's pnl pool.
///
/// Returns the total that this call paid and forfeited. This is informational.
///
/// The caller must validate `oracle_price` in the same slot. `settle_pnl` does this for the settle
/// handlers. `settle_revenue_share` does this itself. The sweep values `net_user_pnl` at that
/// price and keeps that amount in the pool. Payouts can only use the remainder. The pool must
/// still back the positive pnl of other users.
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
) -> crate::error::VelocityResult<u64> {
    let perp_market = &mut perp_market_map.get_ref_mut(&market_index)?;

    // This is a revenue routing path out of the perp market's pnl pool, the
    // same conduit `sweep_market_fees` drains. Respect the market's
    // `SettleRevPool` pause so a paused market can't have its pnl pool swept to
    // builders/referrers while the direct fee sweep is halted.
    if perp_market.is_operation_paused(PerpOperation::SettleRevPool) {
        return Ok(0);
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
    //     get_pending_if_fee_floor())`): `resolve_perp_bankruptcy` consumes
    //     `pending_if_fee` counter-only, so a revenue-share payout must not
    //     drain the tokens that back the tranche either (same class as audit
    //     #53 on the protocol sweep).
    // This sweep does NOT reserve `pending_revenue_share` — it is the payer of
    // that claim, and decrements the counter as it pays below.

    // A builder row must be `Completed` before the sweep pays it. Payment clears the row, and
    // `find_builder_order_index` then loses the open order that accrues to it. In Settlement no
    // fee can accrue, because `fill_perp_order` rejects a market that is not Active or ReduceOnly.
    // The rule is therefore unnecessary here, and it is dropped. The escrow owner cannot always
    // complete a row, because the owner can delete the sub-account that holds it.
    let market_in_settlement = perp_market.status == MarketStatus::Settlement;

    // In Settlement, `settle_expired_position` pays users at `expiry_price`. The reserve must use
    // the same price. A live price below `expiry_price` makes the reserve too small on a net-long
    // market. The sweep then pays out value that the expiry claims need, and those claims later
    // fail with InsufficientPerpPnlPool. `expiry_price` does not change after settlement, so it
    // needs no validity check. The protocol fee sweep uses the same rule.
    let price_to_use = if perp_market.status == MarketStatus::Settlement {
        perp_market.expiry_price
    } else {
        oracle_price
    };
    let reserved_user_claims: u128 = calculate_net_user_pnl(
        &perp_market.amm,
        price_to_use,
        perp_market.quote_asset_amount,
        perp_market.net_unsettled_funding_pnl,
    )?
    .max(0)
    .cast::<u128>()?
    .safe_add(perp_market.get_bankruptcy_if_tranche_reservation(false)?)?;

    let mut swept_total: u64 = 0;
    // The loop below skips a row that the pool cannot pay, and the caller sizes the orders vector
    // up to 128 entries. One log per call keeps the compute cost of those skips constant.
    let mut logged_insufficient_pool = false;

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
        } else if !((is_completed || market_in_settlement)
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
            if !logged_insufficient_pool {
                msg!(
                    "market {} PNL pool has insufficient available balance to sweep some rows. pnl_pool_token_amount: {}, reserved_user_claims: {}, available: {}, first_skipped_fees_accrued: {}",
                    market_index,
                    pnl_pool_token_amount,
                    reserved_user_claims,
                    available_for_sweep,
                    fees_accrued
                );
                logged_insufficient_pool = true;
            }
            // Skip this row only. `reserved_user_claims` is constant for the call, and the loop
            // reads the pool again for each row. A smaller row after this one is still payable.
            // The loop must not stop here. Row order does not change between calls, so one row
            // that the pool cannot pay would stop all later rows forever. The code above this
            // point changes no state, so the skip is safe.
            //
            // A row pays in full or not at all. A part payment of the largest affordable row would
            // give the pool remainder to that beneficiary instead of the revenue pool at the
            // delist. It would also make the payout depend on row order, because the first row
            // would take the whole pool, and the escrow owner controls that order. Full payment
            // keeps the outcome independent of order, so a short pool pays the rows that fit and
            // `forfeit_revenue_share_order` writes off the rest.
            continue;
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
                    swept_total = swept_total.safe_add(fees_accrued)?;
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
                swept_total = swept_total.safe_add(fees_accrued)?;

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
                    swept_total = swept_total.safe_add(fees_accrued)?;
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
                swept_total = swept_total.safe_add(fees_accrued)?;

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

    Ok(swept_total)
}

/// The reason that a row on a closing market cannot be paid.
///
/// A missing account is never a reason on its own. A caller that omits the accounts of a
/// beneficiary proves nothing, and must not destroy a live claim.
///
/// Two of the reasons cannot become false later, so the forfeit needs no waiting period for them.
/// `NoBeneficiaryAccount` is different: the beneficiary can create the account at any time, so the
/// handler permits that reason only after the escrow window of the market ends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RevenueShareForfeitReason {
    /// The beneficiary has no payout account. The handler derives the `User` address for
    /// sub-account 0 of the beneficiary of this row. That account holds no data and the system
    /// program owns it. Sub-account 0 is the only account that can receive the payment, because
    /// `load_revenue_share_map` rejects any other sub-account.
    ///
    /// The beneficiary can create that account at any time, so the handler permits this reason
    /// only after `expiry_ts` plus the escrow window of the state. The beneficiary therefore has
    /// the whole window to act, and the deadline is the first moment that the market may delist.
    NoBeneficiaryAccount,
    /// The market is closed and the pnl pool is smaller than the row. Nothing adds to the pool
    /// again. Fees need fills, and a market that is not Active rejects a fill. Every other
    /// operation removes value. The shortage is therefore permanent.
    PoolExhausted,
    /// The row names no beneficiary that the program can reach. The `builder_idx` is past the end
    /// of `approved_builders`, or the escrow of a referral row has no referrer. The current
    /// accrual paths cannot make such a row. A row like this would block the delist forever.
    UnresolvableBeneficiary,
}

/// Names the reason that the program cannot pay one revenue-share row, or fails.
///
/// `forfeit_revenue_share_order` applies the result. This function changes nothing.
///
/// `beneficiary_user` is the account that the caller passes as the payout account of the row. The
/// function derives the address that the row credits and rejects any other address, so a caller
/// cannot pass an unrelated empty account as proof that a live beneficiary has none.
/// `beneficiary_user_is_empty` states whether that account holds no data and the system program
/// owns it.
///
/// `now` and `escrow_period_before_transfer` set the deadline for `NoBeneficiaryAccount`. That
/// deadline is `PerpMarket.expiry_ts` plus the window, which is the first moment that the market
/// may delist.
pub fn resolve_revenue_share_forfeit_reason(
    perp_market: &PerpMarket,
    quote_spot_market: &SpotMarket,
    revenue_share_escrow: &mut RevenueShareEscrowZeroCopyMut,
    market_index: u16,
    order_index: u32,
    beneficiary_user: &Pubkey,
    beneficiary_user_is_empty: bool,
    now: i64,
    escrow_period_before_transfer: i64,
) -> VelocityResult<RevenueShareForfeitReason> {
    // Only a closing market. On a live market the row blocks nothing, and the beneficiary can
    // still make the account that they need.
    validate!(
        matches!(
            perp_market.status,
            MarketStatus::Settlement | MarketStatus::Delisted
        ),
        ErrorCode::DefaultError,
        "market {} must be in Settlement or Delisted to forfeit revenue share, is {:?}",
        market_index,
        perp_market.status
    )?;

    let (is_referral_order, order_market_type, order_market_index, fees_accrued, builder_idx) = {
        let order = revenue_share_escrow.get_order(order_index)?;
        (
            order.is_referral_order(),
            order.market_type,
            order.market_index,
            order.fees_accrued,
            order.builder_idx,
        )
    };

    validate!(
        order_market_type == MarketType::Perp && order_market_index == market_index,
        ErrorCode::DefaultError,
        "order {} is not for perp market {}",
        order_index,
        market_index
    )?;
    validate!(
        fees_accrued > 0,
        ErrorCode::DefaultError,
        "order {} owes nothing",
        order_index
    )?;

    // The beneficiary of the row. The program reads this from the escrow, not from the caller.
    let beneficiary = if is_referral_order {
        revenue_share_escrow.get_referrer()
    } else {
        revenue_share_escrow
            .get_approved_builder_mut(builder_idx)
            .ok()
            .map(|builder| builder.authority)
    };

    let beneficiary = match beneficiary {
        // No caller can pay a row that names nobody.
        None => return Ok(RevenueShareForfeitReason::UnresolvableBeneficiary),
        Some(beneficiary) => beneficiary,
    };

    // The account must be the one that this row credits. The function derives the address and
    // rejects any other. A caller therefore cannot pass an unrelated empty account as proof that a
    // live beneficiary does not exist.
    let (expected_beneficiary_user, _) = Pubkey::find_program_address(
        &[b"user", beneficiary.as_ref(), 0_u16.to_le_bytes().as_ref()],
        &crate::ID,
    );
    validate!(
        beneficiary_user == &expected_beneficiary_user,
        ErrorCode::DefaultError,
        "beneficiary_user must be {} (sub-account 0 of {}), got {}",
        expected_beneficiary_user,
        beneficiary,
        beneficiary_user
    )?;

    if beneficiary_user_is_empty {
        // The beneficiary can create this account at any time, so the reason is not permanent on
        // its own. Give them the whole escrow window to do it. That window ends at the first
        // moment the market may delist, so this adds no delay to the wind-down. After it, an
        // absent payout account is a missed deadline.
        let forfeit_after = perp_market
            .expiry_ts
            .safe_add(escrow_period_before_transfer)?;
        validate!(
            now > forfeit_after,
            ErrorCode::RevenueShareOrderNotForfeitable,
            "market {} order {}: the beneficiary has until {} to create a payout account",
            market_index,
            order_index,
            forfeit_after
        )?;

        return Ok(RevenueShareForfeitReason::NoBeneficiaryAccount);
    }

    // The program can pay the beneficiary. The only other reason is that the pool holds too
    // little, and that the pool is final. This uses the same closed state that the delist
    // requires. After that state nothing adds to the pool. Fees need fills, and a market that is
    // not Active rejects a fill. Every other operation removes value. The shortage is therefore
    // permanent.
    //
    // The check uses `base_asset_amount_with_amm == 0` and a zero net user cost basis. This equals
    // `net_user_pnl == 0` with the price term removed, and needs no oracle.
    let net_user_cost_basis = calculate_net_user_cost_basis(
        perp_market.quote_asset_amount,
        perp_market.net_unsettled_funding_pnl,
    )?;
    validate!(
        perp_market.amm.base_asset_amount_with_amm == 0 && net_user_cost_basis == 0,
        ErrorCode::RevenueShareOrderNotForfeitable,
        "market {} is not wound down (amm base {}, net user cost basis {}); settle_revenue_share can still pay order {}",
        market_index,
        perp_market.amm.base_asset_amount_with_amm,
        net_user_cost_basis,
        order_index
    )?;

    let pnl_pool_token_amount = get_token_amount(
        perp_market.pnl_pool.scaled_balance,
        quote_spot_market,
        &SpotBalanceType::Deposit,
    )?;
    validate!(
        pnl_pool_token_amount < fees_accrued as u128,
        ErrorCode::RevenueShareOrderNotForfeitable,
        "market {} pnl pool ({}) can still pay order {} ({}); run settle_revenue_share instead",
        market_index,
        pnl_pool_token_amount,
        order_index,
        fees_accrued
    )?;

    Ok(RevenueShareForfeitReason::PoolExhausted)
}

/// Writes off one row that the program cannot pay. The liability counter can then reach zero, and
/// `settle_expired_market_pools_to_revenue_pool` can delist the market.
///
/// The caller permits this only for a market in `Settlement` or `Delisted`. On a live market the
/// row blocks nothing, and the beneficiary can still make the account that they need.
///
/// This moves no tokens. The quote stays in the pnl pool. The vault-owned forfeit above does the
/// same. `sweep_market_fees` reserves `pending_revenue_share`, so the freed value first backs the
/// remaining user claims.
pub fn forfeit_revenue_share_order(
    perp_market: &mut crate::state::perp_market::PerpMarket,
    revenue_share_escrow: &mut RevenueShareEscrowZeroCopyMut,
    order_index: u32,
    reason: RevenueShareForfeitReason,
) -> crate::error::VelocityResult<u64> {
    // Clear the row first. Then subtract the same amount from the counter. A failure then leaves
    // both the row and the counter unchanged. The opposite order can leave a cleared counter and
    // an unpaid row. A later sweep subtracts that amount a second time. The counter becomes too
    // low, and the pool then pays rows that it must keep.
    let fees_accrued = {
        let order = revenue_share_escrow.get_order_mut(order_index)?;
        let fees_accrued = order.fees_accrued;
        if order.is_referral_order() {
            // A referral row keeps its slot. Only the fee goes. The sweep does the same.
            order.fees_accrued = 0;
        } else {
            *order = RevenueShareOrder::default();
        }
        fees_accrued
    };

    perp_market.settle_pending_revenue_share(fees_accrued)?;

    msg!(
        "forfeited {} of revenue share on market {} order {} ({:?})",
        fees_accrued,
        perp_market.market_index,
        order_index,
        reason
    );

    Ok(fees_accrued)
}
