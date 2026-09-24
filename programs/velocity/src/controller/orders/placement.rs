//! Placing a perp order.
//!
//! One order is built from its params, then either written into a slot of
//! `user.orders` as an armed trigger, or handed back as a detached value that
//! routes now. [`build_perp_order`]
//! resolves the order, and [`BuiltPerpOrder::admit`] runs every gate and
//! record that follows it. The two paths differ only in the
//! [`OrderHold`] they pass, so neither can drop a check the other runs.

use super::*;

/// Outcome of a single [`place_perp_trigger_order`] call.
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
/// open-order counter, so a detached taker can route it without ever
/// entering `user.orders`.
pub struct BuiltPerpOrder {
    pub order: Order,
    pub position_index: usize,
    pub risk_increasing: bool,
    pub force_reduce_only: bool,
}

impl BuiltPerpOrder {
    /// The built order, and what the position it lands in makes of it.
    fn new(
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

    /// Run every gate and record that follows the build, and report the
    /// isolated scope the order lands in. `None` is cross margin.
    ///
    /// Both placement paths end here, so neither can drop a check the other
    /// runs. The caller holds the order's reservation on `user`, so margin
    /// measures the account with the order counted.
    fn admit(
        &self,
        user: &mut User,
        user_key: &Pubkey,
        maps: &mut AccountMaps,
        clock: &Clock,
        options: &mut PlaceOrderOptions,
    ) -> VelocityResult<Option<u16>> {
        options.update_risk_increasing(self.risk_increasing);

        let isolated_market_index = user.perp_positions[self.position_index]
            .is_isolated()
            .then_some(self.order.market_index);

        // Bulk placement passes `enforce_margin_check == false` and runs one
        // accumulated check after the batch, in `place_orders`. An early
        // risk-increasing order must not pass under the weaker check a later
        // no-op order would present.
        if options.enforce_margin_check && !options.is_liquidation() {
            meets_place_order_margin_requirement(
                user,
                maps,
                options.risk_increasing,
                isolated_market_index,
            )?;
        }

        if self.force_reduce_only {
            validate_order_for_force_reduce_only(
                &self.order,
                user.perp_positions[self.position_index].base_asset_amount,
            )?;
        }

        let market = &maps.perp_market_map.get_ref(&self.order.market_index)?;
        validate_open_interest_after_order(market, &self.order, self.risk_increasing)?;

        if options.emit_place_record {
            emit_place_records(
                user_key,
                &self.order,
                options.explanation,
                maps.oracle_map.get_price_data(&market.oracle_id())?.price,
                clock.unix_timestamp,
            )?;
        }

        user.update_last_active_slot(clock.slot);

        Ok(isolated_market_index)
    }
}

/// Build a perp `Order` from its params and mint its id, without storing it.
///
/// This is the shared core of order construction. It writes no slot, changes
/// no open-order counter, reserves no `open_bids` or `open_asks`, and runs no
/// margin check. The caller runs those, because a slot placement and a
/// detached fill need them differently.
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
    params: OrderParams,
    options: &PlaceOrderOptions,
    rev_share_order: &mut Option<&mut RevenueShareOrder>,
) -> VelocityResult<Option<BuiltPerpOrder>> {
    validate!(
        params.market_type == MarketType::Perp,
        ErrorCode::InvalidOrderMarketType,
        "must be perp order"
    )?;

    let now = clock.unix_timestamp;
    let market_index = params.market_index;

    // The market's own gates, and the one value the sizing below reads. The
    // borrow ends with this block. A maximum-size order prices against every
    // market the user holds, so it needs the map free.
    let gates = {
        let market = maps.perp_market_map.get_ref(&market_index)?;
        perp_placement_market_gates(&market, user, now)?
    };

    let position_index = get_position_index(&user.perp_positions, market_index)
        .or_else(|_| add_new_position(&mut user.perp_positions, market_index))?;
    let sizing = resolve_order_size(
        user,
        position_index,
        &params,
        options,
        gates.order_step_size,
        maps,
    )?;

    let market = &maps.perp_market_map.get_ref(&market_index)?;
    let oracle_price_data = maps.oracle_map.get_price_data(&market.oracle_id())?;
    let Some(terms) = resolve_order_terms(oracle_price_data, market.contract_tier, &params, now)?
    else {
        // The order id is not consumed yet, so the next placement reuses it.
        return skip_placement(rev_share_order);
    };

    let reduce_only = params.reduce_only || gates.force_reduce_only;
    let resolved = ResolvedOrderFields {
        order_id: get_then_update_id!(user, next_order_id),
        slot: clock.slot,
        order_slot: options.get_order_slot(clock.slot),
        sizing,
        reduce_only,
        worst_price: terms.worst_price,
        max_ts: terms.max_ts,
        bit_flags: new_order_bit_flags(
            &params,
            options,
            reduce_only,
            rev_share_order.is_some(),
            user.perp_positions[position_index].is_isolated(),
        ),
    };
    let new_order = Order::new_perp(&params, market, resolved)?;

    if !validate_built_order(
        &new_order,
        market,
        state,
        clock.slot,
        oracle_price_data.price,
        params.post_only,
    )? {
        // The order id is already consumed, so no later order reuses it.
        return skip_placement(rev_share_order);
    }

    BuiltPerpOrder::new(new_order, user, position_index, gates.force_reduce_only).map(Some)
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

/// What a market lets through, and the step size an order rounds to.
struct PerpPlacementGates {
    /// The market admits only orders that shrink a position.
    force_reduce_only: bool,
    order_step_size: u64,
}

/// The gates every perp placement passes before the order is sized.
fn perp_placement_market_gates(
    market: &PerpMarket,
    user: &User,
    now: i64,
) -> VelocityResult<PerpPlacementGates> {
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

    Ok(PerpPlacementGates {
        force_reduce_only: market.is_reduce_only()?,
        order_step_size: market.order_step_size,
    })
}

/// The base an order carries, against the position it lands on.
struct OrderSizing {
    /// The direction the position already runs, unless the caller overrode it.
    existing_position_direction: PositionDirection,
    base_asset_amount: u64,
}

/// Size the order against its position.
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
) -> VelocityResult<OrderSizing> {
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

    Ok(OrderSizing {
        existing_position_direction,
        base_asset_amount,
    })
}

/// The price the order fills no worse than, and the time it lives.
#[derive(Clone, Copy)]
struct OrderTerms {
    worst_price: u64,
    max_ts: i64,
}

/// Resolve the order's worst price and its time in force.
///
/// A market order that names no price takes its cap from
/// [`derive_worst_price`]. Every other type carries a price it chose. A trigger order is stamped when it fires, not
/// here, because the oracle it is measured against moves while it waits.
///
/// `None` means the order has already expired, which is not an error. The
/// caller skips the placement.
fn resolve_order_terms(
    oracle_price_data: &OraclePriceData,
    contract_tier: crate::state::perp_market::ContractTier,
    params: &OrderParams,
    now: i64,
) -> VelocityResult<Option<OrderTerms>> {
    let worst_price = match params.order_type {
        OrderType::Market => derive_worst_price(
            oracle_price_data,
            contract_tier,
            params.direction,
            params.price,
        )?,
        _ => params.price,
    };

    let max_ts = match params.max_ts {
        Some(max_ts) => max_ts,
        None => default_order_max_ts(params.order_type, now)?,
    };

    if max_ts != 0 && max_ts < now {
        msg!("max_ts ({}) < now ({}), skipping order", max_ts, now);
        return Ok(None);
    }

    Ok(Some(OrderTerms {
        worst_price,
        max_ts,
    }))
}

/// The `max_ts` an order gets when its params name none. A market or oracle
/// order lives `DEFAULT_MARKET_ORDER_LIFETIME_SECONDS`, so its rested remainder
/// cannot outlast its worst price. `max_ts` ends an order and never delays a
/// fill. Every other order type lives until it is cancelled.
fn default_order_max_ts(order_type: OrderType, now: i64) -> VelocityResult<i64> {
    match order_type {
        OrderType::Market | OrderType::Oracle => {
            now.safe_add(DEFAULT_MARKET_ORDER_LIFETIME_SECONDS)
        }
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
    sizing: OrderSizing,
    reduce_only: bool,
    /// The worst price the order fills at. A market order takes the bound
    /// [`derive_worst_price`] resolves; every other type names its own.
    worst_price: u64,
    max_ts: i64,
    bit_flags: u8,
}

impl Order {
    /// One perp order, from its params and the fields resolved for it.
    fn new_perp(
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
                resolved.worst_price,
                params.direction,
                params.post_only,
                &market.amm,
                market.order_tick_size,
            )?,

            existing_position_direction: resolved.sizing.existing_position_direction,
            base_asset_amount: resolved.sizing.base_asset_amount,
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
            clob_node_index: 0,
            clob_order_id: 0,
            unused_auction_duration: 0,
            max_ts: resolved.max_ts,
            posted_slot_tail: get_posted_slot_from_clock_slot(resolved.slot),
            bit_flags: resolved.bit_flags,
            padding: [0; 5],
        })
    }
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

/// Arm a perp trigger order in a slot of `user.orders`, where it waits for
/// its condition. A live order rests on the market's book instead, through
/// [`create_detached_perp_order`], so a slot now holds only conditionals.
///
/// Only `max_ts` is resolved here. The worst price is stamped when the order
/// fires, because the oracle it is measured against moves while it waits.
pub fn place_perp_trigger_order(
    state: &State,
    user: &mut User,
    user_key: Pubkey,
    maps: &mut AccountMaps,
    clock: &Clock,
    params: OrderParams,
    mut options: PlaceOrderOptions,
    rev_share_order: &mut Option<&mut RevenueShareOrder>,
) -> VelocityResult<PlaceOrderResult> {
    validate!(
        params.is_trigger_order(),
        ErrorCode::OrderTypeNotConditional,
        "a live order rests on the market's book, not in a user order slot"
    )?;

    // Both trigger executors fire onto the market's CLOB, so a trigger armed
    // on a market with none would stay armed with nothing to fire it.
    validate!(
        maps.perp_market_map.get_ref(&params.market_index)?.clob_market != Pubkey::default(),
        ErrorCode::TriggerMarketHasNoClob,
        "market {} has no CLOB to fire a trigger onto",
        params.market_index
    )?;

    validate_placement_preconditions(state, user, maps, &options, &params)?;

    if options.try_expire_orders {
        expire_orders(user, &user_key, maps, clock.unix_timestamp, clock.slot)?;
    }

    // Taken before the build, because a full order list must refuse the
    // placement rather than mint an id for an order with nowhere to go.
    let order_index = next_order_slot(user, params.user_order_id)?;

    let Some(built) =
        build_perp_order(state, user, maps, clock, params, &options, rev_share_order)?
    else {
        return Ok(PlaceOrderResult::default());
    };

    commit_order_to_slot(user, order_index, &built.order)?;
    let isolated_market_index = built.admit(user, &user_key, maps, clock, &mut options)?;

    Ok(PlaceOrderResult {
        risk_increasing: built.risk_increasing,
        isolated_market_index: isolated_market_index.filter(|_| built.risk_increasing),
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

/// Write the order into its slot and reserve what it holds open.
fn commit_order_to_slot(user: &mut User, order_index: usize, order: &Order) -> VelocityResult {
    user.orders[order_index] = *order;
    user.reserve_orders(&OrderReservation::of_order(order)?)?;
    Ok(())
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

/// Create a perp order that never touches `user.orders`. The order comes back
/// as a value, holding no slot and no `open_bids` or `open_asks` reservation.
/// It still mints an id, writes a builder-order row when one applies, and
/// stamps activity. The caller routes it through
/// `FillTarget::Detached { reserved: false }` and rests only its remainder on
/// the CLOB. `None` is [`build_perp_order`]'s soft skip.
///
/// The caller sweeps expired slot orders first. An expired order still holds
/// its reservation, and releasing it can be what lets this one pass the
/// margin gate. `options.try_expire_orders` is not read here.
#[allow(clippy::too_many_arguments)]
pub fn create_detached_perp_order(
    state: &State,
    user: &mut User,
    user_key: Pubkey,
    maps: &mut AccountMaps,
    clock: &Clock,
    params: OrderParams,
    mut options: PlaceOrderOptions,
    rev_share_order: &mut Option<&mut RevenueShareOrder>,
) -> VelocityResult<Option<Order>> {
    validate_placement_preconditions(state, user, maps, &options, &params)?;

    let Some(built) =
        build_perp_order(state, user, maps, clock, params, &options, rev_share_order)?
    else {
        return Ok(None);
    };

    // The order is admitted as if it rested, then handed back holding nothing.
    let reservation = OrderReservation::of_order(&built.order)?;
    user.reserve_orders(&reservation)?;
    built.admit(user, &user_key, maps, clock, &mut options)?;
    user.release_orders(&reservation, ReleaseCheck::HeldToReservation)?;

    Ok(Some(built.order))
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

#[cfg(test)]
mod gate_tests {
    use {
        super::next_order_slot,
        crate::{
            error::ErrorCode,
            state::user::{MarketType, Order, OrderStatus, OrderType, User},
        },
    };

    /// A user holding one live order with the given `user_order_id`.
    fn user_with_live_order(user_order_id: u8) -> User {
        let mut user = User::default();
        user.orders[0] = Order {
            user_order_id,
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            market_type: MarketType::Perp,
            ..Order::default()
        };

        user
    }

    #[test]
    fn a_fresh_user_order_id_takes_the_first_free_slot() {
        let user = User::default();
        assert_eq!(next_order_slot(&user, 7), Ok(0));
    }

    #[test]
    fn a_live_user_order_id_refuses_reuse() {
        let user = user_with_live_order(7);
        assert_eq!(
            next_order_slot(&user, 7),
            Err(ErrorCode::UserOrderIdAlreadyInUse)
        );
    }

    /// `user_order_id` zero means the caller did not name the order, so the
    /// same value on two orders is not a collision.
    #[test]
    fn a_zero_user_order_id_never_collides() {
        let user = user_with_live_order(0);
        assert_eq!(next_order_slot(&user, 0), Ok(1));
    }
}
