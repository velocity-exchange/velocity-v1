//! Quote spread construction for the vAMM.
//!
//! Each refresh rebuilds the AMM's cached quote-time state from durable
//! inputs: the long/short spreads, the reference price offset that shifts
//! the quote midpoint, the oracle-reserve divergence pct, and the
//! spread-adjusted ask/bid reserves the swap executes against.
//!
//! The spread is composed from one function per mechanism:
//!
//! ```text
//! w_0        base spread          sigma(q)   inventory scale
//! v          vol spread           lambda(q)  leverage scale
//! d          oracle retreat       r          revenue retreat
//! w_max      dynamic ceiling      beta(f)    funding bias
//!
//! loaded side:  min(w_max, (max(w_0/2, v, d) * sigma(q) * lambda(q) + r) * beta(f))
//! other side:   max(w_0/2, v) + r/2
//! ```
//!
//! The loaded side is the one whose fills grow the pool's net position
//! ([`inventory_increasing_side`]); d lands on whichever side faces the
//! divergence; w_max comes from [`calculate_max_target_spread`]. The
//! pipeline in [`calculate_spread`] is a line-by-line transcription of this
//! composition, and each term's arithmetic lives in its component function,
//! documented with its formula, units, and saturation behavior.

use {
    crate::{
        controller::position::PositionDirection,
        error::{ErrorCode, VelocityResult},
        math::{
            bn::U192,
            casting::Cast,
            constants::{
                AMM_TIMES_PEG_TO_QUOTE_PRECISION_RATIO_I128, AMM_TO_QUOTE_PRECISION_RATIO_I128,
                BID_ASK_SPREAD_PRECISION, BID_ASK_SPREAD_PRECISION_I128,
                DEFAULT_LARGE_BID_ASK_FACTOR, DEFAULT_REVENUE_SINCE_LAST_FUNDING_SPREAD_RETREAT,
                FUNDING_RATE_BUFFER, FUNDING_RATE_OFFSET_DENOMINATOR,
                FUNDING_RATE_OFFSET_PERCENTAGE, MAX_BID_ASK_INVENTORY_SKEW_FACTOR, PEG_PRECISION,
                PERCENTAGE_PRECISION, PERCENTAGE_PRECISION_I128, PRICE_PRECISION,
                PRICE_PRECISION_I128, PRICE_PRECISION_I64, REF_PRICE_OFFSET_SMOOTHING_MIN_STEP,
                REF_PRICE_OFFSET_SMOOTHING_PER_PERIOD_BUDGET,
                REF_PRICE_OFFSET_SMOOTHING_STEP_DIVISOR, SPREAD_CONF_DISCOUNT_DIVISOR,
                SPREAD_CONF_FULL_WEIGHT_THRESHOLD, SPREAD_REVENUE_RETREAT_MAX_DIVISOR,
                SPREAD_VOL_STD_DISCOUNT_DIVISOR,
            },
            safe_math::SafeMath,
            time::{Millis, SlotDuration},
        },
        msg,
        state::{
            oracle::MMOraclePriceData,
            perp_market::{MarketStats, AMM},
        },
        validate,
        vlp::amm::math::amm::_calculate_market_open_bids_asks,
    },
    std::cmp::{max, min, Ordering},
};

#[cfg(test)]
mod tests;

/// Refresh the AMM's cached quote-time state (spreads, reference-price
/// offset, oracle-reserve spread pct, and spread-adjusted ask/bid reserves)
/// in place from durable inputs, stamping `last_spread_update_slot = slot`.
///
/// Restores the legacy `update_spreads` + `update_spread_reserves` mutators
/// that the AMM-decoupling refactor had briefly turned into a returns-only
/// `compute_amm_quote_state`. The cache lives back on `AMM`: it's refreshed
/// here on each AMM crank (`update_oracle_derived_stats`) and each fill
/// `setup`, then read directly by every quote/fill path, so two quotes in
/// the same refresh window see byte-identical spread state, and dashboards
/// can read the values straight off the account.
///
/// `reserve_price` is taken as an input (rather than re-derived from the
/// AMM) so callers can refresh against a just-projected AMM without
/// re-computing the price.
///
/// Internally split shell/body like the batch native handlers: the pure
/// [`compute_quote_state`] derives a [`QuoteState`] snapshot, then
/// [`commit_quote_state`] writes it to the account and re-derives the
/// cached spread reserves, and [`validate_amm_quote_state`] self-checks
/// the result.
pub fn update_amm_quote_state(
    amm: &mut AMM,
    market_stats: &MarketStats,
    mm_oracle_price_data: &MMOraclePriceData,
    reserve_price: u64,
    slot: u64,
    slot_duration: SlotDuration,
) -> VelocityResult<()> {
    let quote_state = compute_quote_state(
        amm,
        market_stats,
        mm_oracle_price_data,
        reserve_price,
        slot,
        slot_duration,
    )?;
    commit_quote_state(amm, &quote_state, slot)?;
    validate_amm_quote_state(amm)
}

/// Pure snapshot of the quote-time fields a refresh writes onto the AMM.
/// Same idea as `ProjectedAmmState` for the curve projection: compute the
/// full result first, commit it to the account in one place.
struct QuoteState {
    long_spread: u32,
    short_spread: u32,
    reference_price_offset: i32,
    last_oracle_reserve_price_spread_pct: i64,
}

/// Body of [`update_amm_quote_state`]: derive the fresh [`QuoteState`] from
/// the AMM, the market stats, and this slot's oracle, without touching the
/// account.
///
/// # Reference-price-offset smoothing
///
/// When the freshly computed `reference_price_offset` has the opposite sign
/// of `market_stats.last_reference_price_offset` AND
/// `amm.curve_update_intensity > 100`, the transition is smoothed across
/// slots rather than snapping. `market_stats.last_reference_price_offset`
/// is written by the crank after every refresh (from `amm.reference_price_offset`)
/// and seeds the smoothing for the next refresh.
fn compute_quote_state(
    amm: &AMM,
    market_stats: &MarketStats,
    mm_oracle_price_data: &MMOraclePriceData,
    reserve_price: u64,
    slot: u64,
    slot_duration: SlotDuration,
) -> VelocityResult<QuoteState> {
    // last_oracle_reserve_price_spread_pct
    let last_oracle_reserve_price_spread_pct =
        crate::vlp::amm::math::amm::calculate_oracle_reserve_price_spread_pct(
            amm,
            mm_oracle_price_data,
            Some(reserve_price),
        )?;

    // reference_price_offset
    let max_ref_offset = amm.get_max_reference_price_offset()?;

    let reference_price_offset = if max_ref_offset > 0 {
        let liquidity_ratio = calculate_inventory_liquidity_ratio_for_reference_price_offset(
            amm.base_asset_amount_with_amm,
            amm.base_asset_reserve,
            amm.min_base_asset_reserve,
            amm.max_base_asset_reserve,
        )?;

        let signed_liquidity_ratio =
            liquidity_ratio.safe_mul(amm.get_protocol_owned_position()?.signum().cast()?)?;

        let deadband_pct = amm.get_reference_price_offset_deadband_pct()?;
        let liquidity_fraction_after_deadband =
            if signed_liquidity_ratio.unsigned_abs() <= deadband_pct {
                0
            } else {
                signed_liquidity_ratio.safe_sub(
                    deadband_pct
                        .cast::<i128>()?
                        .safe_mul(signed_liquidity_ratio.signum())?,
                )?
            };

        calculate_reference_price_offset(
            reserve_price,
            market_stats.last_24h_avg_funding_rate,
            liquidity_fraction_after_deadband,
            market_stats.min_order_size,
            market_stats
                .historical_oracle_data
                .last_oracle_price_twap_5min,
            market_stats.last_mark_price_twap_5min,
            market_stats.historical_oracle_data.last_oracle_price_twap,
            market_stats.last_mark_price_twap,
            max_ref_offset,
        )?
    } else {
        0
    };

    // long/short spread
    // Steps 1-8 of the pipeline map on `calculate_spread`, behind the
    // curve_update_intensity gate.
    let (mut long_spread, mut short_spread) = if amm.curve_update_intensity > 0 {
        let inputs = SpreadInputs::from_stats(market_stats);
        calculate_spread(
            amm,
            &inputs,
            reserve_price,
            last_oracle_reserve_price_spread_pct,
        )?
    } else {
        let half_base_spread = amm.base_spread.safe_div(2)?;
        (half_base_spread, half_base_spread)
    };

    // 9: x bot knob: the crank-set `amm_spread_adjustment`, post-cap.
    // The admin-set `amm_spread_adjustment` applies to the finished u32
    // spreads. Deliberately NOT shared with SpreadPair::apply_percent_adjustment:
    // this block runs in u32 (so `saturating_mul` saturates at u32::MAX) and
    // has no vol floor, while the in-pipeline adjustment runs in u64 with the
    // base/vol floor. Unifying them would change saturation behavior.
    if amm.amm_spread_adjustment < 0 {
        let adjustment = amm.amm_spread_adjustment.unsigned_abs().cast()?;
        long_spread = long_spread
            .saturating_sub(long_spread.saturating_mul(adjustment).safe_div(100)?)
            .max(1);
        short_spread = short_spread
            .saturating_sub(short_spread.saturating_mul(adjustment).safe_div(100)?)
            .max(1);
    } else if amm.amm_spread_adjustment > 0 {
        let adjustment = amm.amm_spread_adjustment.cast()?;
        long_spread = long_spread
            .saturating_add(long_spread.saturating_mul(adjustment).safe_div_ceil(100)?)
            .max(1);
        short_spread = short_spread
            .saturating_add(short_spread.saturating_mul(adjustment).safe_div_ceil(100)?)
            .max(1);
    }

    // 10: offset: the reference price offset shifts BOTH quotes; on a sign
    // transition it is smoothed here rather than snapped.
    // Mirrors the legacy `update_spreads` smoothing branch (deleted from
    // `controller::amm`). Reads the previous offset from `MarketStats` so
    // there's per-crank continuity even though spread state is no longer
    // cached on `AMM`.
    let last_reference_price_offset = market_stats.last_reference_price_offset;
    let do_reference_price_smooth = last_reference_price_offset.signum()
        != reference_price_offset.signum()
        && amm.curve_update_intensity > 100;

    let final_reference_price_offset = if do_reference_price_smooth {
        // The budget is calibrated per 400ms but accrues in proportion to the
        // elapsed milliseconds, so the smoothing completes over the same wall
        // clock at any slot duration. Counting whole 400ms periods instead would
        // floor to zero for every gap under 400ms, which is what a
        // consecutive-slot crank becomes once slots are faster than that; the
        // step would then pin to the minimum and converge slower the more often
        // the market is cranked.
        let elapsed_ms = Millis::from_slots(
            slot.saturating_sub(amm.last_spread_update_slot),
            slot_duration,
        );
        let reference_price_delta = {
            let full_offset_delta = reference_price_offset
                .cast::<i128>()?
                .saturating_sub(last_reference_price_offset.cast::<i128>()?);
            let budget = elapsed_ms
                .as_ms()
                .cast::<i128>()?
                .safe_mul(REF_PRICE_OFFSET_SMOOTHING_PER_PERIOD_BUDGET)?
                .safe_div(Millis::UNIT.as_ms().cast::<i128>()?)?;
            let raw = full_offset_delta
                .abs()
                .min(budget)
                .safe_div(REF_PRICE_OFFSET_SMOOTHING_STEP_DIVISOR)?
                .cast::<i32>()?;

            full_offset_delta.signum().cast::<i32>()?
                * (raw.max(REF_PRICE_OFFSET_SMOOTHING_MIN_STEP).min(
                    if last_reference_price_offset != 0 {
                        last_reference_price_offset.abs()
                    } else {
                        reference_price_offset.abs()
                    },
                ))
        };

        let smoothed = last_reference_price_offset.safe_add(reference_price_delta)?;

        if reference_price_delta < 0 {
            long_spread = long_spread.safe_add(reference_price_delta.unsigned_abs())?;
            short_spread = short_spread.safe_add(smoothed.unsigned_abs())?;
        } else {
            short_spread = short_spread.safe_add(reference_price_delta.unsigned_abs())?;
            long_spread = long_spread.safe_add(smoothed.unsigned_abs())?;
        }
        smoothed
    } else {
        reference_price_offset
    };

    Ok(QuoteState {
        long_spread,
        short_spread,
        reference_price_offset: final_reference_price_offset,
        last_oracle_reserve_price_spread_pct,
    })
}

/// Write a computed [`QuoteState`] onto the AMM, stamp the refresh slot, and
/// re-derive the cached ask/bid spread reserves from the just-written
/// spreads + current curve reserves. The only place refresh results touch
/// the account.
fn commit_quote_state(amm: &mut AMM, quote_state: &QuoteState, slot: u64) -> VelocityResult<()> {
    amm.long_spread = quote_state.long_spread;
    amm.short_spread = quote_state.short_spread;
    amm.reference_price_offset = quote_state.reference_price_offset;
    amm.last_oracle_reserve_price_spread_pct = quote_state.last_oracle_reserve_price_spread_pct;
    amm.last_spread_update_slot = slot;

    refresh_cached_spread_reserves(amm)
}

/// Recompute the cached ask/bid spread reserves from the AMM's currently-cached
/// `long_spread` / `short_spread` / `reference_price_offset` and its live
/// `base`/`quote` reserves + `sqrt_k`. Restores the legacy `update_spread_reserves`
/// mutator: [`update_amm_quote_state`] runs it after recomputing the spreads, and
/// `QuoterCommit::commit_fill` runs it after a fill moves the reserves so the
/// cached projections (which dashboards read) stay consistent with the curve.
/// Leaves the spreads themselves untouched.
pub(crate) fn refresh_cached_spread_reserves(amm: &mut AMM) -> VelocityResult<()> {
    let (ask_base_asset_reserve, ask_quote_asset_reserve) = compute_spread_reserves_for_direction(
        amm,
        amm.long_spread,
        amm.reference_price_offset,
        PositionDirection::Long,
    )?;
    let (bid_base_asset_reserve, bid_quote_asset_reserve) = compute_spread_reserves_for_direction(
        amm,
        amm.short_spread,
        amm.reference_price_offset,
        PositionDirection::Short,
    )?;

    // Mirror the clamp from the legacy `update_spread_reserves`: with no
    // reference offset, asks stay >= reserve and bids stay <= reserve.
    if amm.reference_price_offset == 0 {
        amm.ask_base_asset_reserve = ask_base_asset_reserve.min(amm.base_asset_reserve);
        amm.ask_quote_asset_reserve = ask_quote_asset_reserve.max(amm.quote_asset_reserve);
        amm.bid_base_asset_reserve = bid_base_asset_reserve.max(amm.base_asset_reserve);
        amm.bid_quote_asset_reserve = bid_quote_asset_reserve.min(amm.quote_asset_reserve);
    } else {
        amm.ask_base_asset_reserve = ask_base_asset_reserve;
        amm.ask_quote_asset_reserve = ask_quote_asset_reserve;
        amm.bid_base_asset_reserve = bid_base_asset_reserve;
        amm.bid_quote_asset_reserve = bid_quote_asset_reserve;
    }
    Ok(())
}

/// Self-check the cached spread/reserve invariants master enforced inside
/// `validate_perp_market`. Run at the tail of [`update_amm_quote_state`] so a
/// corrupted refresh result is caught at the source: the only way bad spread
/// state can reach a fill is through that refresh.
fn validate_amm_quote_state(amm: &AMM) -> VelocityResult<()> {
    // long+short never exceeds the precision ceiling (== 100%).
    validate!(
        amm.long_spread.safe_add(amm.short_spread)?.cast::<u64>()? <= BID_ASK_SPREAD_PRECISION,
        ErrorCode::InvalidAmmDetected,
        "amm long_spread {} + short_spread {} > BID_ASK_SPREAD_PRECISION ({}); max_spread {}",
        amm.long_spread,
        amm.short_spread,
        BID_ASK_SPREAD_PRECISION,
        amm.max_spread,
    )?;

    // When both adjustments are non-negative, the post-spread bid/ask
    // can't be tighter than `base_spread - 2` (the -2 absorbs i32→u32
    // signed rounding from the spread builders).
    if amm.amm_spread_adjustment >= 0 && amm.amm_inventory_spread_adjustment >= 0 {
        validate!(
            amm.long_spread.safe_add(amm.short_spread)? >= amm.base_spread.saturating_sub(2),
            ErrorCode::InvalidAmmDetected,
            "amm long_spread {} + short_spread {} < base_spread {} - 2",
            amm.long_spread,
            amm.short_spread,
            amm.base_spread,
        )?;
    }

    // Spread-reserve bounds, used by `swap_base_asset` to price a fill.
    // `reference_price_offset` direction picks which side's bound is
    // checked (the bound on the side that fills first).
    if amm.reference_price_offset <= 0 {
        validate!(
            amm.bid_base_asset_reserve >= amm.base_asset_reserve
                && amm.bid_quote_asset_reserve <= amm.quote_asset_reserve,
            ErrorCode::InvalidAmmDetected,
            "amm bid reserves invalid: base {} -> {}, quote {} -> {}",
            amm.bid_base_asset_reserve,
            amm.base_asset_reserve,
            amm.bid_quote_asset_reserve,
            amm.quote_asset_reserve,
        )?;
    }
    if amm.reference_price_offset >= 0 {
        validate!(
            amm.ask_base_asset_reserve <= amm.base_asset_reserve
                && amm.ask_quote_asset_reserve >= amm.quote_asset_reserve,
            ErrorCode::InvalidAmmDetected,
            "amm ask reserves invalid: base {} -> {}, quote {} -> {}",
            amm.ask_base_asset_reserve,
            amm.base_asset_reserve,
            amm.ask_quote_asset_reserve,
            amm.quote_asset_reserve,
        )?;
    }

    Ok(())
}

/// The quote side whose fills grow the pool's net position: Long when
/// q > 0, Short when q < 0, None when q == 0 (no side to defend).
fn inventory_increasing_side(base_asset_amount_with_amm: i128) -> Option<PositionDirection> {
    match base_asset_amount_with_amm.cmp(&0) {
        Ordering::Greater => Some(PositionDirection::Long),
        Ordering::Less => Some(PositionDirection::Short),
        Ordering::Equal => None,
    }
}

/// The long/short spread under construction (BID_ASK_SPREAD_PRECISION per
/// side). Owns the side-selection arithmetic so each mechanism in
/// [`calculate_spread`] states which side it acts on exactly once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SpreadPair {
    long: u64,
    short: u64,
}

impl SpreadPair {
    fn total(self) -> VelocityResult<u64> {
        self.long.safe_add(self.short)
    }

    fn component_min(self, other: SpreadPair) -> SpreadPair {
        SpreadPair {
            long: self.long.min(other.long),
            short: self.short.min(other.short),
        }
    }

    fn checked_sub(self, other: SpreadPair) -> VelocityResult<SpreadPair> {
        Ok(SpreadPair {
            long: self.long.safe_sub(other.long)?,
            short: self.short.safe_sub(other.short)?,
        })
    }

    fn positive_delta_from(self, before: SpreadPair) -> SpreadPair {
        SpreadPair {
            long: self.long.saturating_sub(before.long),
            short: self.short.saturating_sub(before.short),
        }
    }

    fn add_pair(&mut self, other: SpreadPair) -> VelocityResult<()> {
        self.long = self.long.safe_add(other.long)?;
        self.short = self.short.safe_add(other.short)?;
        Ok(())
    }

    /// Element-wise max of the half base spread and the per-side vol
    /// spreads: `long = max(w_0/2, v_long)`, `short = max(w_0/2, v_short)`.
    fn from_floors(half_base_spread: u64, vol: (u64, u64)) -> Self {
        Self {
            long: max(half_base_spread, vol.0),
            short: max(half_base_spread, vol.1),
        }
    }

    /// Oracle retreat d: when the reserve price sits below the oracle
    /// (negative divergence pct) the long side widens to at least
    /// `|divergence| + v_long`; above, the short side to
    /// `|divergence| + v_short`. The side facing the divergence never
    /// quotes through the oracle.
    #[allow(clippy::comparison_chain)]
    fn apply_oracle_retreat(
        &mut self,
        last_oracle_reserve_price_spread_pct: i64,
        vol: (u64, u64),
    ) -> VelocityResult<()> {
        if last_oracle_reserve_price_spread_pct < 0 {
            self.long = max(
                self.long,
                last_oracle_reserve_price_spread_pct
                    .unsigned_abs()
                    .safe_add(vol.0)?,
            );
        } else if last_oracle_reserve_price_spread_pct > 0 {
            self.short = max(
                self.short,
                last_oracle_reserve_price_spread_pct
                    .unsigned_abs()
                    .safe_add(vol.1)?,
            );
        }
        Ok(())
    }

    /// Multiply one side by `factor / BID_ASK_SPREAD_PRECISION` (checked
    /// mul, errors on overflow). No-op when `side` is None.
    fn scale(&mut self, side: Option<PositionDirection>, factor: u64) -> VelocityResult<()> {
        match side {
            Some(PositionDirection::Long) => {
                self.long = self
                    .long
                    .safe_mul(factor)?
                    .safe_div(BID_ASK_SPREAD_PRECISION)?;
            }
            Some(PositionDirection::Short) => {
                self.short = self
                    .short
                    .safe_mul(factor)?
                    .safe_div(BID_ASK_SPREAD_PRECISION)?;
            }
            None => {}
        }
        Ok(())
    }

    /// Multiply both sides by `factor / BID_ASK_SPREAD_PRECISION` with a
    /// saturating mul: the empty-fee-cushion branch, which must widen
    /// rather than error at any spread level.
    fn scale_both_saturating(&mut self, factor: u64) -> VelocityResult<()> {
        self.long = self
            .long
            .saturating_mul(factor)
            .safe_div(BID_ASK_SPREAD_PRECISION)?;
        self.short = self
            .short
            .saturating_mul(factor)
            .safe_div(BID_ASK_SPREAD_PRECISION)?;
        Ok(())
    }

    /// Add `on_side` to the given side and `opposite` to the other. Both
    /// sides receive `opposite` when `side` is None.
    fn widen(
        &mut self,
        side: Option<PositionDirection>,
        on_side: u64,
        opposite: u64,
    ) -> VelocityResult<()> {
        match side {
            Some(PositionDirection::Long) => {
                self.long = self.long.safe_add(on_side)?;
                self.short = self.short.safe_add(opposite)?;
            }
            Some(PositionDirection::Short) => {
                self.long = self.long.safe_add(opposite)?;
                self.short = self.short.safe_add(on_side)?;
            }
            None => {
                self.long = self.long.safe_add(opposite)?;
                self.short = self.short.safe_add(opposite)?;
            }
        }
        Ok(())
    }

    /// The signed percentage adjustment shared by both sides
    /// (`amm_inventory_spread_adjustment`): shrink by `|adj|`% (floor
    /// division) or grow by `adj`% (ceiling division), each side floored at
    /// 1 and then at its `floor` counterpart (the base/vol floor), so the
    /// adjustment can never quote tighter than the volatility padding.
    #[allow(clippy::comparison_chain)]
    fn apply_percent_adjustment(
        &mut self,
        adjustment: i8,
        floor: SpreadPair,
    ) -> VelocityResult<()> {
        if adjustment < 0 {
            let adjustment: u64 = adjustment.cast::<i64>()?.unsigned_abs();
            self.long = floor.long.max(
                self.long
                    .saturating_sub(self.long.saturating_mul(adjustment).safe_div(100)?)
                    .max(1),
            );
            self.short = floor.short.max(
                self.short
                    .saturating_sub(self.short.saturating_mul(adjustment).safe_div(100)?)
                    .max(1),
            );
        } else if adjustment > 0 {
            let adjustment: u64 = adjustment.cast()?;
            self.long = floor.long.max(
                self.long
                    .saturating_add(self.long.saturating_mul(adjustment).safe_div_ceil(100)?)
                    .max(1),
            );
            self.short = floor.short.max(
                self.short
                    .saturating_add(self.short.saturating_mul(adjustment).safe_div_ceil(100)?)
                    .max(1),
            );
        }
        Ok(())
    }

    /// Cap the combined total at `max_total` via [`cap_to_max_spread`]:
    /// when over, the larger side is scaled to `max_total / total`
    /// (ceiling) and the smaller side takes the remainder.
    fn cap_total(self, max_total: u64) -> VelocityResult<SpreadPair> {
        let (long, short) = cap_to_max_spread(self.long, self.short, max_total)?;
        Ok(SpreadPair { long, short })
    }
}

/// The final raw spread split by safety priority. Components sum exactly to
/// `raw`: the known oracle gap has first claim on the ceiling, the minimum
/// base/vol floor has second claim, directional inventory steering has third
/// claim, and the residual common padding yields first when the quote is over
/// budget.
#[derive(Clone, Copy)]
struct SpreadComponents {
    // Tier 1: known oracle/vAMM mispricing. This receives ceiling room first
    // and is compressed only when the divergence alone exceeds the ceiling.
    divergence: SpreadPair,

    // Tier 2: the minimum quote cushion, max(base / 2, volatility), per side.
    // Keeping this ahead of steering prevents a saturated directional signal
    // from quoting the healing side exactly at mid.
    floor: SpreadPair,

    // Tier 3: directional widening that discourages inventory-growing flow.
    // This receives whatever room remains after divergence protection.
    steering: SpreadPair,

    // Tier 4: common protection above the minimum floor.
    // This is the first layer sacrificed when the total quote is over budget.
    padding: SpreadPair,
}

impl SpreadComponents {
    /// Reconcile the recorded mechanism requirements against the final raw
    /// spread. This keeps every uncapped quote byte-identical even when a
    /// negative admin adjustment has already reduced the raw pair: divergence
    /// claims what remains first, the minimum base/vol floor next, steering
    /// after that, and padding is the residual.
    fn from_raw(
        raw: SpreadPair,
        divergence_required: SpreadPair,
        floor_required: SpreadPair,
        steering_added: SpreadPair,
    ) -> VelocityResult<Self> {
        let divergence = raw.component_min(divergence_required);
        let after_divergence = raw.checked_sub(divergence)?;
        let floor = after_divergence.component_min(floor_required);
        let after_floor = after_divergence.checked_sub(floor)?;
        let steering = after_floor.component_min(steering_added);
        let padding = after_floor.checked_sub(steering)?;

        Ok(Self {
            divergence,
            floor,
            steering,
            padding,
        })
    }

    fn raw(self) -> VelocityResult<SpreadPair> {
        let mut raw = self.divergence;
        raw.add_pair(self.floor)?;
        raw.add_pair(self.steering)?;
        raw.add_pair(self.padding)?;
        Ok(raw)
    }

    /// Cap without mixing safety classes. A layer that only partly fits is
    /// compressed proportionally within that one class; lower-priority layers
    /// receive no room. The dynamic ceiling itself is deliberately unchanged.
    fn cap_total_ordered(self, max_total: u64) -> VelocityResult<SpreadPair> {
        let raw = self.raw()?;
        if raw.total()? <= max_total {
            return Ok(raw);
        }

        let mut result = SpreadPair::default();
        let mut remaining = max_total;

        // Safety order is intentional:
        //   1. Divergence protection keeps its room first.
        //   2. The per-side base/vol floor keeps quotes away from mid.
        //   3. Inventory steering keeps the remaining room next.
        //   4. Common padding above the floor receives only leftover room.
        // If a tier only partly fits, it is compressed within that tier and
        // every lower-priority tier receives zero.
        for layer in [self.divergence, self.floor, self.steering, self.padding] {
            if remaining == 0 {
                break;
            }

            let allocated = layer.cap_total(remaining)?;
            result.add_pair(allocated)?;
            remaining = remaining.safe_sub(allocated.total()?)?;
        }

        validate!(
            result.total()? <= max_total,
            ErrorCode::InvalidAmmMaxSpreadDetected,
            "ordered spread total({}) > max_spread({})",
            result.total()?,
            max_total,
        )?;

        Ok(result)
    }
}

/// Pure known-mispricing requirement, excluding statistical padding. The
/// latter stays in the lowest-priority bucket so stress cannot let volatility
/// crowd directional steering out of the quote.
fn divergence_requirement(last_oracle_reserve_price_spread_pct: i64) -> SpreadPair {
    if last_oracle_reserve_price_spread_pct < 0 {
        SpreadPair {
            long: last_oracle_reserve_price_spread_pct.unsigned_abs(),
            short: 0,
        }
    } else if last_oracle_reserve_price_spread_pct > 0 {
        SpreadPair {
            long: 0,
            short: last_oracle_reserve_price_spread_pct.unsigned_abs(),
        }
    } else {
        SpreadPair::default()
    }
}

/// `MarketStats` scalars the spread math reads, copied out once per refresh.
/// Same shape as `ProjectionInputs` in `repeg.rs`: in the future CPI
/// architecture these are what Velocity sends into the AMM-program call, so
/// the spread math never holds a `&MarketStats`.
#[derive(Debug, Clone, Copy, Default)]
struct SpreadInputs {
    pub last_oracle_conf_pct: u64,
    pub mark_std: u64,
    pub oracle_std: u64,
    pub long_intensity_volume: u64,
    pub short_intensity_volume: u64,
    pub volume_24h: u64,
    pub last_24h_avg_funding_rate: i64,
    pub last_funding_oracle_twap: i64,
}

impl SpreadInputs {
    fn from_stats(stats: &MarketStats) -> Self {
        Self {
            last_oracle_conf_pct: stats.last_oracle_conf_pct,
            mark_std: stats.mark_std,
            oracle_std: stats.oracle_std,
            long_intensity_volume: stats.long_intensity_volume,
            short_intensity_volume: stats.short_intensity_volume,
            volume_24h: stats.volume_24h,
            last_24h_avg_funding_rate: stats.last_24h_avg_funding_rate,
            last_funding_oracle_twap: stats.last_funding_oracle_twap,
        }
    }
}

/// Build the two-sided spread for one refresh. The map, where "loaded side"
/// is the side whose fills grow the pool's net position
/// ([`inventory_increasing_side`]):
///
/// ```text
/// gate: curve_update_intensity > 0, else flat base/2 per side (caller)
/// 1  vol floor        max(base/2, vol x side intensity), per side
/// 2  oracle retreat   loaded side floored at |reserve-oracle gap| + vol
/// 3  x sigma(q)       inventory scale, loaded side only
/// 4  x lambda(q)      leverage scale vs fee cushion, loaded side only
///                     [fee cushion <= 0: BOTH sides x 10 instead]
/// 5  + r              revenue retreat: full on loaded, half on other
/// 6  x beta(f)        funding lean, loaded side, paying regime only
/// 7  x tilt gain      amm_inventory_spread_adjustment (floored at base/vol)
/// 8  CAP              total <= dynamic ceiling; divergence, base/vol floor,
///                     steering, then common padding
/// -- caller (compute_quote_state), post-cap --
/// 9  x bot knob       amm_spread_adjustment (crank's actuator)
/// 10 offset           reference_price_offset shifts BOTH quotes
/// ```
///
/// Each step's arithmetic lives in its component function; every component
/// survives to the cap as a named value.
fn calculate_spread(
    amm: &AMM,
    inputs: &SpreadInputs,
    reserve_price: u64,
    last_oracle_reserve_price_spread_pct: i64,
) -> VelocityResult<(u32, u32)> {
    // 1: vol floor: per-side statistical padding v.
    let vol = calculate_long_short_vol_spread(
        inputs.last_oracle_conf_pct,
        reserve_price,
        inputs.mark_std,
        inputs.oracle_std,
        inputs.long_intensity_volume,
        inputs.short_intensity_volume,
        inputs.volume_24h,
    )?;

    // 1: max(base/2, v) per side, the pre-skew quote.
    let half_base_spread = (amm.base_spread / 2) as u64;
    let floors = SpreadPair::from_floors(half_base_spread, vol);
    let mut spread = floors;
    let mut steering_added = SpreadPair::default();

    // w_max for step 8: dynamic ceiling; divergence and vol can raise it
    // above the admin max_spread.
    let max_target_spread = calculate_max_target_spread(
        last_oracle_reserve_price_spread_pct,
        reserve_price,
        inputs.last_oracle_conf_pct,
        inputs.mark_std,
        inputs.oracle_std,
        amm.max_spread,
    )?;

    // 2: oracle retreat: the side facing the divergence floors at
    // |gap| + v.
    spread.apply_oracle_retreat(last_oracle_reserve_price_spread_pct, vol)?;

    // 3: x sigma(q): inventory scale, loaded side only.
    let side = inventory_increasing_side(amm.base_asset_amount_with_amm);
    let directional_spread = match side {
        Some(PositionDirection::Long) => spread.long,
        _ => spread.short,
    };
    let inventory_scale = calculate_spread_inventory_scale(
        amm.base_asset_amount_with_amm,
        amm.base_asset_reserve,
        amm.min_base_asset_reserve,
        amm.max_base_asset_reserve,
        directional_spread,
        max_target_spread,
    )?;
    let before_inventory_scale = spread;
    spread.scale(side, inventory_scale)?;
    steering_added.add_pair(spread.positive_delta_from(before_inventory_scale))?;

    if amm.total_fee_minus_distributions <= 0 {
        // 4, empty-cushion branch: BOTH sides x 10, bounded by step 8.
        spread.scale_both_saturating(DEFAULT_LARGE_BID_ASK_FACTOR)?;
    } else {
        // 4: x lambda(q): leverage scale vs fee cushion, loaded side only.
        let leverage_scale = calculate_spread_leverage_scale(
            amm.quote_asset_reserve,
            amm.terminal_quote_asset_reserve,
            amm.peg_multiplier,
            amm.base_asset_amount_with_amm,
            reserve_price,
            amm.total_fee_minus_distributions,
        )?;
        let before_leverage_scale = spread;
        spread.scale(side, leverage_scale)?;
        steering_added.add_pair(spread.positive_delta_from(before_leverage_scale))?;
    }

    // 5: + r: revenue retreat, full on loaded side, half on the other
    // (both halves when q == 0).
    let revenue_retreat = calculate_spread_revenue_retreat_amount(
        amm.base_spread,
        max_target_spread,
        amm.net_revenue_since_last_funding,
    )?;
    if revenue_retreat != 0 {
        let common_retreat = revenue_retreat.safe_div(2)?;
        spread.widen(side, revenue_retreat, common_retreat)?;

        // The common half is generic protection; only the extra loaded-side
        // half contributes to the flow-steering difference.
        let directional_retreat = revenue_retreat.safe_sub(common_retreat)?;
        match side {
            Some(PositionDirection::Long) => {
                steering_added.long = steering_added.long.safe_add(directional_retreat)?;
            }
            Some(PositionDirection::Short) => {
                steering_added.short = steering_added.short.safe_add(directional_retreat)?;
            }
            None => {}
        }
    }

    // 6: x beta(f): funding lean, loaded side, paying regime only.
    // beta == 1 when the vAMM receives, so the guard keeps the multiply
    // off the no-op path.
    let funding_bias_scale = calculate_spread_funding_bias_scale(
        amm.base_asset_amount_with_amm,
        inputs.last_24h_avg_funding_rate,
        inputs.last_funding_oracle_twap,
        amm.funding_bias_sensitivity,
    )?;
    if funding_bias_scale > BID_ASK_SPREAD_PRECISION {
        let before_funding_bias = spread;
        spread.scale(side, funding_bias_scale)?;
        steering_added.add_pair(spread.positive_delta_from(before_funding_bias))?;
    }

    // 7: x tilt gain: admin per-market adjustment, floored at base/vol.
    spread.apply_percent_adjustment(amm.amm_inventory_spread_adjustment, floors)?;

    // 8: CAP: keep the existing dynamic ceiling but stop statistically-wide
    // padding from proportionally squeezing the directional signal. The
    // empty-cushion 10x branch retains its legacy proportional behavior until
    // its separate graded redesign (design issue 3).
    let capped = if amm.total_fee_minus_distributions <= 0 {
        spread.cap_total(max_target_spread)?
    } else {
        SpreadComponents::from_raw(
            spread,
            divergence_requirement(last_oracle_reserve_price_spread_pct),
            floors,
            steering_added,
        )?
        .cap_total_ordered(max_target_spread)?
    };

    Ok((capped.long.cast::<u32>()?, capped.short.cast::<u32>()?))
}

/// Proportionally squeeze a two-sided spread whose total exceeds
/// `max_spread`: the larger side is scaled to `max_spread / total` with a
/// ceiling division, the smaller side takes the remainder, so
/// `long + short == max_spread` exactly after a squeeze. Errors if the
/// result still exceeds the cap.
pub fn cap_to_max_spread(
    mut long_spread: u64,
    mut short_spread: u64,
    max_spread: u64,
) -> VelocityResult<(u64, u64)> {
    let total_spread = long_spread.safe_add(short_spread)?;

    if total_spread > max_spread {
        if long_spread > short_spread {
            long_spread = long_spread
                .saturating_mul(max_spread)
                .safe_div_ceil(total_spread)?;
            short_spread = max_spread.safe_sub(long_spread)?;
        } else {
            short_spread = short_spread
                .saturating_mul(max_spread)
                .safe_div_ceil(total_spread)?;
            long_spread = max_spread.safe_sub(short_spread)?;
        }
    }

    let new_total_spread = long_spread.safe_add(short_spread)?;

    validate!(
        new_total_spread <= max_spread,
        ErrorCode::InvalidAmmMaxSpreadDetected,
        "new_total_spread({}) > max_spread({})",
        new_total_spread,
        max_spread
    )?;

    Ok((long_spread, short_spread))
}

/// Per-side vol spread v (BID_ASK_SPREAD_PRECISION): the statistical
/// padding each side starts from.
///
///   s   = (oracle_std + mark_std) / (2 * reserve_price)   (PERCENTAGE_PRECISION)
///   b   = max(conf, s / 4)                                 (vol base)
///   g_i = clamp(intensity_i / volume_24h, 0.01, 1)         (per-side factor)
///   c   = conf            when conf > 25 bp
///       = conf / 20       otherwise
///   v_i = max(c, b * g_i)
///
/// `conf` is `last_oracle_conf_pct` (PERCENTAGE_PRECISION of price). The
/// 25 bp threshold is `SPREAD_CONF_FULL_WEIGHT_THRESHOLD`; below it the
/// confidence contribution is discounted 20x (a step, not a ramp; see
/// design issue 5). `volume_24h` is floored at 1 so the factors are
/// defined on a fresh market.
fn calculate_long_short_vol_spread(
    last_oracle_conf_pct: u64,
    reserve_price: u64,
    mark_std: u64,
    oracle_std: u64,
    long_intensity_volume: u64,
    short_intensity_volume: u64,
    volume_24h: u64,
) -> VelocityResult<(u64, u64)> {
    // 1.6 * std
    let market_avg_std_pct: u128 = oracle_std
        .safe_add(mark_std)?
        .cast::<u128>()?
        .safe_mul(PERCENTAGE_PRECISION)?
        .safe_div(reserve_price.cast::<u128>()?)?
        .safe_div(2)?;

    let vol_spread: u128 = last_oracle_conf_pct
        .cast::<u128>()?
        .max(market_avg_std_pct.safe_div(SPREAD_VOL_STD_DISCOUNT_DIVISOR)?);

    let factor_clamp_min: u128 = PERCENTAGE_PRECISION / 100; // .01
    let factor_clamp_max: u128 = PERCENTAGE_PRECISION; // 1

    // g_i: the side's share of 24h volume, clamped to [0.01, 1].
    let intensity_factor = |intensity_volume: u64| -> VelocityResult<u128> {
        Ok(intensity_volume
            .cast::<u128>()?
            .safe_mul(PERCENTAGE_PRECISION)?
            .safe_div(max(volume_24h.cast::<u128>()?, 1))?
            .clamp(factor_clamp_min, factor_clamp_max))
    };
    let long_vol_spread_factor = intensity_factor(long_intensity_volume)?;
    let short_vol_spread_factor = intensity_factor(short_intensity_volume)?;

    // only consider confidence interval at full value when above 25 bps
    let conf_component = if last_oracle_conf_pct > SPREAD_CONF_FULL_WEIGHT_THRESHOLD {
        last_oracle_conf_pct
    } else {
        last_oracle_conf_pct.safe_div(SPREAD_CONF_DISCOUNT_DIVISOR)?
    };

    // v_i = max(c, b * g_i)
    let side_vol_spread = |factor: u128| -> VelocityResult<u64> {
        Ok(max(
            conf_component,
            vol_spread
                .safe_mul(factor)?
                .safe_div(PERCENTAGE_PRECISION)?
                .cast::<u64>()?,
        ))
    };

    Ok((
        side_vol_spread(long_vol_spread_factor)?,
        side_vol_spread(short_vol_spread_factor)?,
    ))
}

/// Which side of the AMM's open liquidity the inventory ratio is measured
/// against.
enum LiquidityBasis {
    /// The smaller of open bids and open asks; the spread inventory scale,
    /// which must saturate before the thin side runs out.
    MinSide,
    /// The average of open bids and open asks; the reference price offset.
    Average,
}

/// Inventory-to-liquidity ratio x (PERCENTAGE_PRECISION, unsigned):
///
///   x = min(1, |q| / L)
///
/// with L the open liquidity picked by `basis` (floored at 1 so the
/// division is defined). Saturates at 1 when |q| reaches L. The overflow
/// escape (`unwrap_or(i128::MAX)`) keeps the ratio at its saturated value
/// for absurd |q| instead of erroring.
fn inventory_liquidity_ratio(
    base_asset_amount_with_amm: i128,
    base_asset_reserve: u128,
    min_base_asset_reserve: u128,
    max_base_asset_reserve: u128,
    basis: LiquidityBasis,
) -> VelocityResult<i128> {
    let (max_bids, max_asks) = _calculate_market_open_bids_asks(
        base_asset_reserve,
        min_base_asset_reserve,
        max_base_asset_reserve,
    )?;

    let liquidity = match basis {
        LiquidityBasis::MinSide => max_bids.min(max_asks.abs()),
        LiquidityBasis::Average => (max_bids.safe_add(max_asks.abs())?).safe_div(2)?,
    };

    let amm_inventory_pct = if base_asset_amount_with_amm.abs() < liquidity {
        base_asset_amount_with_amm
            .abs()
            .safe_mul(PERCENTAGE_PRECISION_I128)
            .unwrap_or(i128::MAX)
            .safe_div(liquidity.max(1))?
            .min(PERCENTAGE_PRECISION_I128)
    } else {
        PERCENTAGE_PRECISION_I128 // 100%
    };

    Ok(amm_inventory_pct)
}

/// [`inventory_liquidity_ratio`] against the min-side open liquidity,
/// the basis the spread inventory scale uses.
pub(crate) fn calculate_inventory_liquidity_ratio(
    base_asset_amount_with_amm: i128,
    base_asset_reserve: u128,
    min_base_asset_reserve: u128,
    max_base_asset_reserve: u128,
) -> VelocityResult<i128> {
    inventory_liquidity_ratio(
        base_asset_amount_with_amm,
        base_asset_reserve,
        min_base_asset_reserve,
        max_base_asset_reserve,
        LiquidityBasis::MinSide,
    )
}

/// [`inventory_liquidity_ratio`] against the average open liquidity,
/// the basis the reference price offset uses.
pub(crate) fn calculate_inventory_liquidity_ratio_for_reference_price_offset(
    base_asset_amount_with_amm: i128,
    base_asset_reserve: u128,
    min_base_asset_reserve: u128,
    max_base_asset_reserve: u128,
) -> VelocityResult<i128> {
    inventory_liquidity_ratio(
        base_asset_amount_with_amm,
        base_asset_reserve,
        min_base_asset_reserve,
        max_base_asset_reserve,
        LiquidityBasis::Average,
    )
}

/// Inventory scale sigma(q) (BID_ASK_SPREAD_PRECISION): multiplier for the
/// spread on the inventory-increasing side as the pool's position consumes
/// its open liquidity.
///
///   x         = min(1, |q| / L)          (min-side liquidity, PERCENTAGE_PRECISION)
///   sigma_max = max(10, w_max / w_dir)   (w_dir = current spread on that side, floored at 1)
///   sigma(q)  = min(sigma_max, 1 + sigma_max * x)
///
/// sigma is in [1, sigma_max]; at x = 1 the scaled side reaches w_max
/// exactly. Returns 1 when q == 0. The intermediate products saturate to
/// the cap instead of erroring.
fn calculate_spread_inventory_scale(
    base_asset_amount_with_amm: i128,
    base_asset_reserve: u128,
    min_base_asset_reserve: u128,
    max_base_asset_reserve: u128,
    directional_spread: u64,
    max_spread: u64,
) -> VelocityResult<u64> {
    if base_asset_amount_with_amm == 0 {
        return Ok(BID_ASK_SPREAD_PRECISION);
    }

    let amm_inventory_pct = calculate_inventory_liquidity_ratio(
        base_asset_amount_with_amm,
        base_asset_reserve,
        min_base_asset_reserve,
        max_base_asset_reserve,
    )?;

    // only allow up to scale up of larger of MAX_BID_ASK_INVENTORY_SKEW_FACTOR or max spread
    let inventory_scale_max = MAX_BID_ASK_INVENTORY_SKEW_FACTOR.max(
        max_spread
            .safe_mul(BID_ASK_SPREAD_PRECISION)?
            .safe_div(max(directional_spread, 1))?,
    );

    let inventory_scale_capped = min(
        inventory_scale_max,
        BID_ASK_SPREAD_PRECISION
            .safe_add(
                inventory_scale_max
                    .safe_mul(amm_inventory_pct.unsigned_abs().cast()?)
                    .unwrap_or(u64::MAX)
                    .safe_div(PERCENTAGE_PRECISION_I128.cast()?)?,
            )
            .unwrap_or(u64::MAX),
    );

    Ok(inventory_scale_capped)
}

/// Effective leverage scale lambda(q) (BID_ASK_SPREAD_PRECISION):
/// multiplier for the inventory-increasing side as the pool's local
/// exposure outgrows its fee cushion.
///
///   E      = local base value - net base value          (QUOTE_PRECISION)
///   lambda = min(10, 1 + max(0, E) / (max(0, tfmd) + 1))
///
/// `tfmd` is `total_fee_minus_distributions`; the +1 keeps the divisor
/// positive, so lambda saturates toward its 10x cap as the cushion thins.
/// Callers only reach this with tfmd > 0; at tfmd <= 0 the pipeline takes
/// the `DEFAULT_LARGE_BID_ASK_FACTOR` branch on both sides instead.
fn calculate_spread_leverage_scale(
    quote_asset_reserve: u128,
    terminal_quote_asset_reserve: u128,
    peg_multiplier: u128,
    base_asset_amount_with_amm: i128,
    reserve_price: u64,
    total_fee_minus_distributions: i128,
) -> VelocityResult<u64> {
    let net_base_asset_value = quote_asset_reserve
        .cast::<i128>()?
        .safe_sub(terminal_quote_asset_reserve.cast::<i128>()?)?
        .safe_mul(peg_multiplier.cast::<i128>()?)?
        .safe_div(AMM_TIMES_PEG_TO_QUOTE_PRECISION_RATIO_I128)?;

    let local_base_asset_value = base_asset_amount_with_amm
        .safe_mul(reserve_price.cast::<i128>()?)?
        .safe_div(AMM_TO_QUOTE_PRECISION_RATIO_I128 * PRICE_PRECISION_I128)?;

    let effective_leverage = max(0, local_base_asset_value.safe_sub(net_base_asset_value)?)
        .safe_mul(BID_ASK_SPREAD_PRECISION_I128)?
        .safe_div(max(0, total_fee_minus_distributions) + 1)?;

    let effective_leverage_capped = min(
        MAX_BID_ASK_INVENTORY_SKEW_FACTOR,
        BID_ASK_SPREAD_PRECISION.safe_add(max(0, effective_leverage).cast::<u64>()? + 1)?,
    );

    Ok(effective_leverage_capped)
}

/// Revenue retreat r (BID_ASK_SPREAD_PRECISION, additive): defensive
/// widening while the market's revenue since the last funding update is
/// below the retreat threshold.
///
///   r = 0                                    when rev >= threshold
///   r = min(w_max / 10, w_0 * |rev| / |threshold|)
///                                            when threshold*1000 <= rev < threshold
///   r = w_max / 10                           when rev < threshold*1000
///
/// `threshold` is `DEFAULT_REVENUE_SINCE_LAST_FUNDING_SPREAD_RETREAT`
/// (negative, -$25); `w_max / 10` is the retreat cap
/// (`SPREAD_REVENUE_RETREAT_MAX_DIVISOR`). The caller puts the full r on
/// the inventory-increasing side and r/2 on the other.
fn calculate_spread_revenue_retreat_amount(
    base_spread: u32,
    max_spread: u64,
    net_revenue_since_last_funding: i64,
) -> VelocityResult<u64> {
    // on-the-hour revenue scale
    let revenue_retreat_amount = if net_revenue_since_last_funding
        < DEFAULT_REVENUE_SINCE_LAST_FUNDING_SPREAD_RETREAT
    {
        let max_retreat = max_spread.safe_div(SPREAD_REVENUE_RETREAT_MAX_DIVISOR)?;
        if net_revenue_since_last_funding
            >= DEFAULT_REVENUE_SINCE_LAST_FUNDING_SPREAD_RETREAT * 1000
        {
            min(
                max_retreat,
                base_spread
                    .cast::<u64>()?
                    .safe_mul(net_revenue_since_last_funding.unsigned_abs())?
                    .safe_div(DEFAULT_REVENUE_SINCE_LAST_FUNDING_SPREAD_RETREAT.unsigned_abs())?,
            )
        } else {
            max_retreat
        }
    } else {
        0
    };

    Ok(revenue_retreat_amount)
}

/// Target ceiling w_max (BID_ASK_SPREAD_PRECISION) for the cap step:
///
///   w_max = max(max_spread, |divergence|, min(max(2*conf, std_pct), 100%))
///
/// The admin `max_spread` is a floor of the ceiling, not a maximum: the
/// oracle divergence and the vol baseline can raise it under stress. The
/// vol/conf term is clipped at 100%; the divergence term is not (design
/// issue 2).
pub(crate) fn calculate_max_target_spread(
    last_oracle_reserve_price_spread_pct: i64,
    reserve_price: u64,
    last_oracle_conf_pct: u64,
    mark_std: u64,
    oracle_std: u64,
    max_spread: u32,
) -> VelocityResult<u64> {
    let max_spread_baseline = last_oracle_reserve_price_spread_pct.unsigned_abs().max(
        last_oracle_conf_pct
            .safe_mul(2)?
            .max(
                mark_std
                    .max(oracle_std)
                    .safe_mul(crate::math::constants::PERCENTAGE_PRECISION_U64)?
                    .safe_div(reserve_price)?,
            )
            .min(BID_ASK_SPREAD_PRECISION),
    );

    let max_target_spread = max_spread.cast::<u64>()?.max(max_spread_baseline);
    Ok(max_target_spread)
}

/// Funding bias β(f) (BID_ASK_SPREAD_PRECISION): bounded multiplier for the
/// paying-side spread while the vAMM is paying funding.
///
/// q = net user position the vAMM faces (`base_asset_amount_with_amm`),
/// f = 24h avg funding rate normalized to a daily fraction of the oracle
/// twap captured at the last funding update (FUNDING_RATE_PRECISION), the
/// same twap the rate accrued against. f carries the funding offset, so f = 0 is
/// the paying/receiving zero-crossing, not zero premium. The vAMM pays when
/// f * q < 0: f > 0 with q < 0 (vAMM long), or f < 0 with q > 0 (vAMM short).
///
///   ρ(f) = clamp(|f| / f_ref, 0, 1),  f_ref = FUNDING_RATE_OFFSET_PERCENTAGE
///   β(f) = 1 + s * ρ(f),              s = funding_bias_sensitivity / 100
///
/// β ∈ [1, 1 + s], saturating at f_ref (~10.95%/yr, the offset floor), so in
/// the common ρ = 1 regime the paying side widens by exactly 1 + s. β depends
/// on f, not |q|: at low inventory (σ ≈ 1) it dominates and deters the first
/// adverse trades, then σ takes over as |q| grows. Returns 1 when the vAMM
/// receives funding or s = 0.
fn calculate_spread_funding_bias_scale(
    base_asset_amount_with_amm: i128,
    last_24h_avg_funding_rate: i64,
    last_funding_oracle_twap: i64,
    funding_bias_sensitivity: u8,
) -> VelocityResult<u64> {
    if funding_bias_sensitivity == 0 || last_funding_oracle_twap <= 0 {
        return Ok(BID_ASK_SPREAD_PRECISION);
    }

    // f: daily funding rate as a fraction of price, FUNDING_RATE_PRECISION
    let f_norm = last_24h_avg_funding_rate
        .cast::<i128>()?
        .safe_mul(PRICE_PRECISION_I128)?
        .safe_div(last_funding_oracle_twap.cast::<i128>()?)?
        .safe_mul(24)?;

    // f * q >= 0: vAMM receives (or rate/inventory is zero), β = 1
    if f_norm.signum() * base_asset_amount_with_amm.signum() >= 0 {
        return Ok(BID_ASK_SPREAD_PRECISION);
    }

    // ρ = clamp(|f| / f_ref, 0, 1), PERCENTAGE_PRECISION
    let ramp = f_norm
        .unsigned_abs()
        .safe_mul(PERCENTAGE_PRECISION)?
        .safe_div(FUNDING_RATE_OFFSET_PERCENTAGE.cast::<u128>()?)?
        .min(PERCENTAGE_PRECISION);

    // β = 1 + s * ρ
    BID_ASK_SPREAD_PRECISION.safe_add(
        funding_bias_sensitivity
            .cast::<u64>()?
            .safe_mul(ramp.cast::<u64>()?)?
            .safe_div(100)?,
    )
}

/// Reference price offset (PERCENTAGE_PRECISION of price, signed): shift of
/// the quote midpoint toward the measured market premium, applied only when
/// the premium's sign agrees with the inventory's.
///
/// Three premium estimates (the 5min mark-oracle twap gap, the slower twap
/// gap, and the 24h funding rate converted to a quote premium) are each
/// clamped to `max_offset_pct` of price and averaged. The average (as a pct
/// of price) is scaled by `|liquidity_fraction| / 2` and clamped to
/// `±max_offset_pct`. Returns 0 when the 24h funding rate or the liquidity
/// fraction is zero, or when premium and inventory disagree in sign.
#[allow(clippy::comparison_chain)]
pub(crate) fn calculate_reference_price_offset(
    reserve_price: u64,
    last_24h_avg_funding_rate: i64,
    liquidity_fraction: i128,
    _min_order_size: u64,
    oracle_twap_fast: i64,
    mark_twap_fast: u64,
    oracle_twap_slow: i64,
    mark_twap_slow: u64,
    max_offset_pct: i64,
) -> VelocityResult<i32> {
    if last_24h_avg_funding_rate == 0 || liquidity_fraction == 0 {
        return Ok(0);
    }

    let max_offset_in_price = max_offset_pct
        .safe_mul(reserve_price.cast()?)?
        .safe_div(PERCENTAGE_PRECISION.cast()?)?;

    // calculate quote denominated market premium
    let mark_premium_minute: i64 = mark_twap_fast
        .cast::<i64>()?
        .safe_sub(oracle_twap_fast)?
        .clamp(-max_offset_in_price, max_offset_in_price);
    let mark_premium_hour: i64 = mark_twap_slow
        .cast::<i64>()?
        .safe_sub(oracle_twap_slow)?
        .clamp(-max_offset_in_price, max_offset_in_price);
    // convert last_24h_avg_funding_rate to quote denominated premium
    let mark_premium_day: i64 = last_24h_avg_funding_rate
        .safe_div(FUNDING_RATE_BUFFER.cast()?)?
        .safe_mul(24)?
        .safe_sub(
            oracle_twap_slow
                .abs()
                .safe_div(FUNDING_RATE_OFFSET_DENOMINATOR)?,
        )?
        .clamp(-max_offset_in_price, max_offset_in_price); // todo: look at how 24h funding is calc w.r.t. the funding_period
                                                           // take average clamped premium as the price-based offset
    let mark_premium_avg = mark_premium_minute
        .safe_add(mark_premium_hour)?
        .safe_add(mark_premium_day)?
        .safe_div(3_i64)?;

    let mark_premium_avg_pct: i64 = mark_premium_avg
        .safe_mul(PRICE_PRECISION_I64)?
        .safe_div(reserve_price.cast()?)?;

    // only apply when inventory is consistent with recent and 24h market premium
    let offset_pct = if (mark_premium_avg_pct >= 0 && liquidity_fraction >= 0)
        || (mark_premium_avg_pct <= 0 && liquidity_fraction <= 0)
    {
        mark_premium_avg_pct
            .safe_mul(liquidity_fraction.unsigned_abs().cast::<i64>()?)?
            .safe_div(2)?
    } else {
        0
    };

    let clamped_offset_pct = offset_pct.clamp(-max_offset_pct, max_offset_pct);

    validate!(
        clamped_offset_pct.abs() <= max_offset_pct,
        ErrorCode::InvalidAmmDetected,
        "clamp offset pct failed {}",
        clamped_offset_pct
    )?;

    clamped_offset_pct.cast()
}

/// Pure form of the legacy `calculate_spread_reserves` mutator: takes the
/// spread + reference offset directly instead of reading cached AMM fields
/// (which were removed in the AMM-decoupling refactor).
///
/// The signed half-spread `s = ±spread + offset` moves the quote reserve by
/// `quote / floor(BID_ASK_SPREAD_PRECISION / (s / 2))` (the divisor
/// quantization is the legacy behavior, see design issue 6) and the base
/// reserve follows from the invariant `k = sqrt_k^2`.
pub(crate) fn compute_spread_reserves_for_direction(
    amm: &AMM,
    spread: u32,
    reference_price_offset: i32,
    direction: PositionDirection,
) -> VelocityResult<(u128, u128)> {
    let spread_with_offset: i32 = if direction == PositionDirection::Short {
        (-spread.cast::<i32>()?).safe_add(reference_price_offset)?
    } else {
        spread.cast::<i32>()?.safe_add(reference_price_offset)?
    };

    let quote_asset_reserve_delta = if spread_with_offset.abs() > 1 {
        let quote_reserve_divisor =
            BID_ASK_SPREAD_PRECISION_I128 / (spread_with_offset / 2).cast::<i128>()?;
        amm.quote_asset_reserve
            .cast::<i128>()?
            .safe_div(quote_reserve_divisor)?
    } else {
        0_i128
    };

    let quote_asset_reserve = if quote_asset_reserve_delta > 0 {
        amm.quote_asset_reserve
            .safe_add(quote_asset_reserve_delta.unsigned_abs())?
    } else {
        amm.quote_asset_reserve
            .safe_sub(quote_asset_reserve_delta.unsigned_abs())?
    };

    let base_asset_reserve = k_invariant(amm.sqrt_k)?
        .safe_div(U192::from(quote_asset_reserve))?
        .try_to_u128()?;

    Ok((base_asset_reserve, quote_asset_reserve))
}

/// The curve invariant k = sqrt_k^2 as U192, which both reserve
/// derivations divide by a quote reserve.
fn k_invariant(sqrt_k: u128) -> VelocityResult<U192> {
    let invariant_sqrt_u192 = U192::from(sqrt_k);
    invariant_sqrt_u192.safe_mul(invariant_sqrt_u192)
}

/// Size the largest trade that moves the AMM's reserve price to
/// `limit_price`, measured from the spread-adjusted ask/bid reserves (the
/// same basis the swap actually executes against) and return it with the
/// direction that moves the price toward the limit.
pub fn calculate_base_asset_amount_to_trade_to_price(
    amm: &AMM,
    limit_price: u64,
    direction: PositionDirection,
) -> VelocityResult<(u64, PositionDirection)> {
    let invariant = k_invariant(amm.sqrt_k)?;

    validate!(
        limit_price > 0,
        ErrorCode::InvalidOrderLimitPrice,
        "limit_price <= 0"
    )?;

    let new_base_asset_reserve_squared = invariant
        .safe_mul(U192::from(PRICE_PRECISION))?
        .safe_div(U192::from(limit_price))?
        .safe_mul(U192::from(amm.peg_multiplier))?
        .safe_div(U192::from(PEG_PRECISION))?;

    let new_base_asset_reserve = new_base_asset_reserve_squared
        .integer_sqrt()
        .try_to_u128()?;

    // Always size the take off the spread-adjusted ask/bid reserves, the
    // same basis the swap actually executes against. Gating on
    // `base_spread > 0` (and falling back to the raw `base_asset_reserve`)
    // let the limit cap diverge from execution whenever the vol/inventory
    // spreads pushed long/short spread above zero while `base_spread` was
    // still zero, so the AMM would fill past the taker's limit price. The
    // cached ask/bid reserves collapse to `base_asset_reserve` when there is
    // no effective spread, so this is exact in the zero-spread case too.
    let base_asset_reserve_before = match direction {
        PositionDirection::Long => amm.ask_base_asset_reserve,
        PositionDirection::Short => amm.bid_base_asset_reserve,
    };

    if new_base_asset_reserve > base_asset_reserve_before {
        let max_trade_amount = new_base_asset_reserve
            .safe_sub(base_asset_reserve_before)?
            .cast::<u64>()
            .unwrap_or(u64::MAX);
        Ok((max_trade_amount, PositionDirection::Short))
    } else {
        let max_trade_amount = base_asset_reserve_before
            .safe_sub(new_base_asset_reserve)?
            .cast::<u64>()
            .unwrap_or(u64::MAX);
        Ok((max_trade_amount, PositionDirection::Long))
    }
}

#[cfg(test)]
/// Test-only convenience: materialise the spread reserves for one
/// direction from a `PerpMarket`, using a zero-spread / zero-offset quote.
/// Production code refreshes the cached reserves via `update_amm_quote_state`.
/// Tests use this shim where they previously called the deleted
/// `calculate_spread_reserves` mutator path and don't need a real spread.
pub(crate) fn calculate_spread_reserves(
    market: &crate::state::perp_market::PerpMarket,
    direction: PositionDirection,
) -> VelocityResult<(u128, u128)> {
    compute_spread_reserves_for_direction(&market.amm, 0, 0, direction)
}
