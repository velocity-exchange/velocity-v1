//! Cancelling and modifying an order in a `User.orders` slot.
//!
//! Every slot cancel ends in [`cancel_order`], which is the one place that
//! unwinds what a slot order reserved. A modify is a cancel and a re-place of
//! the merged params. A book order unwinds through `User::close_book_order`
//! or `User::release_swept_orders` instead.

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
    let Some(order_index) = slot_order_to_cancel(user, order_id)? else {
        return Ok(());
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

/// Cancel each slot order `order_ids` names. A placed trigger's slot is
/// skipped, as a bulk sweep skips it, so one such id does not fail the batch.
/// Its live order cancels through `cancel_order_v1`.
pub fn cancel_orders_by_order_ids(
    order_ids: &[u32],
    user: &AccountLoader<User>,
    maps: &mut AccountMaps,
    clock: &Clock,
) -> VelocityResult {
    let user_key = user.key();
    let user = &mut load_mut!(user)?;
    for order_id in order_ids {
        let Some(order_index) = slot_order_to_cancel(user, *order_id)? else {
            continue;
        };

        if user.orders[order_index].is_placed_on_clob() {
            msg!("order {} is placed on the CLOB; skipping", order_id);
            continue;
        }

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
    }

    user.update_last_active_slot(clock.slot);
    Ok(())
}

/// The slot of the open order `order_id` names. `None` is an id this user has
/// not minted yet, which is nothing to cancel.
///
/// A minted id with no open slot is an error. A live book order draws its id
/// from the same counter, so a slot cancel that succeeded here would leave
/// that order resting. It cancels through `cancel_order_v1`.
fn slot_order_to_cancel(user: &User, order_id: u32) -> VelocityResult<Option<usize>> {
    if let Ok(order_index) = user.get_order_index(order_id) {
        return Ok(Some(order_index));
    }

    if order_id != 0 && order_id < user.next_order_id {
        msg!(
            "order id {} is not an open slot order; a book order cancels through cancel_order_v1",
            order_id
        );
        return Err(ErrorCode::OrderDoesNotExist);
    }

    msg!("could not find order id {}", order_id);
    Ok(None)
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
/// A cancel is an exit, so a perp release clamps at the reservation. An order
/// left from the old venue then stays cancellable whatever its aggregates read.
fn release_order_reservation(
    user: &mut User,
    order_index: usize,
    is_perp_order: bool,
) -> VelocityResult {
    if is_perp_order {
        let reservation = OrderReservation::of_order(&user.orders[order_index])?;
        user.release_orders(&reservation, ReleaseCheck::ClampedForExit)?;
    } else {
        release_spot_order_reservation(user, order_index)?;
    }

    user.orders[order_index].status = OrderStatus::Canceled;
    Ok(())
}

/// A spot order holds its count and its unfilled base on its spot position.
fn release_spot_order_reservation(user: &mut User, order_index: usize) -> VelocityResult {
    let order = user.orders[order_index];
    user.decrement_open_orders();

    let spot_position_index = user.get_spot_position_index(order.market_index)?;
    decrease_spot_open_bids_and_asks(
        &mut user.spot_positions[spot_position_index],
        &order.direction,
        order.get_base_asset_amount_unfilled(None)?,
        order.update_open_bids_and_asks(),
    )?;

    user.spot_positions[spot_position_index].open_orders -= 1;
    Ok(())
}

pub enum ModifyOrderId {
    UserOrderId(u8),
    OrderId(u32),
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
        let order_id = user.next_order_id;
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

        carry_trigger_recross(&mut user, &existing_order, order_id);
    }

    Ok(())
}

/// Keep `AwaitingTriggerRecross` on the re-placed order, so a modify cannot
/// re-arm a trigger while the price never recrosses. `order_id` is the id the
/// re-place minted. A skipped re-place left no order under it.
fn carry_trigger_recross(user: &mut User, existing_order: &Order, order_id: u32) {
    if !existing_order.is_bit_flag_set(OrderBitFlag::AwaitingTriggerRecross) {
        return;
    }

    if let Ok(order_index) = user.get_order_index(order_id) {
        user.orders[order_index].add_bit_flag(OrderBitFlag::AwaitingTriggerRecross);
    }
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
        bit_flags: 0,
        max_ts: modify_order_params.max_ts.or(Some(existing_order.max_ts)),
        trigger_price: modify_order_params
            .trigger_price
            .or(Some(existing_order.trigger_price)),
        trigger_condition: merged_trigger_condition(existing_order, modify_order_params),
        oracle_price_offset: modify_order_params
            .oracle_price_offset
            .or(Some(existing_order.oracle_price_offset)),
        activation_delay_slots: None,
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

#[cfg(test)]
mod tests {
    use {
        super::{carry_trigger_recross, slot_order_to_cancel},
        crate::{
            error::ErrorCode,
            state::user::{MarketType, Order, OrderBitFlag, OrderStatus, OrderType, User},
        },
    };

    fn trigger(order_id: u32, bit_flags: u8) -> Order {
        Order {
            order_id,
            status: OrderStatus::Open,
            order_type: OrderType::TriggerLimit,
            market_type: MarketType::Perp,
            bit_flags,
            ..Order::default()
        }
    }

    #[test]
    fn a_modify_keeps_the_recross_gate_on_the_replacement() {
        let existing = trigger(3, OrderBitFlag::AwaitingTriggerRecross as u8);
        let mut user = User::default();
        user.orders[1] = trigger(4, 0);

        carry_trigger_recross(&mut user, &existing, 4);
        assert!(user.orders[1].is_bit_flag_set(OrderBitFlag::AwaitingTriggerRecross));
    }

    #[test]
    fn a_modify_of_an_armed_trigger_adds_no_recross_gate() {
        let existing = trigger(3, 0);
        let mut user = User::default();
        user.orders[1] = trigger(4, 0);

        carry_trigger_recross(&mut user, &existing, 4);
        assert!(!user.orders[1].is_bit_flag_set(OrderBitFlag::AwaitingTriggerRecross));
    }

    #[test]
    fn a_skipped_re_place_carries_nothing() {
        let existing = trigger(3, OrderBitFlag::AwaitingTriggerRecross as u8);
        let mut user = User::default();
        user.orders[1] = trigger(2, 0);

        carry_trigger_recross(&mut user, &existing, 4);
        assert!(!user.orders[1].is_bit_flag_set(OrderBitFlag::AwaitingTriggerRecross));
    }

    fn user_with_next_order_id(next_order_id: u32) -> User {
        let mut user = User {
            next_order_id,
            ..User::default()
        };
        user.orders[2] = trigger(5, 0);
        user
    }

    #[test]
    fn a_cancel_finds_an_open_slot_order() {
        assert_eq!(
            slot_order_to_cancel(&user_with_next_order_id(9), 5),
            Ok(Some(2))
        );
    }

    #[test]
    fn a_cancel_of_a_minted_id_with_no_slot_is_an_error() {
        assert_eq!(
            slot_order_to_cancel(&user_with_next_order_id(9), 7),
            Err(ErrorCode::OrderDoesNotExist)
        );
    }

    #[test]
    fn a_cancel_of_an_id_not_minted_yet_is_a_no_op() {
        let user = user_with_next_order_id(9);
        assert_eq!(slot_order_to_cancel(&user, 9), Ok(None));
        assert_eq!(slot_order_to_cancel(&user, 0), Ok(None));
    }
}
