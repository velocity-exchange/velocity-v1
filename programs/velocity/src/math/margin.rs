use {
    super::spot_balance::get_token_amount,
    crate::{
        error::{ErrorCode, VelocityResult},
        instructions::optional_accounts::AccountMaps,
        math::{
            casting::Cast,
            constants::{
                MARGIN_PRECISION_U128, MAX_POSITIVE_UPNL_FOR_INITIAL_MARGIN,
                MAX_SPOT_INTEREST_STALENESS_FOR_MARGIN,
                MAX_SPOT_INTEREST_UNDERSTATEMENT_FOR_MARGIN, ONE_YEAR, PRICE_PRECISION,
                PRICE_PRECISION_I128, PRICE_PRECISION_I64, SPOT_IMF_PRECISION_U128,
                SPOT_WEIGHT_PRECISION, SPOT_WEIGHT_PRECISION_U128,
            },
            funding::calculate_funding_payment,
            oracle::{is_oracle_valid_for_action, LogMode, VelocityAction},
            position::{
                calculate_base_asset_value_and_pnl_with_expiry_price,
                calculate_base_asset_value_and_pnl_with_oracle_price,
            },
            safe_math::SafeMath,
            spot_balance::{
                calculate_accumulated_interest, get_interest_token_amount, get_strict_token_value,
                get_token_value, InterestAccumulated,
            },
        },
        msg,
        state::{
            margin_calculation::{
                MarginCalculation, MarginContext, MarginTypeConfig, MarketIdentifier,
            },
            market_status::MarketStatus,
            oracle::{OraclePriceData, StrictOraclePrice},
            perp_market::{ContractTier, PerpMarket},
            perp_market_map::PerpMarketMap,
            spot_market::{AssetTier, SpotBalanceType, SpotMarket},
            spot_market_map::SpotMarketMap,
            user::{MarketType, OrderFillSimulation, PerpPosition, User},
        },
        validate, validation,
    },
    num_integer::Roots,
    std::cmp::{max, min, Ordering},
};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, PartialEq, Debug, Eq)]
pub enum MarginRequirementType {
    Initial,
    Fill,
    Maintenance,
}

pub fn calculate_size_premium_liability_weight(
    size: u128, // AMM_RESERVE_PRECISION
    imf_factor: u32,
    liability_weight: u32,
    precision: u128,
    is_bounded: bool,
) -> VelocityResult<u32> {
    if imf_factor == 0 {
        return Ok(liability_weight);
    }

    let size_sqrt = ((size * 10) + 1).nth_root(2); //1e9 -> 1e10 -> 1e5

    let imf_factor_u128 = imf_factor.cast::<u128>()?;
    let liability_weight_u128 = liability_weight.cast::<u128>()?;
    let liability_weight_numerator =
        liability_weight_u128.safe_sub(liability_weight_u128.safe_div(5)?)?;

    // increases
    let size_premium_liability_weight = liability_weight_numerator
        .safe_add(
            size_sqrt // 1e5
                .safe_mul(imf_factor_u128)?
                .safe_div(100_000 * SPOT_IMF_PRECISION_U128 / precision)?, // 1e5 * 1e2
        )?
        .cast::<u32>()?;

    if is_bounded {
        let max_liability_weight = max(liability_weight, size_premium_liability_weight);
        return Ok(max_liability_weight);
    }

    Ok(size_premium_liability_weight)
}

pub fn calculate_size_discount_asset_weight(
    size: u128, // AMM_RESERVE_PRECISION
    imf_factor: u32,
    asset_weight: u32,
) -> VelocityResult<u32> {
    if imf_factor == 0 {
        return Ok(asset_weight);
    }

    let size_sqrt = ((size * 10) + 1).nth_root(2); //1e9 -> 1e10 -> 1e5
    let imf_numerator = SPOT_IMF_PRECISION_U128 + SPOT_IMF_PRECISION_U128 / 10;

    let size_discount_asset_weight = imf_numerator
        .safe_mul(SPOT_WEIGHT_PRECISION_U128)?
        .safe_div(
            SPOT_IMF_PRECISION_U128
                .safe_add(size_sqrt.safe_mul(imf_factor.cast()?)?.safe_div(100_000)?)?,
        )?
        .cast::<u32>()?;

    let min_asset_weight = min(asset_weight, size_discount_asset_weight);

    Ok(min_asset_weight)
}

pub fn calculate_perp_position_value_and_pnl(
    market_position: &PerpPosition,
    market: &PerpMarket,
    oracle_price_data: &OraclePriceData,
    strict_quote_price: &StrictOraclePrice,
    margin_requirement_type: MarginRequirementType,
    user_custom_margin_ratio: u32,
) -> VelocityResult<(u128, i128, u128, u128)> {
    let valuation_price = if market.status == MarketStatus::Settlement {
        market.expiry_price
    } else {
        oracle_price_data.price
    };

    // the funding must be calculated before calculated the unrealized pnl w simulated lp position
    let unrealized_funding = calculate_funding_payment(
        if market_position.base_asset_amount > 0 {
            market.cumulative_funding_rate_long
        } else {
            market.cumulative_funding_rate_short
        },
        market_position,
    )?;

    // A committed `expiry_price` may be negative, and the live-oracle helper clamps a
    // non-positive price to zero. Settlement uses the expiry-price variant so margin
    // sees the same signed loss that `settle_expired_position` books (OtterSec #133).
    let (base_asset_value, unrealized_pnl) = if market.status == MarketStatus::Settlement {
        calculate_base_asset_value_and_pnl_with_expiry_price(market_position, valuation_price)?
    } else {
        calculate_base_asset_value_and_pnl_with_oracle_price(market_position, valuation_price)?
    };

    let total_unrealized_pnl = unrealized_pnl.safe_add(unrealized_funding.cast()?)?;

    let (worst_case_base_asset_amount, worse_case_liability_value) =
        market_position.worst_case_liability_value(oracle_price_data.price)?;

    // for calculating the perps value, since it's a liability, use the large of twap and quote oracle price
    let worse_case_liability_value = worse_case_liability_value
        .safe_mul(strict_quote_price.max().cast()?)?
        .safe_div(PRICE_PRECISION)?;

    let mut margin_requirement = if market.status == MarketStatus::Settlement {
        0
    } else {
        let margin_ratio = user_custom_margin_ratio.max(market.get_margin_ratio(
            worst_case_base_asset_amount.unsigned_abs(),
            margin_requirement_type,
        )?);

        worse_case_liability_value
            .safe_mul(margin_ratio.cast()?)?
            .safe_div(MARGIN_PRECISION_U128)?
    };

    // add small margin requirement for every open order
    margin_requirement =
        margin_requirement.safe_add(market_position.margin_requirement_for_open_orders()?)?;

    let unrealized_asset_weight =
        market.get_unrealized_asset_weight(total_unrealized_pnl, margin_requirement_type)?;

    let quote_price = if total_unrealized_pnl > 0 {
        strict_quote_price.min()
    } else if total_unrealized_pnl < 0 {
        strict_quote_price.max()
    } else {
        strict_quote_price.current
    };

    let mut weighted_unrealized_pnl = total_unrealized_pnl;

    if unrealized_asset_weight != SPOT_WEIGHT_PRECISION {
        weighted_unrealized_pnl = weighted_unrealized_pnl
            .safe_mul(unrealized_asset_weight.cast()?)?
            .safe_div(SPOT_WEIGHT_PRECISION.cast()?)?;
    }
    if quote_price != PRICE_PRECISION_I64 {
        weighted_unrealized_pnl = weighted_unrealized_pnl
            .safe_mul(quote_price.cast()?)?
            .safe_div(PRICE_PRECISION_I128)?;
    }

    if margin_requirement_type == MarginRequirementType::Initial {
        // safety guard for dangerously configured perp market
        weighted_unrealized_pnl = weighted_unrealized_pnl.min(MAX_POSITIVE_UPNL_FOR_INITIAL_MARGIN);
    }

    Ok((
        margin_requirement,
        weighted_unrealized_pnl,
        worse_case_liability_value,
        base_asset_value,
    ))
}

pub fn calculate_user_safest_position_tiers(
    user: &User,
    perp_market_map: &PerpMarketMap,
    spot_market_map: &SpotMarketMap,
) -> VelocityResult<(AssetTier, ContractTier)> {
    let mut safest_tier_spot_liablity: AssetTier = AssetTier::default();
    let mut safest_tier_perp_liablity: ContractTier = ContractTier::default();

    for spot_position in user.spot_positions.iter() {
        if spot_position.is_available() || spot_position.balance_type == SpotBalanceType::Deposit {
            continue;
        }
        let spot_market = spot_market_map.get_ref(&spot_position.market_index)?;
        safest_tier_spot_liablity = min(safest_tier_spot_liablity, spot_market.asset_tier);
    }

    for market_position in user.perp_positions.iter() {
        if market_position.is_available() {
            continue;
        }
        // a zero-base position with positive unsettled pnl is a claim on the
        // market's pnl pool, not a liability
        if !market_position.is_open_position()
            && !market_position.has_open_order()
            && market_position.isolated_position_scaled_balance == 0
            && market_position.quote_asset_amount > 0
        {
            continue;
        }
        let market = &perp_market_map.get_ref(&market_position.market_index)?;
        safest_tier_perp_liablity = min(safest_tier_perp_liablity, market.contract_tier);
    }

    Ok((safest_tier_spot_liablity, safest_tier_perp_liablity))
}

/// Reject valuing a user's spot **borrows** for margin when the market's interest
/// accrual is too stale (OtterSec #135 / #148).
///
/// Margin values a scaled borrow through the market's stored
/// `cumulative_borrow_interest`. Interest accrued since `last_interest_ts` is not in
/// that index, so the debt is understated by the un-booked amount. Nothing on these
/// paths refreshes the market. `handle_withdraw` cranks only the market being
/// withdrawn, the perp-fill handler cranks none, and the user's other borrow markets
/// arrive read-only (OtterSec #135 / #148). A borrower can therefore release tokens, or take an adverse
/// in-band maker fill, against debt the check never fully saw. That leaves bad debt
/// once the market is finally cranked.
///
/// Only borrow positions are gated. A stale deposit index understates collateral,
/// which errs in the protocol's favour, so there is nothing to protect against there.
/// Gating deposits would strand withdrawals for no gain.
///
/// The perp side has the same freshness precondition in `amm.is_fresh_at`. Recovery
/// needs no privileges. `update_spot_market_cumulative_interest` is permissionless and
/// may be bundled into the same transaction.
///
/// A market whose interval cannot be booked yet is exempt. The clock is the cheap
/// test, not the property that matters. `update_spot_market_cumulative_interest`
/// defers an interval whose split or configured carveout rounds below one token, and
/// it leaves `last_interest_ts` where it is while it does. On a dust-sized market that
/// deferral can outlast the staleness bound, and no amount of cranking moves the
/// clock. This function measures the omission itself, which keeps such a market
/// fillable. When the un-booked index applied to this borrow converts to less than one
/// token, the debt is understated by less than the smallest unit the account can be
/// charged.
pub fn validate_spot_borrow_interest_fresh_for_margin(
    user: &User,
    spot_market_map: &SpotMarketMap,
    now: i64,
) -> VelocityResult {
    for spot_position in &user.spot_positions {
        if spot_position.is_available()
            || spot_position.balance_type != SpotBalanceType::Borrow
            || spot_position.scaled_balance == 0
        {
            continue;
        }

        let spot_market = spot_market_map.get_ref(&spot_position.market_index)?;
        let staleness = now.safe_sub(spot_market.last_interest_ts.cast()?)?;
        let max_staleness = max_spot_interest_staleness_for_margin(&spot_market)?;

        if staleness <= max_staleness {
            continue;
        }

        let InterestAccumulated {
            borrow_interest, ..
        } = calculate_accumulated_interest(&spot_market, now)?;

        let unbooked_debt = get_interest_token_amount(
            spot_position.scaled_balance.cast()?,
            &spot_market,
            borrow_interest,
        )?;

        validate!(
            unbooked_debt == 0,
            ErrorCode::SpotMarketInterestStaleForMargin,
            "spot market {} interest is {}s stale (max {}s) and hides {} of this borrow — crank \
             update_spot_market_cumulative_interest before valuing it",
            spot_position.market_index,
            staleness,
            max_staleness,
            unbooked_debt
        )?;
    }

    Ok(())
}

/// The time window `validate_spot_borrow_interest_fresh_for_margin` allows one
/// market, derived from what that market may charge.
///
/// The un-booked share of a borrow is `borrow_rate x elapsed / year`. Holding that
/// share under `MAX_SPOT_INTEREST_UNDERSTATEMENT_FOR_MARGIN` means
/// `elapsed <= share x year / borrow_rate`. A fixed window would instead let the
/// hidden share scale with the rate, and the rate is configuration.
/// `validate_borrow_rate` bounds `max_borrow_rate` only from below, so a market may
/// carry a rate high enough to hide a material share of the debt within any fixed
/// window.
///
/// The divisor is the ceiling the market's own curve cannot exceed, not its current
/// rate. The window then costs one multiply and one divide instead of a utilization
/// and rate computation on every borrow of every margin check. `calculate_borrow_rate`
/// interpolates up to `max_borrow_rate` and then raises the result to
/// `min_borrow_rate`, so the larger of the two bounds it.
pub fn max_spot_interest_staleness_for_margin(spot_market: &SpotMarket) -> VelocityResult<i64> {
    let rate_ceiling = spot_market
        .max_borrow_rate
        .max(spot_market.get_min_borrow_rate()?)
        .cast::<u128>()?;

    if rate_ceiling == 0 {
        return Ok(MAX_SPOT_INTEREST_STALENESS_FOR_MARGIN);
    }

    let window = MAX_SPOT_INTEREST_UNDERSTATEMENT_FOR_MARGIN
        .safe_mul(ONE_YEAR)?
        .safe_div(rate_ceiling)?
        .cast::<i64>()?;

    Ok(window.min(MAX_SPOT_INTEREST_STALENESS_FOR_MARGIN))
}

pub fn calculate_margin_requirement_and_total_collateral_and_liability_info(
    user: &User,
    maps: &mut AccountMaps,
    context: MarginContext,
) -> VelocityResult<MarginCalculation> {
    let mut calculation = MarginCalculation::new(context);
    let cross_margin_requirement_type = context
        .margin_type_config
        .get_cross_margin_requirement_type();

    let mut spot_user_custom_margin_ratio =
        if cross_margin_requirement_type == MarginRequirementType::Initial {
            user.max_margin_ratio
        } else {
            0_u32
        };

    if let Some(margin_ratio_override) = context.margin_ratio_override {
        spot_user_custom_margin_ratio = margin_ratio_override.max(spot_user_custom_margin_ratio);
    }

    let user_pool_id = user.pool_id;

    for spot_position in user.spot_positions.iter() {
        validation::position::validate_spot_position(spot_position)?;

        if spot_position.is_available() {
            continue;
        }

        let spot_market = maps.spot_market_map.get_ref(&spot_position.market_index)?;
        let (oracle_price_data, oracle_validity) = maps.oracle_map.get_price_data_and_validity(
            MarketType::Spot,
            spot_market.market_index,
            &spot_market.oracle_id(),
            spot_market.historical_oracle_data.last_oracle_price_twap,
            spot_market.get_max_confidence_interval_multiplier()?,
            -1,
            0,
            Some(LogMode::Margin),
        )?;

        let mut skip_token_value = false;
        if !(user_pool_id == 1 && spot_market.market_index == 0 && !spot_position.is_borrow()) {
            validate!(
                user_pool_id == spot_market.pool_id,
                ErrorCode::InvalidPoolId,
                "user pool id ({}) == spot market pool id ({})",
                user_pool_id,
                spot_market.pool_id,
            )?;
        } else {
            skip_token_value = true;
        }

        let oracle_valid =
            is_oracle_valid_for_action(oracle_validity, Some(VelocityAction::MarginCalc))?;

        let strict_oracle_price = StrictOraclePrice::new(
            oracle_price_data.price,
            spot_market
                .historical_oracle_data
                .last_oracle_price_twap_5min,
            calculation.context.strict,
        );
        strict_oracle_price.validate()?;

        if spot_market.market_index == 0 {
            let token_amount = spot_position.get_signed_token_amount(&spot_market)?;
            if token_amount == 0 {
                validate!(
                    spot_position.scaled_balance == 0,
                    ErrorCode::InvalidMarginRatio,
                    "spot_position.scaled_balance={} when token_amount={}",
                    spot_position.scaled_balance,
                    token_amount,
                )?;
            }

            let mut token_value =
                get_strict_token_value(token_amount, spot_market.decimals, &strict_oracle_price)?;

            match spot_position.balance_type {
                SpotBalanceType::Deposit => {
                    if calculation.context.ignore_invalid_deposit_oracles && !oracle_valid {
                        msg!(
                            "token_value set to 0 for market_index={}",
                            spot_market.market_index
                        );
                        token_value = 0;
                    }

                    if skip_token_value {
                        token_value = 0;
                    }

                    calculation.add_cross_margin_total_collateral(token_value)?;

                    calculation.update_all_deposit_oracles_valid(oracle_valid);

                    #[cfg(feature = "velocity-rs")]
                    calculation.add_spot_asset_value(token_value)?;
                }
                SpotBalanceType::Borrow => {
                    let token_value = token_value.unsigned_abs();

                    validate!(
                        token_value != 0,
                        ErrorCode::InvalidMarginRatio,
                        "token_value=0 for token_amount={} in spot market_index={}",
                        token_amount,
                        spot_market.market_index,
                    )?;

                    calculation.add_cross_margin_margin_requirement(
                        token_value,
                        token_value,
                        MarketIdentifier::spot(0),
                    )?;

                    calculation.add_spot_liability()?;

                    calculation.update_all_spot_liability_oracles_valid(oracle_valid);

                    #[cfg(feature = "velocity-rs")]
                    calculation.add_spot_liability_value(token_value)?;
                }
            }
        } else {
            let signed_token_amount = spot_position.get_signed_token_amount(&spot_market)?;

            let OrderFillSimulation {
                token_amount: worst_case_token_amount,
                orders_value: mut worst_case_orders_value,
                token_value: worst_case_token_value,
                weighted_token_value: mut worst_case_weighted_token_value,
                ..
            } = spot_position
                .get_worst_case_fill_simulation(
                    &spot_market,
                    &strict_oracle_price,
                    Some(signed_token_amount),
                    cross_margin_requirement_type,
                )?
                .apply_user_custom_margin_ratio(
                    &spot_market,
                    strict_oracle_price.current,
                    spot_user_custom_margin_ratio,
                )?;

            if worst_case_token_amount == 0 {
                validate!(
                    spot_position.scaled_balance == 0,
                    ErrorCode::InvalidMarginRatio,
                    "spot_position.scaled_balance={} when worst_case_token_amount={} market_index={}",
                    spot_position.scaled_balance,
                    worst_case_token_amount,
                    spot_market.market_index,
                )?;
            }

            calculation.add_cross_margin_margin_requirement(
                spot_position.margin_requirement_for_open_orders()?,
                0,
                MarketIdentifier::spot(spot_market.market_index),
            )?;

            match worst_case_token_value.cmp(&0) {
                Ordering::Greater => {
                    if calculation.context.ignore_invalid_deposit_oracles && !oracle_valid {
                        msg!(
                            "worst_case_weighted_token_value set to 0 for market_index={}",
                            spot_market.market_index
                        );
                        worst_case_weighted_token_value = 0;
                    }

                    calculation.add_cross_margin_total_collateral(
                        worst_case_weighted_token_value.cast::<i128>()?,
                    )?;

                    calculation.update_all_deposit_oracles_valid(oracle_valid);

                    #[cfg(feature = "velocity-rs")]
                    calculation.add_spot_asset_value(worst_case_token_value)?;
                }
                Ordering::Less => {
                    validate!(
                        worst_case_weighted_token_value.unsigned_abs() >= worst_case_token_value.unsigned_abs(),
                        ErrorCode::InvalidMarginRatio,
                        "weighted_token_value < abs(worst_case_token_value) in spot market_index={}",
                        spot_market.market_index,
                    )?;

                    validate!(
                        worst_case_weighted_token_value != 0,
                        ErrorCode::InvalidOracle,
                        "weighted_token_value=0 for worst_case_token_amount={} in spot market_index={}",
                        worst_case_token_amount,
                        spot_market.market_index,
                    )?;

                    calculation.add_cross_margin_margin_requirement(
                        worst_case_weighted_token_value.unsigned_abs(),
                        worst_case_token_value.unsigned_abs(),
                        MarketIdentifier::spot(spot_market.market_index),
                    )?;

                    calculation.add_spot_liability()?;
                    calculation.update_with_spot_isolated_liability(
                        spot_market.asset_tier == AssetTier::Isolated,
                    );

                    calculation.update_all_spot_liability_oracles_valid(oracle_valid);

                    #[cfg(feature = "velocity-rs")]
                    calculation.add_spot_liability_value(worst_case_token_value.unsigned_abs())?;
                }
                Ordering::Equal => {
                    if spot_position.has_open_order() {
                        calculation.add_spot_liability()?;
                        calculation.update_all_spot_liability_oracles_valid(oracle_valid);
                        calculation.update_with_spot_isolated_liability(
                            spot_market.asset_tier == AssetTier::Isolated,
                        );
                    }
                }
            }

            match worst_case_orders_value.cmp(&0) {
                Ordering::Greater => {
                    if calculation.context.ignore_invalid_deposit_oracles && !oracle_valid {
                        msg!(
                            "worst_case_orders_value set to 0 for market_index={}",
                            spot_market.market_index
                        );
                        worst_case_orders_value = 0;
                    }

                    calculation.add_cross_margin_total_collateral(
                        worst_case_orders_value.cast::<i128>()?,
                    )?;

                    #[cfg(feature = "velocity-rs")]
                    calculation.add_spot_asset_value(worst_case_orders_value)?;
                }
                Ordering::Less => {
                    calculation.add_cross_margin_margin_requirement(
                        worst_case_orders_value.unsigned_abs(),
                        worst_case_orders_value.unsigned_abs(),
                        MarketIdentifier::spot(0),
                    )?;

                    #[cfg(feature = "velocity-rs")]
                    calculation.add_spot_liability_value(worst_case_orders_value.unsigned_abs())?;
                }
                Ordering::Equal => {}
            }
        }
    }

    for market_position in user.perp_positions.iter() {
        if market_position.is_available() {
            continue;
        }

        let market = &maps
            .perp_market_map
            .get_ref(&market_position.market_index)?;

        validate!(
            user_pool_id == market.pool_id,
            ErrorCode::InvalidPoolId,
            "user pool id ({}) == perp market pool id ({})",
            user_pool_id,
            market.pool_id,
        )?;

        let quote_spot_market = maps
            .spot_market_map
            .get_ref(&market.quote_spot_market_index)?;
        let (quote_oracle_price_data, quote_oracle_validity) =
            maps.oracle_map.get_price_data_and_validity(
                MarketType::Spot,
                quote_spot_market.market_index,
                &quote_spot_market.oracle_id(),
                quote_spot_market
                    .historical_oracle_data
                    .last_oracle_price_twap,
                quote_spot_market.get_max_confidence_interval_multiplier()?,
                -1,
                0,
                Some(LogMode::Margin),
            )?;

        let strict_quote_price = StrictOraclePrice::new(
            quote_oracle_price_data.price,
            quote_spot_market
                .historical_oracle_data
                .last_oracle_price_twap_5min,
            calculation.context.strict,
        );
        drop(quote_spot_market);

        let (oracle_price_data, oracle_validity) = maps.oracle_map.get_price_data_and_validity(
            MarketType::Perp,
            market.market_index,
            &market.oracle_id(),
            market
                .market_stats
                .historical_oracle_data
                .last_oracle_price_twap,
            market.get_max_confidence_interval_multiplier()?,
            market.oracle_slot_delay_override,
            market.oracle_low_risk_slot_delay_override,
            Some(LogMode::Margin),
        )?;

        let position_margin_type = if market_position.is_isolated() {
            context
                .margin_type_config
                .get_isolated_margin_requirement_type(market_position.market_index)
        } else {
            context
                .margin_type_config
                .get_cross_margin_requirement_type()
        };

        let perp_user_custom_margin_ratio =
            if position_margin_type == MarginRequirementType::Initial {
                user.max_margin_ratio
            } else {
                0_u32
            };

        let mut perp_position_custom_margin_ratio =
            if position_margin_type == MarginRequirementType::Initial {
                perp_user_custom_margin_ratio.max(market_position.max_margin_ratio as u32)
            } else {
                0_u32
            };

        if let Some(margin_ratio_override) = context.margin_ratio_override {
            perp_position_custom_margin_ratio =
                margin_ratio_override.max(perp_position_custom_margin_ratio);
        }

        let (perp_margin_requirement, weighted_pnl, worst_case_liability_value, _base_asset_value) =
            calculate_perp_position_value_and_pnl(
                market_position,
                market,
                oracle_price_data,
                &strict_quote_price,
                position_margin_type,
                perp_user_custom_margin_ratio.max(perp_position_custom_margin_ratio),
            )?;

        if market_position.is_isolated() {
            let quote_spot_market = maps
                .spot_market_map
                .get_ref(&market.quote_spot_market_index)?;
            let quote_token_amount = get_token_amount(
                market_position
                    .isolated_position_scaled_balance
                    .cast::<u128>()?,
                &quote_spot_market,
                &SpotBalanceType::Deposit,
            )?;

            let quote_token_value = get_strict_token_value(
                quote_token_amount.cast::<i128>()?,
                quote_spot_market.decimals,
                &strict_quote_price,
            )?;

            calculation.add_isolated_margin_calculation(
                market.market_index,
                quote_token_value,
                weighted_pnl,
                worst_case_liability_value,
                perp_margin_requirement,
            )?;

            #[cfg(feature = "velocity-rs")]
            calculation.add_spot_asset_value(quote_token_value)?;
        } else {
            calculation.add_cross_margin_margin_requirement(
                perp_margin_requirement,
                worst_case_liability_value,
                MarketIdentifier::perp(market.market_index),
            )?;

            calculation.add_cross_margin_total_collateral(weighted_pnl)?;
        }

        #[cfg(feature = "velocity-rs")]
        calculation.add_perp_liability_value(worst_case_liability_value)?;
        #[cfg(feature = "velocity-rs")]
        calculation.add_perp_pnl(weighted_pnl)?;

        let has_perp_liability = market_position.base_asset_amount != 0
            || market_position.quote_asset_amount < 0
            || market_position.has_open_order();

        if has_perp_liability {
            calculation.add_perp_liability()?;
            calculation.update_with_perp_isolated_liability(
                market.contract_tier == ContractTier::Isolated,
            );
        }

        if has_perp_liability || position_margin_type != MarginRequirementType::Initial {
            calculation.update_all_liability_oracles_valid(is_oracle_valid_for_action(
                quote_oracle_validity,
                Some(VelocityAction::MarginCalc),
            )?);
            calculation.update_all_liability_oracles_valid(is_oracle_valid_for_action(
                oracle_validity,
                Some(VelocityAction::MarginCalc),
            )?);
        }
    }

    calculation.validate_num_spot_liabilities()?;

    Ok(calculation)
}

pub fn validate_any_isolated_tier_requirements(
    user: &User,
    calculation: &MarginCalculation,
) -> VelocityResult {
    if calculation.with_perp_isolated_liability && !user.is_reduce_only() {
        validate!(
            calculation.num_perp_liabilities <= 1,
            ErrorCode::IsolatedAssetTierViolation,
            "User attempting to increase perp liabilities above 1 with a isolated tier liability"
        )?;

        validate!(
            !user.is_margin_trading_enabled,
            ErrorCode::IsolatedAssetTierViolation,
            "User attempting isolated tier liability with margin trading enabled"
        )?;

        if calculation.num_spot_liabilities > 0 {
            let quote_spot_position = user.get_quote_spot_position();
            validate!(
                    (calculation.num_spot_liabilities == 1 && quote_spot_position.is_borrow()
                    ),
                    ErrorCode::IsolatedAssetTierViolation,
                    "User attempting to increase spot liabilities beyond the quote asset with a isolated tier liability"
                )?;
        }
    }

    if calculation.with_spot_isolated_liability && !user.is_reduce_only() {
        validate!(
            calculation.num_perp_liabilities == 0 && calculation.num_spot_liabilities == 1,
            ErrorCode::IsolatedAssetTierViolation,
            "User attempting to increase perp liabilities above 0 with a isolated tier liability"
        )?;
    }

    Ok(())
}

pub fn meets_place_order_margin_requirement(
    user: &User,
    maps: &mut AccountMaps,
    risk_increasing: bool,
    isolated_market_index: Option<u16>,
) -> VelocityResult {
    let margin_type_config = if risk_increasing {
        match isolated_market_index {
            Some(market_index) => MarginTypeConfig::IsolatedPositionOverride {
                market_index,
                margin_requirement_type: MarginRequirementType::Initial,
                default_isolated_margin_requirement_type: MarginRequirementType::Maintenance,
                cross_margin_requirement_type: MarginRequirementType::Maintenance,
            },
            None => MarginTypeConfig::CrossMarginOverride {
                margin_requirement_type: MarginRequirementType::Initial,
                default_margin_requirement_type: MarginRequirementType::Maintenance,
            },
        }
    } else {
        MarginTypeConfig::Default(MarginRequirementType::Maintenance)
    };

    let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        maps,
        MarginContext::standard_with_config(margin_type_config).strict(true),
    )?;

    if !calculation.meets_margin_requirement() {
        msg!("margin calculation: {:?}", calculation);
        return Err(ErrorCode::InsufficientCollateral);
    }

    if risk_increasing {
        if let Some(net_equity) = calculate_net_equity_for_floor(user, maps)? {
            net_equity.validate_clears_buffered_floor(user)?;
        }
    }

    validate_any_isolated_tier_requirements(user, &calculation)?;

    Ok(())
}

pub fn meets_initial_margin_requirement(
    user: &User,
    maps: &mut AccountMaps,
) -> VelocityResult<bool> {
    calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        maps,
        MarginContext::standard(MarginRequirementType::Initial),
    )
    .map(|calc| calc.meets_margin_requirement())
}

pub fn meets_settle_pnl_maintenance_margin_requirement(
    user: &User,
    maps: &mut AccountMaps,
) -> VelocityResult<bool> {
    calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        maps,
        MarginContext::standard(MarginRequirementType::Maintenance).strict(true),
    )
    .map(|calc| calc.meets_margin_requirement())
}

pub fn meets_maintenance_margin_requirement(
    user: &User,
    maps: &mut AccountMaps,
) -> VelocityResult<bool> {
    calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        maps,
        MarginContext::standard(MarginRequirementType::Maintenance),
    )
    .map(|calc| calc.meets_margin_requirement())
}

pub fn calculate_max_withdrawable_amount(
    market_index: u16,
    user: &User,
    maps: &mut AccountMaps,
) -> VelocityResult<u64> {
    let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        maps,
        MarginContext::standard(MarginRequirementType::Initial),
    )?;

    let spot_market = &mut maps.spot_market_map.get_ref(&market_index)?;

    let token_amount = user
        .get_spot_position(market_index)?
        .get_token_amount(spot_market)?;

    let oracle_price = maps
        .oracle_map
        .get_price_data(&spot_market.oracle_id())?
        .price;

    let asset_weight = spot_market.get_asset_weight(
        token_amount,
        oracle_price,
        &MarginRequirementType::Initial,
    )?;

    if asset_weight == 0 {
        return Ok(u64::MAX);
    }

    if calculation.get_num_of_liabilities()? == 0 {
        // user has small dust deposit and no liabilities
        // so return early with user tokens amount
        return token_amount.cast();
    }

    let free_collateral = calculation.get_cross_free_collateral()?;

    let (numerator_scale, denominator_scale) = if spot_market.decimals > 6 {
        (10_u128.pow(spot_market.decimals - 6), 1)
    } else {
        (1, 10_u128.pow(6 - spot_market.decimals))
    };

    free_collateral
        .saturating_sub(1) // add buffer to avoid insufficient collateral
        .safe_mul(MARGIN_PRECISION_U128)?
        .safe_div(asset_weight.cast()?)?
        .safe_mul(PRICE_PRECISION)?
        .safe_div(oracle_price.cast()?)?
        .safe_mul(numerator_scale)?
        .safe_div(denominator_scale)?
        .cast()
}

pub fn validate_spot_margin_trading(user: &User, maps: &mut AccountMaps) -> VelocityResult {
    if user.is_margin_trading_enabled {
        for perp_position in &user.perp_positions {
            if !perp_position.is_available() {
                let perp_market = maps.perp_market_map.get_ref(&perp_position.market_index)?;

                validate!(
                    perp_market.contract_tier != ContractTier::Isolated,
                    ErrorCode::IsolatedAssetTierViolation,
                    "Isolated perpetual market = {} doesn't allow margin trading",
                    perp_market.market_index
                )?;
            }
        }

        return Ok(());
    }

    let mut total_open_bids_value = 0_i128;
    for spot_position in &user.spot_positions {
        let asks = spot_position.open_asks;
        if asks < 0 {
            let spot_market = maps.spot_market_map.get_ref(&spot_position.market_index)?;
            let signed_token_amount = spot_position.get_signed_token_amount(&spot_market)?;
            // The user can have:
            // 1. no open asks with an existing short
            // 2. open asks with a larger existing long
            validate!(
                signed_token_amount.safe_add(asks.cast()?)? >= 0,
                ErrorCode::MarginTradingDisabled,
                "Open asks can lead to increased borrow in spot market {}",
                spot_position.market_index
            )?;
        }

        let bids = spot_position.open_bids;
        if bids > 0 {
            let spot_market = maps.spot_market_map.get_ref(&spot_position.market_index)?;
            let oracle_price_data = maps.oracle_map.get_price_data(&spot_market.oracle_id())?;
            let open_bids_value =
                get_token_value(-bids as i128, spot_market.decimals, oracle_price_data.price)?;

            total_open_bids_value = total_open_bids_value.safe_add(open_bids_value)?;
        }
    }

    let mut quote_token_amount = 0_i128;
    let quote_spot_position = user.get_quote_spot_position();
    if !quote_spot_position.is_available() {
        let quote_spot_market = maps.spot_market_map.get_quote_spot_market()?;
        quote_token_amount = quote_spot_position.get_signed_token_amount(&quote_spot_market)?;
    }

    // The user can have open bids if their value is less than existing quote token amount
    validate!(
        total_open_bids_value == 0 || quote_token_amount.safe_add(total_open_bids_value)? >= 0,
        ErrorCode::MarginTradingDisabled,
        "Open bids leads to increased borrow for spot market 0"
    )?;

    Ok(())
}

/// The most favorable equity value the breaker can prove from the positions
/// whose raw exchange oracles are valid. `Unprovable` means at least one
/// invalid-oracle asset or long has no finite upper bound, or a quote oracle
/// needed to convert perp pnl is invalid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TripUpperBound {
    Finite(i128),
    Unprovable,
}

impl TripUpperBound {
    fn add_exact(&mut self, value: i128) -> VelocityResult {
        if let Self::Finite(current) = self {
            *current = current.safe_add(value)?;
        }
        Ok(())
    }

    fn mark_unprovable(&mut self) {
        *self = Self::Unprovable;
    }
}

/// One position walk feeds every equity-floor consumer. `observed_value` is the
/// live-oracle point value that strict gates and diagnostics use.
/// `all_oracles_valid` says whether that point is trusted for `MarginCalc`.
/// `trip_upper_bound` is the separate, user-favorable proof used only to arm the
/// breaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UserEquityCalculation {
    observed_value: i128,
    all_oracles_valid: bool,
    trip_upper_bound: TripUpperBound,
}

/// Net equity at live oracle prices, its strict oracle-validity verdict, and the
/// breaker-trip upper bound, all from one position walk. The equity is unweighted
/// assets and funding-inclusive perp pnl, minus unweighted spot liabilities.
///
/// Spot balances and funding are valued as of their markets' last accrual. Interest or
/// funding accrued since then is not applied here. The margin engine shares that
/// staleness. It always overstates equity by the unaccrued borrow cost, and the
/// permissionless interest and funding cranks bound it.
fn calculate_user_equity_calculation<
    const INCLUDE_OBSERVED_VALUE: bool,
    const INCLUDE_TRIP_UPPER_BOUND: bool,
>(
    user: &User,
    maps: &mut AccountMaps,
) -> VelocityResult<UserEquityCalculation> {
    let mut net_usd_value: i128 = 0;
    let mut all_oracles_valid = true;
    let mut trip_upper_bound = TripUpperBound::Finite(0);

    for spot_position in user.spot_positions.iter() {
        if spot_position.is_available() {
            continue;
        }

        let spot_market = maps.spot_market_map.get_ref(&spot_position.market_index)?;
        let (oracle_price_data, oracle_validity) = maps.oracle_map.get_price_data_and_validity(
            MarketType::Spot,
            spot_market.market_index,
            &spot_market.oracle_id(),
            spot_market.historical_oracle_data.last_oracle_price_twap,
            spot_market.get_max_confidence_interval_multiplier()?,
            -1,
            0,
            Some(LogMode::Margin),
        )?;
        let oracle_valid =
            is_oracle_valid_for_action(oracle_validity, Some(VelocityAction::MarginCalc))?;
        all_oracles_valid &= oracle_valid;

        let token_amount = spot_position.get_signed_token_amount(&spot_market)?;
        let oracle_price = oracle_price_data.price;
        if INCLUDE_OBSERVED_VALUE || (INCLUDE_TRIP_UPPER_BOUND && oracle_valid) {
            let token_value = get_token_value(token_amount, spot_market.decimals, oracle_price)?;
            if INCLUDE_OBSERVED_VALUE {
                net_usd_value = net_usd_value.safe_add(token_value)?;
            }
            if INCLUDE_TRIP_UPPER_BOUND && oracle_valid {
                trip_upper_bound.add_exact(token_value)?;
            }
        } else if INCLUDE_TRIP_UPPER_BOUND && token_amount > 0 {
            // A positive asset has no finite upper bound when its oracle is
            // invalid. A liability's most favorable value is zero, so it
            // adds nothing to the breaker upper bound.
            trip_upper_bound.mark_unprovable();
        }
    }

    for market_position in user.perp_positions.iter() {
        if market_position.is_available() {
            continue;
        }

        let market = &maps
            .perp_market_map
            .get_ref(&market_position.market_index)?;

        let (quote_oracle_price, quote_oracle_valid) = {
            let quote_spot_market = maps
                .spot_market_map
                .get_ref(&market.quote_spot_market_index)?;
            let (quote_oracle_price_data, quote_oracle_validity) =
                maps.oracle_map.get_price_data_and_validity(
                    MarketType::Spot,
                    quote_spot_market.market_index,
                    &quote_spot_market.oracle_id(),
                    quote_spot_market
                        .historical_oracle_data
                        .last_oracle_price_twap,
                    quote_spot_market.get_max_confidence_interval_multiplier()?,
                    -1,
                    0,
                    Some(LogMode::Margin),
                )?;

            let quote_oracle_valid = is_oracle_valid_for_action(
                quote_oracle_validity,
                Some(VelocityAction::MarginCalc),
            )?;
            all_oracles_valid &= quote_oracle_valid;

            if INCLUDE_TRIP_UPPER_BOUND && !quote_oracle_valid {
                // This oracle converts the whole perp pnl into the account's
                // equity denomination, so neither the base leg nor the fixed
                // quote/funding leg can provide a finite trip upper bound.
                trip_upper_bound.mark_unprovable();
            }

            if market_position.is_isolated() {
                let quote_token_amount =
                    market_position.get_isolated_token_amount(&quote_spot_market)?;

                if INCLUDE_OBSERVED_VALUE || (INCLUDE_TRIP_UPPER_BOUND && quote_oracle_valid) {
                    let token_value = get_token_value(
                        quote_token_amount.cast()?,
                        quote_spot_market.decimals,
                        quote_oracle_price_data.price,
                    )?;
                    if INCLUDE_OBSERVED_VALUE {
                        net_usd_value = net_usd_value.safe_add(token_value)?;
                    }
                    if INCLUDE_TRIP_UPPER_BOUND && quote_oracle_valid {
                        trip_upper_bound.add_exact(token_value)?;
                    }
                }
            }

            (quote_oracle_price_data.price, quote_oracle_valid)
        };

        let (oracle_price_data, oracle_validity) = maps.oracle_map.get_price_data_and_validity(
            MarketType::Perp,
            market.market_index,
            &market.oracle_id(),
            market
                .market_stats
                .historical_oracle_data
                .last_oracle_price_twap,
            market.get_max_confidence_interval_multiplier()?,
            market.oracle_slot_delay_override,
            market.oracle_low_risk_slot_delay_override,
            Some(LogMode::Margin),
        )?;

        let settled = market.status == MarketStatus::Settlement;

        // A settled market is valued at its expiry price. Its oracle does not
        // enter the number, so its verdict must not enter the flag either. A
        // dead oracle on a settled market would otherwise permanently block
        // every consumer that requires validity: the floor gates, the breaker
        // trip, the reset, and cure transfers.
        let perp_oracle_valid = settled
            || is_oracle_valid_for_action(oracle_validity, Some(VelocityAction::MarginCalc))?;
        all_oracles_valid &= perp_oracle_valid;

        let valuation_price = if settled {
            market.expiry_price
        } else {
            oracle_price_data.price
        };

        let unrealized_funding = calculate_funding_payment(
            if market_position.base_asset_amount > 0 {
                market.cumulative_funding_rate_long
            } else {
                market.cumulative_funding_rate_short
            },
            market_position,
        )?;

        let exact_pnl_value = if INCLUDE_OBSERVED_VALUE
            || (INCLUDE_TRIP_UPPER_BOUND && quote_oracle_valid && perp_oracle_valid)
        {
            // Settlement values against a possibly-negative expiry price (OtterSec #133).
            let (_, unrealized_pnl) = if market.status == MarketStatus::Settlement {
                calculate_base_asset_value_and_pnl_with_expiry_price(
                    market_position,
                    valuation_price,
                )?
            } else {
                calculate_base_asset_value_and_pnl_with_oracle_price(
                    market_position,
                    valuation_price,
                )?
            };
            let pnl = unrealized_pnl.safe_add(unrealized_funding.cast()?)?;
            Some(
                pnl.safe_mul(quote_oracle_price.cast()?)?
                    .safe_div(PRICE_PRECISION_I128)?,
            )
        } else {
            None
        };

        if INCLUDE_OBSERVED_VALUE {
            net_usd_value = net_usd_value.safe_add(exact_pnl_value.ok_or(ErrorCode::MathError)?)?;
        }

        if INCLUDE_TRIP_UPPER_BOUND && quote_oracle_valid {
            if perp_oracle_valid {
                trip_upper_bound.add_exact(exact_pnl_value.ok_or(ErrorCode::MathError)?)?;
            } else if market_position.base_asset_amount > 0 {
                // The invalid-oracle long base leg has no finite upper bound.
                trip_upper_bound.mark_unprovable();
            } else {
                // Entry quote and funding are stored values. A short base leg
                // can only reduce equity at a non-negative true price, so its
                // most favorable value is zero. A zero-base position is exact.
                let fixed_pnl = market_position
                    .quote_asset_amount
                    .cast::<i128>()?
                    .safe_add(unrealized_funding.cast()?)?;
                let fixed_pnl_value = fixed_pnl
                    .safe_mul(quote_oracle_price.cast()?)?
                    .safe_div(PRICE_PRECISION_I128)?;
                trip_upper_bound.add_exact(fixed_pnl_value)?;
            }
        }
    }

    Ok(UserEquityCalculation {
        observed_value: net_usd_value,
        all_oracles_valid,
        trip_upper_bound,
    })
}

/// Compatibility wrapper for non-floor consumers and strict floor gates.
pub fn calculate_user_equity(user: &User, maps: &mut AccountMaps) -> VelocityResult<(i128, bool)> {
    let calculation = calculate_user_equity_calculation::<true, false>(user, maps)?;
    Ok((calculation.observed_value, calculation.all_oracles_valid))
}

/// Net equity paired with the oracle-validity verdict of the walk that produced it.
/// Every floor decision consumes both, because a value built from an invalid price is
/// not bounded by anything. A stale oracle and its own 5-minute twap can share the same
/// wrong value, so no pair of stored prices can bracket the true one. Each predicate
/// therefore fails closed for its own direction instead of trusting the number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FloorNetEquity {
    /// Net equity from [`calculate_user_equity`]. It is unweighted assets and perp pnl
    /// minus unweighted spot liabilities, at live oracle prices.
    pub value: i128,
    /// False when any oracle this walk read is invalid for `MarginCalc`.
    pub all_oracles_valid: bool,
}

impl FloorNetEquity {
    /// True when the subaccount provably clears `floor + buffer`, meaning every oracle
    /// is valid and net equity is at or above the buffered floor. Paths that authorize
    /// an action require this, so an invalid oracle cannot price the subaccount up
    /// through the floor. Those paths are risk-increasing placement and fills,
    /// withdrawals, transfers out, and liquidator admission.
    pub fn clears_buffered_floor(&self, user: &User) -> bool {
        self.all_oracles_valid && !user.is_below_buffered_equity_floor(self.value)
    }

    /// True when the subaccount is provably below its raw floor, meaning every oracle
    /// is valid and net equity is below the floor. Paths where being below the floor
    /// authorizes someone against the user require this, so a bad price cannot
    /// manufacture that authorization. Force-cancel grounds are such a path.
    pub fn proves_below_floor(&self, user: &User) -> bool {
        self.all_oracles_valid && user.is_below_equity_floor(self.value)
    }

    /// Shared fail-closed buffered-floor gate. It returns `InvalidOracle` when the
    /// value cannot be trusted, and `EquityBelowFloor` when a trusted value sits below
    /// `floor + buffer`.
    pub fn validate_clears_buffered_floor(&self, user: &User) -> VelocityResult {
        validate!(
            self.all_oracles_valid,
            ErrorCode::InvalidOracle,
            "cannot verify equity floor {} + buffer {} with an invalid oracle (authority {} subaccount {})",
            user.equity_floor,
            user.equity_floor_buffer,
            user.authority,
            user.sub_account_id
        )?;

        validate!(
            !user.is_below_buffered_equity_floor(self.value),
            ErrorCode::EquityBelowFloor,
            "net equity {} below equity floor {} + buffer {} (authority {} subaccount {})",
            self.value,
            user.equity_floor,
            user.equity_floor_buffer,
            user.authority,
            user.sub_account_id
        )?;

        Ok(())
    }
}

/// Net equity for the equity-floor gates. It returns [`calculate_user_equity`] and its
/// oracle-validity verdict when the user has a floor set, and `None` otherwise so
/// callers skip the extra position pass. Unlike the margin numerator
/// `total_collateral`, this values assets, perp pnl and spot liabilities at unweighted
/// oracle prices, so borrows subtract their full value.
///
/// Callers pick the [`FloorNetEquity`] predicate that fails closed for the
/// decision they make.
pub fn calculate_net_equity_for_floor(
    user: &User,
    maps: &mut AccountMaps,
) -> VelocityResult<Option<FloorNetEquity>> {
    if user.equity_floor == 0 {
        return Ok(None);
    }

    let (value, all_oracles_valid) = calculate_user_equity(user, maps)?;

    Ok(Some(FloorNetEquity {
        value,
        all_oracles_valid,
    }))
}

/// Net-equity upper bound for the breaker trip. Positions with valid oracles
/// are valued at live prices, exactly as [`calculate_user_equity`] values
/// them. For an invalid oracle, a liability or short base leg counts as zero
/// because it can only lower equity at a non-negative true price. An asset or
/// long has no finite upper bound and makes the breach unprovable. The quote
/// oracle stays strict because it converts the whole perp pnl.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TripNetEquity {
    /// Upper bound of net equity. Trusted values are exact and conceded values sit at
    /// their ceiling. The bound is meaningless when `provable` is false.
    pub equity_upper_bound: i128,
    /// False when an invalid-oracle asset or long has no finite upper bound, or when a
    /// quote oracle required to convert perp pnl is invalid.
    pub provable: bool,
}

impl TripNetEquity {
    /// True when the subaccount is provably below its raw floor at every non-negative
    /// true price of the invalid-oracle liabilities and shorts. The permissionless trip
    /// and the lazy trip both decide with this predicate.
    pub fn proves_breach(&self, user: &User) -> bool {
        self.provable && user.is_below_equity_floor(self.equity_upper_bound)
    }
}

/// Breaker-trip view of the shared equity walk. Floor gates, reset and cure transfers
/// keep the strict all-or-nothing validity verdict. Only the two trip paths consume
/// this user-favorable upper bound.
pub fn calculate_user_equity_for_trip(
    user: &User,
    maps: &mut AccountMaps,
) -> VelocityResult<TripNetEquity> {
    let calculation = calculate_user_equity_calculation::<false, true>(user, maps)?;
    Ok(match calculation.trip_upper_bound {
        TripUpperBound::Finite(equity_upper_bound) => TripNetEquity {
            equity_upper_bound,
            provable: true,
        },
        TripUpperBound::Unprovable => TripNetEquity {
            equity_upper_bound: 0,
            provable: false,
        },
    })
}
