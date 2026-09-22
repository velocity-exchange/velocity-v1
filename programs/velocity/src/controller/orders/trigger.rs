//! Firing a trigger order.
//!
//! A trigger order rests dormant until the market reaches its trigger price.
//! Firing it turns it into a live market order and pays the keeper the flat
//! reward. [`trigger_order`] leaves the fired order resting in `user.orders`.
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

/// Fire an armed trigger order and hand back the now-live order for the caller to
/// route straight to the book.
///
/// This is the v1 trigger path. [`trigger_order`] leaves the fired order
/// resting live in `User.orders`, and this one does not. It validates the
/// trigger, transforms a copy of the slot's order into a live market order,
/// frees the slot, pays the keeper, and returns the order as a detached value.
/// The caller fills it against the book and rests only the remainder, which is
/// the straight-to-book shape a v1 place takes. The armed slot reserved no
/// exposure, so freeing it releases only the order count. The remainder the
/// caller rests adds one back for its CLOB order.
///
/// Returns `None` when there is no payable work. That happens when the order
/// is already triggered, or when a risk-increasing trigger on a failing
/// account is cancelled instead of fired. The caller skips the fill and the
/// reservoir payout on `None`, so a failing account's cancel cannot drain the
/// market reservoir.
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

    // The transform runs on a copy of the slot's order, not on the slot.
    // Freeing the slot then never reserves exposure the detached fill does
    // not rest.
    let mut fired = user.orders[order_index];
    {
        let perp_market = maps.perp_market_map.get_ref(&market_index)?;
        arm_trigger_order(&mut fired, &oracle_price_data, &perp_market, state, slot)?;
    }

    let is_risk_increasing = fired_order_increases_risk(user, &fired, oracle_price)?;

    // A risk-increasing trigger on a failing account cancels instead of
    // firing. `trigger_must_cancel` is the same gate `trigger_order` runs, and
    // it runs before any reward.
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

    // The fill the caller runs settles its own fees. This is the trigger's
    // own reward, paid once for the crank that fired the order.
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
    let Some(order_index) = find_triggerable_order(user, order_id, now)? else {
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
/// `None` means the order is already triggered, which is not an error. The
/// caller reports no payable work and leaves the order alone.
fn find_triggerable_order(user: &User, order_id: u32, now: i64) -> VelocityResult<Option<usize>> {
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

    // A placed trigger's slot reads as untriggered, which keeps it out of
    // every discovery path. Its live order already rests on the CLOB, so
    // this guards against it explicitly.
    validate!(
        !order.is_placed_on_clob(),
        ErrorCode::OrderPlacedOnClob,
        "Order is placed on the CLOB"
    )?;

    // An evicted trigger rearms behind an edge gate. Only the book crank that
    // set the gate clears it, once price moves back off the trigger side. This
    // path makes no such observation, so firing here would refire the order at
    // the price that already evicted it.
    validate!(
        !order.is_bit_flag_set(OrderBitFlag::AwaitingTriggerRecross),
        ErrorCode::OrderAwaitingTriggerRecross,
        "Order waits for the trigger price to cross back after an eviction"
    )?;

    if order.triggered() {
        msg!("Order is already triggered");
        return Ok(None);
    }

    // An armed trigger past its own max_ts is dead. should_expire_order exempts
    // anything that must trigger, so it sits until the owner cancels it. Firing
    // it moves nothing, since the fill finds it expired and the book refuses to
    // rest it. The flat reward would then only charge the owner for a free cancel.
    if order.max_ts != 0 && now > order.max_ts {
        msg!(
            "Order max_ts {} passed (now {}); nothing to trigger",
            order.max_ts,
            now
        );

        return Ok(None);
    }

    validate!(
        order.market_type == MarketType::Perp,
        ErrorCode::InvalidOrderMarketType,
        "Order must be a perp order"
    )?;

    Ok(Some(order_index))
}

/// The market gates every trigger passes, whichever endpoint fires it.
///
/// The fill pause gate matches `fill_perp_order`. `MarketStatus` is judged per
/// endpoint: a market order admits `ReduceOnly`, a limit order needs `Active`.
/// Triggering is also forbidden in settlement. Otherwise the reward creates a
/// claim that drains PnL-pool headroom owed to expiry claimants.
pub(crate) fn trigger_market_gates(perp_market: &PerpMarket, now: i64) -> VelocityResult {
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

    Ok(())
}

/// The market and oracle gates a fired market order passes, and the price its
/// condition is judged at.
fn trigger_market_preflight(
    state: &State,
    perp_market: &PerpMarket,
    oracle_map: &mut OracleMap,
    now: i64,
) -> VelocityResult<(OraclePriceData, u64)> {
    trigger_market_gates(perp_market, now)?;

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
        // The minimum auction lasts 8 seconds, counted in the stored 400ms
        // unit.
        Millis::from_secs(8)
            .div_periods(Millis::UNIT)
            .min(u8::MAX as u64) as u8,
        Some(perp_market),
        state.slot_clock(),
    )
}

/// Whether the fired order increases the account's risk.
///
/// The check applies the order's worst-case exposure to the position,
/// measures, then removes the exposure again. The detached fill does not rest
/// the order, so the reservation must not stay.
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
/// and to the authority-wide equity breaker. The breaker check matches the
/// fill, withdraw and transfer paths. While the breaker is set, no
/// risk-increasing action is allowed on any of the authority's subaccounts.
/// The gate runs before the keeper reward is paid, so a keeper earns nothing
/// for turning the resting orders of a frozen or below-floor account into cancels.
///
/// An unverifiable floor rejects the trigger instead of cancelling it. A
/// cancel is irreversible, so an invalid oracle must not destroy a resting
/// order the account may legitimately carry. The keeper retries once the feed
/// recovers, and the gate then resolves either way.
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

    // The floor restricts the user here. It cancels a risk-increasing order
    // that the subaccount may not carry. Every oracle is valid past the check
    // above, so a trusted value below the buffered floor is grounds to cancel.
    Ok(!margin_calc.meets_margin_requirement()
        || net_equity.is_some_and(|net_equity| !net_equity.clears_buffered_floor(user))
        || load!(user_stats_loader)?.is_equity_breaker_tripped())
}

/// Cancel a risk-increasing trigger the account may not carry.
///
/// The subaccount may already sit below its raw floor when the cancel
/// succeeds, so this trips the equity breaker inline. The keeper's trigger is
/// what trips it.
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
/// A keeper that owns the order is paid nothing. It is already loaded as the
/// user, and it cannot be loaded a second time as the filler.
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
/// exposure, so only the order count is released. The caller's fill tolerates
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

    // A reduce-only trigger that rested about 60 seconds is flagged safe for
    // the relaxed oracle delay gate. The rest time is integrated per slot
    // duration regime.
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

#[cfg(test)]
mod gate_tests {
    use {
        super::{find_triggerable_order, trigger_market_gates},
        crate::{
            error::ErrorCode,
            state::{
                market_status::MarketStatus,
                paused_operations::PerpOperation,
                perp_market::PerpMarket,
                user::{MarketType, Order, OrderStatus, OrderTriggerCondition, OrderType, User},
            },
        },
    };

    /// A user holding one armed stop-market with the given expiry.
    fn user_with_armed_trigger(max_ts: i64) -> User {
        let mut user = User::default();
        user.orders[0] = Order {
            order_id: 7,
            status: OrderStatus::Open,
            order_type: OrderType::TriggerMarket,
            market_type: MarketType::Perp,
            trigger_condition: OrderTriggerCondition::Above,
            max_ts,
            ..Order::default()
        };

        user
    }

    /// `should_expire_order` exempts anything that must be triggered, so a
    /// lapsed stop stays armed in its slot. Firing it moves nothing, and the
    /// flat reward would charge the owner for destroying an order they could
    /// cancel for free. It reads as no payable work rather than an error.
    #[test]
    fn an_expired_armed_trigger_is_not_payable_work() {
        let user = user_with_armed_trigger(100);
        assert!(find_triggerable_order(&user, 7, 101).unwrap().is_none());
    }

    #[test]
    fn a_live_armed_trigger_is_payable_work() {
        let user = user_with_armed_trigger(100);
        assert_eq!(find_triggerable_order(&user, 7, 99).unwrap(), Some(0));
    }

    /// Zero means the order never expires, which is the default for a stop.
    #[test]
    fn a_trigger_without_an_expiry_never_reads_as_expired() {
        let user = user_with_armed_trigger(0);
        assert_eq!(find_triggerable_order(&user, 7, i64::MAX).unwrap(), Some(0));
    }

    fn active_market() -> PerpMarket {
        PerpMarket {
            status: MarketStatus::Active,
            ..PerpMarket::default_test()
        }
    }

    #[test]
    fn an_active_market_passes() {
        assert!(trigger_market_gates(&active_market(), 100).is_ok());
    }

    /// Firing a trigger pays the keeper out of the owner and commits the
    /// order, so the market-scoped fill pause has to stop it. `MarketStatus`
    /// carries no fill-paused variant, so a paused market still reads
    /// `Active` and a status check alone does not cover this.
    #[test]
    fn a_fill_paused_market_is_refused() {
        let mut market = active_market();
        market.paused_operations = PerpOperation::Fill as u8;
        assert_eq!(
            trigger_market_gates(&market, 100).err().unwrap(),
            ErrorCode::MarketFillOrderPaused
        );
    }

    #[test]
    fn a_market_in_settlement_is_refused() {
        let mut market = active_market();
        market.status = MarketStatus::Settlement;
        assert_eq!(
            trigger_market_gates(&market, 100).err().unwrap(),
            ErrorCode::MarketPlaceOrderPaused
        );
    }

    /// An expiry already reached puts the market in settlement even while its
    /// status still reads `Active`.
    #[test]
    fn a_market_past_its_expiry_is_refused() {
        let mut market = active_market();
        market.expiry_ts = 50;
        assert_eq!(
            trigger_market_gates(&market, 100).err().unwrap(),
            ErrorCode::MarketPlaceOrderPaused
        );
    }
}
