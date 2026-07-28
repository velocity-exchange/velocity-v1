//! vAMM leg of the router quoter interface (S1): turn the AMM curve into
//! discrete [`PriceLevel`]s for the S6 split. In-program — the vAMM never
//! goes through the CPI legs; the router reads this directly and executes
//! via the existing `AmmQuoter` fill path.
//!
//! Slicing: the capped size divides into equal base chunks, each priced at
//! its exact average swap cost against the cached spread reserves. Rounding
//! goes against the taker (ceil for Long, floor for Short), so the split's
//! floored per-level notionals stay at-or-better against the exact swap the
//! execute leg performs; partial consumption of a level is conservative by
//! curve convexity.

use crate::controller::position::PositionDirection;
use crate::error::VelocityResult;
use crate::math::casting::Cast;
use crate::math::safe_math::SafeMath;
use crate::state::prop_amm::{Direction, PriceLevel};

use super::controller::{calculate_base_swap_output, SwapDirection};
use super::math::amm::calculate_amm_available_liquidity;
use super::state::AMM;

/// Levels a vAMM quote is sliced into.
pub const VAMM_QUOTE_LEVELS: u64 = 8;

fn level_price(
    quote: u64,
    base: u64,
    base_precision: u64,
    direction: Direction,
) -> VelocityResult<u64> {
    let numerator = (quote as u128).safe_mul(base_precision as u128)?;
    let price = match direction {
        // Round against the taker: a long pays, so quote the ceiling…
        Direction::Long => numerator
            .safe_add((base as u128).safe_sub(1)?)?
            .safe_div(base as u128)?,
        // …a short receives, so quote the floor.
        Direction::Short => numerator.safe_div(base as u128)?,
    };
    price.cast::<u64>()
}

/// Quote the vAMM: best-first levels covering `min(size, available)`.
pub fn vamm_quote_levels(
    amm: &AMM,
    direction: Direction,
    size: u64,
    step_size: u64,
    base_precision: u64,
) -> VelocityResult<Vec<PriceLevel>> {
    let (position_direction, swap_direction) = match direction {
        Direction::Long => (PositionDirection::Long, SwapDirection::Remove),
        Direction::Short => (PositionDirection::Short, SwapDirection::Add),
    };
    let available = calculate_amm_available_liquidity(amm, &position_direction, step_size)?;
    let total = size.min(available);
    if total == 0 {
        return Ok(vec![]);
    }

    let chunk = (total / VAMM_QUOTE_LEVELS).max(1);
    let mut levels = Vec::with_capacity(VAMM_QUOTE_LEVELS as usize);
    let mut prev_base: u64 = 0;
    let mut prev_quote: u64 = 0;
    for k in 1..=VAMM_QUOTE_LEVELS {
        // The last chunk absorbs the division remainder.
        let cumulative_base = if k == VAMM_QUOTE_LEVELS {
            total
        } else {
            total.min(chunk.safe_mul(k)?)
        };
        let level_base = cumulative_base.safe_sub(prev_base)?;
        if level_base == 0 {
            continue;
        }
        let output = calculate_base_swap_output(amm, cumulative_base, swap_direction)?;
        let level_quote = output.quote_asset_amount.safe_sub(prev_quote)?;
        levels.push(PriceLevel {
            price: level_price(level_quote, level_base, base_precision, direction)?,
            size: level_base,
        });
        prev_base = cumulative_base;
        prev_quote = output.quote_asset_amount;
    }
    Ok(levels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::constants::{AMM_RESERVE_PRECISION, BASE_PRECISION_U64, PEG_PRECISION};

    fn amm_fixture() -> AMM {
        let mut amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            terminal_quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 50 * PEG_PRECISION,
            min_base_asset_reserve: 50 * AMM_RESERVE_PRECISION,
            max_base_asset_reserve: 200 * AMM_RESERVE_PRECISION,
            // reserve/4 per fill so a 10-unit test size clears the cap
            max_fill_reserve_fraction: 4,
            ..AMM::default()
        };
        amm.seed_no_spread_quote_state();
        amm
    }

    /// The split's floored per-level notional, as `math::router` computes it.
    fn split_notional(levels: &[PriceLevel]) -> u64 {
        levels
            .iter()
            .map(|l| ((l.price as u128) * (l.size as u128) / BASE_PRECISION_U64 as u128) as u64)
            .sum()
    }

    #[test]
    fn long_levels_are_monotone_and_at_or_better() {
        let amm = amm_fixture();
        let size = 10 * BASE_PRECISION_U64;
        let levels = vamm_quote_levels(&amm, Direction::Long, size, 1, BASE_PRECISION_U64).unwrap();

        assert_eq!(levels.iter().map(|l| l.size).sum::<u64>(), size);
        assert!(levels.windows(2).all(|w| w[0].price <= w[1].price));
        // First chunk averages above the no-spread reserve price ($50).
        assert!(levels[0].price >= 50 * PEG_PRECISION as u64);

        // Quoted (floored) notional must cover the exact swap cost of the
        // whole size — the at-or-better invariant a long execute is held to.
        let exact = calculate_base_swap_output(&amm, size, SwapDirection::Remove)
            .unwrap()
            .quote_asset_amount;
        assert!(split_notional(&levels) >= exact);
    }

    #[test]
    fn short_levels_are_monotone_and_at_or_better() {
        let amm = amm_fixture();
        let size = 10 * BASE_PRECISION_U64;
        let levels =
            vamm_quote_levels(&amm, Direction::Short, size, 1, BASE_PRECISION_U64).unwrap();

        assert_eq!(levels.iter().map(|l| l.size).sum::<u64>(), size);
        assert!(levels.windows(2).all(|w| w[0].price >= w[1].price));
        assert!(levels[0].price <= 50 * PEG_PRECISION as u64);

        // Quoted notional must not overpromise what the exact swap pays out.
        let exact = calculate_base_swap_output(&amm, size, SwapDirection::Add)
            .unwrap()
            .quote_asset_amount;
        assert!(split_notional(&levels) <= exact);
    }

    #[test]
    fn size_caps_at_available_liquidity() {
        let amm = amm_fixture();
        let levels =
            vamm_quote_levels(&amm, Direction::Long, u64::MAX, 1, BASE_PRECISION_U64).unwrap();
        let available =
            calculate_amm_available_liquidity(&amm, &PositionDirection::Long, 1).unwrap();
        assert_eq!(levels.iter().map(|l| l.size).sum::<u64>(), available);
    }

    #[test]
    fn zero_size_is_empty() {
        let amm = amm_fixture();
        assert!(
            vamm_quote_levels(&amm, Direction::Long, 0, 1, BASE_PRECISION_U64)
                .unwrap()
                .is_empty()
        );
    }
}
