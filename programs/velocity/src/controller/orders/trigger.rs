//! Firing a trigger order.
//!
//! A trigger order rests dormant until the market reaches its trigger price.
//! Firing it turns it into a live market order and pays the keeper the flat
//! reward. [`trigger_order`] leaves the fired order resting in `user.orders`;
//! [`trigger_and_route_order`] hands it back detached for the caller to fill
//! straight against the book.

use super::*;

/// The accounts one trigger crank runs against.
///
/// The keeper may be the order's own owner, in which case `filler` and `user`
/// name the same account and no reward is paid.
pub struct TriggerAccounts<'a, 'info> {
    pub user: &'a AccountLoader<'info, User>,
    pub user_stats: &'a AccountLoader<'info, UserStats>,
    pub filler: &'a AccountLoader<'info, User>,
}

/// Fire a DLOB trigger order and leave it resting live in `User.orders`.
///
/// Returns whether the trigger did payable work: `true` when it triggered the
/// order and paid the keeper, `false` when it cancelled, found the order
/// already triggered, or did nothing. The crank handler skips the reservoir
/// payout on `false`, so a failing account's cancel branch cannot drain the
/// market's reservoir.
pub fn trigger_order(
    order_id: u32,
    state: &State,
    accounts: &TriggerAccounts,
    maps: &mut AccountMaps,
    clock: &Clock,
) -> VelocityResult<bool> {
    let now = clock.unix_timestamp;
    let slot = clock.slot;
    let user = &mut load_mut!(accounts.user)?;

    let Some(firing) = open_trigger(user, order_id, state, maps, now)? else {
        return Ok(false);
    };
    let FiringOrder {
        order_index,
        market_index,
        oracle_price_data,
        trigger_price,
    } = firing;
    let oracle_price = oracle_price_data.price;

    let (_, worst_case_liability_value_before) = user
        .get_perp_position(market_index)?
        .worst_case_liability_value(oracle_price)?;

    {
        let perp_market = maps.perp_market_map.get_ref(&market_index)?;
        arm_trigger_order(
            &mut user.orders[order_index],
            &oracle_price_data,
            &perp_market,
            state,
            slot,
        )?;
    }
    if user.orders[order_index].has_auction() {
        user.increment_open_auctions();
    }
    let bit_flags = reserve_fired_order(user, order_index, market_index)?;

    let (_, worst_case_liability_value_after) = user
        .get_perp_position(market_index)?
        .worst_case_liability_value(oracle_price)?;
    let is_risk_increasing = worst_case_liability_value_after > worst_case_liability_value_before;

    if is_risk_increasing
        && !user.orders[order_index].reduce_only
        && trigger_must_cancel(user, accounts.user_stats, maps)?
    {
        cancel_trigger_order(user, order_index, accounts, maps, clock)?;
        // The cancel did no payable trigger work — the user paid no flat
        // reward here, so the crank must not draw the reservoir either.
        return Ok(false);
    }

    let fired = user.orders[order_index];
    pay_and_record_trigger(
        user,
        &fired,
        accounts,
        &TriggerRecord {
            oracle_price,
            trigger_price,
            bit_flags,
            flat_filler_fee: state.perp_fee_structure.flat_filler_fee,
        },
        maps,
        clock,
    )?;

    user.update_last_active_slot(slot);

    Ok(true)
}

/// Fire a DLOB trigger order and hand back the now-live order for the caller to
/// route straight to the book.
///
/// This is the v1 trigger path. Unlike [`trigger_order`], it does not leave the
/// fired order resting live in `User.orders`. It validates the trigger,
/// transforms a copy of the slot's order into a live market order, frees the
/// slot, pays the keeper, and returns the order as a detached value. The caller
/// fills it against the book and rests only the remainder, the same
/// straight-to-book shape a v1 place takes. The armed slot reserved no
/// exposure, so freeing it releases only the order count; the remainder the
/// caller rests re-adds one for its CLOB order.
///
/// Returns `None` when there is no payable work: the order is already
/// triggered, or a risk-increasing trigger on a failing account is cancelled
/// instead of fired. The caller skips the fill and the reservoir payout on
/// `None`, so a failing account's cancel cannot drain the market reservoir.
pub fn trigger_and_route_order(
    order_id: u32,
    state: &State,
    accounts: &TriggerAccounts,
    maps: &mut AccountMaps,
    clock: &Clock,
) -> VelocityResult<Option<Order>> {
    let now = clock.unix_timestamp;
    let slot = clock.slot;
    let user = &mut load_mut!(accounts.user)?;

    let Some(firing) = open_trigger(user, order_id, state, maps, now)? else {
        return Ok(None);
    };
    let FiringOrder {
        order_index,
        market_index,
        oracle_price_data,
        trigger_price,
    } = firing;
    let oracle_price = oracle_price_data.price;

    // Transform a copy of the slot's order into the live market order. The copy,
    // not the slot, so freeing the slot never reserves exposure the ephemeral
    // fill does not rest.
    let mut fired = user.orders[order_index];
    {
        let perp_market = maps.perp_market_map.get_ref(&market_index)?;
        arm_trigger_order(&mut fired, &oracle_price_data, &perp_market, state, slot)?;
    }

    let is_risk_increasing = fired_order_increases_risk(user, &fired, oracle_price)?;

    // A risk-increasing trigger on a failing account cancels instead of firing.
    // The same gate `trigger_order` runs: initial margin, buffered equity
    // floor, and the authority-wide equity breaker, before any reward. An
    // unverifiable floor rejects rather than cancels, since a cancel is
    // irreversible.
    if is_risk_increasing
        && !fired.reduce_only
        && trigger_must_cancel(user, accounts.user_stats, maps)?
    {
        cancel_trigger_order(user, order_index, accounts, maps, clock)?;
        return Ok(None);
    }

    let is_isolated = user
        .get_perp_position(market_index)
        .is_ok_and(|position| position.is_isolated());
    let bit_flags = set_order_bit_flag(0, is_isolated, OrderBitFlag::IsIsolatedPosition);

    // Pay the keeper the flat trigger reward and record the trigger. The fill
    // the caller runs settles its own fees; this is the trigger's own reward,
    // paid once for the crank that fired it.
    pay_and_record_trigger(
        user,
        &fired,
        accounts,
        &TriggerRecord {
            oracle_price,
            trigger_price,
            bit_flags,
            flat_filler_fee: state.perp_fee_structure.flat_filler_fee,
        },
        maps,
        clock,
    )?;

    free_fired_order_slot(user, order_index, market_index)?;

    user.update_last_active_slot(slot);

    Ok(Some(fired))
}

/// The order a trigger crank fires, and the prices it fires at.
struct FiringOrder {
    order_index: usize,
    market_index: u16,
    oracle_price_data: OraclePriceData,
    trigger_price: u64,
}

/// Find the order a trigger names, and hold it, its account and its market to
/// every gate before anything moves.
///
/// `None` means the order is already triggered, which is not payable work.
fn open_trigger(
    user: &mut User,
    order_id: u32,
    state: &State,
    maps: &mut AccountMaps,
    now: i64,
) -> VelocityResult<Option<FiringOrder>> {
    let Some(order_index) = find_triggerable_order(user, order_id)? else {
        return Ok(None);
    };
    let market_index = user.orders[order_index].market_index;

    validate_user_not_being_liquidated(user, maps, state.liquidation_margin_buffer_ratio)?;
    validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

    let (oracle_price_data, trigger_price) = {
        let perp_market = maps.perp_market_map.get_ref(&market_index)?;
        trigger_market_preflight(state, &perp_market, &mut maps.oracle_map, now)?
    };
    validate_trigger_condition(&user.orders[order_index], trigger_price)?;

    Ok(Some(FiringOrder {
        order_index,
        market_index,
        oracle_price_data,
        trigger_price,
    }))
}

/// The slot of the order a trigger crank names, once it is known triggerable.
///
/// `None` means the order is already triggered, which is not an error: the
/// caller reports no payable work and leaves the order alone.
fn find_triggerable_order(user: &User, order_id: u32) -> VelocityResult<Option<usize>> {
    let order_index = user
        .orders
        .iter()
        .position(|order| order.order_id == order_id && order.status == OrderStatus::Open)
        .ok_or_else(print_error!(ErrorCode::OrderDoesNotExist))?;
    let order = &user.orders[order_index];

    validate!(
        order.status == OrderStatus::Open,
        ErrorCode::OrderNotOpen,
        "Order not open"
    )?;
    validate!(
        order.must_be_triggered(),
        ErrorCode::OrderNotTriggerable,
        "Order is not triggerable"
    )?;
    // A placed trigger's slot deliberately reads as untriggered (that keeps
    // it out of every DLOB matching path), so guard explicitly: its live
    // order already rests on the CLOB.
    validate!(
        !order.is_placed_on_clob(),
        ErrorCode::OrderPlacedOnClob,
        "Order is placed on the CLOB"
    )?;
    // An evicted trigger comes back armed behind an edge gate: it fires again
    // only after a crank observes the price back off the trigger side. This
    // path has no such observation, so it would flip a re-armed order live at
    // the price that already evicted it. The gate belongs to the book crank
    // that set it, and only that crank clears it.
    validate!(
        !order.is_bit_flag_set(OrderBitFlag::AwaitingTriggerRecross),
        ErrorCode::OrderAwaitingTriggerRecross,
        "Order waits for the trigger price to cross back after an eviction"
    )?;

    if order.triggered() {
        msg!("Order is already triggered");
        return Ok(None);
    }

    validate!(
        order.market_type == MarketType::Perp,
        ErrorCode::InvalidOrderMarketType,
        "Order must be a perp order"
    )?;

    Ok(Some(order_index))
}

/// The market and oracle gates a trigger passes, and the price its condition
/// is judged at.
///
/// Triggering starts the order's auction and pays the keeper reward, so it is
/// part of the fill lifecycle: it respects the market-scoped fill pause the
/// same way `fill_perp_order` does. The exchange-wide `FillPaused` breaker is
/// enforced by the handler's `fill_not_paused` access control. A trigger is
/// also forbidden once a market is in settlement, or a keeper could trigger a
/// dormant order on an expired market, minting a settleable positive
/// zero-base quote claim out of the flat reward and consuming PnL-pool
/// headroom that backs legitimate expiry claimants (OtterSec #86).
fn trigger_market_preflight(
    state: &State,
    perp_market: &PerpMarket,
    oracle_map: &mut OracleMap,
    now: i64,
) -> VelocityResult<(OraclePriceData, u64)> {
    validate!(
        !perp_market.is_operation_paused(PerpOperation::Fill),
        ErrorCode::MarketFillOrderPaused,
        "Market fills paused",
    )?;
    validate!(
        !perp_market.is_in_settlement(now),
        ErrorCode::MarketPlaceOrderPaused,
        "Market is in settlement mode",
    )?;

    let (oracle_price_data, oracle_validity) = oracle_map.get_price_data_and_validity(
        MarketType::Perp,
        perp_market.market_index,
        &perp_market.oracle_id(),
        perp_market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap,
        perp_market.get_max_confidence_interval_multiplier()?,
        perp_market.oracle_slot_delay_override,
        perp_market.oracle_low_risk_slot_delay_override,
        None,
    )?;
    validate!(
        is_oracle_valid_for_action(oracle_validity, Some(VelocityAction::TriggerOrder))?,
        ErrorCode::InvalidOracle
    )?;

    let oracle_too_divergent_with_twap_5min = is_oracle_too_divergent_with_twap_5min(
        oracle_price_data.price,
        perp_market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min,
        state
            .oracle_guard_rails
            .max_oracle_twap_5min_percent_divergence()
            .cast()?,
    )?;
    validate!(
        !oracle_too_divergent_with_twap_5min,
        ErrorCode::OrderBreachesOraclePriceLimits,
        "oracle price vs twap too divergent"
    )?;

    let oracle_price = oracle_price_data.price;
    let trigger_price =
        perp_market.get_trigger_price(oracle_price, now, state.use_median_trigger_price())?;
    Ok((*oracle_price_data, trigger_price))
}

/// Hold the order to its own trigger condition.
fn validate_trigger_condition(order: &Order, trigger_price: u64) -> VelocityResult {
    validate!(
        order_satisfies_trigger_condition(order, trigger_price)?,
        ErrorCode::OrderDidNotSatisfyTriggerCondition,
        "Order did not satisfy trigger condition. trigger_price: {} oracle_price: {} trigger_condition: {:?}",
        trigger_price,
        &order.trigger_price,
        &order.trigger_condition
    )
}

/// Turn a dormant trigger order into the live market order it fires as.
///
/// Trigger-order auction params quote off the AMM's cached spread state,
/// which the keeper crank and the fill setup refresh for this slot.
fn arm_trigger_order(
    order: &mut Order,
    oracle_price_data: &OraclePriceData,
    perp_market: &PerpMarket,
    state: &State,
    slot: u64,
) -> VelocityResult {
    update_trigger_order_params(
        order,
        oracle_price_data,
        slot,
        // ~8s minimum, in wall clock 400ms units
        Millis::from_secs(8)
            .div_periods(Millis::UNIT)
            .min(u8::MAX as u64) as u8,
        Some(perp_market),
        state.slot_clock(),
    )
}

/// Reserve the exposure the fired order holds open, and report whether it
/// landed in an isolated position.
fn reserve_fired_order(
    user: &mut User,
    order_index: usize,
    market_index: u16,
) -> VelocityResult<u8> {
    let direction = user.orders[order_index].direction;
    let base_asset_amount = user.orders[order_index].base_asset_amount;
    let update_open_bids_and_asks = user.orders[order_index].update_open_bids_and_asks();

    let user_position = user.get_perp_position_mut(market_index)?;
    increase_open_bids_and_asks(
        user_position,
        &direction,
        base_asset_amount,
        update_open_bids_and_asks,
    )?;
    Ok(set_order_bit_flag(
        0,
        user_position.is_isolated(),
        OrderBitFlag::IsIsolatedPosition,
    ))
}

/// Whether the fired order increases the account's risk.
///
/// Apply its worst-case exposure to the position, measure, and take it
/// straight back. The ephemeral fill does not rest the order, so the
/// reservation must not linger.
fn fired_order_increases_risk(
    user: &mut User,
    fired: &Order,
    oracle_price: i64,
) -> VelocityResult<bool> {
    let market_index = fired.market_index;
    let update_open_bids_and_asks = fired.update_open_bids_and_asks();

    let (_, worst_case_before) = user
        .get_perp_position(market_index)?
        .worst_case_liability_value(oracle_price)?;
    increase_open_bids_and_asks(
        user.get_perp_position_mut(market_index)?,
        &fired.direction,
        fired.base_asset_amount,
        update_open_bids_and_asks,
    )?;
    let (_, worst_case_after) = user
        .get_perp_position(market_index)?
        .worst_case_liability_value(oracle_price)?;
    decrease_open_bids_and_asks(
        user.get_perp_position_mut(market_index)?,
        &fired.direction,
        fired.base_asset_amount,
        update_open_bids_and_asks,
    )?;

    Ok(worst_case_after > worst_case_before)
}

/// Whether a risk-increasing trigger must be cancelled instead of fired.
///
/// The account is held to initial margin, to its own buffered equity floor,
/// and to the authority-wide equity breaker. The breaker check mirrors the
/// fill, withdraw and transfer paths: while it is set, no risk-increasing
/// action is allowed on any of the authority's subaccounts. It is evaluated
/// before the keeper reward is paid, so a keeper cannot farm the trigger
/// reward out of a frozen or below-floor account by flipping its resting
/// risk-increasing orders into immediate cancels.
///
/// An unverifiable floor rejects the trigger instead of cancelling. A cancel
/// is irreversible, so an oracle blip must not destroy a resting order the
/// account may legitimately carry. The keeper retries once the feed recovers
/// and the gate resolves either way.
fn trigger_must_cancel(
    user: &User,
    user_stats_loader: &AccountLoader<UserStats>,
    maps: &mut AccountMaps,
) -> VelocityResult<bool> {
    let margin_calc = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        maps,
        MarginContext::standard(MarginRequirementType::Initial),
    )?;

    let net_equity = calculate_net_equity_for_floor(user, maps)?;
    if let Some(net_equity) = net_equity {
        validate!(
            net_equity.all_oracles_valid,
            ErrorCode::InvalidOracle,
            "cannot verify equity floor {} + buffer {} with an invalid oracle (authority {} subaccount {})",
            user.equity_floor,
            user.equity_floor_buffer,
            user.authority,
            user.sub_account_id
        )?;
    }

    // The floor restricts the user here: it cancels a risk-increasing
    // order that the subaccount may not carry. Every oracle is valid past
    // the check above, so a trusted value below the buffered floor is
    // grounds to cancel.
    Ok(!margin_calc.meets_margin_requirement()
        || net_equity.is_some_and(|net_equity| !net_equity.clears_buffered_floor(user))
        || load!(user_stats_loader)?.is_equity_breaker_tripped())
}

/// Cancel a risk-increasing trigger the account may not carry.
///
/// The cancel succeeds while the subaccount may already sit below its raw
/// floor, so the breaker is armed inline and the keeper's trigger doubles as
/// the trip.
fn cancel_trigger_order(
    user: &mut User,
    order_index: usize,
    accounts: &TriggerAccounts,
    maps: &mut AccountMaps,
    clock: &Clock,
) -> VelocityResult {
    let filler_key = accounts.filler.key();
    cancel_order(
        order_index,
        user,
        &accounts.user.key(),
        maps,
        clock.unix_timestamp,
        clock.slot,
        OrderActionExplanation::InsufficientFreeCollateral,
        Some(&filler_key),
        0,
        false,
    )?;

    user.update_last_active_slot(clock.slot);

    let mut user_stats = load_mut!(accounts.user_stats)?;
    controller::equity_floor::try_lazy_equity_breaker_trip(user, &mut user_stats, maps)
}

/// What the trigger record says beyond the order itself.
struct TriggerRecord {
    oracle_price: i64,
    trigger_price: u64,
    bit_flags: u8,
    flat_filler_fee: u64,
}

/// Pay the keeper the flat trigger reward, and record the trigger.
///
/// A keeper that owns the order is paid nothing: it is already loaded as the
/// user, and cannot be loaded a second time as the filler.
fn pay_and_record_trigger(
    user: &mut User,
    fired: &Order,
    accounts: &TriggerAccounts,
    record: &TriggerRecord,
    maps: &mut AccountMaps,
    clock: &Clock,
) -> VelocityResult {
    let filler_key = accounts.filler.key();
    let user_key = accounts.user.key();
    let mut filler = if user_key != filler_key {
        Some(load_mut!(accounts.filler)?)
    } else {
        None
    };

    let filler_reward = {
        let mut perp_market = maps.perp_market_map.get_ref_mut(&fired.market_index)?;
        pay_keeper_flat_reward_for_perps(
            user,
            filler.as_deref_mut(),
            &mut perp_market,
            record.flat_filler_fee,
            clock.slot,
        )?
    };

    let order_action_record = get_order_action_record(
        clock.unix_timestamp,
        OrderAction::Trigger,
        OrderActionExplanation::None,
        fired.market_index,
        Some(filler_key),
        None,
        Some(filler_reward),
        None,
        None,
        Some(filler_reward),
        None,
        None,
        None,
        None,
        Some(user_key),
        Some(*fired),
        None,
        None,
        record.oracle_price,
        record.bit_flags,
        None,
        None,
        None,
        None,
        Some(record.trigger_price),
        None,
        None,
    )?;
    emit!(order_action_record);

    Ok(())
}

/// Free the armed slot the fired order left behind.
///
/// The order is now a detached value. An untriggered trigger reserved no
/// exposure, so only the order count comes off. The caller's fill tolerates
/// the now-empty position and rebuilds it.
fn free_fired_order_slot(user: &mut User, order_index: usize, market_index: u16) -> VelocityResult {
    let position_index = get_position_index(&user.perp_positions, market_index)?;
    let slot_had_auction = user.orders[order_index].has_auction();
    user.decrement_open_orders(slot_had_auction);
    user.perp_positions[position_index].open_orders = user.perp_positions[position_index]
        .open_orders
        .saturating_sub(1);
    user.orders[order_index] = Order::default();
    Ok(())
}

pub(super) fn update_trigger_order_params(
    order: &mut Order,
    oracle_price_data: &OraclePriceData,
    slot: u64,
    min_auction_duration: u8,
    perp_market: Option<&PerpMarket>,
    slot_clock: SlotClock,
) -> VelocityResult {
    order.trigger_condition = match order.trigger_condition {
        OrderTriggerCondition::Above => OrderTriggerCondition::TriggeredAbove,
        OrderTriggerCondition::Below => OrderTriggerCondition::TriggeredBelow,
        _ => {
            return Err(print_error!(ErrorCode::InvalidTriggerOrderCondition)());
        }
    };

    // ~60s: a reduce-only trigger left resting this long is flagged safe for
    // the relaxed oracle delay gate. Rest time is integrated per
    // slot duration regime.
    if slot_clock.elapsed(order.slot, slot) > Millis::from_secs(60) && order.reduce_only {
        order.add_bit_flag(OrderBitFlag::SafeTriggerOrder);
    }

    order.slot = slot;

    let (auction_duration, auction_start_price, auction_end_price) =
        calculate_auction_params_for_trigger_order(
            order,
            oracle_price_data,
            min_auction_duration,
            perp_market,
        )?;

    msg!(
        "new auction duration {} start price {} end price {}",
        auction_duration,
        auction_start_price,
        auction_end_price
    );

    order.auction_duration = auction_duration;
    order.auction_start_price = auction_start_price;
    order.auction_end_price = auction_end_price;

    if matches!(order.order_type, OrderType::TriggerMarket) {
        order.add_bit_flag(OrderBitFlag::OracleTriggerMarket);
    }

    Ok(())
}
