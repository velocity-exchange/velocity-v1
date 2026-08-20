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
            fees::{self, determine_user_fee_tier, FillFees},
            fulfillment::determine_perp_fulfillment_methods,
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
            time::{Millis, SlotDuration},
        },
        msg, print_error,
        state::{
            events::{
                emit_stack, get_order_action_record, OrderAction, OrderActionExplanation,
                OrderActionRecord, OrderRecord,
            },
            fill_mode::FillMode,
            fulfillment::PerpFulfillmentMethod,
            margin_calculation::{MarginContext, MarginTypeConfig},
            market_status::MarketStatus,
            oracle::OraclePriceData,
            oracle_map::OracleMap,
            order_params::{ModifyOrderParams, OrderParams, PlaceOrderOptions, PostOnlyParam},
            paused_operations::PerpOperation,
            perp_market::PerpMarket,
            perp_market_map::PerpMarketMap,
            quoter::{
                DlobOrderQuoter, FillFeePolicy, QuoteContext, Quoter, QuoterCommit, QuoterFill,
            },
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
mod amm_jit_tests;

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
            state.slot_duration(),
        )?;
    }

    let (auction_start_price, auction_end_price, auction_duration) = get_auction_params(
        &params,
        oracle_price_data,
        market.order_tick_size,
        state
            .min_perp_auction_duration_ms()
            .to_slots_ceil(state.slot_duration())
            .min(u8::MAX as u64) as u8,
    )?;

    let max_ts = match params.max_ts {
        Some(max_ts) => max_ts,
        None => match params.order_type {
            // default TIF: at least 30s, else the auction's wall-clock length
            // plus a quarter again plus 10s of pad, so the default always
            // outlives the auction. The /800 reproduces the historical
            // `auction_duration_slots / 2 + 10` exactly at the 400ms baseline
            // (a slot was 400ms, so slots/2 == ms/800) and holds that
            // wall-clock shape at every slot duration.
            OrderType::Market | OrderType::Oracle => now.safe_add(
                30_i64.max(
                    Millis::from_slots(auction_duration as u64, state.slot_duration())
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
        padding: [0; 5],
    };

    let valid_oracle_price = Some(oracle_price_data.price);
    match validate_order(&new_order, market, valid_oracle_price, slot) {
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
    let bit_flags = 0;
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

    let reserve_price_before: u64;
    let safe_oracle_validity: OracleValidity;
    let oracle_price: i64;
    let oracle_twap_5min: i64;
    let user_can_skip_duration: bool;
    let oracle_stale_for_margin: bool;
    let amm_not_globally_paused: bool = !state.amm_paused()?;
    let mut amm_is_available: bool = amm_not_globally_paused;
    // AMM JIT in a DLOB match: honors the hard gates but, unlike
    // `amm_is_available`, not the auction-timing gates (JIT feeds the auction).
    let amm_jit_allowed: bool;
    {
        let market = &mut perp_market_map.get_ref_mut(&market_index)?;
        validation::perp_market::validate_perp_market(market)?;
        validate!(
            !market.is_in_settlement(now),
            ErrorCode::MarketFillOrderPaused,
            "Market is in settlement mode",
        )?;

        let oracle_price_data = oracle_map.get_price_data(&market.oracle_id())?;
        let mm_oracle_price_data = market.get_mm_oracle_price_data(
            *oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            state.slot_duration(),
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
            state.slot_duration(),
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
        amm_jit_allowed = amm_not_globally_paused
            && market.amm_fill_gates_ok(safe_oracle_validity, &mm_oracle_price_data)?;

        oracle_stale_for_margin = mm_oracle_price_data.get_delay()
            > state
                .oracle_guard_rails
                .validity
                .stale_for_margin_ms()
                .to_slots(state.slot_duration()) as i64;

        // No AMM mutation here — `fulfill_perp_order_step` constructs an
        // `AmmQuoter` and calls `Quoter::setup` before quoting, which is
        // the sole non-admin AMM-refresh entrypoint. PerpMarket-level
        // oracle bookkeeping (TWAPs, reference-price-offset,
        // last_oracle_valid) is PerpMarket's own concern and stays here
        // (no AMM reacharound — PerpMarket reading its own AMM field).
        let amm_refresh_validity =
            crate::vlp::amm::refresh::compute_amm_refresh_validity_with_guard_rails(
                market,
                &mm_oracle_price_data,
                &state.oracle_guard_rails.validity,
                state.slot_duration(),
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
            state.slot_duration(),
        )?;

        reserve_price_before = market.amm.reserve_price()?;
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
    let match_fills_allowed =
        is_oracle_valid_for_action(safe_oracle_validity, Some(VelocityAction::FillOrderMatch))?;

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
        jit_maker_order_id,
        now,
        slot,
    )?;

    // Runs after `get_maker_orders_info` so its expired-maker-order cleanup
    // still happens; only the matching itself is withheld. AMM fills keep
    // their own gates.
    if !match_fills_allowed && !maker_orders_info.is_empty() {
        msg!(
            "Perp market = {} oracle not valid for match fills",
            market_index
        );
        maker_orders_info.clear();
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
        reserve_price_before,
        valid_oracle_price,
        now,
        slot,
        amm_is_available,
        amm_jit_allowed,
        fill_mode,
        oracle_stale_for_margin,
        rev_share_escrow,
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
        let maker_floor_unverifiable = match calculate_net_equity_for_floor(
            &maker,
            perp_market_map,
            spot_market_map,
            oracle_map,
        )? {
            Some(net_equity) => !net_equity.all_oracles_valid,
            None => false,
        };

        for (maker_order_index, maker_order_price) in maker_order_price_and_indexes.iter() {
            let maker_order_index = *maker_order_index;
            let maker_order_price = *maker_order_price;

            let maker_order = &maker.orders[maker_order_index];
            if !is_maker_for_taker(maker_order, taker_order, slot)? {
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
            // filler still earns the cleanup reward
            if maker_floor_unverifiable
                && !is_order_position_reducing(
                    &maker.orders[maker_order_index].direction,
                    maker.orders[maker_order_index]
                        .get_base_asset_amount_unfilled(Some(existing_base_asset_amount))?,
                    existing_base_asset_amount,
                )?
            {
                continue;
            }

            insert_maker_order_info(
                &mut maker_orders_info,
                (*maker_key, maker_order_index, maker_order_price),
                maker_direction,
            );
        }
    }

    Ok(maker_orders_info)
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
    reserve_price_before: u64,
    valid_oracle_price: Option<i64>,
    now: i64,
    slot: u64,
    amm_is_available: bool,
    amm_jit_allowed: bool,
    fill_mode: FillMode,
    oracle_stale_for_margin: bool,
    rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
    vamm_maker_rebate: bool,
    promo_fee_tier: u8,
) -> VelocityResult<(u64, u64)> {
    let market_index = user.orders[user_order_index].market_index;

    let user_order_position_decreasing =
        determine_if_user_order_is_position_decreasing(user, market_index, user_order_index)?;
    let user_is_isolated_position = user.get_perp_position(market_index)?.is_isolated();

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
    )?;
    let perp_market_oi_before = perp_market.get_open_interest();
    drop(perp_market);

    let fulfillment_methods = {
        let mut market = perp_market_map.get_ref_mut(&market_index)?;
        // Route off the PROJECTED curve, not the stored one. The AMM
        // refresh (snap toward oracle) used to run only inside `Quoter::setup`,
        // after a fulfillment method was already selected, so routing off the
        // stored reserve price let a stale curve block the very fill that would
        // refresh it: the taker failed to cross the stale quote, no method was
        // selected, setup never ran. Project and apply the refresh here, on the
        // real AMM, before routing. `project_and_apply` is slot-idempotent and
        // shares its implementation with `Quoter::setup`, so the projection
        // (the expensive peg / reserves / k-budget math) runs at most once per
        // market per slot: this call does it, and the first fill step's setup
        // then skips it. Routing and execution therefore quote off the exact
        // same curve. When the projection is a passthrough (oracle invalid for
        // curve updates, zero intensity, or the affordability floor rejected
        // it) the AMM is left at its stored curve and routing behaves as before.
        let slot_duration = oracle_map.slot_duration;
        let oracle_pd = *oracle_map.get_price_data(&market.oracle_id())?;
        let mm_oracle_pd = market.get_mm_oracle_price_data(
            oracle_pd,
            slot,
            validity_guard_rails,
            slot_duration,
        )?;
        let amm_refresh_validity =
            crate::vlp::amm::refresh::compute_amm_refresh_validity_with_guard_rails(
                &market,
                &mm_oracle_pd,
                validity_guard_rails,
                slot_duration,
            )?;
        let projection_inputs =
            crate::vlp::amm::math::repeg::ProjectionInputs::from_market(&market);
        crate::vlp::amm::refresh::project_and_apply(
            &mut market.amm,
            &projection_inputs,
            &mm_oracle_pd,
            amm_refresh_validity,
            slot,
        )?;
        // Refresh the cached spread state against the just-projected curve so
        // routing quotes off live spread even on the first fill of a slot
        // before any keeper crank. Reads the post-projection reserve price.
        let projected_reserve_price = market.amm.reserve_price()?;
        {
            let crate::state::perp_market::PerpMarket {
                amm, market_stats, ..
            } = &mut *market;
            crate::vlp::amm::math::spread::update_amm_quote_state(
                amm,
                market_stats,
                &mm_oracle_pd,
                projected_reserve_price,
                slot,
                slot_duration,
            )?;
        }
        determine_perp_fulfillment_methods(
            &user.orders[user_order_index],
            maker_orders_info,
            &market.amm,
            projected_reserve_price,
            limit_price,
            amm_is_available,
        )?
    };

    if fulfillment_methods.is_empty() {
        msg!("no fulfillment methods found");
        return Ok((0, 0));
    }

    let mut base_asset_amount = 0_u64;
    let mut quote_asset_amount = 0_u64;
    let mut maker_fills: BTreeMap<Pubkey, (i64, bool)> = BTreeMap::new();
    let maker_direction = user.orders[user_order_index].direction.opposite();
    for fulfillment_method in fulfillment_methods.iter() {
        if user.orders[user_order_index].status != OrderStatus::Open {
            break;
        }
        let mut market = perp_market_map.get_ref_mut(&market_index)?;
        let user_order_direction: PositionDirection = user.orders[user_order_index].direction;

        let (fill_base_asset_amount, fill_quote_asset_amount) = match fulfillment_method {
            PerpFulfillmentMethod::AMM(maker_price) => {
                // maker may try to fill their own order (e.g. via jit)
                // if amm takes fill, give maker filler reward
                let (mut maker, mut maker_stats) =
                    if makers_and_referrer.0.contains_key(filler_key) && filler.is_none() {
                        let maker = makers_and_referrer.get_ref_mut(filler_key)?;
                        if maker.authority == user.authority {
                            (None, None)
                        } else {
                            let maker_stats =
                                makers_and_referrer_stats.get_ref_mut(&maker.authority)?;
                            (Some(maker), Some(maker_stats))
                        }
                    } else {
                        (None, None)
                    };

                let (fill_base, fill_quote, _) = fulfill_perp_order_step(
                    market.deref_mut(),
                    user,
                    user_stats,
                    user_order_index,
                    user_key,
                    PerpFulfillmentMethod::AMM(*maker_price),
                    &mut maker.as_deref_mut(),
                    &mut maker_stats.as_deref_mut(),
                    None,
                    None,
                    filler,
                    filler_stats,
                    filler_key,
                    reserve_price_before,
                    valid_oracle_price,
                    limit_price,
                    now,
                    slot,
                    validity_guard_rails,
                    fee_structure,
                    oracle_map,
                    fill_mode.is_liquidation(),
                    amm_jit_allowed,
                    rev_share_escrow,
                    vamm_maker_rebate,
                    promo_fee_tier,
                    builder_fee_allowed,
                )?;
                (fill_base, fill_quote)
            }
            PerpFulfillmentMethod::Match(maker_key, maker_order_index, maker_price) => {
                let mut maker = makers_and_referrer.get_ref_mut(maker_key)?;
                let maker_is_isolated_position =
                    maker.get_perp_position(market_index)?.is_isolated();
                let mut maker_stats = if maker.authority == user.authority {
                    None
                } else {
                    Some(makers_and_referrer_stats.get_ref_mut(&maker.authority)?)
                };

                let mut maker_opt: Option<&mut User> = Some(&mut *maker);
                let mut maker_stats_opt: Option<&mut UserStats> = maker_stats.as_deref_mut();
                let (fill_base, fill_quote, maker_fill_base) = fulfill_perp_order_step(
                    market.deref_mut(),
                    user,
                    user_stats,
                    user_order_index,
                    user_key,
                    PerpFulfillmentMethod::Match(*maker_key, *maker_order_index, *maker_price),
                    &mut maker_opt,
                    &mut maker_stats_opt,
                    Some(*maker_order_index as usize),
                    Some(maker_key),
                    filler,
                    filler_stats,
                    filler_key,
                    reserve_price_before,
                    valid_oracle_price,
                    limit_price,
                    now,
                    slot,
                    validity_guard_rails,
                    fee_structure,
                    oracle_map,
                    fill_mode.is_liquidation(),
                    amm_jit_allowed,
                    rev_share_escrow,
                    vamm_maker_rebate,
                    promo_fee_tier,
                    builder_fee_allowed,
                )?;

                if maker_fill_base != 0 {
                    update_maker_fills_map(
                        &mut maker_fills,
                        maker_key,
                        maker_direction,
                        maker_fill_base,
                        maker_is_isolated_position,
                    )?;
                }

                (fill_base, fill_quote)
            }
        };

        base_asset_amount = base_asset_amount.safe_add(fill_base_asset_amount)?;
        quote_asset_amount = quote_asset_amount.safe_add(fill_quote_asset_amount)?;
        // Only real fills update volume stats and stamp `last_trade_ts`; a
        // zero-fill step must not refresh the last-trade timestamp (it gates
        // the trigger-price last-fill leg).
        if fill_base_asset_amount != 0 {
            market.market_stats.update_volume_24h(
                fill_quote_asset_amount,
                user_order_direction,
                now,
            )?;
        }
    }

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

    if !fill_mode.is_liquidation() {
        // if the maker is long, the user sold so
        let _taker_base_asset_amount_delta = if maker_direction == PositionDirection::Long {
            base_asset_amount as i64
        } else {
            -(base_asset_amount as i64)
        };

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
        let maker = makers_and_referrer.get_ref_mut(&maker_key)?;

        let maker_breaker_tripped = if maker.authority == user.authority {
            user_stats.is_equity_breaker_tripped()
        } else {
            makers_and_referrer_stats
                .get_ref(&maker.authority)?
                .is_equity_breaker_tripped()
        };

        let (margin_type, maker_risk_increasing) = select_margin_type_for_perp_maker(
            &maker,
            maker_base_asset_amount_filled,
            market_index,
        )?;

        let margin_type_config = if maker_is_isolated_position {
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
        if !fill_mode.is_liquidation() {
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
    let record = get_order_action_record(
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
    emit_stack::<_, { OrderActionRecord::SIZE }>(record)
}

#[allow(clippy::too_many_arguments)]
/// Settle a single `AmmHouse` fill (sole-AMM step, or a JIT slice inside a
/// Match step). Returns `(base_filled, quote_filled)` to accumulate.
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
    is_jit_within_match: bool,
    is_liquidation: bool,
    maker: &mut Option<&mut User>,
    maker_stats: &mut Option<&mut UserStats>,
    filler: &mut Option<&mut User>,
    filler_stats: &mut Option<&mut UserStats>,
    filler_key: &Pubkey,
    rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
    fee_structure: &FeeStructure,
    oracle_map: &mut OracleMap,
    now: i64,
    slot: u64,
    vamm_maker_rebate: bool,
    promo_fee_tier: u8,
    builder_fee_allowed: bool,
) -> VelocityResult<(u64, u64)> {
    let slot_duration = oracle_map.slot_duration;
    // For sole-AMM steps with a post_only taker, override the
    // fill's quote at the order's limit price (the taker, acting
    // as maker, transacts at limit; the AMM captures the curve
    // ↔ limit gap as spread surplus). For JIT slices inside a
    // Match step, `AmmJitQuoter::try_fill_solo` already returns
    // the jit-price quote + curve↔jit surplus — pass through.
    let (taker_quote, taker_surplus) =
        if !is_jit_within_match && order_post_only && taker_limit_price.is_some() {
            crate::controller::position::calculate_quote_asset_amount_surplus(
                taker_direction,
                fill.quote_filled,
                fill.base_filled,
                taker_limit_price.unwrap(),
            )?
        } else {
            (fill.quote_filled, fill.quote_asset_amount_surplus)
        };

    let reward_referrer =
        can_reward_user_with_referral_reward(market.market_index, rev_share_escrow);
    let reward_filler = can_reward_user_with_perp_pnl(filler, market.market_index)
        || (!is_jit_within_match && can_reward_user_with_perp_pnl(maker, market.market_index));

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
        taker_surplus,
        order_post_only,
        market.fee_adjustment,
        builder_order_fee_bps,
        vamm_maker_rebate,
        market.taker_fee_addon_tenth_bps,
        now,
        promo_fee_tier,
        slot_duration,
    )?;
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
    } else if !is_jit_within_match {
        if let Some(maker_user) = maker.as_mut() {
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
    } else if is_jit_within_match {
        OrderActionExplanation::OrderFilledWithAMMJit
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
    fee_structure: &FeeStructure,
    oracle_map: &mut OracleMap,
    is_liquidation: bool,
    now: i64,
    slot: u64,
    promo_fee_tier: u8,
    builder_fee_allowed: bool,
) -> VelocityResult<(u64, u64, u64)> {
    let slot_duration = oracle_map.slot_duration;
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
        &MarketType::Perp,
        market.fee_adjustment,
        builder_order_fee_bps,
        market.taker_fee_addon_tenth_bps,
        now,
        promo_fee_tier,
        slot_duration,
    )?;
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

/// Unified fulfill step. Replaces `fulfill_perp_order_with_amm` and
/// `fulfill_perp_order_with_match` with a single matcher-driven path: build
/// the right quoter set from the `PerpFulfillmentMethod`, run `match_take`
/// once, then walk `match.fills` and dispatch per-fill on `FillFeePolicy`.
///
/// Returns `(taker_base_filled, taker_quote_filled, maker_base_filled)`.
/// `maker_base_filled` is the sum of base across `DlobMatch` fills (zero for
/// AMM-only steps).
#[allow(clippy::too_many_arguments)]
pub fn fulfill_perp_order_step(
    market: &mut PerpMarket,
    taker: &mut User,
    taker_stats: &mut UserStats,
    taker_order_index: usize,
    taker_key: &Pubkey,
    method: PerpFulfillmentMethod,
    maker: &mut Option<&mut User>,
    maker_stats: &mut Option<&mut UserStats>,
    maker_order_index: Option<usize>,
    maker_key_opt: Option<&Pubkey>,
    filler: &mut Option<&mut User>,
    filler_stats: &mut Option<&mut UserStats>,
    filler_key: &Pubkey,
    // AmmQuoter::setup is what materialises the pre-quote reserves now;
    // this orchestrator-supplied value is informational only. Kept on the
    // signature to avoid churning all call sites.
    _reserve_price_before: u64,
    valid_oracle_price: Option<i64>,
    taker_limit_price: Option<u64>,
    now: i64,
    slot: u64,
    validity_guard_rails: &ValidityGuardRails,
    fee_structure: &FeeStructure,
    oracle_map: &mut OracleMap,
    is_liquidation: bool,
    // False when a hard gate fires (pause/drawdown/volatility/oracle); blocks
    // AMM JIT in the Match branch. Excludes auction-timing gates.
    amm_jit_allowed: bool,
    rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
    vamm_maker_rebate: bool,
    promo_fee_tier: u8,
    // False when the taker does not meet initial margin. The fill proceeds and
    // charges no builder fee. `fulfill_perp_order` computes it and documents
    // the rule.
    builder_fee_allowed: bool,
) -> VelocityResult<(u64, u64, u64)> {
    // ---- 1. Capture taker order fields. ----
    let market_index = market.market_index;
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
    let taker_order_has_builder = taker.orders[taker_order_index].is_has_builder();
    if taker_order_has_builder && rev_share_escrow.is_none() {
        // `fill_perp_order` rejects this case up front when builder codes are
        // enabled outside of liquidation, so reaching here means either the
        // feature is globally disabled or this is a liquidation fill — in both
        // the builder fee is intentionally skipped.
        msg!("Order has builder but no escrow account included; builder fee skipped.");
    }

    let slot_duration = oracle_map.slot_duration;
    let oracle_pd = *oracle_map.get_price_data(&market.oracle_id())?;
    let oracle_price = oracle_pd.price;
    let mm_oracle_price_data =
        market.get_mm_oracle_price_data(oracle_pd, slot, validity_guard_rails, slot_duration)?;
    let sanitize_clamp_denom = market.get_sanitize_clamp_denominator()?;

    // Construct the AMM-side `Quoter` and run setup once. `Quoter::setup`
    // is the sole non-admin AMM-refresh path. The quoter lives until the
    // end of this function — the AMM-match arm uses it directly; the
    // DLOB-Match arm explicitly drops it before constructing an
    // `AmmJitQuoter` over the same `&mut market`.
    //
    // Compute the refresh validity only when setup would actually project.
    // `setup` reads `oracle_validity` solely inside `project_and_apply`,
    // which no-ops when the curve was already refreshed at this slot (the
    // orchestrator's routing phase, a prior step, or a keeper crank). In that
    // dominant case the validity is unused, so skip the `oracle_validity`
    // recompute and pass `None`; `project_and_apply` returns before reading it.
    let amm_refresh_validity = if market.amm.last_update_slot < slot {
        crate::vlp::amm::refresh::compute_amm_refresh_validity_with_guard_rails(
            market,
            &mm_oracle_price_data,
            validity_guard_rails,
            slot_duration,
        )?
    } else {
        None
    };
    let market_stats_snapshot = market.market_stats;
    let safe_oracle = mm_oracle_price_data.get_safe_oracle_price_data();
    let order_tick_size = market.order_tick_size;
    let order_step_size = market.order_step_size;
    let market_status_local = market.status;
    let market_config_local = market.market_config;
    let setup_ctx = QuoteContext {
        stats: &market_stats_snapshot,
        oracle: &safe_oracle,
        mm_oracle: Some(&mm_oracle_price_data),
        oracle_validity: amm_refresh_validity,
        fee_budget: 0,
        tick: order_tick_size,
        step_size: order_step_size,
        slot,
        slot_duration,
        base_precision: BASE_PRECISION_U64,
        market_status: market_status_local,
        market_config: market_config_local,
    };
    let mut amm_quoter = AmmQuoter::for_amm(&mut market.amm);
    <AmmQuoter as Quoter>::setup(&mut amm_quoter, &setup_ctx)?;
    let reserve_after_setup = amm_quoter.amm.reserve_price()?;
    let (amm_bid_price, amm_ask_price) = amm_quoter.amm_bid_ask(reserve_after_setup)?;
    let amm_base_spread = amm_quoter.amm_base_spread();
    // Snapshot the just-refreshed cached spreads for the mark-twap update
    // below (which takes scalars, not an `&AMM` borrow).
    let amm_long_spread = amm_quoter.amm.long_spread;
    let amm_short_spread = amm_quoter.amm.short_spread;

    // ---- 2. Per-fill validate / event metadata. ----
    let taker_base_unfilled = taker.orders[taker_order_index]
        .get_base_asset_amount_unfilled(Some(taker_existing_position_before))?;
    let taker_price_for_match: Option<u64>;
    let match_maker_price: Option<u64>;
    let maker_existing_position_params: Option<(u64, u64)>;
    match method {
        PerpFulfillmentMethod::AMM(_) => {
            taker_price_for_match = None;
            match_maker_price = None;
            maker_existing_position_params = None;
        }
        PerpFulfillmentMethod::Match(_, m_idx, maker_price) => {
            let m_idx = m_idx as usize;
            let maker_ref = maker
                .as_deref()
                .ok_or_else(print_error!(ErrorCode::DefaultError))?;
            if !are_orders_same_market_but_different_sides(
                &maker_ref.orders[m_idx],
                &taker.orders[taker_order_index],
            ) {
                return Ok((0, 0, 0));
            }
            // Taker's price ceiling for the matcher + per-fill validate:
            // explicit limit, or the auction fallback for market orders.
            let taker_price = match taker_limit_price {
                Some(p) => p,
                None => {
                    // Reborrow `amm_quoter.amm` immutably to read AMM-side
                    // fields without dropping the quoter (it still owns the
                    // &mut for the matcher below). `market.market_stats` is
                    // a disjoint PerpMarket field.
                    let amm_ref: &crate::vlp::amm::AMM = amm_quoter.amm;
                    let amm_available = calculate_amm_available_liquidity(
                        amm_ref,
                        &taker_direction,
                        order_step_size,
                    )?;
                    let min_order_size = market.market_stats.min_order_size;
                    amm_ref.get_fallback_price(
                        &market.market_stats,
                        &taker_direction,
                        amm_available,
                        oracle_price,
                        taker.orders[taker_order_index].seconds_til_expiry(now),
                        min_order_size,
                    )?
                }
            };
            let maker_direction = maker_ref.orders[m_idx].direction;
            let maker_position = maker_ref.get_perp_position(market_index)?;
            taker_price_for_match = Some(taker_price);
            match_maker_price = Some(maker_price);
            maker_existing_position_params =
                maker_position.get_existing_position_params_for_order_action(maker_direction);
        }
    }

    let target_size = taker_base_unfilled;
    if target_size == 0 {
        return Ok((0, 0, 0));
    }

    // ---- 3. Mark-TWAP trade-price hint (deferred update). ----
    // Snapshot the trade-price hint from the pre-fill AMM quote: the AMM's
    // natural ask/bid for AMM-only steps, the maker price for Match steps.
    // The actual `update_mark_twap_with_amm_bid_ask` mutation is deferred
    // until *after* the fill is confirmed non-zero (step 3b): the mark-TWAP is
    // `first-write-wins` within a single `now` timestamp (a same-`now` update
    // sees `since_last == 0` and is a no-op), so a step that quotes but fills
    // zero base — e.g. an AMM-override step the matcher passes over — must not
    // stamp its price and pre-empt a real, same-timestamp maker trade that
    // fills afterwards. The AMM-derived inputs are all captured as scalars
    // here (no `&AMM` borrow), shaped for the target architecture where the
    // AMM is an isolated module surfacing bid/ask/base-spread via its contract.
    let twap_trade_price = match_maker_price.unwrap_or(match taker_direction {
        PositionDirection::Long => amm_ask_price,
        PositionDirection::Short => amm_bid_price,
    });

    // ---- 4. Build QuoteContext for the matcher. AMM-projection fields
    // not needed here — setup has already run on amm_quoter.
    //
    // The matcher reprices DLOB makers off `ctx.oracle.price`
    // (`DlobOrderQuoter::effective_price`), so oracle-offset limit orders
    // must be requoted against the *same* oracle that maker discovery froze
    // their `match_maker_price` at. Discovery prices makers at
    // `mm_oracle_price_data.get_price()`, which is exactly `safe_oracle.price`
    // (`get_price()` returns `safe_oracle_price_data.price`). Passing a
    // default (zero-price) oracle here would reprice an oracle-offset maker to
    // just its offset, so its fill would disagree with the frozen maker price
    // and revert in `validate_fill_price`.
    let stats_snapshot = market.market_stats;
    let ctx = QuoteContext {
        stats: &stats_snapshot,
        oracle: &safe_oracle,
        mm_oracle: None,
        oracle_validity: None,
        fee_budget: 0,
        tick: order_tick_size,
        step_size: order_step_size,
        slot,
        slot_duration,
        base_precision: BASE_PRECISION_U64,
        market_status: MarketStatus::default(),
        market_config: 0,
    };

    // ---- 5. Build quoters + match_take. ----
    let match_result = match method {
        PerpFulfillmentMethod::AMM(override_fill_price) => {
            // Effective price ceiling for the AMM: the order's limit
            // (buffered by maker rebate if post_only), `min`-ed with any
            // override price from the FFM payload, stepped one tick inside
            // the limit. Computed at the orchestrator — the AMM Quoter
            // itself is taker-agnostic.
            let fee_tier = determine_user_fee_tier(
                taker_stats,
                fee_structure,
                &MarketType::Perp,
                now,
                promo_fee_tier,
            )?;
            let effective_taker_limit = crate::math::orders::calculate_effective_amm_taker_limit(
                &taker.orders[taker_order_index],
                taker_limit_price,
                override_fill_price,
                &fee_tier,
                market.fee_adjustment,
                market.order_tick_size,
            )?;
            // Reuse the `amm_quoter` constructed at the top of this fn —
            // it's already been setup and holds &mut market.amm. The AMM is
            // the sole continuous maker, so it takes the dedicated analytical
            // fill path rather than the discrete level walk.
            amm_quoter.validate_for_fill(taker_direction)?;
            // An AMM fill takes at most the per-fill available liquidity: half
            // of the side's depth, capped by `max_fill_reserve_fraction`. The
            // continuous fill below further clamps to the taker's limit price
            // and the AMM's hard reserve bound.
            let amm_available = {
                let amm_ref: &crate::vlp::amm::AMM = amm_quoter.amm;
                calculate_amm_available_liquidity(amm_ref, &taker_direction, order_step_size)?
            };
            crate::controller::matching::fill_amm_only(
                &mut amm_quoter,
                &ctx,
                taker_direction,
                target_size.min(amm_available),
                effective_taker_limit,
            )?
        }
        PerpFulfillmentMethod::Match(_, m_idx, maker_price) => {
            // Release the AMM-only quoter so `AmmJitQuoter::from_match_context`
            // can take `&mut market` (`amm_quoter` holds `&mut market.amm`).
            // The AMM has already been refreshed by `amm_quoter`'s setup; the
            // JIT quoter doesn't re-refresh. Discarding the binding ends the
            // borrow scope (no `Drop` impl to run).
            let _ = amm_quoter;
            let m_idx = m_idx as usize;
            let maker_user = maker
                .as_deref_mut()
                .ok_or_else(print_error!(ErrorCode::DefaultError))?;
            // Position-capped unfilled: for a reduce-only maker this is
            // min(order unfilled, |position|), so the fill can never grow
            // or flip the maker's position. Sizes both the JIT split and
            // the DLOB quoter's capacity.
            let maker_unfilled = maker_user.orders[m_idx].get_base_asset_amount_unfilled(Some(
                maker_user
                    .get_perp_position(market_index)?
                    .base_asset_amount,
            ))?;
            // Add the AMM as a JIT maker only when allowed; a hard gate
            // (pause/drawdown) must keep it off the reserves. Else: DLOB only.
            if amm_jit_allowed {
                let taker_has_limit_price =
                    taker.orders[taker_order_index].has_limit_price(slot)?;
                let mut amm_jit = crate::vlp::amm::AmmJitQuoter::from_match_context(
                    market,
                    maker_price,
                    taker_direction,
                    valid_oracle_price,
                    target_size,
                    maker_unfilled,
                    taker_has_limit_price,
                )?;
                let mut dlob = DlobOrderQuoter::new(&mut maker_user.orders[m_idx], maker_unfilled);
                let mut quoters: Vec<&mut dyn QuoterCommit> = vec![&mut amm_jit, &mut dlob];
                crate::controller::matching::match_take(
                    &mut quoters,
                    &ctx,
                    taker_direction,
                    target_size,
                    taker_price_for_match,
                )?
            } else {
                let mut dlob = DlobOrderQuoter::new(&mut maker_user.orders[m_idx], maker_unfilled);
                let mut quoters: Vec<&mut dyn QuoterCommit> = vec![&mut dlob];
                crate::controller::matching::match_take(
                    &mut quoters,
                    &ctx,
                    taker_direction,
                    target_size,
                    taker_price_for_match,
                )?
            }
        }
    };

    if match_result.total_base_filled == 0 {
        return Ok((0, 0, 0));
    }

    // ---- 3b. Deferred Mark-TWAP update (see step 3). ----
    // Reached only once this step actually traded, so a zero-fill quote can
    // never stamp the TWAP ahead of a real same-`now` maker trade. Uses the
    // pre-fill AMM scalars captured above — the fill does not touch the
    // mark/oracle TWAP inputs read here, so the stamped value is identical to
    // computing it before the swap, just gated on a real fill.
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

    // An AmmHouse fill emitted from a Match step is always a JIT slice
    // (the only AMM-side quoter present in a Match step is `AmmJitQuoter`),
    // so we map the explanation off the method alone.
    let is_jit_within_match = matches!(method, PerpFulfillmentMethod::Match(..));

    // ---- 7. Per-fill dispatch. ----
    let mut total_base_filled = 0u64;
    let mut total_quote_filled = 0u64;
    let mut maker_base_filled = 0u64;

    for (_quoter_id, fill) in match_result.fills.iter() {
        if fill.base_filled == 0 {
            continue;
        }
        match fill.fee_policy {
            FillFeePolicy::AmmHouse => {
                let (base_filled, quote_filled) = settle_amm_house_fill(
                    fill,
                    market,
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
                    is_jit_within_match,
                    is_liquidation,
                    maker,
                    maker_stats,
                    filler,
                    filler_stats,
                    filler_key,
                    rev_share_escrow,
                    fee_structure,
                    oracle_map,
                    now,
                    slot,
                    vamm_maker_rebate,
                    promo_fee_tier,
                    builder_fee_allowed,
                )?;
                total_base_filled = total_base_filled.safe_add(base_filled)?;
                total_quote_filled = total_quote_filled.safe_add(quote_filled)?;
            }
            FillFeePolicy::DlobMatch => {
                let (base_filled, quote_filled, maker_filled) = settle_dlob_match_fill(
                    fill,
                    market,
                    taker,
                    taker_stats,
                    taker_position_index,
                    taker_order_index,
                    taker_key,
                    taker_direction,
                    taker_existing_position_params_before,
                    maker,
                    maker_stats,
                    maker_order_index,
                    maker_key_opt,
                    maker_existing_position_params,
                    match_maker_price,
                    taker_price_for_match,
                    oracle_price,
                    filler,
                    filler_stats,
                    filler_key,
                    rev_share_escrow,
                    fee_structure,
                    oracle_map,
                    is_liquidation,
                    now,
                    slot,
                    promo_fee_tier,
                    builder_fee_allowed,
                )?;
                total_base_filled = total_base_filled.safe_add(base_filled)?;
                total_quote_filled = total_quote_filled.safe_add(quote_filled)?;
                maker_base_filled = maker_base_filled.safe_add(maker_filled)?;
            }
        }
    }

    if total_base_filled == 0 {
        return Ok((0, 0, 0));
    }

    // ---- 9. Finalize open-orders counters once-per-order. ----
    if taker.orders[taker_order_index].get_base_asset_amount_unfilled(None)? == 0 {
        taker.decrement_open_orders(taker.orders[taker_order_index].has_auction());
        taker.perp_positions[taker_position_index].open_orders -= 1;
    }

    if maker_base_filled > 0 {
        if let Some(m_idx) = maker_order_index {
            if let Some(maker_user) = maker.as_deref_mut() {
                let maker_position_index =
                    get_position_index(&maker_user.perp_positions, market.market_index)?;
                if maker_user.orders[m_idx].get_base_asset_amount_unfilled(None)? == 0 {
                    maker_user.decrement_open_orders(maker_user.orders[m_idx].has_auction());
                    maker_user.perp_positions[maker_position_index].open_orders -= 1;
                }
            }
        }
    }

    Ok((total_base_filled, total_quote_filled, maker_base_filled))
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
) -> VelocityResult {
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

    if user.orders[order_index].triggered() {
        msg!("Order is already triggered");
        return Ok(());
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
            // ~8s minimum, expressed in actual slots
            Millis::from_secs(8)
                .to_slots_ceil(state.slot_duration())
                .min(u8::MAX as u64) as u8,
            Some(&perp_market),
            state.slot_duration(),
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

            return Ok(());
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

    Ok(())
}

fn update_trigger_order_params(
    order: &mut Order,
    oracle_price_data: &OraclePriceData,
    slot: u64,
    min_auction_duration: u8,
    perp_market: Option<&PerpMarket>,
    slot_duration: SlotDuration,
) -> VelocityResult {
    order.trigger_condition = match order.trigger_condition {
        OrderTriggerCondition::Above => OrderTriggerCondition::TriggeredAbove,
        OrderTriggerCondition::Below => OrderTriggerCondition::TriggeredBelow,
        _ => {
            return Err(print_error!(ErrorCode::InvalidTriggerOrderCondition)());
        }
    };

    // ~60s: a reduce-only trigger left resting this long is flagged safe for
    // the relaxed oracle-delay gate.
    if slot.saturating_sub(order.slot) > Millis::from_secs(60).to_slots(slot_duration)
        && order.reduce_only
    {
        order.add_bit_flag(OrderBitFlag::SafeTriggerOrder);
    }

    order.slot = slot;

    let (auction_duration, auction_start_price, auction_end_price) =
        calculate_auction_params_for_trigger_order(
            order,
            oracle_price_data,
            min_auction_duration,
            perp_market,
            slot_duration,
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
        let user_position = user.get_perp_position_mut(market.market_index)?;
        controller::position::update_quote_asset_and_break_even_amount(
            user_position,
            market,
            -filler_reward.cast()?,
        )?;

        filler.update_last_active_slot(slot);
        // Dont throw error if filler doesnt have position available
        let filler_position = match filler.force_get_perp_position_mut(market.market_index) {
            Ok(position) => position,
            Err(_) => return Ok(0),
        };
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
