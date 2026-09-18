//! What a quoter's fill is held to.
//!
//! [`fill_at_or_better`] validates one quoter's fill. A quoter must deliver
//! each unit at or better than the price its own book advertised.
//!
//! The route itself lives in `controller::orders::perp_fill`. It splits the
//! taker size across the vAMM ladder and the externally-quoted CPI books by
//! priority tier. There is no general continuous-curve clearing algorithm
//! anywhere, no bisection and no price-domain search. Every source publishes
//! discrete price levels, the vAMM included, and the split walks them
//! best-first.

use crate::{
    controller::position::PositionDirection,
    error::VelocityResult,
    math::{router::QuoterAllocation, safe_math::SafeMath},
    state::quoter::QuoterFill,
};

/// True when a fill is at or better than its allocation.
///
/// A fill qualifies when its floored per-unit price does not lose to the
/// allocation's ceiled per-unit price. The comparison is reversed for a short
/// taker. One quote lamport of absolute notional slack is allowed.
///
/// The per-unit comparison absorbs sub-unit rounding on a partial fill, which
/// a raw notional cross-multiply does not. The one-lamport slack absorbs a
/// quoter's terminal rounding against the taker on an otherwise exact
/// notional, such as the AMM's +1 on Remove or a short maker's ceil. Per-unit
/// math cannot hide that rounding when the fill is an exact base-precision
/// multiple. `math::router::quote_notional` already ceils the allocation's
/// quote per level, so one lamport is the whole honest gap and not a tunable
/// tolerance.
pub(crate) fn fill_at_or_better(
    side: PositionDirection,
    fill: &QuoterFill,
    allocation: &QuoterAllocation,
    base_precision: u64,
) -> VelocityResult<bool> {
    if fill.base_filled == 0 {
        return Ok(true);
    }

    match side {
        PositionDirection::Long => {
            if fill.quote_filled <= allocation.quote.saturating_add(1) {
                return Ok(true);
            }
        }
        PositionDirection::Short => {
            if fill.quote_filled.saturating_add(1) >= allocation.quote {
                return Ok(true);
            }
        }
    }

    let bp = base_precision.max(1) as u128;
    let actual_floor = (fill.quote_filled as u128)
        .safe_mul(bp)?
        .safe_div(fill.base_filled as u128)?;
    let quoted = (allocation.quote as u128).safe_mul(bp)?;
    Ok(match side {
        PositionDirection::Long => {
            let quoted_ceil = quoted
                .safe_add((allocation.base as u128).safe_sub(1)?)?
                .safe_div(allocation.base as u128)?;
            actual_floor <= quoted_ceil
        }
        PositionDirection::Short => {
            let actual_ceil = (fill.quote_filled as u128)
                .safe_mul(bp)?
                .safe_add((fill.base_filled as u128).safe_sub(1)?)?
                .safe_div(fill.base_filled as u128)?;
            let quoted_floor = quoted.safe_div(allocation.base as u128)?;
            actual_ceil >= quoted_floor
        }
    })
}
