//! Bounds on a market's configuration.
//!
//! `initialize_market_v0` and `update_market_v0` write the header first and
//! then run [`validate_market_config`] over the result. One check therefore
//! covers every field, including a rule that relates two fields that one
//! update changes separately. The book reads the tick, the step and the
//! minimum without a zero guard, so a zero must never reach the header.
//!
//! `blocking_min_size` must exceed `min_order_size`, so an order of the minimum
//! size never ends a walk. Otherwise 49 such orders on fresh sub-accounts put
//! the depth behind them out of reach of every caller. It has no ceiling. A
//! high floor only lets a caller step over more of the makers it did not load,
//! and no bound on it is right for every asset's price.

use {
    crate::{
        book::validate_evict_threshold,
        error::ClobError,
        state::{
            ClobMarketV0, EXECUTE_FILLS_CEILING, EXECUTE_USERS_CEILING, QUOTE_LEVELS_CEILING,
            RESERVATION_GRACE_SLOTS_CEILING,
        },
    },
    anchor_lang::prelude::*,
};

/// Ceiling on `unknown_user_grace_slots`. A transaction is invalid 150 slots
/// after its blockhash, so a caller's account set cannot lag the book longer.
pub const UNKNOWN_USER_GRACE_SLOTS_CEILING: u32 = 150;

/// Ceiling on `max_activation_delay_slots`, about ten minutes of slots. A
/// longer delay holds its owner's margin against depth that nobody can match.
pub const MAX_ACTIVATION_DELAY_SLOTS_CEILING: u32 = 1_500;

/// Ceiling on the orders one side holds. A walk visits every order on its side,
/// at about 250 CU each, so a full side costs about 133k CU to quote and 140k
/// CU to execute. A router fill pays both.
pub const ORDERS_PER_SIDE_CEILING: u32 = 512;

/// The arena holds two sides, so its capacity is at most twice the side ceiling.
/// Only `initialize_market_v0` and `resize_market_v0` check it. An arena cannot
/// shrink, so a check on update would lock the config of an older, larger book.
pub fn validate_capacity(capacity: u32) -> Result<()> {
    require!(
        capacity <= 2 * ORDERS_PER_SIDE_CEILING,
        ClobError::CapacityOverCeiling
    );

    Ok(())
}

pub fn validate_market_config(market: &ClobMarketV0) -> Result<()> {
    require!(
        market.order_tick_size != 0
            && market.order_step_size != 0
            && market.min_order_size != 0
            && market.min_order_size.is_multiple_of(market.order_step_size),
        ClobError::InvalidConfig
    );
    require!(
        market.blocking_min_size > market.min_order_size,
        ClobError::InvalidConfig
    );

    require!(
        market.default_activation_delay_slots <= market.max_activation_delay_slots
            && market.max_activation_delay_slots <= MAX_ACTIVATION_DELAY_SLOTS_CEILING,
        ClobError::InvalidConfig
    );
    require!(
        market.unknown_user_grace_slots <= UNKNOWN_USER_GRACE_SLOTS_CEILING,
        ClobError::InvalidConfig
    );

    validate_evict_threshold(market.evict_threshold_per_side, market.capacity() as u32)?;
    require!(
        (1..=QUOTE_LEVELS_CEILING).contains(&market.max_quote_levels)
            && (1..=EXECUTE_FILLS_CEILING).contains(&market.max_execute_fills)
            && (1..=EXECUTE_USERS_CEILING).contains(&market.max_execute_users)
            && market.reservation_grace_slots <= RESERVATION_GRACE_SLOTS_CEILING,
        ClobError::InvalidConfig
    );

    Ok(())
}
