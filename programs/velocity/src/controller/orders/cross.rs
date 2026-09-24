//! Crossing two resting sources against each other.
//!
//! A crank matches a book order against a counterparty rather than filling a
//! taker's own order, so it prices and gates the match itself. The oracle
//! pre-flight holds such a crank to the rules an ordinary fill runs under.

use super::*;

/// The market and oracle state a crossed-book crank checks before it moves a
/// position.
pub(crate) struct CrankOraclePreflight {
    pub oracle_price: i64,
    /// Whether the oracle is too old to price margin.
    pub stale_for_margin: bool,
    /// Open interest before the crank. The post-fill rule measures against it.
    pub open_interest: u128,
}

/// The market gates a crossed-book crank passes.
///
/// These are the gates `admit_perp_market` applies to a routed fill. A crank
/// that settles a match itself never reaches that function, so it runs them
/// here. `is_in_settlement` covers only `Settlement` and `Delisted`, so the
/// status check is what refuses an `Initialized` market.
pub(crate) fn crank_market_gates(market: &PerpMarket, now: i64) -> VelocityResult {
    validate!(
        matches!(
            market.status,
            MarketStatus::Active | MarketStatus::ReduceOnly
        ),
        ErrorCode::MarketFillOrderPaused,
        "Market not active",
    )?;
    validate!(
        !market.is_in_settlement(now),
        ErrorCode::MarketFillOrderPaused,
        "Market is in settlement mode",
    )?;
    validate!(
        !market.is_operation_paused(PerpOperation::Fill),
        ErrorCode::MarketFillOrderPaused,
        "Market fills paused",
    )?;

    Ok(())
}

/// Hold a crossed-book crank to the oracle rules an ordinary fill runs under.
///
/// A crank matches two resting sources at a price the oracle bounds, so the
/// gates are the same ones a fill passes.
///
/// `crank` names the caller in the error message, so a refusal says which
/// crank refused. The market stays borrowed by the caller, which reads its
/// own extra fields after this returns.
pub(crate) fn crank_oracle_preflight(
    market: &mut PerpMarket,
    state: &State,
    oracle_map: &mut OracleMap,
    clock: &Clock,
    crank: &str,
) -> VelocityResult<CrankOraclePreflight> {
    validation::perp_market::validate_perp_market(market)?;
    crank_market_gates(market, clock.unix_timestamp)?;

    let oracle_price_data = *oracle_map.get_price_data(&market.oracle_id())?;
    let (mm_oracle_price_data, safe_oracle_validity) =
        safe_mm_oracle_state(market, state, &oracle_price_data, clock.slot)?;
    validate!(
        is_oracle_valid_for_action(safe_oracle_validity, Some(VelocityAction::FillOrderMatch))?,
        ErrorCode::InvalidOracle,
        "oracle not valid for {}",
        crank
    )?;

    let oracle_price = mm_oracle_price_data.get_price();
    validate_market_within_price_band(market, state, oracle_price)?;
    Ok(CrankOraclePreflight {
        oracle_price,
        stale_for_margin: state
            .slot_clock()
            .elapsed_slot_delta(mm_oracle_price_data.get_delay().max(0) as u64, clock.slot)
            > state.oracle_guard_rails.validity.stale_for_margin_ms(),
        open_interest: market.get_open_interest(),
    })
}

/// What one taker-origin cross costs, and the market facts its post-fill
/// checks measure against.
pub struct TakerOriginCrossPricing {
    pub fee: fees::TakerOriginCrossFee,
    pub oracle_stale_for_margin: bool,
    /// Open interest before the fill.
    pub perp_market_oi_before: u128,
}

/// The oracle pre-flight and the pricing of one taker-origin cross, before any
/// of it is committed.
///
/// The caller must run this before the CLOB calls that consume the pair. The
/// cross is refused outright when crossing would leave the taker worse off
/// than the price it was resting at, and a refusal has to leave the book
/// untouched. The pre-flight applies the market gates and the oracle gates the
/// fill path applies: `FillOrderMatch` validity, the price band, and
/// staleness. The reward's size-vs-oracle multiplier is derived from the
/// oracle price, so the reward is a value transfer an oracle read drives.
///
/// `rest_price` is the price the taker-origin order rests at.
/// `counterparty_price` is the price the match will settle at. `order_slot` is
/// the slot the taker-origin order was placed on the book.
#[allow(clippy::too_many_arguments)]
pub fn price_taker_origin_cross(
    state: &State,
    market_index: u16,
    taker_direction: PositionDirection,
    rest_price: u64,
    counterparty_price: u64,
    base_asset_amount: u64,
    order_slot: u64,
    taker_stats: &UserStats,
    perp_market_map: &PerpMarketMap,
    oracle_map: &mut OracleMap,
    clock: &Clock,
) -> VelocityResult<TakerOriginCrossPricing> {
    let (preflight, fee_adjustment, taker_fee_addon) = {
        let market = &mut perp_market_map.get_ref_mut(&market_index)?;
        let preflight =
            crank_oracle_preflight(market, state, oracle_map, clock, "taker-origin cross")?;
        (
            preflight,
            market.fee_adjustment,
            market.taker_fee_addon_tenth_bps,
        )
    };
    let oracle_price = preflight.oracle_price;

    // Both notionals at the CLOB's own rounding, so the improvement is
    // measured in the same units the fill will settle in.
    let notional = |price: u64| clob_notional(price, base_asset_amount);
    let fee = fees::calculate_taker_origin_cross_fee(
        taker_direction,
        notional(rest_price)?,
        notional(counterparty_price)?,
        &fees::determine_user_fee_tier(
            taker_stats,
            &state.perp_fee_structure,
            &MarketType::Perp,
            clock.unix_timestamp,
            state.promo_fee_tier,
        )?,
        fee_adjustment,
        taker_fee_addon,
        order_slot,
        clock.slot,
        state.slot_clock(),
        calculate_filler_multiplier_for_matched_orders(
            counterparty_price,
            taker_direction.opposite(),
            oracle_price,
        )?,
        &state.perp_fee_structure.filler_reward_structure,
    )?;

    Ok(TakerOriginCrossPricing {
        fee,
        oracle_stale_for_margin: preflight.stale_for_margin,
        perp_market_oi_before: preflight.open_interest,
    })
}

/// Notional of `base_asset_amount` at `price`, floored. This is the CLOB's
/// own rounding, so a notional velocity computes for a remainder it prices
/// itself lands in the same units as a book-filled leg.
pub fn clob_notional(price: u64, base_asset_amount: u64) -> VelocityResult<u64> {
    price
        .cast::<u128>()?
        .safe_mul(base_asset_amount.cast()?)?
        .safe_div(BASE_PRECISION_U64.cast()?)?
        .cast::<u64>()
}

/// The `Order` a taker-origin remainder is, so the router fills it like any
/// other order. It is not stored: the fill path takes it directly, so it
/// holds no order slot and never leaves the book. The caller reports the
/// fill afterward, and the order shrinks in place against its reservation.
///
/// Its price is the taker's own resting limit, so a routed fill matches
/// only at or better, reaching the taker with the price improvement.
/// The CLOB's order id is wider than velocity's. Narrowing it keeps fill
/// records pointing at the book's order, since ids are sequential per
/// book. The crank's own record keeps the full-width id.
pub fn taker_origin_order(
    market_index: u16,
    taker_direction: PositionDirection,
    resting: &crate::math::crosses::RestingOrder,
) -> Order {
    Order {
        slot: resting.placed_slot,
        order_id: resting.order_ref.order_id as u32,
        market_index,
        status: OrderStatus::Open,
        order_type: OrderType::Limit,
        market_type: MarketType::Perp,
        direction: taker_direction,
        base_asset_amount: resting.base_asset_amount,
        price: resting.price,
        existing_position_direction: taker_direction,
        reduce_only: resting.reduce_only,
        ..Order::default()
    }
}

#[cfg(test)]
mod gate_tests {
    use {
        super::crank_market_gates,
        crate::{
            error::ErrorCode,
            state::{
                market_status::MarketStatus, paused_operations::PerpOperation,
                perp_market::PerpMarket,
            },
        },
    };

    fn market_with(status: MarketStatus) -> PerpMarket {
        PerpMarket {
            status,
            ..PerpMarket::default_test()
        }
    }

    #[test]
    fn an_active_market_crosses() {
        assert!(crank_market_gates(&market_with(MarketStatus::Active), 100).is_ok());
    }

    /// A wind-down market still crosses. The caller forces `reduce_only` onto
    /// both legs, so the cross can only shrink positions.
    #[test]
    fn a_reduce_only_market_crosses() {
        let market = market_with(MarketStatus::ReduceOnly);
        assert!(crank_market_gates(&market, 100).is_ok());
        assert!(market.is_reduce_only().unwrap());
    }

    /// Fills are paused during the warm-up period, and `is_in_settlement`
    /// does not cover it, so the status check is the only refusal.
    #[test]
    fn an_initialized_market_is_refused() {
        assert_eq!(
            crank_market_gates(&market_with(MarketStatus::Initialized), 100)
                .err()
                .unwrap(),
            ErrorCode::MarketFillOrderPaused
        );
    }

    #[test]
    fn a_settling_market_is_refused() {
        assert_eq!(
            crank_market_gates(&market_with(MarketStatus::Settlement), 100)
                .err()
                .unwrap(),
            ErrorCode::MarketFillOrderPaused
        );
    }

    #[test]
    fn a_fill_paused_market_is_refused() {
        let mut market = market_with(MarketStatus::Active);
        market.paused_operations = PerpOperation::Fill as u8;
        assert_eq!(
            crank_market_gates(&market, 100).err().unwrap(),
            ErrorCode::MarketFillOrderPaused
        );
    }

    /// A pair of remainders settles without a router, and the referred taker
    /// keeps its referrer's accelerated rate there too.
    #[test]
    fn a_settled_pair_keeps_the_referrer_rate() {
        let state = crate::state::state::State::default();
        let rules = super::super::PricingRules::for_settlement(&state, true);
        assert!(rules.referrer_is_accelerated);
        assert!(!rules.vamm_maker_rebate);
        assert!(!super::super::PricingRules::for_settlement(&state, false).referrer_is_accelerated);
    }
}
