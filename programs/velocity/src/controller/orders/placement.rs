//! Placing a perp order.
//!
//! One order is built from its params, then either written into a slot of
//! `user.orders` or handed back as a detached value. Both paths run the same
//! preconditions, the same margin gate and the same open-interest guard, so
//! the two cannot drift.

use super::*;

/// Outcome of a single [`place_perp_order`] call.
///
/// Batch placement (`enforce_batch_margin`) defers the margin
/// check until after every order is placed, so it needs to know what risk each
/// individual placement introduced. This struct carries that back: whether the
/// order increased the user's risk, and — when it did against an isolated
/// position — which isolated market scope must meet initial margin. A no-op
/// placement (skipped order, `TryPostOnly` that couldn't post) returns the
/// default (`risk_increasing == false`, `isolated_market_index == None`).
#[derive(Clone, Copy, Debug, Default)]
pub struct PlaceOrderResult {
    /// Whether this order increased the user's risk in its market/position.
    pub risk_increasing: bool,
    /// `Some(market_index)` when a risk-increasing order's position is isolated,
    /// identifying the isolated scope that must meet initial margin. `None` for
    /// a cross-margin order or any non-risk-increasing / no-op placement.
    pub isolated_market_index: Option<u16>,
}

/// A perp `Order` built from its params, ready to place or route detached.
/// [`build_perp_order`] returns this. It holds no slot and touches no
/// open-order counter, so an ephemeral taker can route it without ever
/// entering `user.orders`.
pub struct BuiltPerpOrder {
    pub order: Order,
    pub position_index: usize,
    pub risk_increasing: bool,
    pub force_reduce_only: bool,
}

impl BuiltPerpOrder {
    /// The built order, and what the position it lands in makes of it.
    fn of(
        order: Order,
        user: &User,
        position_index: usize,
        force_reduce_only: bool,
    ) -> VelocityResult<Self> {
        let position = &user.perp_positions[position_index];
        let risk_increasing = is_new_order_risk_increasing(
            &order,
            position.base_asset_amount,
            position.open_bids,
            position.open_asks,
        )?;

        Ok(Self {
            order,
            position_index,
            risk_increasing,
            force_reduce_only,
        })
    }
}

/// Build a perp `Order` from its params and mint its id, without storing it.
///
/// This is the shared core of order construction. It writes no slot, changes
/// no open-order counter, reserves no `open_bids` or `open_asks`, and runs no
/// margin check. The caller runs those, because a slot placement and an
/// ephemeral detached fill need them differently.
///
/// Returns `None` for the two soft skips that are not errors. Those are an
/// already expired `max_ts`, and a `TryPostOnly` order that would cross. Both
/// clear the builder-order row so it cannot linger.
///
/// The caller must run the placement preconditions first. Those are the
/// not-liquidated check, the not-bankrupt check, and the reduce-only-user
/// gate. They guard the whole placement rather than the order value.
#[allow(clippy::too_many_arguments)]
pub fn build_perp_order(
    state: &State,
    user: &mut User,
    maps: &mut AccountMaps,
    clock: &Clock,
    mut params: OrderParams,
    options: &PlaceOrderOptions,
    rev_share_order: &mut Option<&mut RevenueShareOrder>,
) -> VelocityResult<Option<BuiltPerpOrder>> {
    let now = clock.unix_timestamp;
    let slot: u64 = clock.slot;
    let market_index = params.market_index;

    // The market's own gates, and the one value the sizing below reads. The
    // borrow ends with this block. A maximum-size order prices against every
    // market the user holds, so it needs the map free.
    let (force_reduce_only, order_step_size) = {
        let market = maps.perp_market_map.get_ref(&market_index)?;
        perp_placement_market_gates(&market, user, now)?
    };

    let position_index = get_position_index(&user.perp_positions, market_index)
        .or_else(|_| add_new_position(&mut user.perp_positions, market_index))?;
    let (existing_position_direction, base_asset_amount) = resolve_order_size(
        user,
        position_index,
        &params,
        options,
        order_step_size,
        maps,
    )?;

    let market = &maps.perp_market_map.get_ref(&market_index)?;
    let oracle_price_data = maps.oracle_map.get_price_data(&market.oracle_id())?;
    let Some(auction) =
        resolve_auction_and_max_ts(state, market, oracle_price_data, &mut params, options, now)?
    else {
        // The order id is not consumed yet, so the next placement reuses it.
        return skip_placement(rev_share_order);
    };

    validate!(
        params.market_type == MarketType::Perp,
        ErrorCode::InvalidOrderMarketType,
        "must be perp order"
    )?;

    let reduce_only = params.reduce_only || force_reduce_only;
    let resolved = ResolvedOrderFields {
        order_id: get_then_update_id!(user, next_order_id),
        slot,
        order_slot: options.get_order_slot(slot),
        existing_position_direction,
        base_asset_amount,
        reduce_only,
        auction,
        bit_flags: new_order_bit_flags(
            &params,
            options,
            reduce_only,
            rev_share_order.is_some(),
            user.perp_positions[position_index].is_isolated(),
        ),
    };
    let new_order = assemble_perp_order(&params, market, resolved)?;

    if !validate_built_order(
        &new_order,
        market,
        state,
        slot,
        oracle_price_data.price,
        params.post_only,
    )? {
        // The order id is already consumed, so no later order reuses it.
        return skip_placement(rev_share_order);
    }

    BuiltPerpOrder::of(new_order, user, position_index, force_reduce_only).map(Some)
}

/// Report a placement that stops before it produces an order.
/// `add_builder_order` writes the builder-order row before the order is
/// built, so a placement that returns without one must free the row, keyed
/// to an order id that a later order could reuse and find.
fn skip_placement<T>(
    rev_share_order: &mut Option<&mut RevenueShareOrder>,
) -> VelocityResult<Option<T>> {
    clear_placed_builder_order(rev_share_order);
    Ok(None)
}

/// The gates every perp placement passes before the order is sized, and the
/// step size the sizing rounds to.
///
/// Returns whether the market forces the order reduce-only, and the market's
/// order step size.
fn perp_placement_market_gates(
    market: &PerpMarket,
    user: &User,
    now: i64,
) -> VelocityResult<(bool, u64)> {
    validate!(
        !matches!(market.status, MarketStatus::Initialized),
        ErrorCode::MarketBeingInitialized,
        "Market is being initialized"
    )?;

    validate!(
        user.pool_id == 0,
        ErrorCode::InvalidPoolId,
        "user pool id ({}) != 0",
        user.pool_id
    )?;

    validate!(
        !market.is_in_settlement(now),
        ErrorCode::MarketPlaceOrderPaused,
        "Market is in settlement mode",
    )?;

    Ok((market.is_reduce_only()?, market.order_step_size))
}

/// The base the order carries and the direction the position already runs.
///
/// A `u64::MAX` size means the largest order the user can carry, which prices
/// against every market the user holds.
fn resolve_order_size(
    user: &User,
    position_index: usize,
    params: &OrderParams,
    options: &PlaceOrderOptions,
    order_step_size: u64,
    maps: &mut AccountMaps,
) -> VelocityResult<(PositionDirection, u64)> {
    validate!(
        params.base_asset_amount >= order_step_size,
        ErrorCode::OrderAmountTooSmall,
        "params.base_asset_amount={} cannot be below market.order_step_size={}",
        params.base_asset_amount,
        order_step_size
    )?;

    let base_asset_amount = if params.base_asset_amount == u64::MAX
        && !(params.is_trigger_order() && params.reduce_only)
    {
        calculate_max_perp_order_size(
            user,
            position_index,
            params.market_index,
            params.direction,
            maps,
        )?
    } else {
        standardize_base_asset_amount(params.base_asset_amount, order_step_size)?
    };

    let existing_position_direction = match options.existing_position_direction_override {
        Some(existing_position_direction_override) => existing_position_direction_override,
        None if user.perp_positions[position_index].base_asset_amount >= 0 => {
            PositionDirection::Long
        }
        None => PositionDirection::Short,
    };

    Ok((existing_position_direction, base_asset_amount))
}

/// The auction one order runs, and the time the order lives.
#[derive(Clone, Copy)]
struct OrderAuction {
    start_price: i64,
    end_price: i64,
    duration: u8,
    max_ts: i64,
}

/// Resolve the order's auction and its time in force.
///
/// A crossing limit order without an auction duration gets its auction params
/// here. A liquidation keeps the params it was given.
///
/// `None` means the order has already expired, which is not an error. The
/// caller skips the placement.
fn resolve_auction_and_max_ts(
    state: &State,
    market: &PerpMarket,
    oracle_price_data: &OraclePriceData,
    params: &mut OrderParams,
    options: &PlaceOrderOptions,
    now: i64,
) -> VelocityResult<Option<OrderAuction>> {
    // Downstream auction-param / price / validation logic reads the AMM's
    // cached spread state directly (refreshed by the keeper crank / fill
    // setup), matching pre-decoupling behaviour where order placement quoted
    // off the last-cranked spread rather than recomputing it here.
    if !options.is_liquidation() {
        params.update_perp_auction_params(
            market,
            oracle_price_data.price,
            options.is_signed_msg_order(),
        )?;
    }

    let (start_price, end_price, duration) = get_auction_params(
        params,
        oracle_price_data,
        market.order_tick_size,
        // The stored minimum is already in the auction's 400ms wall-clock
        // units.
        legacy_slot_duration_u8_raw(state.min_perp_auction_duration),
    )?;

    let max_ts = match params.max_ts {
        Some(max_ts) => max_ts,
        None => default_order_max_ts(params.order_type, now, duration)?,
    };

    if max_ts != 0 && max_ts < now {
        msg!("max_ts ({}) < now ({}), skipping order", max_ts, now);
        return Ok(None);
    }

    Ok(Some(OrderAuction {
        start_price,
        end_price,
        duration,
        max_ts,
    }))
}

/// The time in force an auctioned order gets when its params name none.
///
/// The default is at least 30 seconds. Otherwise it is the auction's
/// wall-clock length plus a quarter again, plus 10 seconds of pad. The default
/// therefore always outlives the auction. The division by 800 reproduces the
/// historical `auction_duration_slots / 2 + 10` exactly. A 400ms unit is one
/// historical slot, so units/2 equals ms/800. An order type that runs no
/// auction never expires by default.
fn default_order_max_ts(
    order_type: OrderType,
    now: i64,
    auction_duration: u8,
) -> VelocityResult<i64> {
    match order_type {
        OrderType::Market | OrderType::Oracle => now.safe_add(
            30_i64.max(
                Millis::from_stored_units(auction_duration as u64)
                    .as_ms()
                    .safe_div(800)?
                    .cast::<i64>()?
                    .safe_add(10_i64)?,
            ),
        ),
        _ => Ok(0_i64),
    }
}

/// The bit flags a new order carries.
fn new_order_bit_flags(
    params: &OrderParams,
    options: &PlaceOrderOptions,
    reduce_only: bool,
    has_builder: bool,
    is_isolated: bool,
) -> u8 {
    let mut bit_flags = set_order_bit_flag(
        0,
        options.is_signed_msg_order(),
        OrderBitFlag::SignedMessage,
    );

    bit_flags = set_order_bit_flag(
        bit_flags,
        params.is_trigger_order() && reduce_only,
        OrderBitFlag::NewTriggerReduceOnly,
    );
    bit_flags = set_order_bit_flag(bit_flags, has_builder, OrderBitFlag::HasBuilder);
    set_order_bit_flag(bit_flags, is_isolated, OrderBitFlag::IsIsolatedPosition)
}

/// Everything one order needs beyond its params and its market.
struct ResolvedOrderFields {
    order_id: u32,
    /// The clock slot the placement runs on, which stamps the posted slot.
    slot: u64,
    /// The slot the order counts as placed on, which a signed message order
    /// backdates.
    order_slot: u64,
    existing_position_direction: PositionDirection,
    base_asset_amount: u64,
    reduce_only: bool,
    auction: OrderAuction,
    bit_flags: u8,
}

/// Write one perp order from its params and the fields resolved for it.
fn assemble_perp_order(
    params: &OrderParams,
    market: &PerpMarket,
    resolved: ResolvedOrderFields,
) -> VelocityResult<Order> {
    Ok(Order {
        status: OrderStatus::Open,
        order_type: params.order_type,
        market_type: params.market_type,
        slot: resolved.order_slot,
        order_id: resolved.order_id,
        user_order_id: params.user_order_id,
        market_index: params.market_index,
        price: get_price_for_perp_order(
            params.price,
            params.direction,
            params.post_only,
            &market.amm,
            market.order_tick_size,
        )?,

        existing_position_direction: resolved.existing_position_direction,
        base_asset_amount: resolved.base_asset_amount,
        base_asset_amount_filled: 0,
        quote_asset_amount_filled: 0,
        direction: params.direction,
        reduce_only: resolved.reduce_only,
        trigger_price: standardize_price(
            params.trigger_price.unwrap_or(0),
            market.order_tick_size,
            params.direction,
        )?,

        trigger_condition: params.trigger_condition,
        post_only: params.post_only != PostOnlyParam::None,
        oracle_price_offset: params.oracle_price_offset.unwrap_or(0),
        immediate_or_cancel: params.is_immediate_or_cancel(),
        auction_start_price: resolved.auction.start_price,
        auction_end_price: resolved.auction.end_price,
        auction_duration: resolved.auction.duration,
        max_ts: resolved.auction.max_ts,
        posted_slot_tail: get_posted_slot_from_clock_slot(resolved.slot),
        bit_flags: resolved.bit_flags,
        padding: [0; 5],
    })
}

/// Whether the built order may be placed.
///
/// `false` is the one soft skip. A `TryPostOnly` order that would cross is not
/// an error, and the caller returns without an order.
fn validate_built_order(
    order: &Order,
    market: &PerpMarket,
    state: &State,
    slot: u64,
    oracle_price: i64,
    post_only: PostOnlyParam,
) -> VelocityResult<bool> {
    match validate_order(order, market, Some(oracle_price), slot, state.slot_clock()) {
        Ok(()) => Ok(true),
        Err(ErrorCode::PlacePostOnlyLimitFailure) if post_only == PostOnlyParam::TryPostOnly => {
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

pub fn place_perp_order(
    state: &State,
    user: &mut User,
    user_key: Pubkey,
    maps: &mut AccountMaps,
    clock: &Clock,
    params: OrderParams,
    mut options: PlaceOrderOptions,
    rev_share_order: &mut Option<&mut RevenueShareOrder>,
) -> VelocityResult<PlaceOrderResult> {
    let now = clock.unix_timestamp;
    let slot: u64 = clock.slot;

    validate_placement_preconditions(state, user, maps, &options, &params)?;

    if options.try_expire_orders {
        expire_orders(user, &user_key, maps, now, slot)?;
    }

    let new_order_index = next_order_slot(user, params.user_order_id)?;
    let market_index = params.market_index;

    let Some(built) =
        build_perp_order(state, user, maps, clock, params, &options, rev_share_order)?
    else {
        return Ok(PlaceOrderResult::default());
    };

    let BuiltPerpOrder {
        order: new_order,
        position_index,
        risk_increasing,
        force_reduce_only,
    } = built;

    commit_order_to_slot(user, new_order_index, &new_order, position_index)?;
    options.update_risk_increasing(risk_increasing);

    let isolated_market_index = user.perp_positions[position_index]
        .is_isolated()
        .then_some(market_index);

    // Single-order placement checks margin here. Bulk placement passes
    // `enforce_margin_check == false` and instead runs one accumulated check
    // after the whole batch (see `place_orders`), so an early risk-increasing
    // order cannot be admitted under a weaker check by a later no-op order.
    if options.enforce_margin_check && !options.is_liquidation() {
        meets_place_order_margin_requirement(
            user,
            maps,
            options.risk_increasing,
            isolated_market_index,
        )?;
    }

    if force_reduce_only {
        validate_order_for_force_reduce_only(
            &user.orders[new_order_index],
            user.perp_positions[position_index].base_asset_amount,
        )?;
    }

    let market = &maps.perp_market_map.get_ref(&market_index)?;
    validate_open_interest_after_order(market, &new_order, risk_increasing)?;
    emit_place_records(
        &user_key,
        &new_order,
        options.explanation,
        maps.oracle_map.get_price_data(&market.oracle_id())?.price,
        now,
    )?;

    user.update_last_active_slot(slot);

    Ok(PlaceOrderResult {
        risk_increasing,
        isolated_market_index: isolated_market_index.filter(|_| risk_increasing),
    })
}

/// The gates that guard a whole placement, before the order value itself.
fn validate_placement_preconditions(
    state: &State,
    user: &mut User,
    maps: &mut AccountMaps,
    options: &PlaceOrderOptions,
    params: &OrderParams,
) -> VelocityResult {
    if !options.is_liquidation() {
        validate_user_not_being_liquidated(user, maps, state.liquidation_margin_buffer_ratio)?;
    }

    validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

    if user.is_reduce_only() {
        validate!(
            params.reduce_only,
            ErrorCode::UserReduceOnly,
            "order must be reduce only"
        )?;
    }

    Ok(())
}

/// The free slot of `user.orders` the new order takes.
///
/// A non-zero `user_order_id` is the caller's own handle on the order, so it
/// must not name two live orders at once.
fn next_order_slot(user: &User, user_order_id: u8) -> VelocityResult<usize> {
    let new_order_index = user
        .orders
        .iter()
        .position(|order| order.is_available())
        .ok_or(ErrorCode::MaxNumberOfOrders)?;

    if user_order_id > 0
        && user
            .orders
            .iter()
            .any(|order| order.user_order_id == user_order_id && !order.is_available())
    {
        msg!("user_order_id is already in use {}", user_order_id);
        return Err(ErrorCode::UserOrderIdAlreadyInUse);
    }

    Ok(new_order_index)
}

/// Write the order into its slot and reserve the exposure it holds open.
fn commit_order_to_slot(
    user: &mut User,
    order_index: usize,
    order: &Order,
    position_index: usize,
) -> VelocityResult {
    user.increment_open_orders(order.has_auction());
    user.orders[order_index] = *order;
    user.perp_positions[position_index].open_orders += 1;
    increase_open_bids_and_asks(
        &mut user.perp_positions[position_index],
        &order.direction,
        order.base_asset_amount,
        order.update_open_bids_and_asks(),
    )
}

/// Hold the market to its open-interest cap with the new order added.
///
/// A cap of zero is no cap. An order that does not increase risk cannot breach
/// one.
fn validate_open_interest_after_order(
    market: &PerpMarket,
    order: &Order,
    risk_increasing: bool,
) -> VelocityResult {
    let max_oi = market.max_open_interest;
    if max_oi == 0 || !risk_increasing {
        return Ok(());
    }

    let oi_plus_order = match order.direction {
        PositionDirection::Long => market
            .base_asset_amount_long
            .safe_add(order.base_asset_amount.cast()?)?
            .unsigned_abs(),
        PositionDirection::Short => market
            .base_asset_amount_short
            .safe_sub(order.base_asset_amount.cast()?)?
            .unsigned_abs(),
    };

    validate!(
        oi_plus_order <= max_oi,
        ErrorCode::MaxOpenInterest,
        "Order Base Amount={} could breach Max Open Interest for Perp Market={}",
        order.base_asset_amount,
        market.market_index
    )
}

/// Emit the two records a placement makes: the order action and the order.
fn emit_place_records(
    user_key: &Pubkey,
    order: &Order,
    explanation: OrderActionExplanation,
    oracle_price: i64,
    now: i64,
) -> VelocityResult {
    let (taker, taker_order, maker, maker_order) =
        get_taker_and_maker_for_order_record(user_key, order);

    let order_action_record = get_order_action_record(
        now,
        OrderAction::Place,
        explanation,
        order.market_index,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        taker,
        taker_order,
        maker,
        maker_order,
        oracle_price,
        order.bit_flags,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )?;

    emit_stack::<_, { OrderActionRecord::SIZE }>(order_action_record)?;

    emit_stack::<_, { OrderRecord::SIZE }>(OrderRecord {
        ts: now,
        user: *user_key,
        order: *order,
    })
}

/// Whether `user` can carry one more order of this shape, without keeping any
/// of it.
///
/// The margin engine prices the user with the prospective exposure, so the
/// check models the reservation and then reverses it. The model covers the
/// aggregates and the per-open-order flat term. The user is left as it was.
/// Both the ephemeral create and the remainder rest gate through here, so the
/// two paths cannot drift.
#[allow(clippy::too_many_arguments)]
pub fn check_prospective_order_margin(
    user: &mut User,
    position_index: usize,
    direction: &PositionDirection,
    base_asset_amount: u64,
    update_open_bids_and_asks: bool,
    risk_increasing: bool,
    isolated_market_index: Option<u16>,
    maps: &mut AccountMaps,
) -> VelocityResult<()> {
    increase_open_bids_and_asks(
        &mut user.perp_positions[position_index],
        direction,
        base_asset_amount,
        update_open_bids_and_asks,
    )?;

    // The requirement carries a flat term per open order, so the model
    // counts the prospective one too.
    let open_orders_before = user.perp_positions[position_index].open_orders;
    user.perp_positions[position_index].open_orders = open_orders_before.saturating_add(1);
    let checked =
        meets_place_order_margin_requirement(user, maps, risk_increasing, isolated_market_index);
    user.perp_positions[position_index].open_orders = open_orders_before;
    decrease_open_bids_and_asks(
        &mut user.perp_positions[position_index],
        direction,
        base_asset_amount,
        update_open_bids_and_asks,
    )?;

    checked
}

/// Validate and create a perp order that never touches `user.orders`.
///
/// This is the straight-to-book path. It runs the same preconditions, margin
/// gate, open-interest guard and place records that `place_perp_order` runs,
/// and it returns the order as a value. Nothing is placed. No slot is written,
/// and no `open_bids` or `open_asks` reservation is kept. What does change on
/// the user are the facts of the order coming into existence: the id counter,
/// a builder-order row when one applies, and the activity stamp. The caller
/// routes the returned order through
/// `FillTarget::Detached { reserved: false }` and rests only its remainder on
/// the CLOB. Returns `None` on the same soft skips as `place_perp_order`,
/// which are an expired `max_ts` and a `TryPostOnly` order that would cross.
///
/// The caller sweeps expired slot orders first, with `expire_orders`. This
/// function never touches `user.orders`, and the sweep matters to the gate. An
/// expired order still holds its reservation, and releasing it can be what
/// lets the new order pass. This function does not read
/// `options.try_expire_orders`.
#[allow(clippy::too_many_arguments)]
pub fn create_ephemeral_perp_order(
    state: &State,
    user: &mut User,
    user_key: Pubkey,
    maps: &mut AccountMaps,
    clock: &Clock,
    params: OrderParams,
    mut options: PlaceOrderOptions,
    rev_share_order: &mut Option<&mut RevenueShareOrder>,
) -> VelocityResult<Option<Order>> {
    let now = clock.unix_timestamp;
    let slot: u64 = clock.slot;

    validate_placement_preconditions(state, user, maps, &options, &params)?;

    let market_index = params.market_index;

    let Some(built) =
        build_perp_order(state, user, maps, clock, params, &options, rev_share_order)?
    else {
        return Ok(None);
    };

    let BuiltPerpOrder {
        order,
        position_index,
        risk_increasing,
        force_reduce_only,
    } = built;

    options.update_risk_increasing(risk_increasing);

    let isolated_market_index = user.perp_positions[position_index]
        .is_isolated()
        .then_some(market_index);

    // The ephemeral order never carries a reservation into the fill. The fill
    // unwinds nothing for it, and only the rested remainder reserves, in
    // `try_place_remainder_on_clob`. The check itself is the one
    // `place_perp_order` runs.
    if options.enforce_margin_check && !options.is_liquidation() {
        check_prospective_order_margin(
            user,
            position_index,
            &order.direction,
            order.base_asset_amount,
            order.update_open_bids_and_asks(),
            options.risk_increasing,
            isolated_market_index,
            maps,
        )?;
    }

    if force_reduce_only {
        validate_order_for_force_reduce_only(
            &order,
            user.perp_positions[position_index].base_asset_amount,
        )?;
    }

    let market = &maps.perp_market_map.get_ref(&market_index)?;
    validate_open_interest_after_order(market, &order, risk_increasing)?;

    if options.emit_place_record {
        emit_place_records(
            &user_key,
            &order,
            options.explanation,
            maps.oracle_map.get_price_data(&market.oracle_id())?.price,
            now,
        )?;
    }

    user.update_last_active_slot(slot);

    Ok(Some(order))
}

pub(super) fn get_auction_params(
    params: &OrderParams,
    oracle_price_data: &OraclePriceData,
    tick_size: u64,
    min_auction_duration: u8,
) -> VelocityResult<(i64, i64, u8)> {
    if !matches!(
        params.order_type,
        OrderType::Market | OrderType::Oracle | OrderType::Limit
    ) {
        return Ok((0_i64, 0_i64, 0_u8));
    }

    if params.order_type == OrderType::Limit {
        return match (
            params.auction_start_price,
            params.auction_end_price,
            params.auction_duration,
        ) {
            (Some(auction_start_price), Some(auction_end_price), Some(auction_duration)) => {
                let auction_duration = if auction_duration == 0 {
                    auction_duration
                } else {
                    // if auction is non-zero, force it to be at least min_auction_duration
                    auction_duration.max(min_auction_duration)
                };

                Ok((
                    standardize_price_i64(
                        auction_start_price,
                        tick_size.cast()?,
                        params.direction,
                    )?,
                    standardize_price_i64(auction_end_price, tick_size.cast()?, params.direction)?,
                    auction_duration,
                ))
            }
            _ => Ok((0_i64, 0_i64, 0_u8)),
        };
    }

    let auction_duration = params
        .auction_duration
        .unwrap_or(0)
        .max(min_auction_duration);

    let (auction_start_price, auction_end_price) =
        match (params.auction_start_price, params.auction_end_price) {
            (Some(auction_start_price), Some(auction_end_price)) => {
                (auction_start_price, auction_end_price)
            }
            _ if params.order_type == OrderType::Oracle => {
                msg!("Oracle order must specify auction start and end price offsets");
                return Err(ErrorCode::InvalidOrderAuction);
            }
            _ => calculate_auction_prices(oracle_price_data, params.direction, params.price)?,
        };

    Ok((
        standardize_price_i64(auction_start_price, tick_size.cast()?, params.direction)?,
        standardize_price_i64(auction_end_price, tick_size.cast()?, params.direction)?,
        auction_duration,
    ))
}

/// Clears a builder-order row that `add_builder_order` wrote for a placement that then
/// bailed before committing the order. Resetting the row to default frees the escrow slot —
/// the same "remove" idiom the sweep uses — so a skipped placement leaves no orphaned row
/// keyed to an order id a later order might reuse.
#[inline(always)]
fn clear_placed_builder_order(rev_share_order: &mut Option<&mut RevenueShareOrder>) {
    if let Some(order) = rev_share_order.as_mut() {
        **order = RevenueShareOrder::default();
    }
}
