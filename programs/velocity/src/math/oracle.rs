use {
    crate::{
        error::{ErrorCode, VelocityResult},
        math::{
            casting::Cast,
            constants::{
                BID_ASK_SPREAD_PRECISION, MM_ORACLE_MIN_WRITE_GAP, PERCENTAGE_PRECISION_U64,
            },
            safe_math::SafeMath,
            time::{legacy_slot_duration_i64_raw, DelayOverride, Millis, SlotDuration},
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
            // Same admitted set as `MarginCalc`, deliberately. A DLOB match
            // prices off resting limit orders rather than the oracle, so the
            // looser rule reads reasonable in isolation, but a fill it lets
            // through at a stale-for-margin oracle is a fill whose margin
            // consequences the program cannot evaluate: an exact close by both
            // sides classifies as reducing, which skips the equity-floor gate,
            // and the lazy breaker cannot arm because it requires `MarginCalc`
            // validity. A temporary mark loss then crystallizes permanently at
            // a price the protocol itself treats as unusable, and the
            // counterparty's matching gain settles out of the pnl pool once the
            // feed recovers (OtterSec #142).
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
    slot_duration: SlotDuration,
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
        slot_duration,
    )?;
    let is_oracle_valid =
        is_oracle_valid_for_action(oracle_validity, Some(VelocityAction::UpdateFunding))?;

    let slots_since_amm_update = market.amm.slots_since_update(slot);

    let funding_paused_on_market = market.is_operation_paused(PerpOperation::UpdateFunding);

    // Block if the amm has been stale for more than ~40% of the funding period.
    // `funding_period` is seconds; `* 400` = 0.4 * 1000ms, the historical
    // behavior (the pre-scaling gate compared a raw slot count against
    // `funding_period`, i.e. elapsed slots at 400ms = 0.4 * period seconds).
    // Comparing wall-clock ms on both sides keeps that width at any slot duration.
    let amm_stale_ms = Millis::from_slots(slots_since_amm_update, slot_duration).as_ms();
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
    slot_duration: SlotDuration,
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
        slot_duration,
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
    slot_duration: SlotDuration,
) -> VelocityResult<OracleValidity> {
    let OraclePriceData {
        price: oracle_price,
        confidence: oracle_conf,
        delay: oracle_delay,
        has_sufficient_number_of_data_points,
        ..
    } = *oracle_price_data;

    // Every staleness threshold below is a wall-clock duration (guard rails,
    // per-market overrides, the MM-gap fallback), expressed in actual slots at
    // the live slot duration so the windows stay constant across the IBRL gate
    // activations. Floor rounding: a marginally tighter window is the safe
    // direction for staleness.
    let slots = |m: Millis| m.to_slots(slot_duration) as i64;
    // Ceil variant for the unset MM-sourced immediate threshold: it must match
    // the crank's write gate (`update_mm_oracle`, which ceils
    // `MM_ORACLE_MIN_WRITE_GAP`). Flooring here would put the accept threshold a
    // slot below the write gate at intermediate gates (e.g. 350ms: floor 2 vs
    // write 3), rejecting quotes the crank was allowed to post.
    let slots_ceil = |m: Millis| m.to_slots_ceil(slot_duration) as i64;

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
    // Three cases. `0` is the explicit "never allow immediate AMM fills on this
    // market" sentinel. A positive value is an explicit admin threshold and is
    // used as-is.
    //
    // A negative value means unset, and what it resolves to depends on where
    // the price being classified came from. It previously clamped to
    // `max(override, 0)`, i.e. a threshold of zero, requiring the price to have
    // been written in this exact slot. That is unsatisfiable for an MM-oracle-
    // sourced price by construction: the program refuses any MM-oracle write
    // closer than `MM_ORACLE_MIN_WRITE_GAP` to the previous one, so such a
    // price is at best zero slots old on alternating slots and can never be
    // fresher than that on the rest. A market left at the init default
    // therefore could not pass this gate on roughly half of all slots no matter
    // how aggressively it was cranked — an arithmetic contradiction between two
    // independent constants, so unset resolves to `MM_ORACLE_MIN_WRITE_GAP` for
    // an MM-sourced price: the tightest window the crank can actually satisfy.
    //
    // An exchange-oracle price has no such floor — it can be same-slot fresh
    // every slot — so unset keeps the strict zero threshold there. Widening it
    // too would tolerate extra staleness on immediate fills exactly when the
    // safe-price path has fallen back to the exchange oracle (MM oracle stale
    // or diverged), which is when latency arbitrage against the vAMM pays most.
    //
    // Note `oracle_delay` for an MM price measures from the *landing* slot, and
    // the write path accepts observations up to `MM_ORACLE_MAX_SOURCE_AGE`
    // older than their landing, so the true observation age this gate admits is
    // up to the sum of the two. A `const_assert!` in `math/constants.rs` pins
    // that bound to at most `MM_ORACLE_MIN_WRITE_GAP`, i.e. twice the gap.
    //
    // An explicit override still wins in both directions on both paths, so this
    // only affects markets that never had one set.
    let is_stale_for_amm_immediate =
        match DelayOverride::from_immediate(slots_before_stale_for_amm_immdiate_override) {
            DelayOverride::Never => true,
            DelayOverride::Unset => {
                let unset_threshold: i64 = if immediate_price_is_mm_sourced {
                    slots_ceil(MM_ORACLE_MIN_WRITE_GAP)
                } else {
                    0
                };
                oracle_delay.gt(&unset_threshold)
            }
            DelayOverride::Fixed(threshold) => oracle_delay.gt(&slots(threshold)),
        };

    let is_stale_for_amm_low_risk =
        match DelayOverride::from_low_risk(oracle_low_risk_slot_delay_override) {
            DelayOverride::Fixed(threshold) => oracle_delay.gt(&slots(threshold)),
            _ => oracle_delay.gt(&slots(valid_oracle_guard_rails.stale_for_amm_ms())),
        };

    let is_stale_for_margin = if matches!(oracle_source, OracleSource::PythLazerStableCoin) {
        oracle_delay.gt(&slots(valid_oracle_guard_rails.stale_for_margin_ms()).saturating_mul(3))
    } else {
        oracle_delay.gt(&slots(valid_oracle_guard_rails.stale_for_margin_ms()))
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
