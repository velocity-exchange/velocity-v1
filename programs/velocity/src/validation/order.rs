use crate::{
    controller::position::PositionDirection,
    error::{ErrorCode, VelocityResult},
    math::{
        casting::Cast,
        orders::{calculate_base_asset_amount_to_fill_up_to_limit_price, is_multiple_of_step_size},
    },
    msg,
    state::{
        paused_operations::PerpOperation,
        perp_market::PerpMarket,
        user::{Order, OrderTriggerCondition, OrderType},
    },
    validate,
};

#[cfg(test)]
mod test;

pub fn validate_order(
    order: &Order,
    market: &PerpMarket,
    valid_oracle_price: Option<i64>,
    slot: u64,
) -> VelocityResult {
    match order.order_type {
        OrderType::Market => validate_market_order(
            order,
            market.order_step_size,
            market.market_stats.min_order_size,
        )?,
        OrderType::Limit => validate_limit_order(order, market, valid_oracle_price, slot)?,
        OrderType::TriggerMarket => validate_trigger_market_order(
            order,
            market.order_step_size,
            market.market_stats.min_order_size,
        )?,
        OrderType::TriggerLimit => validate_trigger_limit_order(
            order,
            market.order_step_size,
            market.market_stats.min_order_size,
        )?,
        OrderType::Oracle => validate_oracle_order(
            order,
            market.order_step_size,
            market.market_stats.min_order_size,
        )?,
    }

    Ok(())
}

fn validate_market_order(order: &Order, step_size: u64, min_order_size: u64) -> VelocityResult {
    validate_base_asset_amount(order, step_size, min_order_size, order.reduce_only)?;

    validate!(
        order.price > 0,
        ErrorCode::InvalidOrderLimitPrice,
        "Market order must carry the worst price it accepts"
    )?;

    validate!(
        order.trigger_price == 0,
        ErrorCode::InvalidOrderTrigger,
        "Market should not have trigger price"
    )?;

    validate!(
        !(order.post_only),
        ErrorCode::InvalidOrderPostOnly,
        "Market order can not be post only"
    )?;

    validate!(
        !(order.has_oracle_price_offset()),
        ErrorCode::InvalidOrderOracleOffset,
        "Market order can not have oracle offset"
    )?;

    validate!(
        !(order.immediate_or_cancel),
        ErrorCode::InvalidOrderIOC,
        "Market order can not be immediate or cancel"
    )?;

    Ok(())
}

fn validate_oracle_order(order: &Order, step_size: u64, min_order_size: u64) -> VelocityResult {
    validate_base_asset_amount(order, step_size, min_order_size, order.reduce_only)?;

    validate!(
        order.has_oracle_price_offset(),
        ErrorCode::InvalidOrderOracleOffset,
        "Oracle order must carry the oracle offset it accepts"
    )?;

    validate!(
        order.trigger_price == 0,
        ErrorCode::InvalidOrderTrigger,
        "Oracle order should not have trigger price"
    )?;

    validate!(
        !(order.post_only),
        ErrorCode::InvalidOrderPostOnly,
        "Oracle order can not be post only"
    )?;

    validate!(
        order.price == 0,
        ErrorCode::InvalidOrderLimitPrice,
        "Oracle order can not have a price"
    )?;

    validate!(
        !(order.immediate_or_cancel),
        ErrorCode::InvalidOrderIOC,
        "Oracle order can not be immediate or cancel"
    )?;

    Ok(())
}

fn validate_limit_order(
    order: &Order,
    market: &PerpMarket,
    valid_oracle_price: Option<i64>,
    slot: u64,
) -> VelocityResult {
    validate_base_asset_amount(
        order,
        market.order_step_size,
        market.market_stats.min_order_size,
        order.reduce_only || order.is_jit_maker(),
    )?;

    // A limit order must carry a fixed price. An oracle-floating limit cannot
    // rest on a CLOB, so it would strand in a `User.orders` slot where nothing
    // fills it. A maker that wants an oracle-relative quote uses a PropAMM
    // quoter instead.
    validate!(
        !(order.has_oracle_price_offset()),
        ErrorCode::InvalidOrderOracleOffset,
        "Limit order can not have oracle offset"
    )?;

    validate!(
        order.price != 0,
        ErrorCode::InvalidOrderLimitPrice,
        "Limit order price == 0"
    )?;

    validate!(
        order.trigger_price == 0,
        ErrorCode::InvalidOrderTrigger,
        "Limit order should not have trigger price"
    )?;

    if order.post_only {
        validate_post_only_order(order, market, valid_oracle_price, slot)?;
    }

    Ok(())
}

fn validate_post_only_order(
    order: &Order,
    market: &PerpMarket,
    valid_oracle_price: Option<i64>,
    slot: u64,
) -> VelocityResult {
    // jit maker can fill against amm
    if order.is_jit_maker() {
        return Ok(());
    }

    if market.is_operation_paused(PerpOperation::AmmFill) {
        return Ok(());
    }

    let limit_price =
        order.force_get_limit_price(valid_oracle_price, None, market.order_tick_size)?;

    let base_asset_amount_market_can_fill = calculate_base_asset_amount_to_fill_up_to_limit_price(
        order,
        market,
        Some(limit_price),
        None,
    )?;

    if base_asset_amount_market_can_fill != 0 {
        msg!(
            "Post-only order can immediately fill {} base asset amount",
            base_asset_amount_market_can_fill,
        );

        if !market.amm.is_fresh_at(slot) {
            msg!(
                "market.amm.last_update_slot={} behind current slot={}",
                market.amm.last_update_slot(),
                slot
            );
        }

        let mut invalid = true;
        if let Some(valid_oracle_price) = valid_oracle_price {
            if (valid_oracle_price > limit_price.cast()?
                && order.direction == PositionDirection::Long)
                || (valid_oracle_price < limit_price.cast()?
                    && order.direction == PositionDirection::Short)
            {
                invalid = false;
            }
        }

        if invalid {
            return Err(ErrorCode::PlacePostOnlyLimitFailure);
        }
    }

    Ok(())
}

fn validate_trigger_limit_order(
    order: &Order,
    step_size: u64,
    min_order_size: u64,
) -> VelocityResult {
    validate_base_asset_amount(order, step_size, min_order_size, order.reduce_only)?;

    if !matches!(
        order.trigger_condition,
        OrderTriggerCondition::Above | OrderTriggerCondition::Below
    ) {
        msg!("Invalid trigger condition, must be Above or Below");
        return Err(ErrorCode::InvalidTriggerOrderCondition);
    }

    validate!(
        order.price != 0,
        ErrorCode::InvalidOrderLimitPrice,
        "Trigger limit order price == 0"
    )?;

    validate!(
        order.trigger_price != 0,
        ErrorCode::InvalidOrderTrigger,
        "Trigger price == 0"
    )?;

    validate!(
        !(order.post_only),
        ErrorCode::InvalidOrderPostOnly,
        "Trigger limit order can not be post only"
    )?;

    validate!(
        !(order.has_oracle_price_offset()),
        ErrorCode::InvalidOrderOracleOffset,
        "Trigger limit can not have oracle offset"
    )?;

    Ok(())
}

fn validate_trigger_market_order(
    order: &Order,
    step_size: u64,
    min_order_size: u64,
) -> VelocityResult {
    validate_base_asset_amount(order, step_size, min_order_size, order.reduce_only)?;

    if !matches!(
        order.trigger_condition,
        OrderTriggerCondition::Above | OrderTriggerCondition::Below
    ) {
        msg!("Invalid trigger condition, must be Above or Below");
        return Err(ErrorCode::InvalidTriggerOrderCondition);
    }

    validate!(
        order.price == 0,
        ErrorCode::InvalidOrderLimitPrice,
        "Trigger market order should not have price"
    )?;

    validate!(
        order.trigger_price != 0,
        ErrorCode::InvalidOrderTrigger,
        "Trigger market order trigger_price == 0"
    )?;

    validate!(
        !(order.post_only),
        ErrorCode::InvalidOrderPostOnly,
        "Trigger market order can not be post only"
    )?;

    validate!(
        !(order.has_oracle_price_offset()),
        ErrorCode::InvalidOrderOracleOffset,
        "Trigger market order can not have oracle offset"
    )?;

    Ok(())
}

fn validate_base_asset_amount(
    order: &Order,
    step_size: u64,
    min_order_size: u64,
    reduce_only_or_jit_maker: bool,
) -> VelocityResult {
    validate!(
        order.base_asset_amount != 0,
        ErrorCode::InvalidOrderSizeTooSmall,
        "Order base_asset_amount cant be 0"
    )?;

    validate!(
        is_multiple_of_step_size(order.base_asset_amount, step_size)?,
        ErrorCode::InvalidOrderNotStepSizeMultiple,
        "Order base asset amount ({}) not a multiple of the step size ({})",
        order.base_asset_amount,
        step_size
    )?;

    validate!(
        reduce_only_or_jit_maker || order.base_asset_amount >= min_order_size,
        ErrorCode::InvalidOrderMinOrderSize,
        "Order base_asset_amount ({}) < min_order_size ({})",
        order.base_asset_amount,
        min_order_size
    )?;

    Ok(())
}

pub fn validate_order_for_force_reduce_only(
    order: &Order,
    existing_position: i64,
) -> VelocityResult {
    validate!(
        order.reduce_only,
        ErrorCode::InvalidOrderNotRiskReducing,
        "order must be reduce only",
    )?;

    validate!(
        existing_position != 0,
        ErrorCode::InvalidOrderNotRiskReducing,
        "user must have position to submit order",
    )?;

    let existing_position_direction = if existing_position > 0 {
        PositionDirection::Long
    } else {
        PositionDirection::Short
    };

    validate!(
        order.direction != existing_position_direction,
        ErrorCode::InvalidOrderNotRiskReducing,
        "order direction must be opposite of existing position in reduce only mode",
    )?;

    Ok(())
}
