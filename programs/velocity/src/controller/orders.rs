//! Order lifecycle: placement validation, cancellation, and fill matching (perp + spot).
//! Margin math → `crate::math::margin`. Liquidation fills → `crate::controller::liquidation`.
//! The perp fill chain is three layers. `fill_perp_order` governs the order:
//! it resolves the order, admits or refuses the fill, and writes the order
//! back. `fill_within_taker_risk_limits` governs the taker: it gates the fill
//! on the taker's own risk limits and holds both seats to the post-fill checks.
//! `fill_from_liquidity_sources` governs liquidity: it quotes, splits,
//! executes and settles.
//! `place_perp_order` / `place_spot_order` = user-facing placement with auction parameter derivation.
//! `cancel_order` / `cancel_orders_by_*` = cancellation paths (user-initiated and expiry).

use {
    crate::{
        controller::{
            self,
            funding::settle_funding_payment,
            position::{
                self, add_new_position, decrease_open_bids_and_asks, get_position_index,
                increase_open_bids_and_asks, update_position_and_market, PositionDirection,
            },
            spot_balance::update_spot_balances,
            spot_position::decrease_spot_open_bids_and_asks,
        },
        error::{ErrorCode, VelocityResult},
        get_then_update_id,
        instructions::optional_accounts::AccountMaps,
        load, load_mut,
        math::{
            auction::{calculate_auction_params_for_trigger_order, calculate_auction_prices},
            casting::Cast,
            constants::BASE_PRECISION_U64,
            fees::{self, FillFees},
            liquidation::validate_user_not_being_liquidated,
            margin::*,
            matching::{
                are_orders_same_market_but_different_sides,
                calculate_filler_multiplier_for_matched_orders, is_maker_for_taker,
            },
            oracle::{
                self, is_oracle_valid_for_action, oracle_validity, OracleValidity, VelocityAction,
            },
            orders::*,
            safe_math::SafeMath,
            safe_unwrap::SafeUnwrap,
            time::{legacy_slot_duration_u8_raw, Millis, SlotClock},
        },
        print_error,
        state::{
            events::{
                emit_stack, get_order_action_record, OrderAction, OrderActionExplanation,
                OrderActionRecord, OrderRecord,
            },
            margin_calculation::MarginContext,
            market_status::MarketStatus,
            oracle::OraclePriceData,
            oracle_map::OracleMap,
            order_params::{ModifyOrderParams, OrderParams, PlaceOrderOptions, PostOnlyParam},
            paused_operations::PerpOperation,
            perp_market::PerpMarket,
            perp_market_map::PerpMarketMap,
            quoter::QuoterFill,
            revenue_share::{
                RevenueShareEscrowZeroCopyMut, RevenueShareOrder, RevenueShareOrderBitFlag,
            },
            spot_market::{SpotBalanceType, SpotMarket},
            state::{FeeStructure, *},
            traits::Size,
            user::{
                MarketType, Order, OrderBitFlag, OrderStatus, OrderTriggerCondition, OrderType,
                User, UserStats,
            },
            user_map::UserMap,
        },
        validate,
        validation::{
            self,
            order::{validate_order, validate_order_for_force_reduce_only},
        },
    },
    anchor_lang::prelude::*,
    std::{collections::BTreeMap, ops::DerefMut},
};

mod perp_fill;

pub use perp_fill::{
    fill_perp_order, fill_perp_order_without_external_books, FillParties, FillRequest, FillTarget,
    PerpFillAccounts,
};
pub(crate) use perp_fill::{
    fill_within_taker_risk_limits, FillAmounts, TakerRefs, TakerRiskLimits,
};
#[cfg(test)]
pub(crate) use perp_fill::{FillConditions, FillTerms, OfferedLiquidity};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod router_pass_tests;

/// Outcome of a single [`place_perp_order`] call.
///
/// Batch placement (`place_orders` / `place_scale_orders`) defers the margin
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

/// A perp `Order` built from its params, ready to place or to route detached.
///
/// The output of [`build_perp_order`]: the constructed order plus the facts a
/// caller needs to reserve, margin-check, and record it. It holds no slot and
/// touches no open-order counter, so an ephemeral taker can route it without
/// ever entering `user.orders`.
pub struct BuiltPerpOrder {
    pub order: Order,
    pub position_index: usize,
    pub risk_increasing: bool,
    pub force_reduce_only: bool,
}

/// Build a perp `Order` from its params, minting its id, without persisting it.
///
/// This is the shared core of order construction: market and status checks, the
/// position lookup, size and direction, auction params, the max-time-in-force
/// default, the order value itself, and `validate_order`. It writes no slot,
/// bumps no counter, reserves no `open_bids`/`open_asks`, and runs no margin
/// check — the caller does those, because a slot placement and an ephemeral
/// detached fill need them differently.
///
/// Returns `None` for the two soft-skips that are not errors: an already
/// expired `max_ts`, and a `TryPostOnly` order that would cross. Both clear the
/// builder-order row so it cannot linger.
///
/// The caller must run the placement preconditions first (not-liquidated,
/// not-bankrupt, the reduce-only-user gate), since those guard the whole
/// placement, not the order value.
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
    // borrow ends with this block, because a maximum-size order prices
    // against every market the user holds and needs the whole map set.
    let (force_reduce_only, order_step_size) = {
        let market = maps.perp_market_map.get_ref(&market_index)?;
        let force_reduce_only = market.is_reduce_only()?;

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

        (force_reduce_only, market.order_step_size)
    };

    let position_index = get_position_index(&user.perp_positions, market_index)
        .or_else(|_| add_new_position(&mut user.perp_positions, market_index))?;

    // Increment open orders for existing position
    let (existing_position_direction, order_base_asset_amount) = {
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

        let existing_position_direction = if let Some(existing_position_direction_override) =
            options.existing_position_direction_override
        {
            existing_position_direction_override
        } else {
            let market_position = &user.perp_positions[position_index];
            if market_position.base_asset_amount >= 0 {
                PositionDirection::Long
            } else {
                PositionDirection::Short
            }
        };

        (existing_position_direction, base_asset_amount)
    };

    let market = &maps.perp_market_map.get_ref(&market_index)?;
    let oracle_price_data = maps.oracle_map.get_price_data(&market.oracle_id())?;

    // Downstream auction-param / price / validation logic reads the AMM's
    // cached spread state directly (refreshed by the keeper crank / fill
    // setup), matching pre-decoupling behaviour where order placement quoted
    // off the last-cranked spread rather than recomputing it here.

    // updates auction params for crossing limit orders w/out auction duration
    // dont modify if it's a liquidation
    if !options.is_liquidation() {
        params.update_perp_auction_params(
            market,
            oracle_price_data.price,
            options.is_signed_msg_order(),
        )?;
    }

    let (auction_start_price, auction_end_price, auction_duration) = get_auction_params(
        &params,
        oracle_price_data,
        market.order_tick_size,
        // the stored min is already in the auction's wall clock 400ms units
        legacy_slot_duration_u8_raw(state.min_perp_auction_duration),
    )?;

    let max_ts = match params.max_ts {
        Some(max_ts) => max_ts,
        None => match params.order_type {
            // default TIF: at least 30s, else the auction's wall-clock length
            // plus a quarter again plus 10s of pad, so the default always
            // outlives the auction. The /800 reproduces the historical
            // `auction_duration_slots / 2 + 10` exactly (a 400ms unit is one
            // historical slot, so units/2 == ms/800).
            OrderType::Market | OrderType::Oracle => now.safe_add(
                30_i64.max(
                    Millis::from_stored_units(auction_duration as u64)
                        .as_ms()
                        .safe_div(800)?
                        .cast::<i64>()?
                        .safe_add(10_i64)?,
                ),
            )?,
            _ => 0_i64,
        },
    };

    if max_ts != 0 && max_ts < now {
        msg!("max_ts ({}) < now ({}), skipping order", max_ts, now);
        // The order id is NOT consumed on this path (next_order_id is incremented
        // below), so the next placement reuses it. Clear the builder-order row that
        // `add_builder_order` already wrote for this id, otherwise it would linger
        // and attach to the reusing order (fill-time lookup is keyed by order id).
        clear_placed_builder_order(rev_share_order);
        return Ok(None);
    }

    validate!(
        params.market_type == MarketType::Perp,
        ErrorCode::InvalidOrderMarketType,
        "must be perp order"
    )?;

    // Start with 0 and set bit flags
    let mut bit_flags: u8 = 0;
    bit_flags = set_order_bit_flag(
        bit_flags,
        options.is_signed_msg_order(),
        OrderBitFlag::SignedMessage,
    );

    let reduce_only = params.reduce_only || force_reduce_only;
    bit_flags = set_order_bit_flag(
        bit_flags,
        params.is_trigger_order() && reduce_only,
        OrderBitFlag::NewTriggerReduceOnly,
    );

    if rev_share_order.is_some() {
        bit_flags = set_order_bit_flag(bit_flags, true, OrderBitFlag::HasBuilder);
    }

    if user.perp_positions[position_index].is_isolated() {
        bit_flags = set_order_bit_flag(bit_flags, true, OrderBitFlag::IsIsolatedPosition);
    }

    let new_order = Order {
        status: OrderStatus::Open,
        order_type: params.order_type,
        market_type: params.market_type,
        slot: options.get_order_slot(slot),
        order_id: get_then_update_id!(user, next_order_id),
        user_order_id: params.user_order_id,
        market_index: params.market_index,
        price: get_price_for_perp_order(
            params.price,
            params.direction,
            params.post_only,
            &market.amm,
            market.order_tick_size,
        )?,
        existing_position_direction,
        base_asset_amount: order_base_asset_amount,
        base_asset_amount_filled: 0,
        quote_asset_amount_filled: 0,
        direction: params.direction,
        reduce_only,
        trigger_price: standardize_price(
            params.trigger_price.unwrap_or(0),
            market.order_tick_size,
            params.direction,
        )?,
        trigger_condition: params.trigger_condition,
        post_only: params.post_only != PostOnlyParam::None,
        oracle_price_offset: params.oracle_price_offset.unwrap_or(0),
        immediate_or_cancel: params.is_immediate_or_cancel(),
        auction_start_price,
        auction_end_price,
        auction_duration,
        max_ts,
        posted_slot_tail: get_posted_slot_from_clock_slot(slot),
        bit_flags,
        padding: [0; 5],
    };

    let valid_oracle_price = Some(oracle_price_data.price);
    match validate_order(
        &new_order,
        market,
        valid_oracle_price,
        slot,
        state.slot_clock(),
    ) {
        Ok(()) => {}
        Err(ErrorCode::PlacePostOnlyLimitFailure)
            if params.post_only == PostOnlyParam::TryPostOnly =>
        {
            // just want place to succeeds without error if TryPostOnly.
            // The order id was already consumed above, so it can't be reused; but the
            // builder-order row `add_builder_order` wrote for it would be orphaned
            // (no live order carries it). Clear it so it can't linger in the escrow.
            clear_placed_builder_order(rev_share_order);
            return Ok(None);
        }
        Err(err) => return Err(err),
    };

    let risk_increasing = is_new_order_risk_increasing(
        &new_order,
        user.perp_positions[position_index].base_asset_amount,
        user.perp_positions[position_index].open_bids,
        user.perp_positions[position_index].open_asks,
    )?;

    Ok(Some(BuiltPerpOrder {
        order: new_order,
        position_index,
        risk_increasing,
        force_reduce_only,
    }))
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

    if !options.is_liquidation() {
        validate_user_not_being_liquidated(user, maps, state.liquidation_margin_buffer_ratio)?;
    }

    validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

    if options.try_expire_orders {
        expire_orders(user, &user_key, maps, now, slot)?;
    }

    if user.is_reduce_only() {
        validate!(
            params.reduce_only,
            ErrorCode::UserReduceOnly,
            "order must be reduce only"
        )?;
    }

    let new_order_index = user
        .orders
        .iter()
        .position(|order| order.is_available())
        .ok_or(ErrorCode::MaxNumberOfOrders)?;

    if params.user_order_id > 0 {
        let user_order_id_already_used = user
            .orders
            .iter()
            .position(|order| order.user_order_id == params.user_order_id && !order.is_available());

        if user_order_id_already_used.is_some() {
            msg!("user_order_id is already in use {}", params.user_order_id);
            return Err(ErrorCode::UserOrderIdAlreadyInUse);
        }
    }

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

    user.increment_open_orders(new_order.has_auction());
    user.orders[new_order_index] = new_order;
    user.perp_positions[position_index].open_orders += 1;
    increase_open_bids_and_asks(
        &mut user.perp_positions[position_index],
        &new_order.direction,
        new_order.base_asset_amount,
        new_order.update_open_bids_and_asks(),
    )?;

    options.update_risk_increasing(risk_increasing);

    // if isolated position, the isolated market is the scope that must meet
    // initial margin for a risk-increasing order
    let isolated_market_index = if user.perp_positions[position_index].is_isolated() {
        Some(market_index)
    } else {
        None
    };

    // Single-order placement checks margin here. Bulk placement passes
    // `enforce_margin_check == false` and instead runs one accumulated check
    // after the whole batch (see `place_orders`), so an early risk-increasing
    // order can't be admitted under a weaker check by a later no-op/reducing
    // order.
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
    let max_oi = market.max_open_interest;
    if max_oi != 0 && risk_increasing {
        let oi_plus_order = match new_order.direction {
            PositionDirection::Long => market
                .base_asset_amount_long
                .safe_add(new_order.base_asset_amount.cast()?)?
                .unsigned_abs(),
            PositionDirection::Short => market
                .base_asset_amount_short
                .safe_sub(new_order.base_asset_amount.cast()?)?
                .unsigned_abs(),
        };

        validate!(
            oi_plus_order <= max_oi,
            ErrorCode::MaxOpenInterest,
            "Order Base Amount={} could breach Max Open Interest for Perp Market={}",
            new_order.base_asset_amount,
            market_index
        )?;
    }

    let (taker, taker_order, maker, maker_order) =
        get_taker_and_maker_for_order_record(&user_key, &new_order);

    let order_action_record = get_order_action_record(
        now,
        OrderAction::Place,
        options.explanation,
        market_index,
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
        maps.oracle_map.get_price_data(&market.oracle_id())?.price,
        new_order.bit_flags,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )?;
    emit_stack::<_, { OrderActionRecord::SIZE }>(order_action_record)?;

    let order_record = OrderRecord {
        ts: now,
        user: user_key,
        order: user.orders[new_order_index],
    };
    emit_stack::<_, { OrderRecord::SIZE }>(order_record)?;

    user.update_last_active_slot(slot);

    Ok(PlaceOrderResult {
        risk_increasing,
        isolated_market_index: if risk_increasing {
            isolated_market_index
        } else {
            None
        },
    })
}

/// Whether `user` can carry one more order of this shape, without keeping
/// any of it. The margin engine prices the user *with* the prospective
/// exposure, so the check models the reservation — the aggregates and the
/// per-open-order flat term — and reverses it either way: validation, not a
/// state change. Both the ephemeral create and the remainder rest gate
/// through here, so the two paths cannot drift.
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
/// The straight-to-book path: run the same preconditions, margin gate,
/// open-interest guard, and place records `place_perp_order` runs, and
/// return the order as a value. Nothing is placed: no slot is written and
/// no `open_bids`/`open_asks` reservation is kept. What does change on the
/// user are the facts of the order coming into existence — the id counter,
/// a builder-order row when one applies, and the activity stamp. The
/// caller routes the returned order through
/// `FillTarget::Detached { reserved: false }` and rests only its remainder
/// on the CLOB. Returns `None` on the same soft-skips as `place_perp_order`
/// (expired `max_ts`, `TryPostOnly` that would cross).
///
/// Expired slot orders are the caller's to sweep first (`expire_orders`):
/// this function never touches `user.orders`, and the sweep matters to the
/// gate — an expired order still holds its reservation, and releasing it
/// can be what lets the new order pass. `options.try_expire_orders` is not
/// read here.
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

    let isolated_market_index = if user.perp_positions[position_index].is_isolated() {
        Some(market_index)
    } else {
        None
    };

    // The ephemeral order never carries a reservation into the fill: the
    // fill unwinds nothing for it, and only the rested remainder reserves
    // (`try_place_remainder_on_clob`). The check itself is identical to
    // `place_perp_order`'s.
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
    let max_oi = market.max_open_interest;
    if max_oi != 0 && risk_increasing {
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
            market_index
        )?;
    }

    if options.emit_place_record {
        let (taker, taker_order, maker, maker_order) =
            get_taker_and_maker_for_order_record(&user_key, &order);

        let order_action_record = get_order_action_record(
            now,
            OrderAction::Place,
            options.explanation,
            market_index,
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
            maps.oracle_map.get_price_data(&market.oracle_id())?.price,
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

        let order_record = OrderRecord {
            ts: now,
            user: user_key,
            order,
        };
        emit_stack::<_, { OrderRecord::SIZE }>(order_record)?;
    }

    user.update_last_active_slot(slot);

    Ok(Some(order))
}

fn get_auction_params(
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

pub fn cancel_orders(
    user: &mut User,
    user_key: &Pubkey,
    filler_key: Option<&Pubkey>,
    maps: &mut AccountMaps,
    now: i64,
    slot: u64,
    explanation: OrderActionExplanation,
    market_type: Option<MarketType>,
    market_index: Option<u16>,
    direction: Option<PositionDirection>,
    skip_isolated_positions: bool,
) -> VelocityResult<Vec<u32>> {
    let mut canceled_order_ids: Vec<u32> = vec![];
    let isolated_position_market_indexes = user
        .perp_positions
        .iter()
        .filter(|position| position.is_isolated())
        .map(|position| position.market_index)
        .collect::<Vec<u16>>();
    for order_index in 0..user.orders.len() {
        if user.orders[order_index].status != OrderStatus::Open {
            continue;
        }

        // Placed triggers live on the CLOB; their shadow slots can only be
        // reclaimed through the CLOB removal paths (cancel_order_v1 or the
        // cranks), where the book and the aggregates unwind together.
        if user.orders[order_index].is_placed_on_clob() {
            continue;
        }

        if let (Some(market_type), Some(market_index)) = (market_type, market_index) {
            if user.orders[order_index].market_type != market_type {
                continue;
            }

            if user.orders[order_index].market_index != market_index {
                continue;
            }
        } else if skip_isolated_positions
            && isolated_position_market_indexes.contains(&user.orders[order_index].market_index)
        {
            continue;
        }

        if let Some(direction) = direction {
            if user.orders[order_index].direction != direction {
                continue;
            }
        }

        canceled_order_ids.push(user.orders[order_index].order_id);
        cancel_order(
            order_index,
            user,
            user_key,
            maps,
            now,
            slot,
            explanation,
            filler_key,
            0,
            false,
        )?;
    }

    user.update_last_active_slot(slot);

    Ok(canceled_order_ids)
}

pub fn cancel_order_by_order_id(
    order_id: u32,
    user: &AccountLoader<User>,
    maps: &mut AccountMaps,
    clock: &Clock,
) -> VelocityResult {
    let user_key = user.key();
    let user = &mut load_mut!(user)?;
    let order_index = match user.get_order_index(order_id) {
        Ok(order_index) => order_index,
        Err(_) => {
            msg!("could not find order id {}", order_id);
            return Ok(());
        }
    };

    cancel_order(
        order_index,
        user,
        &user_key,
        maps,
        clock.unix_timestamp,
        clock.slot,
        OrderActionExplanation::None,
        None,
        0,
        false,
    )?;

    user.update_last_active_slot(clock.slot);

    Ok(())
}

pub fn cancel_order_by_user_order_id(
    user_order_id: u8,
    user: &AccountLoader<User>,
    maps: &mut AccountMaps,
    clock: &Clock,
) -> VelocityResult {
    let user_key = user.key();
    let user = &mut load_mut!(user)?;
    let order_index = match user
        .orders
        .iter()
        .position(|order| order.user_order_id == user_order_id)
    {
        Some(order_index) => order_index,
        None => {
            msg!("could not find user order id {}", user_order_id);
            return Ok(());
        }
    };

    cancel_order(
        order_index,
        user,
        &user_key,
        maps,
        clock.unix_timestamp,
        clock.slot,
        OrderActionExplanation::None,
        None,
        0,
        false,
    )?;

    user.update_last_active_slot(clock.slot);

    Ok(())
}

pub fn cancel_order(
    order_index: usize,
    user: &mut User,
    user_key: &Pubkey,
    maps: &mut AccountMaps,
    now: i64,
    _slot: u64,
    explanation: OrderActionExplanation,
    filler_key: Option<&Pubkey>,
    filler_reward: u64,
    skip_log: bool,
) -> VelocityResult {
    let Order {
        status: order_status,
        market_index: order_market_index,
        direction: order_direction,
        market_type: order_market_type,
        ..
    } = user.orders[order_index];

    let is_perp_order = order_market_type == MarketType::Perp;

    validate!(order_status == OrderStatus::Open, ErrorCode::OrderNotOpen)?;

    // A placed trigger's live order rests on the CLOB; the slot here is a
    // shadow whose open-order count the CLOB order carries. Cancelling the
    // shadow would strand the CLOB order and double-unwind its accounting —
    // it must go through `cancel_order_v1` (bulk sweeps skip these slots).
    validate!(
        !user.orders[order_index].is_placed_on_clob(),
        ErrorCode::OrderPlacedOnClob,
        "order {} is placed on the CLOB",
        user.orders[order_index].order_id
    )?;

    let oracle_id = if is_perp_order {
        maps.perp_market_map
            .get_ref(&order_market_index)?
            .oracle_id()
    } else {
        maps.spot_market_map
            .get_ref(&order_market_index)?
            .oracle_id()
    };

    if !skip_log {
        let (taker, taker_order, maker, maker_order) =
            get_taker_and_maker_for_order_record(user_key, &user.orders[order_index]);

        let mut bit_flags = 0;
        if is_perp_order {
            let position_index = get_position_index(&user.perp_positions, order_market_index)?;
            if user.perp_positions[position_index].is_isolated() {
                bit_flags = set_order_bit_flag(bit_flags, true, OrderBitFlag::IsIsolatedPosition);
            }
        }

        let order_action_record = get_order_action_record(
            now,
            OrderAction::Cancel,
            explanation,
            order_market_index,
            filler_key.copied(),
            None,
            Some(filler_reward),
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
            maps.oracle_map.get_price_data(&oracle_id)?.price,
            bit_flags,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )?;
        emit_stack::<_, { OrderActionRecord::SIZE }>(order_action_record)?;
    }

    user.decrement_open_orders(user.orders[order_index].has_auction());
    if is_perp_order {
        // Decrement open orders for existing position
        let position_index = get_position_index(&user.perp_positions, order_market_index)?;

        // only decrease open/bids ask if it's not a trigger order or if it's been triggered
        let update_open_bids_and_asks = user.orders[order_index].update_open_bids_and_asks();
        if update_open_bids_and_asks {
            let base_asset_amount_unfilled =
                user.orders[order_index].get_base_asset_amount_unfilled(None)?;
            position::decrease_open_bids_and_asks(
                &mut user.perp_positions[position_index],
                &order_direction,
                base_asset_amount_unfilled.cast()?,
                update_open_bids_and_asks,
            )?;
        }

        user.perp_positions[position_index].open_orders -= 1;
        user.orders[order_index].status = OrderStatus::Canceled;
    } else {
        let spot_position_index = user.get_spot_position_index(order_market_index)?;

        // only decrease open/bids ask if it's not a trigger order or if it's been triggered
        let update_open_bids_and_asks = user.orders[order_index].update_open_bids_and_asks();
        if update_open_bids_and_asks {
            let base_asset_amount_unfilled =
                user.orders[order_index].get_base_asset_amount_unfilled(None)?;
            decrease_spot_open_bids_and_asks(
                &mut user.spot_positions[spot_position_index],
                &order_direction,
                base_asset_amount_unfilled,
                update_open_bids_and_asks,
            )?;
        }
        user.spot_positions[spot_position_index].open_orders -= 1;
        user.orders[order_index].status = OrderStatus::Canceled;
    }

    Ok(())
}

pub enum ModifyOrderId {
    UserOrderId(u8),
    OrderId(u32),
}

pub fn validate_spot_dlob_trading_enabled_for_market_type(
    market_type: MarketType,
) -> VelocityResult {
    if market_type == MarketType::Spot {
        return Err(ErrorCode::SpotDlobTradingDisabled);
    }

    Ok(())
}

pub fn modify_order(
    order_id: ModifyOrderId,
    modify_order_params: ModifyOrderParams,
    user_loader: &AccountLoader<User>,
    state: &State,
    maps: &mut AccountMaps,
    clock: &Clock,
) -> VelocityResult {
    let user_key = user_loader.key();
    let mut user = load_mut!(user_loader)?;

    let order_index = match order_id {
        ModifyOrderId::UserOrderId(user_order_id) => {
            match user.get_order_index_by_user_order_id(user_order_id) {
                Ok(order_index) => order_index,
                Err(e) => {
                    msg!("User order id {} not found", user_order_id);
                    if modify_order_params.must_modify() {
                        return Err(e);
                    } else {
                        return Ok(());
                    }
                }
            }
        }
        ModifyOrderId::OrderId(order_id) => match user.get_order_index(order_id) {
            Ok(order_index) => order_index,
            Err(e) => {
                msg!("Order id {} not found", order_id);
                if modify_order_params.must_modify() {
                    return Err(e);
                } else {
                    return Ok(());
                }
            }
        },
    };

    let existing_order = user.orders[order_index];

    // A builder-coded order's fee attribution lives in the `RevenueShareEscrow`
    // row keyed to its order_id. modify cancels and re-places under a NEW order
    // id without carrying that row across, silently downgrading the order to
    // no-builder and dropping the builder fee (OtterSec #82). Reject the modify
    // so the attribution can't be stripped; the taker can cancel and re-place
    // with builder params to change a builder-coded order.
    validate!(
        !existing_order.is_has_builder(),
        ErrorCode::CannotModifyBuilderOrder,
        "cannot modify a builder-coded order; cancel and re-place instead"
    )?;

    cancel_order(
        order_index,
        &mut user,
        &user_key,
        maps,
        clock.unix_timestamp,
        clock.slot,
        OrderActionExplanation::None,
        None,
        0,
        false,
    )?;

    user.update_last_active_slot(clock.slot);

    let order_params =
        merge_modify_order_params_with_existing_order(&existing_order, &modify_order_params)?;

    if let Some(order_params) = order_params {
        validate_spot_dlob_trading_enabled_for_market_type(order_params.market_type)?;

        place_perp_order(
            state,
            &mut user,
            user_key,
            maps,
            clock,
            order_params,
            PlaceOrderOptions::default(),
            &mut None,
        )?;
    }

    Ok(())
}

fn merge_modify_order_params_with_existing_order(
    existing_order: &Order,
    modify_order_params: &ModifyOrderParams,
) -> VelocityResult<Option<OrderParams>> {
    let order_type = existing_order.order_type;
    let market_type = existing_order.market_type;
    let direction = modify_order_params
        .direction
        .unwrap_or(existing_order.direction);
    let user_order_id = existing_order.user_order_id;
    let base_asset_amount = match modify_order_params.base_asset_amount {
        Some(base_asset_amount) if modify_order_params.exclude_previous_fill() => {
            let base_asset_amount =
                base_asset_amount.saturating_sub(existing_order.base_asset_amount_filled);

            if base_asset_amount == 0 {
                return Ok(None);
            }

            base_asset_amount
        }
        Some(base_asset_amount) => base_asset_amount,
        None => existing_order.get_base_asset_amount_unfilled(None)?,
    };
    let price = modify_order_params.price.unwrap_or(existing_order.price);
    let market_index = existing_order.market_index;
    let reduce_only = modify_order_params
        .reduce_only
        .unwrap_or(existing_order.reduce_only);
    let post_only = modify_order_params
        .post_only
        .unwrap_or(if existing_order.post_only {
            PostOnlyParam::MustPostOnly
        } else {
            PostOnlyParam::None
        });
    // Preserve the recross gate across a modify. A triggered order that must
    // observe the price recross before re-arming carries
    // `AwaitingTriggerRecross`; rebuilding with `bit_flags = 0` would clear it,
    // letting a user re-arm by modifying without the price ever recrossing.
    let bit_flags = existing_order.bit_flags & (OrderBitFlag::AwaitingTriggerRecross as u8);
    let max_ts = modify_order_params.max_ts.or(Some(existing_order.max_ts));
    let trigger_price = modify_order_params
        .trigger_price
        .or(Some(existing_order.trigger_price));
    let trigger_condition =
        modify_order_params
            .trigger_condition
            .unwrap_or(match existing_order.trigger_condition {
                OrderTriggerCondition::TriggeredAbove | OrderTriggerCondition::Above => {
                    OrderTriggerCondition::Above
                }
                OrderTriggerCondition::TriggeredBelow | OrderTriggerCondition::Below => {
                    OrderTriggerCondition::Below
                }
            });
    let oracle_price_offset = modify_order_params
        .oracle_price_offset
        .or(Some(existing_order.oracle_price_offset));
    let (auction_duration, auction_start_price, auction_end_price) =
        if modify_order_params.auction_duration.is_some()
            && modify_order_params.auction_start_price.is_some()
            && modify_order_params.auction_end_price.is_some()
        {
            (
                modify_order_params.auction_duration,
                modify_order_params.auction_start_price,
                modify_order_params.auction_end_price,
            )
        } else {
            (None, None, None)
        };

    Ok(Some(OrderParams {
        order_type,
        market_type,
        direction,
        user_order_id,
        base_asset_amount,
        price,
        market_index,
        reduce_only,
        post_only,
        bit_flags,
        max_ts,
        trigger_price,
        trigger_condition,
        oracle_price_offset,
        auction_duration,
        auction_start_price,
        auction_end_price,
        builder_idx: None,
        builder_fee_tenth_bps: None,
    }))
}

pub fn validate_market_within_price_band(
    market: &PerpMarket,
    state: &State,
    oracle_price: i64,
) -> VelocityResult<bool> {
    let reserve_price = market.amm.reserve_price()?;

    let reserve_spread_pct = market
        .market_stats
        .historical_oracle_data
        .twap_5min_spread_pct(reserve_price)?;

    let oracle_spread_pct = market
        .market_stats
        .historical_oracle_data
        .twap_5min_spread_pct(oracle_price.unsigned_abs())?;

    if reserve_spread_pct.abs() > oracle_spread_pct.abs() {
        let is_reserve_too_divergent = crate::math::oracle::is_mark_oracle_too_divergent(
            reserve_spread_pct,
            &state.oracle_guard_rails.price_divergence,
        )?;

        // if oracle-mark divergence pushed outside limit, block order
        if is_reserve_too_divergent {
            msg!("Perp market = {} price pushed outside bounds: last_oracle_price_twap_5min={} vs reserve_price={},(breach spread {})",
                market.market_index,
                market
                    .market_stats
                    .historical_oracle_data
                    .last_oracle_price_twap_5min,
                reserve_price,
                reserve_spread_pct,
            );
            return Err(ErrorCode::PriceBandsBreached);
        }
    } else {
        let is_oracle_too_divergent = crate::math::oracle::is_mark_oracle_too_divergent(
            oracle_spread_pct,
            &state.oracle_guard_rails.price_divergence,
        )?;

        // if oracle-mark divergence pushed outside limit, block order
        if is_oracle_too_divergent {
            msg!("Perp market = {} price pushed outside bounds: last_oracle_price_twap_5min={} vs oracle_price={},(breach spread {})",
                market.market_index,
                market
                    .market_stats
                    .historical_oracle_data
                    .last_oracle_price_twap_5min,
                oracle_price,
                oracle_spread_pct,
            );
            return Err(ErrorCode::PriceBandsBreached);
        }
    }

    Ok(true)
}

#[allow(clippy::type_complexity)]
/// One matchable maker order: which loaded maker holds it, where it sits in
/// that maker's orders, and the price it rests at.
///
/// The maker is its position in the loaded set rather than its key. A maker
/// contributes a row per order slot it holds, and a key on every row is
/// thirty-two bytes repeated — on a heap the runtime never reclaims, and a
/// fill against a full book carries dozens of rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MakerOrderInfo {
    pub maker: u16,
    pub order_index: u16,
    pub price: u64,
}

impl MakerOrderInfo {
    /// The key of the maker this row names, from the set the row indexes.
    pub fn key(&self, makers: &UserMap) -> VelocityResult<Pubkey> {
        makers
            .0
            .iter()
            .nth(self.maker as usize)
            .map(|(key, _)| *key)
            .ok_or(ErrorCode::UnableToLoadUserAccount)
    }

    pub fn slot(&self) -> usize {
        self.order_index as usize
    }
}

/// What maker discovery is looking for, and what it may admit.
pub(crate) struct MakerSearch<'a> {
    pub taker_key: &'a Pubkey,
    pub taker_order: &'a Order,
    /// Opposite the taker's, by construction.
    pub maker_direction: PositionDirection,
    /// The keeper that earns the flat reward for each stale order it cleans up.
    pub filler_key: &'a Pubkey,
    pub filler_reward: u64,
    pub oracle_price: i64,
    /// Whether the raw exchange oracle admits a match fill at all.
    pub exchange_match_fills_allowed: bool,
    pub now: i64,
    pub slot: u64,
}

/// One loaded maker, as the fill's maker map holds it.
struct LoadedMaker<'a, 'info> {
    /// The maker's position in the loaded map, which is how a discovered order
    /// names its owner.
    slot: u16,
    key: &'a Pubkey,
    loader: &'a AccountLoader<'info, User>,
}

/// The market facts discovery reads once per maker.
#[derive(Clone, Copy)]
struct MakerMarketFacts {
    initial_margin_ratio: u32,
    step_size: u64,
    /// A `ReduceOnly` market forces resting maker orders risk-reducing too,
    /// regardless of the flag they were placed with. Stamped onto each maker
    /// order so the reduce-only cancel check and the position-capped fill size
    /// both apply.
    reduce_only: bool,
}

/// One of a maker's resting orders, as discovery found it.
struct MakerCandidate<'a> {
    key: &'a Pubkey,
    index: usize,
    /// The sanitized price the order is frozen at for this fill.
    price: u64,
}

/// What discovery decided about one of a maker's resting orders.
enum MakerAdmission {
    /// The order is not matchable, or discovery cancelled it.
    Skip,
    /// The order is matchable. `unfilled` is what it still has to give, which
    /// is what the reducing-set judgement measures a floored maker's orders by.
    Matchable { unfilled: u64 },
}

/// A maker's resting orders on the side this fill needs, as
/// `(order index, sanitized price)`.
type MakerCandidates = Vec<(usize, u64)>;

/// Every DLOB maker order this fill may match, best price first.
///
/// Discovery also cleans up as it walks: a resting order whose price has left
/// the oracle band, or that expired, or that a reduce-only market turned
/// risk-increasing, is cancelled here and the keeper earns the flat reward for
/// it. That cleanup runs whether or not the order was going to be matchable.
fn get_maker_orders_info(
    maps: &mut AccountMaps,
    makers_and_referrer: &UserMap,
    filler: &mut Option<&mut User>,
    search: &MakerSearch,
) -> VelocityResult<Vec<MakerOrderInfo>> {
    // One entry per matchable maker order. Sized so a full book of makers does
    // not grow the buffer part way through: a doubling abandons the old one on
    // an allocator that never reclaims.
    let mut maker_orders_info = Vec::with_capacity(
        makers_and_referrer.0.len() * crate::math::constants::MAX_OPEN_ORDERS as usize,
    );
    for (slot, (key, loader)) in makers_and_referrer.0.iter().enumerate() {
        if key == search.taker_key {
            continue;
        }
        collect_maker_orders(
            LoadedMaker {
                slot: slot as u16,
                key,
                loader,
            },
            &mut maker_orders_info,
            maps,
            filler,
            search,
        )?;
    }
    Ok(maker_orders_info)
}

/// Walk one maker's resting orders, cleaning up what is stale and admitting
/// what is matchable.
fn collect_maker_orders(
    maker: LoadedMaker,
    into: &mut Vec<MakerOrderInfo>,
    maps: &mut AccountMaps,
    filler: &mut Option<&mut User>,
    search: &MakerSearch,
) -> VelocityResult {
    let mut user = load_mut!(maker.loader)?;
    if user.is_being_liquidated() {
        return Ok(());
    }
    let Some((candidates, facts)) = open_maker_orders(&mut user, maker.key, maps, search)? else {
        return Ok(());
    };

    let maker_can_match =
        can_floored_user_match_with_exchange_oracle(&user, search.exchange_match_fills_allowed);
    let floor_unverifiable = maker_can_match && maker_floor_unverifiable(&user, maps)?;

    // Candidates of an unverifiable floored maker that survive the cleanup, as
    // (order index, price, unfilled base). The admit-or-prune decision is made
    // on the whole set afterwards, in `admit_reducing_maker_orders`. Sized to
    // the most a user can hold: growing inside the loop doubles the buffer,
    // and the runtime's allocator never reclaims the one it grew out of.
    let mut deferred: Vec<(usize, u64, u64)> =
        Vec::with_capacity(crate::math::constants::MAX_OPEN_ORDERS as usize);

    for (index, price) in candidates.iter() {
        let candidate = MakerCandidate {
            key: maker.key,
            index: *index,
            price: *price,
        };
        let MakerAdmission::Matchable { unfilled } =
            admit_maker_order(&mut user, &candidate, facts, maps, filler, search)?
        else {
            continue;
        };
        // A selected MM oracle may be fresh enough to quote while the raw
        // exchange oracle the equity floor reads is not valid for margin. The
        // cleanup above stays live, but a floored maker does not execute a
        // DLOB leg its floor check cannot cover.
        if !maker_can_match {
            continue;
        }
        if floor_unverifiable {
            deferred.push((*index, *price, unfilled));
            continue;
        }
        insert_maker_order_info(
            into,
            MakerOrderInfo {
                maker: maker.slot,
                order_index: *index as u16,
                price: *price,
            },
            search.maker_direction,
        );
    }

    if maker_can_match && floor_unverifiable {
        admit_deferred_maker_orders(&user, maker.slot, deferred, into, search)?;
    }
    Ok(())
}

/// Whether this maker's buffered floor cannot be verified for this fill.
///
/// A floored maker with any invalid oracle cannot prove it clears its buffered
/// floor, so the fill-time gate would reject its risk-increasing fills — and
/// by then the maker's leg has executed, so the rejection poisons the taker's
/// whole transaction. Oracle validity cannot change across the fill, so it is
/// resolved here instead: such a maker's risk-increasing orders are pruned,
/// and its provably reducing orders stay matchable because the gate exempts
/// them. Read once per maker, and free when no floor is set.
fn maker_floor_unverifiable(maker: &User, maps: &mut AccountMaps) -> VelocityResult<bool> {
    Ok(match calculate_net_equity_for_floor(maker, maps)? {
        Some(net_equity) => !net_equity.all_oracles_valid,
        None => false,
    })
}

/// The maker's orders that rest on the side this fill needs, and the market
/// facts every one of them is judged against.
///
/// `None` when the maker has nothing resting on that side, which is the
/// common case and the one worth leaving early for.
fn open_maker_orders(
    maker: &mut User,
    maker_key: &Pubkey,
    maps: &mut AccountMaps,
    search: &MakerSearch,
) -> VelocityResult<Option<(MakerCandidates, MakerMarketFacts)>> {
    let mut market = maps
        .perp_market_map
        .get_ref_mut(&search.taker_order.market_index)?;
    let candidates = find_maker_orders(
        maker,
        &search.maker_direction,
        &MarketType::Perp,
        search.taker_order.market_index,
        Some(search.oracle_price),
        search.slot,
        market.order_tick_size,
        maps.oracle_map.slot_clock,
    )?;
    if candidates.is_empty() {
        return Ok(None);
    }
    maker.update_last_active_slot(search.slot);
    settle_funding_payment(maker, maker_key, &mut market, search.now)?;
    let facts = MakerMarketFacts {
        initial_margin_ratio: market.margin_ratio_initial,
        step_size: market.order_step_size,
        reduce_only: market.is_reduce_only()?,
    };
    Ok(Some((candidates, facts)))
}

/// Decide what becomes of one of a maker's resting orders.
fn admit_maker_order(
    maker: &mut User,
    candidate: &MakerCandidate,
    facts: MakerMarketFacts,
    maps: &mut AccountMaps,
    filler: &mut Option<&mut User>,
    search: &MakerSearch,
) -> VelocityResult<MakerAdmission> {
    let order = &maker.orders[candidate.index];
    if !is_maker_for_taker(
        order,
        search.taker_order,
        search.slot,
        maps.oracle_map.slot_clock,
    )? || !are_orders_same_market_but_different_sides(order, search.taker_order)
    {
        return Ok(MakerAdmission::Skip);
    }
    let breaches_oracle_price_limits = limit_price_breaches_maker_oracle_price_bands(
        candidate.price,
        order.direction,
        search.oracle_price,
        facts.initial_margin_ratio,
    )?;
    if facts.reduce_only {
        maker.orders[candidate.index].reduce_only = true;
    }
    let expired = should_expire_order(&maker.orders[candidate.index], search.now)?;
    let existing_base_asset_amount = maker
        .get_perp_position(maker.orders[candidate.index].market_index)?
        .base_asset_amount;
    let increases_position = should_cancel_reduce_only_order(
        &maker.orders[candidate.index],
        existing_base_asset_amount,
        facts.step_size,
    )?;

    if breaches_oracle_price_limits || expired || increases_position {
        let explanation = if breaches_oracle_price_limits {
            OrderActionExplanation::OraclePriceBreachedLimitPrice
        } else if expired {
            OrderActionExplanation::OrderExpired
        } else {
            OrderActionExplanation::ReduceOnlyOrderIncreasedPosition
        };
        cancel_stale_maker_order(maker, candidate, explanation, maps, filler, search)?;
        return Ok(MakerAdmission::Skip);
    }

    Ok(MakerAdmission::Matchable {
        unfilled: maker.orders[candidate.index]
            .get_base_asset_amount_unfilled(Some(existing_base_asset_amount))?,
    })
}

/// Cancel one stale maker order and pay the keeper the flat cleanup reward.
fn cancel_stale_maker_order(
    maker: &mut User,
    candidate: &MakerCandidate,
    explanation: OrderActionExplanation,
    maps: &mut AccountMaps,
    filler: &mut Option<&mut User>,
    search: &MakerSearch,
) -> VelocityResult {
    let filler_reward = {
        let mut market = maps
            .perp_market_map
            .get_ref_mut(&maker.orders[candidate.index].market_index)?;
        pay_keeper_flat_reward_for_perps(
            maker,
            filler.as_deref_mut(),
            market.deref_mut(),
            search.filler_reward,
            search.slot,
        )?
    };
    cancel_order(
        candidate.index,
        maker,
        candidate.key,
        maps,
        search.now,
        search.slot,
        explanation,
        Some(search.filler_key),
        filler_reward,
        false,
    )
}

/// Admit the deferred orders of an unverifiable floored maker that are
/// reducing as a set.
///
/// Admission is deferred so the candidates are judged together: the reducing
/// budget then goes to the best-priced orders instead of the lowest order
/// slots.
fn admit_deferred_maker_orders(
    maker: &User,
    maker_slot: u16,
    deferred: Vec<(usize, u64, u64)>,
    into: &mut Vec<MakerOrderInfo>,
    search: &MakerSearch,
) -> VelocityResult {
    let resting_base_asset_amount = maker
        .get_perp_position(search.taker_order.market_index)
        .map(|position| position.base_asset_amount)
        .unwrap_or(0);
    for (index, price) in
        admit_reducing_maker_orders(deferred, search.maker_direction, resting_base_asset_amount)?
    {
        insert_maker_order_info(
            into,
            MakerOrderInfo {
                maker: maker_slot,
                order_index: index as u16,
                price,
            },
            search.maker_direction,
        );
    }
    Ok(())
}

/// The exchange oracle is the canonical valuation source for the equity floor.
/// An MM oracle may still quote the AMM, but it cannot authorize a floored user
/// to participate in a DLOB match while the exchange oracle is invalid for the
/// match/margin policy.
/// This rule only applies to DLOB matches. Existing AMM gates remain unchanged.
#[inline(always)]
fn can_floored_user_match_with_exchange_oracle(
    user: &User,
    exchange_match_fills_allowed: bool,
) -> bool {
    user.equity_floor == 0 || exchange_match_fills_allowed
}

/// The subset of an unverifiable floored maker's candidate orders
/// `(order index, price, unfilled base)` that is reducing as a set against
/// the maker's resting position, judged best price for the taker first
/// (ascending for maker sells, descending for maker buys). Reducing is a
/// property of the admitted set, not of one order: a maker long 1 with two
/// resting sells of 0.75 has each order reducing against the resting
/// position while the pair flips it short, so each candidate is judged
/// against the position the previously admitted orders would leave behind.
/// Judging best price first spends that budget on the orders the taker
/// wants matched. Every admitted order's fill is exempt at the fill-time
/// floor gate (`is_order_position_reducing` is the shared predicate), so a
/// pruned maker can never revert the taker's transaction.
fn admit_reducing_maker_orders(
    mut candidates: Vec<(usize, u64, u64)>,
    maker_direction: PositionDirection,
    resting_base_asset_amount: i64,
) -> VelocityResult<Vec<(usize, u64)>> {
    match maker_direction {
        PositionDirection::Long => candidates.sort_by_key(|c| std::cmp::Reverse(c.1)),
        PositionDirection::Short => candidates.sort_by_key(|a| a.1),
    }

    let mut projected_base_asset_amount = resting_base_asset_amount;
    let mut admitted = Vec::with_capacity(candidates.len());

    for (order_index, order_price, unfilled) in candidates {
        if !is_order_position_reducing(&maker_direction, unfilled, projected_base_asset_amount)? {
            continue;
        }

        // admitted, so the next candidate is judged against what this one
        // would leave behind
        let signed = match maker_direction {
            PositionDirection::Long => unfilled.cast::<i64>()?,
            PositionDirection::Short => -unfilled.cast::<i64>()?,
        };
        projected_base_asset_amount = projected_base_asset_amount.safe_add(signed)?;

        admitted.push((order_index, order_price));
    }

    Ok(admitted)
}

#[inline(always)]
fn insert_maker_order_info(
    maker_orders_info: &mut Vec<MakerOrderInfo>,
    maker_order_info: MakerOrderInfo,
    direction: PositionDirection,
) {
    let price = maker_order_info.price;
    let index = match maker_orders_info.binary_search_by(|item| match direction {
        PositionDirection::Short => item.price.cmp(&price),
        PositionDirection::Long => price.cmp(&item.price),
    }) {
        Ok(index) => index,
        Err(index) => index,
    };

    if index < maker_orders_info.capacity() {
        maker_orders_info.insert(index, maker_order_info);
    }
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

#[inline(always)]
fn get_builder_escrow_info(
    escrow_opt: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
    sub_account_id: u16,
    order_id: u32,
    market_index: u16,
    order_has_builder: bool,
    builder_fee_allowed: bool,
) -> (Option<u32>, Option<u32>, Option<u16>, Option<u8>) {
    if let Some(escrow) = escrow_opt {
        // Only match a builder-order row for an order that actually carries the
        // `HasBuilder` flag, and bind the row to the market being filled. Escrow rows
        // are keyed on chain by `(sub_account_id, order_id)`, and order ids are reused
        // both within a market (a placement soft-skips after `add_builder_order` wrote
        // the row — e.g. an expired `max_ts`, which returns before `next_order_id` is
        // consumed) and across markets (ids are per-subaccount). Without the
        // `HasBuilder` gate a stale row would attach to a later non-builder order that
        // reuses the id (OtterSec #49); without the market binding a market-A row would
        // attach to a same-id market-B fill and be paid from market A's pnl pool
        // (OtterSec #88). `find_builder_order_index` enforces both. The referral lookup
        // is keyed by market, not order id, so it is unaffected and stays unconditional.
        let builder_order_idx = if order_has_builder {
            escrow.find_builder_order_index(
                sub_account_id,
                order_id,
                market_index,
                MarketType::Perp,
            )
        } else {
            None
        };
        let referrer_builder_order_idx = escrow.find_or_create_referral_index(market_index);

        let builder_order = builder_order_idx.and_then(|idx| escrow.get_order(idx).ok());
        // `builder_fee_allowed` is false when the taker does not meet initial
        // margin. The row stays bound so the fill still reports its builder in
        // the `OrderActionRecord` and `revoke_completed_orders` still closes
        // the row, but the fee for this fill is zero. See the gate in
        // `fulfill_perp_order` for why (OtterSec #83).
        let builder_order_fee_bps = if builder_fee_allowed {
            builder_order.map(|order| order.fee_tenth_bps)
        } else {
            None
        };
        let builder_idx = builder_order.map(|order| order.builder_idx);

        (
            builder_order_idx,
            referrer_builder_order_idx,
            builder_order_fee_bps,
            builder_idx,
        )
    } else {
        (None, None, None, None)
    }
}

#[inline(always)]
pub(crate) fn update_maker_fills_map(
    map: &mut BTreeMap<Pubkey, (i64, bool)>,
    maker_key: &Pubkey,
    maker_direction: PositionDirection,
    fill: u64,
    is_isolated_position: bool,
) -> VelocityResult {
    let signed_fill = match maker_direction {
        PositionDirection::Long => fill.cast::<i64>()?,
        PositionDirection::Short => -fill.cast::<i64>()?,
    };

    if let Some(maker_filled) = map.get_mut(maker_key) {
        *maker_filled = (maker_filled.0.safe_add(signed_fill)?, is_isolated_position);
    } else {
        map.insert(*maker_key, (signed_fill, is_isolated_position));
    }

    Ok(())
}

pub(crate) fn determine_if_user_order_is_position_decreasing(
    user: &User,
    market_index: u16,
    order: &Order,
) -> VelocityResult<bool> {
    // A fresh ephemeral taker has no position yet: opening one is not
    // decreasing, so a missing position reads as base zero.
    let position_base_asset_amount_before = get_position_index(&user.perp_positions, market_index)
        .map(|position_index| user.perp_positions[position_index].base_asset_amount)
        .unwrap_or(0);
    is_order_position_reducing(
        &order.direction,
        order.get_base_asset_amount_unfilled(Some(position_base_asset_amount_before))?,
        position_base_asset_amount_before.cast()?,
    )
}

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

/// Build and emit an `OrderActionRecord`.
///
/// The record is 480 bytes, and the two `Option<Order>` copies are large
/// again. This function is separate so that all three live in its own frame.
/// A settlement path that built them would hold them in a frame that is
/// already large, and the SBPF stack-overwrite check then fires.
///
/// The long parameter list is what makes that work. Do not group these
/// parameters into a struct. The caller builds a struct in its own frame,
/// which is the cost this function exists to avoid. The check runs only on
/// an SBF target, so a host test reports nothing when it regresses.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn emit_perp_action_record(
    market: &mut PerpMarket,
    oracle_map: &mut OracleMap,
    now: i64,
    action_explanation: OrderActionExplanation,
    filler_key: &Pubkey,
    filler_reward: u64,
    base_filled: u64,
    quote_filled: u64,
    taker_fee_plus_builder: u64,
    maker_rebate: Option<u64>,
    referrer_reward: u64,
    quote_asset_amount_surplus: Option<i64>,
    taker_record_key: Option<Pubkey>,
    taker_record_order: Option<Order>,
    maker_record_key: Option<Pubkey>,
    maker_record_order: Option<Order>,
    order_action_bit_flags: u8,
    taker_existing_quote_entry_amount: Option<u64>,
    taker_existing_base_asset_amount: Option<u64>,
    maker_existing_quote_entry_amount: Option<u64>,
    maker_existing_base_asset_amount: Option<u64>,
    builder_idx: Option<u8>,
    builder_fee_option: Option<u64>,
) -> VelocityResult {
    let fill_record_id = get_then_update_id!(market, next_fill_record_id);
    let oracle_price = oracle_map.get_price_data(&market.oracle_id())?.price;
    let mut record = get_order_action_record(
        now,
        OrderAction::Fill,
        action_explanation,
        market.market_index,
        Some(*filler_key),
        Some(fill_record_id),
        Some(filler_reward),
        Some(base_filled),
        Some(quote_filled),
        Some(taker_fee_plus_builder),
        maker_rebate,
        Some(referrer_reward),
        quote_asset_amount_surplus,
        None,
        taker_record_key,
        taker_record_order,
        maker_record_key,
        maker_record_order,
        oracle_price,
        order_action_bit_flags,
        taker_existing_quote_entry_amount,
        taker_existing_base_asset_amount,
        maker_existing_quote_entry_amount,
        maker_existing_base_asset_amount,
        None,
        builder_idx,
        builder_fee_option,
    )?;
    // A maker whose order rests on a book has no `Order` here to snapshot.
    // What the fill knows of it is its id and its side, which is what a
    // reader needs to attribute the fill; the order's size and its running
    // totals live in the reader's own table, built from the place record.
    // Reporting this fill's size as the order's size would be wrong, so the
    // fields say nothing instead.
    if maker_record_order.is_some_and(|order| order.is_placed_on_clob()) {
        record.maker_order_base_asset_amount = None;
        record.maker_order_cumulative_base_asset_amount_filled = None;
        record.maker_order_cumulative_quote_asset_amount_filled = None;
    }
    emit_stack::<_, { OrderActionRecord::SIZE }>(record)
}

/// Quote and AMM surplus for a normal (non-post_only) sole-AMM fill.
///
/// Returns `(taker_quote, taker_surplus)`. `taker_quote` is what the taker
/// pays or receives. `taker_surplus` is the AMM's spread profit booked for
/// the LPs, signed so a positive value grows the AMM's books.
///
/// Two adjustments to the live-curve quote, both booked through the surplus:
///
///  * Capture the shade. The router quoted this slice at the shaded
///    allocation quote, which is taker-worse than the live curve when a rival
///    rung undercut the curve. Charge the taker that quote and book the gap
///    for the LPs, so the shade does not leak to the taker as price
///    improvement.
///  * Hold the taker to its limit. The ladder's `top` understates the swap's
///    first marginal, so a limit just above `top` can still sit below the
///    curve. Cap the taker's quote at its limit and book the improvement
///    against the surplus, so the taker is never charged worse than its
///    limit.
fn settle_amm_house_normal_quote(
    fill: &QuoterFill,
    taker_direction: PositionDirection,
    taker_limit_price: Option<u64>,
    amm_allocation_quote: u64,
    amm_allocation_base: u64,
) -> VelocityResult<(u64, i64)> {
    let curve_quote = fill.quote_filled;
    let mut taker_quote = curve_quote;

    // Shade: charge the shaded allocation quote when it is taker-worse than
    // the curve. Scale it to the base actually filled, taker-worse, so a
    // partial fill is not overcharged the whole allocation.
    if amm_allocation_base > 0 && fill.base_filled > 0 {
        let shade_quote = if fill.base_filled >= amm_allocation_base {
            amm_allocation_quote
        } else {
            let scaled = (amm_allocation_quote as u128).safe_mul(fill.base_filled as u128)?;
            match taker_direction {
                PositionDirection::Long => scaled.safe_div_ceil(amm_allocation_base as u128)?,
                PositionDirection::Short => scaled.safe_div(amm_allocation_base as u128)?,
            }
            .cast::<u64>()?
        };
        taker_quote = match taker_direction {
            PositionDirection::Long => taker_quote.max(shade_quote),
            PositionDirection::Short => taker_quote.min(shade_quote),
        };
    }

    // Limit cap: never charge worse than the taker's own limit.
    if let Some(limit) = taker_limit_price {
        let limit_quote = crate::math::orders::calculate_quote_asset_amount_for_maker_order(
            fill.base_filled,
            limit,
            crate::math::constants::PERP_DECIMALS,
            taker_direction,
        )?;
        taker_quote = match taker_direction {
            PositionDirection::Long => taker_quote.min(limit_quote),
            PositionDirection::Short => taker_quote.max(limit_quote),
        };
    }

    // Book the change against the AMM's spread surplus. Positive when the
    // shade earned more than the curve. Negative when the limit cap gave the
    // taker improvement.
    let delta = match taker_direction {
        PositionDirection::Long => taker_quote.cast::<i64>()?.safe_sub(curve_quote.cast()?)?,
        PositionDirection::Short => curve_quote.cast::<i64>()?.safe_sub(taker_quote.cast()?)?,
    };
    let taker_surplus = fill.quote_asset_amount_surplus.safe_add(delta)?;
    Ok((taker_quote, taker_surplus))
}

/// The taker side of one fill: who fills, the order they fill through, and
/// the position it lands in.
pub(crate) struct TakerSide<'a> {
    pub user: &'a mut User,
    pub stats: &'a mut UserStats,
    pub key: Pubkey,
    pub position_index: usize,
    pub order: &'a mut Order,
    pub direction: PositionDirection,
    /// Base and quote of the position before this fill, when the caller
    /// already read them.
    pub existing_position_params_before: Option<(u64, u64)>,
    /// Whether the taker owns an `open_bids`/`open_asks` + `open_orders`
    /// reservation the fill must unwind. False for a fresh ephemeral taker
    /// that never reserved.
    pub reserved: bool,
}

impl<'a> TakerSide<'a> {
    /// Bind the taker to the position this fill settles into.
    ///
    /// An ephemeral taker holds only the empty position `build_perp_order`
    /// added, which `get_position_index` skips as available.
    /// `add_new_position` reuses that same slot, so the fill settles into it.
    /// A slot order always has a findable position from its placement, so the
    /// fallback never fires for one.
    pub(crate) fn bind(
        user: &'a mut User,
        stats: &'a mut UserStats,
        key: Pubkey,
        order: &'a mut Order,
        reserved: bool,
    ) -> VelocityResult<Self> {
        let direction = order.direction;
        let market_index = order.market_index;
        let position_index = get_position_index(&user.perp_positions, market_index)
            .or_else(|_| add_new_position(&mut user.perp_positions, market_index))?;
        let existing_position_params_before = user.perp_positions[position_index]
            .get_existing_position_params_for_order_action(direction);
        Ok(Self {
            user,
            stats,
            key,
            position_index,
            order,
            direction,
            existing_position_params_before,
            reserved,
        })
    }

    /// The same taker seat, borrowed for a shorter life.
    ///
    /// A layer that holds the seat and hands it to a step below keeps its own
    /// access to the taker afterwards.
    pub(crate) fn reborrow(&mut self) -> TakerSide<'_> {
        TakerSide {
            user: self.user,
            stats: self.stats,
            key: self.key,
            position_index: self.position_index,
            order: self.order,
            direction: self.direction,
            existing_position_params_before: self.existing_position_params_before,
            reserved: self.reserved,
        }
    }

    /// How much of the order is still to fill, capped by the position it
    /// settles into.
    pub(crate) fn unfilled_target(&self) -> VelocityResult<u64> {
        self.order.get_base_asset_amount_unfilled(Some(
            self.user.perp_positions[self.position_index].base_asset_amount,
        ))
    }
}

/// The maker side of one external fill: whose liquidity filled it, and where
/// the fill lands in their account.
///
/// `stats` is absent when the maker and the taker are the same account. The
/// maker volume is then recorded on the taker's stats instead.
pub(crate) struct MakerSide<'a, 'stats> {
    pub user: &'a mut User,
    pub stats: Option<&'stats mut UserStats>,
    pub key: Pubkey,
    /// Opposite the taker's, by construction.
    pub direction: PositionDirection,
    pub position_index: usize,
    /// Base and quote of the position before this fill, when the position
    /// already had one.
    pub existing_position_params: Option<(u64, u64)>,
    /// Whether the maker owns an open-order reservation the fill must
    /// release. The quoter reports it: a quoter that keeps its makers'
    /// aggregates holds the reservation velocity took at placement. This is
    /// [`TakerSide::reserved`] for the other side of the same fill.
    pub reserved: bool,
    /// The maker's own id for the order this fill came off, when the response
    /// named exactly one. It is what lets the fill record attribute to a book
    /// order. `None` when the change merged several and no single order owns
    /// it.
    pub order_id: Option<u32>,
}

impl<'a, 'stats> MakerSide<'a, 'stats> {
    /// Bind the maker to the position this fill lands in. A maker that holds
    /// no position in this market gets one.
    ///
    /// A fresh position is cross-margined, so this fallback would settle a
    /// book order into the wrong collateral pool if it ever fired for one. It
    /// cannot: a resting CLOB order holds `open_orders` and `open_bids` or
    /// `open_asks` on its owner's position, so that slot is never available
    /// and never recycled, and the order keeps one margin regime for its whole
    /// life. The fallback is for a maker the fill reaches with no book order
    /// behind it.
    #[allow(clippy::too_many_arguments)]
    pub fn bind(
        user: &'a mut User,
        stats: Option<&'stats mut UserStats>,
        key: Pubkey,
        taker_direction: PositionDirection,
        market_index: u16,
        reserved: bool,
        order_id: Option<u32>,
    ) -> VelocityResult<Self> {
        let direction = taker_direction.opposite();
        let position_index = get_position_index(&user.perp_positions, market_index)
            .or_else(|_| add_new_position(&mut user.perp_positions, market_index))?;
        let existing_position_params = user.perp_positions[position_index]
            .get_existing_position_params_for_order_action(direction);
        Ok(Self {
            user,
            stats,
            key,
            direction,
            position_index,
            existing_position_params,
            reserved,
            order_id,
        })
    }
}

/// Who takes the filler reward, and where a builder fee is escrowed. A path
/// that pays no filler holds `None` in each option.
///
/// Each option comes from a different owner, so each inner reference keeps
/// its own lifetime. A `&mut` to a `&mut` is invariant, so one shared
/// lifetime would force three unrelated borrows to be equal.
pub(crate) struct FillerSide<'a, 'user, 'stats, 'escrow, 'info> {
    pub user: &'a mut Option<&'user mut User>,
    pub stats: &'a mut Option<&'stats mut UserStats>,
    pub key: Pubkey,
    pub rev_share_escrow: &'a mut Option<&'escrow mut RevenueShareEscrowZeroCopyMut<'info>>,
}

/// The rules a fill prices and charges under. Fixed for a whole instruction.
pub(crate) struct FillPolicy<'a> {
    pub fee_structure: &'a FeeStructure,
    /// The oracle tolerances the quote snapshot is read under.
    pub validity_guard_rails: &'a ValidityGuardRails,
    pub referrer_is_accelerated: bool,
    pub is_liquidation: bool,
    pub promo_fee_tier: u8,
    /// Whether a vAMM fill pays the maker rebate.
    pub vamm_maker_rebate: bool,
    /// False when the taker does not meet initial margin. The fill proceeds and
    /// charges no builder fee. [`FillTerms::policy`] takes the decision and
    /// [`builder_fee_allowed`] states the rule.
    pub builder_fee_allowed: bool,
}

impl<'a> FillPolicy<'a> {
    /// The rules for a path that only settles an already-matched pair. It
    /// prices nothing, so it routes no external book, charges no builder fee
    /// and pays no referral acceleration.
    pub(crate) fn for_settlement(state: &'a State) -> Self {
        Self {
            fee_structure: &state.perp_fee_structure,
            validity_guard_rails: &state.oracle_guard_rails.validity,
            referrer_is_accelerated: false,
            is_liquidation: false,
            promo_fee_tier: state.promo_fee_tier,
            vamm_maker_rebate: false,
            builder_fee_allowed: false,
        }
    }
}

/// The builder order a fill accrues revenue share against, as the taker's
/// escrow names it.
#[derive(Clone, Copy, Default)]
struct BuilderEscrow {
    /// The builder's order in the escrow, when the taker's order carries one.
    order_index: Option<u32>,
    /// The referrer's order in the escrow, when the taker is referred.
    referrer_order_index: Option<u32>,
    /// The builder's rate, in tenths of a basis point.
    fee_tenth_bps: Option<u16>,
    /// The builder, as the escrow indexes it. The fill record carries it.
    builder_index: Option<u8>,
}

impl BuilderEscrow {
    /// Read the taker's escrow for the orders this fill accrues against.
    fn read(
        filler: &mut FillerSide,
        taker: &TakerSide,
        market_index: u16,
        order_id: u32,
        builder_fee_allowed: bool,
    ) -> Self {
        let (order_index, referrer_order_index, fee_tenth_bps, builder_index) =
            get_builder_escrow_info(
                filler.rev_share_escrow,
                taker.user.sub_account_id,
                order_id,
                market_index,
                taker.order.is_has_builder(),
                builder_fee_allowed,
            );
        Self {
            order_index,
            referrer_order_index,
            fee_tenth_bps,
            builder_index,
        }
    }
}

/// What every settle leg carries beyond the two seats: the market they settle
/// into, the rules the leg prices under, the oracle map the fill record reads,
/// the moment, and the per-fill filler allowance the legs draw down.
pub(crate) struct SettleContext<'a, 'o> {
    /// The market both seats settle into.
    pub market: &'a mut PerpMarket,
    pub policy: &'a FillPolicy<'a>,
    pub oracle_map: &'a mut OracleMap<'o>,
    pub now: i64,
    pub slot: u64,
    /// Filler reward already paid by earlier legs of this same fill. The
    /// time-based component of the reward is size-independent, so it is a
    /// per-fill allowance the legs draw down rather than one each.
    pub filler_reward_paid: &'a mut u64,
}

/// The fee split one settled allocation produced, and what it accrues against.
///
/// Each leg prices its own schedule against its own counterparty. From here
/// the three walk one spine: accrue the market's share, charge the taker, pay
/// the keeper, accrue the revenue share, advance the taker's order and unwind
/// what it reserved.
struct SettledFees {
    fees: FillFees,
    escrow: BuilderEscrow,
    /// The builder's share, flattened. Zero when no builder is owed one.
    builder_fee: u64,
}

impl SettledFees {
    /// Keep the split, and draw this leg's share off the per-fill filler
    /// allowance.
    fn take(fees: FillFees, escrow: BuilderEscrow, cx: &mut SettleContext) -> Self {
        *cx.filler_reward_paid = cx.filler_reward_paid.saturating_add(fees.filler_reward);
        let builder_fee = fees.builder_fee.unwrap_or(0);
        Self {
            fees,
            escrow,
            builder_fee,
        }
    }

    /// What the taker pays for this leg: its own fee and the builder's.
    fn taker_debit(&self) -> VelocityResult<u64> {
        self.fees.user_fee.safe_add(self.builder_fee)
    }

    /// Accrue the builder's share against the builder's order.
    ///
    /// A builder fee with no escrow to accrue into is a fee the taker approved
    /// and the builder could never claim, so it fails the fill rather than
    /// resolving to zero.
    fn accrue_builder_fee(
        &self,
        filler: &mut FillerSide,
        cx: &mut SettleContext,
    ) -> VelocityResult {
        if self.builder_fee == 0 {
            return Ok(());
        }
        match (
            self.escrow.order_index,
            filler.rev_share_escrow.as_deref_mut(),
        ) {
            (Some(index), Some(escrow)) => {
                accrue_revenue_share(escrow, index, self.builder_fee, cx.market)
            }
            _ => {
                validate!(
                    false,
                    ErrorCode::UnableToLoadRevenueShareAccount,
                    "Order has builder fee but no escrow account found"
                )?;
                Ok(())
            }
        }
    }

    /// Accrue the referrer's reward against the referrer's order.
    fn accrue_referrer_reward(
        &self,
        filler: &mut FillerSide,
        cx: &mut SettleContext,
    ) -> VelocityResult {
        match (
            self.escrow.referrer_order_index,
            filler.rev_share_escrow.as_deref_mut(),
        ) {
            (Some(index), Some(escrow)) => {
                accrue_revenue_share(escrow, index, self.fees.referrer_reward, cx.market)
            }
            _ => Ok(()),
        }
    }

    /// Mark the builder's order complete once the taker's order is.
    fn mark_builder_order_complete(&self, filler: &mut FillerSide) {
        if let (Some(index), Some(escrow)) = (
            self.escrow.order_index,
            filler.rev_share_escrow.as_deref_mut(),
        ) {
            let _ = escrow
                .get_order_mut(index)
                .map(|order| order.add_bit_flag(RevenueShareOrderBitFlag::Completed));
        }
    }
}

/// Book the market's share of one settled allocation.
///
/// The AMM books ONLY its own money: its fee provision plus any spread surplus
/// (`fee_to_market = amm_fee + surplus`). The protocol and insurance-fund
/// carveouts never touch the AMM's ledger or pools. They accrue as pending
/// quote counters here, because the quote spot market is not in scope at fill;
/// their token value lands in the pnl pool as fills settle and
/// `sweep_market_fees` materializes it. The AMM provision also grows the
/// lifetime backstop-of-last-resort clawback cap.
///
/// `amm_surplus` is `Some` only on the house leg, which is the one leg that
/// can capture spread. A counterparty leg books the AMM's provision only when
/// the schedule produced one.
fn accrue_market_fees(
    cx: &mut SettleContext,
    fees: &FillFees,
    amm_surplus: Option<i64>,
) -> VelocityResult {
    match amm_surplus {
        Some(surplus) => {
            <crate::vlp::amm::AMM as crate::vlp::amm::quoter::AmmContract>::apply_fill_fees(
                &mut cx.market.amm,
                fees.fee_to_market,
                surplus,
            )?;
        }
        None if fees.amm_fee > 0 => {
            <crate::vlp::amm::AMM as crate::vlp::amm::quoter::AmmContract>::apply_fill_fees(
                &mut cx.market.amm,
                fees.fee_to_market,
                0,
            )?;
        }
        None => {}
    }
    cx.market.fee_ledger.accrue_fill_fees(
        fees.user_fee,
        fees.protocol_fee,
        fees.if_fee,
        fees.amm_fee,
    )?;
    Ok(())
}

/// Charge the taker what this leg costs it: its own fee and the builder's.
fn charge_taker(
    taker: &mut TakerSide,
    settled: &SettledFees,
    cx: &mut SettleContext,
) -> VelocityResult {
    controller::position::update_quote_asset_and_break_even_amount(
        &mut taker.user.perp_positions[taker.position_index],
        cx.market,
        -settled.taker_debit()?.cast::<i64>()?,
    )?;
    taker.stats.increment_total_fees(settled.fees.user_fee)?;
    taker
        .stats
        .increment_total_referee_discount(settled.fees.referee_discount)
}

/// Pay the maker its rebate, on the seat that earned it.
///
/// A maker that is another subaccount of the taker's authority has no stats of
/// its own loaded, so its rebate is recorded on the taker's stats instead.
fn credit_maker_rebate(
    maker: &mut MakerSide,
    taker: &mut TakerSide,
    rebate: u64,
    cx: &mut SettleContext,
) -> VelocityResult {
    controller::position::update_quote_asset_and_break_even_amount(
        &mut maker.user.perp_positions[maker.position_index],
        cx.market,
        rebate.cast()?,
    )?;
    match maker.stats.as_mut() {
        Some(stats) => stats.increment_total_rebate(rebate),
        None => taker.stats.increment_total_rebate(rebate),
    }
}

/// Move the maker's position by what this leg filled, and record its volume.
///
/// A maker that is another subaccount of the taker's authority has no stats of
/// its own loaded, so its volume is recorded on the taker's stats instead.
fn move_maker_position(
    maker: &mut MakerSide,
    taker: &mut TakerSide,
    filled: FillAmounts,
    cx: &mut SettleContext,
) -> VelocityResult {
    let delta = get_position_delta_for_fill(filled.base, filled.quote, maker.direction)?;
    update_position_and_market(
        &mut maker.user.perp_positions[maker.position_index],
        cx.market,
        &delta,
    )?;
    match maker.stats.as_mut() {
        Some(stats) => stats.update_maker_volume_30d(filled.quote, cx.now),
        None => taker.stats.update_maker_volume_30d(filled.quote, cx.now),
    }
}

/// Move the taker's position by what this leg filled.
///
/// The volume it counts as is the leg's own business: a post-only taker fills
/// as the maker, so the house leg records maker volume instead.
fn move_taker_position(
    taker: &mut TakerSide,
    filled: FillAmounts,
    cx: &mut SettleContext,
) -> VelocityResult {
    let delta = get_position_delta_for_fill(filled.base, filled.quote, taker.direction)?;
    update_position_and_market(
        &mut taker.user.perp_positions[taker.position_index],
        cx.market,
        &delta,
    )?;
    Ok(())
}

/// Pay the keeper that turned this fill, out of the reward the schedule
/// carved.
///
/// A keeper with no reward still has its last-active slot stamped, so its
/// transaction does not revert for idleness.
fn pay_fill_keeper(
    filler: &mut FillerSide,
    settled: &SettledFees,
    quote_filled: u64,
    cx: &mut SettleContext,
) -> VelocityResult {
    let Some(filler_user) = filler.user.as_mut() else {
        return Ok(());
    };
    if settled.fees.filler_reward > 0 {
        let market_index = cx.market.market_index;
        let position_index = get_position_index(&filler_user.perp_positions, market_index)
            .or_else(|_| add_new_position(&mut filler_user.perp_positions, market_index))?;
        controller::position::update_quote_asset_amount(
            &mut filler_user.perp_positions[position_index],
            cx.market,
            settled.fees.filler_reward.cast()?,
        )?;
        filler
            .stats
            .as_mut()
            .safe_unwrap()?
            .update_filler_volume(quote_filled, cx.now)?;
    }
    filler_user.update_last_active_slot(cx.slot);
    Ok(())
}

/// Advance the taker's order by what this leg filled, and unwind the
/// reservation it held for that size.
///
/// Only a reservation the taker actually took is unwound. A fresh ephemeral
/// taker never reserved, and unwinding here would eat a co-resident order's
/// `open_bids`/`open_asks`.
fn advance_taker_order(
    taker: &mut TakerSide,
    filler: &mut FillerSide,
    settled: &SettledFees,
    filled: FillAmounts,
) -> VelocityResult {
    // Update the taker order BEFORE the event emit.
    if update_order_after_fill(taker.order, filled.base, filled.quote)? {
        settled.mark_builder_order_complete(filler);
    }
    if taker.reserved {
        decrease_open_bids_and_asks(
            &mut taker.user.perp_positions[taker.position_index],
            &taker.direction,
            filled.base,
            taker.order.update_open_bids_and_asks(),
        )?;
    }
    Ok(())
}

/// The vAMM slice one house leg settles, as the router priced it.
struct AmmAllocation {
    /// The shaded quote the router priced `base` at. The shade is taker-worse
    /// than the live curve, so charging this quote and not the curve keeps the
    /// shade for the LPs.
    quote: u64,
    base: u64,
    /// The taker's order as the AMM fee schedule reads it.
    post_only: bool,
    order_slot: u64,
    order_id: u32,
    taker_limit: Option<u64>,
}

/// The vAMM's seat in a house fill.
///
/// The house holds no position, so the only account here is a maker that
/// cranked the fill and therefore earns the keeper reward.
struct HouseSide<'a, 'user, 'stats> {
    cranking_maker: &'a mut Option<&'user mut User>,
    cranking_maker_stats: &'a mut Option<&'stats mut UserStats>,
    /// Whether the house pays a maker rebate for making the fill.
    pays_maker_rebate: bool,
}

/// What the taker pays for a vAMM slice, and what the house keeps as spread.
///
/// A post-only sole-AMM step makes the taker the maker: it transacts at its
/// own limit, and the house keeps the curve-to-limit gap as spread surplus.
/// Every other step charges the router's shade and holds the taker to its
/// limit.
fn amm_house_taker_quote(
    fill: &QuoterFill,
    taker: &TakerSide,
    allocation: &AmmAllocation,
) -> VelocityResult<(u64, i64)> {
    match (allocation.post_only, allocation.taker_limit) {
        (true, Some(limit)) => crate::controller::position::calculate_quote_asset_amount_surplus(
            taker.direction,
            fill.quote_filled,
            fill.base_filled,
            limit,
        ),
        _ => settle_amm_house_normal_quote(
            fill,
            taker.direction,
            allocation.taker_limit,
            allocation.quote,
            allocation.base,
        ),
    }
}

/// Settle one vAMM slice against the house.
///
/// The taker is the only account holding a position, so this leg moves no
/// maker and unwinds nothing of a counterparty's. What it has instead is the
/// spread surplus, which only the house can capture, and a cranking maker to
/// pay when no separate keeper turned the fill.
fn settle_amm_house_fill(
    fill: &QuoterFill,
    taker: &mut TakerSide,
    house: &mut HouseSide,
    allocation: &AmmAllocation,
    filler: &mut FillerSide,
    cx: &mut SettleContext,
) -> VelocityResult<(u64, u64)> {
    let (taker_quote, taker_surplus, settled) =
        price_amm_house_fill(fill, taker, house, allocation, filler, cx)?;
    let filled = FillAmounts {
        base: fill.base_filled,
        quote: taker_quote,
    };
    settled.accrue_builder_fee(filler, cx)?;

    move_taker_position(taker, filled, cx)?;
    accrue_market_fees(cx, &settled.fees, Some(taker_surplus))?;

    taker
        .stats
        .increment_total_rebate(settled.fees.maker_rebate)?;
    settled.accrue_referrer_reward(filler, cx)?;
    charge_taker(taker, &settled, cx)?;
    if settled.fees.maker_rebate != 0 {
        controller::position::update_quote_asset_and_break_even_amount(
            &mut taker.user.perp_positions[taker.position_index],
            cx.market,
            settled.fees.maker_rebate.cast()?,
        )?;
    }
    if allocation.post_only {
        taker.stats.update_maker_volume_30d(taker_quote, cx.now)?;
    } else {
        taker.stats.update_taker_volume_30d(taker_quote, cx.now)?;
    }
    pay_house_keeper(house, filler, &settled, taker_quote, cx)?;

    advance_taker_order(taker, filler, &settled, filled)?;
    emit_amm_house_record(taker, &settled, filled, taker_surplus, &filler.key, cx)?;
    Ok((fill.base_filled, taker_quote))
}

/// Price one vAMM slice on the house fee schedule.
///
/// Returns what the taker pays, what the house keeps as spread, and the split
/// of the fee between them.
fn price_amm_house_fill(
    fill: &QuoterFill,
    taker: &mut TakerSide,
    house: &mut HouseSide,
    allocation: &AmmAllocation,
    filler: &mut FillerSide,
    cx: &mut SettleContext,
) -> VelocityResult<(u64, i64, SettledFees)> {
    let (taker_quote, taker_surplus) = amm_house_taker_quote(fill, taker, allocation)?;
    let market_index = cx.market.market_index;
    let reward_referrer =
        can_reward_user_with_referral_reward(market_index, filler.rev_share_escrow);
    let reward_filler = can_reward_user_with_perp_pnl(filler.user, market_index)
        || can_reward_user_with_perp_pnl(house.cranking_maker, market_index);
    let escrow = BuilderEscrow::read(
        filler,
        taker,
        market_index,
        allocation.order_id,
        cx.policy.builder_fee_allowed,
    );
    let fees = fees::calculate_fee_for_fulfillment_with_amm(
        taker.stats,
        taker_quote,
        cx.policy.fee_structure,
        allocation.order_slot,
        cx.slot,
        reward_filler,
        reward_referrer,
        cx.policy.referrer_is_accelerated,
        taker_surplus,
        allocation.post_only,
        cx.market.fee_adjustment,
        escrow.fee_tenth_bps,
        house.pays_maker_rebate,
        cx.market.taker_fee_addon_tenth_bps,
        cx.now,
        cx.policy.promo_fee_tier,
        cx.oracle_map.slot_clock,
        *cx.filler_reward_paid,
    )?;
    Ok((
        taker_quote,
        taker_surplus,
        SettledFees::take(fees, escrow, cx),
    ))
}

/// Pay whoever turned the house fill: the keeper when one is loaded, otherwise
/// the maker that cranked it.
fn pay_house_keeper(
    house: &mut HouseSide,
    filler: &mut FillerSide,
    settled: &SettledFees,
    taker_quote: u64,
    cx: &mut SettleContext,
) -> VelocityResult {
    if let Some(filler_user) = filler.user.as_mut() {
        return credit_filler_perp_pnl(
            filler_user,
            filler.stats,
            cx.market,
            settled.fees.filler_reward,
            taker_quote,
            cx.now,
            cx.slot,
        );
    }
    if let Some(maker_user) = house.cranking_maker.as_mut() {
        return credit_filler_perp_pnl(
            maker_user,
            house.cranking_maker_stats,
            cx.market,
            settled.fees.filler_reward,
            taker_quote,
            cx.now,
            cx.slot,
        );
    }
    Ok(())
}

/// Emit the fill record for a house leg.
///
/// The house holds no position, so both of the record's seats are the taker's
/// own: a post-only taker filled as the maker and is reported on the maker
/// seat.
fn emit_amm_house_record(
    taker: &mut TakerSide,
    settled: &SettledFees,
    filled: FillAmounts,
    taker_surplus: i64,
    filler_key: &Pubkey,
    cx: &mut SettleContext,
) -> VelocityResult {
    let (taker_record_key, taker_record_order, maker_record_key, maker_record_order) =
        get_taker_and_maker_for_order_record(&taker.key, taker.order);
    let explanation = if cx.policy.is_liquidation {
        OrderActionExplanation::Liquidation
    } else {
        OrderActionExplanation::OrderFilledWithAMM
    };
    // The house is the counterparty, so it holds no position to be isolated.
    let bit_flags = fill_record_bit_flags(taker, false);
    let (existing_quote_entry_amount, existing_base_asset_amount) =
        calculate_existing_position_fields_for_order_action(
            filled.base,
            taker.existing_position_params_before,
        )?;
    let on_taker_seat = taker_record_key.is_some();
    let (taker_existing_quote, taker_existing_base) = if on_taker_seat {
        (existing_quote_entry_amount, existing_base_asset_amount)
    } else {
        (None, None)
    };
    let (maker_existing_quote, maker_existing_base) = if on_taker_seat {
        (None, None)
    } else {
        (existing_quote_entry_amount, existing_base_asset_amount)
    };
    emit_perp_action_record(
        cx.market,
        cx.oracle_map,
        cx.now,
        explanation,
        filler_key,
        settled.fees.filler_reward,
        filled.base,
        filled.quote,
        settled.taker_debit()?,
        (settled.fees.maker_rebate != 0).then_some(settled.fees.maker_rebate),
        settled.fees.referrer_reward,
        Some(taker_surplus),
        taker_record_key,
        taker_record_order,
        maker_record_key,
        maker_record_order,
        bit_flags,
        taker_existing_quote,
        taker_existing_base,
        maker_existing_quote,
        maker_existing_base,
        settled.escrow.builder_index,
        settled.fees.builder_fee,
    )
}

/// The resting maker order a DLOB match filled against, and the prices the
/// match is held to.
pub(crate) struct DlobMatch {
    /// The maker order's slot in its owner's `orders` array.
    pub order_index: usize,
    /// The sanitized price discovery froze the maker order at.
    pub maker_price: u64,
    /// The taker's effective limit. Its side of the fill must clear it.
    pub taker_limit: u64,
    /// The oracle price the filler-reward tier is measured against.
    pub oracle_price: i64,
}

/// The prices an external match is held to.
///
/// There is no maker price here. The route already bound the quoter's response
/// per unit against its own quoted levels, which is the maker-side contract.
pub(crate) struct ExternalMatch {
    /// The taker's effective limit, when the order carries one.
    pub taker_limit: Option<u64>,
    /// The oracle price the filler-reward tier is measured against.
    pub oracle_price: i64,
}

/// What the keeper's reward tier is measured against.
///
/// The tier reads the maker's own price, so a leg with no single maker price
/// hands in the average it filled at instead.
#[derive(Clone, Copy)]
struct RewardTier {
    maker_price: u64,
    oracle_price: i64,
}

/// Price one match on the maker fee schedule.
fn price_matched_fill(
    taker: &mut TakerSide,
    maker: &MakerSide,
    filled: FillAmounts,
    tier: RewardTier,
    filler: &mut FillerSide,
    cx: &mut SettleContext,
) -> VelocityResult<SettledFees> {
    let market_index = cx.market.market_index;
    let reward_referrer =
        can_reward_user_with_referral_reward(market_index, filler.rev_share_escrow);
    // A maker that cranks its own fill arrives as `filler: None` with the
    // filler key naming itself: it is already loaded in the maker map, and the
    // same account cannot be loaded mutably twice. It did the keeper's work on
    // a slice it actually filled, so it earns the reward for that slice, which
    // spreads a multi-maker fill's reward pro rata. A taker filling its own
    // order names *itself*, so this stays false and no reward is charged.
    let maker_is_filler = filler.key == maker.key;
    let reward_filler = can_reward_user_with_perp_pnl(filler.user, market_index) || maker_is_filler;
    let escrow = BuilderEscrow::read(
        filler,
        taker,
        market_index,
        taker.order.order_id,
        cx.policy.builder_fee_allowed,
    );
    let filler_multiplier = if reward_filler {
        calculate_filler_multiplier_for_matched_orders(
            tier.maker_price,
            maker.direction,
            tier.oracle_price,
        )?
    } else {
        0
    };
    let fees = fees::calculate_fee_for_fulfillment_with_match(
        taker.stats,
        &maker.stats,
        filled.quote,
        cx.policy.fee_structure,
        taker.order.slot,
        cx.slot,
        filler_multiplier,
        reward_referrer,
        cx.policy.referrer_is_accelerated,
        &MarketType::Perp,
        cx.market.fee_adjustment,
        escrow.fee_tenth_bps,
        cx.market.taker_fee_addon_tenth_bps,
        cx.now,
        cx.policy.promo_fee_tier,
        cx.oracle_map.slot_clock,
        *cx.filler_reward_paid,
    )?;
    Ok(SettledFees::take(fees, escrow, cx))
}

/// The spine both match legs walk once their own schedule has priced the fill.
///
/// Book the market's share, charge the taker, pay the maker its rebate, pay
/// the keeper, accrue the referrer's reward, and advance the taker's order.
/// What is left for each leg is its counterparty's own unwind and its record.
fn settle_matched_fill(
    taker: &mut TakerSide,
    maker: &mut MakerSide,
    settled: &SettledFees,
    filled: FillAmounts,
    filler: &mut FillerSide,
    cx: &mut SettleContext,
) -> VelocityResult {
    settled.accrue_builder_fee(filler, cx)?;
    accrue_market_fees(cx, &settled.fees, None)?;
    charge_taker(taker, settled, cx)?;
    credit_maker_rebate(maker, taker, settled.fees.maker_rebate, cx)?;
    pay_matched_keeper(maker, filler, settled, filled.quote, cx)?;
    settled.accrue_referrer_reward(filler, cx)?;
    advance_taker_order(taker, filler, settled, filled)
}

/// Pay the keeper that turned a matched fill.
///
/// A maker that cranked its own fill is paid on its own seat, because it is
/// already loaded as the maker and cannot be loaded a second time as the
/// filler.
fn pay_matched_keeper(
    maker: &mut MakerSide,
    filler: &mut FillerSide,
    settled: &SettledFees,
    quote_filled: u64,
    cx: &mut SettleContext,
) -> VelocityResult {
    if filler.user.is_some() {
        return pay_fill_keeper(filler, settled, quote_filled, cx);
    }
    if filler.key != maker.key {
        return Ok(());
    }
    credit_filler_perp_pnl(
        maker.user,
        &mut maker.stats.as_deref_mut(),
        cx.market,
        settled.fees.filler_reward,
        quote_filled,
        cx.now,
        cx.slot,
    )
}

/// Settle one DLOB match: the taker against one resting velocity order.
///
/// The maker's liquidity is a velocity `Order`, so this leg is the one that
/// advances that order and flips it to `Filled`, and the one whose fill record
/// carries a maker order.
pub(crate) fn settle_dlob_match_fill(
    fill: &QuoterFill,
    taker: &mut TakerSide,
    maker: &mut MakerSide,
    matched: &DlobMatch,
    filler: &mut FillerSide,
    cx: &mut SettleContext,
) -> VelocityResult<(u64, u64, u64)> {
    let filled = FillAmounts {
        base: fill.base_filled,
        quote: fill.quote_filled,
    };
    validate_fill_price(
        filled.quote,
        filled.base,
        BASE_PRECISION_U64,
        taker.direction,
        matched.taker_limit,
        true,
    )?;
    validate_fill_price(
        filled.quote,
        filled.base,
        BASE_PRECISION_U64,
        maker.direction,
        matched.maker_price,
        false,
    )?;

    move_maker_position(maker, taker, filled, cx)?;
    move_taker_position(taker, filled, cx)?;
    taker.stats.update_taker_volume_30d(filled.quote, cx.now)?;

    let tier = RewardTier {
        maker_price: matched.maker_price,
        oracle_price: matched.oracle_price,
    };
    let settled = price_matched_fill(taker, maker, filled, tier, filler, cx)?;
    settle_matched_fill(taker, maker, &settled, filled, filler, cx)?;
    unwind_matched_maker_order(maker, matched.order_index, filled.base)?;

    emit_matched_record(
        taker,
        maker,
        &settled,
        filled,
        MatchedRecord {
            explanation: OrderActionExplanation::OrderFilledWithMatch,
            maker_order: Some(maker.user.orders[matched.order_index]),
            filler_key: filler.key,
        },
        cx,
    )?;
    Ok((filled.base, filled.quote, filled.base))
}

/// Unwind the reservation the filled maker order held, and retire it once it
/// has nothing left.
///
/// The quoter already advanced the order's own filled counters, so only the
/// open-bids/asks aggregate and the status remain.
fn unwind_matched_maker_order(
    maker: &mut MakerSide,
    order_index: usize,
    base_filled: u64,
) -> VelocityResult {
    let updates_open_bids_and_asks = maker.user.orders[order_index].update_open_bids_and_asks();
    decrease_open_bids_and_asks(
        &mut maker.user.perp_positions[maker.position_index],
        &maker.direction,
        base_filled,
        updates_open_bids_and_asks,
    )?;
    if maker.user.orders[order_index].get_base_asset_amount_unfilled(None)? == 0 {
        maker.user.orders[order_index].status = OrderStatus::Filled;
    }
    Ok(())
}

/// Settle one external-quoter balance change.
///
/// The maker is a loaded `User` whose resting liquidity lives outside velocity
/// — a CLOB order or a PropAMM quote — so unlike [`settle_dlob_match_fill`]
/// there is no velocity `Order` to advance: the external program already
/// committed its own book state. Everything protocol-level is the same match
/// spine.
///
/// The maker side runs no `validate_fill_price`. The route already held the
/// response per unit to this quoter's own quoted levels, which is the
/// maker-side price contract here. The taker side clears its effective limit
/// as usual.
pub(crate) fn settle_external_match_fill(
    filled: FillAmounts,
    taker: &mut TakerSide,
    maker: &mut MakerSide,
    prices: &ExternalMatch,
    filler: &mut FillerSide,
    cx: &mut SettleContext,
) -> VelocityResult<(u64, u64)> {
    if let Some(limit) = prices.taker_limit {
        validate_fill_price(
            filled.quote,
            filled.base,
            BASE_PRECISION_U64,
            taker.direction,
            limit,
            true,
        )?;
    }

    move_maker_position(maker, taker, filled, cx)?;
    move_taker_position(taker, filled, cx)?;
    taker.stats.update_taker_volume_30d(filled.quote, cx.now)?;

    // An external fill has no single maker limit, so the average fill price
    // stands in for the filler-reward tier.
    let average_price = filled
        .quote
        .cast::<u128>()?
        .safe_mul(BASE_PRECISION_U64.cast()?)?
        .safe_div(filled.base.cast()?)?
        .cast::<u64>()?;
    let tier = RewardTier {
        maker_price: average_price,
        oracle_price: prices.oracle_price,
    };
    let settled = price_matched_fill(taker, maker, filled, tier, filler, cx)?;
    settle_matched_fill(taker, maker, &settled, filled, filler, cx)?;
    release_external_maker_reservation(maker, filled.base)?;

    emit_matched_record(
        taker,
        maker,
        &settled,
        filled,
        MatchedRecord {
            explanation: OrderActionExplanation::OrderFilledWithExternalQuoter,
            maker_order: maker.order_id.map(|order_id| Order {
                order_id,
                market_index: cx.market.market_index,
                market_type: MarketType::Perp,
                direction: maker.direction,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                post_only: true,
                bit_flags: OrderBitFlag::PlacedOnClob as u8,
                ..Order::default()
            }),
            filler_key: filler.key,
        },
        cx,
    )?;
    Ok((filled.base, filled.quote))
}

/// Release the open-base a quoter's maker reserved for the size this leg
/// filled.
///
/// The maker's leg is the quoter's own claim about a user it does not own, so
/// it is held to that user's reservation rather than clamped to it. This is
/// the single place every external settlement passes through — the router
/// fill and both cross cranks — which is what stops a caller from settling one
/// without the bound.
///
/// CLOB orders are margin-reserved through velocity at placement, so their
/// fills release those aggregates. Custom PropAMM depth is never reserved.
fn release_external_maker_reservation(maker: &mut MakerSide, base_filled: u64) -> VelocityResult {
    if !maker.reserved {
        return Ok(());
    }
    position::release_reserved_open_base(
        &mut maker.user.perp_positions[maker.position_index],
        &maker.direction,
        base_filled,
    )
}

/// What one match leg's fill record says that the spine cannot.
struct MatchedRecord {
    explanation: OrderActionExplanation,
    /// The maker's order, as the record reports it. An external quoter has no
    /// velocity order, so it reconstructs the one its book row stands for.
    maker_order: Option<Order>,
    filler_key: Pubkey,
}

/// Emit the fill record for a matched leg.
fn emit_matched_record(
    taker: &mut TakerSide,
    maker: &MakerSide,
    settled: &SettledFees,
    filled: FillAmounts,
    record: MatchedRecord,
    cx: &mut SettleContext,
) -> VelocityResult {
    let explanation = if cx.policy.is_liquidation {
        OrderActionExplanation::Liquidation
    } else {
        record.explanation
    };
    let bit_flags = fill_record_bit_flags(
        taker,
        maker.user.perp_positions[maker.position_index].is_isolated(),
    );
    let (taker_existing_quote, taker_existing_base) =
        calculate_existing_position_fields_for_order_action(
            filled.base,
            taker.existing_position_params_before,
        )?;
    let (maker_existing_quote, maker_existing_base) =
        calculate_existing_position_fields_for_order_action(
            filled.base,
            maker.existing_position_params,
        )?;
    let taker_order = *taker.order;
    emit_perp_action_record(
        cx.market,
        cx.oracle_map,
        cx.now,
        explanation,
        &record.filler_key,
        settled.fees.filler_reward,
        filled.base,
        filled.quote,
        settled.taker_debit()?,
        Some(settled.fees.maker_rebate),
        settled.fees.referrer_reward,
        None,
        Some(taker.key),
        Some(taker_order),
        Some(maker.key),
        record.maker_order,
        bit_flags,
        taker_existing_quote,
        taker_existing_base,
        maker_existing_quote,
        maker_existing_base,
        settled.escrow.builder_index,
        settled.fees.builder_fee,
    )
}

/// Accrue a revenue-share amount against the builder's order.
///
/// The per-order accrual is mirrored into the market aggregate the fee sweep
/// reserves against (audit #73), so the two never drift.
fn accrue_revenue_share(
    escrow: &mut RevenueShareEscrowZeroCopyMut,
    order_index: u32,
    amount: u64,
    market: &mut PerpMarket,
) -> VelocityResult<()> {
    let order = escrow.get_order_mut(order_index)?;
    order.fees_accrued = order.fees_accrued.safe_add(amount)?;
    market.accrue_pending_revenue_share(amount)?;
    Ok(())
}

/// Fold one settled allocation into the worst price the fill has reached.
///
/// Worse means higher for a buy and lower for a sell. The price is the
/// allocation's own quote over its own base, floored, which is how every other
/// per-fill price in this file is derived. Both directions round the same way,
/// so a caller comparing a buy price against a sell price compares two numbers
/// that were rounded alike.
fn note_worst_fill_price(
    worst: &mut Option<u64>,
    direction: PositionDirection,
    base_filled: u64,
    quote_filled: u64,
) -> VelocityResult {
    if base_filled == 0 {
        return Ok(());
    }
    let price = quote_filled
        .cast::<u128>()?
        .safe_mul(BASE_PRECISION_U64.cast()?)?
        .safe_div(base_filled.cast()?)?
        .cast::<u64>()?;
    *worst = Some(match (*worst, direction) {
        (None, _) => price,
        (Some(seen), PositionDirection::Long) => seen.max(price),
        (Some(seen), PositionDirection::Short) => seen.min(price),
    });
    Ok(())
}

/// The bit flags every fill record carries.
///
/// A signed-message order is marked so a consumer can tell swift flow from
/// on-chain flow. A fill is marked isolated when either side settles into an
/// isolated position, because the record then describes a position whose
/// collateral is not the account's.
fn fill_record_bit_flags(taker: &TakerSide, maker_is_isolated: bool) -> u8 {
    let flags = set_order_bit_flag(0, taker.order.is_signed_msg(), OrderBitFlag::SignedMessage);
    let taker_is_isolated = taker.user.perp_positions[taker.position_index].is_isolated();
    set_order_bit_flag(
        flags,
        taker_is_isolated || maker_is_isolated,
        OrderBitFlag::IsIsolatedPosition,
    )
}

pub fn update_order_after_fill(
    order: &mut Order,
    base_asset_amount: u64,
    quote_asset_amount: u64,
) -> VelocityResult<bool> {
    order.base_asset_amount_filled = order.base_asset_amount_filled.safe_add(base_asset_amount)?;

    order.quote_asset_amount_filled = order
        .quote_asset_amount_filled
        .safe_add(quote_asset_amount)?;

    let is_filled = order.get_base_asset_amount_unfilled(None)? == 0;
    if is_filled {
        order.status = OrderStatus::Filled;
    }

    Ok(is_filled)
}

#[allow(clippy::type_complexity)]
fn get_taker_and_maker_for_order_record(
    user_key: &Pubkey,
    user_order: &Order,
) -> (Option<Pubkey>, Option<Order>, Option<Pubkey>, Option<Order>) {
    if user_order.post_only {
        (None, None, Some(*user_key), Some(*user_order))
    } else {
        (Some(*user_key), Some(*user_order), None, None)
    }
}

fn cancel_reduce_only_trigger_orders(
    user: &mut User,
    user_key: &Pubkey,
    filler_key: Option<&Pubkey>,
    maps: &mut AccountMaps,
    now: i64,
    slot: u64,
    perp_market_index: u16,
) -> VelocityResult {
    for order_index in 0..user.orders.len() {
        if user.orders[order_index].status != OrderStatus::Open {
            continue;
        }

        if user.orders[order_index].market_type != MarketType::Perp {
            continue;
        }

        if user.orders[order_index].market_index != perp_market_index {
            continue;
        }

        if !user.orders[order_index].must_be_triggered() || user.orders[order_index].triggered() {
            continue;
        }

        if !user.orders[order_index].reduce_only {
            continue;
        }

        cancel_order(
            order_index,
            user,
            user_key,
            maps,
            now,
            slot,
            OrderActionExplanation::ReduceOnlyOrderIncreasedPosition,
            filler_key,
            0,
            false,
        )?;
    }

    Ok(())
}

/// The market's safe MM oracle price and how valid it is.
///
/// Every perp fill path reads the oracle this way: the MM price the market
/// derives from the raw feed, then the validity of its safe, confidence
/// bounded form. The caller passes the raw price data it already holds, so
/// this never repeats the map lookup and never reorders it.
fn safe_mm_oracle_state(
    market: &PerpMarket,
    state: &State,
    oracle_price_data: &OraclePriceData,
    slot: u64,
) -> VelocityResult<(crate::state::oracle::MMOraclePriceData, OracleValidity)> {
    let mm_oracle_price_data = market.get_mm_oracle_price_data(
        *oracle_price_data,
        slot,
        &state.oracle_guard_rails.validity,
        state.slot_clock(),
    )?;
    let safe_oracle_price_data = mm_oracle_price_data.get_safe_oracle_price_data();
    let safe_oracle_validity = oracle::oracle_validity(
        MarketType::Perp,
        market.market_index,
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap,
        &safe_oracle_price_data,
        &state.oracle_guard_rails.validity,
        market.get_max_confidence_interval_multiplier()?,
        &market.oracle_source,
        oracle::LogMode::SafeMMOracle,
        market.oracle_slot_delay_override,
        mm_oracle_price_data.is_safe_price_mm_sourced(),
        market.oracle_low_risk_slot_delay_override,
        slot,
        state.slot_clock(),
    )?;
    Ok((mm_oracle_price_data, safe_oracle_validity))
}

/// The oracle state a crossed-book crank checks before it moves a position.
///
/// A crank matches two resting sources at a price the oracle bounds, so it
/// holds the oracle to the rules an ordinary fill holds it to. The market
/// must not be in settlement. Its fills must not be paused. The safe MM
/// oracle must permit a match fill. The mark must sit inside the market's
/// price band.
///
/// `crank` names the caller in the error message, so a refusal says which
/// crank refused. The market stays borrowed by the caller, which reads its
/// own extra fields after this returns.
pub(crate) struct CrankOraclePreflight {
    pub oracle_price: i64,
    /// Whether the oracle is too stale for the margin checks to trust it.
    pub stale_for_margin: bool,
    /// Open interest before the crank, which the post-fill rule measures
    /// against.
    pub open_interest: u128,
}

pub(crate) fn crank_oracle_preflight(
    market: &mut PerpMarket,
    state: &State,
    oracle_map: &mut OracleMap,
    clock: &Clock,
    crank: &str,
) -> VelocityResult<CrankOraclePreflight> {
    validation::perp_market::validate_perp_market(market)?;
    validate!(
        !market.is_in_settlement(clock.unix_timestamp),
        ErrorCode::MarketFillOrderPaused,
        "Market is in settlement mode",
    )?;
    validate!(
        !market.is_operation_paused(PerpOperation::Fill),
        ErrorCode::MarketFillOrderPaused,
        "Market fills paused",
    )?;

    let oracle_price_data = *oracle_map.get_price_data(&market.oracle_id())?;
    let (mm_oracle_price_data, safe_oracle_validity) =
        safe_mm_oracle_state(market, state, &oracle_price_data, clock.slot)?;
    validate!(
        is_oracle_valid_for_action(safe_oracle_validity, Some(VelocityAction::FillOrderMatch))?,
        ErrorCode::InvalidOracle,
        "oracle not valid for {}",
        crank
    )?;
    let oracle_price = mm_oracle_price_data.get_price();
    validate_market_within_price_band(market, state, oracle_price)?;
    Ok(CrankOraclePreflight {
        oracle_price,
        stale_for_margin: state
            .slot_clock()
            .elapsed_slot_delta(mm_oracle_price_data.get_delay().max(0) as u64, clock.slot)
            > state.oracle_guard_rails.validity.stale_for_margin_ms(),
        open_interest: market.get_open_interest(),
    })
}

/// The oracle pre-flight and the pricing of one taker-origin cross, before any
/// of it is committed.
///
/// The caller must run this *before* the CLOB calls that consume the pair: the
/// cross is refused outright when crossing would leave the taker worse off
/// than the price it was resting at, and a refusal has to leave the book
/// untouched. Same oracle gates the fill path applies (`FillOrderMatch`
/// validity, price band, staleness) — the reward's size-vs-oracle multiplier
/// is derived from the price, so it is a value transfer driven by an oracle
/// read and gated like one.
///
/// Returns the fee split, the mm-oracle price the settlement values fills
/// against, whether the oracle is stale for margin, and the market's open
/// interest before the fill. The last three are what a caller that settles the
/// match itself needs for [`fulfill_perp_order_post_checks`].
///
/// `rest_price` is the price the taker-origin order rests at,
/// `counterparty_price` the price the match will settle at, and `order_slot`
/// the slot the taker-origin order was placed on the book.
#[allow(clippy::too_many_arguments)]
pub fn price_taker_origin_cross(
    state: &State,
    market_index: u16,
    taker_direction: PositionDirection,
    rest_price: u64,
    counterparty_price: u64,
    base_asset_amount: u64,
    order_slot: u64,
    taker_stats: &UserStats,
    perp_market_map: &PerpMarketMap,
    oracle_map: &mut OracleMap,
    clock: &Clock,
) -> VelocityResult<(fees::TakerOriginCrossFee, i64, bool, u128)> {
    let (
        oracle_price,
        oracle_stale_for_margin,
        perp_market_oi_before,
        fee_adjustment,
        taker_fee_addon,
    ) = {
        let market = &mut perp_market_map.get_ref_mut(&market_index)?;
        let preflight =
            crank_oracle_preflight(market, state, oracle_map, clock, "taker-origin cross")?;
        (
            preflight.oracle_price,
            preflight.stale_for_margin,
            preflight.open_interest,
            market.fee_adjustment,
            market.taker_fee_addon_tenth_bps,
        )
    };

    // Both notionals at the CLOB's own rounding, so the improvement is
    // measured in the same units the fill will settle in.
    let notional = |price: u64| clob_notional(price, base_asset_amount);
    let fee = fees::calculate_taker_origin_cross_fee(
        taker_direction,
        notional(rest_price)?,
        notional(counterparty_price)?,
        &fees::determine_user_fee_tier(
            taker_stats,
            &state.perp_fee_structure,
            &MarketType::Perp,
            clock.unix_timestamp,
            state.promo_fee_tier,
        )?,
        fee_adjustment,
        taker_fee_addon,
        order_slot,
        clock.slot,
        state.slot_clock(),
        calculate_filler_multiplier_for_matched_orders(
            counterparty_price,
            taker_direction.opposite(),
            oracle_price,
        )?,
        &state.perp_fee_structure.filler_reward_structure,
    )?;
    Ok((
        fee,
        oracle_price,
        oracle_stale_for_margin,
        perp_market_oi_before,
    ))
}

/// Notional of `base_asset_amount` at `price`, floored.
///
/// The CLOB's own rounding, which is what makes a notional velocity computes
/// for a remainder it prices itself land in the same units a book-filled leg
/// would have.
pub fn clob_notional(price: u64, base_asset_amount: u64) -> VelocityResult<u64> {
    price
        .cast::<u128>()?
        .safe_mul(base_asset_amount.cast()?)?
        .safe_div(BASE_PRECISION_U64.cast()?)?
        .cast::<u64>()
}

pub fn trigger_order(
    order_id: u32,
    state: &State,
    user: &AccountLoader<User>,
    user_stats: &AccountLoader<UserStats>,
    maps: &mut AccountMaps,
    filler: &AccountLoader<User>,
    clock: &Clock,
    // Returns whether the trigger did payable work: `true` when it triggered
    // the order and paid the keeper, `false` when it cancelled, found the
    // order already triggered, or did nothing. The crank handler skips the
    // reservoir payout on `false`, so a failing account's cancel branch cannot
    // drain the market's reservoir.
) -> VelocityResult<bool> {
    let now = clock.unix_timestamp;
    let slot = clock.slot;

    let filler_key = filler.key();
    let user_key = user.key();
    let user = &mut load_mut!(user)?;
    let user_stats_loader = user_stats;
    let user_stats = load!(user_stats_loader)?;

    let order_index = user
        .orders
        .iter()
        .position(|order| order.order_id == order_id && order.status == OrderStatus::Open)
        .ok_or_else(print_error!(ErrorCode::OrderDoesNotExist))?;

    let Order {
        status: order_status,
        market_index,
        market_type,
        ..
    } = user.orders[order_index];

    validate!(
        order_status == OrderStatus::Open,
        ErrorCode::OrderNotOpen,
        "Order not open"
    )?;

    validate!(
        user.orders[order_index].must_be_triggered(),
        ErrorCode::OrderNotTriggerable,
        "Order is not triggerable"
    )?;

    // A placed trigger's slot deliberately reads as untriggered (that keeps
    // it out of every DLOB matching path), so guard explicitly: its live
    // order already rests on the CLOB.
    validate!(
        !user.orders[order_index].is_placed_on_clob(),
        ErrorCode::OrderPlacedOnClob,
        "Order is placed on the CLOB"
    )?;

    if user.orders[order_index].triggered() {
        msg!("Order is already triggered");
        return Ok(false);
    }

    validate!(
        market_type == MarketType::Perp,
        ErrorCode::InvalidOrderMarketType,
        "Order must be a perp order"
    )?;

    validate_user_not_being_liquidated(user, maps, state.liquidation_margin_buffer_ratio)?;

    validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

    let perp_market = maps.perp_market_map.get_ref_mut(&market_index)?;

    // Triggering starts the order's auction (and pays the keeper reward), so it
    // is part of the fill lifecycle: respect the market-scoped fill pause the
    // same way `fill_perp_order` does. The exchange-wide `FillPaused` breaker is
    // enforced by the handler's `fill_not_paused` access control.
    validate!(
        !perp_market.is_operation_paused(PerpOperation::Fill),
        ErrorCode::MarketFillOrderPaused,
        "Market fills paused",
    )?;

    // A trigger starts the order's auction and pays the flat keeper reward, both
    // of which the place/fill paths forbid once a market is in settlement (see
    // the `is_in_settlement` gate in `place_perp_order`). Without the same gate
    // here a keeper could trigger a dormant order on an expired/settling market,
    // minting a settleable positive zero-base quote claim out of the flat reward
    // and consuming PnL-pool headroom that backs legitimate expiry claimants
    // (OtterSec #86).
    validate!(
        !perp_market.is_in_settlement(now),
        ErrorCode::MarketPlaceOrderPaused,
        "Market is in settlement mode",
    )?;

    let (oracle_price_data, oracle_validity) = maps.oracle_map.get_price_data_and_validity(
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

    let is_oracle_valid =
        is_oracle_valid_for_action(oracle_validity, Some(VelocityAction::TriggerOrder))?;

    validate!(is_oracle_valid, ErrorCode::InvalidOracle)?;

    let oracle_price = oracle_price_data.price;

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

    let trigger_price =
        perp_market.get_trigger_price(oracle_price, now, state.use_median_trigger_price())?;
    let can_trigger = order_satisfies_trigger_condition(&user.orders[order_index], trigger_price)?;

    validate!(
        can_trigger,
        ErrorCode::OrderDidNotSatisfyTriggerCondition,
        "Order did not satisfy trigger condition. trigger_price: {} oracle_price: {} trigger_condition: {:?}",
        trigger_price,
        &user.orders[order_index].trigger_price,
        &user.orders[order_index].trigger_condition
    )?;

    let (_, worst_case_liability_value_before) = user
        .get_perp_position(market_index)?
        .worst_case_liability_value(oracle_price)?;

    let mut bit_flags = 0;
    {
        // Trigger-order auction params quote off the AMM's cached spread
        // state (refreshed by the keeper crank / fill setup for this slot).
        update_trigger_order_params(
            &mut user.orders[order_index],
            oracle_price_data,
            slot,
            // ~8s minimum, in wall clock 400ms units
            Millis::from_secs(8)
                .div_periods(Millis::UNIT)
                .min(u8::MAX as u64) as u8,
            Some(&perp_market),
            state.slot_clock(),
        )?;

        if user.orders[order_index].has_auction() {
            user.increment_open_auctions();
        }

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
        if user_position.is_isolated() {
            bit_flags = set_order_bit_flag(bit_flags, true, OrderBitFlag::IsIsolatedPosition);
        }
    }

    let (_, worst_case_liability_value_after) = user
        .get_perp_position(market_index)?
        .worst_case_liability_value(oracle_price)?;

    let is_risk_increasing = worst_case_liability_value_after > worst_case_liability_value_before;

    drop(perp_market);

    // If order increases risk and the user is below initial margin, below their
    // own buffered equity floor, or the authority-wide equity breaker is tripped, cancel
    // it instead of activating it. A floored account whose floor cannot be
    // verified (any invalid oracle) rejects the trigger instead; cancelling
    // is irreversible and must not run on an unverifiable value. The breaker
    // check mirrors the fill/withdraw/transfer paths: while it is set, no
    // risk-increasing action is allowed on any of the authority's
    // subaccounts. Evaluated before the keeper reward is paid, so a keeper
    // cannot farm the trigger reward out of a frozen or below-floor account
    // by flipping its resting risk-increasing orders into immediate cancels.
    if is_risk_increasing && !user.orders[order_index].reduce_only {
        let margin_calc = calculate_margin_requirement_and_total_collateral_and_liability_info(
            user,
            maps,
            MarginContext::standard(MarginRequirementType::Initial),
        )?;

        let net_equity = calculate_net_equity_for_floor(user, maps)?;

        // An unverifiable floor rejects the trigger instead of cancelling:
        // a cancel is irreversible, so an oracle blip must not destroy a
        // resting order the account may legitimately carry. The keeper
        // retries once the feed recovers and the gate resolves either way.
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
        if !margin_calc.meets_margin_requirement()
            || net_equity.is_some_and(|net_equity| !net_equity.clears_buffered_floor(user))
            || user_stats.is_equity_breaker_tripped()
        {
            cancel_order(
                order_index,
                user,
                &user_key,
                maps,
                now,
                slot,
                OrderActionExplanation::InsufficientFreeCollateral,
                Some(&filler_key),
                0,
                false,
            )?;

            user.update_last_active_slot(slot);

            // The cancel succeeds while the subaccount may already sit below
            // its raw floor; arm the breaker inline so the keeper's trigger
            // doubles as the trip.
            drop(user_stats);
            let mut user_stats = load_mut!(user_stats_loader)?;
            controller::equity_floor::try_lazy_equity_breaker_trip(user, &mut user_stats, maps)?;

            // The cancel did no payable trigger work — the user paid no flat
            // reward here, so the crank must not draw the reservoir either.
            return Ok(false);
        }
    }

    let is_filler_taker = user_key == filler_key;
    let mut filler = if !is_filler_taker {
        Some(load_mut!(filler)?)
    } else {
        None
    };

    let mut perp_market = maps.perp_market_map.get_ref_mut(&market_index)?;

    let filler_reward = pay_keeper_flat_reward_for_perps(
        user,
        filler.as_deref_mut(),
        &mut perp_market,
        state.perp_fee_structure.flat_filler_fee,
        slot,
    )?;

    drop(perp_market);

    let order_action_record = get_order_action_record(
        now,
        OrderAction::Trigger,
        OrderActionExplanation::None,
        market_index,
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
        Some(user.orders[order_index]),
        None,
        None,
        oracle_price,
        bit_flags,
        None,
        None,
        None,
        None,
        Some(trigger_price),
        None,
        None,
    )?;
    emit!(order_action_record);

    user.update_last_active_slot(slot);

    Ok(true)
}

/// Fire a DLOB trigger order and hand back the now-live order for the caller to
/// route straight to the book.
///
/// This is the v1 trigger path. Unlike [`trigger_order`], it does not leave the
/// fired order resting live in `User.orders` for a later fill crank to find. It
/// validates the trigger, transforms a copy of the slot's order into a live
/// market order, frees the slot, pays the keeper, and returns the order as a
/// detached value. The caller fills it against the book and rests only the
/// remainder, the same straight-to-book shape a v1 place takes. The armed slot
/// reserved no exposure, so freeing it releases only the order count; the
/// remainder the caller rests re-adds one for its CLOB order.
///
/// Returns `None` when there is no payable work: the order is already
/// triggered, or a risk-increasing trigger on a failing account is cancelled
/// instead of fired. The caller skips the fill and the reservoir payout on
/// `None`, so a failing account's cancel cannot drain the market reservoir.
#[allow(clippy::too_many_arguments)]
pub fn trigger_and_route_order(
    order_id: u32,
    state: &State,
    user: &AccountLoader<User>,
    user_stats: &AccountLoader<UserStats>,
    maps: &mut AccountMaps,
    filler: &AccountLoader<User>,
    clock: &Clock,
) -> VelocityResult<Option<Order>> {
    let now = clock.unix_timestamp;
    let slot = clock.slot;

    let filler_key = filler.key();
    let user_key = user.key();
    let user = &mut load_mut!(user)?;
    let user_stats_loader = user_stats;
    let user_stats = load!(user_stats_loader)?;

    let order_index = user
        .orders
        .iter()
        .position(|order| order.order_id == order_id && order.status == OrderStatus::Open)
        .ok_or_else(print_error!(ErrorCode::OrderDoesNotExist))?;

    let Order {
        status: order_status,
        market_index,
        market_type,
        ..
    } = user.orders[order_index];

    validate!(
        order_status == OrderStatus::Open,
        ErrorCode::OrderNotOpen,
        "Order not open"
    )?;
    validate!(
        user.orders[order_index].must_be_triggered(),
        ErrorCode::OrderNotTriggerable,
        "Order is not triggerable"
    )?;
    // A placed trigger's slot reads as untriggered to stay out of every DLOB
    // matching path; its live order already rests on the CLOB.
    validate!(
        !user.orders[order_index].is_placed_on_clob(),
        ErrorCode::OrderPlacedOnClob,
        "Order is placed on the CLOB"
    )?;

    if user.orders[order_index].triggered() {
        msg!("Order is already triggered");
        return Ok(None);
    }

    validate!(
        market_type == MarketType::Perp,
        ErrorCode::InvalidOrderMarketType,
        "Order must be a perp order"
    )?;

    validate_user_not_being_liquidated(user, maps, state.liquidation_margin_buffer_ratio)?;
    validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

    // Validate market state and oracle, and price the trigger. The market
    // borrow is dropped before the margin calc, which walks every market
    // itself.
    let (oracle_price_data, oracle_price, trigger_price) = {
        let perp_market = maps.perp_market_map.get_ref(&market_index)?;
        validate!(
            !perp_market.is_operation_paused(PerpOperation::Fill),
            ErrorCode::MarketFillOrderPaused,
            "Market fills paused",
        )?;
        // A trigger starts the order's auction and pays the flat reward, both
        // forbidden once a market is in settlement. Without this a keeper could
        // fire a dormant order on a settling market for the reward (OtterSec #86).
        validate!(
            !perp_market.is_in_settlement(now),
            ErrorCode::MarketPlaceOrderPaused,
            "Market is in settlement mode",
        )?;

        let (oracle_price_data, oracle_validity) = maps.oracle_map.get_price_data_and_validity(
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
        let is_oracle_valid =
            is_oracle_valid_for_action(oracle_validity, Some(VelocityAction::TriggerOrder))?;
        validate!(is_oracle_valid, ErrorCode::InvalidOracle)?;
        let oracle_price = oracle_price_data.price;

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

        let trigger_price =
            perp_market.get_trigger_price(oracle_price, now, state.use_median_trigger_price())?;
        (*oracle_price_data, oracle_price, trigger_price)
    };

    let can_trigger = order_satisfies_trigger_condition(&user.orders[order_index], trigger_price)?;
    validate!(
        can_trigger,
        ErrorCode::OrderDidNotSatisfyTriggerCondition,
        "Order did not satisfy trigger condition. trigger_price: {} oracle_price: {} trigger_condition: {:?}",
        trigger_price,
        &user.orders[order_index].trigger_price,
        &user.orders[order_index].trigger_condition
    )?;

    // Transform a copy of the slot's order into the live market order. The copy,
    // not the slot, so freeing the slot never reserves exposure the ephemeral
    // fill does not rest.
    let mut fired = user.orders[order_index];
    {
        let perp_market = maps.perp_market_map.get_ref(&market_index)?;
        update_trigger_order_params(
            &mut fired,
            &oracle_price_data,
            slot,
            // ~8s minimum, in wall clock 400ms units
            Millis::from_secs(8)
                .div_periods(Millis::UNIT)
                .min(u8::MAX as u64) as u8,
            Some(&perp_market),
            state.slot_clock(),
        )?;
    }

    // Whether the fired order increases risk: apply its worst-case exposure to
    // the position, measure, and take it straight back. The ephemeral fill does
    // not rest it, so the reservation must not linger.
    let (_, worst_case_before) = user
        .get_perp_position(market_index)?
        .worst_case_liability_value(oracle_price)?;
    let update_open_bids_and_asks = fired.update_open_bids_and_asks();
    {
        let position = user.get_perp_position_mut(market_index)?;
        increase_open_bids_and_asks(
            position,
            &fired.direction,
            fired.base_asset_amount,
            update_open_bids_and_asks,
        )?;
    }
    let (_, worst_case_after) = user
        .get_perp_position(market_index)?
        .worst_case_liability_value(oracle_price)?;
    {
        let position = user.get_perp_position_mut(market_index)?;
        decrease_open_bids_and_asks(
            position,
            &fired.direction,
            fired.base_asset_amount,
            update_open_bids_and_asks,
        )?;
    }
    let is_risk_increasing = worst_case_after > worst_case_before;

    // A risk-increasing trigger on a failing account cancels instead of firing.
    // The same gate `trigger_order` runs: initial margin, buffered equity
    // floor, and the authority-wide equity breaker, before any reward. An
    // unverifiable floor rejects rather than cancels, since a cancel is
    // irreversible.
    if is_risk_increasing && !fired.reduce_only {
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
        if !margin_calc.meets_margin_requirement()
            || net_equity.is_some_and(|net_equity| !net_equity.clears_buffered_floor(user))
            || user_stats.is_equity_breaker_tripped()
        {
            cancel_order(
                order_index,
                user,
                &user_key,
                maps,
                now,
                slot,
                OrderActionExplanation::InsufficientFreeCollateral,
                Some(&filler_key),
                0,
                false,
            )?;
            user.update_last_active_slot(slot);
            drop(user_stats);
            let mut user_stats = load_mut!(user_stats_loader)?;
            controller::equity_floor::try_lazy_equity_breaker_trip(user, &mut user_stats, maps)?;
            return Ok(None);
        }
    }

    let mut bit_flags = 0;
    if user
        .get_perp_position(market_index)
        .map(|position| position.is_isolated())
        .unwrap_or(false)
    {
        bit_flags = set_order_bit_flag(bit_flags, true, OrderBitFlag::IsIsolatedPosition);
    }

    // Pay the keeper the flat trigger reward and record the trigger. The fill
    // the caller runs settles its own fees; this is the trigger's own reward,
    // paid once for the crank that fired it.
    let is_filler_taker = user_key == filler_key;
    let mut filler = if !is_filler_taker {
        Some(load_mut!(filler)?)
    } else {
        None
    };
    let filler_reward = {
        let mut perp_market = maps.perp_market_map.get_ref_mut(&market_index)?;
        pay_keeper_flat_reward_for_perps(
            user,
            filler.as_deref_mut(),
            &mut perp_market,
            state.perp_fee_structure.flat_filler_fee,
            slot,
        )?
    };

    let order_action_record = get_order_action_record(
        now,
        OrderAction::Trigger,
        OrderActionExplanation::None,
        market_index,
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
        Some(fired),
        None,
        None,
        oracle_price,
        bit_flags,
        None,
        None,
        None,
        None,
        Some(trigger_price),
        None,
        None,
    )?;
    emit!(order_action_record);

    // Free the armed slot last, after the reward the position must still be
    // present for. The order is now the detached `fired` value; an untriggered
    // trigger reserved no exposure, so only the order count comes off. The
    // caller's fill tolerates the now-empty position and rebuilds it.
    {
        let position_index = get_position_index(&user.perp_positions, market_index)?;
        let slot_had_auction = user.orders[order_index].has_auction();
        user.decrement_open_orders(slot_had_auction);
        user.perp_positions[position_index].open_orders = user.perp_positions[position_index]
            .open_orders
            .saturating_sub(1);
        user.orders[order_index] = Order::default();
    }

    user.update_last_active_slot(slot);

    Ok(Some(fired))
}

fn update_trigger_order_params(
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

    // Here "below floor" authorizes a keeper against the user, so it fails
    // closed in the other direction from the gates above: the floor counts as
    // grounds only when every oracle is valid and the trusted value sits below
    // it, so a bad price cannot manufacture authorization. Under oracle
    // degradation the keeper falls back to the margin arm, which keeps
    // force-cancel available on a margin-breached account.
    let below_equity_floor = calculate_net_equity_for_floor(user, maps)?
        .is_some_and(|net_equity| net_equity.proves_below_floor(user));
    let meets_initial_margin_requirement = margin_calc.meets_margin_requirement();

    validate!(
        !meets_initial_margin_requirement || below_equity_floor,
        ErrorCode::SufficientCollateral
    )?;

    let cross_margin_meets_initial_margin_requirement =
        margin_calc.meets_cross_margin_requirement() && !below_equity_floor;

    let mut total_fee = 0_u64;

    for order_index in 0..user.orders.len() {
        if user.orders[order_index].status != OrderStatus::Open {
            continue;
        }

        // Placed triggers rest on the CLOB; force-cancelling them goes
        // through the CLOB (keeper-passed `OrderRef`s), not the shadow slot.
        if user.orders[order_index].is_placed_on_clob() {
            continue;
        }

        let market_index = user.orders[order_index].market_index;
        let market_type = user.orders[order_index].market_type;

        let fee = match market_type {
            MarketType::Spot => {
                let spot_market = maps.spot_market_map.get_ref(&market_index)?;
                let token_amount = user
                    .get_spot_position(market_index)?
                    .get_signed_token_amount(&spot_market)?
                    .cast::<i64>()?;
                let is_position_reducing = is_order_position_reducing(
                    &user.orders[order_index].direction,
                    user.orders[order_index].get_base_asset_amount_unfilled(Some(token_amount))?,
                    token_amount,
                )?;
                if is_position_reducing {
                    continue;
                }

                if cross_margin_meets_initial_margin_requirement {
                    continue;
                }

                state.spot_fee_structure.flat_filler_fee
            }
            MarketType::Perp => {
                let base_asset_amount = user.get_perp_position(market_index)?.base_asset_amount;
                let is_position_reducing = is_order_position_reducing(
                    &user.orders[order_index].direction,
                    user.orders[order_index]
                        .get_base_asset_amount_unfilled(Some(base_asset_amount))?,
                    base_asset_amount,
                )?;
                if is_position_reducing {
                    continue;
                }

                if !user.get_perp_position(market_index)?.is_isolated() {
                    if cross_margin_meets_initial_margin_requirement {
                        continue;
                    }
                } else {
                    let meets_isolated_margin_requirement =
                        margin_calc.meets_isolated_margin_requirement(market_index)?;
                    if meets_isolated_margin_requirement {
                        continue;
                    }
                }

                state.perp_fee_structure.flat_filler_fee
            }
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
        // Claim the filler's position slot BEFORE debiting the user, because
        // this is the half that can fail — a filler holding a position in
        // every slot, none of them this market's, gets none. Debiting first
        // and then bailing paid nobody and destroyed the user's quote: the
        // user's position and the market's aggregate both came out short by
        // the reward, so the value accrued to the pool instead of to the
        // keeper that earned it. `force_get_perp_position_mut` creates the
        // slot, so claiming it here is what the credit below finds.
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

/// The `Order` a taker-origin remainder is, so the router can fill it the way
/// it fills anything else.
///
/// It lives nowhere. The fill path takes the order itself, so this never
/// occupies one of the owner's order slots, and the remainder itself never
/// leaves the book — what the fill takes is reported to the book afterwards
/// and the order shrinks in place, against the reservation it already holds.
///
/// The order is a limit at the price it rested at. That price is the taker's
/// own bound, so a routed fill can only fill at or better than it, which is
/// what makes the improvement the auction is for reach the taker.
///
/// The CLOB's order id is wider than a velocity one. Narrowing it keeps the
/// fill records pointing at the book's order, since ids are sequential per
/// book, and the full-width id rides the crank's own record.
pub fn taker_origin_order(
    market_index: u16,
    taker_direction: PositionDirection,
    resting: &crate::math::crosses::RestingOrder,
) -> Order {
    Order {
        slot: resting.placed_slot,
        order_id: resting.order_ref.order_id as u32,
        market_index,
        status: OrderStatus::Open,
        order_type: OrderType::Limit,
        market_type: MarketType::Perp,
        direction: taker_direction,
        base_asset_amount: resting.base_asset_amount,
        price: resting.price,
        existing_position_direction: taker_direction,
        reduce_only: resting.reduce_only,
        ..Order::default()
    }
}

/// Pay the cranker its cut of a routed remainder's improvement, in quote,
/// taker → filler.
///
/// The same quote-for-work transfer the flat keeper rewards make, sized by the
/// improvement rather than by a flat fee, and paid in full or not at all —
/// [`crate::math::fees::calculate_taker_origin_cross_fee`] already decided
/// which.
///
/// The fill this follows has already run its own margin checks, so the debit
/// here happens after them and the taker is re-checked below. The reward is
/// bounded by the improvement the fill just delivered, so a taker that could
/// afford to rest can afford this; the check is what makes that a fact rather
/// than an argument.
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
        // A filler with no room for a position in this market reports zero
        // *after* debiting the taker, so the reward would leave the taker and
        // land nowhere. Revert instead: the cranker can pass a `User` that can
        // hold the position.
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
