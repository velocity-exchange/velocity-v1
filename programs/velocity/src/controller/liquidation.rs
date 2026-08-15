//! Liquidation engine: margin checks, position reduction, social loss, insurance fund draws.
//! Entry points are in `crate::instructions::keeper` (liquidate_perp, liquidate_spot, etc.).
//! `liquidate_perp` = perp position reduction with liquidation fee. `liquidate_spot` = spot borrow resolution.
//! `resolve_perp_bankruptcy` / `resolve_spot_bankruptcy` = social loss and insurance draws.

use {
    crate::{
        controller::{
            funding::settle_funding_payment,
            orders::{self, cancel_order, fill_perp_order, place_perp_order},
            position::{
                get_position_index, update_position_and_market, update_quote_asset_amount,
                update_quote_asset_and_break_even_amount, update_settled_pnl, PositionDirection,
            },
            spot_balance::{
                transfer_spot_balances, update_protocol_fee_pool_balances,
                update_revenue_pool_balances, update_spot_balances,
                update_spot_market_and_check_validity, update_spot_market_cumulative_interest,
            },
            spot_position::update_spot_balances_and_cumulative_deposits,
        },
        error::{ErrorCode, VelocityResult},
        get_then_update_id, load_mut,
        math::{
            bankruptcy::{
                has_pending_cross_margin_perp_bankruptcy, has_realizable_spot_assets_for_setoff,
                is_cross_margin_bankrupt, perp_markets_with_forfeitable_claims,
            },
            casting::Cast,
            constants::{
                LIQUIDATION_FEE_PRECISION, LIQUIDATION_FEE_PRECISION_U128,
                LIQUIDATION_PCT_PRECISION, LST_POOL_ID, QUOTE_PRECISION, QUOTE_PRECISION_I128,
                QUOTE_PRECISION_U64, QUOTE_SPOT_MARKET_INDEX, SPOT_WEIGHT_PRECISION,
            },
            liquidation::{
                calculate_asset_transfer_for_liability_transfer,
                calculate_asset_transfer_for_liability_transfer_exact,
                calculate_base_asset_amount_to_cover_margin_shortage,
                calculate_cumulative_deposit_interest_delta_to_resolve_bankruptcy,
                calculate_funding_rate_deltas_to_resolve_bankruptcy,
                calculate_liability_transfer_implied_by_asset_amount,
                calculate_liability_transfer_to_cover_margin_shortage,
                calculate_liquidation_multiplier, calculate_max_pct_to_liquidate,
                calculate_perp_if_fee, calculate_spot_if_fee,
                calculate_user_protective_asset_price, calculate_user_protective_liability_price,
                get_liquidation_fee, get_liquidation_order_params,
                validate_swap_within_liquidation_boundaries,
                validate_transfer_satisfies_limit_price, LiquidationMultiplierType,
            },
            margin::{
                calculate_margin_requirement_and_total_collateral_and_liability_info,
                calculate_net_equity_for_floor, meets_initial_margin_requirement,
                MarginRequirementType,
            },
            oracle::{is_oracle_valid_for_action, oracle_validity, LogMode, VelocityAction},
            orders::{
                calculate_existing_position_fields_for_order_action, get_position_delta_for_fill,
                is_multiple_of_step_size, is_oracle_too_divergent_with_twap_5min,
                standardize_base_asset_amount, standardize_base_asset_amount_ceil,
            },
            position::calculate_base_asset_value_with_oracle_price,
            safe_math::SafeMath,
            spot_balance::{get_token_amount, get_token_value},
        },
        msg,
        state::{
            events::{
                LiquidateBorrowForPerpPnlRecord, LiquidatePerpPnlForDepositRecord,
                LiquidatePerpRecord, LiquidateSpotRecord, LiquidationRecord, LiquidationType,
                OrderAction, OrderActionExplanation, OrderActionRecord, OrderRecord,
                PerpBankruptcyRecord, SpotBankruptcyRecord,
            },
            fill_mode::FillMode,
            liquidation_mode::{get_perp_liquidation_mode, LiquidatePerpMode},
            margin_calculation::{MarginCalculation, MarginContext, MarketIdentifier},
            market_status::MarketStatus,
            oracle_map::OracleMap,
            order_params::PlaceOrderOptions,
            paused_operations::{PerpOperation, SpotOperation},
            perp_market_map::PerpMarketMap,
            spot_market::{SpotBalance, SpotBalanceType},
            spot_market_map::SpotMarketMap,
            state::State,
            user::{MarketType, Order, OrderStatus, OrderType, User, UserStats},
            user_map::{UserMap, UserStatsMap},
        },
        validate,
        vlp::amm::{controller::get_fee_pool_tokens, refresh::update_amm_and_check_validity},
    },
    anchor_lang::prelude::*,
    std::ops::{Deref, DerefMut},
};

#[cfg(test)]
mod tests;

pub fn liquidate_perp(
    market_index: u16,
    liquidator_max_base_asset_amount: u64,
    limit_price: Option<u64>,
    user: &mut User,
    user_key: &Pubkey,
    user_stats: &mut UserStats,
    liquidator: &mut User,
    liquidator_key: &Pubkey,
    liquidator_stats: &mut UserStats,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    slot: u64,
    now: i64,
    state: &State,
) -> VelocityResult {
    let liquidation_margin_buffer_ratio = state.liquidation_margin_buffer_ratio;
    let initial_pct_to_liquidate = state.initial_pct_to_liquidate as u128;
    let liquidation_duration = state.liquidation_duration as u128;

    let liquidation_mode = get_perp_liquidation_mode(user, market_index)?;

    validate!(
        !liquidation_mode.is_user_bankrupt(user)?,
        ErrorCode::UserBankrupt,
        "user bankrupt",
    )?;

    validate!(
        !liquidator.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "liquidator bankrupt",
    )?;

    validate!(
        liquidator.pool_id == 0,
        ErrorCode::InvalidPoolId,
        "liquidator pool id ({}) != 0",
        liquidator.pool_id
    )?;

    let market = perp_market_map.get_ref(&market_index)?;

    validate!(
        !market.is_operation_paused(PerpOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        market_index
    )?;

    // OtterSec #149: once `expiry_ts` passes, an expired perp position must only be
    // closed out at the market's committed `expiry_price`, never at the live oracle.
    //
    // Every ordinary user path already refuses past expiry via the same
    // `is_in_settlement(now)` predicate — placing, filling, triggering, transferring and
    // settling all gate on it — but direct permissionless liquidation did not, so it kept
    // valuing and transferring the position at the live oracle for the whole window
    // between `expiry_ts` and a warm admin flipping the status to `Settlement`. A
    // liquidator could take the position at a live price that the fixed settlement price
    // then supersedes, while the owner had no way to act.
    //
    // Scoped precisely to that window. Deliberately NOT `is_in_settlement(now)`, which is
    // also true once the status *is* `Settlement`/`Delisted` — by then `expiry_price` is
    // committed and liquidation during the wind-down is a legitimate way to resolve bad
    // debt (the delisting tests exercise exactly that). What must be refused is only the
    // gap where the market has expired but no settlement price exists yet.
    let expired_awaiting_settlement = market.expiry_ts != 0
        && now >= market.expiry_ts
        && !matches!(
            market.status,
            MarketStatus::Settlement | MarketStatus::Delisted
        );

    validate!(
        !expired_awaiting_settlement,
        ErrorCode::InvalidLiquidation,
        "market {} expired at {} but has no committed expiry price yet; \
         settle_expired_market must run first",
        market_index,
        market.expiry_ts
    )?;

    drop(market);

    settle_funding_payment(
        user,
        user_key,
        perp_market_map.get_ref_mut(&market_index)?.deref_mut(),
        now,
    )?;

    settle_funding_payment(
        liquidator,
        liquidator_key,
        perp_market_map.get_ref_mut(&market_index)?.deref_mut(),
        now,
    )?;

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        MarginContext::liquidation(liquidation_margin_buffer_ratio)
            .track_market_margin_requirement(MarketIdentifier::perp(market_index))?,
    )?;

    let user_is_being_liquidated = liquidation_mode.user_is_being_liquidated(user)?;
    if !user_is_being_liquidated
        && liquidation_mode.meets_margin_requirements(&margin_calculation)?
    {
        msg!("margin calculation: {:?}", margin_calculation);
        return Err(ErrorCode::SufficientCollateral);
    } else if user_is_being_liquidated
        && liquidation_mode.can_exit_liquidation(&margin_calculation)?
    {
        liquidation_mode.exit_liquidation(user)?;
        return Ok(());
    }

    user.get_perp_position(market_index).inspect_err(|_e| {
        msg!(
            "User does not have a position for perp market {}",
            market_index
        );
    })?;

    liquidator
        .force_get_perp_position_mut(market_index)
        .inspect_err(|_e| {
            msg!(
                "Liquidator has no available positions to take on perp position in market {}",
                market_index
            );
        })?;

    let liquidation_id = liquidation_mode.enter_liquidation(user, slot)?;
    let mut margin_freed = 0_u64;

    let position_index = get_position_index(&user.perp_positions, market_index)?;
    validate!(
        user.perp_positions[position_index].is_open_position()
            || user.perp_positions[position_index].has_open_order(),
        ErrorCode::PositionDoesntHaveOpenPositionOrOrders
    )?;

    let (cancel_order_market_type, cancel_order_market_index) =
        liquidation_mode.get_cancel_orders_params();
    let canceled_order_ids = orders::cancel_orders(
        user,
        user_key,
        Some(liquidator_key),
        perp_market_map,
        spot_market_map,
        oracle_map,
        now,
        slot,
        OrderActionExplanation::Liquidation,
        cancel_order_market_type,
        cancel_order_market_index,
        None,
        true,
    )?;

    let mut market = perp_market_map.get_ref_mut(&market_index)?;
    let oracle_price_data = oracle_map.get_price_data(&market.oracle_id())?;
    let mm_oracle_price_data = market.get_mm_oracle_price_data(
        *oracle_price_data,
        slot,
        &state.oracle_guard_rails.validity,
    )?;

    update_amm_and_check_validity(
        &mut market,
        &mm_oracle_price_data,
        state,
        now,
        slot,
        Some(VelocityAction::Liquidate),
    )?;

    let oracle_price = if market.status == MarketStatus::Settlement {
        market.expiry_price
    } else {
        oracle_price_data.price
    };

    drop(market);

    // check if user exited liquidation territory
    let intermediate_margin_calculation = if !canceled_order_ids.is_empty() {
        let intermediate_margin_calculation =
            calculate_margin_requirement_and_total_collateral_and_liability_info(
                user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                MarginContext::liquidation(liquidation_margin_buffer_ratio)
                    .track_market_margin_requirement(MarketIdentifier::perp(market_index))?,
            )?;

        let initial_margin_shortage = liquidation_mode.margin_shortage(&margin_calculation)?;
        let new_margin_shortage =
            liquidation_mode.margin_shortage(&intermediate_margin_calculation)?;

        margin_freed = initial_margin_shortage
            .saturating_sub(new_margin_shortage)
            .cast::<u64>()?;
        liquidation_mode.increment_free_margin(user, margin_freed)?;

        if liquidation_mode.can_exit_liquidation(&intermediate_margin_calculation)? {
            let (margin_requirement, total_collateral, bit_flags) =
                liquidation_mode.get_event_fields(&margin_calculation)?;
            emit!(LiquidationRecord {
                ts: now,
                liquidation_id,
                liquidation_type: LiquidationType::LiquidatePerp,
                user: *user_key,
                liquidator: *liquidator_key,
                margin_requirement,
                total_collateral,
                bankrupt: liquidation_mode.is_user_bankrupt(user)?,
                canceled_order_ids,
                margin_freed,
                liquidate_perp: LiquidatePerpRecord {
                    market_index,
                    oracle_price,
                    ..LiquidatePerpRecord::default()
                },
                bit_flags,
                ..LiquidationRecord::default()
            });

            liquidation_mode.exit_liquidation(user)?;
            return Ok(());
        }

        intermediate_margin_calculation
    } else {
        margin_calculation.clone()
    };

    if user.perp_positions[position_index].base_asset_amount == 0 {
        msg!("User has no base asset amount");
        return Ok(());
    }

    let liquidator_max_base_asset_amount = standardize_base_asset_amount(
        liquidator_max_base_asset_amount,
        perp_market_map.get_ref(&market_index)?.order_step_size,
    )?;

    validate!(
        liquidator_max_base_asset_amount != 0,
        ErrorCode::InvalidBaseAssetAmountForLiquidatePerp,
        "liquidator_max_base_asset_amount must be greater or equal to the step size",
    )?;

    {
        let perp_market = perp_market_map.get_ref(&market_index)?;

        if perp_market.status != MarketStatus::Settlement {
            let oracle_price_too_divergent = is_oracle_too_divergent_with_twap_5min(
                oracle_price,
                perp_market
                    .market_stats
                    .historical_oracle_data
                    .last_oracle_price_twap_5min,
                state
                    .oracle_guard_rails
                    .max_oracle_twap_5min_percent_divergence()
                    .cast()?,
            )?;

            validate!(!oracle_price_too_divergent, ErrorCode::PriceBandsBreached)?;
        }
    }

    let user_base_asset_amount = user.perp_positions[position_index]
        .base_asset_amount
        .unsigned_abs();

    let margin_ratio = perp_market_map.get_ref(&market_index)?.get_margin_ratio(
        user_base_asset_amount.cast()?,
        MarginRequirementType::Maintenance,
    )?;

    let margin_ratio_with_buffer = margin_ratio.safe_add(liquidation_margin_buffer_ratio)?;

    let margin_shortage = liquidation_mode.margin_shortage(&intermediate_margin_calculation)?;

    let market = perp_market_map.get_ref(&market_index)?;
    let quote_spot_market = spot_market_map.get_ref(&market.quote_spot_market_index)?;
    let quote_oracle_price = oracle_map
        .get_price_data(&quote_spot_market.oracle_id())?
        .price;

    let liquidator_fee = get_liquidation_fee(
        market.get_base_liquidator_fee(),
        market.get_max_liquidation_fee()?,
        user.last_active_slot,
        slot,
    )?;

    // Compute the total insurance-side budget (margin-shortage aware) with the
    // cap raised to `if_liquidation_fee + protocol_liquidation_fee`, then split
    // IF-first: the IF receives exactly what it would have without the protocol
    // fee; the protocol only captures margin headroom beyond that, up to its
    // flat rate. This keeps the combined fee inside the margin budget so the
    // protocol fee can never push a liquidation into spurious bankruptcy.
    let total_if_side_fee = calculate_perp_if_fee(
        intermediate_margin_calculation.tracked_market_margin_shortage(margin_shortage)?,
        user_base_asset_amount,
        margin_ratio_with_buffer,
        liquidator_fee,
        oracle_price,
        quote_oracle_price,
        market
            .if_liquidation_fee
            .safe_add(market.protocol_liquidation_fee)?,
    )?;
    let if_liquidation_fee = total_if_side_fee.min(market.if_liquidation_fee);
    let protocol_liquidation_fee = total_if_side_fee.safe_sub(if_liquidation_fee)?;

    let mut base_asset_amount_to_cover_margin_shortage =
        calculate_base_asset_amount_to_cover_margin_shortage(
            margin_shortage,
            margin_ratio_with_buffer,
            liquidator_fee,
            total_if_side_fee,
            oracle_price,
            quote_oracle_price,
        )?;

    if base_asset_amount_to_cover_margin_shortage != u64::MAX {
        base_asset_amount_to_cover_margin_shortage = standardize_base_asset_amount_ceil(
            base_asset_amount_to_cover_margin_shortage,
            market.order_step_size,
        )?;
    }

    drop(market);
    drop(quote_spot_market);

    let max_pct_allowed = liquidation_mode.calculate_max_pct_to_liquidate(
        user,
        margin_shortage,
        slot,
        initial_pct_to_liquidate,
        liquidation_duration,
    )?;
    let max_base_asset_amount_allowed_to_be_transferred =
        base_asset_amount_to_cover_margin_shortage
            .cast::<u128>()?
            .saturating_mul(max_pct_allowed)
            .safe_div(LIQUIDATION_PCT_PRECISION)?
            .cast::<u64>()?;

    if max_base_asset_amount_allowed_to_be_transferred == 0 {
        msg!("max_base_asset_amount_allowed_to_be_transferred == 0");
        return Ok(());
    }

    let base_asset_value =
        calculate_base_asset_value_with_oracle_price(user_base_asset_amount.cast()?, oracle_price)?
            .cast::<u64>()?;

    // if position is less than $50, liquidator can liq all of it
    let min_base_asset_amount = if base_asset_value > 50 * QUOTE_PRECISION_U64 {
        0_u64
    } else {
        user_base_asset_amount
    };

    let base_asset_amount = user_base_asset_amount
        .min(liquidator_max_base_asset_amount)
        .min(max_base_asset_amount_allowed_to_be_transferred.max(min_base_asset_amount));
    let base_asset_amount = standardize_base_asset_amount_ceil(
        base_asset_amount,
        perp_market_map.get_ref(&market_index)?.order_step_size,
    )?;

    // Make sure liquidator enters at better than limit price
    if let Some(limit_price) = limit_price {
        // calculate fee in price terms
        let oracle_price_u128 = oracle_price.cast::<u128>()?;
        let fee = oracle_price_u128
            .safe_mul(liquidator_fee.cast()?)?
            .safe_div(LIQUIDATION_FEE_PRECISION_U128)?;
        match user.perp_positions[position_index].get_direction() {
            PositionDirection::Long => {
                let transfer_price = oracle_price_u128.safe_sub(fee)?;
                validate!(
                    transfer_price <= limit_price.cast()?,
                    ErrorCode::LiquidationDoesntSatisfyLimitPrice,
                    "limit price ({}) > transfer price ({})",
                    limit_price,
                    transfer_price
                )?
            }
            PositionDirection::Short => {
                let transfer_price = oracle_price_u128.safe_add(fee)?;
                validate!(
                    transfer_price >= limit_price.cast()?,
                    ErrorCode::LiquidationDoesntSatisfyLimitPrice,
                    "limit price ({}) < transfer price ({})",
                    limit_price,
                    transfer_price
                )?
            }
        }
    }

    let base_asset_value =
        calculate_base_asset_value_with_oracle_price(base_asset_amount.cast()?, oracle_price)?
            .cast::<u64>()?;

    let liquidator_fee = -base_asset_value
        .cast::<u128>()?
        .safe_mul(liquidator_fee.cast()?)?
        .safe_div(LIQUIDATION_FEE_PRECISION_U128)?
        .cast::<i64>()?;

    let if_fee = -base_asset_value
        .cast::<u128>()?
        .safe_mul(if_liquidation_fee.cast()?)?
        .safe_div(LIQUIDATION_FEE_PRECISION_U128)?
        .cast::<i64>()?;

    let protocol_fee = -base_asset_value
        .cast::<u128>()?
        .safe_mul(protocol_liquidation_fee.cast()?)?
        .safe_div(LIQUIDATION_FEE_PRECISION_U128)?
        .cast::<i64>()?;

    user_stats.update_taker_volume_30d(base_asset_value, now)?;
    liquidator_stats.update_maker_volume_30d(base_asset_value, now)?;

    let user_position_delta = get_position_delta_for_fill(
        base_asset_amount,
        base_asset_value,
        user.perp_positions[position_index].get_direction_to_close(),
    )?;

    let liquidator_position_delta = get_position_delta_for_fill(
        base_asset_amount,
        base_asset_value,
        user.perp_positions[position_index].get_direction(),
    )?;

    let (
        user_existing_position_direction,
        user_position_direction_to_close,
        user_existing_position_params_for_order_action,
        liquidator_existing_position_direction,
        liquidator_existing_position_params_for_order_action,
    ) = {
        let mut market = perp_market_map.get_ref_mut(&market_index)?;

        let user_position = user.get_perp_position_mut(market_index)?;
        let user_existing_position_direction = user_position.get_direction();
        let user_position_direction_to_close = user_position.get_direction_to_close();
        let user_existing_position_params = user_position
            .get_existing_position_params_for_order_action(user_position_direction_to_close);
        update_position_and_market(user_position, &mut market, &user_position_delta)?;
        update_quote_asset_and_break_even_amount(user_position, &mut market, liquidator_fee)?;
        update_quote_asset_and_break_even_amount(user_position, &mut market, if_fee)?;
        update_quote_asset_and_break_even_amount(user_position, &mut market, protocol_fee)?;

        validate!(
            is_multiple_of_step_size(
                user_position.base_asset_amount.unsigned_abs(),
                market.order_step_size
            )?,
            ErrorCode::InvalidPerpPosition,
            "base asset amount {} step size {}",
            user_position.base_asset_amount,
            market.order_step_size
        )?;

        let liquidator_position = liquidator.force_get_perp_position_mut(market_index)?;
        let liquidator_existing_position_direction = liquidator_position.get_direction();
        let liquidator_existing_position_params = liquidator_position
            .get_existing_position_params_for_order_action(user_existing_position_direction);
        update_position_and_market(liquidator_position, &mut market, &liquidator_position_delta)?;
        update_quote_asset_and_break_even_amount(
            liquidator_position,
            &mut market,
            -liquidator_fee,
        )?;

        validate!(
            is_multiple_of_step_size(
                liquidator_position.base_asset_amount.unsigned_abs(),
                market.order_step_size
            )?,
            ErrorCode::InvalidPerpPosition,
            "base asset amount {} step size {}",
            liquidator_position.base_asset_amount,
            market.order_step_size
        )?;

        // both cuts accrue to pending counters, materialized into
        // revenue_pool / protocol_fee_pool by `sweep_market_fees`
        // (total_liquidation_fee remains a lifetime analytics counter)
        market.fee_ledger.accrue_liquidation_fees(
            if_fee.unsigned_abs().cast()?,
            protocol_fee.unsigned_abs().cast()?,
        )?;

        (
            user_existing_position_direction,
            user_position_direction_to_close,
            user_existing_position_params,
            liquidator_existing_position_direction,
            liquidator_existing_position_params,
        )
    };

    let (margin_freed_for_perp_position, _) = calculate_margin_freed(
        user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        liquidation_margin_buffer_ratio,
        margin_shortage,
        Some(liquidation_mode.as_ref()),
    )?;
    margin_freed = margin_freed.safe_add(margin_freed_for_perp_position)?;
    liquidation_mode.increment_free_margin(user, margin_freed_for_perp_position)?;

    if base_asset_amount >= base_asset_amount_to_cover_margin_shortage {
        liquidation_mode.exit_liquidation(user)?;
    } else if liquidation_mode.should_user_enter_bankruptcy(
        user,
        spot_market_map,
        perp_market_map,
    )? {
        liquidation_mode.enter_bankruptcy(user)?;
    }

    let liquidator_meets_initial_margin_requirement =
        meets_initial_margin_requirement(liquidator, perp_market_map, spot_market_map, oracle_map)?;

    validate!(
        liquidator_meets_initial_margin_requirement,
        ErrorCode::InsufficientCollateral,
        "Liquidator doesnt have enough collateral to take over perp position"
    )?;

    // The liquidation adds exposure to the liquidator like a risk-increasing
    // fill; the liquidator subaccount must clear its own buffered equity floor
    // to take it on.
    if let Some(liquidator_net_equity) =
        calculate_net_equity_for_floor(liquidator, perp_market_map, spot_market_map, oracle_map)?
    {
        liquidator_net_equity.validate_clears_buffered_floor(liquidator)?;
    }

    // get ids for order fills
    let user_order_id = get_then_update_id!(user, next_order_id);
    let liquidator_order_id = get_then_update_id!(liquidator, next_order_id);
    let fill_record_id = {
        let mut market = perp_market_map.get_ref_mut(&market_index)?;
        get_then_update_id!(market, next_fill_record_id)
    };

    let user_order = Order {
        slot,
        base_asset_amount,
        order_id: user_order_id,
        market_index,
        status: OrderStatus::Open,
        order_type: OrderType::Market,
        market_type: MarketType::Perp,
        direction: user_position_direction_to_close,
        existing_position_direction: user_existing_position_direction,
        ..Order::default()
    };

    emit!(OrderRecord {
        ts: now,
        user: *user_key,
        order: user_order
    });

    let liquidator_order = Order {
        slot,
        price: limit_price.unwrap_or_default(),
        base_asset_amount,
        order_id: liquidator_order_id,
        market_index,
        status: OrderStatus::Open,
        order_type: if limit_price.is_some() {
            OrderType::Limit
        } else {
            OrderType::Market
        },
        market_type: MarketType::Perp,
        direction: user_existing_position_direction,
        existing_position_direction: liquidator_existing_position_direction,
        ..Order::default()
    };

    emit!(OrderRecord {
        ts: now,
        user: *liquidator_key,
        order: liquidator_order
    });

    let (taker_existing_quote_entry_amount, taker_existing_base_asset_amount) =
        calculate_existing_position_fields_for_order_action(
            base_asset_amount,
            user_existing_position_params_for_order_action,
        )?;

    let (maker_existing_quote_entry_amount, maker_existing_base_asset_amount) =
        calculate_existing_position_fields_for_order_action(
            base_asset_amount,
            liquidator_existing_position_params_for_order_action,
        )?;

    let fill_record = OrderActionRecord {
        ts: now,
        action: OrderAction::Fill,
        action_explanation: OrderActionExplanation::Liquidation,
        market_index,
        market_type: MarketType::Perp,
        filler: None,
        filler_reward: None,
        fill_record_id: Some(fill_record_id),
        base_asset_amount_filled: Some(base_asset_amount),
        quote_asset_amount_filled: Some(base_asset_value),
        taker_fee: Some(
            liquidator_fee
                .unsigned_abs()
                .safe_add(if_fee.unsigned_abs())?
                .safe_add(protocol_fee.unsigned_abs())?,
        ),
        maker_fee: Some(liquidator_fee),
        referrer_reward: None,
        quote_asset_amount_surplus: None,
        spot_fulfillment_method_fee: None,
        taker: Some(*user_key),
        taker_order_id: Some(user_order_id),
        taker_order_direction: Some(user_position_direction_to_close),
        taker_order_base_asset_amount: Some(base_asset_amount),
        taker_order_cumulative_base_asset_amount_filled: Some(base_asset_amount),
        taker_order_cumulative_quote_asset_amount_filled: Some(base_asset_value),
        maker: Some(*liquidator_key),
        maker_order_id: Some(liquidator_order_id),
        maker_order_direction: Some(user_existing_position_direction),
        maker_order_base_asset_amount: Some(base_asset_amount),
        maker_order_cumulative_base_asset_amount_filled: Some(base_asset_amount),
        maker_order_cumulative_quote_asset_amount_filled: Some(base_asset_value),
        oracle_price,
        bit_flags: 0,
        taker_existing_quote_entry_amount,
        taker_existing_base_asset_amount,
        maker_existing_quote_entry_amount,
        maker_existing_base_asset_amount,
        trigger_price: None,
        builder_idx: None,
        builder_fee: None,
    };
    emit!(fill_record);

    let (margin_requirement, total_collateral, bit_flags) =
        liquidation_mode.get_event_fields(&margin_calculation)?;
    emit!(LiquidationRecord {
        ts: now,
        liquidation_id,
        liquidation_type: LiquidationType::LiquidatePerp,
        user: *user_key,
        liquidator: *liquidator_key,
        margin_requirement,
        total_collateral,
        bankrupt: liquidation_mode.is_user_bankrupt(user)?,
        canceled_order_ids,
        margin_freed,
        liquidate_perp: LiquidatePerpRecord {
            market_index,
            oracle_price,
            base_asset_amount: user_position_delta.base_asset_amount,
            quote_asset_amount: user_position_delta.quote_asset_amount,
            user_order_id,
            liquidator_order_id,
            fill_record_id,
            liquidator_fee: liquidator_fee.abs().cast()?,
            if_fee: if_fee.abs().cast()?,
            protocol_fee: protocol_fee.abs().cast()?,
        },
        bit_flags,
        ..LiquidationRecord::default()
    });

    Ok(())
}

pub fn liquidate_perp_with_fill(
    market_index: u16,
    user_loader: &AccountLoader<User>,
    user_key: &Pubkey,
    user_stats_loader: &AccountLoader<UserStats>,
    liquidator_loader: &AccountLoader<User>,
    liquidator_key: &Pubkey,
    liquidator_stats_loader: &AccountLoader<UserStats>,
    makers_and_referrer: &UserMap,
    makers_and_referrer_stats: &UserStatsMap,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    clock: &Clock,
    state: &State,
) -> VelocityResult {
    let now = clock.unix_timestamp;
    let slot = clock.slot;

    let mut user = load_mut!(user_loader)?;
    let mut liquidator = load_mut!(liquidator_loader)?;

    let liquidation_margin_buffer_ratio = state.liquidation_margin_buffer_ratio;
    let initial_pct_to_liquidate = state.initial_pct_to_liquidate as u128;
    let liquidation_duration = state.liquidation_duration as u128;

    let liquidation_mode = get_perp_liquidation_mode(&user, market_index)?;

    validate!(
        !liquidation_mode.is_user_bankrupt(&user)?,
        ErrorCode::UserBankrupt,
        "user bankrupt",
    )?;

    validate!(
        liquidator.pool_id == 0,
        ErrorCode::InvalidPoolId,
        "liquidator pool id ({}) != 0",
        liquidator.pool_id
    )?;

    validate!(
        !liquidator.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "liquidator bankrupt",
    )?;

    let market = perp_market_map.get_ref(&market_index)?;

    validate!(
        !market.is_operation_paused(PerpOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        market_index
    )?;

    // OtterSec #149: once `expiry_ts` passes, an expired perp position must only be
    // closed out at the market's committed `expiry_price`, never at the live oracle.
    //
    // Every ordinary user path already refuses past expiry via the same
    // `is_in_settlement(now)` predicate — placing, filling, triggering, transferring and
    // settling all gate on it — but direct permissionless liquidation did not, so it kept
    // valuing and transferring the position at the live oracle for the whole window
    // between `expiry_ts` and a warm admin flipping the status to `Settlement`. A
    // liquidator could take the position at a live price that the fixed settlement price
    // then supersedes, while the owner had no way to act.
    //
    // Scoped precisely to that window. Deliberately NOT `is_in_settlement(now)`, which is
    // also true once the status *is* `Settlement`/`Delisted` — by then `expiry_price` is
    // committed and liquidation during the wind-down is a legitimate way to resolve bad
    // debt (the delisting tests exercise exactly that). What must be refused is only the
    // gap where the market has expired but no settlement price exists yet.
    let expired_awaiting_settlement = market.expiry_ts != 0
        && now >= market.expiry_ts
        && !matches!(
            market.status,
            MarketStatus::Settlement | MarketStatus::Delisted
        );

    validate!(
        !expired_awaiting_settlement,
        ErrorCode::InvalidLiquidation,
        "market {} expired at {} but has no committed expiry price yet; \
         settle_expired_market must run first",
        market_index,
        market.expiry_ts
    )?;

    drop(market);

    settle_funding_payment(
        &mut user,
        user_key,
        perp_market_map.get_ref_mut(&market_index)?.deref_mut(),
        now,
    )?;

    settle_funding_payment(
        &mut liquidator,
        liquidator_key,
        perp_market_map.get_ref_mut(&market_index)?.deref_mut(),
        now,
    )?;

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        &user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        MarginContext::liquidation(liquidation_margin_buffer_ratio)
            .track_market_margin_requirement(MarketIdentifier::perp(market_index))?,
    )?;

    let user_is_being_liquidated = liquidation_mode.user_is_being_liquidated(&user)?;
    if !user_is_being_liquidated
        && liquidation_mode.meets_margin_requirements(&margin_calculation)?
    {
        msg!("margin calculation: {:?}", margin_calculation);
        return Err(ErrorCode::SufficientCollateral);
    } else if user_is_being_liquidated
        && liquidation_mode.can_exit_liquidation(&margin_calculation)?
    {
        liquidation_mode.exit_liquidation(&mut user)?;
        return Ok(());
    }

    user.get_perp_position(market_index).inspect_err(|_e| {
        msg!(
            "User does not have a position for perp market {}",
            market_index
        );
    })?;

    let liquidation_id = liquidation_mode.enter_liquidation(&mut user, slot)?;
    let mut margin_freed = 0_u64;

    let position_index = get_position_index(&user.perp_positions, market_index)?;
    validate!(
        user.perp_positions[position_index].is_open_position()
            || user.perp_positions[position_index].has_open_order(),
        ErrorCode::PositionDoesntHaveOpenPositionOrOrders
    )?;

    let (cancel_orders_market_type, cancel_orders_market_index) =
        liquidation_mode.get_cancel_orders_params();
    let canceled_order_ids = orders::cancel_orders(
        &mut user,
        user_key,
        Some(liquidator_key),
        perp_market_map,
        spot_market_map,
        oracle_map,
        now,
        slot,
        OrderActionExplanation::Liquidation,
        cancel_orders_market_type,
        cancel_orders_market_index,
        None,
        true,
    )?;

    let mut market = perp_market_map.get_ref_mut(&market_index)?;
    let oracle_price_data = oracle_map.get_price_data(&market.oracle_id())?;
    let mm_oracle_price_data = market.get_mm_oracle_price_data(
        *oracle_price_data,
        slot,
        &state.oracle_guard_rails.validity,
    )?;

    update_amm_and_check_validity(
        &mut market,
        &mm_oracle_price_data,
        state,
        now,
        slot,
        Some(VelocityAction::Liquidate),
    )?;

    let oracle_price = if market.status == MarketStatus::Settlement {
        market.expiry_price
    } else {
        oracle_price_data.price
    };

    drop(market);

    // check if user exited liquidation territory
    let intermediate_margin_calculation = if !canceled_order_ids.is_empty() {
        let intermediate_margin_calculation =
            calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                MarginContext::liquidation(liquidation_margin_buffer_ratio)
                    .track_market_margin_requirement(MarketIdentifier::perp(market_index))?,
            )?;

        let initial_margin_shortage = liquidation_mode.margin_shortage(&margin_calculation)?;
        let new_margin_shortage =
            liquidation_mode.margin_shortage(&intermediate_margin_calculation)?;

        margin_freed = initial_margin_shortage
            .saturating_sub(new_margin_shortage)
            .cast::<u64>()?;
        liquidation_mode.increment_free_margin(&mut user, margin_freed)?;

        if liquidation_mode.can_exit_liquidation(&intermediate_margin_calculation)? {
            let (margin_requirement, total_collateral, bit_flags) =
                liquidation_mode.get_event_fields(&margin_calculation)?;
            emit!(LiquidationRecord {
                ts: now,
                liquidation_id,
                liquidation_type: LiquidationType::LiquidatePerp,
                user: *user_key,
                liquidator: *liquidator_key,
                margin_requirement,
                total_collateral,
                bankrupt: liquidation_mode.is_user_bankrupt(&user)?,
                canceled_order_ids,
                margin_freed,
                liquidate_perp: LiquidatePerpRecord {
                    market_index,
                    oracle_price,
                    ..LiquidatePerpRecord::default()
                },
                bit_flags,
                ..LiquidationRecord::default()
            });

            liquidation_mode.exit_liquidation(&mut user)?;
            return Ok(());
        }

        intermediate_margin_calculation
    } else {
        margin_calculation.clone()
    };

    if user.perp_positions[position_index].base_asset_amount == 0 {
        msg!("User has no base asset amount");
        return Ok(());
    }

    let oracle_price_too_divergent = is_oracle_too_divergent_with_twap_5min(
        oracle_price,
        perp_market_map
            .get_ref(&market_index)?
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min,
        state
            .oracle_guard_rails
            .max_oracle_twap_5min_percent_divergence()
            .cast()?,
    )?;

    validate!(!oracle_price_too_divergent, ErrorCode::PriceBandsBreached)?;

    let user_base_asset_amount = user.perp_positions[position_index]
        .base_asset_amount
        .unsigned_abs();

    let margin_ratio = perp_market_map.get_ref(&market_index)?.get_margin_ratio(
        user_base_asset_amount.cast()?,
        MarginRequirementType::Maintenance,
    )?;

    let margin_ratio_with_buffer = margin_ratio.safe_add(liquidation_margin_buffer_ratio)?;

    let margin_shortage = liquidation_mode.margin_shortage(&intermediate_margin_calculation)?;

    let market = perp_market_map.get_ref(&market_index)?;
    let quote_spot_market = spot_market_map.get_ref(&market.quote_spot_market_index)?;
    let quote_oracle_price = oracle_map
        .get_price_data(&quote_spot_market.oracle_id())?
        .price;
    // Use the time-adjusted liquidator fee (grace-period ramp) as the basis for
    // both the IF/protocol fee budget and the margin-shortage base sizing, so it
    // matches the fee the forced liquidation order is actually priced with
    // (see `liquidator_fee` below). Sizing against the un-aged `market.liquidator_fee`
    // would under-budget the insurance/protocol fees relative to the larger
    // execution discount the victim pays post-grace-period (matches liquidate_perp).
    let liquidator_fee = get_liquidation_fee(
        market.get_base_liquidator_fee(),
        market.get_max_liquidation_fee()?,
        user.last_active_slot,
        slot,
    )?;
    // total insurance-side budget with the cap raised to if + protocol rates,
    // split IF-first (see liquidate_perp for rationale)
    let total_if_side_fee = calculate_perp_if_fee(
        intermediate_margin_calculation.tracked_market_margin_shortage(margin_shortage)?,
        user_base_asset_amount,
        margin_ratio_with_buffer,
        liquidator_fee,
        oracle_price,
        quote_oracle_price,
        market
            .if_liquidation_fee
            .safe_add(market.protocol_liquidation_fee)?,
    )?;
    let if_liquidation_fee = total_if_side_fee.min(market.if_liquidation_fee);
    let protocol_liquidation_fee = total_if_side_fee.safe_sub(if_liquidation_fee)?;
    let base_asset_amount_to_cover_margin_shortage = standardize_base_asset_amount_ceil(
        calculate_base_asset_amount_to_cover_margin_shortage(
            margin_shortage,
            margin_ratio_with_buffer,
            liquidator_fee,
            total_if_side_fee,
            oracle_price,
            quote_oracle_price,
        )?,
        market.order_step_size,
    )?;
    drop(market);
    drop(quote_spot_market);

    let max_pct_allowed = liquidation_mode.calculate_max_pct_to_liquidate(
        &user,
        margin_shortage,
        slot,
        initial_pct_to_liquidate,
        liquidation_duration,
    )?;
    let max_base_asset_amount_allowed_to_be_transferred =
        base_asset_amount_to_cover_margin_shortage
            .cast::<u128>()?
            .saturating_mul(max_pct_allowed)
            .safe_div(LIQUIDATION_PCT_PRECISION)?
            .cast::<u64>()?;

    if max_base_asset_amount_allowed_to_be_transferred == 0 {
        msg!("max_base_asset_amount_allowed_to_be_transferred == 0");
        return Ok(());
    }

    let base_asset_value =
        calculate_base_asset_value_with_oracle_price(user_base_asset_amount.cast()?, oracle_price)?
            .cast::<u64>()?;

    // if position is less than $50, liquidator can liq all of it
    let min_base_asset_amount = if base_asset_value > 50 * QUOTE_PRECISION_U64 {
        0_u64
    } else {
        user_base_asset_amount
    };

    let base_asset_amount = user_base_asset_amount
        .min(max_base_asset_amount_allowed_to_be_transferred.max(min_base_asset_amount));
    let base_asset_amount = standardize_base_asset_amount_ceil(
        base_asset_amount,
        perp_market_map.get_ref(&market_index)?.order_step_size,
    )?;

    let existing_direction = user.perp_positions[position_index].get_direction();

    let order_params = get_liquidation_order_params(
        market_index,
        existing_direction,
        base_asset_amount,
        oracle_price,
        liquidator_fee,
    )?;

    let order_id = user.next_order_id;
    let fill_record_id = perp_market_map.get_ref(&market_index)?.next_fill_record_id;
    place_perp_order(
        state,
        &mut user,
        *user_key,
        perp_market_map,
        spot_market_map,
        oracle_map,
        clock,
        order_params,
        PlaceOrderOptions::default().explanation(OrderActionExplanation::Liquidation),
        &mut None,
    )?;

    drop(user);
    drop(liquidator);

    let (fill_base_asset_amount, fill_quote_asset_amount) = fill_perp_order(
        order_id,
        state,
        user_loader,
        user_stats_loader,
        spot_market_map,
        perp_market_map,
        oracle_map,
        liquidator_loader,
        liquidator_stats_loader,
        makers_and_referrer,
        makers_and_referrer_stats,
        None,
        clock,
        FillMode::Liquidation,
        &mut None,
    )?;

    let mut user = load_mut!(user_loader)?;

    if let Ok(order_index) = user.get_order_index(order_id) {
        cancel_order(
            order_index,
            &mut user,
            user_key,
            perp_market_map,
            spot_market_map,
            oracle_map,
            clock.unix_timestamp,
            clock.slot,
            OrderActionExplanation::None,
            Some(liquidator_key),
            0,
            false,
        )?;
    }

    // no fill
    if fill_base_asset_amount == 0 {
        return Err(ErrorCode::LiquidationOrderFailedToFill);
    }

    let if_fee = -fill_quote_asset_amount
        .cast::<u128>()?
        .safe_mul(if_liquidation_fee.cast()?)?
        .safe_div(LIQUIDATION_FEE_PRECISION_U128)?
        .cast::<i64>()?;

    let protocol_fee = -fill_quote_asset_amount
        .cast::<u128>()?
        .safe_mul(protocol_liquidation_fee.cast()?)?
        .safe_div(LIQUIDATION_FEE_PRECISION_U128)?
        .cast::<i64>()?;

    {
        let mut market = perp_market_map.get_ref_mut(&market_index)?;

        let user_position = user.get_perp_position_mut(market_index)?;
        update_quote_asset_and_break_even_amount(user_position, &mut market, if_fee)?;
        update_quote_asset_and_break_even_amount(user_position, &mut market, protocol_fee)?;

        market.fee_ledger.accrue_liquidation_fees(
            if_fee.unsigned_abs().cast()?,
            protocol_fee.unsigned_abs().cast()?,
        )?;
    }

    let (margin_freed_for_perp_position, margin_calculation_after) = calculate_margin_freed(
        &user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        liquidation_margin_buffer_ratio,
        margin_shortage,
        Some(liquidation_mode.as_ref()),
    )?;

    margin_freed = margin_freed.safe_add(margin_freed_for_perp_position)?;
    liquidation_mode.increment_free_margin(&mut user, margin_freed_for_perp_position)?;

    if liquidation_mode.can_exit_liquidation(&margin_calculation_after)? {
        liquidation_mode.exit_liquidation(&mut user)?;
    } else if liquidation_mode.should_user_enter_bankruptcy(
        &user,
        spot_market_map,
        perp_market_map,
    )? {
        liquidation_mode.enter_bankruptcy(&mut user)?;
    }

    let user_position_delta = get_position_delta_for_fill(
        fill_base_asset_amount,
        fill_quote_asset_amount,
        existing_direction,
    )?;

    let (margin_requirement, total_collateral, bit_flags) =
        liquidation_mode.get_event_fields(&margin_calculation)?;
    emit!(LiquidationRecord {
        ts: now,
        liquidation_id,
        liquidation_type: LiquidationType::LiquidatePerp,
        user: *user_key,
        liquidator: *liquidator_key,
        margin_requirement,
        total_collateral,
        bankrupt: liquidation_mode.is_user_bankrupt(&user)?,
        canceled_order_ids,
        margin_freed,
        liquidate_perp: LiquidatePerpRecord {
            market_index,
            oracle_price,
            base_asset_amount: user_position_delta.base_asset_amount,
            quote_asset_amount: user_position_delta.quote_asset_amount,
            user_order_id: order_id,
            liquidator_order_id: 0,
            fill_record_id,
            liquidator_fee: 0,
            if_fee: if_fee.abs().cast()?,
            protocol_fee: protocol_fee.abs().cast()?,
        },
        bit_flags,
        ..LiquidationRecord::default()
    });

    Ok(())
}

pub fn liquidate_spot(
    asset_market_index: u16,
    liability_market_index: u16,
    liquidator_max_liability_transfer: u128,
    limit_price: Option<u64>,
    user: &mut User,
    user_key: &Pubkey,
    liquidator: &mut User,
    liquidator_key: &Pubkey,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    now: i64,
    slot: u64,
    state: &State,
) -> VelocityResult {
    let liquidation_margin_buffer_ratio = state.liquidation_margin_buffer_ratio;
    let initial_pct_to_liquidate = state.initial_pct_to_liquidate as u128;
    let liquidation_duration = state.liquidation_duration as u128;
    let funding_paused = state.funding_paused()?;

    validate!(
        !user.is_cross_margin_bankrupt(),
        ErrorCode::UserBankrupt,
        "user bankrupt",
    )?;

    validate!(
        !liquidator.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "liquidator bankrupt",
    )?;

    let asset_spot_market = spot_market_map.get_ref(&asset_market_index)?;

    validate!(
        !asset_spot_market.is_operation_paused(SpotOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        asset_market_index
    )?;

    validate!(
        liquidator.pool_id == asset_spot_market.pool_id,
        ErrorCode::InvalidPoolId,
        "liquidator pool id ({}) != asset spot market pool id ({})",
        liquidator.pool_id,
        asset_spot_market.pool_id
    )?;

    drop(asset_spot_market);

    let liability_spot_market = spot_market_map.get_ref(&liability_market_index)?;

    validate!(
        !liability_spot_market.is_operation_paused(SpotOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        liability_market_index
    )?;

    validate!(
        liquidator.pool_id == liability_spot_market.pool_id,
        ErrorCode::InvalidPoolId,
        "liquidator pool id ({}) != liablity spot market pool id ({})",
        liquidator.pool_id,
        liability_spot_market.pool_id
    )?;

    drop(liability_spot_market);

    // validate user and liquidator have spot balances
    user.get_spot_position(asset_market_index).map_err(|_| {
        msg!(
            "User does not have a spot balance for asset market {}",
            asset_market_index
        );
        ErrorCode::CouldNotFindSpotPosition
    })?;

    user.get_spot_position(liability_market_index)
        .map_err(|_| {
            msg!(
                "User does not have a spot balance for liability market {}",
                liability_market_index
            );
            ErrorCode::CouldNotFindSpotPosition
        })?;

    liquidator
        .force_get_spot_position_mut(asset_market_index)
        .inspect_err(|_e| {
            msg!("Liquidator has no available spot balances to take on deposit");
        })?;

    liquidator
        .force_get_spot_position_mut(liability_market_index)
        .inspect_err(|_e| {
            msg!("Liquidator has no available spot balances to take on borrow");
        })?;

    let (
        asset_amount,
        asset_oracle_price,
        asset_price,
        asset_decimals,
        asset_weight,
        asset_liquidation_multiplier,
        asset_pool_id,
        asset_oracle_delay,
    ) = {
        let mut asset_market = spot_market_map.get_ref_mut(&asset_market_index)?;
        let (asset_price_data, validity_guard_rails) =
            oracle_map.get_price_data_and_guard_rails(&asset_market.oracle_id())?;

        let asset_oracle_validity = update_spot_market_and_check_validity(
            &mut asset_market,
            asset_price_data,
            validity_guard_rails,
            now,
            Some(VelocityAction::Liquidate),
            funding_paused,
        )?;

        let spot_deposit_position = user.get_spot_position(asset_market_index)?;

        validate!(
            spot_deposit_position.balance_type == SpotBalanceType::Deposit,
            ErrorCode::WrongSpotBalanceType,
            "User did not have a deposit for the asset market index"
        )?;

        let token_amount = spot_deposit_position.get_token_amount(&asset_market)?;

        validate!(
            token_amount != 0,
            ErrorCode::InvalidSpotPosition,
            "asset token amount zero for market index = {}",
            asset_market_index
        )?;

        // a margin-invalid (stale/uncertain) deposit oracle may make the account
        // liquidatable, but must not let its collateral be seized at a depressed
        // price: size the transfer at a user-protective price instead
        let asset_price =
            if is_oracle_valid_for_action(asset_oracle_validity, Some(VelocityAction::MarginCalc))?
            {
                asset_price_data.price
            } else {
                calculate_user_protective_asset_price(
                    asset_price_data,
                    asset_market
                        .historical_oracle_data
                        .last_oracle_price_twap_5min,
                )?
            };

        (
            token_amount,
            asset_price_data.price,
            asset_price,
            asset_market.decimals,
            asset_market.maintenance_asset_weight,
            calculate_liquidation_multiplier(
                asset_market.liquidator_fee,
                LiquidationMultiplierType::Premium,
            )?,
            asset_market.pool_id,
            asset_price_data.delay,
        )
    };

    let (
        liability_amount,
        liability_oracle_price,
        liability_price,
        liability_decimals,
        liability_weight,
        liability_liquidation_multiplier,
        liability_pool_id,
        liability_oracle_delay,
    ) = {
        let mut liability_market = spot_market_map.get_ref_mut(&liability_market_index)?;
        let (liability_price_data, validity_guard_rails) =
            oracle_map.get_price_data_and_guard_rails(&liability_market.oracle_id())?;

        let liability_oracle_validity = update_spot_market_and_check_validity(
            &mut liability_market,
            liability_price_data,
            validity_guard_rails,
            now,
            Some(VelocityAction::Liquidate),
            funding_paused,
        )?;

        let spot_position = user.get_spot_position(liability_market_index)?;

        validate!(
            spot_position.balance_type == SpotBalanceType::Borrow,
            ErrorCode::WrongSpotBalanceType,
            "User did not have a borrow for the liability market index"
        )?;

        let token_amount = spot_position.get_token_amount(&liability_market)?;

        validate!(
            token_amount != 0,
            ErrorCode::InvalidSpotPosition,
            "liability token amount zero for market index = {}",
            liability_market_index
        )?;

        // the liability side of the exchange rate gets the mirrored protection: a
        // margin-invalid (stale/uncertain) borrow oracle must not overvalue the debt
        // being repaid and cheapen the collateral received for it
        let liability_price = if is_oracle_valid_for_action(
            liability_oracle_validity,
            Some(VelocityAction::MarginCalc),
        )? {
            liability_price_data.price
        } else {
            calculate_user_protective_liability_price(
                liability_price_data,
                liability_market
                    .historical_oracle_data
                    .last_oracle_price_twap_5min,
            )?
        };

        (
            token_amount,
            liability_price_data.price,
            liability_price,
            liability_market.decimals,
            liability_market.maintenance_liability_weight,
            calculate_liquidation_multiplier(
                liability_market.liquidator_fee,
                LiquidationMultiplierType::Discount,
            )?,
            liability_market.pool_id,
            liability_price_data.delay,
        )
    };

    if asset_pool_id == LST_POOL_ID && liability_pool_id == LST_POOL_ID {
        validate!(
            asset_oracle_delay == 0 && liability_oracle_delay == 0,
            ErrorCode::InvalidLiquidation,
            "asset oracle delay ({}) != 0 || liability oracle delay ({}) != 0",
            asset_oracle_delay,
            liability_oracle_delay
        )?;
    }

    let margin_context = MarginContext::liquidation(liquidation_margin_buffer_ratio)
        .track_market_margin_requirement(MarketIdentifier::spot(liability_market_index))?;

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        margin_context,
    )?;

    if !user.is_cross_margin_being_liquidated()
        && margin_calculation.meets_cross_margin_requirement()
    {
        msg!("margin calculation: {:?}", margin_calculation);
        return Err(ErrorCode::SufficientCollateral);
    } else if user.is_cross_margin_being_liquidated()
        && margin_calculation.can_exit_cross_margin_liquidation()?
    {
        user.exit_cross_margin_liquidation();
        return Ok(());
    }

    let liquidation_id = user.enter_cross_margin_liquidation(slot)?;
    let mut margin_freed = 0_u64;

    let canceled_order_ids = orders::cancel_orders(
        user,
        user_key,
        Some(liquidator_key),
        perp_market_map,
        spot_market_map,
        oracle_map,
        now,
        slot,
        OrderActionExplanation::Liquidation,
        None,
        None,
        None,
        true,
    )?;

    // check if user exited liquidation territory
    let intermediate_margin_calculation = if !canceled_order_ids.is_empty() {
        let intermediate_margin_calculation =
            calculate_margin_requirement_and_total_collateral_and_liability_info(
                user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                MarginContext::liquidation(liquidation_margin_buffer_ratio)
                    .track_market_margin_requirement(MarketIdentifier::spot(
                        liability_market_index,
                    ))?,
            )?;

        let initial_margin_shortage = margin_calculation.cross_margin_margin_shortage()?;
        let new_margin_shortage = intermediate_margin_calculation.cross_margin_margin_shortage()?;

        margin_freed = initial_margin_shortage
            .saturating_sub(new_margin_shortage)
            .cast::<u64>()?;
        user.increment_margin_freed(margin_freed)?;

        if intermediate_margin_calculation.can_exit_cross_margin_liquidation()? {
            emit!(LiquidationRecord {
                ts: now,
                liquidation_id,
                liquidation_type: LiquidationType::LiquidateSpot,
                user: *user_key,
                liquidator: *liquidator_key,
                margin_requirement: margin_calculation.margin_requirement,
                total_collateral: margin_calculation.total_collateral,
                bankrupt: user.is_cross_margin_bankrupt(),
                canceled_order_ids,
                margin_freed,
                liquidate_spot: LiquidateSpotRecord {
                    asset_market_index,
                    asset_price,
                    asset_transfer: 0,
                    liability_market_index,
                    liability_price,
                    liability_transfer: 0,
                    if_fee: 0,
                    protocol_fee: 0,
                },
                ..LiquidationRecord::default()
            });

            user.exit_cross_margin_liquidation();
            return Ok(());
        }

        intermediate_margin_calculation
    } else {
        margin_calculation.clone()
    };

    let margin_shortage = intermediate_margin_calculation.cross_margin_margin_shortage()?;

    let liability_weight_with_buffer =
        liability_weight.safe_add(liquidation_margin_buffer_ratio)?;

    // total insurance-side budget (margin-shortage aware) with the cap raised to
    // if + protocol rates, split IF-first (see liquidate_perp for rationale)
    let (liability_if_liquidation_fee, liability_protocol_liquidation_fee) = {
        let liability_market = spot_market_map.get_ref(&liability_market_index)?;
        (
            liability_market.if_liquidation_fee,
            liability_market.protocol_liquidation_fee,
        )
    };
    let total_if_side_fee = calculate_spot_if_fee(
        intermediate_margin_calculation.tracked_market_margin_shortage(margin_shortage)?,
        liability_amount,
        asset_weight,
        asset_liquidation_multiplier,
        liability_weight_with_buffer,
        liability_liquidation_multiplier,
        liability_decimals,
        // valuation (shortage -> tokens/fees) stays at the raw oracle price, consistent
        // with the margin calculation; only the exchange rate uses the protective price
        liability_oracle_price,
        liability_if_liquidation_fee.safe_add(liability_protocol_liquidation_fee)?,
    )?;
    let liquidation_if_fee = total_if_side_fee.min(liability_if_liquidation_fee);
    let liquidation_protocol_fee = total_if_side_fee.safe_sub(liquidation_if_fee)?;

    // Determine what amount of borrow to transfer to reduce margin shortage to 0
    let liability_transfer_to_cover_margin_shortage =
        calculate_liability_transfer_to_cover_margin_shortage(
            margin_shortage,
            asset_weight,
            asset_liquidation_multiplier,
            liability_weight_with_buffer,
            liability_liquidation_multiplier,
            liability_decimals,
            liability_oracle_price,
            total_if_side_fee,
        )?;

    let max_pct_allowed = calculate_max_pct_to_liquidate(
        user,
        margin_shortage,
        slot,
        initial_pct_to_liquidate,
        liquidation_duration,
    )?;
    let max_liability_allowed_to_be_transferred = liability_transfer_to_cover_margin_shortage
        .saturating_mul(max_pct_allowed)
        .safe_div(LIQUIDATION_PCT_PRECISION)?;

    if max_liability_allowed_to_be_transferred == 0 {
        msg!("max_liability_allowed_to_be_transferred == 0");
        return Ok(());
    }

    // Given the user's deposit amount, how much borrow can be transferred?
    let liability_transfer_implied_by_asset_amount =
        calculate_liability_transfer_implied_by_asset_amount(
            asset_amount,
            asset_liquidation_multiplier,
            asset_decimals,
            asset_price,
            liability_liquidation_multiplier,
            liability_decimals,
            liability_price,
        )?;

    let liability_value = get_token_value(
        liability_amount.cast()?,
        liability_decimals,
        liability_oracle_price,
    )?;

    let minimum_liability_transfer = if liability_value > 10 * QUOTE_PRECISION_I128 {
        0_u128
    } else {
        liability_amount
    };

    let liability_transfer = liquidator_max_liability_transfer
        .min(liability_amount)
        // want to make sure the liability_transfer_to_cover_margin_shortage doesn't lead to dust positions
        .min(max_liability_allowed_to_be_transferred.max(minimum_liability_transfer))
        .min(liability_transfer_implied_by_asset_amount);

    // Given the borrow amount to transfer, determine how much deposit amount to transfer
    let asset_transfer = calculate_asset_transfer_for_liability_transfer(
        asset_amount,
        asset_liquidation_multiplier,
        asset_decimals,
        asset_price,
        liability_transfer,
        liability_liquidation_multiplier,
        liability_decimals,
        liability_price,
    )?;

    if asset_transfer == 0 || liability_transfer == 0 {
        msg!(
            "asset_market_index {} liability_market_index {}",
            asset_market_index,
            liability_market_index
        );
        msg!("liquidator_max_liability_transfer {} liability_amount {} liability_transfer_to_cover_margin_shortage {}", liquidator_max_liability_transfer, liability_amount, liability_transfer_to_cover_margin_shortage);
        msg!(
            "liability_transfer_implied_by_asset_amount {} liability_transfer {} asset_transfer {}",
            liability_transfer_implied_by_asset_amount,
            liability_transfer,
            asset_transfer
        );
        return Err(ErrorCode::InvalidLiquidation);
    }

    let liability_oracle_too_divergent = is_oracle_too_divergent_with_twap_5min(
        liability_oracle_price.cast()?,
        spot_market_map
            .get_ref(&liability_market_index)?
            .historical_oracle_data
            .last_oracle_price_twap_5min,
        state
            .oracle_guard_rails
            .max_oracle_twap_5min_percent_divergence()
            .cast()?,
    )?;

    validate!(
        !liability_oracle_too_divergent,
        ErrorCode::PriceBandsBreached,
        "liability oracle too divergent"
    )?;

    let asset_oracle_too_divergent = is_oracle_too_divergent_with_twap_5min(
        asset_oracle_price.cast()?,
        spot_market_map
            .get_ref(&asset_market_index)?
            .historical_oracle_data
            .last_oracle_price_twap_5min,
        state
            .oracle_guard_rails
            .max_oracle_twap_5min_percent_divergence()
            .cast()?,
    )?;

    validate!(
        !asset_oracle_too_divergent,
        ErrorCode::PriceBandsBreached,
        "asset oracle too divergent"
    )?;

    validate_transfer_satisfies_limit_price(
        asset_transfer,
        liability_transfer,
        asset_decimals,
        liability_decimals,
        limit_price,
    )?;

    let if_fee = liability_transfer
        .safe_mul(liquidation_if_fee.cast()?)?
        .safe_div(LIQUIDATION_FEE_PRECISION_U128)?;
    let protocol_fee = liability_transfer
        .safe_mul(liquidation_protocol_fee.cast()?)?
        .safe_div(LIQUIDATION_FEE_PRECISION_U128)?;
    {
        let mut liability_market = spot_market_map.get_ref_mut(&liability_market_index)?;

        let user_liability_reduction = liability_transfer
            .safe_sub(if_fee)?
            .safe_sub(protocol_fee)?;
        update_spot_balances_and_cumulative_deposits(
            user_liability_reduction,
            &SpotBalanceType::Deposit,
            &mut liability_market,
            user.get_spot_position_mut(liability_market_index)?,
            false,
            Some(user_liability_reduction),
        )?;

        update_revenue_pool_balances(
            if_fee,
            &SpotBalanceType::Deposit,
            &mut liability_market,
            false,
        )?;
        update_protocol_fee_pool_balances(
            protocol_fee,
            &SpotBalanceType::Deposit,
            &mut liability_market,
            false,
        )?;

        update_spot_balances_and_cumulative_deposits(
            liability_transfer,
            &SpotBalanceType::Borrow,
            &mut liability_market,
            liquidator.get_spot_position_mut(liability_market_index)?,
            false,
            Some(liability_transfer),
        )?;
    }

    {
        let mut asset_market = spot_market_map.get_ref_mut(&asset_market_index)?;

        update_spot_balances_and_cumulative_deposits(
            asset_transfer,
            &SpotBalanceType::Deposit,
            &mut asset_market,
            liquidator.force_get_spot_position_mut(asset_market_index)?,
            false,
            Some(asset_transfer),
        )?;

        update_spot_balances_and_cumulative_deposits(
            asset_transfer,
            &SpotBalanceType::Borrow,
            &mut asset_market,
            user.force_get_spot_position_mut(asset_market_index)?,
            false,
            Some(asset_transfer),
        )?;
    }

    let (margin_freed_from_liability, _) = calculate_margin_freed(
        user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        liquidation_margin_buffer_ratio,
        margin_shortage,
        None,
    )?;
    margin_freed = margin_freed.safe_add(margin_freed_from_liability)?;
    user.increment_margin_freed(margin_freed_from_liability)?;

    if liability_transfer >= liability_transfer_to_cover_margin_shortage {
        user.exit_cross_margin_liquidation();
    } else if is_cross_margin_bankrupt(user, spot_market_map, perp_market_map)? {
        user.enter_cross_margin_bankruptcy();
    }

    let liq_margin_context = MarginContext::standard(MarginRequirementType::Initial);

    let liquidator_meets_initial_margin_requirement =
        calculate_margin_requirement_and_total_collateral_and_liability_info(
            liquidator,
            perp_market_map,
            spot_market_map,
            oracle_map,
            liq_margin_context,
        )
        .map(|calc| calc.meets_margin_requirement())?;

    validate!(
        liquidator_meets_initial_margin_requirement,
        ErrorCode::InsufficientCollateral,
        "Liquidator doesnt have enough collateral to take over borrow"
    )?;

    // The liquidation adds exposure to the liquidator like a risk-increasing
    // fill; the liquidator subaccount must clear its own buffered equity floor
    // to take it on.
    if let Some(liquidator_net_equity) =
        calculate_net_equity_for_floor(liquidator, perp_market_map, spot_market_map, oracle_map)?
    {
        liquidator_net_equity.validate_clears_buffered_floor(liquidator)?;
    }

    emit!(LiquidationRecord {
        ts: now,
        liquidation_id,
        liquidation_type: LiquidationType::LiquidateSpot,
        user: *user_key,
        liquidator: *liquidator_key,
        margin_requirement: margin_calculation.margin_requirement,
        total_collateral: margin_calculation.total_collateral,
        bankrupt: user.is_cross_margin_bankrupt(),
        margin_freed,
        liquidate_spot: LiquidateSpotRecord {
            asset_market_index,
            asset_price,
            asset_transfer,
            liability_market_index,
            liability_price,
            liability_transfer,
            if_fee: if_fee.cast()?,
            protocol_fee: protocol_fee.cast()?,
        },
        ..LiquidationRecord::default()
    });

    Ok(())
}

pub fn liquidate_spot_with_swap_begin(
    asset_market_index: u16,
    liability_market_index: u16,
    swap_amount_in: u64,
    user: &mut User,
    user_key: &Pubkey,
    liquidator: &mut User,
    liquidator_key: &Pubkey,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    now: i64,
    slot: u64,
    state: &State,
) -> VelocityResult {
    let liquidation_margin_buffer_ratio = state.liquidation_margin_buffer_ratio;
    let initial_pct_to_liquidate = state.initial_pct_to_liquidate as u128;
    let liquidation_duration = state.liquidation_duration as u128;
    let funding_paused = state.funding_paused()?;

    validate!(
        !user.is_cross_margin_bankrupt(),
        ErrorCode::UserBankrupt,
        "user bankrupt",
    )?;

    validate!(
        !liquidator.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "liquidator bankrupt",
    )?;

    let asset_spot_market = spot_market_map.get_ref(&asset_market_index)?;

    validate!(
        !asset_spot_market.is_operation_paused(SpotOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        asset_market_index
    )?;

    let liability_spot_market = spot_market_map.get_ref(&liability_market_index)?;

    validate!(
        !liability_spot_market.is_operation_paused(SpotOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        liability_market_index
    )?;

    validate!(
        asset_spot_market.pool_id == liability_spot_market.pool_id,
        ErrorCode::InvalidPoolId,
        "asset_spot_market pool id ({}) != liability_spot_market pool id ({})",
        asset_spot_market.pool_id,
        liability_spot_market.pool_id
    )?;

    drop(asset_spot_market);
    drop(liability_spot_market);

    let (
        asset_amount,
        asset_oracle_price,
        asset_price,
        asset_decimals,
        asset_weight,
        asset_pool_id,
        asset_oracle_delay,
    ) = {
        let mut asset_market = spot_market_map.get_ref_mut(&asset_market_index)?;
        let (asset_price_data, validity_guard_rails) =
            oracle_map.get_price_data_and_guard_rails(&asset_market.oracle_id())?;

        let asset_oracle_validity = update_spot_market_and_check_validity(
            &mut asset_market,
            asset_price_data,
            validity_guard_rails,
            now,
            Some(VelocityAction::Liquidate),
            funding_paused,
        )?;

        let spot_deposit_position = user.get_spot_position(asset_market_index)?;

        validate!(
            spot_deposit_position.balance_type == SpotBalanceType::Deposit,
            ErrorCode::WrongSpotBalanceType,
            "User did not have a deposit for the asset market index"
        )?;

        let token_amount = spot_deposit_position.get_token_amount(&asset_market)?;

        validate!(
            token_amount != 0,
            ErrorCode::InvalidSpotPosition,
            "asset token amount zero for market index = {}",
            asset_market_index
        )?;

        // a margin-invalid (stale/uncertain) deposit oracle may make the account
        // liquidatable, but must not let its collateral be swapped away at a
        // depressed price: cap the swap at a user-protective price instead
        let asset_price =
            if is_oracle_valid_for_action(asset_oracle_validity, Some(VelocityAction::MarginCalc))?
            {
                asset_price_data.price
            } else {
                calculate_user_protective_asset_price(
                    asset_price_data,
                    asset_market
                        .historical_oracle_data
                        .last_oracle_price_twap_5min,
                )?
            };

        (
            token_amount,
            asset_price_data.price,
            asset_price,
            asset_market.decimals,
            asset_market.maintenance_asset_weight,
            asset_market.pool_id,
            asset_price_data.delay,
        )
    };

    let (
        liability_oracle_price,
        liability_price,
        liability_decimals,
        liability_weight,
        liability_if_fee,
        liability_protocol_fee,
        liability_pool_id,
        liability_oracle_delay,
    ) = {
        let mut liability_market = spot_market_map.get_ref_mut(&liability_market_index)?;
        let (liability_price_data, validity_guard_rails) =
            oracle_map.get_price_data_and_guard_rails(&liability_market.oracle_id())?;

        let liability_oracle_validity = update_spot_market_and_check_validity(
            &mut liability_market,
            liability_price_data,
            validity_guard_rails,
            now,
            Some(VelocityAction::Liquidate),
            funding_paused,
        )?;

        let spot_position = user.get_spot_position(liability_market_index)?;

        validate!(
            spot_position.balance_type == SpotBalanceType::Borrow,
            ErrorCode::WrongSpotBalanceType,
            "User did not have a borrow for the liability market index"
        )?;

        let token_amount = spot_position.get_token_amount(&liability_market)?;

        validate!(
            token_amount != 0,
            ErrorCode::InvalidSpotPosition,
            "liability token amount zero for market index = {}",
            liability_market_index
        )?;

        // the liability side of the exchange rate gets the mirrored protection: a
        // margin-invalid (stale/uncertain) borrow oracle must not overvalue the debt
        // being repaid and inflate the collateral allowed to be swapped for it
        let liability_price = if is_oracle_valid_for_action(
            liability_oracle_validity,
            Some(VelocityAction::MarginCalc),
        )? {
            liability_price_data.price
        } else {
            calculate_user_protective_liability_price(
                liability_price_data,
                liability_market
                    .historical_oracle_data
                    .last_oracle_price_twap_5min,
            )?
        };

        (
            liability_price_data.price,
            liability_price,
            liability_market.decimals,
            liability_market.maintenance_liability_weight,
            liability_market.if_liquidation_fee,
            liability_market.protocol_liquidation_fee,
            liability_market.pool_id,
            liability_price_data.delay,
        )
    };

    if asset_pool_id == LST_POOL_ID && liability_pool_id == LST_POOL_ID {
        validate!(
            asset_oracle_delay == 0 && liability_oracle_delay == 0,
            ErrorCode::InvalidLiquidation,
            "asset oracle delay ({}) != 0 || liability oracle delay ({}) != 0",
            asset_oracle_delay,
            liability_oracle_delay
        )?;
    }

    let margin_context = MarginContext::liquidation(liquidation_margin_buffer_ratio)
        .track_market_margin_requirement(MarketIdentifier::spot(liability_market_index))?;

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        margin_context,
    )?;

    if !user.is_cross_margin_being_liquidated()
        && margin_calculation.meets_cross_margin_requirement()
    {
        msg!("margin calculation: {:?}", margin_calculation);
        return Err(ErrorCode::SufficientCollateral);
    } else if user.is_cross_margin_being_liquidated()
        && margin_calculation.can_exit_cross_margin_liquidation()?
    {
        msg!("margin calculation: {:?}", margin_calculation);
        return Err(ErrorCode::InvalidLiquidation);
    }

    let liquidation_id = user.enter_cross_margin_liquidation(slot)?;

    let canceled_order_ids = orders::cancel_orders(
        user,
        user_key,
        Some(liquidator_key),
        perp_market_map,
        spot_market_map,
        oracle_map,
        now,
        slot,
        OrderActionExplanation::Liquidation,
        None,
        None,
        None,
        true,
    )?;

    // check if user exited liquidation territory
    let intermediate_margin_calculation = if !canceled_order_ids.is_empty() {
        let intermediate_margin_calculation =
            calculate_margin_requirement_and_total_collateral_and_liability_info(
                user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                MarginContext::liquidation(liquidation_margin_buffer_ratio)
                    .track_market_margin_requirement(MarketIdentifier::spot(
                        liability_market_index,
                    ))?,
            )?;

        let initial_margin_shortage = margin_calculation.cross_margin_margin_shortage()?;
        let new_margin_shortage = intermediate_margin_calculation.cross_margin_margin_shortage()?;

        let margin_freed = initial_margin_shortage
            .saturating_sub(new_margin_shortage)
            .cast::<u64>()?;
        user.increment_margin_freed(margin_freed)?;

        emit!(LiquidationRecord {
            ts: now,
            liquidation_id,
            liquidation_type: LiquidationType::LiquidateSpot,
            user: *user_key,
            liquidator: *liquidator_key,
            margin_requirement: margin_calculation.margin_requirement,
            total_collateral: margin_calculation.total_collateral,
            bankrupt: user.is_cross_margin_bankrupt(),
            canceled_order_ids,
            margin_freed,
            liquidate_spot: LiquidateSpotRecord {
                asset_market_index,
                asset_price,
                asset_transfer: 0,
                liability_market_index,
                liability_price,
                liability_transfer: 0,
                if_fee: 0,
                protocol_fee: 0,
            },
            ..LiquidationRecord::default()
        });

        // must throw error to stop swap
        if intermediate_margin_calculation.can_exit_cross_margin_liquidation()? {
            return Err(ErrorCode::InvalidLiquidation);
        }

        intermediate_margin_calculation
    } else {
        margin_calculation.clone()
    };

    let margin_shortage = intermediate_margin_calculation.cross_margin_margin_shortage()?;

    let liability_weight_with_buffer =
        liability_weight.safe_add(liquidation_margin_buffer_ratio)?;

    // The borrow reduction the user receives in `liquidate_spot_with_swap_end`
    // is `liability_transfer - if_fee - protocol_fee`, so size the transfer
    // against the combined insurance-side fee. Using only `if_fee` here would
    // under-size the swap and leave the user with less margin relief than
    // intended (matches the combined fee `liquidate_spot` sizes with).
    let liability_total_if_side_fee = liability_if_fee.safe_add(liability_protocol_fee)?;

    // Determine what amount of borrow to transfer to reduce margin shortage to 0
    // assume 0 liquidator fee and swap is executed at oracle price.
    // valuation (shortage -> tokens) stays at the raw oracle price, consistent with
    // the margin calculation; only the exchange rate uses the protective price
    let liability_transfer_to_cover_margin_shortage =
        calculate_liability_transfer_to_cover_margin_shortage(
            margin_shortage,
            asset_weight,
            LIQUIDATION_FEE_PRECISION,
            liability_weight_with_buffer,
            LIQUIDATION_FEE_PRECISION,
            liability_decimals,
            liability_oracle_price,
            liability_total_if_side_fee,
        )?;

    let max_pct_allowed = calculate_max_pct_to_liquidate(
        user,
        margin_shortage,
        slot,
        initial_pct_to_liquidate,
        liquidation_duration,
    )?;
    let max_liability_allowed_to_be_transferred = liability_transfer_to_cover_margin_shortage
        .saturating_mul(max_pct_allowed)
        .safe_div(LIQUIDATION_PCT_PRECISION)?;

    if max_liability_allowed_to_be_transferred == 0 {
        msg!("max_liability_allowed_to_be_transferred == 0");
        return Err(ErrorCode::InvalidLiquidation);
    }

    // Size the swap bound against the time-ramped max-pct-to-liquidate throttle
    // (`max_liability_allowed_to_be_transferred`), NOT the uncapped
    // `liability_transfer_to_cover_margin_shortage`. Deriving `max_asset_transfer`
    // from the full shortage would let this lane seize more collateral in a single
    // swap than the throttle permits, since `swap_amount_in` is only bounded by
    // `max_asset_transfer` here (swap_end re-checks price, not the throttle). This
    // mirrors the direct `liquidate_spot` path, which caps the transfer at
    // `max_liability_allowed_to_be_transferred`.
    //
    // The bound is exact. No headroom is added on top of the throttle: begin and
    // end run in one transaction and read the same oracle prices, so there is no
    // price drift to absorb, and `swap_end` bounds the exchange rate on its own
    // with `validate_swap_within_liquidation_boundaries`. Headroom here only
    // raises the collateral volume the liquidator can seize above the throttle.
    // For the same reason this uses the exact conversion: the round-to-whole-
    // deposit form would lift the bound to the user's entire deposit whenever the
    // throttle lands within $1 of it.
    let max_asset_transfer = calculate_asset_transfer_for_liability_transfer_exact(
        LIQUIDATION_FEE_PRECISION,
        asset_decimals,
        asset_price,
        max_liability_allowed_to_be_transferred,
        LIQUIDATION_FEE_PRECISION,
        liability_decimals,
        liability_price,
    )?
    .min(asset_amount);

    if max_asset_transfer == 0 {
        msg!(
            "asset_market_index {} liability_market_index {}",
            asset_market_index,
            liability_market_index
        );
        msg!(
            "max_asset_transfer {} liability_transfer_to_cover_margin_shortage {}",
            max_asset_transfer,
            liability_transfer_to_cover_margin_shortage
        );
        msg!(
            "max_liability_allowed_to_be_transferred {} liability_transfer_to_cover_margin_shortage {}",
            max_liability_allowed_to_be_transferred,
            liability_transfer_to_cover_margin_shortage
        );
        msg!("swap_amount_in {}", swap_amount_in);
        return Err(ErrorCode::InvalidLiquidation);
    }

    validate!(
        max_asset_transfer >= swap_amount_in.cast()?,
        ErrorCode::InvalidLiquidation,
        "swap_amount_in larger than max_asset_transfer (swap_amount_in: {}, max_asset_transfer: {})",
        swap_amount_in,
        max_asset_transfer
    )?;

    validate!(
        asset_amount >= swap_amount_in.cast()?,
        ErrorCode::InvalidLiquidation,
        "swap_amount_in larger than asset_amount (swap_amount_in: {}, asset_amount: {})",
        swap_amount_in,
        asset_amount
    )?;

    let liability_oracle_too_divergent = is_oracle_too_divergent_with_twap_5min(
        liability_oracle_price.cast()?,
        spot_market_map
            .get_ref(&liability_market_index)?
            .historical_oracle_data
            .last_oracle_price_twap_5min,
        state
            .oracle_guard_rails
            .max_oracle_twap_5min_percent_divergence()
            .cast()?,
    )?;

    validate!(
        !liability_oracle_too_divergent,
        ErrorCode::PriceBandsBreached,
        "liability oracle too divergent"
    )?;

    let asset_oracle_too_divergent = is_oracle_too_divergent_with_twap_5min(
        asset_oracle_price.cast()?,
        spot_market_map
            .get_ref(&asset_market_index)?
            .historical_oracle_data
            .last_oracle_price_twap_5min,
        state
            .oracle_guard_rails
            .max_oracle_twap_5min_percent_divergence()
            .cast()?,
    )?;

    validate!(
        !asset_oracle_too_divergent,
        ErrorCode::PriceBandsBreached,
        "asset oracle too divergent"
    )?;

    Ok(())
}

pub fn liquidate_spot_with_swap_end(
    asset_market_index: u16,
    liability_market_index: u16,
    user: &mut User,
    user_key: &Pubkey,
    liquidator_key: &Pubkey,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    now: i64,
    slot: u64,
    state: &State,
    asset_transfer: u128,
    liability_transfer: u128,
) -> VelocityResult {
    let liquidation_margin_buffer_ratio = state.liquidation_margin_buffer_ratio;

    let (asset_price, asset_decimals, asset_weight, asset_liquidation_multiplier) = {
        let asset_market = spot_market_map.get_ref_mut(&asset_market_index)?;
        let (asset_price_data, validity_guard_rails) =
            oracle_map.get_price_data_and_guard_rails(&asset_market.oracle_id())?;

        // mirror the protective pricing applied in liquidate_spot_with_swap_begin: a
        // margin-invalid (stale/uncertain) deposit oracle must not lower the worst-case
        // swap price the liquidator's swap is validated against
        let asset_price = if asset_market.market_index == QUOTE_SPOT_MARKET_INDEX {
            asset_price_data.price
        } else {
            let asset_oracle_validity = oracle_validity(
                MarketType::Spot,
                asset_market.market_index,
                asset_market.historical_oracle_data.last_oracle_price_twap,
                asset_price_data,
                validity_guard_rails,
                asset_market.get_max_confidence_interval_multiplier()?,
                &asset_market.oracle_source,
                LogMode::None,
                -1,
                false, // exchange-oracle price, never MM-sourced
                0,
            )?;

            if is_oracle_valid_for_action(asset_oracle_validity, Some(VelocityAction::MarginCalc))?
            {
                asset_price_data.price
            } else {
                calculate_user_protective_asset_price(
                    asset_price_data,
                    asset_market
                        .historical_oracle_data
                        .last_oracle_price_twap_5min,
                )?
            }
        };

        (
            asset_price,
            asset_market.decimals,
            asset_market.maintenance_asset_weight,
            calculate_liquidation_multiplier(
                asset_market.liquidator_fee,
                LiquidationMultiplierType::Premium,
            )?,
        )
    };

    let (
        liability_price,
        liability_decimals,
        liability_weight,
        liability_liquidation_multiplier,
        liability_if_liquidation_fee,
        liability_protocol_liquidation_fee,
    ) = {
        let liability_market = spot_market_map.get_ref_mut(&liability_market_index)?;
        let (liability_price_data, validity_guard_rails) =
            oracle_map.get_price_data_and_guard_rails(&liability_market.oracle_id())?;

        // mirror the protective pricing applied in liquidate_spot_with_swap_begin: a
        // margin-invalid (stale/uncertain) borrow oracle must not raise the worst-case
        // swap price the liquidator's swap is validated against
        let liability_price = if liability_market.market_index == QUOTE_SPOT_MARKET_INDEX {
            liability_price_data.price
        } else {
            let liability_oracle_validity = oracle_validity(
                MarketType::Spot,
                liability_market.market_index,
                liability_market
                    .historical_oracle_data
                    .last_oracle_price_twap,
                liability_price_data,
                validity_guard_rails,
                liability_market.get_max_confidence_interval_multiplier()?,
                &liability_market.oracle_source,
                LogMode::None,
                -1,
                false, // exchange-oracle price, never MM-sourced
                0,
            )?;

            if is_oracle_valid_for_action(
                liability_oracle_validity,
                Some(VelocityAction::MarginCalc),
            )? {
                liability_price_data.price
            } else {
                calculate_user_protective_liability_price(
                    liability_price_data,
                    liability_market
                        .historical_oracle_data
                        .last_oracle_price_twap_5min,
                )?
            }
        };

        (
            liability_price,
            liability_market.decimals,
            liability_market.maintenance_liability_weight,
            calculate_liquidation_multiplier(
                liability_market.liquidator_fee,
                LiquidationMultiplierType::Discount,
            )?,
            liability_market.if_liquidation_fee,
            liability_market.protocol_liquidation_fee,
        )
    };

    validate_swap_within_liquidation_boundaries(
        asset_transfer,
        liability_transfer,
        asset_decimals,
        liability_decimals,
        asset_price,
        liability_price,
        asset_liquidation_multiplier,
        liability_liquidation_multiplier,
    )?;

    let margin_context = MarginContext::liquidation(liquidation_margin_buffer_ratio)
        .track_market_margin_requirement(MarketIdentifier::spot(liability_market_index))?;

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        margin_context,
    )?;

    let liquidation_id = user.enter_cross_margin_liquidation(slot)?;
    let mut margin_freed = 0_u64;

    let margin_shortage = margin_calculation.cross_margin_margin_shortage()?;

    // Audit #51: cap the insurance-side fee by the account's margin shortage,
    // exactly as the direct spot-liquidation path (`liquidate_spot`) does via
    // `calculate_spot_if_fee`. Charging the raw if + protocol rates on the
    // swap-realized borrow relief would route value into the fee pools that the
    // account needs to climb out of its shortage, delivering less borrow relief
    // than the direct path for an equivalent seizure. The basis is the
    // swap-realized `liability_transfer` (the actual borrow reduction), and the
    // capped total is split IF-first then protocol, mirroring `liquidate_spot`.
    let liability_weight_with_buffer =
        liability_weight.safe_add(liquidation_margin_buffer_ratio)?;
    let total_if_side_fee = calculate_spot_if_fee(
        margin_calculation.tracked_market_margin_shortage(margin_shortage)?,
        liability_transfer,
        asset_weight,
        asset_liquidation_multiplier,
        liability_weight_with_buffer,
        liability_liquidation_multiplier,
        liability_decimals,
        liability_price,
        liability_if_liquidation_fee.safe_add(liability_protocol_liquidation_fee)?,
    )?;
    let liquidation_if_fee = total_if_side_fee.min(liability_if_liquidation_fee);
    let liquidation_protocol_fee = total_if_side_fee.safe_sub(liquidation_if_fee)?;

    let if_fee = liability_transfer
        .cast::<u128>()?
        .safe_mul(liquidation_if_fee.cast()?)?
        .safe_div(LIQUIDATION_FEE_PRECISION_U128)?;
    let protocol_fee = liability_transfer
        .cast::<u128>()?
        .safe_mul(liquidation_protocol_fee.cast()?)?
        .safe_div(LIQUIDATION_FEE_PRECISION_U128)?;
    {
        let mut liability_market = spot_market_map.get_ref_mut(&liability_market_index)?;

        let user_liability_reduction = liability_transfer
            .cast::<u128>()?
            .safe_sub(if_fee)?
            .safe_sub(protocol_fee)?;
        update_spot_balances_and_cumulative_deposits(
            user_liability_reduction,
            &SpotBalanceType::Deposit,
            &mut liability_market,
            user.get_spot_position_mut(liability_market_index)?,
            false,
            Some(user_liability_reduction),
        )?;

        update_revenue_pool_balances(
            if_fee,
            &SpotBalanceType::Deposit,
            &mut liability_market,
            false,
        )?;
        update_protocol_fee_pool_balances(
            protocol_fee,
            &SpotBalanceType::Deposit,
            &mut liability_market,
            false,
        )?;
    }

    {
        let mut asset_market = spot_market_map.get_ref_mut(&asset_market_index)?;

        update_spot_balances_and_cumulative_deposits(
            asset_transfer,
            &SpotBalanceType::Borrow,
            &mut asset_market,
            user.force_get_spot_position_mut(asset_market_index)?,
            false,
            Some(asset_transfer),
        )?;
    }

    let (margin_freed_from_liability, margin_calulcation_after) = calculate_margin_freed(
        user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        liquidation_margin_buffer_ratio,
        margin_shortage,
        None,
    )?;

    margin_freed = margin_freed.safe_add(margin_freed_from_liability)?;
    user.increment_margin_freed(margin_freed_from_liability)?;

    if margin_calulcation_after.can_exit_cross_margin_liquidation()? {
        user.exit_cross_margin_liquidation();
    } else if is_cross_margin_bankrupt(user, spot_market_map, perp_market_map)? {
        user.enter_cross_margin_bankruptcy();
    }

    emit!(LiquidationRecord {
        ts: now,
        liquidation_id,
        liquidation_type: LiquidationType::LiquidateSpot,
        user: *user_key,
        liquidator: *liquidator_key,
        margin_requirement: margin_calculation.margin_requirement,
        total_collateral: margin_calculation.total_collateral,
        bankrupt: user.is_cross_margin_bankrupt(),
        margin_freed,
        liquidate_spot: LiquidateSpotRecord {
            asset_market_index,
            asset_price,
            asset_transfer,
            liability_market_index,
            liability_price,
            liability_transfer,
            if_fee: if_fee.cast()?,
            protocol_fee: protocol_fee.cast()?,
        },
        ..LiquidationRecord::default()
    });

    Ok(())
}

pub fn liquidate_borrow_for_perp_pnl(
    perp_market_index: u16,
    liability_market_index: u16,
    liquidator_max_liability_transfer: u128,
    limit_price: Option<u64>,
    user: &mut User,
    user_key: &Pubkey,
    liquidator: &mut User,
    liquidator_key: &Pubkey,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    now: i64,
    slot: u64,
    liquidation_margin_buffer_ratio: u32,
    initial_pct_to_liquidate: u128,
    liquidation_duration: u128,
    funding_paused: bool,
) -> VelocityResult {
    // liquidator takes over a user borrow in exchange for that user's positive perpetual pnl
    // can only be done once a user's perpetual position size is 0
    // blocks borrows where oracle is deemed invalid

    validate!(
        !user.is_cross_margin_bankrupt(),
        ErrorCode::UserBankrupt,
        "user bankrupt",
    )?;

    validate!(
        !liquidator.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "liquidator bankrupt",
    )?;

    validate!(
        liquidator.pool_id == 0,
        ErrorCode::InvalidPoolId,
        "liquidator pool id ({}) != 0",
        liquidator.pool_id
    )?;

    let perp_market = perp_market_map.get_ref(&perp_market_index)?;

    validate!(
        !perp_market.is_operation_paused(PerpOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for perp market {}",
        perp_market_index
    )?;

    drop(perp_market);

    let liability_spot_market = spot_market_map.get_ref(&liability_market_index)?;

    validate!(
        !liability_spot_market.is_operation_paused(SpotOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        liability_market_index
    )?;

    drop(liability_spot_market);

    user.get_perp_position(perp_market_index)
        .inspect_err(|_e| {
            msg!(
                "User does not have a position for perp market {}",
                perp_market_index
            );
        })?;

    user.get_spot_position(liability_market_index)
        .map_err(|_| {
            msg!(
                "User does not have a spot balance for liability market {}",
                liability_market_index
            );
            ErrorCode::CouldNotFindSpotPosition
        })?;

    liquidator
        .force_get_perp_position_mut(perp_market_index)
        .inspect_err(|_e| {
            msg!("Liquidator has no available positions to take on pnl");
        })?;

    liquidator
        .force_get_spot_position_mut(liability_market_index)
        .inspect_err(|_e| {
            msg!("Liquidator has no available spot balances to take on borrow");
        })?;

    settle_funding_payment(
        user,
        user_key,
        perp_market_map.get_ref_mut(&perp_market_index)?.deref_mut(),
        now,
    )?;

    settle_funding_payment(
        liquidator,
        liquidator_key,
        perp_market_map.get_ref_mut(&perp_market_index)?.deref_mut(),
        now,
    )?;

    let (pnl, quote_price, quote_decimals, pnl_asset_weight, pnl_liquidation_multiplier) = {
        let user_position = user.get_perp_position(perp_market_index)?;

        let base_asset_amount = user_position.base_asset_amount;

        validate!(
            base_asset_amount == 0,
            ErrorCode::InvalidPerpPositionToLiquidate,
            "Cant have open perp position (base_asset_amount: {})",
            base_asset_amount
        )?;

        let pnl = user_position.quote_asset_amount.cast::<i128>()?;

        validate!(
            pnl > 0,
            ErrorCode::InvalidPerpPositionToLiquidate,
            "Perp position must have position pnl"
        )?;

        validate!(
            !user_position.is_isolated(),
            ErrorCode::InvalidPerpPositionToLiquidate,
            "Perp position is an isolated position"
        )?;

        let market = perp_market_map.get_ref(&perp_market_index)?;

        let quote_spot_market = spot_market_map.get_ref(&market.quote_spot_market_index)?;
        let quote_price = oracle_map
            .get_price_data(&quote_spot_market.oracle_id())?
            .price;

        let pnl_asset_weight =
            market.get_unrealized_asset_weight(pnl, MarginRequirementType::Maintenance)?;

        (
            pnl.unsigned_abs(),
            quote_price,
            6_u32,
            pnl_asset_weight,
            calculate_liquidation_multiplier(
                market.liquidator_fee,
                LiquidationMultiplierType::Premium,
            )?,
        )
    };

    let (
        liability_amount,
        liability_oracle_price,
        liability_price,
        liability_decimals,
        liability_weight,
        liability_liquidation_multiplier,
    ) = {
        let mut liability_market = spot_market_map.get_ref_mut(&liability_market_index)?;
        let (liability_price_data, validity_guard_rails) =
            oracle_map.get_price_data_and_guard_rails(&liability_market.oracle_id())?;

        let liability_oracle_validity = update_spot_market_and_check_validity(
            &mut liability_market,
            liability_price_data,
            validity_guard_rails,
            now,
            Some(VelocityAction::Liquidate),
            funding_paused,
        )?;

        let spot_position = user.get_spot_position(liability_market_index)?;

        validate!(
            spot_position.balance_type == SpotBalanceType::Borrow,
            ErrorCode::WrongSpotBalanceType,
            "User did not have a borrow for the borrow market index"
        )?;

        let token_amount = spot_position.get_token_amount(&liability_market)?;

        validate!(
            token_amount != 0,
            ErrorCode::InvalidSpotPosition,
            "liability token amount zero for market index = {}",
            liability_market_index
        )?;

        // the liability side of the exchange rate gets the mirrored protection: a
        // margin-invalid (stale/uncertain) borrow oracle must not overvalue the debt
        // being taken over and cheapen the pnl received for it
        let liability_price = if is_oracle_valid_for_action(
            liability_oracle_validity,
            Some(VelocityAction::MarginCalc),
        )? {
            liability_price_data.price
        } else {
            calculate_user_protective_liability_price(
                liability_price_data,
                liability_market
                    .historical_oracle_data
                    .last_oracle_price_twap_5min,
            )?
        };

        (
            token_amount,
            liability_price_data.price,
            liability_price,
            liability_market.decimals,
            liability_market.maintenance_liability_weight,
            calculate_liquidation_multiplier(
                liability_market.liquidator_fee,
                LiquidationMultiplierType::Discount,
            )?,
        )
    };

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        MarginContext::liquidation(liquidation_margin_buffer_ratio),
    )?;

    if !user.is_cross_margin_being_liquidated()
        && margin_calculation.meets_cross_margin_requirement()
    {
        msg!("margin calculation {:?}", margin_calculation);
        return Err(ErrorCode::SufficientCollateral);
    } else if user.is_cross_margin_being_liquidated()
        && margin_calculation.can_exit_cross_margin_liquidation()?
    {
        user.exit_cross_margin_liquidation();
        return Ok(());
    }

    let liquidation_id = user.enter_cross_margin_liquidation(slot)?;
    let mut margin_freed = 0_u64;

    let canceled_order_ids = orders::cancel_orders(
        user,
        user_key,
        Some(liquidator_key),
        perp_market_map,
        spot_market_map,
        oracle_map,
        now,
        slot,
        OrderActionExplanation::Liquidation,
        None,
        None,
        None,
        true,
    )?;

    // check if user exited liquidation territory
    let intermediate_margin_calculation = if !canceled_order_ids.is_empty() {
        let intermediate_margin_calculation =
            calculate_margin_requirement_and_total_collateral_and_liability_info(
                user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                MarginContext::liquidation(liquidation_margin_buffer_ratio),
            )?;

        let initial_margin_shortage = margin_calculation.cross_margin_margin_shortage()?;
        let new_margin_shortage = intermediate_margin_calculation.cross_margin_margin_shortage()?;

        margin_freed = initial_margin_shortage
            .saturating_sub(new_margin_shortage)
            .cast::<u64>()?;
        user.increment_margin_freed(margin_freed)?;

        if intermediate_margin_calculation.can_exit_cross_margin_liquidation()? {
            let market = perp_market_map.get_ref(&perp_market_index)?;
            let market_oracle_price = oracle_map.get_price_data(&market.oracle_id())?.price;

            emit!(LiquidationRecord {
                ts: now,
                liquidation_id,
                liquidation_type: LiquidationType::LiquidateBorrowForPerpPnl,
                user: *user_key,
                liquidator: *liquidator_key,
                margin_requirement: margin_calculation.margin_requirement,
                total_collateral: margin_calculation.total_collateral,
                bankrupt: user.is_cross_margin_bankrupt(),
                canceled_order_ids,
                margin_freed,
                liquidate_borrow_for_perp_pnl: LiquidateBorrowForPerpPnlRecord {
                    perp_market_index,
                    market_oracle_price,
                    pnl_transfer: 0,
                    liability_market_index,
                    liability_price,
                    liability_transfer: 0,
                },
                ..LiquidationRecord::default()
            });

            user.exit_cross_margin_liquidation();
            return Ok(());
        }

        intermediate_margin_calculation
    } else {
        margin_calculation.clone()
    };

    let margin_shortage = intermediate_margin_calculation.cross_margin_margin_shortage()?;

    let liability_weight_with_buffer =
        liability_weight.safe_add(liquidation_margin_buffer_ratio)?;

    // Determine what amount of borrow to transfer to reduce margin shortage to 0.
    // valuation (shortage -> tokens) stays at the raw oracle price, consistent with
    // the margin calculation; only the exchange rate uses the protective price
    let liability_transfer_to_cover_margin_shortage =
        calculate_liability_transfer_to_cover_margin_shortage(
            margin_shortage,
            pnl_asset_weight,
            pnl_liquidation_multiplier,
            liability_weight_with_buffer,
            liability_liquidation_multiplier,
            liability_decimals,
            liability_oracle_price,
            0,
        )?;

    let max_pct_allowed = calculate_max_pct_to_liquidate(
        user,
        margin_shortage,
        slot,
        initial_pct_to_liquidate,
        liquidation_duration,
    )?;
    let max_liability_allowed_to_be_transferred = liability_transfer_to_cover_margin_shortage
        .saturating_mul(max_pct_allowed)
        .safe_div(LIQUIDATION_PCT_PRECISION)?;

    if max_liability_allowed_to_be_transferred == 0 {
        msg!("max_liability_allowed_to_be_transferred == 0");
        return Ok(());
    }

    // Given the user's deposit amount, how much borrow can be transferred?
    let liability_transfer_implied_by_pnl = calculate_liability_transfer_implied_by_asset_amount(
        pnl,
        pnl_liquidation_multiplier,
        quote_decimals,
        quote_price,
        liability_liquidation_multiplier,
        liability_decimals,
        liability_price,
    )?;

    let liability_value = get_token_value(
        liability_amount.cast()?,
        liability_decimals,
        liability_oracle_price,
    )?;

    let minimum_liability_transfer = if liability_value > 10 * QUOTE_PRECISION_I128 {
        0_u128
    } else {
        liability_amount
    };

    let liability_transfer = liquidator_max_liability_transfer
        .min(liability_amount)
        // want to make sure the liability_transfer_to_cover_margin_shortage doesn't lead to dust positions
        .min(max_liability_allowed_to_be_transferred.max(minimum_liability_transfer))
        .min(liability_transfer_implied_by_pnl);

    // Given the borrow amount to transfer, determine how much deposit amount to transfer
    let pnl_transfer = calculate_asset_transfer_for_liability_transfer(
        pnl,
        pnl_liquidation_multiplier,
        quote_decimals,
        quote_price,
        liability_transfer,
        liability_liquidation_multiplier,
        liability_decimals,
        liability_price,
    )?;

    if liability_transfer == 0 || pnl_transfer == 0 {
        msg!(
            "perp_market_index {} liability_market_index {}",
            perp_market_index,
            liability_market_index
        );
        msg!("liquidator_max_liability_transfer {} liability_amount {} liability_transfer_to_cover_margin_shortage {}", liquidator_max_liability_transfer, liability_amount, liability_transfer_to_cover_margin_shortage);
        msg!(
            "liability_transfer_implied_by_pnl {} liability_transfer {} pnl_transfer {}",
            liability_transfer_implied_by_pnl,
            liability_transfer,
            pnl_transfer
        );
        return Err(ErrorCode::InvalidLiquidation);
    }

    validate_transfer_satisfies_limit_price(
        pnl_transfer,
        liability_transfer,
        quote_decimals,
        liability_decimals,
        limit_price,
    )?;

    {
        let mut liability_market = spot_market_map.get_ref_mut(&liability_market_index)?;

        update_spot_balances_and_cumulative_deposits(
            liability_transfer,
            &SpotBalanceType::Deposit,
            &mut liability_market,
            user.force_get_spot_position_mut(liability_market_index)?,
            false,
            Some(liability_transfer),
        )?;

        update_spot_balances_and_cumulative_deposits(
            liability_transfer,
            &SpotBalanceType::Borrow,
            &mut liability_market,
            liquidator.force_get_spot_position_mut(liability_market_index)?,
            false,
            Some(liability_transfer),
        )?;
    }

    {
        let mut market = perp_market_map.get_ref_mut(&perp_market_index)?;
        let liquidator_position = liquidator.force_get_perp_position_mut(perp_market_index)?;
        update_quote_asset_amount(liquidator_position, &mut market, pnl_transfer.cast()?)?;

        let user_position = user.get_perp_position_mut(perp_market_index)?;
        update_quote_asset_amount(user_position, &mut market, -pnl_transfer.cast()?)?;
    }

    let (margin_freed_from_liability, _) = calculate_margin_freed(
        user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        liquidation_margin_buffer_ratio,
        margin_shortage,
        None,
    )?;
    margin_freed = margin_freed.safe_add(margin_freed_from_liability)?;
    user.increment_margin_freed(margin_freed_from_liability)?;

    if liability_transfer >= liability_transfer_to_cover_margin_shortage {
        user.exit_cross_margin_liquidation();
    } else if is_cross_margin_bankrupt(user, spot_market_map, perp_market_map)? {
        user.enter_cross_margin_bankruptcy();
    }

    let liquidator_meets_initial_margin_requirement =
        meets_initial_margin_requirement(liquidator, perp_market_map, spot_market_map, oracle_map)?;

    validate!(
        liquidator_meets_initial_margin_requirement,
        ErrorCode::InsufficientCollateral,
        "Liquidator doesnt have enough collateral to take over borrow"
    )?;

    // The liquidation adds exposure to the liquidator like a risk-increasing
    // fill; the liquidator subaccount must clear its own buffered equity floor
    // to take it on.
    if let Some(liquidator_net_equity) =
        calculate_net_equity_for_floor(liquidator, perp_market_map, spot_market_map, oracle_map)?
    {
        liquidator_net_equity.validate_clears_buffered_floor(liquidator)?;
    }

    let market_oracle_price = {
        let market = perp_market_map.get_ref_mut(&perp_market_index)?;
        oracle_map.get_price_data(&market.oracle_id())?.price
    };

    emit!(LiquidationRecord {
        ts: now,
        liquidation_id,
        liquidation_type: LiquidationType::LiquidateBorrowForPerpPnl,
        user: *user_key,
        liquidator: *liquidator_key,
        margin_requirement: margin_calculation.margin_requirement,
        total_collateral: margin_calculation.total_collateral,
        bankrupt: user.is_cross_margin_bankrupt(),
        margin_freed,
        liquidate_borrow_for_perp_pnl: LiquidateBorrowForPerpPnlRecord {
            perp_market_index,
            market_oracle_price,
            pnl_transfer,
            liability_market_index,
            liability_price,
            liability_transfer,
        },
        ..LiquidationRecord::default()
    });

    Ok(())
}

pub fn liquidate_perp_pnl_for_deposit(
    perp_market_index: u16,
    asset_market_index: u16,
    liquidator_max_pnl_transfer: u128,
    limit_price: Option<u64>,
    user: &mut User,
    user_key: &Pubkey,
    liquidator: &mut User,
    liquidator_key: &Pubkey,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    now: i64,
    slot: u64,
    liquidation_margin_buffer_ratio: u32,
    initial_pct_to_liquidate: u128,
    liquidation_duration: u128,
    funding_paused: bool,
) -> VelocityResult {
    // liquidator takes over remaining negative perpetual pnl in exchange for a user deposit
    // can only be done once the perpetual position's size is 0
    // blocked when 1) user deposit oracle is deemed invalid
    // or 2) user has outstanding liability with higher tier

    let liquidation_mode = get_perp_liquidation_mode(user, perp_market_index)?;

    validate!(
        !liquidation_mode.is_user_bankrupt(user)?,
        ErrorCode::UserBankrupt,
        "user bankrupt",
    )?;

    validate!(
        !liquidator.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "liquidator bankrupt",
    )?;

    validate!(
        liquidator.pool_id == 0,
        ErrorCode::InvalidPoolId,
        "liquidator pool id ({}) != 0",
        liquidator.pool_id
    )?;

    let asset_spot_market = spot_market_map.get_ref(&asset_market_index)?;

    validate!(
        !asset_spot_market.is_operation_paused(SpotOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        asset_market_index
    )?;

    drop(asset_spot_market);

    let perp_market = perp_market_map.get_ref(&perp_market_index)?;

    validate!(
        !perp_market.is_operation_paused(PerpOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        perp_market_index
    )?;

    // Audit #25 scoping: an expired/delisted market (Settlement) winds positions
    // down at the expiry price regardless of margin improvement, so the
    // "shortage must not grow" postcondition below is deliberately skipped there
    // — see the guard for the rationale.
    let market_in_settlement = perp_market.status == MarketStatus::Settlement;

    drop(perp_market);

    user.get_perp_position(perp_market_index)
        .inspect_err(|_e| {
            msg!(
                "User does not have a position for perp market {}",
                perp_market_index
            );
        })?;

    liquidation_mode.validate_spot_position(user, asset_market_index)?;

    liquidator
        .force_get_perp_position_mut(perp_market_index)
        .inspect_err(|_e| {
            msg!("Liquidator has no available positions to take on pnl");
        })?;

    liquidator
        .force_get_spot_position_mut(asset_market_index)
        .inspect_err(|_e| {
            msg!("Liquidator has no available spot balances to take on deposit");
        })?;

    settle_funding_payment(
        user,
        user_key,
        perp_market_map.get_ref_mut(&perp_market_index)?.deref_mut(),
        now,
    )?;

    settle_funding_payment(
        liquidator,
        liquidator_key,
        perp_market_map.get_ref_mut(&perp_market_index)?.deref_mut(),
        now,
    )?;

    let (
        asset_amount,
        asset_price,
        _asset_tier,
        asset_decimals,
        asset_weight,
        asset_liquidation_multiplier,
    ) = {
        let mut asset_market = spot_market_map.get_ref_mut(&asset_market_index)?;
        let (asset_price_data, validity_guard_rails) =
            oracle_map.get_price_data_and_guard_rails(&asset_market.oracle_id())?;

        let asset_oracle_validity = update_spot_market_and_check_validity(
            &mut asset_market,
            asset_price_data,
            validity_guard_rails,
            now,
            Some(VelocityAction::Liquidate),
            funding_paused,
        )?;

        // a margin-invalid (stale/uncertain) deposit oracle may make the account
        // liquidatable, but must not let its collateral be seized at a depressed
        // price: size the transfer at a user-protective price instead
        let token_price =
            if is_oracle_valid_for_action(asset_oracle_validity, Some(VelocityAction::MarginCalc))?
            {
                asset_price_data.price
            } else {
                calculate_user_protective_asset_price(
                    asset_price_data,
                    asset_market
                        .historical_oracle_data
                        .last_oracle_price_twap_5min,
                )?
            };

        let token_amount = liquidation_mode.get_spot_token_amount(user, &asset_market)?;

        (
            token_amount,
            token_price,
            asset_market.asset_tier,
            asset_market.decimals,
            asset_market.maintenance_asset_weight,
            calculate_liquidation_multiplier(
                asset_market.liquidator_fee,
                LiquidationMultiplierType::Premium,
            )?,
        )
    };

    let (
        unsettled_pnl,
        quote_price,
        contract_tier,
        quote_decimals,
        pnl_liability_weight,
        pnl_liquidation_multiplier,
    ) = {
        let user_position = user.get_perp_position(perp_market_index)?;

        let base_asset_amount = user_position.base_asset_amount;

        validate!(
            base_asset_amount == 0,
            ErrorCode::InvalidPerpPositionToLiquidate,
            "Cant have open perp position (base_asset_amount: {})",
            base_asset_amount
        )?;

        let unsettled_pnl = user_position.quote_asset_amount.cast::<i128>()?;

        validate!(
            unsettled_pnl < 0,
            ErrorCode::InvalidPerpPositionToLiquidate,
            "Perp position must have negative pnl"
        )?;

        let market = perp_market_map.get_ref(&perp_market_index)?;

        let quote_spot_market = spot_market_map.get_ref(&market.quote_spot_market_index)?;
        let quote_price = oracle_map
            .get_price_data(&quote_spot_market.oracle_id())?
            .price;

        (
            unsettled_pnl.unsigned_abs(),
            quote_price,
            market.contract_tier,
            6_u32,
            SPOT_WEIGHT_PRECISION,
            calculate_liquidation_multiplier(
                market.liquidator_fee,
                LiquidationMultiplierType::Discount,
            )?,
        )
    };

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        MarginContext::liquidation(liquidation_margin_buffer_ratio),
    )?;

    let user_is_being_liquidated = liquidation_mode.user_is_being_liquidated(user)?;
    if !user_is_being_liquidated
        && liquidation_mode.meets_margin_requirements(&margin_calculation)?
    {
        msg!("margin calculation {:?}", margin_calculation);
        return Err(ErrorCode::SufficientCollateral);
    } else if user_is_being_liquidated
        && liquidation_mode.can_exit_liquidation(&margin_calculation)?
    {
        liquidation_mode.exit_liquidation(user)?;
        return Ok(());
    }

    let liquidation_id = liquidation_mode.enter_liquidation(user, slot)?;
    let mut margin_freed = 0_u64;

    let (cancel_orders_market_type, cancel_orders_market_index) =
        liquidation_mode.get_cancel_orders_params();
    let canceled_order_ids = orders::cancel_orders(
        user,
        user_key,
        Some(liquidator_key),
        perp_market_map,
        spot_market_map,
        oracle_map,
        now,
        slot,
        OrderActionExplanation::Liquidation,
        cancel_orders_market_type,
        cancel_orders_market_index,
        None,
        true,
    )?;

    let (safest_tier_spot_liability, safest_tier_perp_liability) = liquidation_mode
        .calculate_user_safest_position_tiers(user, perp_market_map, spot_market_map)?;
    let is_contract_tier_violation =
        !(contract_tier.is_as_safe_as(&safest_tier_perp_liability, &safest_tier_spot_liability));

    // check if user exited liquidation territory
    let intermediate_margin_calculation = if !canceled_order_ids.is_empty() {
        let intermediate_margin_calculation =
            calculate_margin_requirement_and_total_collateral_and_liability_info(
                user,
                perp_market_map,
                spot_market_map,
                oracle_map,
                MarginContext::liquidation(liquidation_margin_buffer_ratio),
            )?;

        let initial_margin_shortage = liquidation_mode.margin_shortage(&margin_calculation)?;
        let new_margin_shortage =
            liquidation_mode.margin_shortage(&intermediate_margin_calculation)?;

        margin_freed = initial_margin_shortage
            .saturating_sub(new_margin_shortage)
            .cast::<u64>()?;
        liquidation_mode.increment_free_margin(user, margin_freed)?;

        let exiting_liq_territory =
            liquidation_mode.can_exit_liquidation(&intermediate_margin_calculation)?;

        if exiting_liq_territory || is_contract_tier_violation {
            let market = perp_market_map.get_ref(&perp_market_index)?;
            let market_oracle_price = oracle_map.get_price_data(&market.oracle_id())?.price;

            let (margin_requirement, total_collateral, bit_flags) =
                liquidation_mode.get_event_fields(&margin_calculation)?;
            emit!(LiquidationRecord {
                ts: now,
                liquidation_id,
                liquidation_type: LiquidationType::LiquidatePerpPnlForDeposit,
                user: *user_key,
                liquidator: *liquidator_key,
                margin_requirement,
                total_collateral,
                bankrupt: liquidation_mode.is_user_bankrupt(user)?,
                canceled_order_ids,
                margin_freed,
                liquidate_perp_pnl_for_deposit: LiquidatePerpPnlForDepositRecord {
                    perp_market_index,
                    market_oracle_price,
                    pnl_transfer: 0,
                    asset_market_index,
                    asset_price,
                    asset_transfer: 0,
                },
                bit_flags,
                ..LiquidationRecord::default()
            });

            if exiting_liq_territory {
                liquidation_mode.exit_liquidation(user)?;
            } else if is_contract_tier_violation {
                msg!(
                        "return early after cancel orders: liquidating contract tier={:?} pnl is riskier than outstanding {:?} & {:?}",
                        contract_tier,
                        safest_tier_perp_liability,
                        safest_tier_spot_liability
                    );
            }

            return Ok(());
        }

        intermediate_margin_calculation
    } else {
        margin_calculation.clone()
    };

    if is_contract_tier_violation {
        msg!(
            "liquidating contract tier={:?} pnl is riskier than outstanding {:?} & {:?}",
            contract_tier,
            safest_tier_perp_liability,
            safest_tier_spot_liability
        );
        return Err(ErrorCode::TierViolationLiquidatingPerpPnl);
    }

    let margin_shortage = liquidation_mode.margin_shortage(&intermediate_margin_calculation)?;

    let pnl_liability_weight_plus_buffer =
        pnl_liability_weight.safe_add(liquidation_margin_buffer_ratio)?;

    // Audit #25: refuse a transfer that cannot improve the account. The account
    // gives up deposit valued at `asset_weight` and priced with the liquidator
    // premium, and receives pnl relief valued at `pnl_liability_weight_plus_buffer`
    // and priced with the liquidator discount. The margin improvement per unit
    // transferred is therefore constant, and it is positive only while the asset
    // side stays below the liability side. When the asset side reaches the
    // liability side, every transfer size strips more collateral than it frees, so
    // no partial size helps and the call must revert.
    // `calculate_liability_transfer_to_cover_margin_shortage` below detects the
    // same condition, but reports it as `u128::MAX`. The sizing then reads that
    // sentinel as "no bound" and transfers the largest amount the other caps allow.
    //
    // `asset_weight` is the raw maintenance weight. A size-scaled (imf) weight is
    // never higher, so this check errs toward refusing a transfer that would in
    // fact help by a small amount.
    //
    // Settlement is exempt for the reason given at `market_in_settlement`.
    if !market_in_settlement {
        // The extra factor of 10 mirrors the precision scaling in
        // `calculate_liability_transfer_to_cover_margin_shortage`.
        let asset_weight_component = asset_weight
            .cast::<u128>()?
            .safe_mul(10)?
            .safe_mul(asset_liquidation_multiplier.cast::<u128>()?)?
            .safe_div(pnl_liquidation_multiplier.cast::<u128>()?)?;
        let pnl_liability_weight_component = pnl_liability_weight_plus_buffer
            .cast::<u128>()?
            .safe_mul(10)?;

        validate!(
            asset_weight_component < pnl_liability_weight_component,
            ErrorCode::LiquidationWorsensAccountHealth,
            "liquidate_perp_pnl_for_deposit cannot improve account health (asset weight component {} >= liability weight component {})",
            asset_weight_component,
            pnl_liability_weight_component
        )?;
    }

    // Determine what amount of borrow to transfer to reduce margin shortage to 0
    let pnl_transfer_to_cover_margin_shortage =
        calculate_liability_transfer_to_cover_margin_shortage(
            margin_shortage,
            asset_weight,
            asset_liquidation_multiplier,
            pnl_liability_weight_plus_buffer,
            pnl_liquidation_multiplier,
            quote_decimals,
            quote_price,
            0, // no if fee
        )?;

    let max_pct_allowed = liquidation_mode.calculate_max_pct_to_liquidate(
        user,
        margin_shortage,
        slot,
        initial_pct_to_liquidate,
        liquidation_duration,
    )?;
    let max_pnl_allowed_to_be_transferred = pnl_transfer_to_cover_margin_shortage
        .saturating_mul(max_pct_allowed)
        .safe_div(LIQUIDATION_PCT_PRECISION)?;

    if max_pnl_allowed_to_be_transferred == 0 {
        msg!("max_pnl_allowed_to_be_transferred == 0");
        return Ok(());
    }

    // Given the user's deposit amount, how much borrow can be transferred?
    let pnl_transfer_implied_by_asset_amount =
        calculate_liability_transfer_implied_by_asset_amount(
            asset_amount,
            asset_liquidation_multiplier,
            asset_decimals,
            asset_price,
            pnl_liquidation_multiplier,
            quote_decimals,
            quote_price,
        )?;

    let minimum_pnl_transfer = if unsettled_pnl > 10 * QUOTE_PRECISION {
        0_u128
    } else {
        unsettled_pnl
    };

    let pnl_transfer = liquidator_max_pnl_transfer
        .min(unsettled_pnl)
        // want to make sure the pnl_transfer_to_cover_margin_shortage doesn't lead to dust pnl
        .min(max_pnl_allowed_to_be_transferred.max(minimum_pnl_transfer))
        .min(pnl_transfer_implied_by_asset_amount);

    // Given the borrow amount to transfer, determine how much deposit amount to transfer.
    //
    // Audit #25: every unit seized must be paid for, so this path does not use the
    // round-to-whole-deposit form of the conversion. That form takes up to $1 of
    // collateral the pnl relief does not cover, which is real value, not rounding.
    //
    // The whole deposit still goes when the deposit is what limited the transfer:
    // `pnl_transfer_implied_by_asset_amount` is the pnl the whole deposit buys, and
    // it rounds up, so charging the whole deposit for it never overcharges. The
    // only gap is the base-unit truncation of the two inverse conversions, and
    // taking the deposit to zero avoids stranding that dust in the position.
    //
    // The exact form can exceed the deposit by a unit or two through the same
    // truncation, so it is clamped.
    let asset_transfer = if pnl_transfer == pnl_transfer_implied_by_asset_amount {
        asset_amount
    } else {
        calculate_asset_transfer_for_liability_transfer_exact(
            asset_liquidation_multiplier,
            asset_decimals,
            asset_price,
            pnl_transfer,
            pnl_liquidation_multiplier,
            quote_decimals,
            quote_price,
        )?
        .min(asset_amount)
    };

    if asset_transfer == 0 || pnl_transfer == 0 {
        msg!(
            "asset_market_index {} perp_market_index {}",
            asset_market_index,
            perp_market_index
        );
        msg!("liquidator_max_pnl_transfer {} unsettled_pnl {} pnl_transfer_to_cover_margin_shortage {}", liquidator_max_pnl_transfer, unsettled_pnl, pnl_transfer_to_cover_margin_shortage);
        msg!(
            "pnl_transfer_implied_by_asset_amount {} pnl_transfer {} asset_transfer {}",
            pnl_transfer_implied_by_asset_amount,
            pnl_transfer,
            asset_transfer
        );
        return Err(ErrorCode::InvalidLiquidation);
    }

    validate_transfer_satisfies_limit_price(
        asset_transfer,
        pnl_transfer,
        asset_decimals,
        quote_decimals,
        limit_price,
    )?;

    {
        let mut asset_market = spot_market_map.get_ref_mut(&asset_market_index)?;

        update_spot_balances_and_cumulative_deposits(
            asset_transfer,
            &SpotBalanceType::Deposit,
            &mut asset_market,
            liquidator.get_spot_position_mut(asset_market_index)?,
            false,
            Some(asset_transfer),
        )?;

        liquidation_mode.decrease_spot_token_amount(
            user,
            asset_transfer,
            &mut asset_market,
            Some(asset_transfer),
        )?;
    }

    {
        let mut perp_market = perp_market_map.get_ref_mut(&perp_market_index)?;
        let liquidator_position = liquidator.force_get_perp_position_mut(perp_market_index)?;
        update_quote_asset_amount(liquidator_position, &mut perp_market, -pnl_transfer.cast()?)?;

        let user_position = user.get_perp_position_mut(perp_market_index)?;
        update_quote_asset_amount(user_position, &mut perp_market, pnl_transfer.cast()?)?;
    }

    let (margin_freed_from_liability, margin_calculation_after) = calculate_margin_freed(
        user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        liquidation_margin_buffer_ratio,
        margin_shortage,
        Some(liquidation_mode.as_ref()),
    )?;

    // Audit #25: `liquidate_perp_pnl_for_deposit` must never worsen the account's
    // (buffered) margin shortage. The weight check above rejects the market
    // parameters that make the transfer loss-making at every size, and
    // `calculate_margin_freed` saturates a negative improvement to 0, so this is
    // the backstop for anything the sizing math does not model.
    //
    // The check is exact. It holds no tolerance, because the seizure above pays
    // for every unit it takes. A tolerance here would let a liquidator size each
    // transfer to degrade the account by just under it and repeat the call until
    // the deposit is gone.
    //
    // Exempt Settlement (delisting): an expired market winds every position down
    // at the expiry price and this path clears the residual expired pnl into the
    // liquidator, which legitimately drives the account to bankruptcy. There is
    // no live risk left to protect, so the worsen-check must not block the
    // wind-down. The finding targets the ordinary permissionless liquidation of a
    // live market, which stays guarded.
    if !market_in_settlement {
        let new_margin_shortage = liquidation_mode.margin_shortage(&margin_calculation_after)?;
        validate!(
            new_margin_shortage <= margin_shortage,
            ErrorCode::LiquidationWorsensAccountHealth,
            "liquidate_perp_pnl_for_deposit would grow margin shortage ({} -> {}); refusing to worsen account health",
            margin_shortage,
            new_margin_shortage
        )?;
    }

    margin_freed = margin_freed.safe_add(margin_freed_from_liability)?;
    liquidation_mode.increment_free_margin(user, margin_freed_from_liability)?;

    if pnl_transfer >= pnl_transfer_to_cover_margin_shortage {
        liquidation_mode.exit_liquidation(user)?;
    } else if liquidation_mode.should_user_enter_bankruptcy(
        user,
        spot_market_map,
        perp_market_map,
    )? {
        liquidation_mode.enter_bankruptcy(user)?;
    }

    let liquidator_meets_initial_margin_requirement =
        meets_initial_margin_requirement(liquidator, perp_market_map, spot_market_map, oracle_map)?;

    validate!(
        liquidator_meets_initial_margin_requirement,
        ErrorCode::InsufficientCollateral,
        "Liquidator doesnt have enough collateral to take over borrow"
    )?;

    // The liquidation adds exposure to the liquidator like a risk-increasing
    // fill; the liquidator subaccount must clear its own buffered equity floor
    // to take it on.
    if let Some(liquidator_net_equity) =
        calculate_net_equity_for_floor(liquidator, perp_market_map, spot_market_map, oracle_map)?
    {
        liquidator_net_equity.validate_clears_buffered_floor(liquidator)?;
    }

    let market_oracle_price = {
        let market = perp_market_map.get_ref_mut(&perp_market_index)?;
        oracle_map.get_price_data(&market.oracle_id())?.price
    };

    let (margin_requirement, total_collateral, bit_flags) =
        liquidation_mode.get_event_fields(&margin_calculation)?;
    emit!(LiquidationRecord {
        ts: now,
        liquidation_id,
        liquidation_type: LiquidationType::LiquidatePerpPnlForDeposit,
        user: *user_key,
        liquidator: *liquidator_key,
        margin_requirement,
        total_collateral,
        bankrupt: liquidation_mode.is_user_bankrupt(user)?,
        margin_freed,
        liquidate_perp_pnl_for_deposit: LiquidatePerpPnlForDepositRecord {
            perp_market_index,
            market_oracle_price,
            pnl_transfer,
            asset_market_index,
            asset_price,
            asset_transfer,
        },
        bit_flags,
        ..LiquidationRecord::default()
    });

    Ok(())
}

/// Forfeit the estate's unfundable positive perp claims to their markets' insurance tranches, and
/// return the total (OtterSec #145).
///
/// A positive `quote_asset_amount` on a zero-base position is a claim on that market's PnL pool. When
/// the pool cannot pay it, the claim is unfunded but still owed. `is_cross_margin_bankrupt` lets such
/// a claim through, because a permanent veto strands a resolvable loss in another market forever.
///
/// The claim must not simply be ignored. The resolver draws the full per-market debt, the re-derive
/// then finds the account perp-solvent, and the latch clears. The user keeps a live claim that
/// insurance has already paid for.
///
/// So the creditor moves instead of the obligation vanishing. This zeroes the user's claim and adds
/// the same amount to the market's `pending_if_fee`.
///
/// The swap is equity-neutral. Zeroing the claim lowers `market.quote_asset_amount`, and therefore
/// `net_user_pnl`, which raises the market's excess. The `pending_if_fee` credit lowers the excess by
/// the same amount. No new state is needed, and no inter-market receivable: `pending_if_fee` is
/// already a claim on future PnL-pool inflows, which is what the user's claim was.
///
/// The insurance fund receives a claim, not cash. This does not reduce the draw for the bankruptcy in
/// progress. The fund is paid later, if the pool fills, through `pending_if_fee` ->
/// `sweep_market_fees` -> revenue pool -> `settle_revenue_to_insurance_fund`.
///
/// The net-insolvency gate in `is_cross_margin_bankrupt` bounds this. Admission needs a net perp
/// quote of 0 or less, so total claims never exceed total debt.
fn extinguish_unfundable_perp_claims(
    user: &mut User,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
) -> VelocityResult<u128> {
    let quote_spot_market = spot_market_map.get_quote_spot_market()?;
    let mut total_forfeited: u128 = 0;

    // One pass over the same list the handler declared writable. A position with base exposure or a
    // live order is not a settled claim and is excluded there, so this loop cannot write to a market
    // the handler did not declare.
    for market_index in perp_markets_with_forfeitable_claims(user) {
        let index = get_position_index(&user.perp_positions, market_index)?;
        let position = user.perp_positions[index];

        // Recompute fundability under a READ borrow. The pool can move between admission and
        // resolution, and only the part it cannot pay may be taken; a fundable part settles through
        // the ordinary pipeline. Taking the write borrow only once something is actually forfeited
        // keeps a no-op pass from touching write access it does not use.
        let unfundable = {
            let perp_market = perp_market_map.get_ref(&market_index)?;
            let pnl_pool_tokens = get_token_amount(
                perp_market.pnl_pool.balance(),
                &quote_spot_market,
                perp_market.pnl_pool.balance_type(),
            )?;

            position
                .quote_asset_amount
                .cast::<u128>()?
                .saturating_sub(pnl_pool_tokens)
        };

        if unfundable == 0 {
            continue;
        }

        let mut perp_market = perp_market_map.get_ref_mut(&market_index)?;

        update_quote_asset_amount(
            &mut user.perp_positions[index],
            &mut perp_market,
            -unfundable.cast::<i64>()?,
        )?;
        perp_market
            .fee_ledger
            .accrue_forfeited_claim_to_if(unfundable)?;

        msg!(
            "perp market {} bankruptcy: forfeited {} of unfundable claim to the insurance tranche",
            market_index,
            unfundable
        );

        total_forfeited = total_forfeited.safe_add(unfundable)?;
    }

    Ok(total_forfeited)
}

/// Set off the user's quote deposit against this perp market's bad debt, and return the amount
/// applied (OtterSec #130).
///
/// The bankruptcy latch records that nothing was left to seize when liquidation set it. Assets can
/// arrive after that: the revenue-share sweep is permissionless, and keeper filler rewards credit the
/// filler with no bankruptcy check. Once the latch is set, every route that could pay the debt is
/// closed. `settle_pnl` rejects a bankrupt user, `liquidate_spot` rejects a bankrupt user, and the
/// resolver reads only the liability row. The credit therefore paid nothing, insurance covered the
/// whole debt, and the credit became withdrawable when the resolver cleared the latch.
///
/// This performs the `settle_pnl` move that a bankrupt user cannot make. Tokens go from the quote
/// deposit to the market's `pnl_pool`, and the perp debt falls by the same amount. Those tokens land
/// where tranche 2 would have put insurance money, so every tranche below sees the net debt and the
/// draw falls one for one. The quote market is token-neutral, so the handler's vault-amount assertion
/// still holds.
///
/// `min(deposit, |debt|)` bounds this, so it never takes more than is owed. A guard at the credit's
/// source could not do the same: the sweep cannot compute what the account owes, because that spans
/// every perp market and every spot borrow, and non-quote borrows need oracles.
fn apply_quote_deposit_setoff_for_perp_bankruptcy(
    market_index: u16,
    user: &mut User,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    now: i64,
    funding_paused: bool,
) -> VelocityResult<u128> {
    let position_index = get_position_index(&user.perp_positions, market_index)?;

    // An isolated position is walled off from cross collateral. Its resolver never reads cross
    // deposits, and must not start now.
    if user.perp_positions[position_index].is_isolated() {
        return Ok(0);
    }

    let debt = user.perp_positions[position_index].quote_asset_amount;
    if debt >= 0 {
        return Ok(0);
    }

    let quote_spot_market = &mut spot_market_map.get_quote_spot_market_mut()?;
    let oracle_price_data = oracle_map.get_price_data(&quote_spot_market.oracle_id())?;
    update_spot_market_cumulative_interest(
        quote_spot_market,
        Some(oracle_price_data),
        now,
        funding_paused,
    )?;

    let quote_position = user.get_quote_spot_position();
    if quote_position.balance_type != SpotBalanceType::Deposit {
        return Ok(0);
    }

    let setoff = quote_position
        .get_token_amount(quote_spot_market)?
        .min(debt.unsigned_abs().cast()?);

    if setoff == 0 {
        return Ok(0);
    }

    let mut perp_market = perp_market_map.get_ref_mut(&market_index)?;

    transfer_spot_balances(
        setoff.cast()?,
        quote_spot_market,
        user.get_quote_spot_position_mut(),
        &mut perp_market.pnl_pool,
    )?;

    update_quote_asset_amount(
        &mut user.perp_positions[position_index],
        &mut perp_market,
        setoff.cast()?,
    )?;

    // Parity with `settle_pnl`, which stamps the same counter when it moves this value.
    update_settled_pnl(user, position_index, -setoff.cast::<i64>()?)?;

    msg!(
        "perp market {} bankruptcy: set off {} of quote deposit against bad debt",
        market_index,
        setoff
    );

    Ok(setoff)
}

pub fn resolve_perp_bankruptcy(
    market_index: u16,
    user: &mut User,
    user_key: &Pubkey,
    liquidator: &mut User,
    liquidator_key: &Pubkey,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    now: i64,
    insurance_fund_vault_balance: u64,
    funding_paused: bool,
) -> VelocityResult<u64> {
    let liquidation_mode = get_perp_liquidation_mode(user, market_index)?;

    if !liquidation_mode.is_user_bankrupt(user)?
        && liquidation_mode.should_user_enter_bankruptcy(user, spot_market_map, perp_market_map)?
    {
        liquidation_mode.enter_bankruptcy(user)?;
    }

    validate!(
        liquidation_mode.is_user_bankrupt(user)?,
        ErrorCode::UserNotBankrupt,
        "user not bankrupt",
    )?;

    validate!(
        !liquidator.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "liquidator bankrupt",
    )?;

    validate!(
        !liquidator.is_being_liquidated(),
        ErrorCode::UserIsBeingLiquidated,
        "liquidator being liquidated",
    )?;

    let market = perp_market_map.get_ref(&market_index)?;

    validate!(
        !market.is_operation_paused(PerpOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        market_index
    )?;

    drop(market);

    user.get_perp_position(market_index).inspect_err(|_e| {
        msg!(
            "User does not have a position for perp market {}",
            market_index
        );
    })?;

    // OtterSec #130: apply the estate's own quote deposit before drawing on anyone else's money.
    // Safe to run ahead of the stale-latch check below: it debits a deposit and credits the debt by
    // the same amount, which is exactly the `settle_pnl` the user is barred from making, so it leaves
    // the estate no worse off even when the account is handed back to ordinary liquidation.
    let setoff = apply_quote_deposit_setoff_for_perp_bankruptcy(
        market_index,
        user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        now,
        funding_paused,
    )?;

    // OtterSec #130 fallback, for what the setoff cannot reach: a credit in a NON-QUOTE deposit.
    // Netting that against a quote debt needs a cross-asset swap, not a balance transfer.
    //
    // If such an asset remains, the latch's premise is stale. Clear it and return without drawing.
    // Ordinary liquidation rejects a latched user, so it becomes legal again, seizes the asset, and
    // re-latches for the real residual.
    //
    // This tests only for realizable assets, not the full predicate. That one also vetoes on an open
    // order or base exposure, which the resolvers are reached with.
    //
    // Commit the un-latch instead of erroring. An error leaves the bit set and wedges both paths.
    // Nothing is drawn here, so this cannot reorder insurance spending against the #52 precedence.
    if has_realizable_spot_assets_for_setoff(user, spot_market_map)? {
        msg!(
            "stale cross-margin bankruptcy latch (assets present after setoff of {}); un-latching without drawing",
            setoff
        );
        liquidation_mode.exit_bankruptcy(user)?;
        return Ok(0);
    }

    // OtterSec #145: wind up the estate's unfundable claims, so the account cannot keep one after
    // insurance covers its debt.
    //
    // This MUST sit below the un-latch above. Forfeiting is irreversible, and the un-latch path hands
    // the account back to ordinary liquidation, which may cover the whole debt out of the seized
    // asset — leaving no bankruptcy, no insurance draw, and a confiscation that bought nothing. The
    // net-solvency bound this relies on is established at admission, and a stale latch means exactly
    // that the state has moved since, so it cannot be assumed here. `resolve_spot_bankruptcy` orders
    // these two the same way.
    //
    // It stays above the `loss` read: if this market's own claim was positive and unfundable, zeroing
    // it lands on the `loss == 0` path below rather than tripping the negative-pnl assertion.
    extinguish_unfundable_perp_claims(user, perp_market_map, spot_market_map)?;

    let loss = user
        .get_perp_position(market_index)?
        .quote_asset_amount
        .cast::<i128>()?;

    // The setoff can clear this market's debt while a liability elsewhere keeps the account bankrupt.
    // Nothing is left to resolve here. `has_pending_cross_margin_perp_bankruptcy` no longer reports
    // this market, so the spot resolver is unblocked (#52). Return instead of tripping the assertion
    // below.
    if loss == 0 {
        msg!(
            "perp market {} bad debt fully covered by setoff; nothing to resolve",
            market_index
        );
        return Ok(0);
    }

    validate!(
        loss < 0,
        ErrorCode::InvalidPerpPositionToLiquidate,
        "user must have negative pnl"
    )?;

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        MarginContext::standard(MarginRequirementType::Maintenance),
    )?;

    // spot market's insurance fund draw attempt here (before social loss)
    // subtract 1 from available insurance_fund_vault_balance so deposits in insurance vault always remains >= 1

    // Tranche 1: the market's own in-transit insurance fees (`pending_if_fee`)
    // are consumed BEFORE the shared IF vault is tapped. Counter-only: the
    // pending claim and the forgiven loss are both claims on future pnl-pool
    // inflows, so canceling one against the other needs no token movement —
    // the fee value that would have swept to the revenue pool stays in the
    // pnl pool backing the counterparties this spares from socialization.
    let pending_if_payment: u128 = {
        let mut perp_market = perp_market_map.get_ref_mut(&market_index)?;

        let pending_if_payment = loss
            .unsigned_abs()
            .min(perp_market.fee_ledger.pending_if_fee);

        if pending_if_payment > 0 {
            perp_market
                .fee_ledger
                .consume_pending_if(pending_if_payment)?;
            msg!("bankruptcy pending_if_fee tranche: {}", pending_if_payment);
        }

        pending_if_payment
    };

    let loss_after_pending = loss.safe_add(pending_if_payment.cast::<i128>()?)?;

    // Tranche 2: the shared insurance fund vault
    let if_payment = {
        let mut perp_market = perp_market_map.get_ref_mut(&market_index)?;
        let max_insurance_withdraw = perp_market
            .insurance_claim
            .quote_max_insurance
            .safe_sub(perp_market.insurance_claim.quote_settled_insurance)?
            .cast::<u128>()?;

        let if_payment = loss_after_pending
            .unsigned_abs()
            .min(insurance_fund_vault_balance.saturating_sub(1).cast()?)
            .min(max_insurance_withdraw);

        perp_market.insurance_claim.quote_settled_insurance = perp_market
            .insurance_claim
            .quote_settled_insurance
            .safe_add(if_payment.cast()?)?;

        // move if payment to pnl pool
        let spot_market = &mut spot_market_map.get_ref_mut(&QUOTE_SPOT_MARKET_INDEX)?;
        let oracle_price_data = oracle_map.get_price_data(&spot_market.oracle_id())?;
        update_spot_market_cumulative_interest(
            spot_market,
            Some(oracle_price_data),
            now,
            funding_paused,
        )?;

        update_spot_balances(
            if_payment,
            &SpotBalanceType::Deposit,
            spot_market,
            &mut perp_market.pnl_pool,
            false,
        )?;

        if_payment
    };

    let losses_remaining: i128 = loss_after_pending.safe_add(if_payment.cast::<i128>()?)?;
    validate!(
        losses_remaining <= 0,
        ErrorCode::InvalidPerpPositionToLiquidate,
        "losses_remaining must be non-positive"
    )?;

    // Tranche 3: claw back the AMM's fee provision — the backstop of LAST
    // resort, capped at `amm_protocol_fees_received` (cumulative provision
    // granted via the amm_fee_numerator cut, net of prior clawbacks). The
    // AMM's own spread/trading capital beyond the provision is never tapped
    // (nor is the external LP pool). Two phases:
    //   3a. the not-yet-tokenized provision (`pending_amm_provision`) —
    //       counter-only, like tranche 1: its token backing still sits in the
    //       pnl pool, where it now backs the spared counterparties instead.
    //   3b. the tokenized remainder — real tokens move amm.fee_pool ->
    //       pnl_pool, capped by what the fee pool actually holds.
    // Both phases debit the AMM's books (`record_amm_pnl`): the provision was
    // booked into `total_fee_minus_distributions` at fill, and the dent to
    // `net_revenue_since_last_funding` lets the drawdown breaker see the hit.
    let amm_tranche_payment: i128 = if losses_remaining < 0 {
        let mut perp_market = perp_market_map.get_ref_mut(&market_index)?;
        // reborrow through the RefMut so disjoint field borrows split
        let perp_market = &mut *perp_market;
        let spot_market = &mut spot_market_map.get_ref_mut(&QUOTE_SPOT_MARKET_INDEX)?;

        let clawback_budget: u128 = losses_remaining
            .unsigned_abs()
            .min(perp_market.fee_ledger.amm_protocol_fees_received);

        // 3a. untokenized provision: counter-only
        let untokenized = clawback_budget.min(perp_market.fee_ledger.pending_amm_provision);
        if untokenized > 0 {
            perp_market
                .fee_ledger
                .consume_pending_amm_provision(untokenized)?;
            perp_market.fee_ledger.consume_amm_backstop(untokenized)?;
            <crate::vlp::amm::AMM as crate::vlp::amm::quoter::AmmContract>::record_amm_pnl(
                &mut perp_market.amm,
                -untokenized.cast::<i128>()?,
            )?;
            msg!(
                "bankruptcy amm provision tranche (untokenized): {}",
                untokenized
            );
        }

        // 3b. tokenized provision: fee-pool tokens move to the pnl pool
        let fee_pool_tokens: u128 = get_fee_pool_tokens(&perp_market.amm, spot_market)?
            .max(0)
            .cast()?;
        let tokenized = clawback_budget.safe_sub(untokenized)?.min(fee_pool_tokens);
        if tokenized > 0 {
            transfer_spot_balances(
                tokenized.cast()?,
                spot_market,
                &mut perp_market.amm.fee_pool,
                &mut perp_market.pnl_pool,
            )?;
            perp_market.fee_ledger.consume_amm_backstop(tokenized)?;
            <crate::vlp::amm::AMM as crate::vlp::amm::quoter::AmmContract>::record_amm_pnl(
                &mut perp_market.amm,
                -tokenized.cast::<i128>()?,
            )?;
            msg!(
                "bankruptcy amm provision tranche (tokenized): {}",
                tokenized
            );
        }

        untokenized.safe_add(tokenized)?.cast()?
    } else {
        0
    };

    let loss_to_socialize = losses_remaining.safe_add(amm_tranche_payment)?;
    validate!(
        loss_to_socialize <= 0,
        ErrorCode::InvalidPerpPositionToLiquidate,
        "loss_to_socialize must be non-positive"
    )?;

    // Only socialized loss needs a funding-rate delta. With full coverage
    // (loss_to_socialize == 0) skip the helper: it requires nonzero open
    // interest, so a fully-covered bankruptcy in a market with zero OI would
    // otherwise revert the whole atomic resolution and leave the account
    // bankrupt despite sufficient coverage.
    let cumulative_funding_rate_delta = if loss_to_socialize < 0 {
        calculate_funding_rate_deltas_to_resolve_bankruptcy(
            loss_to_socialize,
            perp_market_map.get_ref(&market_index)?.deref(),
        )?
    } else {
        0
    };

    // socialize loss
    if loss_to_socialize < 0 {
        let mut market = perp_market_map.get_ref_mut(&market_index)?;

        market.total_social_loss = market
            .total_social_loss
            .safe_add(loss_to_socialize.unsigned_abs())?;

        // Fully settle the AMM's OWN funding through the current (pre-
        // socialization) cum rates against its actual net position first —
        // exactly the payment the `FundingUpdated` quoter handler applies — so
        // no genuine accrued AMM funding is dropped when we advance the AMM
        // stamp past the socialization bump below. `calculate_amm_funding_payment`
        // pays the AMM `(cumulative_funding_rate − amm.last_cumulative_funding_rate)
        // × −net_position` per leg; here the deltas are only what has genuinely
        // accrued (the socialization bump has NOT been applied yet). Today the
        // market cum rates and the AMM stamp only ever advance together in
        // `update_funding_rate`, so on entry this payment is 0 — but applying it
        // explicitly (rather than assuming the invariant) keeps the AMM's books
        // correct even if another writer of the cum rates is ever added.
        let amm_funding_payment = crate::math::funding::calculate_amm_funding_payment(
            market.base_asset_amount_long,
            market.base_asset_amount_short,
            market.cumulative_funding_rate_long,
            market.cumulative_funding_rate_short,
            market.amm.last_cumulative_funding_rate_long,
            market.amm.last_cumulative_funding_rate_short,
        )?;
        <crate::vlp::amm::AMM as crate::vlp::amm::quoter::AmmContract>::record_amm_pnl(
            &mut market.amm,
            amm_funding_payment,
        )?;

        // Socialize the loss across surviving open interest via an asymmetric
        // cum-rate bump (longs and shorts both owe funding covering the loss).
        market.cumulative_funding_rate_long = market
            .cumulative_funding_rate_long
            .safe_add(cumulative_funding_rate_delta)?;

        market.cumulative_funding_rate_short = market
            .cumulative_funding_rate_short
            .safe_sub(cumulative_funding_rate_delta)?;

        // The cum-rate bump makes surviving positions owe `loss_to_socialize`
        // in aggregate funding. Record it the same way funding accrual does
        // (net_unsettled_funding_pnl -= protocol funding revenue): without this
        // the obligation is absent from net_unsettled until users settle, and
        // calculate_net_user_pnl overstates aggregate user PnL by the socialized
        // loss in the meantime.
        market.net_unsettled_funding_pnl = market
            .net_unsettled_funding_pnl
            .safe_add(loss_to_socialize.cast()?)?;

        // Now advance the AMM stamp PAST the socialization bump. The AMM's
        // genuine funding was just settled above, so this only excludes the
        // socialization delta from the AMM's next funding payment — the
        // socialized loss is borne by surviving USER open interest, not the AMM.
        // Leaving the stamp behind would instead credit the AMM phantom funding
        // on the bump (both legs resolve positive for a balanced book),
        // manufacturing `total_fee_minus_distributions` (≈ D·G1/G0, able to
        // exceed the socialized loss D as gross OI grows) that is then payable to
        // survivors or spendable as curve budget (OtterSec #89).
        market.amm.last_cumulative_funding_rate_long =
            market.cumulative_funding_rate_long.cast::<i64>()?;
        market.amm.last_cumulative_funding_rate_short =
            market.cumulative_funding_rate_short.cast::<i64>()?;
    }

    // clear bad debt
    {
        let mut market = perp_market_map.get_ref_mut(&market_index)?;
        let position_index = get_position_index(&user.perp_positions, market_index)?;
        let quote_asset_amount = user.perp_positions[position_index].quote_asset_amount;
        update_quote_asset_amount(
            &mut user.perp_positions[position_index],
            &mut market,
            -quote_asset_amount,
        )?;

        user.increment_total_socialized_loss(quote_asset_amount.unsigned_abs())?;
    }

    // True if a bankrupting liability remains; clears status otherwise.
    let still_bankrupt =
        liquidation_mode.should_user_enter_bankruptcy(user, spot_market_map, perp_market_map)?;
    if !still_bankrupt {
        liquidation_mode.exit_bankruptcy(user)?;
    }

    let liquidation_id = user.next_liquidation_id.safe_sub(1)?;

    let (margin_requirement, total_collateral, bit_flags) =
        liquidation_mode.get_event_fields(&margin_calculation)?;
    emit!(LiquidationRecord {
        ts: now,
        liquidation_id,
        liquidation_type: LiquidationType::PerpBankruptcy,
        user: *user_key,
        liquidator: *liquidator_key,
        margin_requirement,
        total_collateral,
        bankrupt: still_bankrupt,
        perp_bankruptcy: PerpBankruptcyRecord {
            market_index,
            if_payment,
            pnl: loss,
            clawback_user: None,
            clawback_user_payment: None,
            cumulative_funding_rate_delta,
        },
        bit_flags,
        ..LiquidationRecord::default()
    });

    if_payment.cast()
}

pub fn resolve_spot_bankruptcy(
    market_index: u16,
    user: &mut User,
    user_key: &Pubkey,
    liquidator: &mut User,
    liquidator_key: &Pubkey,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    now: i64,
    insurance_fund_vault_balance: u64,
    funding_paused: bool,
) -> VelocityResult<u64> {
    if !user.is_cross_margin_bankrupt()
        && is_cross_margin_bankrupt(user, spot_market_map, perp_market_map)?
    {
        user.enter_cross_margin_bankruptcy();
    }

    validate!(
        user.is_cross_margin_bankrupt(),
        ErrorCode::UserNotBankrupt,
        "user not bankrupt",
    )?;

    // OtterSec #130: assets can arrive after the latch is set, through the permissionless
    // revenue-share sweep or keeper filler rewards. Every route that could apply them to the debt is
    // closed to a bankrupt user, and this resolver reads only the liability row. A stale latch would
    // socialize the whole borrow while the new asset became withdrawable.
    //
    // If a realizable deposit is present, clear the latch and return without drawing. Ordinary
    // liquidation then seizes it and re-latches for the real residual. Commit the un-latch instead of
    // erroring, which would wedge both paths. This tests only for assets, not the full predicate.
    // It sits above the #52 check because it draws nothing.
    if has_realizable_spot_assets_for_setoff(user, spot_market_map)? {
        msg!("stale cross-margin bankruptcy latch (assets present); un-latching without drawing");
        user.exit_cross_margin_bankruptcy();
        return Ok(0);
    }

    // OtterSec #145: an account can reach this resolver holding unfundable perp claims, because its
    // liability is a spot borrow and the #52 precedence below does not divert it. Wind them up here
    // too, or insurance covers the borrow and the claim stays live to collect later.
    extinguish_unfundable_perp_claims(user, perp_market_map, spot_market_map)?;

    // Audit #52: enforce a deterministic perp-before-spot bankruptcy precedence.
    // resolve_perp_bankruptcy and resolve_spot_bankruptcy both draw from the
    // shared (quote) insurance fund vault, so a public caller could otherwise
    // pick which resolver spends it first and shift socialized loss between perp
    // and spot stakeholders. The keeper bots already resolve every perp
    // bankruptcy before any spot bankruptcy, so we require the same order
    // on-chain: any pending cross-margin perp bankruptcy must be cleared first.
    validate!(
        !has_pending_cross_margin_perp_bankruptcy(user),
        ErrorCode::PerpBankruptcyMustPrecedeSpot,
        "resolve pending perp bankruptcies before spot bankruptcies",
    )?;

    validate!(
        !liquidator.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "liquidator bankrupt",
    )?;

    validate!(
        !liquidator.is_being_liquidated(),
        ErrorCode::UserIsBeingLiquidated,
        "liquidator being liquidated",
    )?;

    let market = spot_market_map.get_ref(&market_index)?;

    validate!(
        !market.is_operation_paused(SpotOperation::Liquidation),
        ErrorCode::InvalidLiquidation,
        "Liquidation operation is paused for market {}",
        market_index
    )?;

    drop(market);

    // validate user and liquidator have spot position balances
    user.get_spot_position(market_index).map_err(|_| {
        msg!(
            "User does not have a spot balance for market {}",
            market_index
        );
        ErrorCode::CouldNotFindSpotPosition
    })?;

    let MarginCalculation {
        margin_requirement,
        total_collateral,
        ..
    } = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        MarginContext::standard(MarginRequirementType::Maintenance),
    )?;

    // Accrue the borrow market's cumulative interest to `now` before reading
    // the borrow amount. `SpotPosition::get_token_amount` scales the position
    // by `cumulative_borrow_interest`, so a stale (un-accrued) index would clear
    // the debt at less than its current value — under-drawing the revenue-pool
    // and IF tranches, under-socializing the residual, and forgiving the
    // interest accrued since the last touch. Pass `None` (interest + token/util
    // TWAPs only, no oracle price data): the fix only needs the interest index
    // refreshed, and interest accrual does not depend on the oracle. Feeding the
    // oracle price here would also stamp the market's `historical_oracle_data`
    // (conf/delay/TWAPs) as a side effect of a bankruptcy resolution — state
    // this path never reads — so it is deliberately omitted, matching the
    // sibling `resolve_perp_pnl_deficit` refresh.
    {
        let spot_market = &mut spot_market_map.get_ref_mut(&market_index)?;
        update_spot_market_cumulative_interest(spot_market, None, now, funding_paused)?;
    }

    let borrow_amount = {
        let spot_position = user.get_spot_position(market_index)?;
        validate!(
            spot_position.balance_type == SpotBalanceType::Borrow,
            ErrorCode::UserHasInvalidBorrow
        )?;

        validate!(
            spot_position.scaled_balance > 0,
            ErrorCode::UserHasInvalidBorrow
        )?;

        spot_position.get_token_amount(spot_market_map.get_ref(&market_index)?.deref())?
    };

    // Tranche 1: the market's own unsettled IF revenue (`revenue_pool`) is
    // consumed BEFORE the staker-owned IF vault and any social loss. Counter-
    // only: the pool's tokens already sit in the spot vault, so canceling the
    // pool's deposit claim against the forgiven borrow needs no token movement
    // — value that would have settled to the insurance vault covers the bad
    // debt directly instead of depositors. Unlike the periodic revenue settle,
    // this draw is not timer-gated or staker-APR-capped: in a bankruptcy the
    // pool is first-loss capital.
    let revenue_pool_payment = {
        let mut spot_market = spot_market_map.get_ref_mut(&market_index)?;
        let revenue_pool_token_amount = get_token_amount(
            spot_market.revenue_pool.scaled_balance,
            spot_market.deref(),
            &SpotBalanceType::Deposit,
        )?;
        let payment = borrow_amount.min(revenue_pool_token_amount);
        if payment > 0 {
            // counter-only draw, no tokens leave the vault
            update_revenue_pool_balances(
                payment,
                &SpotBalanceType::Borrow,
                &mut spot_market,
                false,
            )?;
            msg!("bankruptcy revenue pool tranche: {}", payment);
        }
        payment
    };

    // Tranche 2: the staker-owned insurance fund vault.
    // subtract 1 so insurance_fund_vault_balance always stays >= 1
    let if_payment = borrow_amount
        .safe_sub(revenue_pool_payment)?
        .min(insurance_fund_vault_balance.saturating_sub(1).cast()?);

    let loss_to_socialize = borrow_amount
        .safe_sub(revenue_pool_payment)?
        .safe_sub(if_payment)?;

    let cumulative_deposit_interest_delta =
        calculate_cumulative_deposit_interest_delta_to_resolve_bankruptcy(
            loss_to_socialize,
            spot_market_map.get_ref(&market_index)?.deref(),
        )?;

    {
        let mut spot_market = spot_market_map.get_ref_mut(&market_index)?;
        let oracle_price_data = &oracle_map.get_price_data(&spot_market.oracle_id())?;
        // The user records the gross bad debt; the spot-market counters record
        // only the loss actually borne by depositors, i.e. after the
        // revenue-pool and IF payments.
        let gross_quote_loss = get_token_value(
            -borrow_amount.cast()?,
            spot_market.decimals,
            oracle_price_data.price,
        )?;
        let socialized_quote_loss = get_token_value(
            -loss_to_socialize.cast()?,
            spot_market.decimals,
            oracle_price_data.price,
        )?;
        user.increment_total_socialized_loss(gross_quote_loss.unsigned_abs().cast()?)?;

        let spot_position = user.get_spot_position_mut(market_index)?;
        update_spot_balances_and_cumulative_deposits(
            borrow_amount,
            &SpotBalanceType::Deposit,
            &mut spot_market,
            spot_position,
            false,
            None,
        )?;

        spot_market.cumulative_deposit_interest = spot_market
            .cumulative_deposit_interest
            .safe_sub(cumulative_deposit_interest_delta)?;

        spot_market.total_social_loss = spot_market
            .total_social_loss
            .safe_add(loss_to_socialize.cast()?)?;

        spot_market.total_quote_social_loss = spot_market
            .total_quote_social_loss
            .safe_add(socialized_quote_loss.unsigned_abs().cast()?)?;
    }

    // True if a bankrupting liability remains; clears status otherwise.
    let still_bankrupt = is_cross_margin_bankrupt(user, spot_market_map, perp_market_map)?;
    if !still_bankrupt {
        user.exit_cross_margin_bankruptcy();
    }

    let liquidation_id = user.next_liquidation_id.safe_sub(1)?;

    emit!(LiquidationRecord {
        ts: now,
        liquidation_id,
        liquidation_type: LiquidationType::SpotBankruptcy,
        user: *user_key,
        liquidator: *liquidator_key,
        margin_requirement,
        total_collateral,
        bankrupt: still_bankrupt,
        spot_bankruptcy: SpotBankruptcyRecord {
            market_index,
            borrow_amount,
            if_payment,
            cumulative_deposit_interest_delta,
        },
        ..LiquidationRecord::default()
    });

    if_payment.cast()
}

pub fn calculate_margin_freed(
    user: &User,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    liquidation_margin_buffer_ratio: u32,
    initial_margin_shortage: u128,
    liquidation_mode: Option<&dyn LiquidatePerpMode>,
) -> VelocityResult<(u64, MarginCalculation)> {
    let margin_calculation_after =
        calculate_margin_requirement_and_total_collateral_and_liability_info(
            user,
            perp_market_map,
            spot_market_map,
            oracle_map,
            MarginContext::liquidation(liquidation_margin_buffer_ratio),
        )?;

    let new_margin_shortage = if let Some(liquidation_mode) = liquidation_mode {
        liquidation_mode.margin_shortage(&margin_calculation_after)?
    } else {
        margin_calculation_after.cross_margin_margin_shortage()?
    };

    let margin_freed = initial_margin_shortage
        .saturating_sub(new_margin_shortage)
        .cast::<u64>()?;

    Ok((margin_freed, margin_calculation_after))
}

pub fn set_user_status_to_being_liquidated(
    user: &mut User,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    slot: u64,
    state: &State,
) -> VelocityResult {
    validate!(
        !user.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "user bankrupt",
    )?;

    validate!(
        !user.is_being_liquidated(),
        ErrorCode::UserIsBeingLiquidated,
        "user is already being liquidated",
    )?;

    let liquidation_margin_buffer_ratio = state.liquidation_margin_buffer_ratio;
    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        MarginContext::liquidation(liquidation_margin_buffer_ratio),
    )?;

    let mut updated_liquidation_status = false;
    if !user.is_cross_margin_being_liquidated()
        && !margin_calculation.meets_cross_margin_requirement()
    {
        updated_liquidation_status = true;
        user.enter_cross_margin_liquidation(slot)?;
    }

    for (market_index, isolated_margin_calculation) in
        margin_calculation.isolated_margin_calculations.iter()
    {
        if !user.is_isolated_margin_being_liquidated(*market_index)?
            && !isolated_margin_calculation.meets_margin_requirement()
        {
            updated_liquidation_status = true;
            user.enter_isolated_margin_liquidation(*market_index, slot)?;
        }
    }

    if !updated_liquidation_status {
        return Err(ErrorCode::SufficientCollateral);
    }

    Ok(())
}
