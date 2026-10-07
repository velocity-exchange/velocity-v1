//! The keeper's work on other users' orders, and what it is paid for it.
//!
//! A keeper cancels the orders of an account that cannot carry them, sweeps
//! expired orders, and is paid a flat reward in quote out of the user it
//! served. Every payment here moves quote from the user to the keeper, so each
//! one claims the keeper's seat before it debits the user.

use super::*;

/// Pay the keeper its reward on its own perp seat.
///
/// A keeper with no reward still has its last-active slot stamped, so its
/// transaction does not revert for idleness.
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

/// Why a keeper may act against an account.
///
/// Two grounds, either of which authorizes: the account fails initial margin,
/// or it is proven below its equity floor. The floor arm fails closed in the
/// direction opposite the gates that restrict the user, so a bad price cannot
/// manufacture authorization.
///
/// The authority-wide equity breaker is deliberately not a ground. It bars the
/// authority from risk-increasing activity, which is a different question from
/// whether one subaccount can carry the orders it already rests. Every
/// force-cancel surface answers the same two grounds.
pub(crate) struct ForceCancelGrounds {
    pub(crate) margin_calc: MarginCalculation,
    below_equity_floor: bool,
}

impl ForceCancelGrounds {
    pub(crate) fn measure(user: &User, maps: &mut AccountMaps) -> VelocityResult<Self> {
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

        Ok(Self {
            margin_calc,
            below_equity_floor,
        })
    }

    /// True when either ground stands.
    pub(crate) fn any(&self) -> bool {
        !self.margin_calc.meets_margin_requirement() || self.below_equity_floor
    }

    /// True when this market answers for its own orders, so they are out of
    /// the keeper's reach.
    pub(crate) fn market_recoverable(
        &self,
        user: &User,
        market_index: u16,
    ) -> VelocityResult<bool> {
        if user
            .get_perp_position(market_index)
            .map(|position| position.is_isolated())
            .unwrap_or(false)
        {
            return self
                .margin_calc
                .meets_isolated_margin_requirement(market_index);
        }

        Ok(self.cross_margin_recoverable())
    }

    /// True when the cross-margin scope still answers for its own orders.
    pub(crate) fn cross_margin_recoverable(&self) -> bool {
        self.margin_calc.meets_cross_margin_requirement() && !self.below_equity_floor
    }
}

/// What authorizes a keeper to force-cancel an account's orders, and which of
/// them it may cancel.
struct ForceCancelScope(ForceCancelGrounds);

impl ForceCancelScope {
    /// Hold the keeper to the grounds that let it act against this account.
    ///
    /// Under oracle degradation the floor arm goes quiet and the keeper falls
    /// back to the margin arm, which keeps force-cancel available on a
    /// margin-breached account.
    fn authorize(user: &User, maps: &mut AccountMaps) -> VelocityResult<Self> {
        let grounds = ForceCancelGrounds::measure(user, maps)?;
        validate!(grounds.any(), ErrorCode::SufficientCollateral)?;

        Ok(Self(grounds))
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

        if is_position_reducing || self.0.cross_margin_recoverable() {
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

        if self.0.market_recoverable(user, market_index)? {
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
        // The filler's position slot is claimed before the user is debited,
        // since it is the half that can fail. A filler with a position in
        // every slot but this market's gets none. Debiting first would take
        // the user's quote and pay nobody, so the reward would accrue to the pool instead of the keeper.
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
            ErrorCode::MaxNumberOfPositions,
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
