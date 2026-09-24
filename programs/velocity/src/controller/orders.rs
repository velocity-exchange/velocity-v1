//! The life of an order, from placement to the fill that retires it.
//!
//! Each step is its own module, and this root holds only what several of them
//! speak:
//!
//! * [`placement`] builds a perp order and either writes it into a slot of
//!   `user.orders` or hands it back detached.
//! * [`amend`] cancels and modifies an order that already exists.
//! * [`trigger`] fires a dormant trigger order into a live market order.
//! * [`perp_fill`] fills one perp order in three layers: the order, the
//!   taker's risk limits, and liquidity.
//! * [`settle`] settles one filled allocation against the vAMM, a maker,
//!   or an external quoter.
//! * [`cross`] prices a crank that crosses two resting sources.
//! * [`keeper`] holds the keeper's work on other users' orders, and the flat
//!   rewards it earns.
//!
//! Margin math is in `crate::math::margin`. Liquidation fills are in
//! `crate::controller::liquidation`.

use {
    crate::{
        controller::{
            self,
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
            casting::Cast,
            constants::{BASE_PRECISION_U64, DEFAULT_MARKET_ORDER_LIFETIME_SECONDS},
            fees::{self, FillFees},
            liquidation::validate_user_not_being_liquidated,
            margin::*,
            matching::calculate_filler_multiplier_for_matched_orders,
            oracle::{
                self, is_oracle_valid_for_action, oracle_validity, OracleValidity, VelocityAction,
            },
            orders::*,
            safe_math::SafeMath,
            safe_unwrap::SafeUnwrap,
            time::{Millis, SlotClock},
            worst_price::derive_worst_price,
        },
        print_error,
        state::{
            events::{
                emit_stack, get_order_action_record, OrderAction, OrderActionExplanation,
                OrderActionRecord, OrderRecord,
            },
            margin_calculation::{MarginCalculation, MarginContext},
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
                MarketType, Order, OrderBitFlag, OrderReservation, OrderStatus,
                OrderTriggerCondition, OrderType, ReleaseCheck, User, UserStats,
            },
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

mod amend;
mod cross;
mod keeper;
mod perp_fill;
mod placement;
mod settle;
mod trigger;

#[cfg(test)]
pub(crate) use perp_fill::{
    fill_perp_order_without_external_books, FillConditions, OfferedLiquidity,
};
pub use {
    amend::*,
    cross::*,
    keeper::*,
    perp_fill::{fill_perp_order, FillParties, FillRequest, PerpFillAccounts},
    placement::*,
    trigger::*,
};
pub(crate) use {
    perp_fill::{fill_within_taker_risk_limits, FillAmounts, TakerRefs, TakerRiskLimits},
    settle::*,
};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod router_pass_tests;

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
    // A fresh detached taker has no position yet. Opening one is not
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

        // A placed trigger's slot is a shadow; `cancel_order` refuses it because
        // cancelling would strand the live CLOB order and unwind its accounting
        // twice. The slot also holds an `open_bids` or `open_asks` reservation,
        // but the flat position this sweep runs under already excludes it, so skip it here too.
        if user.orders[order_index].is_placed_on_clob() {
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
/// Every perp fill path reads the oracle this way. It takes the MM price the
/// market derives from the raw feed, then the validity of the safe,
/// confidence-bounded form of that price. The caller passes the raw price data
/// it already holds, so this never repeats the map lookup and never reorders
/// it.
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
