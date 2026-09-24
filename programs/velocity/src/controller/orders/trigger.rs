//! Firing a trigger order.
//!
//! A trigger order rests dormant until the market reaches its trigger price.
//! Firing it turns it into a live market order and pays the keeper the flat
//! reward. [`trigger_and_route_order`] hands the fired order back detached
//! for the caller to fill straight against the book.

use {super::*, crate::state::perp_market::ContractTier};

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
/// Nothing stays live in `User.orders`. The caller fills the returned order
/// against the book and rests only the remainder, the shape a v1 place takes.
/// The armed slot reserved no exposure, so freeing it releases only the order
/// count. The remainder the caller rests adds one back for its CLOB order.
///
/// Returns `None` when there is no payable work. That happens when the order
/// is past its `max_ts`, or when a risk-increasing trigger on a failing
/// account is cancelled instead of fired. The caller skips the fill and the
/// reservoir payout on `None`, so a failing account's cancel cannot drain the
/// market reservoir.
pub fn trigger_and_route_order(
    order_to_fire: OrderToFire,
    state: &State,
    accounts: &TriggerAccounts,
    maps: &mut AccountMaps,
    clock: &Clock,
) -> VelocityResult<Option<Order>> {
    let now = clock.unix_timestamp;
    let slot = clock.slot;
    let user = &mut load_mut!(accounts.user)?;

    let Some(firing) = open_trigger(user, order_to_fire, state, maps, now)? else {
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
    let contract_tier = maps.perp_market_map.get_ref(&market_index)?.contract_tier;
    update_trigger_order_params(
        &mut fired,
        &oracle_price_data,
        contract_tier,
        slot,
        state.slot_clock(),
    )?;

    // A risk-increasing trigger on a failing account cancels instead of
    // firing. The gate runs before any reward.
    let armed = user.orders[order_index];
    if fired_order_must_cancel(
        user,
        &armed,
        &fired,
        oracle_price,
        accounts.user_stats,
        maps,
    )? {
        cancel_trigger_order(user, order_index, accounts, maps, clock)?;
        return Ok(None);
    }

    let is_isolated = user
        .get_perp_position(market_index)
        .is_ok_and(|position| position.is_isolated());

    // The fill the caller runs settles its own fees. This is the trigger's
    // own reward, paid once for the crank that fired the order.
    let filler_reward = pay_trigger_reward(
        user,
        market_index,
        accounts,
        state.perp_fee_structure.flat_filler_fee,
        maps,
        slot,
    )?;

    TriggerRecord {
        fired,
        user: accounts.user.key(),
        filler: accounts.filler.key(),
        filler_reward,
        oracle_price,
        trigger_price,
        is_isolated_position: is_isolated,
    }
    .emit(now)?;

    free_fired_order_slot(user, order_index)?;

    user.update_last_active_slot(slot);

    Ok(Some(fired))
}

/// The armed order a trigger crank names, and the market the crank runs on.
#[derive(Clone, Copy)]
pub struct OrderToFire {
    pub market_index: u16,
    pub order_id: u32,
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
/// `None` means the order is past its `max_ts`, which is not payable work.
fn open_trigger(
    user: &mut User,
    order_to_fire: OrderToFire,
    state: &State,
    maps: &mut AccountMaps,
    now: i64,
) -> VelocityResult<Option<FiringOrder>> {
    let OrderToFire {
        market_index,
        order_id,
    } = order_to_fire;
    let Some(order_index) = find_triggerable_order(user, order_id, market_index, now)? else {
        return Ok(None);
    };

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

/// The slot of the armed stop-market a trigger crank names.
///
/// The order must be a trigger-market on the crank's perp market. The rest
/// places behind that market's book, so an order on another market would
/// rest on the wrong book. A trigger-limit fires through
/// `trigger_limit_order_v1`, which rests it whole.
///
/// `None` means the order is past its `max_ts`, which is not an error. The
/// caller reports no payable work and leaves the order alone.
fn find_triggerable_order(
    user: &User,
    order_id: u32,
    market_index: u16,
    now: i64,
) -> VelocityResult<Option<usize>> {
    let order_index = user
        .orders
        .iter()
        .position(|order| order.order_id == order_id && order.status == OrderStatus::Open)
        .ok_or_else(print_error!(ErrorCode::OrderDoesNotExist))?;
    let order = &user.orders[order_index];

    validate!(
        order.order_type == OrderType::TriggerMarket,
        ErrorCode::OrderNotTriggerable,
        "only trigger-market orders fire here (trigger-limits go through trigger_limit_order_v1)"
    )?;
    validate!(
        order.market_type == MarketType::Perp && order.market_index == market_index,
        ErrorCode::InvalidOrderMarketType,
        "order is not a perp order on market {}",
        market_index
    )?;

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

    Ok(Some(order_index))
}

/// The market gates every trigger passes, whichever endpoint fires it.
///
/// Each endpoint judges `MarketStatus` itself. A trigger in settlement would
/// pay a reward out of PnL-pool headroom owed to expiry claimants.
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

/// Whether the fired order increases the account's risk on an account that
/// may not take more, so that it must be cancelled instead of fired.
///
/// The fired order's exposure replaces the armed slot's reservation while the
/// gate measures, so initial margin counts the order it admits. The slot's
/// reservation comes back after. The detached fill does not rest the order.
fn fired_order_must_cancel(
    user: &mut User,
    armed: &Order,
    fired: &Order,
    oracle_price: i64,
    user_stats_loader: &AccountLoader<UserStats>,
    maps: &mut AccountMaps,
) -> VelocityResult<bool> {
    let market_index = fired.market_index;
    let (_, worst_case_before) = user
        .get_perp_position(market_index)?
        .worst_case_liability_value(oracle_price)?;

    let armed_reservation = OrderReservation::of_order(armed)?;
    let fired_reservation = OrderReservation::of_order(fired)?;
    user.replace_reservation(&armed_reservation, &fired_reservation)?;

    let (_, worst_case_after) = user
        .get_perp_position(market_index)?
        .worst_case_liability_value(oracle_price)?;
    let must_cancel = worst_case_after > worst_case_before
        && !fired.reduce_only
        && trigger_must_cancel(user, user_stats_loader, maps)?;

    user.replace_reservation(&fired_reservation, &armed_reservation)?;
    Ok(must_cancel)
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

/// Pay the keeper the flat trigger reward, and report what it was paid.
///
/// A keeper that owns the order is paid nothing. It is already loaded as the
/// user, and it cannot be loaded a second time as the filler.
fn pay_trigger_reward(
    user: &mut User,
    market_index: u16,
    accounts: &TriggerAccounts,
    flat_filler_fee: u64,
    maps: &mut AccountMaps,
    slot: u64,
) -> VelocityResult<u64> {
    let mut filler = if accounts.user.key() != accounts.filler.key() {
        Some(load_mut!(accounts.filler)?)
    } else {
        None
    };

    let mut perp_market = maps.perp_market_map.get_ref_mut(&market_index)?;
    pay_keeper_flat_reward_for_perps(
        user,
        filler.as_deref_mut(),
        &mut perp_market,
        flat_filler_fee,
        slot,
    )
}

/// The `OrderAction::Trigger` record of one fired trigger, on either
/// endpoint.
pub(crate) struct TriggerRecord {
    pub fired: Order,
    pub user: Pubkey,
    pub filler: Pubkey,
    pub filler_reward: u64,
    pub oracle_price: i64,
    pub trigger_price: u64,
    pub is_isolated_position: bool,
}

impl TriggerRecord {
    pub(crate) fn emit(&self, now: i64) -> VelocityResult {
        let bit_flags = set_order_bit_flag(
            0,
            self.is_isolated_position,
            OrderBitFlag::IsIsolatedPosition,
        );
        let order_action_record = get_order_action_record(
            now,
            OrderAction::Trigger,
            OrderActionExplanation::None,
            self.fired.market_index,
            Some(self.filler),
            None,
            Some(self.filler_reward),
            None,
            None,
            Some(self.filler_reward),
            None,
            None,
            None,
            None,
            Some(self.user),
            Some(self.fired),
            None,
            None,
            self.oracle_price,
            bit_flags,
            None,
            None,
            None,
            None,
            Some(self.trigger_price),
            None,
            None,
        )?;

        emit!(order_action_record);

        Ok(())
    }
}

/// Free the armed slot the fired order left behind. The order is now a
/// detached value. The caller's fill tolerates the now-empty position and
/// rebuilds it.
fn free_fired_order_slot(user: &mut User, order_index: usize) -> VelocityResult {
    let reservation = OrderReservation::of_order(&user.orders[order_index])?;
    user.release_orders(&reservation, ReleaseCheck::HeldToReservation)?;
    user.orders[order_index] = Order::default();
    Ok(())
}

/// Turn a dormant trigger order into the live market order it fires as.
pub(super) fn update_trigger_order_params(
    order: &mut Order,
    oracle_price_data: &OraclePriceData,
    contract_tier: ContractTier,
    slot: u64,
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

    // The worst price is stamped now rather than when the order was armed.
    // The oracle it is measured against moved while the order waited.
    let worst_price = derive_worst_price(
        oracle_price_data,
        contract_tier,
        order.direction,
        order.price,
    )?;

    if matches!(order.order_type, OrderType::TriggerMarket) {
        // A fired trigger-market is a market order priced off the oracle it
        // fired against, so it holds its bound as an offset.
        order.add_bit_flag(OrderBitFlag::OracleTriggerMarket);
        order.oracle_price_offset = worst_price
            .cast::<i64>()?
            .safe_sub(oracle_price_data.price)?;
        order.price = 0;
    } else {
        order.price = worst_price;
    }

    msg!("fired trigger worst price {}", worst_price);

    Ok(())
}

#[cfg(test)]
mod cancel_gate_tests;

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

    const MARKET: u16 = 3;

    /// A user holding one armed stop-market with the given expiry.
    fn user_with_armed_trigger(max_ts: i64) -> User {
        let mut user = User::default();
        user.orders[0] = Order {
            order_id: 7,
            status: OrderStatus::Open,
            order_type: OrderType::TriggerMarket,
            market_type: MarketType::Perp,
            market_index: MARKET,
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
        assert!(find_triggerable_order(&user, 7, MARKET, 101)
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_live_armed_trigger_is_payable_work() {
        let user = user_with_armed_trigger(100);
        assert_eq!(
            find_triggerable_order(&user, 7, MARKET, 99).unwrap(),
            Some(0)
        );
    }

    /// Zero means the order never expires, which is the default for a stop.
    #[test]
    fn a_trigger_without_an_expiry_never_reads_as_expired() {
        let user = user_with_armed_trigger(0);
        assert_eq!(
            find_triggerable_order(&user, 7, MARKET, i64::MAX).unwrap(),
            Some(0)
        );
    }

    /// The rest places behind the crank's book, so an order on another market
    /// must not fire through it.
    #[test]
    fn an_order_on_another_market_is_refused() {
        let user = user_with_armed_trigger(0);
        assert_eq!(
            find_triggerable_order(&user, 7, MARKET + 1, 0)
                .err()
                .unwrap(),
            ErrorCode::InvalidOrderMarketType
        );
    }

    /// A stop-limit rests whole through `trigger_limit_order_v1`. Fired here,
    /// it has no price to rest at, and the owner loses it after paying the
    /// reward.
    #[test]
    fn a_trigger_limit_is_refused() {
        let mut user = user_with_armed_trigger(0);
        user.orders[0].order_type = OrderType::TriggerLimit;
        assert_eq!(
            find_triggerable_order(&user, 7, MARKET, 0).err().unwrap(),
            ErrorCode::OrderNotTriggerable
        );
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
