use {
    crate::{
        controller::position::PositionDirection,
        error::{ErrorCode, VelocityResult},
        math::{
            casting::Cast,
            constants::AUCTION_DERIVE_PRICE_FRACTION,
            orders::standardize_price,
            safe_math::SafeMath,
            time::{Millis, SlotClock},
        },
        msg,
        state::{
            oracle::OraclePriceData,
            perp_market::{ContractTier, PerpMarket},
            user::{Order, OrderBitFlag, OrderType},
        },
        OrderParams,
    },
    std::cmp::min,
};

#[cfg(test)]
mod tests;

pub fn calculate_auction_prices(
    oracle_price_data: &OraclePriceData,
    direction: PositionDirection,
    limit_price: u64,
) -> VelocityResult<(i64, i64)> {
    let oracle_price = oracle_price_data.price;
    let limit_price = limit_price.cast::<i64>()?;
    if limit_price > 0 {
        let (auction_start_price, auction_end_price) = match direction {
            // Long and limit price is better than oracle price
            PositionDirection::Long if limit_price < oracle_price => {
                let limit_derive_start_price =
                    limit_price.safe_sub(limit_price / AUCTION_DERIVE_PRICE_FRACTION)?;
                let oracle_derive_start_price =
                    oracle_price.safe_sub(oracle_price / AUCTION_DERIVE_PRICE_FRACTION)?;

                (
                    limit_derive_start_price.min(oracle_derive_start_price),
                    limit_price,
                )
            }
            // Long and limit price is worse than oracle price
            PositionDirection::Long if limit_price >= oracle_price => {
                let oracle_derive_end_price =
                    oracle_price.safe_add(oracle_price / AUCTION_DERIVE_PRICE_FRACTION)?;

                (oracle_price, limit_price.min(oracle_derive_end_price))
            }
            // Short and limit price is better than oracle price
            PositionDirection::Short if limit_price > oracle_price => {
                let limit_derive_start_price =
                    limit_price.safe_add(limit_price / AUCTION_DERIVE_PRICE_FRACTION)?;
                let oracle_derive_start_price =
                    oracle_price.safe_add(oracle_price / AUCTION_DERIVE_PRICE_FRACTION)?;

                (
                    limit_derive_start_price.max(oracle_derive_start_price),
                    limit_price,
                )
            }
            // Short and limit price is worse than oracle price
            PositionDirection::Short if limit_price <= oracle_price => {
                let oracle_derive_end_price =
                    oracle_price.safe_sub(oracle_price / AUCTION_DERIVE_PRICE_FRACTION)?;

                (oracle_price, limit_price.max(oracle_derive_end_price))
            }
            _ => unreachable!(),
        };

        return Ok((auction_start_price, auction_end_price));
    }

    let auction_end_price = match direction {
        PositionDirection::Long => {
            oracle_price.safe_add(oracle_price / AUCTION_DERIVE_PRICE_FRACTION)?
        }
        PositionDirection::Short => {
            oracle_price.safe_sub(oracle_price / AUCTION_DERIVE_PRICE_FRACTION)?
        }
    };

    Ok((oracle_price, auction_end_price))
}

/// Auction interpolation progress, as elapsed milliseconds over the auction's
/// wall-clock length. Elapsed time is integrated per slot-duration regime and
/// saturates at zero for a same-slot read. It is capped at the auction length.
/// `Order.auction_duration` stores 400ms units, so at the 400ms baseline the ramp's
/// endpoints and wall-clock shape match the earlier per-slot interpolation. That shape
/// holds at every slot-duration gate.
fn auction_progress(order: &Order, slot: u64, slot_clock: SlotClock) -> (u64, u64) {
    let duration_ms = Millis::from_stored_units(order.auction_duration as u64).as_ms();
    let elapsed_ms = slot_clock.elapsed(order.slot, slot).as_ms();
    (min(elapsed_ms, duration_ms), duration_ms)
}

/// The auction's wall-clock progress at `fraction_pct` percent of its length. Use it
/// for a caller that prices a fixed fraction rather than a chain slot, such as
/// place-and-take's `auction_duration_percentage`.
pub fn auction_progress_at_fraction(
    order: &Order,
    fraction_pct: u64,
) -> VelocityResult<(u64, u64)> {
    let duration_ms = Millis::from_stored_units(order.auction_duration as u64).as_ms();
    let elapsed_ms = duration_ms.safe_mul(fraction_pct.min(100))?.safe_div(100)?;
    Ok((elapsed_ms, duration_ms))
}

pub fn calculate_auction_price(
    order: &Order,
    slot: u64,
    tick_size: u64,
    valid_oracle_price: Option<i64>,
    slot_clock: SlotClock,
) -> VelocityResult<u64> {
    calculate_auction_price_with_progress(
        order,
        auction_progress(order, slot, slot_clock),
        tick_size,
        valid_oracle_price,
    )
}

/// Interpolate the auction price at an explicit `(elapsed_ms, duration_ms)` progress
/// pair. [`auction_progress`] and [`auction_progress_at_fraction`] build that pair.
pub fn calculate_auction_price_with_progress(
    order: &Order,
    progress: (u64, u64),
    tick_size: u64,
    valid_oracle_price: Option<i64>,
) -> VelocityResult<u64> {
    match order.order_type {
        OrderType::TriggerMarket if order.is_bit_flag_set(OrderBitFlag::OracleTriggerMarket) => {
            calculate_auction_price_for_oracle_offset_auction(
                order,
                progress,
                tick_size,
                valid_oracle_price,
            )
        }
        OrderType::Market | OrderType::TriggerMarket | OrderType::TriggerLimit => {
            calculate_auction_price_for_fixed_auction(order, progress, tick_size)
        }
        OrderType::Limit => {
            if order.has_oracle_price_offset() {
                calculate_auction_price_for_oracle_offset_auction(
                    order,
                    progress,
                    tick_size,
                    valid_oracle_price,
                )
            } else {
                calculate_auction_price_for_fixed_auction(order, progress, tick_size)
            }
        }
        OrderType::Oracle => calculate_auction_price_for_oracle_offset_auction(
            order,
            progress,
            tick_size,
            valid_oracle_price,
        ),
    }
}

fn calculate_auction_price_for_fixed_auction(
    order: &Order,
    progress: (u64, u64),
    tick_size: u64,
) -> VelocityResult<u64> {
    let (delta_numerator, delta_denominator) = progress;

    let auction_start_price = order.auction_start_price.cast::<u64>()?;
    let auction_end_price = order.auction_end_price.cast::<u64>()?;

    if delta_denominator == 0 {
        return standardize_price(auction_end_price, tick_size, order.direction);
    }

    let price_delta = match order.direction {
        PositionDirection::Long => auction_end_price
            .safe_sub(auction_start_price)?
            .safe_mul(delta_numerator.cast()?)?
            .safe_div(delta_denominator.cast()?)?,
        PositionDirection::Short => auction_start_price
            .safe_sub(auction_end_price)?
            .safe_mul(delta_numerator.cast()?)?
            .safe_div(delta_denominator.cast()?)?,
    };

    let price = match order.direction {
        PositionDirection::Long => auction_start_price.safe_add(price_delta)?,
        PositionDirection::Short => auction_start_price.safe_sub(price_delta)?,
    };

    standardize_price(price, tick_size, order.direction)
}

fn calculate_auction_price_for_oracle_offset_auction(
    order: &Order,
    progress: (u64, u64),
    tick_size: u64,
    valid_oracle_price: Option<i64>,
) -> VelocityResult<u64> {
    let oracle_price = valid_oracle_price.ok_or_else(|| {
        msg!("Could not find oracle too calculate oracle offset auction price");
        ErrorCode::OracleNotFound
    })?;

    let (delta_numerator, delta_denominator) = progress;

    let auction_start_price_offset = order.auction_start_price;
    let auction_end_price_offset = order.auction_end_price;

    if delta_denominator == 0 {
        let price = oracle_price
            .safe_add(auction_end_price_offset)?
            .max(tick_size.cast()?)
            .cast::<u64>()?;

        return standardize_price(price, tick_size, order.direction);
    }

    let price_offset_delta = match order.direction {
        PositionDirection::Long => auction_end_price_offset
            .safe_sub(auction_start_price_offset)?
            .safe_mul(delta_numerator.cast()?)?
            .safe_div(delta_denominator.cast()?)?,
        PositionDirection::Short => auction_start_price_offset
            .safe_sub(auction_end_price_offset)?
            .safe_mul(delta_numerator.cast()?)?
            .safe_div(delta_denominator.cast()?)?,
    };

    let price_offset = match order.direction {
        PositionDirection::Long => auction_start_price_offset.safe_add(price_offset_delta)?,
        PositionDirection::Short => auction_start_price_offset.safe_sub(price_offset_delta)?,
    };

    let price = oracle_price
        .safe_add(price_offset)?
        .max(tick_size.cast()?)
        .cast::<u64>()?;

    standardize_price(price, tick_size, order.direction)
}

pub fn is_auction_complete(
    order_slot: u64,
    auction_duration: u8,
    slot: u64,
    slot_clock: SlotClock,
) -> VelocityResult<bool> {
    if auction_duration == 0 {
        return Ok(true);
    }

    // Wall-clock elapsed time, integrated per slot-duration regime, against the
    // auction's wall-clock length in 400ms units. At the 400ms baseline this is the
    // same comparison the earlier per-slot code made.
    let elapsed = slot_clock.elapsed(order_slot, slot);

    Ok(elapsed > Millis::from_stored_units(auction_duration as u64))
}

pub fn calculate_auction_params_for_trigger_order(
    order: &Order,
    oracle_price_data: &OraclePriceData,
    min_auction_duration: u8,
    perp_market: Option<&PerpMarket>,
) -> VelocityResult<(u8, i64, i64)> {
    let auction_duration = min_auction_duration;

    if let Some(perp_market) = perp_market {
        // negative buffer is crossing
        let auction_start_buffer = if perp_market
            .contract_tier
            .is_as_safe_as_contract(&ContractTier::B)
        {
            -500
        } else {
            -3_500
        };

        let (auction_start_price, auction_end_price, derived_auction_duration) =
            if matches!(order.order_type, OrderType::TriggerMarket) {
                OrderParams::derive_oracle_order_auction_params(
                    perp_market,
                    order.direction,
                    oracle_price_data.price,
                    None,
                    auction_start_buffer,
                )?
            } else {
                OrderParams::derive_market_order_auction_params(
                    perp_market,
                    order.direction,
                    oracle_price_data.price,
                    order.price,
                    auction_start_buffer,
                )?
            };

        let auction_duration = auction_duration.max(derived_auction_duration);

        Ok((auction_duration, auction_start_price, auction_end_price))
    } else {
        let (auction_start_price, auction_end_price) =
            calculate_auction_prices(oracle_price_data, order.direction, order.price)?;

        Ok((auction_duration, auction_start_price, auction_end_price))
    }
}
