//! Order lifecycle: placement validation, cancellation, and fill matching (perp + spot).
//! Margin math → `crate::math::margin`. Liquidation fills → `crate::controller::liquidation`.
//! `fill_perp_order` / `fulfill_perp_order_step` = keeper fill dispatch and maker matching.
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
        get_struct_values, get_then_update_id, load, load_mut,
        math::{
            auction::{calculate_auction_params_for_trigger_order, calculate_auction_prices},
            casting::Cast,
            constants::{BASE_PRECISION_U64, MARGIN_PRECISION},
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
            fill_mode::FillMode,
            margin_calculation::{MarginContext, MarginTypeConfig},
            market_status::MarketStatus,
            oracle::OraclePriceData,
            oracle_map::OracleMap,
            order_params::{ModifyOrderParams, OrderParams, PlaceOrderOptions, PostOnlyParam},
            paused_operations::PerpOperation,
            perp_market::PerpMarket,
            perp_market_map::PerpMarketMap,
            prop_amm::WireDirectionExt,
            quoter::{DlobOrderQuoter, MarketQuoteInputs as QuoteInputs, QuoteContext, QuoterFill},
            revenue_share::{
                RevenueShareEscrowZeroCopyMut, RevenueShareOrder, RevenueShareOrderBitFlag,
            },
            spot_market::{SpotBalanceType, SpotMarket},
            spot_market_map::SpotMarketMap,
            state::{FeeStructure, *},
            traits::Size,
            user::{
                MarketType, Order, OrderBitFlag, OrderStatus, OrderTriggerCondition, OrderType,
                ReferrerStatus, User, UserStats,
            },
            user_map::{UserMap, UserStatsMap},
        },
        validate,
        validation::{
            self,
            order::{validate_order, validate_order_for_force_reduce_only},
        },
        vlp::amm::{math::amm::calculate_amm_available_liquidity, AmmQuoter},
    },
    anchor_lang::prelude::*,
    std::{collections::BTreeMap, ops::DerefMut},
};

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

pub fn place_perp_order(
    state: &State,
    user: &mut User,
    user_key: Pubkey,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    clock: &Clock,
    mut params: OrderParams,
    mut options: PlaceOrderOptions,
    rev_share_order: &mut Option<&mut RevenueShareOrder>,
) -> VelocityResult<PlaceOrderResult> {
    let now = clock.unix_timestamp;
    let slot: u64 = clock.slot;

    if !options.is_liquidation() {
        validate_user_not_being_liquidated(
            user,
            perp_market_map,
            spot_market_map,
            oracle_map,
            state.liquidation_margin_buffer_ratio,
        )?;
    }

    validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

    if options.try_expire_orders {
        expire_orders(
            user,
            &user_key,
            perp_market_map,
            spot_market_map,
            oracle_map,
            now,
            slot,
        )?;
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
    let market = &perp_market_map.get_ref(&market_index)?;
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

    let position_index = get_position_index(&user.perp_positions, market_index)
        .or_else(|_| add_new_position(&mut user.perp_positions, market_index))?;

    // Increment open orders for existing position
    let (existing_position_direction, order_base_asset_amount) = {
        validate!(
            params.base_asset_amount >= market.order_step_size,
            ErrorCode::OrderAmountTooSmall,
            "params.base_asset_amount={} cannot be below market.order_step_size={}",
            params.base_asset_amount,
            market.order_step_size
        )?;

        let base_asset_amount = if params.base_asset_amount == u64::MAX
            && !(params.is_trigger_order() && params.reduce_only)
        {
            calculate_max_perp_order_size(
                user,
                position_index,
                params.market_index,
                params.direction,
                perp_market_map,
                spot_market_map,
                oracle_map,
            )?
        } else {
            standardize_base_asset_amount(params.base_asset_amount, market.order_step_size)?
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

    let oracle_price_data = oracle_map.get_price_data(&market.oracle_id())?;

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
        return Ok(PlaceOrderResult::default());
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
        route_digest: crate::state::order_params::NO_ROUTE_DIGEST,
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
            return Ok(PlaceOrderResult::default());
        }
        Err(err) => return Err(err),
    };

    let risk_increasing = is_new_order_risk_increasing(
        &new_order,
        user.perp_positions[position_index].base_asset_amount,
        user.perp_positions[position_index].open_bids,
        user.perp_positions[position_index].open_asks,
    )?;

    user.increment_open_orders(new_order.has_auction());
    user.orders[new_order_index] = new_order;
    user.perp_positions[position_index].open_orders += 1;
    increase_open_bids_and_asks(
        &mut user.perp_positions[position_index],
        &params.direction,
        order_base_asset_amount,
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
            perp_market_map,
            spot_market_map,
            oracle_map,
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

    let max_oi = market.max_open_interest;
    if max_oi != 0 && risk_increasing {
        let oi_plus_order = match params.direction {
            PositionDirection::Long => market
                .base_asset_amount_long
                .safe_add(order_base_asset_amount.cast()?)?
                .unsigned_abs(),
            PositionDirection::Short => market
                .base_asset_amount_short
                .safe_sub(order_base_asset_amount.cast()?)?
                .unsigned_abs(),
        };

        validate!(
            oi_plus_order <= max_oi,
            ErrorCode::MaxOpenInterest,
            "Order Base Amount={} could breach Max Open Interest for Perp Market={}",
            order_base_asset_amount,
            params.market_index
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
        oracle_map.get_price_data(&market.oracle_id())?.price,
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
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
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
        // reclaimed through the CLOB removal paths (cancel_clob_order or the
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
            perp_market_map,
            spot_market_map,
            oracle_map,
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
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
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
        perp_market_map,
        spot_market_map,
        oracle_map,
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
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
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
        perp_market_map,
        spot_market_map,
        oracle_map,
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
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    now: i64,
    _slot: u64,
    explanation: OrderActionExplanation,
    filler_key: Option<&Pubkey>,
    filler_reward: u64,
    skip_log: bool,
) -> VelocityResult {
    let (order_status, order_market_index, order_direction, order_market_type) = get_struct_values!(
        user.orders[order_index],
        status,
        market_index,
        direction,
        market_type
    );

    let is_perp_order = order_market_type == MarketType::Perp;

    validate!(order_status == OrderStatus::Open, ErrorCode::OrderNotOpen)?;

    // A placed trigger's live order rests on the CLOB; the slot here is a
    // shadow whose open-order count the CLOB order carries. Cancelling the
    // shadow would strand the CLOB order and double-unwind its accounting —
    // it must go through `cancel_clob_order` (bulk sweeps skip these slots).
    validate!(
        !user.orders[order_index].is_placed_on_clob(),
        ErrorCode::OrderPlacedOnClob,
        "order {} is placed on the CLOB",
        user.orders[order_index].order_id
    )?;

    let oracle_id = if is_perp_order {
        perp_market_map.get_ref(&order_market_index)?.oracle_id()
    } else {
        spot_market_map.get_ref(&order_market_index)?.oracle_id()
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
            oracle_map.get_price_data(&oracle_id)?.price,
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
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
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
        perp_market_map,
        spot_market_map,
        oracle_map,
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
            perp_market_map,
            spot_market_map,
            oracle_map,
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

/// [`fill_perp_order_with_router`] with no external quoter books: the router
/// pass still runs — vAMM ladder + passed DLOB makers — the split just has
/// no CPI books to price in. This is every fill entrypoint that doesn't
/// carry quoter accounts (place-and-take flows; external books there are a
/// planned follow-up).
pub fn fill_perp_order(
    order_id: u32,
    state: &State,
    user: &AccountLoader<User>,
    user_stats: &AccountLoader<UserStats>,
    spot_market_map: &SpotMarketMap,
    perp_market_map: &PerpMarketMap,
    oracle_map: &mut OracleMap,
    filler: &AccountLoader<User>,
    filler_stats: &AccountLoader<UserStats>,
    makers_and_referrer: &UserMap,
    makers_and_referrer_stats: &UserStatsMap,
    jit_maker_order_id: Option<u32>,
    clock: &Clock,
    fill_mode: FillMode,
    rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
    referrer_is_accelerated: bool,
) -> VelocityResult<(u64, u64)> {
    let mut no_externals = crate::state::prop_amm::NoExternalQuoters;
    let mut router_inputs = crate::math::router::RouterFillInputs {
        books: &[],
        executor: &mut no_externals,
        protocol_authority: state.signer,
        // This path carries no external book, so nothing can withhold depth
        // and the obligation is never reached.
        obligation: crate::math::router::FillerObligation::default(),
    };
    fill_perp_order_with_router(
        order_id,
        state,
        user,
        user_stats,
        spot_market_map,
        perp_market_map,
        oracle_map,
        filler,
        filler_stats,
        makers_and_referrer,
        makers_and_referrer_stats,
        jit_maker_order_id,
        clock,
        fill_mode,
        &mut router_inputs,
        rev_share_escrow,
        referrer_is_accelerated,
    )
}

/// [`fill_perp_order`] taking the caller's external quoter books explicitly.
/// The fill routes across the vAMM, the passed DLOB makers, and those books.
#[allow(clippy::too_many_arguments)]
pub fn fill_perp_order_with_router(
    order_id: u32,
    state: &State,
    user: &AccountLoader<User>,
    user_stats: &AccountLoader<UserStats>,
    spot_market_map: &SpotMarketMap,
    perp_market_map: &PerpMarketMap,
    oracle_map: &mut OracleMap,
    filler: &AccountLoader<User>,
    filler_stats: &AccountLoader<UserStats>,
    makers_and_referrer: &UserMap,
    makers_and_referrer_stats: &UserStatsMap,
    jit_maker_order_id: Option<u32>,
    clock: &Clock,
    fill_mode: FillMode,
    router: &mut crate::math::router::RouterFillInputs,
    rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
    referrer_is_accelerated: bool,
) -> VelocityResult<(u64, u64)> {
    let now = clock.unix_timestamp;
    let slot = clock.slot;

    let filler_key = filler.key();
    let user_key = user.key();
    let user = &mut load_mut!(user)?;
    let user_stats = &mut load_mut!(user_stats)?;

    let order_index = user
        .orders
        .iter()
        .position(|order| order.order_id == order_id && order.status == OrderStatus::Open)
        .ok_or_else(print_error!(ErrorCode::OrderDoesNotExist))?;

    let (order_status, market_index, order_market_type, order_reduce_only) = get_struct_values!(
        user.orders[order_index],
        status,
        market_index,
        market_type,
        reduce_only
    );

    validate!(
        order_market_type == MarketType::Perp,
        ErrorCode::InvalidOrderMarketType,
        "must be perp order"
    )?;

    // settle lp position so its tradeable
    let mut market = perp_market_map.get_ref_mut(&market_index)?;
    settle_funding_payment(user, &user_key, &mut market, now)?;

    validate!(
        matches!(
            market.status,
            MarketStatus::Active | MarketStatus::ReduceOnly
        ),
        ErrorCode::MarketFillOrderPaused,
        "Market not active",
    )?;

    validate!(
        !market.is_operation_paused(PerpOperation::Fill),
        ErrorCode::MarketFillOrderPaused,
        "Market fills paused",
    )?;

    // A `ReduceOnly` market forces every order it fills to be risk-reducing.
    // Placement only stamps `order.reduce_only` from the market status at the
    // time the order was created (`place_perp_order` -> `force_reduce_only`),
    // so a legacy order placed while the market was `Active` still carries
    // `reduce_only = false` after the market is flipped to `ReduceOnly`. Since
    // every downstream reduce-only guard (fill-size clamp in
    // `get_base_asset_amount_unfilled`, `should_cancel_reduce_only_order`, the
    // trigger-path risk check) keys off the stored flag, re-derive it from the
    // live market status here and stamp it onto the order so the fill cannot
    // increase exposure. Mirrors placement: once a market is reduce-only, its
    // orders are reduce-only.
    let market_is_reduce_only = market.is_reduce_only()?;

    drop(market);

    if market_is_reduce_only {
        user.orders[order_index].reduce_only = true;
    }

    validate!(
        order_status == OrderStatus::Open,
        ErrorCode::OrderNotOpen,
        "Order not open"
    )?;

    validate!(
        !user.orders[order_index].must_be_triggered() || user.orders[order_index].triggered(),
        ErrorCode::OrderMustBeTriggeredFirst,
        "Order must be triggered first"
    )?;

    if user.is_bankrupt() {
        msg!("user is bankrupt");
        return Ok((0, 0));
    }

    if !fill_mode.is_liquidation() {
        match validate_user_not_being_liquidated(
            user,
            perp_market_map,
            spot_market_map,
            oracle_map,
            state.liquidation_margin_buffer_ratio,
        ) {
            Ok(_) => {}
            Err(_) => {
                msg!("user is being liquidated");
                return Ok((0, 0));
            }
        }
    }

    // Revenue-share enforcement: the taker's `RevenueShareEscrow` is an optional
    // account, so a keeper could omit it and the associated fees would silently
    // resolve to zero. Two cases require it to be supplied:
    // 1. the taker order carries a builder code (the builder fee must accrue), or
    // 2. the taker is referred and their escrow exists (`BuilderReferral` is set
    //    only when an escrow was initialized with a referrer, and escrows cannot
    //    be closed), so the referee discount and referrer reward must apply.
    // Skip when the builder-codes feature is globally disabled (the keeper
    // passes no escrow by design) and for liquidations (the liquidatee's order
    // is force-filled without an escrow).
    if !fill_mode.is_liquidation() && state.builder_codes_enabled() {
        validate!(
            !user.orders[order_index].is_has_builder() || rev_share_escrow.is_some(),
            ErrorCode::UnableToLoadRevenueShareAccount,
            "Order has builder but no RevenueShareEscrow account was included in the fill"
        )?;
        validate!(
            !ReferrerStatus::has_builder_referral(user_stats.referrer_status)
                || rev_share_escrow.is_some(),
            ErrorCode::UnableToLoadRevenueShareAccount,
            "User is referred with an escrow but no RevenueShareEscrow account was included in the fill"
        )?;
    }

    let safe_oracle_validity: OracleValidity;
    let exchange_oracle_validity: OracleValidity;
    let oracle_price: i64;
    let oracle_twap_5min: i64;
    let user_can_skip_duration: bool;
    let oracle_stale_for_margin: bool;
    let amm_not_globally_paused: bool = !state.amm_paused()?;
    let mut amm_is_available: bool = amm_not_globally_paused;
    {
        let market = &mut perp_market_map.get_ref_mut(&market_index)?;
        validation::perp_market::validate_perp_market(market)?;
        validate!(
            !market.is_in_settlement(now),
            ErrorCode::MarketFillOrderPaused,
            "Market is in settlement mode",
        )?;

        let oracle_price_data = oracle_map.get_price_data(&market.oracle_id())?;
        exchange_oracle_validity = oracle_validity(
            MarketType::Perp,
            market.market_index,
            market
                .market_stats
                .historical_oracle_data
                .last_oracle_price_twap,
            oracle_price_data,
            &state.oracle_guard_rails.validity,
            market.get_max_confidence_interval_multiplier()?,
            &market.oracle_source,
            oracle::LogMode::ExchangeOracle,
            market.oracle_slot_delay_override,
            false,
            market.oracle_low_risk_slot_delay_override,
            slot,
            state.slot_clock(),
        )?;
        let mm_oracle_price_data = market.get_mm_oracle_price_data(
            *oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            state.slot_clock(),
        )?;
        let safe_oracle_price_data = mm_oracle_price_data.get_safe_oracle_price_data();
        safe_oracle_validity = oracle_validity(
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

        user_can_skip_duration = user.can_skip_auction_duration(user_stats, order_reduce_only)?;
        amm_is_available &= market.amm_can_fill_order(
            &user.orders[order_index],
            slot,
            fill_mode,
            state,
            safe_oracle_validity,
            user_can_skip_duration,
            &mm_oracle_price_data,
        )?;
        oracle_stale_for_margin = state
            .slot_clock()
            .elapsed_slot_delta(mm_oracle_price_data.get_delay().max(0) as u64, slot)
            > state.oracle_guard_rails.validity.stale_for_margin_ms();

        // No AMM mutation here — the fulfillment pass constructs an
        // `AmmQuoter` and calls `refresh` before quoting, which is the sole
        // non-admin AMM-refresh entrypoint. PerpMarket-level
        // oracle bookkeeping (TWAPs, reference-price-offset,
        // last_oracle_valid) is PerpMarket's own concern and stays here
        // (no AMM reacharound — PerpMarket reading its own AMM field).
        let amm_refresh_validity =
            crate::vlp::amm::refresh::compute_amm_refresh_validity_with_guard_rails(
                market,
                &mm_oracle_price_data,
                &state.oracle_guard_rails.validity,
                slot,
                state.slot_clock(),
            )?;

        // Snapshot the 5-minute oracle TWAP *before* the refresh below advances
        // it. This fill's own band checks — `is_oracle_too_divergent_with_twap_5min`
        // and `validate_fill_price_within_price_bands` — both measure against this
        // value, and the refresh pulls it toward the live oracle price. Reading it
        // afterwards let a currently-divergent oracle normalize itself inside the
        // same instruction and clear the very checks meant to stop the fill
        // (OtterSec #112).
        //
        // Unlike the funding crank (#109), the refresh itself stays: a fill is one
        // of the paths that legitimately advances the TWAPs, and it does not gate
        // on them, so snapshotting the reader is the whole fix.
        oracle_twap_5min = market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min;

        market.update_oracle_derived_stats(
            &mm_oracle_price_data,
            amm_refresh_validity,
            now,
            slot,
            state.slot_clock(),
        )?;

        oracle_price = mm_oracle_price_data.get_price();
    }

    // allow oracle price to be used to calculate limit price if it's valid or stale for amm
    let valid_oracle_price = if is_oracle_valid_for_action(
        safe_oracle_validity,
        Some(VelocityAction::OracleOrderPrice),
    )? {
        Some(oracle_price)
    } else {
        msg!("Perp market = {} oracle deemed invalid", market_index);
        None
    };

    // DLOB matches execute at maker limit prices with no auction protection,
    // so they carry their own validity rule: a NonPositive, TooVolatile or
    // TooUncertain oracle blocks match fills the same way the AMM's fill
    // gates already block AMM fills. `OracleOrderPrice` above is weaker (it
    // only decides whether oracle-relative limit prices resolve), so without
    // this a match could execute while every other consumer of the oracle
    // refuses it.
    let safe_match_fills_allowed =
        is_oracle_valid_for_action(safe_oracle_validity, Some(VelocityAction::FillOrderMatch))?;
    let exchange_match_fills_allowed = is_oracle_valid_for_action(
        exchange_oracle_validity,
        Some(VelocityAction::FillOrderMatch),
    )?;

    let is_filler_taker = user_key == filler_key;
    let is_filler_maker = makers_and_referrer.0.contains_key(&filler_key);
    let (mut filler, mut filler_stats) = if !is_filler_maker && !is_filler_taker {
        let filler = load_mut!(filler)?;

        validate!(
            filler.pool_id == 0,
            ErrorCode::InvalidPoolId,
            "filler pool id ({}) != 0",
            filler.pool_id
        )?;

        if filler.authority != user.authority {
            (Some(filler), Some(load_mut!(filler_stats)?))
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    let mut maker_orders_info = get_maker_orders_info(
        perp_market_map,
        spot_market_map,
        oracle_map,
        makers_and_referrer,
        &user_key,
        &user.orders[order_index],
        &mut filler.as_deref_mut(),
        &filler_key,
        state.perp_fee_structure.flat_filler_fee,
        oracle_price,
        exchange_match_fills_allowed,
        jit_maker_order_id,
        now,
        slot,
    )?;

    // Runs after `get_maker_orders_info` so its expired-maker-order cleanup
    // still happens; only the matching itself is withheld. AMM fills keep
    // their own gates.
    let taker_can_match =
        can_floored_user_match_with_exchange_oracle(user, exchange_match_fills_allowed);
    if !safe_match_fills_allowed || !taker_can_match {
        if !maker_orders_info.is_empty() {
            msg!(
                "Perp market = {} oracle not valid for match fills (safe={}, taker_exchange={})",
                market_index,
                safe_match_fills_allowed,
                taker_can_match,
            );
            maker_orders_info.clear();
        }
        // External quoter books (CLOB, Custom PropAMMs) execute at their own
        // maker prices with no auction protection, exactly like a DLOB match,
        // so the same oracle gate applies. Without this they would fill while
        // the oracle is NonPositive / TooVolatile / TooUncertain, bounded only
        // by the margin band around that same suspect oracle. The vAMM keeps
        // its own inclusion gate.
        router.books = &[];
    }

    let oracle_too_divergent_with_twap_5min = is_oracle_too_divergent_with_twap_5min(
        oracle_price,
        oracle_twap_5min,
        state
            .oracle_guard_rails
            .max_oracle_twap_5min_percent_divergence()
            .cast()?,
    )?;

    if oracle_too_divergent_with_twap_5min {
        // update filler last active so tx doesn't revert
        if let Some(filler) = filler.as_deref_mut() {
            filler.update_last_active_slot(slot);
        }
        return Ok((0, 0));
    }

    let should_expire_order = should_expire_order(user, order_index, now)?;

    let position_index =
        get_position_index(&user.perp_positions, user.orders[order_index].market_index)?;
    let existing_base_asset_amount = user.perp_positions[position_index].base_asset_amount;
    let should_cancel_reduce_only = should_cancel_reduce_only_order(
        &user.orders[order_index],
        existing_base_asset_amount,
        perp_market_map.get_ref_mut(&market_index)?.order_step_size,
    )?;

    if should_expire_order || should_cancel_reduce_only {
        let filler_reward = {
            let mut market = perp_market_map.get_ref_mut(&market_index)?;
            pay_keeper_flat_reward_for_perps(
                user,
                filler.as_deref_mut(),
                market.deref_mut(),
                state.perp_fee_structure.flat_filler_fee,
                slot,
            )?
        };

        let explanation = if should_expire_order {
            OrderActionExplanation::OrderExpired
        } else {
            OrderActionExplanation::ReduceOnlyOrderIncreasedPosition
        };

        cancel_order(
            order_index,
            user,
            &user_key,
            perp_market_map,
            spot_market_map,
            oracle_map,
            now,
            slot,
            explanation,
            Some(&filler_key),
            filler_reward,
            false,
        )?;

        return Ok((0, 0));
    }

    let (base_asset_amount, quote_asset_amount) = fulfill_perp_order(
        user,
        order_index,
        &user_key,
        user_stats,
        makers_and_referrer,
        makers_and_referrer_stats,
        &maker_orders_info,
        &mut filler.as_deref_mut(),
        &filler_key,
        &mut filler_stats.as_deref_mut(),
        spot_market_map,
        perp_market_map,
        oracle_map,
        &state.oracle_guard_rails.validity,
        &state.perp_fee_structure,
        valid_oracle_price,
        now,
        slot,
        amm_is_available,
        fill_mode,
        oracle_stale_for_margin,
        router,
        rev_share_escrow,
        referrer_is_accelerated,
        state.vamm_maker_rebate_enabled(),
        state.promo_fee_tier,
    )?;

    if base_asset_amount != 0 {
        let fill_price =
            calculate_fill_price(quote_asset_amount, base_asset_amount, BASE_PRECISION_U64)?;

        let mut perp_market = perp_market_map.get_ref_mut(&market_index)?;
        validate_fill_price_within_price_bands(
            fill_price,
            oracle_price,
            oracle_twap_5min,
            perp_market.margin_ratio_initial,
            state
                .oracle_guard_rails
                .max_oracle_twap_5min_percent_divergence(),
            None,
        )?;

        perp_market.last_fill_price = fill_price;
    }

    let base_asset_amount_after = user.perp_positions[position_index].base_asset_amount;
    let should_cancel_reduce_only = should_cancel_reduce_only_order(
        &user.orders[order_index],
        base_asset_amount_after,
        perp_market_map.get_ref_mut(&market_index)?.order_step_size,
    )?;

    if should_cancel_reduce_only {
        let filler_reward = {
            let mut market = perp_market_map.get_ref_mut(&market_index)?;
            pay_keeper_flat_reward_for_perps(
                user,
                filler.as_deref_mut(),
                market.deref_mut(),
                state.perp_fee_structure.flat_filler_fee,
                slot,
            )?
        };

        let explanation = OrderActionExplanation::ReduceOnlyOrderIncreasedPosition;

        cancel_order(
            order_index,
            user,
            &user_key,
            perp_market_map,
            spot_market_map,
            oracle_map,
            now,
            slot,
            explanation,
            Some(&filler_key),
            filler_reward,
            false,
        )?
    }

    if base_asset_amount_after == 0
        && user.perp_positions[position_index].open_asks == 0
        && user.perp_positions[position_index].open_bids == 0
    {
        cancel_reduce_only_trigger_orders(
            user,
            &user_key,
            Some(&filler_key),
            perp_market_map,
            spot_market_map,
            oracle_map,
            now,
            slot,
            market_index,
        )?;
    }

    if base_asset_amount == 0 {
        return Ok((base_asset_amount, quote_asset_amount));
    }

    {
        let market = perp_market_map.get_ref(&market_index)?;

        let open_interest = market.get_open_interest();
        let max_open_interest = market.max_open_interest;

        validate!(
            max_open_interest == 0 || max_open_interest > open_interest,
            ErrorCode::MaxOpenInterest,
            "open interest ({}) > max open interest ({})",
            open_interest,
            max_open_interest
        )?;
    }

    // Try to update the funding rate at the end of every trade
    {
        let market = &mut perp_market_map.get_ref_mut(&market_index)?;
        let funding_paused =
            state.funding_paused()? || market.is_operation_paused(PerpOperation::UpdateFunding);

        // Pass `None` so the funding update recomputes the reserve price from
        // the POST-fill AMM. The fills just moved the reserves, so gating the
        // mark/oracle divergence check (and the oracle-TWAP sanitization that
        // shares this value) on `reserve_price_before` would test a stale,
        // pre-fill mark — letting a fill that pushes the mark past the
        // divergence band still update funding, or conversely blocking a
        // funding update the post-fill mark no longer warrants.
        controller::funding::update_funding_rate(
            market_index,
            market,
            oracle_map,
            now,
            slot,
            &state.oracle_guard_rails,
            funding_paused,
            None,
        )?;
    }

    user.update_last_active_slot(slot);

    Ok((base_asset_amount, quote_asset_amount))
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
fn get_maker_orders_info(
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    makers_and_referrer: &UserMap,
    taker_key: &Pubkey,
    taker_order: &Order,
    filler: &mut Option<&mut User>,
    filler_key: &Pubkey,
    filler_reward: u64,
    oracle_price: i64,
    exchange_match_fills_allowed: bool,
    jit_maker_order_id: Option<u32>,
    now: i64,
    slot: u64,
) -> VelocityResult<Vec<(Pubkey, usize, u64)>> {
    let maker_direction = taker_order.direction.opposite();

    let mut maker_orders_info = Vec::with_capacity(16);

    for (maker_key, user_account_loader) in makers_and_referrer.0.iter() {
        if maker_key == taker_key {
            continue;
        }

        let mut maker = load_mut!(user_account_loader)?;

        if maker.is_being_liquidated() {
            continue;
        }

        let mut market = perp_market_map.get_ref_mut(&taker_order.market_index)?;
        let maker_order_price_and_indexes = find_maker_orders(
            &maker,
            &maker_direction,
            &MarketType::Perp,
            taker_order.market_index,
            Some(oracle_price),
            slot,
            market.order_tick_size,
            oracle_map.slot_clock,
        )?;

        if maker_order_price_and_indexes.is_empty() {
            continue;
        }

        maker.update_last_active_slot(slot);

        settle_funding_payment(&mut maker, maker_key, &mut market, now)?;

        let initial_margin_ratio = market.margin_ratio_initial;
        let step_size = market.order_step_size;
        // A `ReduceOnly` market forces resting maker orders risk-reducing too,
        // regardless of the flag they were placed with (see the taker-side note
        // in `fill_perp_order`). Stamped onto each maker order below so the
        // reduce-only cancel check and the position-capped `maker_unfilled`
        // fill size both apply.
        let market_is_reduce_only = market.is_reduce_only()?;

        drop(market);

        // A floored maker with any invalid oracle cannot prove it clears its
        // buffered floor, so the fill-time gate would reject its
        // risk-increasing fills, and by then the maker's leg has executed,
        // so the rejection poisons the taker's whole transaction. Oracle
        // validity cannot change across the fill, so resolve it here instead:
        // such a maker's risk-increasing orders are pruned (unmatchable until
        // its oracles recover), its provably reducing orders stay matchable
        // (the gate exempts them). Computed once per maker; free when no
        // floor is set.
        let maker_can_match =
            can_floored_user_match_with_exchange_oracle(&maker, exchange_match_fills_allowed);
        let maker_floor_unverifiable = if maker_can_match {
            match calculate_net_equity_for_floor(
                &maker,
                perp_market_map,
                spot_market_map,
                oracle_map,
            )? {
                Some(net_equity) => !net_equity.all_oracles_valid,
                None => false,
            }
        } else {
            false
        };

        // Candidates of an unverifiable floored maker that survive the
        // cleanup below, as (order index, price, unfilled base). The
        // admit/prune decision is made on the whole set after the loop, in
        // `admit_reducing_maker_orders`.
        let mut floor_prune_candidates: Vec<(usize, u64, u64)> = Vec::new();

        for (maker_order_index, maker_order_price) in maker_order_price_and_indexes.iter() {
            let maker_order_index = *maker_order_index;
            let maker_order_price = *maker_order_price;

            let maker_order = &maker.orders[maker_order_index];
            if !is_maker_for_taker(maker_order, taker_order, slot, oracle_map.slot_clock)? {
                continue;
            }

            if !are_orders_same_market_but_different_sides(maker_order, taker_order) {
                continue;
            }

            if let Some(jit_maker_order_id) = jit_maker_order_id {
                // if jit maker order id exists, must only use that order
                if maker_order.order_id != jit_maker_order_id {
                    continue;
                }
            }

            let breaches_oracle_price_limits = {
                limit_price_breaches_maker_oracle_price_bands(
                    maker_order_price,
                    maker_order.direction,
                    oracle_price,
                    initial_margin_ratio,
                )?
            };

            if market_is_reduce_only {
                maker.orders[maker_order_index].reduce_only = true;
            }

            let should_expire_order = should_expire_order(&maker, maker_order_index, now)?;

            let existing_base_asset_amount = maker
                .get_perp_position(maker.orders[maker_order_index].market_index)?
                .base_asset_amount;
            let should_cancel_reduce_only_order = should_cancel_reduce_only_order(
                &maker.orders[maker_order_index],
                existing_base_asset_amount,
                step_size,
            )?;

            if breaches_oracle_price_limits
                || should_expire_order
                || should_cancel_reduce_only_order
            {
                let filler_reward = {
                    let mut market = perp_market_map
                        .get_ref_mut(&maker.orders[maker_order_index].market_index)?;
                    pay_keeper_flat_reward_for_perps(
                        &mut maker,
                        filler.as_deref_mut(),
                        market.deref_mut(),
                        filler_reward,
                        slot,
                    )?
                };

                let explanation = if breaches_oracle_price_limits {
                    OrderActionExplanation::OraclePriceBreachedLimitPrice
                } else if should_expire_order {
                    OrderActionExplanation::OrderExpired
                } else {
                    OrderActionExplanation::ReduceOnlyOrderIncreasedPosition
                };

                cancel_order(
                    maker_order_index,
                    maker.deref_mut(),
                    maker_key,
                    perp_market_map,
                    spot_market_map,
                    oracle_map,
                    now,
                    slot,
                    explanation,
                    Some(filler_key),
                    filler_reward,
                    false,
                )?;

                continue;
            }

            // runs after the expire/reduce-only/band cleanup above so a
            // pruned maker still gets its stale orders cancelled and the
            // filler still earns the cleanup reward. Admission is deferred:
            // the candidates are judged together after the loop, so the
            // reducing budget goes to the best-priced orders instead of the
            // lowest order slots.
            // A selected MM oracle may be fresh enough to quote while the raw
            // exchange oracle used by the equity floor is not valid for margin.
            // Keep cleanup above live, but do not let a floored maker execute a
            // DLOB leg when its floor check cannot use the exchange oracle.
            if !maker_can_match {
                continue;
            }

            if maker_floor_unverifiable {
                let unfilled = maker.orders[maker_order_index]
                    .get_base_asset_amount_unfilled(Some(existing_base_asset_amount))?;
                floor_prune_candidates.push((maker_order_index, maker_order_price, unfilled));
                continue;
            }

            insert_maker_order_info(
                &mut maker_orders_info,
                (*maker_key, maker_order_index, maker_order_price),
                maker_direction,
            );
        }

        if maker_can_match && maker_floor_unverifiable {
            let resting_base_asset_amount = maker
                .get_perp_position(taker_order.market_index)
                .map(|position| position.base_asset_amount)
                .unwrap_or(0);

            for (maker_order_index, maker_order_price) in admit_reducing_maker_orders(
                floor_prune_candidates,
                maker_direction,
                resting_base_asset_amount,
            )? {
                insert_maker_order_info(
                    &mut maker_orders_info,
                    (*maker_key, maker_order_index, maker_order_price),
                    maker_direction,
                );
            }
        }
    }

    Ok(maker_orders_info)
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
        PositionDirection::Long => candidates.sort_by(|a, b| b.1.cmp(&a.1)),
        PositionDirection::Short => candidates.sort_by(|a, b| a.1.cmp(&b.1)),
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
    maker_orders_info: &mut Vec<(Pubkey, usize, u64)>,
    maker_order_info: (Pubkey, usize, u64),
    direction: PositionDirection,
) {
    let price = maker_order_info.2;
    let index = match maker_orders_info.binary_search_by(|item| match direction {
        PositionDirection::Short => item.2.cmp(&price),
        PositionDirection::Long => price.cmp(&item.2),
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

#[allow(clippy::too_many_arguments)]
fn fulfill_perp_order(
    user: &mut User,
    user_order_index: usize,
    user_key: &Pubkey,
    user_stats: &mut UserStats,
    makers_and_referrer: &UserMap,
    makers_and_referrer_stats: &UserStatsMap,
    maker_orders_info: &[(Pubkey, usize, u64)],
    filler: &mut Option<&mut User>,
    filler_key: &Pubkey,
    filler_stats: &mut Option<&mut UserStats>,
    spot_market_map: &SpotMarketMap,
    perp_market_map: &PerpMarketMap,
    oracle_map: &mut OracleMap,
    validity_guard_rails: &ValidityGuardRails,
    fee_structure: &FeeStructure,
    valid_oracle_price: Option<i64>,
    now: i64,
    slot: u64,
    amm_is_available: bool,
    fill_mode: FillMode,
    oracle_stale_for_margin: bool,
    // The external quoter books and their execute leg. Empty books are
    // normal — that is a fill against the vAMM and the passed DLOB makers.
    router: &mut crate::math::router::RouterFillInputs,
    rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
    referrer_is_accelerated: bool,
    vamm_maker_rebate: bool,
    promo_fee_tier: u8,
) -> VelocityResult<(u64, u64)> {
    let market_index = user.orders[user_order_index].market_index;

    let user_order_position_decreasing =
        determine_if_user_order_is_position_decreasing(user, market_index, user_order_index)?;
    let user_is_isolated_position = user.get_perp_position(market_index)?.is_isolated();

    // A risk-increasing taker whose floor cannot be verified would execute
    // its fulfillment legs and then revert at the buffered-floor gate below:
    // `validate_clears_buffered_floor` fails closed on any invalid oracle in
    // the taker's portfolio, related to this market or not, and by then the
    // legs have executed. A floored maker with the same defect is pruned in
    // `get_maker_orders_info`; the taker had no counterpart, so its visible
    // order made every fill attempt revert deterministically for the length
    // of the outage. Withhold the whole fill instead (the match and AMM
    // legs both end at that gate) and leave the order resting until its
    // oracles recover. Runs after the caller's expired/reduce-only cleanup,
    // which is unaffected. Reducing orders are exempt at the gate and stay
    // fillable; `user_order_position_decreasing` decides both. A liquidation
    // fill skips the gate, so it must skip this precheck too.
    if user.equity_floor > 0 && !fill_mode.is_liquidation() && !user_order_position_decreasing {
        let taker_floor_unverifiable = match calculate_net_equity_for_floor(
            user,
            perp_market_map,
            spot_market_map,
            oracle_map,
        )? {
            Some(net_equity) => !net_equity.all_oracles_valid,
            None => false,
        };

        if taker_floor_unverifiable {
            msg!(
                "taker {} equity floor unverifiable (invalid oracle in portfolio), withholding fill",
                user_key
            );
            if let Some(filler) = filler.as_deref_mut() {
                filler.update_last_active_slot(slot);
            }
            return Ok((0, 0));
        }
    }

    // A builder fee is an additive debit on the taker (the fill debits
    // `user_fee + builder_fee`) that the builder later claims into its own
    // account. The taker approves the builder, so the taker can approve
    // itself. The fee is therefore a transfer out of the account, and a
    // transfer out must clear the gate a withdrawal clears: initial margin.
    //
    // A position-decreasing fill is checked against maintenance margin below,
    // not initial. Without this gate, a taker below initial margin reduces the
    // position in slices and routes up to `MAX_BUILDER_FEE_TENTH_BPS` of each
    // slice to itself. Each slice also lowers the maintenance requirement, so
    // the next slice has more room and the sequence compounds. It moves value
    // that the initial-margin gate holds in the account (OtterSec #83).
    //
    // The fee is waived, not the fill. The taker still closes the position and
    // the builder is not paid for that fill. The margin state is read before
    // the fill, so a reduction that restores initial margin still waives the
    // fee for that fill. This is the safe direction.
    //
    // The gate uses the same oracle rules as the withdraw gate. It is strict,
    // so each price is the more conservative of the live price and the TWAP.
    // It ignores invalid deposit oracles, so a deposit with a bad oracle adds
    // no collateral. It also requires every liability oracle to be valid. A
    // single oracle push, or one stale oracle on an unrelated position, then
    // cannot clear the gate for the instant the fill needs. An oracle the
    // program cannot trust waives the fee; it does not fail the fill.
    let builder_fee_allowed = if fill_mode.is_liquidation()
        || !user.orders[user_order_index].is_has_builder()
        || rev_share_escrow.is_none()
    {
        false
    } else {
        let margin_type_config = if user_is_isolated_position {
            MarginTypeConfig::IsolatedPositionOverride {
                market_index,
                margin_requirement_type: MarginRequirementType::Initial,
                default_isolated_margin_requirement_type: MarginRequirementType::Maintenance,
                cross_margin_requirement_type: MarginRequirementType::Maintenance,
            }
        } else {
            MarginTypeConfig::CrossMarginOverride {
                margin_requirement_type: MarginRequirementType::Initial,
                default_margin_requirement_type: MarginRequirementType::Maintenance,
            }
        };

        let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
            user,
            perp_market_map,
            spot_market_map,
            oracle_map,
            MarginContext::standard_with_config(margin_type_config)
                .strict(true)
                .ignore_invalid_deposit_oracles(true),
        )?;

        calculation.meets_margin_requirement() && calculation.all_liability_oracles_valid
    };

    let perp_market = perp_market_map.get_ref(&market_index)?;
    let limit_price = fill_mode.get_limit_price(
        &user.orders[user_order_index],
        valid_oracle_price,
        slot,
        perp_market.order_tick_size,
        oracle_map.slot_clock,
    )?;
    let perp_market_oi_before = perp_market.get_open_interest();
    drop(perp_market);

    let mut maker_fills: BTreeMap<Pubkey, (i64, bool)> = BTreeMap::new();
    let (base_asset_amount, quote_asset_amount) = fulfill_perp_order_router_pass(
        user,
        user_order_index,
        user_key,
        user_stats,
        makers_and_referrer,
        makers_and_referrer_stats,
        maker_orders_info,
        filler,
        filler_key,
        filler_stats,
        spot_market_map,
        perp_market_map,
        oracle_map,
        validity_guard_rails,
        fee_structure,
        limit_price,
        now,
        slot,
        amm_is_available,
        fill_mode.is_liquidation(),
        router,
        rev_share_escrow,
        referrer_is_accelerated,
        vamm_maker_rebate,
        promo_fee_tier,
        builder_fee_allowed,
        &mut maker_fills,
    )?;
    fulfill_perp_order_post_checks(
        user,
        user_stats,
        makers_and_referrer,
        makers_and_referrer_stats,
        spot_market_map,
        perp_market_map,
        oracle_map,
        market_index,
        base_asset_amount,
        quote_asset_amount,
        &maker_fills,
        user_order_position_decreasing,
        user_is_isolated_position,
        perp_market_oi_before,
        oracle_stale_for_margin,
        fill_mode.is_liquidation(),
        now,
    )
}

/// Post-fill invariants the router pass runs after it settles its allocations:
/// fill-amount coherence, the taker's fill-margin + equity-floor/breaker
/// check, per-maker margin + equity-floor checks over the accumulated
/// `maker_fills`, and the stale-oracle OI rule.
#[allow(clippy::too_many_arguments)]
fn fulfill_perp_order_post_checks(
    user: &User,
    user_stats: &mut UserStats,
    makers_and_referrer: &UserMap,
    makers_and_referrer_stats: &UserStatsMap,
    spot_market_map: &SpotMarketMap,
    perp_market_map: &PerpMarketMap,
    oracle_map: &mut OracleMap,
    market_index: u16,
    base_asset_amount: u64,
    quote_asset_amount: u64,
    maker_fills: &BTreeMap<Pubkey, (i64, bool)>,
    user_order_position_decreasing: bool,
    user_is_isolated_position: bool,
    perp_market_oi_before: u128,
    oracle_stale_for_margin: bool,
    is_liquidation: bool,
    now: i64,
) -> VelocityResult<(u64, u64)> {
    validate!(
        (base_asset_amount > 0) == (quote_asset_amount > 0),
        ErrorCode::DefaultError,
        "invalid fill base = {} quote = {}",
        base_asset_amount,
        quote_asset_amount
    )?;

    let total_maker_fill = maker_fills.values().map(|(fill, _)| fill).sum::<i64>();

    validate!(
        total_maker_fill.unsigned_abs() <= base_asset_amount,
        ErrorCode::DefaultError,
        "invalid total maker fill {} total fill {}",
        total_maker_fill,
        base_asset_amount
    )?;

    if !is_liquidation {
        let margin_requirement_type = if user_order_position_decreasing {
            MarginRequirementType::Maintenance
        } else {
            MarginRequirementType::Fill
        };

        let margin_type_config = if user_is_isolated_position {
            MarginTypeConfig::IsolatedPositionOverride {
                market_index,
                margin_requirement_type,
                default_isolated_margin_requirement_type: MarginRequirementType::Maintenance,
                cross_margin_requirement_type: MarginRequirementType::Maintenance,
            }
        } else {
            MarginTypeConfig::CrossMarginOverride {
                margin_requirement_type,
                default_margin_requirement_type: MarginRequirementType::Maintenance,
            }
        };

        // A spot deposit whose oracle is invalid for margin contributes zero
        // collateral instead of its stale weighted value (OtterSec #143). Crediting
        // it let phantom collateral buy an in-band losing DLOB trade whose
        // counterparty then settled a real profit out of the PnL pool. Every other
        // value-releasing path already drops such a deposit —
        // `meets_withdraw_margin_requirement` and its two siblings all set this — and
        // a fill is the same decision.
        //
        // Dropping the deposit rather than rejecting the fill keeps the honest test:
        // an account with enough *valid* collateral still fills, and an account that
        // needs the stale deposit fails on `InsufficientCollateral`. It also covers
        // the reducing fill, which no reject keyed on risk direction can reach.
        let mut context = MarginContext::standard_with_config(margin_type_config)
            .ignore_invalid_deposit_oracles(true);

        if oracle_stale_for_margin && !user_order_position_decreasing {
            context = context.margin_ratio_override(MARGIN_PRECISION);
        }

        let taker_margin_calculation =
            calculate_margin_requirement_and_total_collateral_and_liability_info(
                user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                context,
            )?;

        if !taker_margin_calculation.meets_margin_requirement() {
            let (margin_requirement, total_collateral) =
                if taker_margin_calculation.has_isolated_margin_calculation(market_index) {
                    let isolated_margin_calculation =
                        taker_margin_calculation.get_isolated_margin_calculation(market_index)?;
                    (
                        isolated_margin_calculation.margin_requirement,
                        isolated_margin_calculation.total_collateral,
                    )
                } else {
                    (
                        taker_margin_calculation.margin_requirement,
                        taker_margin_calculation.total_collateral,
                    )
                };

            msg!(
                "taker breached fill requirements (margin requirement {}) (total_collateral {})",
                margin_requirement,
                total_collateral
            );
            return Err(ErrorCode::InsufficientCollateral);
        }

        // A borrow the calculation above could not value must not admit the fill
        // (OtterSec #144 / #148). The two ways it misvalues one are a stale oracle
        // and a stale cumulative index:
        //   #144 — a `StaleForMargin` spot borrow was priced at its stale low value,
        //          so an account that is insolvent at the refreshed price passes and
        //          becomes protocol bad debt.
        //   #148 — this handler makes no spot market refreshable, so every scaled
        //          borrow is valued through the market's *stored* borrow index and
        //          the interest accrued since `last_interest_ts` is simply absent.
        // A borrow has no counterpart to the deposit treatment above: dropping it
        // understates the debt, which is the very error being closed, so the fill
        // must revert instead.
        //
        // Both apply whichever direction the fill moves the position. The two-account
        // DLOB transfer in these findings works with both seats reducing: one seat
        // closes into the worst in-band price and leaves bad debt, the other settles
        // the matching profit out of the PnL pool. `meets_withdraw_margin_requirement`
        // draws the same line and exempts no direction. Liquidations are excluded —
        // this whole block is `if !fill_mode.is_liquidation()`.
        //
        // The spot-only liability flag is deliberate. `all_liability_oracles_valid` is
        // also cleared by an invalid *perp* oracle, which `oracle_stale_for_margin`
        // above already handles by overriding margin to 100% rather than rejecting.
        // Reading the broader field would silently replace that design with a hard
        // reject.
        validate!(
            taker_margin_calculation.all_spot_liability_oracles_valid,
            ErrorCode::InvalidOracle,
            "taker filling while a spot borrow oracle is invalid for margin"
        )?;

        // The crank is permissionless and can be bundled into the same transaction.
        crate::math::margin::validate_spot_borrow_interest_fresh_for_margin(
            user,
            spot_market_map,
            now,
        )?;

        if !user_order_position_decreasing {
            validate!(
                !user_stats.is_equity_breaker_tripped(),
                ErrorCode::EquityBelowFloor,
                "taker equity breaker is tripped"
            )?;

            // A risk-increasing fill must prove the taker clears its buffered
            // floor: an invalid oracle cannot price the taker up through the
            // floor and buy the fill.
            if let Some(taker_net_equity) =
                calculate_net_equity_for_floor(user, perp_market_map, spot_market_map, oracle_map)?
            {
                taker_net_equity.validate_clears_buffered_floor(user)?;
            }
        } else {
            // A reducing fill is exempt from the buffered-floor gate and may
            // legally leave the subaccount below its raw floor; arm the
            // breaker inline instead of waiting for the permissionless trip.
            controller::equity_floor::try_lazy_equity_breaker_trip(
                user,
                user_stats,
                perp_market_map,
                spot_market_map,
                oracle_map,
            )?;
        }
    }

    for (maker_key, (maker_base_asset_amount_filled, maker_is_isolated_position)) in maker_fills {
        let maker = makers_and_referrer.get_ref_mut(maker_key)?;

        let maker_breaker_tripped = if maker.authority == user.authority {
            user_stats.is_equity_breaker_tripped()
        } else {
            makers_and_referrer_stats
                .get_ref(&maker.authority)?
                .is_equity_breaker_tripped()
        };

        let (margin_type, maker_risk_increasing) = select_margin_type_for_perp_maker(
            &maker,
            *maker_base_asset_amount_filled,
            market_index,
        )?;

        let margin_type_config = if *maker_is_isolated_position {
            MarginTypeConfig::IsolatedPositionOverride {
                market_index,
                margin_requirement_type: margin_type,
                default_isolated_margin_requirement_type: MarginRequirementType::Maintenance,
                cross_margin_requirement_type: MarginRequirementType::Maintenance,
            }
        } else {
            MarginTypeConfig::CrossMarginOverride {
                margin_requirement_type: margin_type,
                default_margin_requirement_type: MarginRequirementType::Maintenance,
            }
        };

        // Same treatment of a stale spot deposit as the taker context above
        // (OtterSec #143). The DLOB transfer in that finding needs two accounts, so
        // crediting phantom collateral on the maker seat is worth exactly as much to
        // it as on the taker seat.
        let mut context = MarginContext::standard_with_config(margin_type_config)
            .ignore_invalid_deposit_oracles(true);

        if oracle_stale_for_margin {
            validate!(
                user_order_position_decreasing || !maker_risk_increasing,
                ErrorCode::InvalidOracle,
                "taker or maker must be reducing position if oracle stale for margin"
            )?;

            if maker_risk_increasing {
                context = context.margin_ratio_override(MARGIN_PRECISION);
            }
        }

        let maker_margin_calculation =
            calculate_margin_requirement_and_total_collateral_and_liability_info(
                &maker,
                perp_market_map,
                spot_market_map,
                oracle_map,
                context,
            )?;

        if !maker_margin_calculation.meets_margin_requirement() {
            let (margin_requirement, total_collateral) =
                if maker_margin_calculation.has_isolated_margin_calculation(market_index) {
                    let isolated_margin_calculation =
                        maker_margin_calculation.get_isolated_margin_calculation(market_index)?;
                    (
                        isolated_margin_calculation.margin_requirement,
                        isolated_margin_calculation.total_collateral,
                    )
                } else {
                    (
                        maker_margin_calculation.margin_requirement,
                        maker_margin_calculation.total_collateral,
                    )
                };

            msg!(
                "maker ({}) breached fill requirements (margin requirement {}) (total_collateral {})",
                maker_key,
                margin_requirement,
                total_collateral
            );
            return Err(ErrorCode::InsufficientCollateral);
        }

        // Same borrow-side gate as the taker (OtterSec #144 / #148), and on the same
        // terms: it applies whichever direction the fill moves the maker's position,
        // because the transfer these findings describe works with both seats
        // reducing.
        //
        // Excluded during a liquidation, which is how the taker side treats it as
        // well. This loop also runs for liquidation fills, so an unqualified reject
        // here would let one maker's stale spot oracle, or one maker's un-cranked
        // borrow market, block the liquidation of another account.
        if !is_liquidation {
            validate!(
                maker_margin_calculation.all_spot_liability_oracles_valid,
                ErrorCode::InvalidOracle,
                "maker ({}) filling while a spot borrow oracle is invalid for margin",
                maker_key
            )?;

            crate::math::margin::validate_spot_borrow_interest_fresh_for_margin(
                &maker,
                spot_market_map,
                now,
            )?;
        }

        if maker_risk_increasing {
            validate!(
                !maker_breaker_tripped,
                ErrorCode::EquityBelowFloor,
                "maker ({}) equity breaker is tripped",
                maker_key
            )?;

            // A risk-increasing maker fill must prove the maker clears its
            // buffered floor, the same fail-closed rule as the taker gate.
            // The invalid-oracle arm is normally unreachable: oracle validity
            // cannot change across the fill, and `get_maker_orders_info`
            // prunes a floored maker's risk-increasing orders while any of
            // its oracles is invalid. What reverts here is a genuine value
            // breach (or a fill that flipped a reducing order into new
            // risk).
            if let Some(maker_net_equity) = calculate_net_equity_for_floor(
                &maker,
                perp_market_map,
                spot_market_map,
                oracle_map,
            )? {
                maker_net_equity.validate_clears_buffered_floor(&maker)?;
            }
        } else if maker.equity_floor > 0 {
            // A reducing maker fill is exempt from the buffered-floor gate
            // and may legally leave the subaccount below its raw floor; arm
            // the breaker inline instead of waiting for the permissionless
            // trip.
            if maker.authority == user.authority {
                controller::equity_floor::try_lazy_equity_breaker_trip(
                    &maker,
                    user_stats,
                    perp_market_map,
                    spot_market_map,
                    oracle_map,
                )?;
            } else {
                let mut maker_stats = makers_and_referrer_stats.get_ref_mut(&maker.authority)?;
                controller::equity_floor::try_lazy_equity_breaker_trip(
                    &maker,
                    &mut maker_stats,
                    perp_market_map,
                    spot_market_map,
                    oracle_map,
                )?;
            }
        }

        if maker.authority != user.authority {
            let mut maker_stats = makers_and_referrer_stats.get_ref_mut(&maker.authority)?;
            maker_stats.try_auto_enroll_accelerated_referral_and_emit(now);
        }
    }

    // On a liquidation fill the taker seat is the liquidatee, who did not place the fill, so
    // it does not enroll. The maker seat above is unaffected.
    if base_asset_amount != 0 && !is_liquidation {
        user_stats.try_auto_enroll_accelerated_referral_and_emit(now);
    }

    if oracle_stale_for_margin {
        let perp_market_oi_after = perp_market_map.get_ref(&market_index)?.get_open_interest();
        validate!(
            perp_market_oi_after <= perp_market_oi_before,
            ErrorCode::InvalidOracle,
            "oracle stale for margin but open interest increased"
        )?;
    }

    Ok((base_asset_amount, quote_asset_amount))
}

#[inline(always)]
fn update_maker_fills_map(
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

fn determine_if_user_order_is_position_decreasing(
    user: &User,
    market_index: u16,
    order_index: usize,
) -> VelocityResult<bool> {
    let position_index = get_position_index(&user.perp_positions, market_index)?;
    let order_direction = user.orders[order_index].direction;
    let position_base_asset_amount_before = user.perp_positions[position_index].base_asset_amount;
    is_order_position_reducing(
        &order_direction,
        user.orders[order_index]
            .get_base_asset_amount_unfilled(Some(position_base_asset_amount_before))?,
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

/// Build and emit an `OrderActionRecord`. Extracted out of
/// `fulfill_perp_order_step` so its 480-byte `OrderActionRecord` plus the
/// two `Option<Order>` copies live in this helper's frame rather than the
/// already-large orchestrator's frame — the SBPF stack-overwrite check
/// otherwise fires.
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

#[allow(clippy::too_many_arguments)]
/// Settle a single sole-AMM fill step. Returns `(base_filled, quote_filled)`
/// to accumulate.
fn settle_amm_house_fill(
    fill: &QuoterFill,
    market: &mut PerpMarket,
    taker: &mut User,
    taker_stats: &mut UserStats,
    taker_position_index: usize,
    taker_order_index: usize,
    taker_key: &Pubkey,
    taker_direction: PositionDirection,
    taker_existing_position_params_before: Option<(u64, u64)>,
    order_post_only: bool,
    order_slot: u64,
    order_id: u32,
    taker_limit_price: Option<u64>,
    // The router priced this vAMM slice at the shaded allocation quote for
    // `amm_allocation_base` base. The shade is taker-worse than the live
    // curve. Charging the taker this quote, not the curve, keeps the shade
    // for the LPs.
    amm_allocation_quote: u64,
    amm_allocation_base: u64,
    is_liquidation: bool,
    maker: &mut Option<&mut User>,
    maker_stats: &mut Option<&mut UserStats>,
    filler: &mut Option<&mut User>,
    filler_stats: &mut Option<&mut UserStats>,
    filler_key: &Pubkey,
    rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
    referrer_is_accelerated: bool,
    fee_structure: &FeeStructure,
    oracle_map: &mut OracleMap,
    now: i64,
    slot: u64,
    vamm_maker_rebate: bool,
    promo_fee_tier: u8,
    builder_fee_allowed: bool,
    // Filler reward already paid by earlier legs of this same fill. The
    // time-based component of the reward is size-independent, so it is a
    // per-fill allowance the legs draw down rather than one each.
    filler_reward_paid: &mut u64,
) -> VelocityResult<(u64, u64)> {
    let slot_clock = oracle_map.slot_clock;
    // Decide the taker's quote and the AMM's surplus for this fill.
    //
    //  * post_only sole-AMM step: the taker acts as maker and transacts at
    //    its limit. The AMM keeps the curve to limit gap as spread surplus.
    //  * Normal sole-AMM step: charge the shade and hold the taker to its
    //    limit. See `settle_amm_house_normal_quote`.
    let (taker_quote, taker_surplus) = if order_post_only && taker_limit_price.is_some() {
        crate::controller::position::calculate_quote_asset_amount_surplus(
            taker_direction,
            fill.quote_filled,
            fill.base_filled,
            taker_limit_price.unwrap(),
        )?
    } else {
        settle_amm_house_normal_quote(
            fill,
            taker_direction,
            taker_limit_price,
            amm_allocation_quote,
            amm_allocation_base,
        )?
    };

    let reward_referrer =
        can_reward_user_with_referral_reward(market.market_index, rev_share_escrow);
    let reward_filler = can_reward_user_with_perp_pnl(filler, market.market_index)
        || can_reward_user_with_perp_pnl(maker, market.market_index);

    let (builder_order_idx, referrer_builder_order_idx, builder_order_fee_bps, builder_idx) =
        get_builder_escrow_info(
            rev_share_escrow,
            taker.sub_account_id,
            order_id,
            market.market_index,
            taker.orders[taker_order_index].is_has_builder(),
            builder_fee_allowed,
        );

    let FillFees {
        user_fee,
        fee_to_market,
        filler_reward,
        referee_discount,
        referrer_reward,
        maker_rebate,
        builder_fee: builder_fee_option,
        protocol_fee,
        if_fee,
        amm_fee,
    } = fees::calculate_fee_for_fulfillment_with_amm(
        taker_stats,
        taker_quote,
        fee_structure,
        order_slot,
        slot,
        reward_filler,
        reward_referrer,
        referrer_is_accelerated,
        taker_surplus,
        order_post_only,
        market.fee_adjustment,
        builder_order_fee_bps,
        vamm_maker_rebate,
        market.taker_fee_addon_tenth_bps,
        now,
        promo_fee_tier,
        slot_clock,
        *filler_reward_paid,
    )?;
    *filler_reward_paid = filler_reward_paid.saturating_add(filler_reward);
    let builder_fee = builder_fee_option.unwrap_or(0);

    if builder_fee != 0 {
        if let (Some(idx), Some(escrow)) = (builder_order_idx, rev_share_escrow.as_mut()) {
            let order = escrow.get_order_mut(idx)?;
            order.fees_accrued = order.fees_accrued.safe_add(builder_fee)?;
            // mirror the per-order accrual into the market aggregate the fee
            // sweep reserves (audit #73)
            market.accrue_pending_revenue_share(builder_fee)?;
        } else {
            validate!(
                false,
                ErrorCode::UnableToLoadRevenueShareAccount,
                "Order has builder fee but no escrow account found"
            )?;
        }
    }

    let taker_pd = get_position_delta_for_fill(fill.base_filled, taker_quote, taker_direction)?;
    update_position_and_market(
        &mut taker.perp_positions[taker_position_index],
        market,
        &taker_pd,
    )?;

    // the AMM books ONLY its own money: its fee provision + spread surplus
    // (`fee_to_market = amm_fee + surplus`). Protocol / IF carveouts never
    // touch the AMM's ledger or pools.
    <crate::vlp::amm::AMM as crate::vlp::amm::quoter::AmmContract>::apply_fill_fees(
        &mut market.amm,
        fee_to_market,
        taker_surplus,
    )?;
    // gross taker fee for analytics plus the explicit protocol / IF / AMM
    // carveouts of the trade-fee remainder. All three accrue as pending quote
    // counters here (the quote spot market isn't in scope at fill); their
    // token value lands in the pnl pool as fills settle and is materialized
    // into revenue_pool / protocol_fee_pool / amm.fee_pool by
    // `sweep_market_fees`. The AMM provision also grows the lifetime
    // backstop-of-last-resort clawback cap.
    market
        .fee_ledger
        .accrue_fill_fees(user_fee, protocol_fee, if_fee, amm_fee)?;

    taker_stats.increment_total_fees(user_fee)?;
    taker_stats.increment_total_rebate(maker_rebate)?;
    taker_stats.increment_total_referee_discount(referee_discount)?;

    if let (Some(idx), Some(escrow)) = (referrer_builder_order_idx, rev_share_escrow.as_mut()) {
        let order = escrow.get_order_mut(idx)?;
        order.fees_accrued = order.fees_accrued.safe_add(referrer_reward)?;
        // mirror into the market aggregate the fee sweep reserves (audit #73)
        market.accrue_pending_revenue_share(referrer_reward)?;
    }

    if user_fee != 0 || builder_fee != 0 {
        controller::position::update_quote_asset_and_break_even_amount(
            &mut taker.perp_positions[taker_position_index],
            market,
            -(user_fee.safe_add(builder_fee)?).cast()?,
        )?;
    }
    if maker_rebate != 0 {
        controller::position::update_quote_asset_and_break_even_amount(
            &mut taker.perp_positions[taker_position_index],
            market,
            maker_rebate.cast()?,
        )?;
    }

    if order_post_only {
        taker_stats.update_maker_volume_30d(taker_quote, now)?;
    } else {
        taker_stats.update_taker_volume_30d(taker_quote, now)?;
    }

    if let Some(filler_user) = filler.as_mut() {
        credit_filler_perp_pnl(
            filler_user,
            filler_stats,
            market,
            filler_reward,
            taker_quote,
            now,
            slot,
        )?;
    } else if let Some(maker_user) = maker.as_mut() {
        credit_filler_perp_pnl(
            maker_user,
            maker_stats,
            market,
            filler_reward,
            taker_quote,
            now,
            slot,
        )?;
    }

    // Update taker order BEFORE event emit.
    let is_taker_filled_after_this = update_order_after_fill(
        &mut taker.orders[taker_order_index],
        fill.base_filled,
        taker_quote,
    )?;
    if is_taker_filled_after_this {
        if let (Some(idx), Some(escrow)) = (builder_order_idx, rev_share_escrow.as_mut()) {
            let _ = escrow
                .get_order_mut(idx)
                .map(|o| o.add_bit_flag(RevenueShareOrderBitFlag::Completed));
        }
    }
    decrease_open_bids_and_asks(
        &mut taker.perp_positions[taker_position_index],
        &taker_direction,
        fill.base_filled,
        taker.orders[taker_order_index].update_open_bids_and_asks(),
    )?;

    let (taker_record_key, taker_record_order, maker_record_key, maker_record_order) =
        get_taker_and_maker_for_order_record(taker_key, &taker.orders[taker_order_index]);

    let order_action_explanation = if is_liquidation {
        OrderActionExplanation::Liquidation
    } else {
        OrderActionExplanation::OrderFilledWithAMM
    };
    let mut order_action_bit_flags: u8 = 0;
    order_action_bit_flags = set_order_bit_flag(
        order_action_bit_flags,
        taker.orders[taker_order_index].is_signed_msg(),
        OrderBitFlag::SignedMessage,
    );
    if taker.perp_positions[taker_position_index].is_isolated() {
        order_action_bit_flags = set_order_bit_flag(
            order_action_bit_flags,
            true,
            OrderBitFlag::IsIsolatedPosition,
        );
    }

    let (
        taker_existing_quote_entry_amount,
        taker_existing_base_asset_amount,
        maker_existing_quote_entry_amount,
        maker_existing_base_asset_amount,
    ) = {
        let (existing_quote_entry_amount, existing_base_asset_amount) =
            calculate_existing_position_fields_for_order_action(
                fill.base_filled,
                taker_existing_position_params_before,
            )?;
        if taker_record_key.is_some() {
            (
                existing_quote_entry_amount,
                existing_base_asset_amount,
                None,
                None,
            )
        } else {
            (
                None,
                None,
                existing_quote_entry_amount,
                existing_base_asset_amount,
            )
        }
    };

    emit_perp_action_record(
        market,
        oracle_map,
        now,
        order_action_explanation,
        filler_key,
        filler_reward,
        fill.base_filled,
        taker_quote,
        user_fee.safe_add(builder_fee)?,
        if maker_rebate != 0 {
            Some(maker_rebate)
        } else {
            None
        },
        referrer_reward,
        Some(taker_surplus),
        taker_record_key,
        taker_record_order,
        maker_record_key,
        maker_record_order,
        order_action_bit_flags,
        taker_existing_quote_entry_amount,
        taker_existing_base_asset_amount,
        maker_existing_quote_entry_amount,
        maker_existing_base_asset_amount,
        builder_idx,
        builder_fee_option,
    )?;

    Ok((fill.base_filled, taker_quote))
}

#[allow(clippy::too_many_arguments)]
/// Settle a single `DlobMatch` fill (taker vs a resting DLOB maker order).
/// Returns `(base_filled, quote_filled, maker_base_filled)` to accumulate.
fn settle_dlob_match_fill(
    fill: &QuoterFill,
    market: &mut PerpMarket,
    taker: &mut User,
    taker_stats: &mut UserStats,
    taker_position_index: usize,
    taker_order_index: usize,
    taker_key: &Pubkey,
    taker_direction: PositionDirection,
    taker_existing_position_params_before: Option<(u64, u64)>,
    maker: &mut Option<&mut User>,
    maker_stats: &mut Option<&mut UserStats>,
    maker_order_index: Option<usize>,
    maker_key_opt: Option<&Pubkey>,
    maker_existing_position_params: Option<(u64, u64)>,
    match_maker_price: Option<u64>,
    taker_price_for_match: Option<u64>,
    oracle_price: i64,
    filler: &mut Option<&mut User>,
    filler_stats: &mut Option<&mut UserStats>,
    filler_key: &Pubkey,
    rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
    referrer_is_accelerated: bool,
    fee_structure: &FeeStructure,
    oracle_map: &mut OracleMap,
    is_liquidation: bool,
    now: i64,
    slot: u64,
    promo_fee_tier: u8,
    builder_fee_allowed: bool,
    // Filler reward already paid by earlier legs of this same fill. The
    // time-based component of the reward is size-independent, so it is a
    // per-fill allowance the legs draw down rather than one each.
    filler_reward_paid: &mut u64,
) -> VelocityResult<(u64, u64, u64)> {
    let slot_clock = oracle_map.slot_clock;
    // DlobMatch fills only land from a Match step, which always
    // populates `match_maker_price`.
    let match_maker_price = match_maker_price.ok_or_else(print_error!(ErrorCode::DefaultError))?;
    let m_idx = maker_order_index.ok_or_else(print_error!(ErrorCode::DefaultError))?;
    let m_key = maker_key_opt.ok_or_else(print_error!(ErrorCode::DefaultError))?;
    let maker_user = maker
        .as_deref_mut()
        .ok_or_else(print_error!(ErrorCode::DefaultError))?;
    let maker_position_index = get_position_index(&maker_user.perp_positions, market.market_index)?;
    let maker_direction = maker_user.orders[m_idx].direction;
    let maker_order_has_jit_flag = maker_user.orders[m_idx].is_jit_maker();

    let taker_price_validate =
        taker_price_for_match.ok_or_else(print_error!(ErrorCode::DefaultError))?;
    validate_fill_price(
        fill.quote_filled,
        fill.base_filled,
        BASE_PRECISION_U64,
        taker_direction,
        taker_price_validate,
        true,
    )?;
    validate_fill_price(
        fill.quote_filled,
        fill.base_filled,
        BASE_PRECISION_U64,
        maker_direction,
        match_maker_price,
        false,
    )?;

    let maker_pd =
        get_position_delta_for_fill(fill.base_filled, fill.quote_filled, maker_direction)?;
    update_position_and_market(
        &mut maker_user.perp_positions[maker_position_index],
        market,
        &maker_pd,
    )?;

    if let Some(ms) = maker_stats.as_mut() {
        ms.update_maker_volume_30d(fill.quote_filled, now)?;
    } else {
        taker_stats.update_maker_volume_30d(fill.quote_filled, now)?;
    }

    let taker_pd =
        get_position_delta_for_fill(fill.base_filled, fill.quote_filled, taker_direction)?;
    update_position_and_market(
        &mut taker.perp_positions[taker_position_index],
        market,
        &taker_pd,
    )?;
    taker_stats.update_taker_volume_30d(fill.quote_filled, now)?;

    let reward_referrer =
        can_reward_user_with_referral_reward(market.market_index, rev_share_escrow);
    // A maker that cranks its own fill arrives as `filler: None` with
    // `filler_key` naming itself: it is already loaded in the maker map, and
    // the same account cannot be loaded mutably twice (see `is_filler_maker`
    // in `fill_perp_order_with_router`). It did the keeper's work on a slice it
    // actually filled, so it earns the reward for that slice -- which spreads a
    // multi-maker fill's reward pro rata, since each slice's reward is computed
    // from the base that slice filled. A taker filling its own order names
    // *itself*, so this stays false and no reward is charged at all.
    let maker_is_filler = filler_key == m_key;
    let reward_filler =
        can_reward_user_with_perp_pnl(filler, market.market_index) || maker_is_filler;

    let (builder_order_idx, referrer_builder_order_idx, builder_order_fee_bps, builder_idx) =
        get_builder_escrow_info(
            rev_share_escrow,
            taker.sub_account_id,
            taker.orders[taker_order_index].order_id,
            market.market_index,
            taker.orders[taker_order_index].is_has_builder(),
            builder_fee_allowed,
        );

    let filler_multiplier = if reward_filler {
        calculate_filler_multiplier_for_matched_orders(
            match_maker_price,
            maker_direction,
            oracle_price,
        )?
    } else {
        0
    };

    let FillFees {
        user_fee: taker_fee,
        maker_rebate,
        fee_to_market,
        filler_reward,
        referrer_reward,
        referee_discount,
        builder_fee: builder_fee_option,
        protocol_fee,
        if_fee,
        amm_fee,
        ..
    } = fees::calculate_fee_for_fulfillment_with_match(
        taker_stats,
        maker_stats,
        fill.quote_filled,
        fee_structure,
        taker.orders[taker_order_index].slot,
        slot,
        filler_multiplier,
        reward_referrer,
        referrer_is_accelerated,
        &MarketType::Perp,
        market.fee_adjustment,
        builder_order_fee_bps,
        market.taker_fee_addon_tenth_bps,
        now,
        promo_fee_tier,
        slot_clock,
        *filler_reward_paid,
    )?;
    *filler_reward_paid = filler_reward_paid.saturating_add(filler_reward);
    let builder_fee = builder_fee_option.unwrap_or(0);

    if builder_fee != 0 {
        if let (Some(idx), Some(escrow)) = (builder_order_idx, rev_share_escrow.as_deref_mut()) {
            let order = escrow.get_order_mut(idx)?;
            order.fees_accrued = order.fees_accrued.safe_add(builder_fee)?;
            // mirror the per-order accrual into the market aggregate the fee
            // sweep reserves (audit #73)
            market.accrue_pending_revenue_share(builder_fee)?;
        } else {
            validate!(
                false,
                ErrorCode::UnableToLoadRevenueShareAccount,
                "Order has builder fee but no escrow account found"
            )?;
        }
    }

    // gross taker fee for the analytics counter; protocol / IF / AMM carveouts
    // accrue to pending counters, materialized out of the pnl pool by
    // `sweep_market_fees`. The AMM's provision (`fee_to_market == amm_fee`) is
    // also credited to the AMM books at fill — tokens follow at the sweep's
    // tokenization step — and grows its backstop-of-last-resort clawback cap.
    market
        .fee_ledger
        .accrue_fill_fees(taker_fee, protocol_fee, if_fee, amm_fee)?;
    if amm_fee > 0 {
        <crate::vlp::amm::AMM as crate::vlp::amm::quoter::AmmContract>::apply_fill_fees(
            &mut market.amm,
            fee_to_market,
            0,
        )?;
    }

    controller::position::update_quote_asset_and_break_even_amount(
        &mut taker.perp_positions[taker_position_index],
        market,
        -(taker_fee.safe_add(builder_fee)?).cast()?,
    )?;

    taker_stats.increment_total_fees(taker_fee)?;
    taker_stats.increment_total_referee_discount(referee_discount)?;

    controller::position::update_quote_asset_and_break_even_amount(
        &mut maker_user.perp_positions[maker_position_index],
        market,
        maker_rebate.cast()?,
    )?;

    if let Some(ms) = maker_stats.as_mut() {
        ms.increment_total_rebate(maker_rebate)?;
    } else {
        taker_stats.increment_total_rebate(maker_rebate)?;
    }

    if let Some(filler_user) = filler.as_mut() {
        if filler_reward > 0 {
            let filler_position_index =
                get_position_index(&filler_user.perp_positions, market.market_index).or_else(
                    |_| add_new_position(&mut filler_user.perp_positions, market.market_index),
                )?;
            controller::position::update_quote_asset_amount(
                &mut filler_user.perp_positions[filler_position_index],
                market,
                filler_reward.cast()?,
            )?;
            filler_stats
                .as_mut()
                .safe_unwrap()?
                .update_filler_volume(fill.quote_filled, now)?;
        }
        filler_user.update_last_active_slot(slot);
    } else if maker_is_filler {
        credit_filler_perp_pnl(
            maker_user,
            maker_stats,
            market,
            filler_reward,
            fill.quote_filled,
            now,
            slot,
        )?;
    }

    if let (Some(idx), Some(escrow)) = (referrer_builder_order_idx, rev_share_escrow.as_deref_mut())
    {
        let order = escrow.get_order_mut(idx)?;
        order.fees_accrued = order.fees_accrued.safe_add(referrer_reward)?;
        // mirror into the market aggregate the fee sweep reserves (audit #73)
        market.accrue_pending_revenue_share(referrer_reward)?;
    }

    // Update taker order BEFORE event emit.
    let is_taker_filled_after_this = update_order_after_fill(
        &mut taker.orders[taker_order_index],
        fill.base_filled,
        fill.quote_filled,
    )?;
    if is_taker_filled_after_this {
        if let (Some(idx), Some(escrow)) = (builder_order_idx, rev_share_escrow.as_deref_mut()) {
            let _ = escrow
                .get_order_mut(idx)
                .map(|o| o.add_bit_flag(RevenueShareOrderBitFlag::Completed));
        }
    }
    decrease_open_bids_and_asks(
        &mut taker.perp_positions[taker_position_index],
        &taker_direction,
        fill.base_filled,
        taker.orders[taker_order_index].update_open_bids_and_asks(),
    )?;

    // Maker open-bids/asks bookkeeping. commit_fill already
    // updated maker order's filled counters; we only need open
    // bids/asks decrement + status flip.
    decrease_open_bids_and_asks(
        &mut maker_user.perp_positions[maker_position_index],
        &maker_direction,
        fill.base_filled,
        maker_user.orders[m_idx].update_open_bids_and_asks(),
    )?;
    if maker_user.orders[m_idx].get_base_asset_amount_unfilled(None)? == 0 {
        maker_user.orders[m_idx].status = OrderStatus::Filled;
    }

    let order_action_explanation = if is_liquidation {
        OrderActionExplanation::Liquidation
    } else if maker_order_has_jit_flag {
        OrderActionExplanation::OrderFilledWithMatchJit
    } else {
        OrderActionExplanation::OrderFilledWithMatch
    };
    let mut order_action_bit_flags: u8 = 0;
    order_action_bit_flags = set_order_bit_flag(
        order_action_bit_flags,
        taker.orders[taker_order_index].is_signed_msg(),
        OrderBitFlag::SignedMessage,
    );
    if taker.perp_positions[taker_position_index].is_isolated()
        || maker_user.perp_positions[maker_position_index].is_isolated()
    {
        order_action_bit_flags = set_order_bit_flag(
            order_action_bit_flags,
            true,
            OrderBitFlag::IsIsolatedPosition,
        );
    }

    let (taker_existing_quote_entry_amount, taker_existing_base_asset_amount) =
        calculate_existing_position_fields_for_order_action(
            fill.base_filled,
            taker_existing_position_params_before,
        )?;
    let (maker_existing_quote_entry_amount, maker_existing_base_asset_amount) =
        calculate_existing_position_fields_for_order_action(
            fill.base_filled,
            maker_existing_position_params,
        )?;
    let taker_order_for_record = taker.orders[taker_order_index];
    let maker_order_for_record = maker_user.orders[m_idx];
    let m_key_owned = *m_key;
    emit_perp_action_record(
        market,
        oracle_map,
        now,
        order_action_explanation,
        filler_key,
        filler_reward,
        fill.base_filled,
        fill.quote_filled,
        taker_fee.safe_add(builder_fee)?,
        Some(maker_rebate),
        referrer_reward,
        None,
        Some(*taker_key),
        Some(taker_order_for_record),
        Some(m_key_owned),
        Some(maker_order_for_record),
        order_action_bit_flags,
        taker_existing_quote_entry_amount,
        taker_existing_base_asset_amount,
        maker_existing_quote_entry_amount,
        maker_existing_base_asset_amount,
        builder_idx,
        builder_fee_option,
    )?;

    Ok((fill.base_filled, fill.quote_filled, fill.base_filled))
}

/// Settle one external-quoter balance change: the maker is a loaded `User`
/// whose resting liquidity lives outside velocity (a CLOB order or a PropAMM
/// quote), so unlike [`settle_dlob_match_fill`] there is no velocity `Order`
/// to update — the external program already committed its own book state.
/// Everything protocol-level is identical to a DLOB match: match-fee
/// schedule (maker rebate), position updates on both sides, filler reward,
/// referrer/builder accrual, and the fill record (with no maker order).
///
/// The maker side has no `validate_fill_price` — the router pass already
/// enforced per-unit at-or-better against this quoter's own quoted levels,
/// which is the maker-side price contract here. The taker side validates
/// against the effective taker limit as usual.
///
/// `maker_aggregates_tracked`: CLOB orders are margin-reserved through
/// velocity at placement (`open_bids`/`open_asks`), so their fills unwind
/// those aggregates; Custom PropAMM depth is never reserved.
#[allow(clippy::too_many_arguments)]
fn settle_external_match_fill(
    base_filled: u64,
    quote_filled: u64,
    market: &mut PerpMarket,
    taker: &mut User,
    taker_stats: &mut UserStats,
    taker_position_index: usize,
    taker_order_index: usize,
    taker_key: &Pubkey,
    taker_direction: PositionDirection,
    taker_existing_position_params_before: Option<(u64, u64)>,
    maker_user: &mut User,
    mut maker_stats: Option<&mut UserStats>,
    maker_key: &Pubkey,
    maker_aggregates_tracked: bool,
    // The maker's own id for the order this fill came off, when the response
    // named exactly one. It is what lets the fill record attribute to a book
    // order; `None` when the change merged several and no single order owns it.
    maker_order_id: Option<u32>,
    taker_limit_price: Option<u64>,
    oracle_price: i64,
    filler: &mut Option<&mut User>,
    filler_stats: &mut Option<&mut UserStats>,
    filler_key: &Pubkey,
    rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
    referrer_is_accelerated: bool,
    fee_structure: &FeeStructure,
    oracle_map: &mut OracleMap,
    is_liquidation: bool,
    now: i64,
    slot: u64,
    promo_fee_tier: u8,
    builder_fee_allowed: bool,
    // Filler reward already paid by earlier legs of this same fill. The
    // time-based component of the reward is size-independent, so it is a
    // per-fill allowance the legs draw down rather than one each.
    filler_reward_paid: &mut u64,
) -> VelocityResult<(u64, u64)> {
    let maker_direction = taker_direction.opposite();
    let maker_position_index = get_position_index(&maker_user.perp_positions, market.market_index)
        .or_else(|_| add_new_position(&mut maker_user.perp_positions, market.market_index))?;
    let maker_existing_position_params = maker_user.perp_positions[maker_position_index]
        .get_existing_position_params_for_order_action(maker_direction);

    if let Some(limit) = taker_limit_price {
        validate_fill_price(
            quote_filled,
            base_filled,
            BASE_PRECISION_U64,
            taker_direction,
            limit,
            true,
        )?;
    }

    let maker_pd = get_position_delta_for_fill(base_filled, quote_filled, maker_direction)?;
    update_position_and_market(
        &mut maker_user.perp_positions[maker_position_index],
        market,
        &maker_pd,
    )?;
    if let Some(ms) = maker_stats.as_mut() {
        ms.update_maker_volume_30d(quote_filled, now)?;
    } else {
        taker_stats.update_maker_volume_30d(quote_filled, now)?;
    }

    let taker_pd = get_position_delta_for_fill(base_filled, quote_filled, taker_direction)?;
    update_position_and_market(
        &mut taker.perp_positions[taker_position_index],
        market,
        &taker_pd,
    )?;
    taker_stats.update_taker_volume_30d(quote_filled, now)?;

    let reward_referrer =
        can_reward_user_with_referral_reward(market.market_index, rev_share_escrow);
    let reward_filler = can_reward_user_with_perp_pnl(filler, market.market_index);

    let (builder_order_idx, referrer_builder_order_idx, builder_order_fee_bps, builder_idx) =
        get_builder_escrow_info(
            rev_share_escrow,
            taker.sub_account_id,
            taker.orders[taker_order_index].order_id,
            market.market_index,
            taker.orders[taker_order_index].is_has_builder(),
            builder_fee_allowed,
        );

    // The maker's per-unit price for the filler-reward tier — external fills
    // have no single maker limit, so the average fill price stands in.
    let avg_fill_price = quote_filled
        .cast::<u128>()?
        .safe_mul(BASE_PRECISION_U64.cast()?)?
        .safe_div(base_filled.cast()?)?
        .cast::<u64>()?;
    let filler_multiplier = if reward_filler {
        calculate_filler_multiplier_for_matched_orders(
            avg_fill_price,
            maker_direction,
            oracle_price,
        )?
    } else {
        0
    };

    let FillFees {
        user_fee: taker_fee,
        maker_rebate,
        fee_to_market,
        filler_reward,
        referrer_reward,
        referee_discount,
        builder_fee: builder_fee_option,
        protocol_fee,
        if_fee,
        amm_fee,
        ..
    } = fees::calculate_fee_for_fulfillment_with_match(
        taker_stats,
        &maker_stats,
        quote_filled,
        fee_structure,
        taker.orders[taker_order_index].slot,
        slot,
        filler_multiplier,
        reward_referrer,
        referrer_is_accelerated,
        &MarketType::Perp,
        market.fee_adjustment,
        builder_order_fee_bps,
        market.taker_fee_addon_tenth_bps,
        now,
        promo_fee_tier,
        oracle_map.slot_clock,
        *filler_reward_paid,
    )?;
    *filler_reward_paid = filler_reward_paid.saturating_add(filler_reward);
    let builder_fee = builder_fee_option.unwrap_or(0);

    if builder_fee != 0 {
        if let (Some(idx), Some(escrow)) = (builder_order_idx, rev_share_escrow.as_deref_mut()) {
            let order = escrow.get_order_mut(idx)?;
            order.fees_accrued = order.fees_accrued.safe_add(builder_fee)?;
            market.accrue_pending_revenue_share(builder_fee)?;
        } else {
            validate!(
                false,
                ErrorCode::UnableToLoadRevenueShareAccount,
                "Order has builder fee but no escrow account found"
            )?;
        }
    }

    market
        .fee_ledger
        .accrue_fill_fees(taker_fee, protocol_fee, if_fee, amm_fee)?;
    if amm_fee > 0 {
        <crate::vlp::amm::AMM as crate::vlp::amm::quoter::AmmContract>::apply_fill_fees(
            &mut market.amm,
            fee_to_market,
            0,
        )?;
    }

    controller::position::update_quote_asset_and_break_even_amount(
        &mut taker.perp_positions[taker_position_index],
        market,
        -(taker_fee.safe_add(builder_fee)?).cast()?,
    )?;
    taker_stats.increment_total_fees(taker_fee)?;
    taker_stats.increment_total_referee_discount(referee_discount)?;

    controller::position::update_quote_asset_and_break_even_amount(
        &mut maker_user.perp_positions[maker_position_index],
        market,
        maker_rebate.cast()?,
    )?;
    if let Some(ms) = maker_stats.as_mut() {
        ms.increment_total_rebate(maker_rebate)?;
    } else {
        taker_stats.increment_total_rebate(maker_rebate)?;
    }

    if let Some(filler_user) = filler.as_mut() {
        if filler_reward > 0 {
            let filler_position_index =
                get_position_index(&filler_user.perp_positions, market.market_index).or_else(
                    |_| add_new_position(&mut filler_user.perp_positions, market.market_index),
                )?;
            controller::position::update_quote_asset_amount(
                &mut filler_user.perp_positions[filler_position_index],
                market,
                filler_reward.cast()?,
            )?;
            filler_stats
                .as_mut()
                .safe_unwrap()?
                .update_filler_volume(quote_filled, now)?;
        }
        filler_user.update_last_active_slot(slot);
    }

    if let (Some(idx), Some(escrow)) = (referrer_builder_order_idx, rev_share_escrow.as_deref_mut())
    {
        let order = escrow.get_order_mut(idx)?;
        order.fees_accrued = order.fees_accrued.safe_add(referrer_reward)?;
        market.accrue_pending_revenue_share(referrer_reward)?;
    }

    let is_taker_filled_after_this = update_order_after_fill(
        &mut taker.orders[taker_order_index],
        base_filled,
        quote_filled,
    )?;
    if is_taker_filled_after_this {
        if let (Some(idx), Some(escrow)) = (builder_order_idx, rev_share_escrow.as_deref_mut()) {
            let _ = escrow
                .get_order_mut(idx)
                .map(|o| o.add_bit_flag(RevenueShareOrderBitFlag::Completed));
        }
    }
    decrease_open_bids_and_asks(
        &mut taker.perp_positions[taker_position_index],
        &taker_direction,
        base_filled,
        taker.orders[taker_order_index].update_open_bids_and_asks(),
    )?;
    decrease_open_bids_and_asks(
        &mut maker_user.perp_positions[maker_position_index],
        &maker_direction,
        base_filled,
        maker_aggregates_tracked,
    )?;

    let order_action_explanation = if is_liquidation {
        OrderActionExplanation::Liquidation
    } else {
        OrderActionExplanation::OrderFilledWithExternalQuoter
    };
    let mut order_action_bit_flags: u8 = 0;
    order_action_bit_flags = set_order_bit_flag(
        order_action_bit_flags,
        taker.orders[taker_order_index].is_signed_msg(),
        OrderBitFlag::SignedMessage,
    );
    if taker.perp_positions[taker_position_index].is_isolated()
        || maker_user.perp_positions[maker_position_index].is_isolated()
    {
        order_action_bit_flags = set_order_bit_flag(
            order_action_bit_flags,
            true,
            OrderBitFlag::IsIsolatedPosition,
        );
    }

    let (taker_existing_quote_entry_amount, taker_existing_base_asset_amount) =
        calculate_existing_position_fields_for_order_action(
            base_filled,
            taker_existing_position_params_before,
        )?;
    let (maker_existing_quote_entry_amount, maker_existing_base_asset_amount) =
        calculate_existing_position_fields_for_order_action(
            base_filled,
            maker_existing_position_params,
        )?;
    let taker_order_for_record = taker.orders[taker_order_index];
    emit_perp_action_record(
        market,
        oracle_map,
        now,
        order_action_explanation,
        filler_key,
        filler_reward,
        base_filled,
        quote_filled,
        taker_fee.safe_add(builder_fee)?,
        Some(maker_rebate),
        referrer_reward,
        None,
        Some(*taker_key),
        Some(taker_order_for_record),
        Some(*maker_key),
        maker_order_id.map(|order_id| Order {
            order_id,
            market_index: market.market_index,
            market_type: MarketType::Perp,
            direction: maker_direction,
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            post_only: true,
            bit_flags: OrderBitFlag::PlacedOnClob as u8,
            ..Order::default()
        }),
        order_action_bit_flags,
        taker_existing_quote_entry_amount,
        taker_existing_base_asset_amount,
        maker_existing_quote_entry_amount,
        maker_existing_base_asset_amount,
        builder_idx,
        builder_fee_option,
    )?;

    Ok((base_filled, quote_filled))
}

/// Fulfillment: one pass over every liquidity source.
///
/// Quote (sanitized DLOB makers as single-level books, external CPI books,
/// the vAMM ladder last with everything else as its last look), split the
/// taker's unfilled size across the union by priority tier, then execute and
/// settle each allocation through the fee-policy-keyed settle functions.
///
/// One quote/route pass rather than the route-then-quote-per-method loop this
/// replaced, which is why there is no scratch-AMM projection (routing and
/// quoting see the same curve), no separate JIT participant (the vAMM's
/// last-look shading is its general form), and no per-step fallback recompute
/// (one effective taker limit bounds every book up front).
///
/// External books are priced into the split; allocations that land on them
/// execute through `RouterFillInputs::executor` (the CPI leg the fill
/// entrypoint supplies) and settle per returned balance change against the
/// loaded makers. Maker prices are the sanitized frozen prices from
/// discovery; `DlobOrderQuoter::execute` requotes off the same
/// oracle/slot/tick so the two agree by construction, and
/// `settle_dlob_match_fill`'s `validate_fill_price` enforces it.
#[allow(clippy::too_many_arguments)]
fn fulfill_perp_order_router_pass(
    taker: &mut User,
    taker_order_index: usize,
    taker_key: &Pubkey,
    taker_stats: &mut UserStats,
    makers_and_referrer: &UserMap,
    makers_and_referrer_stats: &UserStatsMap,
    maker_orders_info: &[(Pubkey, usize, u64)],
    filler: &mut Option<&mut User>,
    filler_key: &Pubkey,
    filler_stats: &mut Option<&mut UserStats>,
    spot_market_map: &SpotMarketMap,
    perp_market_map: &PerpMarketMap,
    oracle_map: &mut OracleMap,
    validity_guard_rails: &ValidityGuardRails,
    fee_structure: &FeeStructure,
    taker_limit_price: Option<u64>,
    now: i64,
    slot: u64,
    amm_is_available: bool,
    is_liquidation: bool,
    router: &mut crate::math::router::RouterFillInputs,
    rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
    referrer_is_accelerated: bool,
    vamm_maker_rebate: bool,
    promo_fee_tier: u8,
    // False when the taker does not meet initial margin. The fill proceeds and
    // charges no builder fee. `fulfill_perp_order` computes it and documents
    // the rule.
    builder_fee_allowed: bool,
    maker_fills: &mut BTreeMap<Pubkey, (i64, bool)>,
) -> VelocityResult<(u64, u64)> {
    use crate::{
        math::router::{split_across_quoters, QuoterBook},
        state::{
            prop_amm::{Direction, PriceLevel, QuoterType},
            quoter::RouterQuoter,
        },
        vlp::amm::router_adapter::vamm_quote_levels,
    };

    let external_books = router.books;

    let market_index = taker.orders[taker_order_index].market_index;

    // ---- Taker order fields (mirrors the step's capture). ----
    let taker_position_index = get_position_index(&taker.perp_positions, market_index)?;
    let taker_existing_position_before =
        taker.perp_positions[taker_position_index].base_asset_amount;
    let taker_existing_position_params_before = taker.perp_positions[taker_position_index]
        .get_existing_position_params_for_order_action(taker.orders[taker_order_index].direction);
    let (order_post_only, order_slot, taker_direction, order_id) = get_struct_values!(
        taker.orders[taker_order_index],
        post_only,
        slot,
        direction,
        order_id
    );
    let maker_direction = taker_direction.opposite();
    let direction = match taker_direction {
        PositionDirection::Long => Direction::Long,
        PositionDirection::Short => Direction::Short,
    };

    let target_size = taker.orders[taker_order_index]
        .get_base_asset_amount_unfilled(Some(taker_existing_position_before))?;
    if target_size == 0 {
        return Ok((0, 0));
    }

    // Resolves a wire user reference against the loaded set. Built here
    // rather than at the settle loop below because the CLOB depth clamp
    // needs it too, and the index is a snapshot of identities that no fill
    // changes.
    let user_ref_index = makers_and_referrer.user_ref_index()?;
    let taker_ref = crate::state::prop_amm::ClobUserRefV0 {
        authority: taker.authority,
        sub_account_id: taker.sub_account_id.into(),
    };
    let protocol_authority = router.protocol_authority;
    // One bit per loaded user, in the order the map holds them. Set as each
    // balance change settles, so the obligation check at the end of the pass
    // can name a loaded user that did nothing. The wire carries at most
    // `MAX_QUOTER_WIRE_USERS` users, which is inside a `u64`.
    let mut settled_users: u64 = 0;
    let mut mark_settled = |key: &Pubkey| {
        if let Some(index) = makers_and_referrer.0.keys().position(|held| held == key) {
            if index < u64::BITS as usize {
                settled_users |= 1u64 << index;
            }
        }
    };
    let resolve_user = |user: &crate::state::prop_amm::ClobUserRefV0| -> VelocityResult<Pubkey> {
        user_ref_index
            .get(&(user.authority, user.sub_account_id))
            .copied()
            .ok_or_else(|| {
                msg!(
                    "quoter returned a balance change for an unloaded user {}/{}",
                    user.authority,
                    user.sub_account_id
                );
                ErrorCode::DefaultError
            })
    };

    // ---- Pre-execute depth clamps on external books. ----
    // Both clamps answer one problem: size the split routes to a book that
    // the post-fill checks then refuse fails the whole fill, taking the
    // taker and every other maker in the transaction with it. Both cut a
    // prefix off the book's own quoted ladder, so what survives is still
    // depth that quoter promised and the `ext_base == allocation.base` bound
    // below still holds.
    //
    // Custom: a PropAMM's depth is never margin-reserved, so the cap is what
    // the quoted user's account supports right now.
    //
    // A CLOB needs no clamp: its makers are sized before the books are
    // quoted and the book passes over anyone out of room, mid-book, so the
    // depth behind them is still quoted and still fillable
    // (`build_user_caps`). Truncating in front of them, which is all a clamp
    // out here can do, would cost every order behind.
    //
    // Runs before this market's `RefMut` is taken, because the margin walk
    // values every market the quoted user touches.
    let clamped_books: Vec<Option<Vec<PriceLevel>>> = (0..external_books.len())
        .map(|i| -> VelocityResult<Option<Vec<PriceLevel>>> {
            let cap = match router.executor.quoter_type(i) {
                QuoterType::Custom => {
                    let quoter_user_key = router.executor.quoter_user(i);
                    // A quoter quoting for the taker themselves is a self-trade.
                    if quoter_user_key == *taker_key {
                        return Ok(Some(vec![]));
                    }
                    let position_index = {
                        let mut maker = makers_and_referrer.get_ref_mut(&quoter_user_key)?;
                        get_position_index(&maker.perp_positions, market_index).or_else(|_| {
                            add_new_position(&mut maker.perp_positions, market_index)
                        })?
                    };
                    let maker = makers_and_referrer.get_ref(&quoter_user_key)?;
                    crate::math::orders::calculate_max_perp_order_size(
                        &maker,
                        position_index,
                        market_index,
                        maker_direction,
                        perp_market_map,
                        spot_market_map,
                        oracle_map,
                    )?
                }
                QuoterType::Clob | QuoterType::Vamm => return Ok(None),
            };
            let depth = external_books[i]
                .levels
                .iter()
                .fold(0u64, |total, level| total.saturating_add(level.size));
            if depth <= cap {
                return Ok(None);
            }
            let mut remaining = cap;
            let levels = external_books[i]
                .levels
                .iter()
                .map_while(|level| {
                    if remaining == 0 {
                        return None;
                    }
                    let size = level.size.min(remaining);
                    remaining -= size;
                    Some(PriceLevel {
                        price: level.price,
                        size: size.into(),
                    })
                })
                .collect();
            Ok(Some(levels))
        })
        .collect::<VelocityResult<_>>()?;
    let external_levels = |i: usize| -> &[PriceLevel] {
        clamped_books[i]
            .as_deref()
            .unwrap_or(external_books[i].levels)
    };

    let mut market = perp_market_map.get_ref_mut(&market_index)?;

    // ---- Oracle context + AMM refresh (shared with the quote view). ----
    let oracle_pd = *oracle_map.get_price_data(&market.oracle_id())?;
    let oracle_price = oracle_pd.price;
    let quote_inputs = QuoteInputs::load(
        &market,
        oracle_pd,
        slot,
        validity_guard_rails,
        oracle_map.slot_clock,
    )?;
    let sanitize_clamp_denom = quote_inputs.sanitize_clamp_denominator;
    let order_tick_size = quote_inputs.tick_size;
    let order_step_size = quote_inputs.step_size;
    let setup_ctx = quote_inputs.ctx(slot);
    let market_fee_adjustment = market.fee_adjustment;
    let mut amm_quoter = AmmQuoter::for_amm(&mut market.amm);
    amm_quoter.refresh(&setup_ctx)?;
    let reserve_after_setup = amm_quoter.amm.reserve_price()?;
    let (amm_bid_price, amm_ask_price) = amm_quoter.amm_bid_ask(reserve_after_setup)?;
    let amm_base_spread = amm_quoter.amm_base_spread();
    let amm_long_spread = amm_quoter.amm.long_spread;
    let amm_short_spread = amm_quoter.amm.short_spread;

    // The vAMM gets a *tighter* ceiling than the maker books do, and it is
    // not cosmetic: a post-only taker acts as a maker, so its limit is
    // buffered by the maker rebate it earns and stepped one tick inside the
    // limit (`calculate_effective_amm_taker_limit`). Bounding the ladder by
    // the raw limit instead lets a post-only order sweep past the buffer —
    // more size, and every unit priced at the raw limit rather than the
    // buffered one, which is LP value handed to the taker. Maker books keep
    // the raw limit: the buffer is the AMM's, not theirs.
    let amm_taker_limit = crate::math::orders::calculate_effective_amm_taker_limit(
        &taker.orders[taker_order_index],
        taker_limit_price,
        None,
        &crate::math::fees::determine_user_fee_tier(
            taker_stats,
            fee_structure,
            &MarketType::Perp,
            now,
            promo_fee_tier,
        )?,
        market_fee_adjustment,
        order_tick_size,
    )?;

    // One effective limit bounds every book. Market orders fall back to the
    // AMM fallback price so a router sweep stays price-bounded, exactly like
    // the legacy match legs.
    let effective_taker_limit = match taker_limit_price {
        Some(price) => Some(price),
        None => {
            let amm_ref: &crate::vlp::amm::AMM = amm_quoter.amm;
            let amm_available =
                calculate_amm_available_liquidity(amm_ref, &taker_direction, order_step_size)?;
            Some(amm_ref.get_fallback_price(
                &quote_inputs.stats,
                &taker_direction,
                amm_available,
                oracle_price,
                taker.orders[taker_order_index].seconds_til_expiry(now),
                quote_inputs.stats.min_order_size,
            )?)
        }
    };
    let within_limit = |levels: &[PriceLevel]| -> usize {
        let Some(limit) = effective_taker_limit else {
            return levels.len();
        };
        levels
            .iter()
            .position(|level| match taker_direction {
                PositionDirection::Long => level.price > limit,
                PositionDirection::Short => level.price < limit,
            })
            .unwrap_or(levels.len())
    };

    // ---- Quote: maker books as plain data (frozen sanitized prices). ----
    struct RouterMaker {
        key: Pubkey,
        order_index: usize,
        price: u64,
        unfilled: u64,
        is_isolated: bool,
    }
    let mut router_makers: Vec<RouterMaker> = Vec::with_capacity(maker_orders_info.len());
    for (maker_key, maker_order_index, maker_price) in maker_orders_info {
        let maker = makers_and_referrer.get_ref(maker_key)?;
        let position = maker.get_perp_position(market_index)?;
        let unfilled = maker.orders[*maker_order_index]
            .get_base_asset_amount_unfilled(Some(position.base_asset_amount))?;
        if unfilled == 0 {
            continue;
        }
        router_makers.push(RouterMaker {
            key: *maker_key,
            order_index: *maker_order_index,
            price: *maker_price,
            unfilled,
            is_isolated: position.is_isolated(),
        });
    }
    let maker_levels: Vec<[PriceLevel; 1]> = router_makers
        .iter()
        .map(|maker| {
            [PriceLevel {
                price: maker.price.into(),
                size: maker.unfilled.into(),
            }]
        })
        .collect();

    // The vAMM quotes last: every other book is its last look.
    let clob_tier = QuoterType::Clob.default_priority();
    let amm_levels: Vec<PriceLevel> = if amm_is_available {
        let rivals: Vec<QuoterBook> = external_books
            .iter()
            .enumerate()
            .map(|(i, book)| {
                let levels = external_levels(i);
                QuoterBook {
                    priority: book.priority,
                    levels: &levels[..within_limit(levels)],
                    withheld: book.withheld,
                }
            })
            .chain(maker_levels.iter().map(|levels| QuoterBook {
                priority: clob_tier,
                levels: &levels[..within_limit(levels.as_slice())],
                withheld: PriceLevel::default(),
            }))
            .collect();
        vamm_quote_levels(
            amm_quoter.amm,
            direction,
            target_size,
            order_step_size,
            &rivals,
            // Fall back to the shared limit when the order has no limit of
            // its own (a market order): `amm_taker_limit` is None then.
            amm_taker_limit.or(effective_taker_limit),
        )?
    } else {
        vec![]
    };

    // ---- Split across the union, all books truncated at the limit. ----
    let books: Vec<QuoterBook> = external_books
        .iter()
        .enumerate()
        .map(|(i, book)| {
            let levels = external_levels(i);
            QuoterBook {
                priority: book.priority,
                levels: &levels[..within_limit(levels)],
                withheld: book.withheld,
            }
        })
        .chain(maker_levels.iter().map(|levels| QuoterBook {
            priority: clob_tier,
            levels: &levels[..within_limit(levels.as_slice())],
            withheld: PriceLevel::default(),
        }))
        // The vAMM book is NOT re-truncated: `vamm_quote_levels` already
        // capped the ladder at the limit, and its per-rung prices are
        // rounded slice averages — comparing those to the limit would drop
        // dust rungs whose true cost is inside it.
        .chain(core::iter::once(QuoterBook {
            priority: QuoterType::Vamm.default_priority(),
            levels: &amm_levels,
            withheld: PriceLevel::default(),
        }))
        .collect();
    // A book that stops its walk at an order whose owner this transaction does
    // not carry reports the depth behind it as withheld. Velocity holds back no
    // taker size for it: the taker asked to trade, the rest of the route can
    // fill, and the aged order keeps its place in the queue because the book
    // stopped its own walk. What the withheld report drives instead is the
    // obligation check at the end of this pass, which asks whether the party
    // that built the transaction could have carried that owner.
    let withheld_depth = external_books
        .iter()
        .any(|book| book.withheld.price != 0 && book.withheld.size != 0);
    let allocations = split_across_quoters(direction, target_size, &books, order_step_size)?;
    let externals_end = external_books.len();
    let makers_end = externals_end + maker_levels.len();

    // ---- Execute the vAMM allocation, then release &mut market.amm. ----
    let amm_allocation = allocations[makers_end];
    let amm_fill = if amm_allocation.base > 0 {
        let fill =
            RouterQuoter::execute(&mut amm_quoter, &setup_ctx, direction, amm_allocation.base)?;
        validate!(
            fill.base_filled <= amm_allocation.base,
            ErrorCode::DefaultError,
            "router vAMM overfilled: {} > {}",
            fill.base_filled,
            amm_allocation.base
        )?;
        validate!(
            crate::controller::matching::fill_at_or_better(
                taker_direction,
                &fill,
                &amm_allocation,
                BASE_PRECISION_U64
            )?,
            ErrorCode::DefaultError,
            "router vAMM filled worse than quoted: fill {}/{} vs quoted {}/{}",
            fill.quote_filled,
            fill.base_filled,
            amm_allocation.quote,
            amm_allocation.base
        )?;
        (fill.base_filled > 0).then_some(fill)
    } else {
        None
    };
    let _ = amm_quoter;

    // ---- Execute + settle each maker allocation. ----
    // Re-quote against the *same* oracle price discovery froze the book at
    // (the MM price), not the confidence-bounded safe price. An oracle-offset
    // maker prices off `ctx.oracle`, so using a different price here would
    // re-quote it away from its quoted level and trip the at-or-better check —
    // failing the whole fill closed instead of filling.
    let discovery_oracle = OraclePriceData {
        price: quote_inputs.mm_oracle.get_price(),
        ..quote_inputs.safe_oracle
    };
    let ctx = QuoteContext {
        stats: &quote_inputs.stats,
        oracle: &discovery_oracle,
        mm_oracle: None,
        oracle_validity: None,
        fee_budget: 0,
        tick: order_tick_size,
        step_size: order_step_size,
        slot,
        slot_clock: oracle_map.slot_clock,
        base_precision: BASE_PRECISION_U64,
        market_status: MarketStatus::default(),
        market_config: 0,
    };
    let mut total_base = 0u64;
    let mut total_quote = 0u64;
    // The filler reward's time-based component is size-independent, so it is
    // an allowance for the whole fill: each leg draws down what earlier legs
    // already paid instead of being granted it afresh. Otherwise a taker
    // crossing N sources pays that component N times.
    let mut filler_reward_paid = 0_u64;
    for (i, router_maker) in router_makers.iter().enumerate() {
        let allocation = allocations[externals_end + i];
        if allocation.base == 0 {
            continue;
        }
        let mut maker = makers_and_referrer.get_ref_mut(&router_maker.key)?;
        // The settle helpers update positions directly, so the maker's
        // funding must be current first — same pre-flight every match path
        // runs (a stale stamp fails `update_position_and_market`'s check).
        settle_funding_payment(&mut maker, &router_maker.key, market.deref_mut(), now)?;
        let maker_existing_position_params = maker
            .get_perp_position(market_index)?
            .get_existing_position_params_for_order_action(maker_direction);
        let fill = {
            let mut dlob = DlobOrderQuoter::new(
                &mut maker.orders[router_maker.order_index],
                router_maker.unfilled,
            );
            RouterQuoter::execute(&mut dlob, &ctx, direction, allocation.base)?
        };
        if fill.base_filled == 0 {
            continue;
        }
        mark_settled(&router_maker.key);
        validate!(
            fill.base_filled <= allocation.base,
            ErrorCode::DefaultError,
            "router maker {} overfilled: {} > {}",
            router_maker.key,
            fill.base_filled,
            allocation.base
        )?;
        validate!(
            crate::controller::matching::fill_at_or_better(
                taker_direction,
                &fill,
                &allocation,
                BASE_PRECISION_U64
            )?,
            ErrorCode::DefaultError,
            "router maker {} filled worse than quoted",
            router_maker.key
        )?;

        let mut maker_stats = if maker.authority == taker.authority {
            None
        } else {
            Some(makers_and_referrer_stats.get_ref_mut(&maker.authority)?)
        };
        let mut maker_opt: Option<&mut User> = Some(&mut maker);
        let mut maker_stats_opt: Option<&mut UserStats> = maker_stats.as_deref_mut();
        let (base_filled, quote_filled, maker_filled) = settle_dlob_match_fill(
            &fill,
            market.deref_mut(),
            taker,
            taker_stats,
            taker_position_index,
            taker_order_index,
            taker_key,
            taker_direction,
            taker_existing_position_params_before,
            &mut maker_opt,
            &mut maker_stats_opt,
            Some(router_maker.order_index),
            Some(&router_maker.key),
            maker_existing_position_params,
            Some(router_maker.price),
            effective_taker_limit,
            oracle_price,
            filler,
            filler_stats,
            filler_key,
            rev_share_escrow,
            referrer_is_accelerated,
            fee_structure,
            oracle_map,
            is_liquidation,
            now,
            slot,
            promo_fee_tier,
            builder_fee_allowed,
            &mut filler_reward_paid,
        )?;
        total_base = total_base.safe_add(base_filled)?;
        total_quote = total_quote.safe_add(quote_filled)?;
        if maker_filled != 0 {
            update_maker_fills_map(
                maker_fills,
                &router_maker.key,
                maker_direction,
                maker_filled,
                router_maker.is_isolated,
            )?;
        }
        // Once-per-order open-orders counter (mirrors the step's finalize).
        if maker.orders[router_maker.order_index].get_base_asset_amount_unfilled(None)? == 0 {
            let maker_position_index = get_position_index(&maker.perp_positions, market_index)?;
            let has_auction = maker.orders[router_maker.order_index].has_auction();
            maker.decrement_open_orders(has_auction);
            maker.perp_positions[maker_position_index].open_orders -= 1;
        }
    }

    // ---- Settle the vAMM fill. ----
    if let Some(fill) = amm_fill {
        // A maker that cranked this fill did the keeper's work for the *whole*
        // order, not just its own slice, so it earns the reward on the vAMM
        // slice too. It arrives as `filler: None` with `filler_key` naming
        // itself (already loaded in the maker map, so it cannot be loaded a
        // second time as the filler). Gated on it having actually filled, so a
        // maker that names itself but wins no allocation earns nothing. The
        // DLOB loop above has released its borrows by here.
        let cranking_maker_key = (filler.is_none() && maker_fills.contains_key(filler_key))
            .then_some(filler_key)
            .filter(|key| makers_and_referrer.0.contains_key(key));
        let mut cranking_maker = match cranking_maker_key {
            Some(key) => Some(makers_and_referrer.get_ref_mut(key)?),
            None => None,
        };
        let mut cranking_maker_stats = match cranking_maker.as_deref() {
            Some(maker) if maker.authority != taker.authority => {
                Some(makers_and_referrer_stats.get_ref_mut(&maker.authority)?)
            }
            _ => None,
        };
        let mut cranking_maker_opt: Option<&mut User> = cranking_maker.as_deref_mut();
        let mut cranking_maker_stats_opt: Option<&mut UserStats> =
            cranking_maker_stats.as_deref_mut();
        let (base_filled, quote_filled) = settle_amm_house_fill(
            &fill,
            market.deref_mut(),
            taker,
            taker_stats,
            taker_position_index,
            taker_order_index,
            taker_key,
            taker_direction,
            taker_existing_position_params_before,
            order_post_only,
            order_slot,
            order_id,
            taker_limit_price,
            amm_allocation.quote,
            amm_allocation.base,
            is_liquidation,
            &mut cranking_maker_opt,
            &mut cranking_maker_stats_opt,
            filler,
            filler_stats,
            filler_key,
            rev_share_escrow,
            referrer_is_accelerated,
            fee_structure,
            oracle_map,
            now,
            slot,
            vamm_maker_rebate,
            promo_fee_tier,
            builder_fee_allowed,
            &mut filler_reward_paid,
        )?;
        total_base = total_base.safe_add(base_filled)?;
        total_quote = total_quote.safe_add(quote_filled)?;
    }

    // ---- Execute + settle each external allocation via the CPI leg. ----
    // The response is untrusted on three axes, and each is bounded before a
    // single balance is moved: the volume (never more than allocated), the
    // price (inside the levels this quoter quoted moments ago, in this same
    // transaction), and the subject (a user this quoter is allowed to act
    // against — the loaded set is far wider than that, and it holds the taker
    // and every rival quoter's makers).
    for (i, allocation) in allocations[..externals_end].iter().enumerate() {
        if allocation.base == 0 {
            continue;
        }
        // Before the CPI: a book-backed quoter's permitted subjects live in
        // the state its execute is about to consume.
        let subjects = router.executor.subjects(i, direction, allocation.base)?;
        // The guard lives here, for exactly as long as this leg reads the
        // response — so the records below borrow out of the quoter's account
        // instead of being copied onto velocity's heap.
        // Priced before the CPI: execute overwrites the very buffer the ladder
        // was read from.
        //
        // Quantized at the same step the split used. The split skips a level's
        // sub-step tail, so it reaches further down the ladder than an
        // unquantized walk of the same base would, and the two disagree about
        // which levels the allocation was cut from. The band below is built
        // from this prefix's best and worst price, so a prefix that stopped
        // short rejects an honest fill priced at the level the split actually
        // allocated at.
        let quoted =
            crate::math::router::quoted_prefix(books[i].levels, order_step_size, allocation.base)?;
        let located = router.executor.execute(i, direction, allocation.base)?;
        let data = located.borrow()?;
        let response = located.execute_response(&data)?;
        // CLOB orders are margin-reserved through velocity at placement, so
        // their fills/culls unwind open-order aggregates; Custom PropAMM
        // depth is never reserved, so there is nothing to unwind.
        let maker_aggregates_tracked = router.executor.quoter_type(i).tracks_maker_aggregates();

        let (ext_base, ext_quote) = response.changes.iter().try_fold(
            (0u64, 0u64),
            |(base, quote), change| -> VelocityResult<(u64, u64)> {
                // A zero-base change is not a fill, so it has no place in the
                // response. Admitting one lets a quoter carry quote on a record
                // the per-change band and the subject check both skip (they
                // continue on base_size == 0), while its quote was already
                // summed here. A short taker is then settled at the quoter's
                // worst rung and the quoter keeps the difference.
                validate!(
                    change.base_size > 0,
                    ErrorCode::QuoterFillOffQuote,
                    "quoter {} returned a zero-base balance change carrying {} quote",
                    router.executor.quoter_key(i),
                    change.quote_size
                )?;
                Ok((
                    base.safe_add(change.base_size)?,
                    quote.safe_add(change.quote_size)?,
                ))
            },
        )?;
        // A quote is what its quoter can deliver. The CLOB spends execute's own
        // fill and user budget while it walks, and a custom quoter's ladder was
        // already cut to what its own margin supports — so the allocation is
        // fillable in full. Anything else is the quoter contradicting its own
        // quote.
        //
        // Delivering nothing is the same contradiction as delivering part, and
        // is treated the same way. It used to be skipped, which let a quoter
        // win base off a tight quote and hand the taker a hole: the size went
        // nowhere, and a source that would have filled it never saw it. An
        // allocation of zero is already skipped above, so reaching here with
        // nothing means this quoter was given real size.
        validate!(
            ext_base <= allocation.base,
            ErrorCode::QuoterOverfilled,
            "quoter {} filled {} of the {} it quoted",
            router.executor.quoter_key(i),
            ext_base,
            allocation.base
        )?;
        validate!(
            ext_base == allocation.base,
            ErrorCode::QuoterFilledShort,
            "quoter {} filled {} of the {} it quoted",
            router.executor.quoter_key(i),
            ext_base,
            allocation.base
        )?;
        // Held to the number the split accrued off the ladder, so the ladder
        // itself is dead the moment routing ends.
        validate!(
            crate::math::router::validate_allocated_notional(allocation, ext_quote)?,
            ErrorCode::QuoterFillOffQuote,
            "quoter {} filled {}/{} off its quote of {}",
            router.executor.quoter_key(i),
            ext_quote,
            ext_base,
            allocation.scaled_quote
        )?;

        for (change_index, change) in response.changes.iter().enumerate() {
            if change.base_size == 0 {
                continue;
            }
            let maker_key = resolve_user(&change.user)?;
            mark_settled(&maker_key);
            validate!(
                subjects.permits(&change.user, &maker_key, &taker_ref, &protocol_authority),
                ErrorCode::QuoterSubjectNotPermitted,
                "quoter {} may not act against user {}",
                router.executor.quoter_key(i),
                maker_key
            )?;
            validate!(
                crate::math::router::validate_change_notional(
                    &quoted,
                    change.base_size,
                    change.quote_size,
                    merged_orders(response.completed_count(change_index))?,
                )?,
                ErrorCode::QuoterFillOffQuote,
                "quoter {} priced user {} outside its quoted band",
                router.executor.quoter_key(i),
                maker_key
            )?;
            // Per-leg oracle band. The quoted-band and the aggregate checks
            // bound a change against the quoter's own quote and the blended
            // average, but a single maker can still sit far from oracle while
            // the blend passes — value moved onto that maker at a price the
            // average hides. Bound each maker's fill price the way the DLOB
            // match path bounds a resting maker order.
            let change_price = (change.quote_size as u128)
                .safe_mul(BASE_PRECISION_U64.cast()?)?
                .safe_div(change.base_size.cast()?)?
                .cast::<u64>()?;
            validate!(
                !crate::math::orders::limit_price_breaches_maker_oracle_price_bands(
                    change_price,
                    maker_direction,
                    oracle_price,
                    market.margin_ratio_initial,
                )?,
                ErrorCode::QuoterFillOffQuote,
                "quoter {} filled user {} at {} outside the oracle band",
                router.executor.quoter_key(i),
                maker_key,
                change_price
            )?;
            let mut maker = makers_and_referrer.get_ref_mut(&maker_key)?;
            // Same pre-flight as the DLOB leg: the maker's funding stamp
            // must be current before `settle_external_match_fill` touches
            // their position.
            settle_funding_payment(&mut maker, &maker_key, market.deref_mut(), now)?;
            let mut maker_stats = if maker.authority == taker.authority {
                None
            } else {
                Some(makers_and_referrer_stats.get_ref_mut(&maker.authority)?)
            };
            let (base_filled, quote_filled) = settle_external_match_fill(
                change.base_size,
                change.quote_size,
                market.deref_mut(),
                taker,
                taker_stats,
                taker_position_index,
                taker_order_index,
                taker_key,
                taker_direction,
                taker_existing_position_params_before,
                &mut maker,
                maker_stats.as_deref_mut(),
                &maker_key,
                maker_aggregates_tracked,
                response.sole_client_order_id(change_index),
                effective_taker_limit,
                oracle_price,
                filler,
                filler_stats,
                filler_key,
                rev_share_escrow,
                referrer_is_accelerated,
                fee_structure,
                oracle_map,
                is_liquidation,
                now,
                slot,
                promo_fee_tier,
                builder_fee_allowed,
                &mut filler_reward_paid,
            )?;
            total_base = total_base.safe_add(base_filled)?;
            total_quote = total_quote.safe_add(quote_filled)?;

            let maker_position_index = get_position_index(&maker.perp_positions, market_index)?;
            update_maker_fills_map(
                maker_fills,
                &maker_key,
                maker_direction,
                base_filled,
                maker.perp_positions[maker_position_index].is_isolated(),
            )?;
            if maker_aggregates_tracked {
                let position = &mut maker.perp_positions[maker_position_index];
                position.open_orders = position
                    .open_orders
                    .saturating_sub(response.completed_count(change_index).cast()?);
                for clob_order_id in response.completed_for(change_index) {
                    maker.decrement_open_orders(false);
                    // A fully-consumed order may be a placed trigger's live
                    // half; the shadow slot frees with it.
                    maker.release_placed_trigger_slot(
                        market_index,
                        clob_order_id,
                        OrderStatus::Filled,
                    );
                }
            }
        }

        // Sub-min remainders the quoter culled with this fill: unwind the
        // remainder from the maker's aggregates (that maker was just filled,
        // so they are loaded). A cull releases a margin reservation, so it is
        // held to the same subject rule as a balance change.
        if maker_aggregates_tracked {
            for cancelled in response.cancelled {
                let maker_key = resolve_user(&cancelled.user)?;
                validate!(
                    subjects.permits(&cancelled.user, &maker_key, &taker_ref, &protocol_authority),
                    ErrorCode::QuoterSubjectNotPermitted,
                    "quoter {} may not cancel for user {}",
                    router.executor.quoter_key(i),
                    maker_key
                )?;
                // A cull is a remainder the book refused to let rest, so it is
                // below the book's own minimum — and the attach requires that
                // minimum to be at or under the market's. The release below
                // saturates, so an oversized figure here would collapse the
                // maker's whole reservation for this market and free the margin
                // backing orders that are still resting.
                //
                // A market with no minimum of its own bounds nothing, which is
                // the same case the attach lets through.
                validate!(
                    market.market_stats.min_order_size == 0
                        || cancelled.base_asset_amount < market.market_stats.min_order_size,
                    ErrorCode::QuoterFillOffQuote,
                    "quoter {} culled {} base, at or above the market minimum {}",
                    router.executor.quoter_key(i),
                    cancelled.base_asset_amount,
                    market.market_stats.min_order_size
                )?;
                let mut maker = makers_and_referrer.get_ref_mut(&maker_key)?;
                let maker_position_index = get_position_index(&maker.perp_positions, market_index)?;
                decrease_open_bids_and_asks(
                    &mut maker.perp_positions[maker_position_index],
                    &maker_direction,
                    cancelled.base_asset_amount,
                    true,
                )?;
                let position = &mut maker.perp_positions[maker_position_index];
                position.open_orders = position.open_orders.saturating_sub(1);
                maker.decrement_open_orders(false);
                maker.release_placed_trigger_slot(
                    market_index,
                    cancelled.order_id,
                    OrderStatus::Canceled,
                );
                let is_isolated = maker.perp_positions[maker_position_index].is_isolated();
                drop(maker);
                // The cull is the only removal on this path velocity authors,
                // and it is bounded at one per book, so the record is one
                // record. Everything else the fill removed was consumed, and
                // a consumed order is reported by the fill.
                crate::instructions::emit_clob_cancel_record(
                    now,
                    market.market_stats.historical_oracle_data.last_oracle_price,
                    &maker_key,
                    crate::instructions::ClobOrderFacts {
                        order_id: cancelled.client_order_id,
                        market_index,
                        direction: maker_direction,
                        price: cancelled.price,
                        base_asset_amount: cancelled.base_asset_amount,
                        base_asset_amount_filled: 0,
                        max_ts: 0,
                        slot,
                        taker_origin: false,
                    },
                    OrderActionExplanation::ClobRemainderCulled,
                    None,
                    None,
                    is_isolated,
                )?;
            }
        }
    }

    if total_base == 0 {
        return Ok((0, 0));
    }

    // ---- Deferred mark-TWAP + volume, gated on a real fill (mirrors the
    // step's 3b). ----
    let twap_trade_price = match taker_direction {
        PositionDirection::Long => amm_ask_price,
        PositionDirection::Short => amm_bid_price,
    };
    market.market_stats.update_mark_twap_with_amm_bid_ask(
        amm_bid_price,
        amm_ask_price,
        amm_base_spread,
        amm_long_spread,
        amm_short_spread,
        now,
        Some(twap_trade_price),
        Some(taker_direction),
        sanitize_clamp_denom,
        order_tick_size,
    )?;
    market
        .market_stats
        .update_volume_24h(total_quote, taker_direction, now)?;

    // Once-per-order taker open-orders counter (mirrors the step's finalize).
    if taker.orders[taker_order_index].get_base_asset_amount_unfilled(None)? == 0 {
        taker.decrement_open_orders(taker.orders[taker_order_index].has_auction());
        taker.perp_positions[taker_position_index].open_orders -= 1;
    }

    // A book asked for an owner this transaction does not carry. Whoever built
    // the transaction owes the taker every maker it had room for, so count the
    // loaded users that filled nothing and hold no role in the fill. Those
    // accounts spent locks the missing maker needed.
    //
    // Only reached when a book withheld depth, which is what keeps the cost of
    // this off every ordinary fill.
    if withheld_depth {
        // `settled_users` and `idle_loaded_users` address loaded users by a bit
        // in a u64. A loaded map past 64 users cannot mark a filled maker
        // beyond index 64 as settled, so it would read as idle and fail an
        // honest fill. Guard the assumption loudly. The wire user set is capped
        // well under 64 (`MAX_QUOTER_WIRE_USERS`), so a real fill never reaches
        // this; if the caps ever grow, widen the bitmap instead of silently
        // miscounting.
        validate!(
            makers_and_referrer.0.len() <= u64::BITS as usize,
            ErrorCode::DefaultError,
            "loaded user map has {} users, past the 64 the obligation bitmap covers",
            makers_and_referrer.0.len()
        )?;
        let idle = idle_loaded_users(
            makers_and_referrer,
            settled_users,
            taker_key,
            filler_key,
            taker_stats.referrer,
            router.executor,
        )?;
        crate::math::router::withheld_obligation(&router.obligation, idle)?;
    }

    Ok((total_base, total_quote))
}

/// Loaded users that the fill did not move and that hold no role in it.
///
/// A role is one of four: the taker, the filler, the taker's referrer, or the
/// account a registered quoter fills for. Each of those has to be loaded whether
/// or not it receives a balance change. Everything else in the map is there to
/// be filled, and one that filled nothing spent two account locks for nothing.
fn idle_loaded_users<'info>(
    makers_and_referrer: &UserMap,
    settled_users: u64,
    taker_key: &Pubkey,
    filler_key: &Pubkey,
    referrer_authority: Pubkey,
    executor: &dyn crate::state::prop_amm::ExternalQuoterExecutor<'info>,
) -> VelocityResult<usize> {
    let mut idle = 0usize;
    for (index, (key, loader)) in makers_and_referrer.0.iter().enumerate() {
        if index < u64::BITS as usize && settled_users & (1u64 << index) != 0 {
            continue;
        }
        if key == taker_key || key == filler_key {
            continue;
        }
        let authority = loader
            .load()
            .map_err(|_| ErrorCode::UnableToLoadAccountLoader)?
            .authority;
        if authority == referrer_authority && referrer_authority != Pubkey::default() {
            continue;
        }
        if (0..crate::instructions::MAX_ROUTE_QUOTERS).any(|i| executor.quoter_user(i) == *key) {
            continue;
        }
        idle = idle.saturating_add(1);
    }
    Ok(idle)
}

/// How many of a quoter's own orders one balance change merges, as the
/// response itself declares: every order the change consumed outright, plus
/// at most one it left a remainder on. Bounds the integer rounding a merged
/// record can carry (see `math::router::validate_change_notional`).
fn merged_orders(consumed: usize) -> VelocityResult<u64> {
    consumed.cast::<u64>()?.safe_add(1)
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
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
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
            perp_market_map,
            spot_market_map,
            oracle_map,
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

/// The cumulative base where a buy book's asks still cross a sell book's bids.
///
/// `asks` and `bids` are best-first — asks ascending in price, bids descending —
/// the order a book quotes them in. Pairing them unit for unit, a cross is
/// profitable while the marginal ask is at or below the marginal bid; this is
/// the size at which that stops. A cross sized to this fills only crossed levels
/// on both legs, so no unit trades at a loss and none sweeps past the crossing
/// region into the caller's own intermediate orders.
fn crossing_prefix_size(
    asks: &[crate::state::prop_amm::PriceLevel],
    bids: &[crate::state::prop_amm::PriceLevel],
) -> u64 {
    let mut ai = 0usize;
    let mut bi = 0usize;
    let mut a_rem = asks.first().map_or(0, |level| level.size);
    let mut b_rem = bids.first().map_or(0, |level| level.size);
    let mut crossed = 0u64;
    while ai < asks.len() && bi < bids.len() {
        if a_rem == 0 {
            ai += 1;
            a_rem = asks.get(ai).map_or(0, |level| level.size);
            continue;
        }
        if b_rem == 0 {
            bi += 1;
            b_rem = bids.get(bi).map_or(0, |level| level.size);
            continue;
        }
        if asks[ai].price > bids[bi].price {
            break;
        }
        let take = a_rem.min(b_rem);
        crossed = crossed.saturating_add(take);
        a_rem -= take;
        b_rem -= take;
    }
    crossed
}

/// Match two crossed external sources against each other with the protocol
/// `User` as the pass-through taker — the arb bot the cross-match crank
/// runs. Buy `size` from the `buy_index` book's asks, then sell exactly what
/// filled into the `sell_index` book's bids (the two indexes may name the
/// same book: an internally crossed CLOB). Both legs settle through the
/// standard external-match path, so every maker experiences an ordinary
/// fill — positions, match fees, records, aggregate unwinds, and the shared
/// post-fill margin checks all apply.
///
/// The taker side needs no margin: its base is asserted unchanged (an
/// imbalanced pair of legs reverts), and its quote delta — the crossed
/// spread net of both legs' taker fees — must be strictly positive, so the
/// protocol never runs a losing cross and a fee-gulfed cross simply rests.
/// The taker's two orders are ephemeral: written into a free slot for the
/// legs' settlement (fee schedule, fill records) and cleared before return,
/// never counted in any open-order accounting.
///
/// Returns `(base_matched, quote_surplus)`.
#[allow(clippy::too_many_arguments)]
pub fn cross_match(
    state: &State,
    market_index: u16,
    mut size: u64,
    buy_index: usize,
    sell_index: usize,
    taker_loader: &AccountLoader<User>,
    taker_stats_loader: &AccountLoader<UserStats>,
    makers_and_referrer: &UserMap,
    makers_and_referrer_stats: &UserStatsMap,
    executor: &mut dyn crate::state::prop_amm::ExternalQuoterExecutor,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    clock: &Clock,
    // Floor on the protocol's quote surplus, from the market's crank
    // conditions. A cross the protocol nets less than this on is not worth
    // the lamports the reservoir pays out for it.
    min_surplus: u64,
) -> VelocityResult<(u64, u64)> {
    let now = clock.unix_timestamp;
    let slot = clock.slot;
    let taker_key = taker_loader.key();

    validate!(
        size > 0,
        ErrorCode::DefaultError,
        "cross size must be nonzero"
    )?;

    // ---- Oracle pre-flight, mirroring the fill path. ----
    let (oracle_price, oracle_stale_for_margin, perp_market_oi_before) = {
        let market = &mut perp_market_map.get_ref_mut(&market_index)?;
        validation::perp_market::validate_perp_market(market)?;
        validate!(
            !market.is_in_settlement(now),
            ErrorCode::MarketFillOrderPaused,
            "Market is in settlement mode",
        )?;
        validate!(
            !market.is_operation_paused(PerpOperation::Fill),
            ErrorCode::MarketFillOrderPaused,
            "Market fills paused",
        )?;

        let oracle_price_data = oracle_map.get_price_data(&market.oracle_id())?;
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
            clock.slot,
            state.slot_clock(),
        )?;
        validate!(
            is_oracle_valid_for_action(safe_oracle_validity, Some(VelocityAction::FillOrderMatch))?,
            ErrorCode::InvalidOracle,
            "oracle not valid for cross match"
        )?;
        let oracle_price = mm_oracle_price_data.get_price();
        validate_market_within_price_band(market, state, oracle_price)?;
        let oracle_stale_for_margin = state
            .slot_clock()
            .elapsed_slot_delta(mm_oracle_price_data.get_delay().max(0) as u64, slot)
            > state.oracle_guard_rails.validity.stale_for_margin_ms();
        (
            oracle_price,
            oracle_stale_for_margin,
            market.get_open_interest(),
        )
    };

    // Settle funding for everyone the legs can touch BEFORE any position
    // update, mirroring the fill path's pre-flight: `update_position_and_market`
    // requires each position's `last_cumulative_funding_rate` to match the
    // market's, and a maker who last traded before a funding update fails
    // that invariant otherwise.
    {
        let mut market = perp_market_map.get_ref_mut(&market_index)?;
        for (maker_key, maker_loader) in makers_and_referrer.0.iter() {
            let mut maker = load_mut!(maker_loader)?;
            if maker.get_perp_position(market_index).is_ok() {
                settle_funding_payment(&mut maker, maker_key, &mut market, now)?;
            }
        }
    }

    let taker = &mut load_mut!(taker_loader)?;
    let mut taker_stats = load_mut!(taker_stats_loader)?;
    {
        let mut market = perp_market_map.get_ref_mut(&market_index)?;
        settle_funding_payment(taker, &taker_key, &mut market, now)?;
    }
    let taker_ref = crate::state::prop_amm::ClobUserRefV0 {
        authority: taker.authority,
        sub_account_id: taker.sub_account_id.into(),
    };
    let protocol_authority = state.signer;
    let user_ref_index = makers_and_referrer.user_ref_index()?;
    let resolve_user = |user: &crate::state::prop_amm::ClobUserRefV0| -> VelocityResult<Pubkey> {
        user_ref_index
            .get(&(user.authority, user.sub_account_id))
            .copied()
            .ok_or_else(|| {
                msg!(
                    "cross leg returned a balance change for an unloaded user {}/{}",
                    user.authority,
                    user.sub_account_id
                );
                ErrorCode::DefaultError
            })
    };

    let taker_position_index = get_position_index(&taker.perp_positions, market_index)
        .or_else(|_| add_new_position(&mut taker.perp_positions, market_index))?;
    let base_before = taker.perp_positions[taker_position_index].base_asset_amount;
    let quote_before = taker.perp_positions[taker_position_index].quote_asset_amount;

    // The ephemeral taker order the settlement path reads (fee schedule off
    // its slot, id for the fill records). Cleared before return.
    let taker_order_index = taker
        .orders
        .iter()
        .position(|order| order.is_available())
        .ok_or(ErrorCode::MaxNumberOfOrders)?;
    let order_id = taker.next_order_id;
    taker.next_order_id = taker.next_order_id.wrapping_add(1).max(1);

    let mut maker_fills: BTreeMap<Pubkey, (i64, bool)> = BTreeMap::new();
    let mut none_filler: Option<&mut User> = None;
    let mut none_filler_stats: Option<&mut UserStats> = None;
    let mut no_escrow: Option<&mut RevenueShareEscrowZeroCopyMut> = None;
    let mut filler_reward_paid = 0u64;

    // Bound the cross to the prefix where the two books actually cross. The
    // caller names `size`, and past the crossing depth the extra fills
    // non-crossed levels — which lets a caller that rests its own orders at
    // intermediate prices siphon the spread the protocol would have taken, and
    // lets a sweep run past a taker-origin order the dedicated crank owes an
    // improvement to. Computable only when both sides are books (a ladder to
    // read); a Custom side is bounded by its pre-execute margin clamp and the
    // surplus floor instead. The two ladders come from the same quote the
    // execute below fills, so the boundary they show is the one it hits.
    {
        let buy_asks =
            executor.resting_levels(buy_index, crate::state::prop_amm::Direction::Long, size)?;
        let sell_bids =
            executor.resting_levels(sell_index, crate::state::prop_amm::Direction::Short, size)?;
        if let (Some(buy_asks), Some(sell_bids)) = (buy_asks, sell_bids) {
            size = size.min(crossing_prefix_size(&buy_asks, &sell_bids));
        }
    }

    let mut leg_totals: [(u64, u64); 2] = [(0, 0); 2];
    let legs = [
        (buy_index, PositionDirection::Long),
        (sell_index, PositionDirection::Short),
    ];
    #[allow(clippy::needless_range_loop)]
    for (leg, (book_index, taker_direction)) in legs.iter().copied().enumerate() {
        // The sell leg must return exactly what the buy leg took.
        let leg_size = if leg == 0 { size } else { leg_totals[0].0 };
        if leg_size == 0 {
            break;
        }

        // Custom PropAMM depth is never margin-reserved; clamp the leg to
        // what the quoter's own account supports before mutating external
        // state, exactly like the fill's pre-execute clamp.
        if executor.quoter_type(book_index) == crate::state::prop_amm::QuoterType::Custom {
            let quoter_user_key = executor.quoter_user(book_index);
            validate!(
                quoter_user_key != taker_key,
                ErrorCode::DefaultError,
                "cross leg quotes for the protocol user itself"
            )?;
            let maker_direction = taker_direction.opposite();
            let position_index = {
                let mut maker = makers_and_referrer.get_ref_mut(&quoter_user_key)?;
                get_position_index(&maker.perp_positions, market_index)
                    .or_else(|_| add_new_position(&mut maker.perp_positions, market_index))?
            };
            let maker = makers_and_referrer.get_ref(&quoter_user_key)?;
            let cap = crate::math::orders::calculate_max_perp_order_size(
                &maker,
                position_index,
                market_index,
                maker_direction,
                perp_market_map,
                spot_market_map,
                oracle_map,
            )?;
            validate!(
                cap >= leg_size,
                ErrorCode::CrossMatchImbalanced,
                "cross leg {} margin cap {} below leg size {}",
                leg,
                cap,
                leg_size
            )?;
        }

        // The leg's ephemeral order (settlement reads direction + slot + id).
        taker.orders[taker_order_index] = Order {
            slot,
            order_id,
            market_index,
            status: OrderStatus::Open,
            order_type: OrderType::Market,
            market_type: MarketType::Perp,
            direction: taker_direction,
            base_asset_amount: leg_size,
            existing_position_direction: taker_direction,
            ..Order::default()
        };
        let taker_existing_position_params_before = taker.perp_positions[taker_position_index]
            .get_existing_position_params_for_order_action(taker_direction);

        let cpi_direction = match taker_direction {
            PositionDirection::Long => crate::state::prop_amm::Direction::Long,
            PositionDirection::Short => crate::state::prop_amm::Direction::Short,
        };
        let subjects = executor.subjects(book_index, cpi_direction, leg_size)?;
        // Read before the CPI, because execute consumes the orders it is read
        // from. A cross has no `quote_v0` leg to bind against (the crank's
        // account list carries only the execute surface), so the run these
        // orders rest at stands in as the quote. A Custom entry offers none,
        // and there it is the entry's single consenting `user` plus the
        // surplus check that bound the leg.
        let resting = executor.resting_levels(book_index, cpi_direction, leg_size)?;
        let located = executor.execute(book_index, cpi_direction, leg_size)?;
        let data = located.borrow()?;
        let response = located.execute_response(&data)?;
        let maker_aggregates_tracked = executor.quoter_type(book_index).tracks_maker_aggregates();
        let maker_direction = taker_direction.opposite();

        let (leg_base, leg_quote) = response.changes.iter().try_fold(
            (0u64, 0u64),
            |(base, quote), change| -> VelocityResult<(u64, u64)> {
                // A zero-base change carries no fill; its quote would be summed
                // here but skipped by the per-change price and subject checks
                // below. Reject it so a cross leg cannot smuggle quote past them.
                validate!(
                    change.base_size > 0,
                    ErrorCode::QuoterFillOffQuote,
                    "cross leg quoter {} returned a zero-base balance change carrying {} quote",
                    executor.quoter_key(book_index),
                    change.quote_size
                )?;
                Ok((
                    base.safe_add(change.base_size)?,
                    quote.safe_add(change.quote_size)?,
                ))
            },
        )?;
        // These are real orders rather than a quoted ladder, so they price at
        // their own size with no step quantization.
        let quoted = match resting {
            Some(levels) if leg_base > 0 => {
                Some(crate::math::router::quoted_prefix(&levels, 1, leg_base)?)
            }
            _ => None,
        };
        if let Some(quoted) = quoted.as_ref() {
            validate!(
                crate::math::router::validate_executed_notional(&quoted, leg_quote)?,
                ErrorCode::QuoterFillOffQuote,
                "quoter {} filled {}/{} outside the book it swept ({}..{})",
                executor.quoter_key(book_index),
                leg_quote,
                leg_base,
                quoted.best_price,
                quoted.worst_price
            )?;
        }

        let mut market = perp_market_map.get_ref_mut(&market_index)?;
        for (change_index, change) in response.changes.iter().enumerate() {
            if change.base_size == 0 {
                continue;
            }
            let maker_key = resolve_user(&change.user)?;
            validate!(
                subjects.permits(&change.user, &maker_key, &taker_ref, &protocol_authority),
                ErrorCode::QuoterSubjectNotPermitted,
                "quoter {} may not act against user {} (the protocol user \
                 itself is never a subject)",
                executor.quoter_key(book_index),
                maker_key
            )?;
            if let Some(quoted) = quoted.as_ref() {
                validate!(
                    crate::math::router::validate_change_notional(
                        quoted,
                        change.base_size,
                        change.quote_size,
                        merged_orders(response.completed_count(change_index))?,
                    )?,
                    ErrorCode::QuoterFillOffQuote,
                    "quoter {} priced user {} outside the book it swept",
                    executor.quoter_key(book_index),
                    maker_key
                )?;
            }
            // Per-leg oracle band, as the router fill applies. A cross settles
            // real makers at the crossed prices; bound each against oracle so a
            // maker resting far off it is not filled at that price.
            let change_price = (change.quote_size as u128)
                .safe_mul(BASE_PRECISION_U64.cast()?)?
                .safe_div(change.base_size.cast()?)?
                .cast::<u64>()?;
            validate!(
                !crate::math::orders::limit_price_breaches_maker_oracle_price_bands(
                    change_price,
                    maker_direction,
                    oracle_price,
                    market.margin_ratio_initial,
                )?,
                ErrorCode::QuoterFillOffQuote,
                "quoter {} filled user {} at {} outside the oracle band",
                executor.quoter_key(book_index),
                maker_key,
                change_price
            )?;
            let mut maker = makers_and_referrer.get_ref_mut(&maker_key)?;
            let mut maker_stats = Some(makers_and_referrer_stats.get_ref_mut(&maker.authority)?);
            let (base_filled, quote_filled) = settle_external_match_fill(
                change.base_size,
                change.quote_size,
                market.deref_mut(),
                taker,
                &mut taker_stats,
                taker_position_index,
                taker_order_index,
                &taker_key,
                taker_direction,
                taker_existing_position_params_before,
                &mut maker,
                maker_stats.as_deref_mut(),
                &maker_key,
                maker_aggregates_tracked,
                response.sole_client_order_id(change_index),
                None,
                oracle_price,
                &mut none_filler,
                &mut none_filler_stats,
                &taker_key,
                &mut no_escrow,
                false,
                &state.perp_fee_structure,
                oracle_map,
                false,
                now,
                slot,
                state.promo_fee_tier,
                // The crank's taker is the protocol User. It has no builder
                // escrow, so there is no builder fee to allow.
                false,
                &mut filler_reward_paid,
            )?;
            leg_totals[leg].0 = leg_totals[leg].0.safe_add(base_filled)?;
            leg_totals[leg].1 = leg_totals[leg].1.safe_add(quote_filled)?;

            let maker_position_index = get_position_index(&maker.perp_positions, market_index)?;
            update_maker_fills_map(
                &mut maker_fills,
                &maker_key,
                maker_direction,
                base_filled,
                maker.perp_positions[maker_position_index].is_isolated(),
            )?;
            if maker_aggregates_tracked {
                let position = &mut maker.perp_positions[maker_position_index];
                position.open_orders = position
                    .open_orders
                    .saturating_sub(response.completed_count(change_index).cast()?);
                for clob_order_id in response.completed_for(change_index) {
                    maker.decrement_open_orders(false);
                    maker.release_placed_trigger_slot(
                        market_index,
                        clob_order_id,
                        OrderStatus::Filled,
                    );
                }
            }
        }
        if maker_aggregates_tracked {
            for cancelled in response.cancelled {
                let maker_key = resolve_user(&cancelled.user)?;
                validate!(
                    subjects.permits(&cancelled.user, &maker_key, &taker_ref, &protocol_authority),
                    ErrorCode::QuoterSubjectNotPermitted,
                    "quoter {} may not cancel for user {}",
                    executor.quoter_key(book_index),
                    maker_key
                )?;
                // A cull is a remainder the book refused to let rest, so it is
                // below the book's own minimum — and the attach requires that
                // minimum to be at or under the market's. The release below
                // saturates, so an oversized figure here would collapse the
                // maker's whole reservation for this market and free the margin
                // backing orders that are still resting.
                //
                // A market with no minimum of its own bounds nothing, which is
                // the same case the attach lets through.
                validate!(
                    market.market_stats.min_order_size == 0
                        || cancelled.base_asset_amount < market.market_stats.min_order_size,
                    ErrorCode::QuoterFillOffQuote,
                    "quoter {} culled {} base, at or above the market minimum {}",
                    executor.quoter_key(book_index),
                    cancelled.base_asset_amount,
                    market.market_stats.min_order_size
                )?;
                let mut maker = makers_and_referrer.get_ref_mut(&maker_key)?;
                let maker_position_index = get_position_index(&maker.perp_positions, market_index)?;
                decrease_open_bids_and_asks(
                    &mut maker.perp_positions[maker_position_index],
                    &maker_direction,
                    cancelled.base_asset_amount,
                    true,
                )?;
                let position = &mut maker.perp_positions[maker_position_index];
                position.open_orders = position.open_orders.saturating_sub(1);
                maker.decrement_open_orders(false);
                maker.release_placed_trigger_slot(
                    market_index,
                    cancelled.order_id,
                    OrderStatus::Canceled,
                );
                let is_isolated = maker.perp_positions[maker_position_index].is_isolated();
                drop(maker);
                crate::instructions::emit_clob_cancel_record(
                    now,
                    oracle_price,
                    &maker_key,
                    crate::instructions::ClobOrderFacts {
                        order_id: cancelled.client_order_id,
                        market_index,
                        direction: maker_direction,
                        price: cancelled.price,
                        base_asset_amount: cancelled.base_asset_amount,
                        base_asset_amount_filled: 0,
                        max_ts: 0,
                        slot,
                        taker_origin: false,
                    },
                    OrderActionExplanation::ClobRemainderCulled,
                    None,
                    None,
                    is_isolated,
                )?;
            }
        }
        validate!(
            leg_totals[leg].0 <= leg_size,
            ErrorCode::DefaultError,
            "cross leg {} overfilled: {} > {}",
            leg,
            leg_totals[leg].0,
            leg_size
        )?;
    }

    // The ephemeral order never outlives the match.
    taker.orders[taker_order_index] = Order::default();

    let (base_matched, _) = leg_totals[0];
    validate!(
        leg_totals[1].0 == base_matched,
        ErrorCode::CrossMatchImbalanced,
        "cross legs imbalanced: bought {} sold {}",
        base_matched,
        leg_totals[1].0
    )?;
    validate!(
        base_matched > 0,
        ErrorCode::CrossMatchUnprofitable,
        "nothing crossed"
    )?;
    validate!(
        taker.perp_positions[taker_position_index].base_asset_amount == base_before,
        ErrorCode::CrossMatchImbalanced,
        "protocol user base changed: {} -> {}",
        base_before,
        taker.perp_positions[taker_position_index].base_asset_amount
    )?;
    let surplus = taker.perp_positions[taker_position_index]
        .quote_asset_amount
        .safe_sub(quote_before)?;
    validate!(
        surplus > 0 && surplus.unsigned_abs() >= min_surplus,
        ErrorCode::CrossMatchUnprofitable,
        "cross surplus {} below the market's floor of {} (fees are the gulf)",
        surplus,
        min_surplus
    )?;
    taker.update_last_active_slot(slot);

    // Shared post-fill invariants: the per-maker margin/equity-floor/breaker
    // checks over both legs' fills, plus the stale-oracle OI rule. The taker
    // side of the shared check passes trivially — the protocol User's base
    // is unchanged and its quote strictly grew.
    fulfill_perp_order_post_checks(
        taker,
        &mut taker_stats,
        makers_and_referrer,
        makers_and_referrer_stats,
        spot_market_map,
        perp_market_map,
        oracle_map,
        market_index,
        base_matched,
        leg_totals[0].1,
        &maker_fills,
        true,
        false,
        perp_market_oi_before,
        oracle_stale_for_margin,
        false,
        now,
    )?;

    Ok((base_matched, surplus.cast()?))
}

/// The oracle pre-flight and R5 pricing of one taker-origin cross, before any
/// of it is committed.
///
/// Split from [`settle_taker_origin_cross`] because it must run *before* the
/// CLOB calls that consume the pair: the cross is refused outright when
/// crossing would leave the taker worse off than the price it was resting at,
/// and a refusal has to leave the book untouched. Same oracle gates the fill
/// path applies (`FillOrderMatch` validity, price band, staleness) — the
/// reward's size-vs-oracle multiplier is derived from the price, so it is a
/// value transfer driven by an oracle read and gated like one.
///
/// Returns the fee split, the mm-oracle price the settlement values fills
/// against, whether the oracle is stale for margin, and the market's open
/// interest before the fill — the last three being what
/// [`settle_taker_origin_cross`] needs to run the shared post-fill checks.
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

        let oracle_price_data = oracle_map.get_price_data(&market.oracle_id())?;
        let mm_oracle_price_data = market.get_mm_oracle_price_data(
            *oracle_price_data,
            clock.slot,
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
            clock.slot,
            state.slot_clock(),
        )?;
        validate!(
            is_oracle_valid_for_action(safe_oracle_validity, Some(VelocityAction::FillOrderMatch))?,
            ErrorCode::InvalidOracle,
            "oracle not valid for taker-origin cross"
        )?;
        let oracle_price = mm_oracle_price_data.get_price();
        validate_market_within_price_band(market, state, oracle_price)?;
        (
            oracle_price,
            state
                .slot_clock()
                .elapsed_slot_delta(mm_oracle_price_data.get_delay().max(0) as u64, clock.slot)
                > state.oracle_guard_rails.validity.stale_for_margin_ms(),
            market.get_open_interest(),
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

/// What one resolved taker-origin cross settled to.
#[derive(Clone, Copy, Debug)]
pub struct TakerOriginCrossFill {
    pub base_filled: u64,
    pub quote_filled: u64,
    /// Quote moved from the taker to the filler out of the improvement.
    pub crank_reward: u64,
    /// The counterparty the response named, resolved against the loaded users.
    pub maker: Pubkey,
}

/// How the counterparty leg of a taker-origin cross reached the settlement,
/// and therefore what bookkeeping the settlement still owes its `User`.
///
/// The two arms are the two shapes a resolvable cross comes in, and the book
/// decides which — they are not interchangeable ways of doing the same thing.
/// An ordinary maker is *consumed* through the book, the only actor allowed to
/// price its own fill, so the settlement holds the response to the quote
/// velocity read off the book beforehand. A second taker remainder cannot be
/// consumed at all: the book withholds a crossed taker-origin order from
/// `execute_v0` exactly as it withholds the first, and would silently fill
/// deeper depth instead. So the crank cancels both, and the price comes from
/// two removals the book itself vouched for.
pub enum TakerOriginCounterparty {
    /// Filled through the book. `levels` is the resting run velocity read
    /// before the CPI — the quote the response is held to — and the response's
    /// retired order ids are what unwind the maker's open-order slots.
    Executed {
        /// The counterparty's fill. Exactly one, by construction: the crank
        /// sizes the match at the counterparty's own remaining size, so a
        /// response naming anyone else — or splitting across orders — is not
        /// the cross that was priced.
        change: crate::state::prop_amm::UserBalanceChangeV0,
        /// The counterparty's order, when the fill took it whole: the book's
        /// id, and the maker's own id for the same order.
        consumed: Option<(u64, u32)>,
        /// The sub-min remainder the book culled with it, if any.
        cancelled: Option<crate::state::prop_amm::CancelledRemainderV0>,
        subjects: crate::state::prop_amm::QuoterSubjects,
        /// The prices the counterparty's side rested at, read before the CPI
        /// consumed it. The cross has no quote leg, so this is what the
        /// executed notional is held to.
        levels: Vec<crate::state::prop_amm::PriceLevel>,
    },
    /// Cancelled off the book by the crank, and settled at its own price for
    /// `base_asset_amount` of its size. Its order left the book whole, so its
    /// open-order slot and any leftover reservation are the *caller's* to
    /// settle — only the caller can re-place a leftover, which is a CPI.
    Cancelled {
        removed: crate::state::prop_amm::ClobRemovedOrderV0,
        base_asset_amount: u64,
    },
}

/// The counterparty leg, resolved to what the settlement needs of it.
struct CounterpartyLeg<'a> {
    user: crate::state::prop_amm::ClobUserRefV0,
    base_asset_amount: u64,
    quote_asset_amount: u64,
    /// The counterparty's own id for its order. Known either way: an executed
    /// leg is one order by construction, and a cancelled one is the order the
    /// removal named.
    client_order_id: Option<u32>,
    /// The book's balance change, when the book is what filled it — whose
    /// retired order ids the settlement unwinds. `None` for a cancelled
    /// counterparty, which had no fill on the book to report.
    change: Option<&'a crate::state::prop_amm::UserBalanceChangeV0>,
}

impl TakerOriginCounterparty {
    /// Read the bounded cross out of a quoter's response.
    ///
    /// Copies rather than borrows, and can afford to: the whole of what a
    /// resolved cross carries is one fill, at most one consumed order and at
    /// most one cull — three fixed-width records on the stack. Validating
    /// those bounds here is also the check that the response describes *this*
    /// cross rather than some larger fill.
    pub(crate) fn executed(
        response: &crate::state::prop_amm::ExecuteResponseV0<'_>,
        subjects: crate::state::prop_amm::QuoterSubjects,
        levels: Vec<crate::state::prop_amm::PriceLevel>,
    ) -> VelocityResult<Self> {
        let mut filled = response
            .changes
            .iter()
            .filter(|change| change.base_size > 0);
        let change = *filled.next().ok_or_else(|| {
            msg!("taker-origin cross expects one counterparty fill, got none");
            ErrorCode::InvalidQuoterResponse
        })?;
        validate!(
            filled.next().is_none(),
            ErrorCode::InvalidQuoterResponse,
            "taker-origin cross expects one counterparty fill, got several"
        )?;
        let mut consumed = response
            .completed
            .iter()
            .filter(|entry| entry.change_index == 0)
            .map(|entry| (entry.order_id, entry.client_order_id));
        let first = consumed.next();
        validate!(
            consumed.next().is_none(),
            ErrorCode::InvalidQuoterResponse,
            "taker-origin cross cannot consume more than the counterparty's order"
        )?;
        validate!(
            response.cancelled.len() <= 1,
            ErrorCode::InvalidQuoterResponse,
            "taker-origin cross cannot cull more than one remainder"
        )?;
        Ok(Self::Executed {
            change,
            consumed: first,
            cancelled: response.cancelled.first().copied(),
            subjects,
            levels,
        })
    }

    fn leg(&self) -> VelocityResult<CounterpartyLeg<'_>> {
        match self {
            // One counterparty, one balance change: the crank sizes the match
            // at the counterparty's own remaining size, so a response naming
            // anyone else — or splitting across orders — is not the cross that
            // was priced.
            Self::Executed {
                change,
                consumed,
                cancelled,
                ..
            } => Ok(CounterpartyLeg {
                user: change.user,
                base_asset_amount: change.base_size,
                quote_asset_amount: change.quote_size,
                client_order_id: consumed
                    .map(|(_, client_order_id)| client_order_id)
                    .or_else(|| cancelled.map(|cull| cull.client_order_id)),
                change: Some(change),
            }),
            Self::Cancelled {
                removed,
                base_asset_amount,
            } => Ok(CounterpartyLeg {
                user: removed.user,
                base_asset_amount: *base_asset_amount,
                quote_asset_amount: clob_notional(removed.price, *base_asset_amount)?,
                client_order_id: Some(removed.client_order_id),
                change: None,
            }),
        }
    }
}

/// Notional of `base_asset_amount` at `price`, floored.
///
/// The CLOB's own rounding, which is what makes a notional velocity computes
/// for a cross it prices itself land in the same units a book-filled leg
/// would have — and lets one number serve both the fee's improvement and the
/// settlement's quote.
pub fn clob_notional(price: u64, base_asset_amount: u64) -> VelocityResult<u64> {
    price
        .cast::<u128>()?
        .safe_mul(base_asset_amount.cast()?)?
        .safe_div(BASE_PRECISION_U64.cast()?)?
        .cast::<u64>()
}

/// Settle a resolved taker-origin cross as an ordinary two-user match at the
/// counterparty's price.
///
/// This is R3: the taker-origin order demanded liquidity, so the counterparty
/// keeps its own price and the taker captures the whole difference. Nothing
/// reprices anything here — the counterparty's `(base, quote)` *is* that price,
/// whether the book filled it (its stored price, as the response reports) or
/// the crank cancelled it (its removal price, at [`clob_notional`]'s rounding).
///
/// Unlike [`cross_match`] there is no protocol pass-through: no ephemeral
/// protocol taker, no second leg, no surplus landing in the protocol `User`,
/// and no `min_cross_surplus` floor. Both sides of this match are real users
/// with real exposure, and the improvement is the taker's, not the protocol's
/// — the only cut taken out of it is the cranker's reward (R5, priced by
/// [`price_taker_origin_cross`]), paid in quote from the taker to the filler
/// the way every keeper reward on a perp is.
///
/// The taker's order lives on the book rather than in `User.orders`, so a free
/// order slot is reconstructed from the removal the CLOB reported (direction,
/// price, size, placement slot) for the settlement to read, and cleared before
/// return. That is not a fabrication like `cross_match`'s ephemeral order: the
/// taker really does have this order, it is just stored on the book.
///
/// `removed` is the taker-origin order the *aggressor* was resting as the CLOB
/// reported it on removal — the authority on which side was demanding
/// liquidity, not velocity's book read — and `order_slot` the slot it was
/// placed. `counterparty` is the other side of the match, in whichever of its
/// two shapes ([`TakerOriginCounterparty`]) the book left available.
#[allow(clippy::too_many_arguments)]
pub fn settle_taker_origin_cross(
    state: &State,
    market_index: u16,
    removed: &crate::state::prop_amm::ClobRemovedOrderV0,
    order_slot: u64,
    counterparty: &TakerOriginCounterparty,
    fee: &fees::TakerOriginCrossFee,
    oracle_price: i64,
    oracle_stale_for_margin: bool,
    perp_market_oi_before: u128,
    taker_loader: &AccountLoader<User>,
    taker_stats_loader: &AccountLoader<UserStats>,
    filler_loader: &AccountLoader<User>,
    filler_stats_loader: &AccountLoader<UserStats>,
    makers_and_referrer: &UserMap,
    makers_and_referrer_stats: &UserStatsMap,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    clock: &Clock,
) -> VelocityResult<TakerOriginCrossFill> {
    let now = clock.unix_timestamp;
    let slot = clock.slot;
    let taker_key = taker_loader.key();
    let filler_key = filler_loader.key();
    let taker_direction = removed.side.to_position_direction();
    let maker_direction = taker_direction.opposite();

    let leg = counterparty.leg()?;
    let taker_ref = {
        let taker = load!(taker_loader)?;
        crate::state::prop_amm::ClobUserRefV0 {
            authority: taker.authority,
            sub_account_id: taker.sub_account_id.into(),
        }
    };
    let protocol_authority = state.signer;
    let maker_key = *makers_and_referrer
        .user_ref_index()?
        .get(&(leg.user.authority, leg.user.sub_account_id))
        .ok_or_else(|| {
            msg!(
                "counterparty {}/{} is not loaded",
                leg.user.authority,
                leg.user.sub_account_id
            );
            ErrorCode::UserNotFound
        })?;
    if let TakerOriginCounterparty::Executed {
        subjects, levels, ..
    } = counterparty
    {
        validate!(
            subjects.permits(&leg.user, &maker_key, &taker_ref, &protocol_authority),
            ErrorCode::QuoterSubjectNotPermitted,
            "the book may not fill user {} for this cross",
            maker_key
        )?;
        // Hold the fill to the run velocity read off the book before the CPI —
        // the same binding the router fill and the cross crank apply, and what
        // makes "the counterparty's price" a fact rather than the book's claim.
        // A cancelled counterparty needs none of it: velocity priced that leg
        // itself, off a removal the book reported.
        let quoted = crate::math::router::quoted_prefix(levels, 1, leg.base_asset_amount)?;
        validate!(
            crate::math::router::validate_executed_notional(&quoted, leg.quote_asset_amount)?,
            ErrorCode::QuoterFillOffQuote,
            "counterparty filled {}/{} outside the book it rested on ({}..{})",
            leg.quote_asset_amount,
            leg.base_asset_amount,
            quoted.best_price,
            quoted.worst_price
        )?;
    }

    // Funding stamps first: `update_position_and_market` requires both
    // positions' `last_cumulative_funding_rate` to match the market's.
    let taker = &mut load_mut!(taker_loader)?;
    let mut taker_stats = load_mut!(taker_stats_loader)?;
    let mut maker = makers_and_referrer.get_ref_mut(&maker_key)?;
    let mut market = perp_market_map.get_ref_mut(&market_index)?;
    settle_funding_payment(taker, &taker_key, market.deref_mut(), now)?;
    settle_funding_payment(&mut maker, &maker_key, market.deref_mut(), now)?;

    let taker_position_index = get_position_index(&taker.perp_positions, market_index)
        .or_else(|_| add_new_position(&mut taker.perp_positions, market_index))?;
    let taker_existing_position_params_before = taker.perp_positions[taker_position_index]
        .get_existing_position_params_for_order_action(taker_direction);
    // Which margin requirement the taker is held to afterwards, decided before
    // its position moves: a cross that reduces an existing position is
    // measured against maintenance, as on any other fill.
    let taker_position_decreasing = is_order_position_reducing(
        &taker_direction,
        removed.base_asset_amount,
        taker.perp_positions[taker_position_index]
            .base_asset_amount
            .cast()?,
    )?;

    // The taker's order, reconstructed from the book's own record of it. The
    // CLOB's order id is wider than a velocity one; narrowing it keeps the
    // fill record pointing at the book's order (ids are sequential per book)
    // and the full-width id rides `TakerOriginCrossRecordV0`.
    let taker_order_index = taker
        .orders
        .iter()
        .position(|order| order.is_available())
        .ok_or(ErrorCode::MaxNumberOfOrders)?;
    taker.orders[taker_order_index] = Order {
        slot: order_slot,
        order_id: removed.order_id as u32,
        market_index,
        status: OrderStatus::Open,
        order_type: OrderType::Limit,
        market_type: MarketType::Perp,
        direction: taker_direction,
        base_asset_amount: removed.base_asset_amount,
        price: removed.price,
        existing_position_direction: taker_direction,
        ..Order::default()
    };

    let mut maker_stats = if maker.authority == taker.authority {
        None
    } else {
        Some(makers_and_referrer_stats.get_ref_mut(&maker.authority)?)
    };
    // No filler on the settlement itself: the cranker's reward comes out of
    // the improvement below, not out of the taker fee, so the fee schedule
    // splits exactly as it would on any other fill. No escrow either — a
    // builder/referrer accrual belongs to the order the taker signed, which
    // was settled when the remainder migrated.
    let mut none_filler: Option<&mut User> = None;
    let mut none_filler_stats: Option<&mut UserStats> = None;
    let mut no_escrow: Option<&mut RevenueShareEscrowZeroCopyMut> = None;
    let mut filler_reward_paid = 0u64;
    let (base_filled, quote_filled) = settle_external_match_fill(
        leg.base_asset_amount,
        leg.quote_asset_amount,
        market.deref_mut(),
        taker,
        &mut taker_stats,
        taker_position_index,
        taker_order_index,
        &taker_key,
        taker_direction,
        taker_existing_position_params_before,
        &mut maker,
        maker_stats.as_deref_mut(),
        &maker_key,
        true,
        // The counterparty is one order by construction: the crank sizes the
        // match at that order's own remaining size.
        leg.client_order_id,
        // The price the order was resting at is the taker's own limit, so the
        // fill has to be at-or-better than it — the cross is only allowed to
        // improve on the rest price, never to walk away from it.
        Some(removed.price),
        oracle_price,
        &mut none_filler,
        &mut none_filler_stats,
        &filler_key,
        &mut no_escrow,
        false,
        &state.perp_fee_structure,
        oracle_map,
        false,
        now,
        slot,
        state.promo_fee_tier,
        // The crank's taker is the protocol User. It has no builder escrow, so
        // there is no builder fee to allow.
        false,
        &mut filler_reward_paid,
    )?;
    // The reconstructed order never outlives the match.
    taker.orders[taker_order_index] = Order::default();

    let maker_position_index = get_position_index(&maker.perp_positions, market_index)?;
    let mut maker_fills: BTreeMap<Pubkey, (i64, bool)> = BTreeMap::new();
    update_maker_fills_map(
        &mut maker_fills,
        &maker_key,
        maker_direction,
        base_filled,
        maker.perp_positions[maker_position_index].is_isolated(),
    )?;
    // The counterparty's own book bookkeeping, for the leg the book filled:
    // orders the fill consumed outright, and a sub-min remainder the book
    // culled with it. A cancelled counterparty has neither — its order left
    // the book whole, and the caller settles its slot and its leftover.
    if let (
        Some(change),
        TakerOriginCounterparty::Executed {
            consumed,
            cancelled,
            ..
        },
    ) = (leg.change, counterparty)
    {
        maker.perp_positions[maker_position_index].open_orders = maker.perp_positions
            [maker_position_index]
            .open_orders
            .saturating_sub(consumed.is_some().into());
        if let Some((clob_order_id, _)) = *consumed {
            maker.decrement_open_orders(false);
            maker.release_placed_trigger_slot(market_index, clob_order_id, OrderStatus::Filled);
        }
        for cancelled in cancelled.iter() {
            validate!(
                cancelled.user == change.user,
                ErrorCode::QuoterSubjectNotPermitted,
                "the book may not cancel for a user this cross did not fill"
            )?;
            decrease_open_bids_and_asks(
                &mut maker.perp_positions[maker_position_index],
                &maker_direction,
                cancelled.base_asset_amount,
                true,
            )?;
            maker.perp_positions[maker_position_index].open_orders = maker.perp_positions
                [maker_position_index]
                .open_orders
                .saturating_sub(1);
            maker.decrement_open_orders(false);
            maker.release_placed_trigger_slot(
                market_index,
                cancelled.order_id,
                OrderStatus::Canceled,
            );
        }
    }

    // R5: the cranker's cut of the improvement, in quote, taker → filler —
    // the same quote-for-work transfer the flat keeper rewards make, sized by
    // the improvement instead of by a flat fee. Paid before the margin checks
    // so the taker is measured net of it.
    let crank_reward = if fee.crank_reward > 0 {
        let mut filler = load_mut!(filler_loader)?;
        let paid = pay_keeper_flat_reward_for_perps(
            taker,
            Some(&mut filler),
            market.deref_mut(),
            fee.crank_reward,
            slot,
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
        load_mut!(filler_stats_loader)?.update_filler_volume(quote_filled, now)?;
        paid
    } else {
        0
    };
    taker.update_last_active_slot(slot);
    drop(market);
    drop(maker_stats);
    drop(maker);

    fulfill_perp_order_post_checks(
        taker,
        &mut taker_stats,
        makers_and_referrer,
        makers_and_referrer_stats,
        spot_market_map,
        perp_market_map,
        oracle_map,
        market_index,
        base_filled,
        quote_filled,
        &maker_fills,
        taker_position_decreasing,
        taker.perp_positions[taker_position_index].is_isolated(),
        perp_market_oi_before,
        oracle_stale_for_margin,
        false,
        now,
    )?;

    Ok(TakerOriginCrossFill {
        base_filled,
        quote_filled,
        crank_reward,
        maker: maker_key,
    })
}

pub fn trigger_order(
    order_id: u32,
    state: &State,
    user: &AccountLoader<User>,
    user_stats: &AccountLoader<UserStats>,
    spot_market_map: &SpotMarketMap,
    perp_market_map: &PerpMarketMap,
    oracle_map: &mut OracleMap,
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

    let (order_status, market_index, market_type) =
        get_struct_values!(user.orders[order_index], status, market_index, market_type);

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

    validate_user_not_being_liquidated(
        user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        state.liquidation_margin_buffer_ratio,
    )?;

    validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

    let perp_market = perp_market_map.get_ref_mut(&market_index)?;

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
            perp_market_map,
            spot_market_map,
            oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )?;

        let net_equity =
            calculate_net_equity_for_floor(user, perp_market_map, spot_market_map, oracle_map)?;

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
                perp_market_map,
                spot_market_map,
                oracle_map,
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
            controller::equity_floor::try_lazy_equity_breaker_trip(
                user,
                &mut user_stats,
                perp_market_map,
                spot_market_map,
                oracle_map,
            )?;

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

    let mut perp_market = perp_market_map.get_ref_mut(&market_index)?;

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
    spot_market_map: &SpotMarketMap,
    perp_market_map: &PerpMarketMap,
    oracle_map: &mut OracleMap,
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
        perp_market_map,
        spot_market_map,
        oracle_map,
        MarginContext::standard(MarginRequirementType::Initial),
    )?;

    // Here "below floor" authorizes a keeper against the user, so it fails
    // closed in the other direction from the gates above: the floor counts as
    // grounds only when every oracle is valid and the trusted value sits below
    // it, so a bad price cannot manufacture authorization. Under oracle
    // degradation the keeper falls back to the margin arm, which keeps
    // force-cancel available on a margin-breached account.
    let below_equity_floor =
        calculate_net_equity_for_floor(user, perp_market_map, spot_market_map, oracle_map)?
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
                let spot_market = spot_market_map.get_ref(&market_index)?;
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
            perp_market_map,
            spot_market_map,
            oracle_map,
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
        spot_market_map.get_quote_spot_market_mut()?.deref_mut(),
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
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    now: i64,
    slot: u64,
) -> VelocityResult {
    for order_index in 0..user.orders.len() {
        if !should_expire_order(user, order_index, now)? {
            continue;
        }

        cancel_order(
            order_index,
            user,
            user_key,
            perp_market_map,
            spot_market_map,
            oracle_map,
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
