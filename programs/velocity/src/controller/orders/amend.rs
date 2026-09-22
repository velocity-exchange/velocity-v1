//! Cancelling and modifying an order that already exists.
//!
//! Every cancel path ends in [`cancel_order`], which is the one place that
//! unwinds what an open order reserved. A modify is a cancel and a re-place of
//! the merged params.

use super::*;

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
    let scope = CancelScope {
        market: market_type.zip(market_index),
        direction,
        skip_isolated_positions,
        isolated_market_indexes: user
            .perp_positions
            .iter()
            .filter(|position| position.is_isolated())
            .map(|position| position.market_index)
            .collect(),
    };

    let mut canceled_order_ids: Vec<u32> = vec![];
    for order_index in 0..user.orders.len() {
        if !scope.covers(&user.orders[order_index]) {
            continue;
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

/// The orders one sweep cancels.
///
/// A sweep names one market, or every market the user has an order in. The
/// latter form skips isolated positions, since each carries its own collateral.
struct CancelScope {
    /// The one market the sweep covers, as `(type, index)`. `None` covers
    /// every market.
    market: Option<(MarketType, u16)>,
    direction: Option<PositionDirection>,
    skip_isolated_positions: bool,
    isolated_market_indexes: Vec<u16>,
}

impl CancelScope {
    /// Whether the sweep cancels this order.
    fn covers(&self, order: &Order) -> bool {
        if order.status != OrderStatus::Open {
            return false;
        }

        // A placed trigger's live order rests on the CLOB. Its shadow slot
        // is reclaimed only through a CLOB removal path, which is
        // `cancel_order_v1` or one of the cranks. There the book and the
        // aggregates unwind together.
        if order.is_placed_on_clob() {
            return false;
        }

        match self.market {
            Some((market_type, market_index)) => {
                if order.market_type != market_type || order.market_index != market_index {
                    return false;
                }
            }
            None => {
                if self.skip_isolated_positions
                    && self.isolated_market_indexes.contains(&order.market_index)
                {
                    return false;
                }
            }
        }

        self.direction
            .is_none_or(|direction| order.direction == direction)
    }
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
        market_type: order_market_type,
        ..
    } = user.orders[order_index];

    let is_perp_order = order_market_type == MarketType::Perp;

    validate!(order_status == OrderStatus::Open, ErrorCode::OrderNotOpen)?;

    // A placed trigger's live order rests on the CLOB. The slot here is a
    // shadow whose open-order count the CLOB order carries. Cancelling the
    // shadow would strand the CLOB order and unwind its accounting twice, so
    // the cancel must go through `cancel_order_v1`. A bulk sweep skips these
    // slots.
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
        let mut bit_flags = 0;
        if is_perp_order {
            let position_index = get_position_index(&user.perp_positions, order_market_index)?;
            if user.perp_positions[position_index].is_isolated() {
                bit_flags = set_order_bit_flag(bit_flags, true, OrderBitFlag::IsIsolatedPosition);
            }
        }

        emit_cancel_record(
            &user.orders[order_index],
            &CancelRecord {
                user_key,
                filler_key,
                filler_reward,
                explanation,
                bit_flags,
            },
            maps.oracle_map.get_price_data(&oracle_id)?.price,
            now,
        )?;
    }

    release_order_reservation(user, order_index, is_perp_order)
}

/// What one cancel record says beyond the order itself.
struct CancelRecord<'a> {
    user_key: &'a Pubkey,
    filler_key: Option<&'a Pubkey>,
    filler_reward: u64,
    explanation: OrderActionExplanation,
    bit_flags: u8,
}

/// Emit the record one cancel makes.
fn emit_cancel_record(
    order: &Order,
    record: &CancelRecord,
    oracle_price: i64,
    now: i64,
) -> VelocityResult {
    let (taker, taker_order, maker, maker_order) =
        get_taker_and_maker_for_order_record(record.user_key, order);

    let order_action_record = get_order_action_record(
        now,
        OrderAction::Cancel,
        record.explanation,
        order.market_index,
        record.filler_key.copied(),
        None,
        Some(record.filler_reward),
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
        record.bit_flags,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )?;

    emit_stack::<_, { OrderActionRecord::SIZE }>(order_action_record)
}

/// Release what the cancelled order reserved, and retire its slot.
///
/// A trigger order that never fired reserved nothing, so only its open-order
/// count is released.
fn release_order_reservation(
    user: &mut User,
    order_index: usize,
    is_perp_order: bool,
) -> VelocityResult {
    let Order {
        market_index: order_market_index,
        direction: order_direction,
        ..
    } = user.orders[order_index];

    user.decrement_open_orders();

    // only decrease open/bids ask if it's not a trigger order or if it's been triggered
    let update_open_bids_and_asks = user.orders[order_index].update_open_bids_and_asks();

    if is_perp_order {
        // Decrement open orders for existing position
        let position_index = get_position_index(&user.perp_positions, order_market_index)?;

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
    } else {
        let spot_position_index = user.get_spot_position_index(order_market_index)?;

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
    }

    user.orders[order_index].status = OrderStatus::Canceled;

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

    let Some(order_index) = find_order_to_modify(&user, &order_id, &modify_order_params)? else {
        return Ok(());
    };
    let existing_order = user.orders[order_index];

    // A builder-coded order's fee attribution lives in the `RevenueShareEscrow`
    // row keyed to its order id. A modify cancels and re-places under a new
    // order id, and it does not carry that row across. The order becomes a
    // no-builder order and the builder fee is dropped (OtterSec #82). The
    // modify is refused instead. A taker changes a builder-coded order by
    // cancelling it and placing again with builder params.
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

        place_perp_trigger_order(
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

/// The slot of the order a modify names.
///
/// `None` means the order is gone and the caller asked for a modify that may
/// miss. A modify that must apply reports the miss as an error instead.
fn find_order_to_modify(
    user: &User,
    order_id: &ModifyOrderId,
    modify_order_params: &ModifyOrderParams,
) -> VelocityResult<Option<usize>> {
    let found = match order_id {
        ModifyOrderId::UserOrderId(user_order_id) => user
            .get_order_index_by_user_order_id(*user_order_id)
            .inspect_err(|_| msg!("User order id {} not found", user_order_id)),
        ModifyOrderId::OrderId(order_id) => user
            .get_order_index(*order_id)
            .inspect_err(|_| msg!("Order id {} not found", order_id)),
    };

    match found {
        Ok(order_index) => Ok(Some(order_index)),
        Err(e) if modify_order_params.must_modify() => Err(e),
        Err(_) => Ok(None),
    }
}

fn merge_modify_order_params_with_existing_order(
    existing_order: &Order,
    modify_order_params: &ModifyOrderParams,
) -> VelocityResult<Option<OrderParams>> {
    let Some(base_asset_amount) = merged_base_asset_amount(existing_order, modify_order_params)?
    else {
        return Ok(None);
    };

    Ok(Some(OrderParams {
        order_type: existing_order.order_type,
        market_type: existing_order.market_type,
        direction: modify_order_params
            .direction
            .unwrap_or(existing_order.direction),
        user_order_id: existing_order.user_order_id,
        base_asset_amount,
        price: modify_order_params.price.unwrap_or(existing_order.price),
        market_index: existing_order.market_index,
        reduce_only: modify_order_params
            .reduce_only
            .unwrap_or(existing_order.reduce_only),
        post_only: merged_post_only(existing_order, modify_order_params),
        // Preserve the recross gate across a modify. A triggered order that
        // must observe the price recross before it re-arms carries
        // `AwaitingTriggerRecross`. Rebuilding with `bit_flags = 0` would
        // clear the flag. A user could then re-arm the order by modifying it,
        // with the price never recrossing.
        bit_flags: existing_order.bit_flags & (OrderBitFlag::AwaitingTriggerRecross as u8),
        max_ts: modify_order_params.max_ts.or(Some(existing_order.max_ts)),
        trigger_price: modify_order_params
            .trigger_price
            .or(Some(existing_order.trigger_price)),
        trigger_condition: merged_trigger_condition(existing_order, modify_order_params),
        oracle_price_offset: modify_order_params
            .oracle_price_offset
            .or(Some(existing_order.oracle_price_offset)),
        activation_delay_slots: modify_order_params.activation_delay_slots,
        builder_idx: None,
        builder_fee_tenth_bps: None,
    }))
}

/// The base the modified order carries.
///
/// `None` means the modify has nothing left to place. The caller asked for a
/// size it has already filled.
fn merged_base_asset_amount(
    existing_order: &Order,
    modify_order_params: &ModifyOrderParams,
) -> VelocityResult<Option<u64>> {
    match modify_order_params.base_asset_amount {
        Some(base_asset_amount) if modify_order_params.exclude_previous_fill() => Ok(Some(
            base_asset_amount.saturating_sub(existing_order.base_asset_amount_filled),
        )
        .filter(|base_asset_amount| *base_asset_amount != 0)),
        Some(base_asset_amount) => Ok(Some(base_asset_amount)),
        None => existing_order
            .get_base_asset_amount_unfilled(None)
            .map(Some),
    }
}

/// The post-only rule the modified order carries.
fn merged_post_only(
    existing_order: &Order,
    modify_order_params: &ModifyOrderParams,
) -> PostOnlyParam {
    modify_order_params
        .post_only
        .unwrap_or(if existing_order.post_only {
            PostOnlyParam::MustPostOnly
        } else {
            PostOnlyParam::None
        })
}

/// The trigger condition the modified order carries.
///
/// A condition that already fired re-arms as its untriggered form.
fn merged_trigger_condition(
    existing_order: &Order,
    modify_order_params: &ModifyOrderParams,
) -> OrderTriggerCondition {
    modify_order_params
        .trigger_condition
        .unwrap_or(match existing_order.trigger_condition {
            OrderTriggerCondition::TriggeredAbove | OrderTriggerCondition::Above => {
                OrderTriggerCondition::Above
            }
            OrderTriggerCondition::TriggeredBelow | OrderTriggerCondition::Below => {
                OrderTriggerCondition::Below
            }
        })
}
