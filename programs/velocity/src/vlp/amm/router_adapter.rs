//! vAMM leg of the router's quoter interface: turn the AMM curve into
//! discrete [`PriceLevel`]s for the router split. In-program — the vAMM
//! never goes through the CPI legs, which is what makes last look possible:
//! the router quotes every CPI book first and hands them here, and rival
//! prices become ladder rungs. The slice of curve that is cheaper than a
//! rival's price is quoted AT the rival's price — the vAMM wins the tie by
//! tier priority and captures the difference for the LPs instead of
//! donating it as taker price improvement.
//!
//! Every checkpoint's price comes from the swap math the execute leg will
//! run (`calculate_base_swap_output`, spread reserves and all): a rung's
//! price is the exact per-unit cost of its own slice, rounded against the
//! taker. So each level upper-bounds (long) / lower-bounds (short) what the
//! AMM will actually charge for it, and at-or-better holds by construction
//! rather than by tolerance. A running bound keeps the book monotone when a
//! shading rung would otherwise exceed the next honest slice. Rival rungs
//! are only honored
//! within [`LAST_LOOK_BAND`] of the vAMM's top — a garbage price from a
//! malicious-but-approved quoter can't inflate this book; beyond the band
//! the curve is priced honestly by equal-size checkpoints.

use {
    super::{
        controller::{calculate_base_swap_output, SwapDirection},
        math::{
            amm::calculate_amm_available_liquidity,
            spread::calculate_base_asset_amount_to_trade_to_price,
        },
        quoter::AmmQuoter,
        state::AMM,
    },
    crate::{
        controller::position::PositionDirection,
        error::VelocityResult,
        math::{
            casting::Cast,
            constants::{BASE_PRECISION_U64, PERCENTAGE_PRECISION_U64},
            router::QuoterBook,
            safe_math::SafeMath,
        },
        state::{
            prop_amm::{Direction, PriceLevel, QuoterType},
            quoter::{QuoteContext, QuoterFill, RouterQuoter},
        },
    },
};

/// Ladder checkpoints per quote (rival rungs + equal-size filler).
pub const VAMM_QUOTE_CHECKPOINTS: usize = 8;

/// Rival prices are honored as shading rungs only within this fraction of
/// the vAMM's top price (PERCENTAGE_PRECISION): 5%.
pub const LAST_LOOK_BAND: u64 = PERCENTAGE_PRECISION_U64 / 20;

/// Quote the vAMM: best-first ladder levels covering `min(size, available)`,
/// shaded toward `rival_books` within the last-look band.
///
/// `taker_limit` bounds the ladder honestly: the curve inversion finds the
/// cumulative where the marginal price reaches the limit and the total is
/// capped there, so every rung's true cost is inside the limit (a slice's
/// average never exceeds its end marginal). This is the taker's own price —
/// no shading-band question.
///
/// Because the cap happens here, the ladder's book is authoritative for the
/// limit and callers must NOT re-truncate it by comparing rung prices to the
/// limit: a dust rung's notional rounds up to a whole lamport, which inflates
/// its quoted per-unit price past the very limit that admitted it, and
/// dropping it would cost the taker liquidity they can afford.
pub fn vamm_quote_levels(
    amm: &AMM,
    direction: Direction,
    size: u64,
    step_size: u64,
    rival_books: &[QuoterBook],
    taker_limit: Option<u64>,
) -> VelocityResult<Vec<PriceLevel>> {
    let (position_direction, swap_direction) = match direction {
        Direction::Long => (PositionDirection::Long, SwapDirection::Remove),
        Direction::Short => (PositionDirection::Short, SwapDirection::Add),
    };
    let available = calculate_amm_available_liquidity(amm, &position_direction, step_size)?;
    let mut total = size.min(available);
    if total == 0 {
        return Ok(vec![]);
    }

    let reserve_price = amm.reserve_price()?;
    let top = match direction {
        Direction::Long => {
            amm.ask_price(reserve_price, amm.long_spread, amm.reference_price_offset)?
        }
        Direction::Short => {
            amm.bid_price(reserve_price, amm.short_spread, amm.reference_price_offset)?
        }
    };
    if let Some(limit) = taker_limit {
        let crossed_at_top = match direction {
            Direction::Long => limit < top,
            Direction::Short => limit > top,
        };
        if crossed_at_top {
            return Ok(vec![]);
        }
        let (reachable, trade_direction) =
            calculate_base_asset_amount_to_trade_to_price(amm, limit, position_direction)?;
        if trade_direction == position_direction {
            total = total.min(reachable);
        }
        if total == 0 {
            return Ok(vec![]);
        }
    }
    let band_edge = {
        let band = (top as u128)
            .safe_mul(LAST_LOOK_BAND as u128)?
            .safe_div(PERCENTAGE_PRECISION_U64 as u128)? as u64;
        match direction {
            Direction::Long => top.safe_add(band)?,
            Direction::Short => top.safe_sub(band)?,
        }
    };

    // Last look: rival prices beyond our top (but within the band and the
    // taker's limit) become shading rungs, best-first. Always on — the vAMM
    // was winning this flow at its honest price anyway (price priority), so
    // filling at the rival's price instead is strictly LP surplus with no
    // cost to any maker. A rung past the taker's limit would price its whole
    // slice unfillable, so those are dropped here rather than truncated
    // downstream.
    let rung_edge = match (taker_limit, direction) {
        (Some(limit), Direction::Long) => band_edge.min(limit),
        (Some(limit), Direction::Short) => band_edge.max(limit),
        (None, _) => band_edge,
    };
    let mut rival_rungs: Vec<u64> = rival_books
        .iter()
        .flat_map(|book| book.levels.iter().map(|level| level.price))
        .filter(|&price| match direction {
            Direction::Long => price > top && price <= rung_edge,
            Direction::Short => price < top && price >= rung_edge && price > 0,
        })
        .collect();
    rival_rungs.sort_unstable();
    if direction == Direction::Short {
        rival_rungs.reverse();
    }
    rival_rungs.dedup();
    rival_rungs.truncate(VAMM_QUOTE_CHECKPOINTS);

    // Checkpoints: (cumulative base, shading price if this is a rival rung).
    let mut checkpoints: Vec<(u64, Option<u64>)> = Vec::with_capacity(VAMM_QUOTE_CHECKPOINTS + 1);
    for price in rival_rungs {
        let (cumulative, trade_direction) =
            calculate_base_asset_amount_to_trade_to_price(amm, price, position_direction)?;
        if trade_direction != position_direction {
            continue;
        }
        let cumulative = cumulative.min(total);
        checkpoints.push((cumulative, Some(price)));
        if cumulative == total {
            break;
        }
    }
    // Beyond the last rival rung the curve is priced honestly: equal-size
    // checkpoints, priced below from the swap math itself.
    let covered = checkpoints.last().map(|c| c.0).unwrap_or(0);
    if covered < total {
        let filler = VAMM_QUOTE_CHECKPOINTS
            .saturating_sub(checkpoints.len())
            .max(1) as u64;
        let chunk = ((total - covered) / filler).max(1);
        for k in 1..=filler {
            let cumulative = if k == filler {
                total
            } else {
                total.min(covered.safe_add(chunk.safe_mul(k)?)?)
            };
            checkpoints.push((cumulative, None));
            if cumulative == total {
                break;
            }
        }
    }

    // Emit step-aligned rungs priced off the swap math that will actually
    // execute. Two invariants have to hold together:
    //
    //  * `size` is an `order_step_size` multiple — the split allocates in
    //    step quanta, so a rung's sub-step tail would be floored away and
    //    the vAMM would silently under-quote its depth.
    //  * `price` is a true per-unit bound on its own slice. The slice's
    //    exact notional comes from `calculate_base_swap_output` (the same
    //    call the execute leg makes, spread reserves and all) rather than
    //    the raw-invariant marginal price, which diverges from what the AMM
    //    charges because the spread quote reserve isn't the invariant's.
    //    Rival rungs keep their shading price when it's the taker-worse of
    //    the two, and a running bound keeps the book monotone (a rival rung
    //    can otherwise exceed the next honest slice price, and the split
    //    truncates a book at its first non-monotone level).
    let step = step_size.max(1);
    let mut levels = Vec::with_capacity(checkpoints.len());
    let mut previous = 0u64;
    let mut previous_notional = 0u64;
    let mut bound: Option<u64> = None;
    for (cumulative, shade) in checkpoints {
        let cumulative = cumulative - cumulative % step;
        if cumulative <= previous {
            continue;
        }
        let size = cumulative.safe_sub(previous)?;
        let notional = calculate_base_swap_output(amm, cumulative, swap_direction)?
            .quote_asset_amount
            .max(previous_notional);
        let slice_notional = notional.safe_sub(previous_notional)?;
        let exact = (slice_notional as u128).safe_mul(BASE_PRECISION_U64 as u128)?;
        let honest = match direction {
            Direction::Long => exact.safe_div_ceil(size as u128)?,
            Direction::Short => exact.safe_div(size as u128)?,
        }
        .cast::<u64>()?;
        let price = match direction {
            Direction::Long => shade.unwrap_or(0).max(honest).max(bound.unwrap_or(0)),
            Direction::Short => shade
                .unwrap_or(u64::MAX)
                .min(honest)
                .min(bound.unwrap_or(u64::MAX)),
        };
        if price == 0 {
            break;
        }
        bound = Some(price);
        levels.push(PriceLevel {
            price: price.into(),
            size: size.into(),
        });
        previous = cumulative;
        previous_notional = notional;
    }
    Ok(levels)
}

impl RouterQuoter for AmmQuoter<'_> {
    fn priority(&self) -> u8 {
        QuoterType::Vamm.default_priority()
    }

    /// The vAMM's router quote is the shaded ladder — `rival_books` is the
    /// last look. The AMM's per-fill refresh happens in its own `setup`
    /// step, which the fill controller runs before the router quotes.
    fn quote(
        &self,
        ctx: &QuoteContext,
        direction: Direction,
        size: u64,
        rival_books: &[QuoterBook],
    ) -> VelocityResult<Vec<PriceLevel>> {
        vamm_quote_levels(self.amm, direction, size, ctx.step_size, rival_books, None)
    }

    fn execute(
        &mut self,
        ctx: &QuoteContext,
        direction: Direction,
        size: u64,
    ) -> VelocityResult<QuoterFill> {
        let side = direction.to_position_direction();
        let fill = self
            .try_fill_solo(ctx, side, size)?
            .unwrap_or(QuoterFill::ZERO);
        if fill.base_filled > 0 {
            self.commit_fill(ctx, &fill)?;
        }
        Ok(fill)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            math::constants::{AMM_RESERVE_PRECISION, BASE_PRECISION_U64, PEG_PRECISION},
            vlp::amm::controller::calculate_base_swap_output,
        },
    };

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

    fn rival_book(levels: &[PriceLevel]) -> Vec<QuoterBook<'_>> {
        vec![QuoterBook {
            priority: 10,
            levels,
        }]
    }

    /// The split's floored per-level notional, as `math::router` computes it.
    fn split_notional(levels: &[PriceLevel]) -> u64 {
        levels
            .iter()
            .map(|l| ((l.price as u128) * (l.size as u128) / BASE_PRECISION_U64 as u128) as u64)
            .sum()
    }

    const TOP: u64 = 50 * PEG_PRECISION as u64; // no-spread reserve price

    #[test]
    fn long_ladder_is_monotone_and_at_or_better() {
        let amm = amm_fixture();
        let size = 10 * BASE_PRECISION_U64;
        let levels = vamm_quote_levels(&amm, Direction::Long, size, 1, &[], None).unwrap();

        assert_eq!(levels.iter().map(|l| l.size).sum::<u64>(), size);
        assert!(levels.windows(2).all(|w| w[0].price <= w[1].price));
        assert!(levels[0].price >= TOP);

        // Marginal-end pricing upper-bounds each slice's true average cost,
        // so the quoted (floored) notional covers the exact one-shot swap.
        let exact = calculate_base_swap_output(&amm, size, SwapDirection::Remove)
            .unwrap()
            .quote_asset_amount;
        assert!(split_notional(&levels) >= exact);
    }

    #[test]
    fn short_ladder_is_monotone_and_at_or_better() {
        let amm = amm_fixture();
        let size = 10 * BASE_PRECISION_U64;
        let levels = vamm_quote_levels(&amm, Direction::Short, size, 1, &[], None).unwrap();

        assert_eq!(levels.iter().map(|l| l.size).sum::<u64>(), size);
        assert!(levels.windows(2).all(|w| w[0].price >= w[1].price));
        assert!(levels[0].price <= TOP);

        let exact = calculate_base_swap_output(&amm, size, SwapDirection::Add)
            .unwrap()
            .quote_asset_amount;
        assert!(split_notional(&levels) <= exact);
    }

    #[test]
    fn last_look_shades_to_an_in_band_rival() {
        let amm = amm_fixture();
        let size = 10 * BASE_PRECISION_U64;
        let rival_price = TOP + TOP / 100; // +1%, inside the 5% band
        let rival_levels = [PriceLevel {
            price: rival_price,
            size: BASE_PRECISION_U64,
        }];
        let levels = vamm_quote_levels(
            &amm,
            Direction::Long,
            size,
            1,
            &rival_book(&rival_levels),
            None,
        )
        .unwrap();

        // The slice of curve cheaper than the rival is quoted AT the rival's
        // price (winning the tie by tier priority), and it comes first.
        assert_eq!(levels[0].price, rival_price);
        assert!(levels[0].size > 0);
        assert_eq!(levels.iter().map(|l| l.size).sum::<u64>(), size);
        assert!(levels.windows(2).all(|w| w[0].price <= w[1].price));

        // Shading only raises quoted notional: still at-or-better.
        let exact = calculate_base_swap_output(&amm, size, SwapDirection::Remove)
            .unwrap()
            .quote_asset_amount;
        assert!(split_notional(&levels) >= exact);
    }

    #[test]
    fn taker_limit_caps_the_ladder_at_an_honest_final_rung() {
        let amm = amm_fixture();
        let size = 10 * BASE_PRECISION_U64;
        // +0.5% — far tighter than the curve impact of a 10-unit take.
        let limit = TOP + TOP / 200;
        let levels = vamm_quote_levels(&amm, Direction::Long, size, 1, &[], Some(limit)).unwrap();

        // The ladder quotes exactly the reachable slice: nonzero, smaller
        // than the request, every rung within the limit. Rungs are priced at
        // their slice's true average cost, so the deepest one sits at-or-
        // under the limit (the limit bounds where the curve was cut, not
        // what the last slice costs).
        let quoted: u64 = levels.iter().map(|l| l.size).sum();
        assert!(quoted > 0);
        assert!(quoted < size);
        assert!(levels.iter().all(|l| l.price <= limit));

        // Same slice, no limit: identical pricing for the shared prefix
        // cumulative — the limit only truncates, never reprices.
        let exact = calculate_base_swap_output(&amm, quoted, SwapDirection::Remove)
            .unwrap()
            .quote_asset_amount;
        assert!(split_notional(&levels) >= exact);
    }

    #[test]
    fn taker_limit_crossing_the_top_empties_the_book() {
        let amm = amm_fixture();
        let size = 10 * BASE_PRECISION_U64;
        // Below the ask top: the vAMM can't fill a buyer within this limit.
        let limit = TOP - TOP / 100;
        assert!(
            vamm_quote_levels(&amm, Direction::Long, size, 1, &[], Some(limit))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rival_rungs_past_the_taker_limit_are_dropped() {
        let amm = amm_fixture();
        let size = 10 * BASE_PRECISION_U64;
        let limit = TOP + TOP / 100; // +1%
        let rival_levels = [PriceLevel {
            price: TOP + TOP / 50, // +2%: in band, but past the limit
            size: BASE_PRECISION_U64,
        }];
        let levels = vamm_quote_levels(
            &amm,
            Direction::Long,
            size,
            1,
            &rival_book(&rival_levels),
            Some(limit),
        )
        .unwrap();
        // No rung priced past the limit — the unfillable rival never becomes
        // a rung that would drag a fillable slice out of the book.
        assert!(levels.iter().all(|l| l.price <= limit));
        assert!(levels.iter().map(|l| l.size).sum::<u64>() > 0);
    }

    #[test]
    fn out_of_band_and_crossing_rivals_are_ignored() {
        let amm = amm_fixture();
        let size = 10 * BASE_PRECISION_U64;
        let garbage = [
            // 10x the top: outside the band — must not inflate the book.
            PriceLevel {
                price: TOP * 10,
                size: BASE_PRECISION_U64,
            },
            // Below our top (crossing): not a shading target.
            PriceLevel {
                price: TOP - TOP / 100,
                size: BASE_PRECISION_U64,
            },
        ];
        let shaded =
            vamm_quote_levels(&amm, Direction::Long, size, 1, &rival_book(&garbage), None).unwrap();
        let honest = vamm_quote_levels(&amm, Direction::Long, size, 1, &[], None).unwrap();
        assert_eq!(shaded, honest);
    }

    #[test]
    fn size_caps_at_available_liquidity() {
        let amm = amm_fixture();
        let levels = vamm_quote_levels(&amm, Direction::Long, u64::MAX, 1, &[], None).unwrap();
        let available =
            calculate_amm_available_liquidity(&amm, &PositionDirection::Long, 1).unwrap();
        assert_eq!(levels.iter().map(|l| l.size).sum::<u64>(), available);
    }

    #[test]
    fn zero_size_is_empty() {
        let amm = amm_fixture();
        assert!(vamm_quote_levels(&amm, Direction::Long, 0, 1, &[], None)
            .unwrap()
            .is_empty());
    }
}

#[cfg(test)]
mod ts_mirror_fixture {
    //! Emits a ladder for a fixed AMM so the TypeScript mirror
    //! (`packages/sdk/src/math/vammLadder.ts`) can be checked against the
    //! program's own numbers rather than against a reading of this file.
    //! Run: `cargo test -p velocity --lib ts_mirror_fixture -- --nocapture`
    use {
        super::*,
        crate::math::constants::{AMM_RESERVE_PRECISION, BASE_PRECISION_U64, PEG_PRECISION},
    };

    #[test]
    fn amm_is_copy_so_a_view_ix_can_quote_without_mutating() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<AMM>();
    }

    #[test]
    fn print_ladder_for_ts_mirror() {
        let mut amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            terminal_quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 50 * PEG_PRECISION,
            min_base_asset_reserve: 50 * AMM_RESERVE_PRECISION,
            max_base_asset_reserve: 200 * AMM_RESERVE_PRECISION,
            max_fill_reserve_fraction: 4,
            ..AMM::default()
        };
        amm.seed_no_spread_quote_state();
        for (label, direction) in [("long", Direction::Long), ("short", Direction::Short)] {
            let levels =
                vamm_quote_levels(&amm, direction, 10 * BASE_PRECISION_U64, 1, &[], None).unwrap();
            let encoded: Vec<String> = levels
                .iter()
                .map(|l| format!("{}:{}", l.price, l.size))
                .collect();
            println!("TS_MIRROR {} {}", label, encoded.join(","));
        }
    }
}
