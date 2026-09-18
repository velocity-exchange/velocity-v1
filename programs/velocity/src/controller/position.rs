use {
    crate::{
        error::{ErrorCode, VelocityResult},
        math::{
            casting::Cast,
            constants::PERP_DECIMALS,
            orders::{calculate_quote_asset_amount_for_maker_order, is_multiple_of_step_size},
            position::{get_new_position_amounts, get_position_update_type, PositionUpdateType},
            safe_math::SafeMath,
        },
        math_error, msg, safe_increment,
        state::{
            perp_market::PerpMarket,
            user::{PerpPosition, PerpPositions, User},
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Debug, Eq, Default)]
pub enum PositionDirection {
    #[default]
    Long,
    Short,
}

impl PositionDirection {
    pub fn opposite(&self) -> Self {
        match self {
            PositionDirection::Long => PositionDirection::Short,
            PositionDirection::Short => PositionDirection::Long,
        }
    }
}

pub fn add_new_position(
    user_positions: &mut PerpPositions,
    market_index: u16,
) -> VelocityResult<usize> {
    let new_position_index = user_positions
        .iter()
        .position(|market_position| market_position.is_available())
        .ok_or(ErrorCode::MaxNumberOfPositions)?;

    let max_margin_ratio = {
        let old_position = &user_positions[new_position_index];

        if old_position.market_index == market_index {
            old_position.max_margin_ratio
        } else {
            0_u16
        }
    };

    let new_market_position = PerpPosition {
        market_index,
        max_margin_ratio,
        ..PerpPosition::default()
    };

    user_positions[new_position_index] = new_market_position;

    Ok(new_position_index)
}

pub fn get_position_index(
    user_positions: &PerpPositions,
    market_index: u16,
) -> VelocityResult<usize> {
    let position_index = user_positions
        .iter()
        .position(|market_position| market_position.is_for(market_index));

    match position_index {
        Some(position_index) => Ok(position_index),
        None => Err(ErrorCode::UserHasNoPositionInMarket),
    }
}

#[derive(Default, PartialEq, Debug)]
pub struct PositionDelta {
    pub quote_asset_amount: i64,
    pub base_asset_amount: i64,
}

impl PositionDelta {
    pub fn get_delta_base_abs(&self) -> VelocityResult<i128> {
        self.base_asset_amount.abs().cast::<i128>()
    }
}

pub fn update_position_and_market(
    position: &mut PerpPosition,
    market: &mut PerpMarket,
    delta: &PositionDelta,
) -> VelocityResult<i64> {
    if delta.base_asset_amount == 0 {
        update_quote_asset_amount(position, market, delta.quote_asset_amount)?;
        return Ok(delta.quote_asset_amount);
    }

    let update_type = get_position_update_type(position, delta)?;

    // Update User
    let (new_base_asset_amount, new_quote_asset_amount) =
        get_new_position_amounts(position, delta)?;

    let (new_quote_entry_amount, new_quote_break_even_amount, pnl) = match update_type {
        PositionUpdateType::Open | PositionUpdateType::Increase => {
            let new_quote_entry_amount = position
                .quote_entry_amount
                .safe_add(delta.quote_asset_amount)?;

            let new_quote_break_even_amount = position
                .quote_break_even_amount
                .safe_add(delta.quote_asset_amount)?;

            (new_quote_entry_amount, new_quote_break_even_amount, 0_i64)
        }
        PositionUpdateType::Reduce | PositionUpdateType::Close => {
            let current_base_i128 = position.get_base_asset_amount_abs()?;
            let delta_base_i128 = delta.get_delta_base_abs()?;

            let new_quote_entry_amount = position.quote_entry_amount.safe_sub(
                position
                    .quote_entry_amount
                    .cast::<i128>()?
                    .safe_mul(delta_base_i128)?
                    .safe_div(current_base_i128)?
                    .cast()?,
            )?;

            let new_quote_break_even_amount = position.quote_break_even_amount.safe_sub(
                position
                    .quote_break_even_amount
                    .cast::<i128>()?
                    .safe_mul(delta_base_i128)?
                    .safe_div(current_base_i128)?
                    .cast()?,
            )?;

            let pnl = position
                .quote_entry_amount
                .safe_sub(new_quote_entry_amount)?
                .safe_add(delta.quote_asset_amount)?;

            (new_quote_entry_amount, new_quote_break_even_amount, pnl)
        }
        PositionUpdateType::Flip => {
            let current_base_i128 = position.get_base_asset_amount_abs()?;
            let delta_base_i128 = delta.get_delta_base_abs()?;

            // same calculation for new_quote_entry_amount
            let new_quote_break_even_amount = delta.quote_asset_amount.safe_sub(
                delta
                    .quote_asset_amount
                    .cast::<i128>()?
                    .safe_mul(current_base_i128)?
                    .safe_div(delta_base_i128)?
                    .cast()?,
            )?;

            let pnl = position.quote_entry_amount.safe_add(
                delta
                    .quote_asset_amount
                    .safe_sub(new_quote_break_even_amount)?,
            )?;

            (
                new_quote_break_even_amount,
                new_quote_break_even_amount,
                pnl,
            )
        }
    };

    // Update Market open interest
    if let PositionUpdateType::Open = update_type {
        if position.quote_asset_amount == 0 && position.base_asset_amount == 0 {
            market.number_of_users = market.number_of_users.safe_add(1)?;
        }

        market.number_of_users_with_base = market.number_of_users_with_base.safe_add(1)?;
    } else if let PositionUpdateType::Close = update_type {
        if new_base_asset_amount == 0 && new_quote_asset_amount == 0 {
            market.number_of_users = market.number_of_users.saturating_sub(1);
        }

        market.number_of_users_with_base = market.number_of_users_with_base.saturating_sub(1);
    }

    market.quote_asset_amount = market
        .quote_asset_amount
        .safe_add(delta.quote_asset_amount.cast()?)?;

    match update_type {
        PositionUpdateType::Open | PositionUpdateType::Increase => {
            if new_base_asset_amount > 0 {
                market.base_asset_amount_long = market
                    .base_asset_amount_long
                    .safe_add(delta.base_asset_amount.cast()?)?;
                market.quote_entry_amount_long = market
                    .quote_entry_amount_long
                    .safe_add(delta.quote_asset_amount.cast()?)?;
                market.quote_break_even_amount_long = market
                    .quote_break_even_amount_long
                    .safe_add(delta.quote_asset_amount.cast()?)?;
            } else {
                market.base_asset_amount_short = market
                    .base_asset_amount_short
                    .safe_add(delta.base_asset_amount.cast()?)?;
                market.quote_entry_amount_short = market
                    .quote_entry_amount_short
                    .safe_add(delta.quote_asset_amount.cast()?)?;
                market.quote_break_even_amount_short = market
                    .quote_break_even_amount_short
                    .safe_add(delta.quote_asset_amount.cast()?)?;
            }
        }
        PositionUpdateType::Reduce | PositionUpdateType::Close => {
            if position.base_asset_amount > 0 {
                market.base_asset_amount_long = market
                    .base_asset_amount_long
                    .safe_add(delta.base_asset_amount.cast()?)?;
                market.quote_entry_amount_long = market.quote_entry_amount_long.safe_sub(
                    position
                        .quote_entry_amount
                        .safe_sub(new_quote_entry_amount)?
                        .cast()?,
                )?;
                market.quote_break_even_amount_long =
                    market.quote_break_even_amount_long.safe_sub(
                        position
                            .quote_break_even_amount
                            .safe_sub(new_quote_break_even_amount)?
                            .cast()?,
                    )?;
            } else {
                market.base_asset_amount_short = market
                    .base_asset_amount_short
                    .safe_add(delta.base_asset_amount.cast()?)?;
                market.quote_entry_amount_short = market.quote_entry_amount_short.safe_sub(
                    position
                        .quote_entry_amount
                        .safe_sub(new_quote_entry_amount)?
                        .cast()?,
                )?;
                market.quote_break_even_amount_short =
                    market.quote_break_even_amount_short.safe_sub(
                        position
                            .quote_break_even_amount
                            .safe_sub(new_quote_break_even_amount)?
                            .cast()?,
                    )?;
            }
        }
        PositionUpdateType::Flip => {
            if new_base_asset_amount > 0 {
                market.base_asset_amount_short = market
                    .base_asset_amount_short
                    .safe_sub(position.base_asset_amount.cast()?)?;
                market.base_asset_amount_long = market
                    .base_asset_amount_long
                    .safe_add(new_base_asset_amount.cast()?)?;

                market.quote_entry_amount_short = market
                    .quote_entry_amount_short
                    .safe_sub(position.quote_entry_amount.cast()?)?;
                market.quote_entry_amount_long = market
                    .quote_entry_amount_long
                    .safe_add(new_quote_entry_amount.cast()?)?;

                market.quote_break_even_amount_short = market
                    .quote_break_even_amount_short
                    .safe_sub(position.quote_break_even_amount.cast()?)?;
                market.quote_break_even_amount_long = market
                    .quote_break_even_amount_long
                    .safe_add(new_quote_break_even_amount.cast()?)?;
            } else {
                market.base_asset_amount_long = market
                    .base_asset_amount_long
                    .safe_sub(position.base_asset_amount.cast()?)?;
                market.base_asset_amount_short = market
                    .base_asset_amount_short
                    .safe_add(new_base_asset_amount.cast()?)?;

                market.quote_entry_amount_long = market
                    .quote_entry_amount_long
                    .safe_sub(position.quote_entry_amount.cast()?)?;
                market.quote_entry_amount_short = market
                    .quote_entry_amount_short
                    .safe_add(new_quote_entry_amount.cast()?)?;

                market.quote_break_even_amount_long = market
                    .quote_break_even_amount_long
                    .safe_sub(position.quote_break_even_amount.cast()?)?;
                market.quote_break_even_amount_short = market
                    .quote_break_even_amount_short
                    .safe_add(new_quote_break_even_amount.cast()?)?;
            }
        }
    }

    // Validate that user funding rate is up to date before modifying
    match position.get_direction() {
        PositionDirection::Long if position.base_asset_amount != 0 => {
            validate!(
                position.last_cumulative_funding_rate.cast::<i128>()?
                    == market.cumulative_funding_rate_long,
                ErrorCode::InvalidPositionLastFundingRate,
                "position.last_cumulative_funding_rate {} market.cumulative_funding_rate_long {}",
                position.last_cumulative_funding_rate.cast::<i128>()?,
                market.cumulative_funding_rate_long,
            )?;
        }
        PositionDirection::Short => {
            validate!(
                position.last_cumulative_funding_rate
                    == market.cumulative_funding_rate_short.cast::<i64>()?,
                ErrorCode::InvalidPositionLastFundingRate,
                "position.last_cumulative_funding_rate {} market.cumulative_funding_rate_short {}",
                position.last_cumulative_funding_rate,
                market.cumulative_funding_rate_short,
            )?;
        }
        _ => {}
    }

    // Update user position
    if let PositionUpdateType::Close = update_type {
        position.last_cumulative_funding_rate = 0;
    } else if matches!(
        update_type,
        PositionUpdateType::Open | PositionUpdateType::Increase | PositionUpdateType::Flip
    ) {
        if new_base_asset_amount > 0 {
            position.last_cumulative_funding_rate = market.cumulative_funding_rate_long.cast()?;
        } else {
            position.last_cumulative_funding_rate = market.cumulative_funding_rate_short.cast()?;
        }
    }

    validate!(
        is_multiple_of_step_size(
            position.base_asset_amount.unsigned_abs(),
            market.order_step_size
        )?,
        ErrorCode::InvalidPerpPositionDetected,
        "update_position_and_market left invalid position before {} after {}",
        position.base_asset_amount,
        new_base_asset_amount
    )?;

    position.base_asset_amount = new_base_asset_amount;

    position.quote_asset_amount = new_quote_asset_amount;
    position.quote_entry_amount = new_quote_entry_amount;
    position.quote_break_even_amount = new_quote_break_even_amount;

    // This path writes the quote directly, so it releases a booked claim too.
    // A booked position can reach it. The stale-latch path clears the latch on
    // an estate that still owes the market, and the account can trade again.
    release_bankruptcy_claim_if_settled(position, market);

    Ok(pnl)
}

pub fn calculate_quote_asset_amount_surplus(
    position_direction: PositionDirection,
    quote_asset_swapped: u64,
    base_asset_amount: u64,
    fill_price: u64,
) -> VelocityResult<(u64, i64)> {
    let quote_asset_amount = calculate_quote_asset_amount_for_maker_order(
        base_asset_amount,
        fill_price,
        PERP_DECIMALS,
        position_direction,
    )?;

    let quote_asset_amount_surplus = match position_direction {
        PositionDirection::Long => quote_asset_amount
            .cast::<i64>()?
            .safe_sub(quote_asset_swapped.cast()?)?,
        PositionDirection::Short => quote_asset_swapped
            .cast::<i64>()?
            .safe_sub(quote_asset_amount.cast()?)?,
    };

    Ok((quote_asset_amount, quote_asset_amount_surplus))
}

pub fn update_quote_asset_and_break_even_amount(
    position: &mut PerpPosition,
    market: &mut PerpMarket,
    delta: i64,
) -> VelocityResult {
    update_quote_asset_amount(position, market, delta)?;
    update_quote_break_even_amount(position, market, delta)
}

pub fn update_quote_asset_amount(
    position: &mut PerpPosition,
    market: &mut PerpMarket,
    delta: i64,
) -> VelocityResult<()> {
    if delta == 0 {
        return Ok(());
    }

    if position.quote_asset_amount == 0 && position.base_asset_amount == 0 {
        market.number_of_users = market.number_of_users.safe_add(1)?;
    }

    position.quote_asset_amount = position.quote_asset_amount.safe_add(delta)?;

    market.quote_asset_amount = market.quote_asset_amount.safe_add(delta.cast()?)?;

    if position.quote_asset_amount == 0 && position.base_asset_amount == 0 {
        market.number_of_users = market.number_of_users.saturating_sub(1);
    }

    release_bankruptcy_claim_if_settled(position, market);

    Ok(())
}

/// Releases a booked bankruptcy claim once the position's quote debt is gone.
///
/// allow-verbose: this is the one release point for a market-wide freeze, and
/// each invariant below is a distinct way that freeze can go stuck for good.
///
/// A latched bankrupt debt is booked against the market in
/// `pending_bankruptcy_claims`, which freezes the fee sweep's IF drain. The
/// booking is released by whoever clears the debt: the bankruptcy resolver, a
/// quote-deposit setoff, or a settle or fill after the latch lifts. The
/// position flag makes the release happen once.
///
/// Every writer of `PerpPosition::quote_asset_amount` must call this. Both
/// `update_quote_asset_amount` and `update_position_and_market` do. A missed
/// call strands the counter. `add_new_position` recycles a slot reporting
/// `is_available()` by overwriting the whole position, `position_flag`
/// included, so a claim left on a zeroed position is destroyed without
/// decrementing the market, freezing its IF sweep for good.
///
/// A non-negative quote means no bankrupt debt remains for the tranche to
/// absorb. It does not prove solvency, since a position holding base can
/// carry either sign; `resolve_perp_bankruptcy` takes only a negative quote.
/// `flag_perp_bankruptcy_claim` books only a settled claim with zero base, so
/// a later loss on a re-traded account is a new admission and a new booking.
fn release_bankruptcy_claim_if_settled(position: &mut PerpPosition, market: &mut PerpMarket) {
    if position.has_bankruptcy_claim() && position.quote_asset_amount >= 0 {
        position.clear_bankruptcy_claim();
        market.decrement_pending_bankruptcy_claims();
    }
}

pub fn update_quote_break_even_amount(
    position: &mut PerpPosition,
    market: &mut PerpMarket,
    delta: i64,
) -> VelocityResult<()> {
    if delta == 0 || position.base_asset_amount == 0 {
        return Ok(());
    }

    position.quote_break_even_amount = position.quote_break_even_amount.safe_add(delta)?;
    match position.get_direction() {
        PositionDirection::Long => {
            market.quote_break_even_amount_long = market
                .quote_break_even_amount_long
                .safe_add(delta.cast()?)?
        }
        PositionDirection::Short => {
            market.quote_break_even_amount_short = market
                .quote_break_even_amount_short
                .safe_add(delta.cast()?)?
        }
    }

    Ok(())
}

pub fn update_settled_pnl(
    user: &mut User,
    position_index: usize,
    delta: i64,
) -> VelocityResult<()> {
    update_user_settled_pnl(user, delta)?;
    update_position_settled_pnl(&mut user.perp_positions[position_index], delta)?;
    Ok(())
}

pub fn update_position_settled_pnl(position: &mut PerpPosition, delta: i64) -> VelocityResult<()> {
    position.settled_pnl = position.settled_pnl.safe_add(delta)?;

    Ok(())
}

pub fn update_user_settled_pnl(user: &mut User, delta: i64) -> VelocityResult<()> {
    safe_increment!(user.settled_perp_pnl, delta);
    Ok(())
}

pub fn increase_open_bids_and_asks(
    position: &mut PerpPosition,
    direction: &PositionDirection,
    base_asset_amount_unfilled: u64,
    update: bool,
) -> VelocityResult {
    if !update {
        return Ok(());
    }

    match direction {
        PositionDirection::Long => {
            position.open_bids = position
                .open_bids
                .safe_add(base_asset_amount_unfilled.cast()?)?;
        }
        PositionDirection::Short => {
            position.open_asks = position
                .open_asks
                .safe_sub(base_asset_amount_unfilled.cast()?)?;
        }
    }

    Ok(())
}

/// Releases exactly `base_asset_amount` of the reservation this position holds
/// on `direction`, or fails.
///
/// [`decrease_open_bids_and_asks`] clamps at zero, which is right when
/// velocity authored the number itself, since the reservation and the order
/// it backs move together and the clamp only absorbs rounding. It is wrong
/// for an external quoter's report, since a report above the reservation
/// would silently collapse the whole side and free margin behind resting orders. Use this function for every quoter-reported number instead.
pub fn release_reserved_open_base(
    position: &mut PerpPosition,
    direction: &PositionDirection,
    base_asset_amount: u64,
) -> VelocityResult {
    let reserved = position.reserved_open_base(*direction);
    validate!(
        base_asset_amount <= reserved,
        ErrorCode::QuoterReportExceedsReservation,
        "quoter reported {} base on the {:?} side of market {}, above the {} reserved",
        base_asset_amount,
        direction,
        position.market_index,
        reserved
    )?;

    decrease_open_bids_and_asks(position, direction, base_asset_amount, true)
}

/// Releases what the report names, or the whole reservation when the report
/// names more.
///
/// This is the lenient form of [`release_reserved_open_base`]. Use it only on
/// the paths an owner signs to remove their own orders from a book. Those
/// paths run against a book that may be dead or de-listed, so a failure would
/// trap a maker's orders on the book they need to leave. The log line keeps
/// the clamp from being silent.
pub fn release_reserved_open_base_for_exit(
    position: &mut PerpPosition,
    direction: &PositionDirection,
    base_asset_amount: u64,
) -> VelocityResult {
    let reserved = position.reserved_open_base(*direction);
    if base_asset_amount > reserved {
        msg!(
            "clob reported {} base on the {:?} side of market {}, above the {} reserved; \
             releasing the reservation and letting the exit through",
            base_asset_amount,
            direction,
            position.market_index,
            reserved
        );
    }

    decrease_open_bids_and_asks(position, direction, base_asset_amount, true)
}

/// Removes `count` open-order slots from this position, or fails.
///
/// The counterpart to [`release_reserved_open_base`] for the order count a
/// quoter reports it retired. A saturating subtraction would let one report
/// collapse the count that backs orders which still rest.
pub fn release_reserved_open_orders(position: &mut PerpPosition, count: u8) -> VelocityResult {
    validate!(
        count <= position.open_orders,
        ErrorCode::QuoterReportExceedsReservation,
        "quoter reported {} retired orders on market {}, above the {} open",
        count,
        position.market_index,
        position.open_orders
    )?;

    position.open_orders -= count;
    Ok(())
}

pub fn decrease_open_bids_and_asks(
    position: &mut PerpPosition,
    direction: &PositionDirection,
    base_asset_amount_unfilled: u64,
    update: bool,
) -> VelocityResult {
    if !update {
        return Ok(());
    }

    match direction {
        PositionDirection::Long => {
            position.open_bids = position
                .open_bids
                .safe_sub(base_asset_amount_unfilled.cast()?)?
                .max(0);
        }
        PositionDirection::Short => {
            position.open_asks = position
                .open_asks
                .safe_add(base_asset_amount_unfilled.cast()?)?
                .min(0);
        }
    }

    Ok(())
}
