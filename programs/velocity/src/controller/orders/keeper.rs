//! The keeper's work on other users' orders, and what it is paid for it.
//!
//! A keeper cancels the orders of an account that cannot carry them, sweeps
//! expired orders, and is paid a flat reward in quote out of the user it
//! served. Every payment here moves quote from the user to the keeper, so each
//! one claims the keeper's seat before it debits the user.

use super::*;

pub fn credit_filler_perp_pnl(
    filler: &mut User,
    filler_stats: &mut Option<&mut UserStats>,
    market: &mut PerpMarket,
    filler_reward: u64,
    quote_asset_amount: u64,
    now: i64,
    slot: u64,
) -> VelocityResult {
    if filler_reward > 0 {
        let position_index = get_position_index(&filler.perp_positions, market.market_index)
            .or_else(|_| add_new_position(&mut filler.perp_positions, market.market_index))?;

        controller::position::update_quote_asset_amount(
            &mut filler.perp_positions[position_index],
            market,
            filler_reward.cast()?,
        )?;

        filler_stats
            .as_mut()
            .safe_unwrap()?
            .update_filler_volume(quote_asset_amount, now)?;
    }

    filler.update_last_active_slot(slot);

    Ok(())
}

pub fn force_cancel_orders(
    state: &State,
    user_account_loader: &AccountLoader<User>,
    maps: &mut AccountMaps,
    filler: &AccountLoader<User>,
    clock: &Clock,
) -> VelocityResult {
    let now = clock.unix_timestamp;
    let slot = clock.slot;

    let filler_key = filler.key();
    let user_key = user_account_loader.key();
    let user = &mut load_mut!(user_account_loader)?;
    let filler = &mut load_mut!(filler)?;

    let scope = ForceCancelScope::authorize(user, maps)?;

    let mut total_fee = 0_u64;
    for order_index in 0..user.orders.len() {
        let Some(fee) = scope.cancel_fee(user, order_index, state, maps)? else {
            continue;
        };

        total_fee = total_fee.safe_add(fee)?;

        cancel_order(
            order_index,
            user,
            &user_key,
            maps,
            now,
            slot,
            OrderActionExplanation::InsufficientFreeCollateral,
            Some(&filler_key),
            fee,
            false,
        )?;
    }

    pay_keeper_flat_reward_for_spot(
        user,
        Some(filler),
        maps.spot_market_map
            .get_quote_spot_market_mut()?
            .deref_mut(),
        total_fee,
        slot,
    )?;

    user.update_last_active_slot(slot);

    Ok(())
}

/// What authorizes a keeper to force-cancel an account's orders, and which of
/// them it may cancel.
struct ForceCancelScope {
    margin_calc: MarginCalculation,
    /// Whether the cross-margin scope still meets initial margin. Its orders
    /// are then out of the keeper's reach, because the scope answers for them.
    cross_margin_meets_initial_margin_requirement: bool,
}

impl ForceCancelScope {
    /// Hold the keeper to the grounds that let it act against this account.
    ///
    /// A below-floor account is grounds for a keeper to act, so the floor test
    /// fails closed. The floor counts only when every oracle is valid and the
    /// trusted value sits below it. A bad price then cannot manufacture
    /// authorization. Under oracle degradation the keeper falls back to the
    /// margin arm, which keeps force-cancel available on a margin-breached
    /// account.
    fn authorize(user: &User, maps: &mut AccountMaps) -> VelocityResult<Self> {
        validate!(
            !user.is_being_liquidated(),
            ErrorCode::UserIsBeingLiquidated
        )?;

        validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

        let margin_calc = calculate_margin_requirement_and_total_collateral_and_liability_info(
            user,
            maps,
            MarginContext::standard(MarginRequirementType::Initial),
        )?;

        let below_equity_floor = calculate_net_equity_for_floor(user, maps)?
            .is_some_and(|net_equity| net_equity.proves_below_floor(user));

        validate!(
            !margin_calc.meets_margin_requirement() || below_equity_floor,
            ErrorCode::SufficientCollateral
        )?;

        let cross_margin_meets_initial_margin_requirement =
            margin_calc.meets_cross_margin_requirement() && !below_equity_floor;

        Ok(Self {
            margin_calc,
            cross_margin_meets_initial_margin_requirement,
        })
    }

    /// The flat reward this order earns the keeper, or `None` when the order
    /// is not the keeper's to cancel.
    fn cancel_fee(
        &self,
        user: &User,
        order_index: usize,
        state: &State,
        maps: &mut AccountMaps,
    ) -> VelocityResult<Option<u64>> {
        let order = &user.orders[order_index];
        if order.status != OrderStatus::Open {
            return Ok(None);
        }

        // A placed trigger rests on the CLOB. A keeper force-cancels it
        // through the CLOB with an `OrderRef`, so the shadow slot is left
        // alone.
        if order.is_placed_on_clob() {
            return Ok(None);
        }

        match order.market_type {
            MarketType::Spot => self.spot_cancel_fee(user, order, state, maps),
            MarketType::Perp => self.perp_cancel_fee(user, order, state),
        }
    }

    /// The reward a spot order earns. A reducing order, and any order of an
    /// account whose cross-margin scope still meets initial margin, is left
    /// alone.
    fn spot_cancel_fee(
        &self,
        user: &User,
        order: &Order,
        state: &State,
        maps: &mut AccountMaps,
    ) -> VelocityResult<Option<u64>> {
        let market_index = order.market_index;
        let spot_market = maps.spot_market_map.get_ref(&market_index)?;
        let token_amount = user
            .get_spot_position(market_index)?
            .get_signed_token_amount(&spot_market)?
            .cast::<i64>()?;
        let is_position_reducing = is_order_position_reducing(
            &order.direction,
            order.get_base_asset_amount_unfilled(Some(token_amount))?,
            token_amount,
        )?;

        if is_position_reducing || self.cross_margin_meets_initial_margin_requirement {
            return Ok(None);
        }

        Ok(Some(state.spot_fee_structure.flat_filler_fee))
    }

    /// The reward a perp order earns. An isolated position answers for its own
    /// orders, so it is measured against its own margin requirement rather
    /// than the account's.
    fn perp_cancel_fee(
        &self,
        user: &User,
        order: &Order,
        state: &State,
    ) -> VelocityResult<Option<u64>> {
        let market_index = order.market_index;
        let position = user.get_perp_position(market_index)?;
        let base_asset_amount = position.base_asset_amount;
        let is_position_reducing = is_order_position_reducing(
            &order.direction,
            order.get_base_asset_amount_unfilled(Some(base_asset_amount))?,
            base_asset_amount,
        )?;
        if is_position_reducing {
            return Ok(None);
        }

        let meets_margin_requirement = if position.is_isolated() {
            self.margin_calc
                .meets_isolated_margin_requirement(market_index)?
        } else {
            self.cross_margin_meets_initial_margin_requirement
        };
        if meets_margin_requirement {
            return Ok(None);
        }

        Ok(Some(state.perp_fee_structure.flat_filler_fee))
    }
}

pub fn can_reward_user_with_perp_pnl(user: &mut Option<&mut User>, market_index: u16) -> bool {
    match user.as_mut() {
        Some(user) => user.force_get_perp_position_mut(market_index).is_ok(),
        None => false,
    }
}

pub fn can_reward_user_with_referral_reward(
    market_index: u16,
    rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
) -> bool {
    if let Some(escrow) = rev_share_escrow {
        // returns None for an escrow without a referrer, so a never-referred
        // escrow holder gets no referee discount and claims no referral slot
        escrow.find_or_create_referral_index(market_index).is_some()
    } else {
        false
    }
}

pub fn pay_keeper_flat_reward_for_perps(
    user: &mut User,
    filler: Option<&mut User>,
    market: &mut PerpMarket,
    filler_reward: u64,
    slot: u64,
) -> VelocityResult<u64> {
    let filler_reward = if let Some(filler) = filler {
        filler.update_last_active_slot(slot);
        // The filler's position slot is the half that can fail, so it is
        // claimed before the user is debited. A filler that holds a position
        // in every slot, none of them this market's, gets no slot. A debit
        // first would take the user's quote and pay nobody. The reward would
        // then accrue to the pool instead of to the keeper that earned it.
        // `force_get_perp_position_mut` creates the slot, so the credit below
        // finds it.
        if filler
            .force_get_perp_position_mut(market.market_index)
            .is_err()
        {
            return Ok(0);
        }

        let user_position = user.get_perp_position_mut(market.market_index)?;
        controller::position::update_quote_asset_and_break_even_amount(
            user_position,
            market,
            -filler_reward.cast()?,
        )?;

        let filler_position = filler.force_get_perp_position_mut(market.market_index)?;
        controller::position::update_quote_asset_amount(
            filler_position,
            market,
            filler_reward.cast()?,
        )?;

        filler_reward
    } else {
        0
    };

    Ok(filler_reward)
}

pub fn pay_keeper_flat_reward_for_spot(
    user: &mut User,
    filler: Option<&mut User>,
    quote_market: &mut SpotMarket,
    filler_reward: u64,
    slot: u64,
) -> VelocityResult<u64> {
    let filler_reward = if let Some(filler) = filler {
        update_spot_balances(
            filler_reward as u128,
            &SpotBalanceType::Deposit,
            quote_market,
            filler.get_quote_spot_position_mut(),
            false,
        )?;

        filler.update_last_active_slot(slot);

        filler.update_cumulative_spot_fees(filler_reward.cast()?)?;

        update_spot_balances(
            filler_reward as u128,
            &SpotBalanceType::Borrow,
            quote_market,
            user.get_quote_spot_position_mut(),
            false,
        )?;

        user.update_cumulative_spot_fees(-filler_reward.cast()?)?;

        filler_reward
    } else {
        0
    };

    Ok(filler_reward)
}

pub fn expire_orders(
    user: &mut User,
    user_key: &Pubkey,
    maps: &mut AccountMaps,
    now: i64,
    slot: u64,
) -> VelocityResult {
    for order_index in 0..user.orders.len() {
        if !should_expire_order(&user.orders[order_index], now)? {
            continue;
        }

        cancel_order(
            order_index,
            user,
            user_key,
            maps,
            now,
            slot,
            OrderActionExplanation::OrderExpired,
            None,
            0,
            false,
        )?;
    }

    Ok(())
}

/// Pay the cranker its cut of a routed remainder's improvement. The quote
/// moves from the taker to the filler.
///
/// This is the quote-for-work transfer the flat keeper rewards make. The
/// improvement sizes it instead of a flat fee. The reward is paid in full or
/// not at all, and
/// [`crate::math::fees::calculate_taker_origin_cross_fee`] decides which.
///
/// The fill this follows already ran its own margin checks, so this debit
/// lands after them. The reward is bounded by the improvement the fill
/// delivered, so a taker that could afford to rest can afford the reward. The
/// maintenance-margin check below proves that rather than assuming it.
#[allow(clippy::too_many_arguments)]
pub fn pay_taker_origin_crank_reward(
    market_index: u16,
    fee: &fees::TakerOriginCrossFee,
    quote_filled: u64,
    taker_loader: &AccountLoader<User>,
    filler_loader: &AccountLoader<User>,
    filler_stats_loader: &AccountLoader<UserStats>,
    maps: &mut AccountMaps,
    clock: &Clock,
) -> VelocityResult<u64> {
    if fee.crank_reward == 0 {
        return Ok(0);
    }
    let paid = {
        let mut taker = load_mut!(taker_loader)?;
        let mut filler = load_mut!(filler_loader)?;
        let mut market = maps.perp_market_map.get_ref_mut(&market_index)?;
        let paid = pay_keeper_flat_reward_for_perps(
            &mut taker,
            Some(&mut filler),
            market.deref_mut(),
            fee.crank_reward,
            clock.slot,
        )?;
        // `pay_keeper_flat_reward_for_perps` pays nothing when the filler has
        // no room for a position in this market. Refuse the crank instead of
        // completing it unpaid. The cranker can pass a `User` that can hold
        // the position.
        validate!(
            paid == fee.crank_reward,
            ErrorCode::DefaultError,
            "cranker's User cannot hold a position in market {} to be paid in",
            market_index
        )?;
        taker.update_last_active_slot(clock.slot);
        paid
    };
    load_mut!(filler_stats_loader)?.update_filler_volume(quote_filled, clock.unix_timestamp)?;

    // The debit lands after the fill's own checks, so this is where the taker
    // is held to maintenance margin for it.
    let taker = load!(taker_loader)?;
    crate::math::margin::meets_maintenance_margin_requirement(&taker, maps)?
        .then_some(paid)
        .ok_or_else(|| {
            msg!("crank reward would leave the taker below maintenance margin");
            ErrorCode::InsufficientCollateral
        })
}
