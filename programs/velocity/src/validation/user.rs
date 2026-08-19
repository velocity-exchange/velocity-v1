use crate::{
    error::{ErrorCode, VelocityResult},
    math::{constants::THIRTEEN_DAY, slots::base_units_from_slots},
    msg,
    state::{
        spot_market::SpotBalanceType,
        user::{User, UserStats},
    },
    validate, State,
};

pub fn validate_user_deletion(
    user: &User,
    user_stats: &UserStats,
    state: &State,
    now: i64,
) -> VelocityResult {
    validate!(
        !user_stats.is_referrer() || user.sub_account_id != 0,
        ErrorCode::UserCantBeDeleted,
        "user id 0 cant be deleted if user is a referrer"
    )?;

    validate!(
        !user.is_bankrupt(),
        ErrorCode::UserCantBeDeleted,
        "user bankrupt"
    )?;

    validate!(
        !user.is_being_liquidated(),
        ErrorCode::UserCantBeDeleted,
        "user being liquidated"
    )?;

    for perp_position in &user.perp_positions {
        validate!(
            perp_position.is_available(),
            ErrorCode::UserCantBeDeleted,
            "user has perp position for market {}",
            perp_position.market_index
        )?;
    }

    for spot_position in &user.spot_positions {
        validate!(
            spot_position.is_available(),
            ErrorCode::UserCantBeDeleted,
            "user has spot position for market {}",
            spot_position.market_index
        )?;
    }

    for order in &user.orders {
        validate!(
            order.is_available(),
            ErrorCode::UserCantBeDeleted,
            "user has an open order"
        )?;
    }

    if state.max_initialize_user_fee > 0 {
        let estimated_user_stats_age = user_stats.get_age_ts(now);
        if estimated_user_stats_age < THIRTEEN_DAY {
            validate!(
                user.idle,
                ErrorCode::UserCantBeDeleted,
                "user is not idle with fresh user stats account creation ({} < {})",
                estimated_user_stats_age,
                THIRTEEN_DAY
            )?;
        }
    }

    Ok(())
}

pub fn validate_user_is_idle(
    user: &User,
    slot: u64,
    accelerated: bool,
    slot_duration_ms: u64,
) -> VelocityResult {
    // thresholds are in 400ms baseline units; deflate the measured slot delta
    // to the same units so the wall-clock windows hold at any slot duration
    let slots_since_last_active =
        base_units_from_slots(slot.saturating_sub(user.last_active_slot), slot_duration_ms);

    let slots_before_idle = if accelerated {
        9000_u64 // ~1 hour
    } else {
        1512000_u64 // ~1 week
    };

    validate!(
        slots_since_last_active >= slots_before_idle,
        ErrorCode::UserNotInactive,
        "user only been idle for {} slot",
        slots_since_last_active
    )?;

    validate!(
        !user.is_bankrupt(),
        ErrorCode::UserNotInactive,
        "user bankrupt"
    )?;

    validate!(
        !user.is_being_liquidated(),
        ErrorCode::UserNotInactive,
        "user being liquidated"
    )?;

    for perp_position in &user.perp_positions {
        validate!(
            perp_position.is_available(),
            ErrorCode::UserNotInactive,
            "user has perp position for market {}",
            perp_position.market_index
        )?;
    }

    for spot_position in &user.spot_positions {
        validate!(
            spot_position.balance_type != SpotBalanceType::Borrow
                || spot_position.scaled_balance == 0,
            ErrorCode::UserNotInactive,
            "user has borrow for market {}",
            spot_position.market_index
        )?;

        validate!(
            spot_position.open_orders == 0,
            ErrorCode::UserNotInactive,
            "user has open order for market {}",
            spot_position.market_index
        )?;
    }

    for order in &user.orders {
        validate!(
            order.is_available(),
            ErrorCode::UserNotInactive,
            "user has an open order"
        )?;
    }

    Ok(())
}
