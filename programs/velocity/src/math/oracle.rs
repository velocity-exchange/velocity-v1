use {
    crate::{
        error::{ErrorCode, VelocityResult},
        math::{
            casting::Cast,
            constants::{
                BID_ASK_SPREAD_PRECISION, MM_ORACLE_MIN_WRITE_GAP, PERCENTAGE_PRECISION_U64,
            },
            safe_math::SafeMath,
            time::{legacy_slot_duration_i64_raw, DelayOverride, Millis, SlotClock},
        },
        state::{
            oracle::{OraclePriceData, OracleSource},
            paused_operations::PerpOperation,
            perp_market::PerpMarket,
            state::{OracleGuardRails, PriceDivergenceGuardRails, ValidityGuardRails},
            user::MarketType,
        },
    },
    anchor_lang::prelude::{AnchorDeserialize, AnchorSerialize},
    std::{convert::TryFrom, fmt},
};

/// True when |spread_pct| exceeds the configured divergence threshold (with
/// a 10% safety floor). Pure decision helper — no AMM, no oracle state.
pub fn is_mark_oracle_too_divergent(
    price_spread_pct: i64,
    guard_rails: &PriceDivergenceGuardRails,
) -> VelocityResult<bool> {
    let max_divergence = guard_rails
        .mark_oracle_percent_divergence
        .max(PERCENTAGE_PRECISION_U64 / 10);
    Ok(price_spread_pct.unsigned_abs() > max_divergence)
}

#[cfg(test)]
mod tests;

// ordered by "severity"
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Debug, Eq, Default)]
pub enum OracleValidity {
    NonPositive,
    TooVolatile,
    TooUncertain,
    StaleForMargin,
    InsufficientDataPoints,
    StaleForAMM {
        immediate: bool,
        low_risk: bool,
    },
    #[default]
    Valid,
}

impl OracleValidity {
    pub fn get_error_code(&self) -> ErrorCode {
        match self {
            OracleValidity::NonPositive => ErrorCode::OracleNonPositive,
            OracleValidity::TooVolatile => ErrorCode::OracleTooVolatile,
            OracleValidity::TooUncertain => ErrorCode::OracleTooUncertain,
            OracleValidity::StaleForMargin => ErrorCode::OracleStaleForMargin,
            OracleValidity::InsufficientDataPoints => ErrorCode::OracleInsufficientDataPoints,
            OracleValidity::StaleForAMM { .. } => ErrorCode::OracleStaleForAMM,
            OracleValidity::Valid => unreachable!(),
        }
    }
}

impl fmt::Display for OracleValidity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OracleValidity::NonPositive => write!(f, "NonPositive"),
            OracleValidity::TooVolatile => write!(f, "TooVolatile"),
            OracleValidity::TooUncertain => write!(f, "TooUncertain"),
            OracleValidity::StaleForMargin => write!(f, "StaleForMargin"),
            OracleValidity::InsufficientDataPoints => write!(f, "InsufficientDataPoints"),
            OracleValidity::StaleForAMM {
                immediate,
                low_risk,
            } => {
                if *immediate {
                    write!(f, "StaleForAMM (immediate)")
                } else if *low_risk {
                    write!(f, "StaleForAMM (low risk)")
                } else {
                    write!(f, "StaleForAMM")
                }
            }
            OracleValidity::Valid => write!(f, "Valid"),
        }
    }
}

impl TryFrom<u8> for OracleValidity {
    type Error = ErrorCode;

    fn try_from(v: u8) -> VelocityResult<Self> {
        match v {
            0 => Ok(OracleValidity::NonPositive),
            1 => Ok(OracleValidity::TooVolatile),
            2 => Ok(OracleValidity::TooUncertain),
            3 => Ok(OracleValidity::StaleForMargin),
            4 => Ok(OracleValidity::InsufficientDataPoints),
            5 => Ok(OracleValidity::StaleForAMM {
                immediate: true,
                low_risk: true,
            }),
            6 => Ok(OracleValidity::StaleForAMM {
                immediate: true,
                low_risk: false,
            }),
            7 => Ok(OracleValidity::Valid),
            _ => panic!("Invalid OracleValidity"),
        }
    }
}

impl From<OracleValidity> for u8 {
    fn from(src: OracleValidity) -> u8 {
        match src {
            OracleValidity::NonPositive => 0,
            OracleValidity::TooVolatile => 1,
            OracleValidity::TooUncertain => 2,
            OracleValidity::StaleForMargin => 3,
            OracleValidity::InsufficientDataPoints => 4,
            OracleValidity::StaleForAMM {
                immediate: true,
                low_risk: true,
            } => 5,
            OracleValidity::StaleForAMM {
                immediate: true,
                low_risk: false,
            } => 6,
            OracleValidity::Valid
            | OracleValidity::StaleForAMM {
                immediate: false,
                low_risk: false,
            } => 7,
            OracleValidity::StaleForAMM {
                immediate: false,
                low_risk: true,
            } => unreachable!(),
        }
    }
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Debug, Eq)]
pub enum VelocityAction {
    UpdateFunding,
    SettlePnl,
    TriggerOrder,
    FillOrderMatch,
    FillOrderAmmLowRisk,
    FillOrderAmmImmediate,
    Liquidate,
    MarginCalc,
    UpdateTwap,
    UpdateAMMCurve,
    OracleOrderPrice,
    UseMMOraclePrice,
    UpdateAmmCache,
    UpdateLpPoolAum,
    LpPoolSwap,
}

pub fn is_oracle_valid_for_action(
    oracle_validity: OracleValidity,
    action: Option<VelocityAction>,
) -> VelocityResult<bool> {
    let is_ok = match action {
        Some(action) => match action {
            VelocityAction::FillOrderAmmImmediate => {
                matches!(oracle_validity, OracleValidity::Valid)
            }
            VelocityAction::FillOrderAmmLowRisk => {
                matches!(
                    oracle_validity,
                    OracleValidity::Valid
                        | OracleValidity::StaleForAMM {
                            immediate: _,
                            low_risk: false
                        }
                )
            }
            // relax oracle staleness, later checks for sufficiently recent amm slot update for funding update
            VelocityAction::UpdateFunding => {
                matches!(
                    oracle_validity,
                    OracleValidity::Valid
                        | OracleValidity::StaleForAMM { .. }
                        | OracleValidity::InsufficientDataPoints
                        | OracleValidity::StaleForMargin
                )
            }
            VelocityAction::OracleOrderPrice => {
                matches!(
                    oracle_validity,
                    OracleValidity::Valid
                        | OracleValidity::StaleForAMM { .. }
                        | OracleValidity::InsufficientDataPoints
                )
            }
            VelocityAction::MarginCalc => !matches!(
                oracle_validity,
                OracleValidity::NonPositive
                    | OracleValidity::TooVolatile
                    | OracleValidity::TooUncertain
                    | OracleValidity::StaleForMargin
            ),
            VelocityAction::TriggerOrder => !matches!(
                oracle_validity,
                OracleValidity::NonPositive | OracleValidity::TooVolatile
            ),
            VelocityAction::SettlePnl => matches!(
                oracle_validity,
                OracleValidity::Valid
                    | OracleValidity::StaleForAMM { .. }
                    | OracleValidity::InsufficientDataPoints
                    | OracleValidity::StaleForMargin
            ),
            // The admitted set matches `MarginCalc` on purpose. A maker match
            // prices off resting limit orders rather than the oracle, so a
            // looser rule reads reasonable in isolation. A fill it lets through
            // at a stale-for-margin oracle still has margin consequences the
            // program cannot evaluate. An exact close by both sides classifies
            // as reducing, which skips the equity-floor gate, and the lazy
            // breaker cannot arm because it requires `MarginCalc` validity. A
            // temporary mark loss then crystallizes permanently at a price the
            // protocol itself treats as unusable, and the counterparty's
            // matching gain settles out of the pnl pool once the feed recovers
            // (OtterSec #142).
            VelocityAction::FillOrderMatch => !matches!(
                oracle_validity,
                OracleValidity::NonPositive
                    | OracleValidity::TooVolatile
                    | OracleValidity::TooUncertain
                    | OracleValidity::StaleForMargin
            ),
            VelocityAction::UpdateAmmCache
            | VelocityAction::UpdateLpPoolAum
            | VelocityAction::LpPoolSwap => !matches!(
                oracle_validity,
                OracleValidity::NonPositive
                    | OracleValidity::TooVolatile
                    | OracleValidity::TooUncertain
            ),
            VelocityAction::Liquidate => !matches!(
                oracle_validity,
                OracleValidity::NonPositive | OracleValidity::TooVolatile
            ),
            VelocityAction::UpdateTwap => !matches!(oracle_validity, OracleValidity::NonPositive),
            VelocityAction::UpdateAMMCurve => {
                !matches!(oracle_validity, OracleValidity::NonPositive)
            }
            VelocityAction::UseMMOraclePrice => !matches!(
                oracle_validity,
                OracleValidity::NonPositive | OracleValidity::TooVolatile,
            ),
        },
        None => {
            matches!(oracle_validity, OracleValidity::Valid)
        }
    };

    Ok(is_ok)
}

pub fn block_operation(
    market: &PerpMarket,
    oracle_price_data: &OraclePriceData,
    guard_rails: &OracleGuardRails,
    reserve_price: u64,
    slot: u64,
    slot_clock: SlotClock,
) -> VelocityResult<bool> {
    let OracleStatus {
        oracle_validity,
        mark_too_divergent: is_oracle_mark_too_divergent,
        oracle_reserve_price_spread_pct: _,
        ..
    } = get_oracle_status(
        market,
        oracle_price_data,
        guard_rails,
        reserve_price,
        slot,
        slot_clock,
    )?;
    let is_oracle_valid =
        is_oracle_valid_for_action(oracle_validity, Some(VelocityAction::UpdateFunding))?;

    let slots_since_amm_update = market.amm.slots_since_update(slot);

    let funding_paused_on_market = market.is_operation_paused(PerpOperation::UpdateFunding);

    // Block when the amm has been stale for more than 40% of the funding period.
    // `funding_period` is in seconds, and `* 400` is 0.4 * 1000ms. An earlier gate
    // compared a raw slot count against `funding_period`, which at 400ms slots is the
    // same 40% of the period. Comparing wall-clock milliseconds on both sides keeps
    // that width at any slot duration.
    let amm_stale_ms = slot_clock
        .elapsed_slot_delta(slots_since_amm_update, slot)
        .as_ms();
    let block = amm_stale_ms
        > market
            .market_stats
            .funding_period
            .cast::<u64>()?
            .safe_mul(400)?
        || !is_oracle_valid
        || is_oracle_mark_too_divergent
        || funding_paused_on_market;
    Ok(block)
}

#[derive(Default, Clone, Copy, Debug)]
pub struct OracleStatus {
    pub price_data: OraclePriceData,
    pub oracle_reserve_price_spread_pct: i64,
    pub mark_too_divergent: bool,
    pub oracle_validity: OracleValidity,
}

pub fn get_oracle_status(
    market: &PerpMarket,
    oracle_price_data: &OraclePriceData,
    guard_rails: &OracleGuardRails,
    reserve_price: u64,
    slot: u64,
    slot_clock: SlotClock,
) -> VelocityResult<OracleStatus> {
    let slot_delay_override =
        legacy_slot_duration_i64_raw(guard_rails.validity.slots_before_stale_for_amm).cast()?;
    let oracle_validity = oracle_validity(
        MarketType::Perp,
        market.market_index,
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap,
        oracle_price_data,
        &guard_rails.validity,
        market.get_max_confidence_interval_multiplier()?,
        &market.oracle_source,
        LogMode::None,
        slot_delay_override,
        false, // exchange-oracle price, never MM-sourced
        slot_delay_override,
        slot,
        slot_clock,
    )?;
    let oracle_reserve_price_spread_pct = market
        .market_stats
        .historical_oracle_data
        .twap_5min_spread_pct(reserve_price)?;
    let is_oracle_mark_too_divergent = is_mark_oracle_too_divergent(
        oracle_reserve_price_spread_pct,
        &guard_rails.price_divergence,
    )?;

    Ok(OracleStatus {
        price_data: *oracle_price_data,
        oracle_reserve_price_spread_pct,
        mark_too_divergent: is_oracle_mark_too_divergent,
        oracle_validity,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogMode {
    None,
    ExchangeOracle,
    MMOracle,
    SafeMMOracle,
    Margin,
}

pub fn oracle_validity(
    market_type: MarketType,
    market_index: u16,
    last_oracle_twap: i64,
    oracle_price_data: &OraclePriceData,
    valid_oracle_guard_rails: &ValidityGuardRails,
    max_confidence_interval_multiplier: u64,
    oracle_source: &OracleSource,
    log_mode: LogMode,
    slots_before_stale_for_amm_immdiate_override: i8,
    immediate_price_is_mm_sourced: bool,
    oracle_low_risk_slot_delay_override: i8,
    current_slot: u64,
    slot_clock: SlotClock,
) -> VelocityResult<OracleValidity> {
    let OraclePriceData {
        price: oracle_price,
        confidence: oracle_conf,
        delay: oracle_delay,
        has_sufficient_number_of_data_points,
        ..
    } = *oracle_price_data;

    let oracle_age = slot_clock.elapsed_slot_delta(oracle_delay.max(0) as u64, current_slot);

    let is_oracle_price_nonpositive = oracle_price <= 0;

    let is_oracle_price_too_volatile = (oracle_price.max(last_oracle_twap))
        .safe_div(last_oracle_twap.min(oracle_price).max(1))?
        .gt(&valid_oracle_guard_rails.too_volatile_ratio);

    let conf_pct_of_price = oracle_conf
        .safe_mul(BID_ASK_SPREAD_PRECISION)?
        .safe_div(oracle_price.cast()?)?;

    // TooUncertain
    let is_conf_too_large = conf_pct_of_price.gt(&valid_oracle_guard_rails
        .confidence_interval_max_size
        .safe_mul(max_confidence_interval_multiplier)?);

    // Immediate (JIT / auction-skipping) AMM fills.
    //
    // There are three cases. `0` is the explicit sentinel for never allowing an
    // immediate AMM fill on this market. A positive value is an explicit admin
    // threshold and is used as it stands.
    //
    // A negative value means unset, and what it resolves to depends on where the price
    // being classified came from. A threshold of zero requires the price to have been
    // written in this exact slot. An MM-oracle-sourced price cannot satisfy that by
    // construction. The program refuses any MM-oracle write closer than
    // `MM_ORACLE_MIN_WRITE_GAP` to the previous one, so such a price is at best zero
    // slots old on alternating slots and is older on the rest. A market left at the
    // init default would fail this gate on roughly half of all slots however hard it
    // was cranked. Unset therefore resolves to `MM_ORACLE_MIN_WRITE_GAP` for an
    // MM-sourced price, which is the tightest window the crank can satisfy.
    //
    // An exchange-oracle price has no such floor and can be same-slot fresh every
    // slot, so unset keeps the strict zero threshold there. Widening it would tolerate
    // extra staleness on immediate fills exactly when the safe-price path has fallen
    // back to the exchange oracle, which happens when the MM oracle is stale or
    // diverged. That is when latency arbitrage against the vAMM pays most.
    //
    // `oracle_delay` for an MM price measures from the landing slot, and the write path
    // accepts observations up to `MM_ORACLE_MAX_SOURCE_AGE` older than their landing.
    // The true observation age this gate admits is therefore up to the sum of the two.
    // A `const_assert!` in `math/constants.rs` holds that bound at or below
    // `MM_ORACLE_MIN_WRITE_GAP`, so the sum is twice the gap, to within one slot.
    //
    // An explicit override still wins in both directions on both paths, so this only
    // affects markets that never had one set.
    let is_stale_for_amm_immediate =
        match DelayOverride::from_immediate(slots_before_stale_for_amm_immdiate_override) {
            DelayOverride::Never => true,
            DelayOverride::Unset => {
                let unset_threshold: Millis = if immediate_price_is_mm_sourced {
                    // The MM write gate rounds its minimum interval up to a
                    // whole number of slots. Measure that same accepted slot
                    // window through the clock. Otherwise 3 x 350ms reads as
                    // stale even though the crank cannot legally write at 2.
                    slot_clock.elapsed_slot_delta(
                        MM_ORACLE_MIN_WRITE_GAP
                            .to_slots_ceil(slot_clock.slot_duration_at(current_slot)),
                        current_slot,
                    )
                } else {
                    Millis::ZERO
                };
                oracle_age > unset_threshold
            }
            DelayOverride::Fixed(threshold) => oracle_age > threshold,
        };

    let is_stale_for_amm_low_risk =
        match DelayOverride::from_low_risk(oracle_low_risk_slot_delay_override) {
            DelayOverride::Fixed(threshold) => oracle_age > threshold,
            _ => oracle_age > valid_oracle_guard_rails.stale_for_amm_ms(),
        };

    let is_stale_for_margin = if matches!(oracle_source, OracleSource::PythLazerStableCoin) {
        oracle_age
            > valid_oracle_guard_rails
                .stale_for_margin_ms()
                .saturating_mul(3)
    } else {
        oracle_age > valid_oracle_guard_rails.stale_for_margin_ms()
    };

    let oracle_validity = if is_oracle_price_nonpositive {
        OracleValidity::NonPositive
    } else if is_oracle_price_too_volatile {
        OracleValidity::TooVolatile
    } else if is_conf_too_large {
        OracleValidity::TooUncertain
    } else if is_stale_for_margin {
        OracleValidity::StaleForMargin
    } else if !has_sufficient_number_of_data_points {
        OracleValidity::InsufficientDataPoints
    } else if is_stale_for_amm_immediate || is_stale_for_amm_low_risk {
        OracleValidity::StaleForAMM {
            immediate: is_stale_for_amm_immediate,
            low_risk: is_stale_for_amm_low_risk,
        }
    } else {
        OracleValidity::Valid
    };

    if log_mode != LogMode::None {
        let oracle_type = if log_mode == LogMode::ExchangeOracle || log_mode == LogMode::Margin {
            "Exchange"
        } else if log_mode == LogMode::SafeMMOracle {
            "SafeMM"
        } else {
            "MM"
        };
        if !has_sufficient_number_of_data_points {
            crate::msg!(
                "Invalid {} {} {} Oracle: Insufficient Data Points",
                market_type,
                market_index,
                oracle_type
            );
        }

        if is_oracle_price_nonpositive {
            crate::msg!(
                "Invalid {} {} {} Oracle: Non-positive (oracle_price <=0)",
                market_type,
                market_index,
                oracle_type
            );
        }

        if is_oracle_price_too_volatile {
            crate::msg!(
                "Invalid {} {} {} Oracle: Too Volatile (last_oracle_price_twap={:?} vs oracle_price={:?})",
                market_type,
                market_index,
                oracle_type,
                last_oracle_twap,
                oracle_price,
            );
        }

        if is_conf_too_large {
            crate::msg!(
                "Invalid {} {} {} Oracle: Confidence Too Large (is_conf_too_large={:?})",
                market_type,
                market_index,
                oracle_type,
                conf_pct_of_price
            );
        }

        if is_stale_for_margin {
            crate::msg!(
                "Invalid {} {} {} Oracle: Stale for Margin (oracle_delay={:?})",
                market_type,
                market_index,
                oracle_type,
                oracle_delay
            );
        }

        if (is_stale_for_amm_immediate || is_stale_for_amm_low_risk) && log_mode != LogMode::Margin
        {
            crate::msg!(
                "Invalid {} {} {} Oracle: Stale (oracle_delay={:?}), (stale_for_amm_immediate={}, stale_for_amm_low_risk={}, stale_for_margin={})",
                market_type,
                market_index,
                oracle_type,
                oracle_delay,
                is_stale_for_amm_immediate,
                is_stale_for_amm_low_risk,
                is_stale_for_margin
            );
        }
    }

    Ok(oracle_validity)
}
