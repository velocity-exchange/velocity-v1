//! vAMM leg of the router's quoter interface. It turns the AMM curve into
//! discrete [`PriceLevelV0`]s for the router split.
//!
//! The vAMM runs in this program and never goes through a CPI leg, which is
//! what makes last look possible. The router quotes every CPI book first and
//! hands those books here, and rival prices become ladder rungs. A slice of
//! curve that is cheaper than a rival's price is quoted at the rival's price.
//! The vAMM wins the tie by tier priority and keeps the difference for the LPs
//! instead of giving it to the taker as price improvement.
//!
//! A rung shades only base that the rivals at its price also fill in the same
//! take. The split fills the curve up to `reach(P)`, the base at which the
//! curve reaches the rival price `P`, and all rival depth `R` priced better
//! than `P` first. Rival depth at or inside the vAMM's top counts in `R`. A
//! rung therefore covers at most `min(d, total - reach(P) - R)` base, where `d`
//! is the rival depth at `P`. Its surcharge over the honest curve is at most
//! that base times `P - top`, plus rounding. A rival that the take does not
//! reach shades nothing, so an order that never trades cannot reprice the
//! vAMM. Rival depth counts only the levels the split reads.
//!
//! Every checkpoint's price comes from the swap math the execute leg runs,
//! which is `calculate_base_swap_output` over the spread reserves. A rung's
//! price is the exact per-unit cost of its own slice, rounded against the
//! taker. Each level therefore bounds what the AMM charges for it, above for a
//! long and below for a short. At-or-better holds by construction rather than
//! by tolerance. A shading rung covers the last base before the curve reaches
//! the rival price. The honest slices on each side of it keep the book
//! monotone, and the rest of the curve is priced honestly.
//!
//! Rival rungs are honored only within [`LAST_LOOK_BAND`] of the vAMM's top, so
//! a bad price from an approved quoter cannot inflate this book. Beyond the
//! band the curve is priced by equal-size checkpoints.

use {
    super::{
        controller::{calculate_base_swap_output, SwapDirection},
        math::{
            amm::{calculate_amm_available_liquidity, calculate_price},
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
            router::{QuoterBook, MAX_LEVELS_PER_BOOK},
            safe_math::SafeMath,
        },
        state::{
            prop_amm::{DirectionV0, PriceLevelV0},
            quoter::{QuoteContext, QuoterFill, RouterQuoter},
        },
    },
};

/// Equal-size honest checkpoints per quote, and the most rival rungs kept.
/// Each rival rung adds at most two checkpoints.
pub const VAMM_QUOTE_CHECKPOINTS: usize = 8;

/// Rival prices are honored as shading rungs only within this fraction of the
/// vAMM's top price. The value is 5% in `PERCENTAGE_PRECISION`.
pub const LAST_LOOK_BAND: u64 = PERCENTAGE_PRECISION_U64 / 20;

/// Quote the vAMM as best-first ladder levels covering `min(size, available)`.
/// The levels are shaded toward `rival_books` within the last-look band, and
/// only for base those books also fill.
///
/// `taker_limit` bounds the ladder. The curve inversion finds the cumulative
/// where the marginal price reaches the limit, and the total is capped there.
/// Every rung's true cost is then inside the limit, because a slice's average
/// never exceeds its end marginal. A limit that the swap's first marginal
/// already passes gives an empty ladder. The limit is the taker's own price,
/// so the shading band does not apply to it.
///
/// The cap happens here, so the ladder's book is authoritative for the limit. A
/// caller must not truncate it again by comparing rung prices to the limit. A
/// dust rung's notional rounds up to a whole lamport, which pushes its quoted
/// per-unit price past the limit that admitted it. Dropping that rung would
/// cost the taker liquidity they can afford.
pub fn vamm_quote_levels(
    amm: &AMM,
    direction: DirectionV0,
    size: u64,
    step_size: u64,
    rival_books: &[QuoterBook],
    taker_limit: Option<u64>,
) -> VelocityResult<Vec<PriceLevelV0>> {
    let (position_direction, swap_direction) = match direction {
        DirectionV0::Long => (PositionDirection::Long, SwapDirection::Remove),
        DirectionV0::Short => (PositionDirection::Short, SwapDirection::Add),
    };
    let available = calculate_amm_available_liquidity(amm, &position_direction, step_size)?;
    let mut total = size.min(available);
    if total == 0 {
        return Ok(vec![]);
    }

    // The swap's first marginal price on the spread reserves it runs on. It
    // differs from `ask_price` and `bid_price` by about `R x^2 / 4` for a
    // composite spread `x`.
    let top = match direction {
        DirectionV0::Long => calculate_price(
            amm.ask_quote_asset_reserve,
            amm.ask_base_asset_reserve,
            amm.peg_multiplier,
        )?,
        DirectionV0::Short => calculate_price(
            amm.bid_quote_asset_reserve,
            amm.bid_base_asset_reserve,
            amm.peg_multiplier,
        )?,
    };

    if let Some(limit) = taker_limit {
        let crossed_at_top = match direction {
            DirectionV0::Long => limit < top,
            DirectionV0::Short => limit > top,
        };

        if crossed_at_top {
            return Ok(vec![]);
        }

        // The reach runs on the same reserves as `top` but rounds differently.
        // Its direction is the exact test that some base fills within the limit.
        let (reachable, trade_direction) =
            calculate_base_asset_amount_to_trade_to_price(amm, limit, position_direction)?;
        if trade_direction != position_direction {
            return Ok(vec![]);
        }

        total = total.min(reachable);
        if total == 0 {
            return Ok(vec![]);
        }
    }

    let band_edge = {
        let band = (top as u128)
            .safe_mul(LAST_LOOK_BAND as u128)?
            .safe_div(PERCENTAGE_PRECISION_U64 as u128)? as u64;
        match direction {
            DirectionV0::Long => top.safe_add(band)?,
            DirectionV0::Short => top.safe_sub(band)?,
        }
    };

    // Last look: a rival price beyond the vAMM's top becomes a shading rung
    // when within the band and the taker's limit, best price first. The vAMM
    // already wins by price priority, so shading to that price is LP surplus
    // at no maker's cost. A rung past the limit is dropped here as unfillable.
    let rung_edge = match (taker_limit, direction) {
        (Some(limit), DirectionV0::Long) => band_edge.min(limit),
        (Some(limit), DirectionV0::Short) => band_edge.max(limit),
        (None, _) => band_edge,
    };

    let step = step_size.max(1);
    let rivals = RivalRungs::collect(rival_books, direction, top, rung_edge, step);

    // Each checkpoint holds a cumulative base and, for a rival rung, its
    // shading price. A rung shades the last base before the curve reaches its
    // price, floored to the step. The honest slice before the rung is
    // cheaper and the slice after it is dearer, so the book stays monotone.
    // Equal-size checkpoints price the rest of the curve honestly. The emit
    // loop skips a checkpoint that does not advance the ladder.
    let chunk = (total / VAMM_QUOTE_CHECKPOINTS as u64).max(1);
    let grid_point = |k: usize| match k {
        VAMM_QUOTE_CHECKPOINTS => total,
        _ => total.min(chunk * k as u64),
    };

    let mut checkpoints: Vec<(u64, Option<u64>)> = Vec::with_capacity(3 * VAMM_QUOTE_CHECKPOINTS);
    let mut grid_index = 1usize;
    let mut rival_depth_ahead = rivals.depth_inside_top;
    let mut covered = 0u64;
    for rung in rivals.rungs() {
        let depth_better_than_rung = rival_depth_ahead;
        rival_depth_ahead = rival_depth_ahead.saturating_add(rung.size);
        let (reach, trade_direction) =
            calculate_base_asset_amount_to_trade_to_price(amm, rung.price, position_direction)?;
        if trade_direction != position_direction {
            continue;
        }

        // The rivals at this price trade only the take that the curve up to
        // `reach` and every better rival leave over.
        let shade_end = reach.min(total) - reach.min(total) % step;
        let shade_budget = rung.size.min(
            total
                .saturating_sub(reach)
                .saturating_sub(depth_better_than_rung),
        );
        let shade_start = shade_end
            .saturating_sub(shade_budget - shade_budget % step)
            .max(covered);
        if shade_start >= shade_end {
            continue;
        }

        while grid_index <= VAMM_QUOTE_CHECKPOINTS && grid_point(grid_index) < shade_start {
            checkpoints.push((grid_point(grid_index), None));
            grid_index += 1;
        }

        while grid_index <= VAMM_QUOTE_CHECKPOINTS && grid_point(grid_index) <= shade_end {
            grid_index += 1;
        }

        checkpoints.push((shade_start, None));
        checkpoints.push((shade_end, Some(rung.price)));
        covered = shade_end;
    }

    checkpoints.extend((grid_index..=VAMM_QUOTE_CHECKPOINTS).map(|k| (grid_point(k), None)));

    // Emit step-aligned rungs priced off the swap math that executes. Two
    // invariants hold together.
    //
    //  * `size` is a multiple of `order_step_size`. The split allocates in step
    //    quanta, so a rung's sub-step tail is floored away, and the vAMM then
    //    under-quotes its depth.
    //  * `price` is a true per-unit bound on its own slice. The slice's exact
    //    notional comes from `calculate_base_swap_output`, the same call the
    //    execute leg makes over the spread reserves. The raw-invariant marginal
    //    price diverges from what the AMM charges, because the spread quote
    //    reserve is not the invariant's. A rival rung keeps its shading price
    //    when that price is the worse of the two for the taker. A running bound
    //    keeps the book monotone where rounding or that divergence puts a slice
    //    past the rung before it. The split truncates a book at its first
    //    non-monotone level.
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
            DirectionV0::Long => exact.safe_div_ceil(size as u128)?,
            DirectionV0::Short => exact.safe_div(size as u128)?,
        }
        .cast::<u64>()?;
        let price = match direction {
            DirectionV0::Long => shade.unwrap_or(0).max(honest).max(bound.unwrap_or(0)),
            DirectionV0::Short => shade
                .unwrap_or(u64::MAX)
                .min(honest)
                .min(bound.unwrap_or(u64::MAX)),
        };

        if price == 0 {
            break;
        }

        bound = Some(price);
        levels.push(PriceLevelV0 { price, size });
        previous = cumulative;
        previous_notional = notional;
    }

    Ok(levels)
}

/// The rival depth the shade budget reads, over the levels the split reads.
struct RivalRungs {
    /// The best [`VAMM_QUOTE_CHECKPOINTS`] in-band prices, best first and
    /// deduped, each with the rivals' depth at that price.
    rungs: [PriceLevelV0; VAMM_QUOTE_CHECKPOINTS],
    rung_count: usize,
    /// Rival depth priced at or better than the vAMM's top. The split fills it
    /// before any rung.
    depth_inside_top: u64,
}

impl RivalRungs {
    /// An insert sort keeps the rungs in a fixed array instead of a
    /// collect-and-truncate, whose doubling buffers the allocator never
    /// reclaims. A level that the full array drops is worse than every rung it
    /// keeps, so no kept rung needs its depth.
    fn collect(
        rival_books: &[QuoterBook],
        direction: DirectionV0,
        top: u64,
        rung_edge: u64,
        step: u64,
    ) -> Self {
        let mut rivals = Self {
            rungs: [PriceLevelV0::default(); VAMM_QUOTE_CHECKPOINTS],
            rung_count: 0,
            depth_inside_top: 0,
        };

        let levels = rival_books
            .iter()
            .flat_map(|book| split_readable_levels(book.levels, direction, step));
        for level in levels {
            let (inside_top, in_band) = match direction {
                DirectionV0::Long => (level.price <= top, level.price <= rung_edge),
                DirectionV0::Short => (level.price >= top, level.price >= rung_edge),
            };

            if inside_top {
                rivals.depth_inside_top = rivals.depth_inside_top.saturating_add(level.size);
            } else if in_band {
                rivals.insert(direction, level);
            }
        }

        rivals
    }

    fn insert(&mut self, direction: DirectionV0, level: PriceLevelV0) {
        let ranks_before = |a: u64, b: u64| match direction {
            DirectionV0::Long => a < b,
            DirectionV0::Short => a > b,
        };

        let mut at = 0usize;
        while at < self.rung_count && ranks_before(self.rungs[at].price, level.price) {
            at += 1;
        }

        if at < self.rung_count && self.rungs[at].price == level.price {
            self.rungs[at].size = self.rungs[at].size.saturating_add(level.size);
            return;
        }

        if at >= VAMM_QUOTE_CHECKPOINTS {
            return;
        }

        // Shift the worse rungs down one. A full array drops its worst rung.
        let end = self.rung_count.min(VAMM_QUOTE_CHECKPOINTS - 1);
        if at < end {
            self.rungs.copy_within(at..end, at + 1);
        }

        self.rungs[at] = level;
        self.rung_count = (self.rung_count + 1).min(VAMM_QUOTE_CHECKPOINTS);
    }

    fn rungs(&self) -> &[PriceLevelV0] {
        &self.rungs[..self.rung_count]
    }
}

/// The levels of one book that [`crate::math::router::split_across_quoters`]
/// can allocate, each size floored to the step. This matches the split's
/// cursor. It reads at most [`MAX_LEVELS_PER_BOOK`] levels, stops at the first
/// level out of price order, and skips a zero price or a sub-step size.
fn split_readable_levels(
    levels: &[PriceLevelV0],
    direction: DirectionV0,
    step: u64,
) -> impl Iterator<Item = PriceLevelV0> + '_ {
    let capped = &levels[..levels.len().min(MAX_LEVELS_PER_BOOK)];
    let monotone = capped
        .windows(2)
        .position(|pair| match direction {
            DirectionV0::Long => pair[1].price < pair[0].price,
            DirectionV0::Short => pair[1].price > pair[0].price,
        })
        .map_or(capped.len(), |last_in_order| last_in_order + 1);
    capped[..monotone].iter().filter_map(move |level| {
        let size = level.size - level.size % step;
        (level.price != 0 && size != 0).then_some(PriceLevelV0 {
            price: level.price,
            size,
        })
    })
}

impl RouterQuoter for AmmQuoter<'_> {
    /// The vAMM's router quote is the shaded ladder, and `rival_books` is the
    /// last look. The AMM's per-fill refresh happens in `AmmQuoter::refresh`,
    /// which the fill controller runs before the router quotes.
    #[cfg(test)]
    fn quote(
        &self,
        ctx: &QuoteContext,
        direction: DirectionV0,
        size: u64,
        rival_books: &[QuoterBook],
    ) -> VelocityResult<Vec<PriceLevelV0>> {
        vamm_quote_levels(self.amm, direction, size, ctx.step_size, rival_books, None)
    }

    fn execute(
        &mut self,
        ctx: &QuoteContext,
        direction: DirectionV0,
        size: u64,
    ) -> VelocityResult<QuoterFill> {
        let side = PositionDirection::from(direction);
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
            vlp::amm::{
                controller::calculate_base_swap_output,
                math::spread::refresh_cached_spread_reserves,
            },
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

    fn rival_book(levels: &[PriceLevelV0]) -> Vec<QuoterBook<'_>> {
        vec![QuoterBook {
            priority: 10,
            levels,
            withheld: PriceLevelV0::default(),
        }]
    }

    /// The split's floored per-level notional, as `math::router` computes it.
    fn split_notional(levels: &[PriceLevelV0]) -> u64 {
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
        let levels = vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &[], None).unwrap();

        assert_eq!(levels.iter().map(|l| l.size).sum::<u64>(), size);
        assert!(levels.windows(2).all(|w| w[0].price <= w[1].price));
        assert!(levels[0].price >= TOP);

        // Marginal-end pricing bounds each slice's true average cost from
        // above, so the floored quoted notional covers the exact one-shot swap.
        let exact = calculate_base_swap_output(&amm, size, SwapDirection::Remove)
            .unwrap()
            .quote_asset_amount;
        assert!(split_notional(&levels) >= exact);
    }

    #[test]
    fn short_ladder_is_monotone_and_at_or_better() {
        let amm = amm_fixture();
        let size = 10 * BASE_PRECISION_U64;
        let levels = vamm_quote_levels(&amm, DirectionV0::Short, size, 1, &[], None).unwrap();

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
        let rival_levels = [PriceLevelV0 {
            price: rival_price,
            size: BASE_PRECISION_U64,
        }];
        let levels = vamm_quote_levels(
            &amm,
            DirectionV0::Long,
            size,
            1,
            &rival_book(&rival_levels),
            None,
        )
        .unwrap();

        // The slice of curve cheaper than the rival is quoted at the rival's
        // price, because the vAMM wins the tie by tier priority. It comes first.
        assert_eq!(levels[0].price, rival_price);
        assert!(levels[0].size > 0);
        assert_eq!(levels.iter().map(|l| l.size).sum::<u64>(), size);
        assert!(levels.windows(2).all(|w| w[0].price <= w[1].price));

        // Shading only raises the quoted notional, so at-or-better still holds.
        let exact = calculate_base_swap_output(&amm, size, SwapDirection::Remove)
            .unwrap()
            .quote_asset_amount;
        assert!(split_notional(&levels) >= exact);
    }

    /// A rival that offers one step of depth reprices one step of curve. The
    /// rest of the ladder is the honest curve, so resting a dust order in the
    /// band cannot make the taker pay the rival price for the whole slice.
    #[test]
    fn last_look_shades_only_the_depth_a_rival_offers() {
        let amm = amm_fixture();
        let size = 10 * BASE_PRECISION_U64;
        let rival_price = TOP + TOP / 100; // +1%, inside the 5% band
        let dust = [PriceLevelV0 {
            price: rival_price,
            size: BASE_PRECISION_U64 / 1000,
        }];
        let deep = [PriceLevelV0 {
            price: rival_price,
            size,
        }];
        let shaded_by_dust =
            vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &rival_book(&dust), None).unwrap();
        let shaded_by_depth =
            vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &rival_book(&deep), None).unwrap();
        let honest = vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &[], None).unwrap();

        assert!(shaded_by_dust
            .iter()
            .any(|l| l.price == rival_price && l.size == dust[0].size));
        assert_eq!(shaded_by_depth[0].price, rival_price);
        assert!(shaded_by_depth[0].size > dust[0].size);

        // The dust order moves the taker's bill by no more than the rung it
        // paid for. Real depth at the same price reprices far more.
        let dust_cost = split_notional(&shaded_by_dust) - split_notional(&honest);
        let depth_cost = split_notional(&shaded_by_depth) - split_notional(&honest);
        assert!(
            depth_cost > dust_cost * 100,
            "{} vs {}",
            depth_cost,
            dust_cost
        );
    }

    /// 1M base reserve, so a 100-unit take moves the curve far less than the
    /// last-look band.
    fn deep_amm_fixture() -> AMM {
        let mut amm = AMM {
            base_asset_reserve: 1_000_000 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 1_000_000 * AMM_RESERVE_PRECISION,
            terminal_quote_asset_reserve: 1_000_000 * AMM_RESERVE_PRECISION,
            sqrt_k: 1_000_000 * AMM_RESERVE_PRECISION,
            peg_multiplier: 50 * PEG_PRECISION,
            min_base_asset_reserve: 500_000 * AMM_RESERVE_PRECISION,
            max_base_asset_reserve: 2_000_000 * AMM_RESERVE_PRECISION,
            max_fill_reserve_fraction: 4,
            ..AMM::default()
        };

        amm.seed_no_spread_quote_state();
        amm
    }

    /// [`deep_amm_fixture`] with a nonzero spread on both sides and the cached
    /// spread reserves the swap reads.
    fn deep_spread_amm_fixture(spread: u32) -> AMM {
        let mut amm = deep_amm_fixture();
        amm.long_spread = spread;
        amm.short_spread = spread;
        refresh_cached_spread_reserves(&mut amm).unwrap();
        amm
    }

    /// The ask reserves put the swap's first marginal at about
    /// `ask_price + R x^2 / 4`. A long limit in that window trades nothing, so
    /// the vAMM must quote nothing rather than a ladder the limit does not cap.
    #[test]
    fn long_limit_below_the_first_marginal_empties_the_book() {
        let amm = deep_spread_amm_fixture(10_000);
        let spread_ask = amm
            .ask_price(amm.reserve_price().unwrap(), amm.long_spread, 0)
            .unwrap();
        let size = 10_000 * BASE_PRECISION_U64;

        let window_limit = spread_ask + 100;
        let (_, trade_direction) = calculate_base_asset_amount_to_trade_to_price(
            &amm,
            window_limit,
            PositionDirection::Long,
        )
        .unwrap();
        assert_eq!(trade_direction, PositionDirection::Short);

        let levels =
            vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &[], Some(window_limit)).unwrap();
        assert!(levels.is_empty(), "{:?}", levels);

        // A limit past the first marginal still gets the slice it can reach.
        let reachable_limit = spread_ask + spread_ask / 1000;
        let levels =
            vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &[], Some(reachable_limit))
                .unwrap();
        let quoted: u64 = levels.iter().map(|l| l.size).sum();
        assert!(quoted > 0 && quoted < size);
        let exact = calculate_base_swap_output(&amm, quoted, SwapDirection::Remove)
            .unwrap()
            .quote_asset_amount;
        assert!(
            exact as u128 * BASE_PRECISION_U64 as u128 <= reachable_limit as u128 * quoted as u128
        );
    }

    /// On a deep curve a 100-unit take never reaches a rival inside the band.
    /// The rival fills nothing, so it shades nothing at any depth or limit.
    #[test]
    fn a_rival_the_take_does_not_reach_shades_nothing() {
        let amm = deep_amm_fixture();
        let size = 100 * BASE_PRECISION_U64;

        for (direction, rival_price) in [
            (DirectionV0::Long, TOP + TOP * 49 / 1000),
            (DirectionV0::Long, TOP + TOP / 100),
            (DirectionV0::Short, TOP - TOP * 49 / 1000),
            (DirectionV0::Short, TOP - TOP / 100),
        ] {
            for taker_limit in [None, Some(rival_price)] {
                let honest = vamm_quote_levels(&amm, direction, size, 1, &[], taker_limit).unwrap();
                for depth in [
                    BASE_PRECISION_U64 / 1000,
                    10 * BASE_PRECISION_U64,
                    100 * BASE_PRECISION_U64,
                    30_000 * BASE_PRECISION_U64,
                ] {
                    let rival = [PriceLevelV0 {
                        price: rival_price,
                        size: depth,
                    }];
                    let shaded = vamm_quote_levels(
                        &amm,
                        direction,
                        size,
                        1,
                        &rival_book(&rival),
                        taker_limit,
                    )
                    .unwrap();

                    assert_eq!(shaded, honest, "{} at {}", depth, rival_price);
                }
            }
        }
    }

    /// A take past `reach(P)` fills the rival as well. The rung shades at most
    /// the take past `reach(P)`, however deep the rival is, and the surcharge
    /// stays within that base times `P - top`.
    #[test]
    fn shade_is_bounded_by_the_take_past_the_rival_price() {
        let amm = deep_amm_fixture();
        let past_reach = 500 * BASE_PRECISION_U64;

        for (direction, rival_price) in [
            (DirectionV0::Long, TOP + TOP / 200),
            (DirectionV0::Short, TOP - TOP / 200),
        ] {
            let (reach, _) = calculate_base_asset_amount_to_trade_to_price(
                &amm,
                rival_price,
                PositionDirection::from(direction),
            )
            .unwrap();
            let size = reach + past_reach;
            let honest = vamm_quote_levels(&amm, direction, size, 1, &[], None).unwrap();

            for depth in [100 * BASE_PRECISION_U64, reach] {
                let rival = [PriceLevelV0 {
                    price: rival_price,
                    size: depth,
                }];
                let shaded =
                    vamm_quote_levels(&amm, direction, size, 1, &rival_book(&rival), None).unwrap();

                assert_eq!(shaded.iter().map(|l| l.size).sum::<u64>(), size);
                assert!(shaded.windows(2).all(|w| match direction {
                    DirectionV0::Long => w[0].price <= w[1].price,
                    DirectionV0::Short => w[0].price >= w[1].price,
                }));

                let shade_budget = depth.min(past_reach);
                let shaded_base: u64 = shaded
                    .iter()
                    .filter(|l| l.price == rival_price)
                    .map(|l| l.size)
                    .sum();
                assert!(shaded_base > 0 && shaded_base <= shade_budget);

                // Each rung's price rounds by under one unit, so the two ladders'
                // notionals differ by under one lamport per base unit and per rung.
                let rounding = size / BASE_PRECISION_U64 + shaded.len() as u64;
                let surcharge = split_notional(&shaded).abs_diff(split_notional(&honest));
                let bound = (shade_budget as u128 * rival_price.abs_diff(TOP) as u128
                    / BASE_PRECISION_U64 as u128) as u64;
                assert!(surcharge <= bound + rounding, "{} > {}", surcharge, bound);
            }
        }
    }

    /// Depth at or inside the vAMM's top fills before a rival at `P`. A take of
    /// `2 reach(P)` against `reach(P)` of such depth never reaches the rival, so
    /// the ladder is the honest curve and the taker pays no surcharge.
    #[test]
    fn econ_probe_better_source_hides_the_rival_from_the_shade_budget() {
        let amm = deep_amm_fixture();
        for divisor in [200u64, 100, 50, 25] {
            let rival_price = TOP + TOP / divisor;
            let (reach, _) = calculate_base_asset_amount_to_trade_to_price(
                &amm,
                rival_price,
                PositionDirection::Long,
            )
            .unwrap();
            let size = 2 * reach;
            let inside_top = [PriceLevelV0 {
                price: TOP,
                size: reach,
            }];
            let wall = [PriceLevelV0 {
                price: rival_price,
                size: reach,
            }];
            let rivals = [
                QuoterBook {
                    priority: 10,
                    levels: &inside_top,
                    withheld: PriceLevelV0::default(),
                },
                QuoterBook {
                    priority: 10,
                    levels: &wall,
                    withheld: PriceLevelV0::default(),
                },
            ];

            let honest = vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &[], None).unwrap();
            let shaded =
                vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &rivals, None).unwrap();
            assert_eq!(shaded, honest, "wall at +{} bps", 10_000 / divisor);
        }
    }

    /// Rival depth below the top fills first, so the rival at `P` it covers
    /// shades nothing. Two rungs: the take past `reach(P2)` is only the depth
    /// at `P1`, so the rival at `P2` fills nothing and shades nothing.
    #[test]
    fn rival_depth_better_than_a_rung_is_charged_to_the_take() {
        let amm = amm_fixture();
        let wall_price = TOP + TOP / 50;
        let (wall_reach, _) = calculate_base_asset_amount_to_trade_to_price(
            &amm,
            wall_price,
            PositionDirection::Long,
        )
        .unwrap();
        let inside = 5 * BASE_PRECISION_U64;
        let levels = [
            PriceLevelV0 {
                price: TOP - TOP / 500,
                size: inside,
            },
            PriceLevelV0 {
                price: wall_price,
                size: 5 * BASE_PRECISION_U64,
            },
        ];
        let size = wall_reach + inside;
        let shaded =
            vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &rival_book(&levels), None)
                .unwrap();
        let honest = vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &[], None).unwrap();
        assert_eq!(shaded, honest);

        let near = TOP + TOP / 200;
        let far = TOP + TOP / 50;
        let near_depth = BASE_PRECISION_U64 / 10;
        let (far_reach, _) =
            calculate_base_asset_amount_to_trade_to_price(&amm, far, PositionDirection::Long)
                .unwrap();
        let two_rungs = [
            PriceLevelV0 {
                price: near,
                size: near_depth,
            },
            PriceLevelV0 {
                price: far,
                size: 5 * BASE_PRECISION_U64,
            },
        ];
        let shaded = vamm_quote_levels(
            &amm,
            DirectionV0::Long,
            far_reach + near_depth,
            1,
            &rival_book(&two_rungs),
            None,
        )
        .unwrap();
        assert!(
            shaded.iter().all(|level| level.price != far),
            "{:?}",
            shaded
        );
        assert!(shaded.iter().any(|level| level.price == near));
    }

    /// A level the split never reads gives the shade no depth: one past the
    /// first level out of price order, and one past `MAX_LEVELS_PER_BOOK`.
    #[test]
    fn rival_depth_counts_only_levels_the_split_reads() {
        let amm = amm_fixture();
        let size = 10 * BASE_PRECISION_U64;
        let rival_price = TOP + TOP / 100;
        let readable = [PriceLevelV0 {
            price: rival_price,
            size: BASE_PRECISION_U64 / 10,
        }];
        let out_of_order = [
            readable[0],
            PriceLevelV0 {
                price: TOP + TOP / 200,
                size,
            },
        ];
        let expected = vamm_quote_levels(
            &amm,
            DirectionV0::Long,
            size,
            1,
            &rival_book(&readable),
            None,
        )
        .unwrap();
        let shaded = vamm_quote_levels(
            &amm,
            DirectionV0::Long,
            size,
            1,
            &rival_book(&out_of_order),
            None,
        )
        .unwrap();
        assert_eq!(shaded, expected);

        let mut deep = vec![
            PriceLevelV0 {
                price: TOP - TOP / 100,
                size: 1,
            };
            MAX_LEVELS_PER_BOOK
        ];
        let read =
            vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &rival_book(&deep), None).unwrap();
        deep.push(PriceLevelV0 {
            price: rival_price,
            size,
        });
        let past_the_cap =
            vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &rival_book(&deep), None).unwrap();
        assert_eq!(past_the_cap, read);
    }

    #[test]
    fn taker_limit_caps_the_ladder_at_an_honest_final_rung() {
        let amm = amm_fixture();
        let size = 10 * BASE_PRECISION_U64;
        // +0.5%, far tighter than the curve impact of a 10-unit take.
        let limit = TOP + TOP / 200;
        let levels = vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &[], Some(limit)).unwrap();

        // The ladder quotes the reachable slice: nonzero, smaller than the
        // request, every rung within the limit. Each rung prices at its
        // slice's true average cost, so the limit bounds where the curve was
        // cut, not what the last slice costs.
        let quoted: u64 = levels.iter().map(|l| l.size).sum();
        assert!(quoted > 0);
        assert!(quoted < size);
        assert!(levels.iter().all(|l| l.price <= limit));

        // The same slice without a limit prices the shared prefix cumulative
        // identically. The limit only truncates, it never reprices.
        let exact = calculate_base_swap_output(&amm, quoted, SwapDirection::Remove)
            .unwrap()
            .quote_asset_amount;
        assert!(split_notional(&levels) >= exact);
    }

    #[test]
    fn taker_limit_crossing_the_top_empties_the_book() {
        let amm = amm_fixture();
        let size = 10 * BASE_PRECISION_U64;
        // Below the ask top. The vAMM cannot fill a buyer within this limit.
        let limit = TOP - TOP / 100;
        assert!(
            vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &[], Some(limit))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rival_rungs_past_the_taker_limit_are_dropped() {
        let amm = amm_fixture();
        let size = 10 * BASE_PRECISION_U64;
        let limit = TOP + TOP / 100; // +1%
        let rival_levels = [PriceLevelV0 {
            price: TOP + TOP / 50, // +2%, in band but past the limit
            size: BASE_PRECISION_U64,
        }];
        let levels = vamm_quote_levels(
            &amm,
            DirectionV0::Long,
            size,
            1,
            &rival_book(&rival_levels),
            Some(limit),
        )
        .unwrap();
        // No rung is priced past the limit. The unfillable rival never becomes
        // a rung that would pull a fillable slice out of the book.
        assert!(levels.iter().all(|l| l.price <= limit));
        assert!(levels.iter().map(|l| l.size).sum::<u64>() > 0);
    }

    #[test]
    fn out_of_band_and_crossing_rivals_are_ignored() {
        let amm = amm_fixture();
        let size = 10 * BASE_PRECISION_U64;
        let garbage = [
            // 10x the top, outside the band. It must not inflate the book.
            PriceLevelV0 {
                price: TOP * 10,
                size: BASE_PRECISION_U64,
            },
            // Below the vAMM's top, so it crosses and is not a shading target.
            PriceLevelV0 {
                price: TOP - TOP / 100,
                size: BASE_PRECISION_U64,
            },
        ];
        let shaded = vamm_quote_levels(
            &amm,
            DirectionV0::Long,
            size,
            1,
            &rival_book(&garbage),
            None,
        )
        .unwrap();
        let honest = vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &[], None).unwrap();
        assert_eq!(shaded, honest);
    }

    #[test]
    fn size_caps_at_available_liquidity() {
        let amm = amm_fixture();
        let levels = vamm_quote_levels(&amm, DirectionV0::Long, u64::MAX, 1, &[], None).unwrap();
        let available =
            calculate_amm_available_liquidity(&amm, &PositionDirection::Long, 1).unwrap();
        assert_eq!(levels.iter().map(|l| l.size).sum::<u64>(), available);
    }

    #[test]
    fn zero_size_is_empty() {
        let amm = amm_fixture();
        assert!(vamm_quote_levels(&amm, DirectionV0::Long, 0, 1, &[], None)
            .unwrap()
            .is_empty());
    }
}

#[cfg(test)]
mod ts_mirror_fixture {
    //! Emits a ladder for a fixed AMM so the TypeScript mirror in
    //! `packages/sdk/src/math/vammLadder.ts` can be checked against the
    //! program's own numbers rather than against a reading of this file. Run it
    //! with `cargo test -p velocity --lib ts_mirror_fixture -- --nocapture`.
    use {
        super::*,
        crate::{
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I128, BASE_PRECISION_U64, PEG_PRECISION,
                    QUOTE_PRECISION_I128,
                },
                oracle::OracleValidity,
            },
            state::{
                oracle::{HistoricalOracleData, MMOraclePriceData, OraclePriceData},
                perp_market::MarketStats,
            },
            vlp::amm::math::spread::update_amm_quote_state,
        },
    };

    fn encode(levels: &[PriceLevelV0]) -> String {
        levels
            .iter()
            .map(|l| format!("{}:{}", l.price, l.size))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The 100-unit AMM with a dynamic spread and an inventory reference price
    /// offset. The program's own quote-state update sets the spreads and the
    /// cached spread reserves, against an oracle at the reserve price.
    fn spread_amm() -> AMM {
        let reserves = 100 * AMM_RESERVE_PRECISION;
        let base_asset_amount_with_amm = 2 * BASE_PRECISION_I128;
        let mut amm = AMM {
            base_asset_reserve: reserves,
            quote_asset_reserve: reserves,
            sqrt_k: reserves,
            peg_multiplier: 50 * PEG_PRECISION,
            min_base_asset_reserve: reserves / 2,
            max_base_asset_reserve: reserves * 2,
            max_fill_reserve_fraction: 4,
            base_spread: 2_000,
            max_spread: 50_000,
            curve_update_intensity: 200,
            base_asset_amount_with_amm,
            total_fee_minus_distributions: 1_000 * QUOTE_PRECISION_I128,
            ..AMM::default()
        };

        // The quote reserve after the pool's position closes, as a repeg sets it.
        amm.terminal_quote_asset_reserve =
            reserves * reserves / (reserves + base_asset_amount_with_amm as u128);

        let oracle_price = 50 * PEG_PRECISION as i64;
        let stats = MarketStats {
            last_mark_price_twap: 50_500_000,
            last_mark_price_twap_5min: 50_500_000,
            last_24h_avg_funding_rate: 1_000_000_000,
            funding_period: 3600,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price: oracle_price,
                last_oracle_price_twap: oracle_price,
                last_oracle_price_twap_5min: oracle_price,
                ..HistoricalOracleData::default()
            },
            ..MarketStats::default()
        };
        let oracle_price_data = OraclePriceData {
            price: oracle_price,
            confidence: 0,
            delay: 0,
            has_sufficient_number_of_data_points: true,
            sequence_id: None,
        };
        let mm =
            MMOraclePriceData::new(oracle_price, 0, 0, OracleValidity::Valid, oracle_price_data)
                .unwrap();
        let reserve_price = amm.reserve_price().unwrap();
        update_amm_quote_state(&mut amm, &stats, &mm, reserve_price, 0).unwrap();
        amm
    }

    /// Nonzero spreads and a nonzero offset, so the dump below exercises the
    /// spread reserves that every other fixture here leaves at the curve.
    #[test]
    fn print_spread_ladder_for_ts_mirror() {
        let amm = spread_amm();
        assert!(amm.long_spread > 0 && amm.short_spread > 0);
        assert_ne!(amm.reference_price_offset, 0);
        println!(
            "TS_MIRROR spread_state long_spread={} short_spread={} offset={} ask={}/{} bid={}/{}",
            amm.long_spread,
            amm.short_spread,
            amm.reference_price_offset,
            amm.ask_base_asset_reserve,
            amm.ask_quote_asset_reserve,
            amm.bid_base_asset_reserve,
            amm.bid_quote_asset_reserve,
        );

        let size = 10 * BASE_PRECISION_U64;
        for (label, direction) in [
            ("spread_long", DirectionV0::Long),
            ("spread_short", DirectionV0::Short),
        ] {
            let levels = vamm_quote_levels(&amm, direction, size, 1, &[], None).unwrap();
            println!("TS_MIRROR {} {}", label, encode(&levels));
        }

        // A rival 1% past the first marginal, and a limit just above the
        // spread-adjusted ask that the first marginal already passes.
        let ask_top = calculate_price(
            amm.ask_quote_asset_reserve,
            amm.ask_base_asset_reserve,
            amm.peg_multiplier,
        )
        .unwrap();
        let rival = [PriceLevelV0 {
            price: ask_top * 101 / 100,
            size: BASE_PRECISION_U64 / 10,
        }];
        let books = [QuoterBook {
            priority: 10,
            levels: &rival,
            withheld: PriceLevelV0::default(),
        }];
        let levels = vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &books, None).unwrap();
        println!(
            "TS_MIRROR spread_long_rival price={} {}",
            rival[0].price,
            encode(&levels)
        );

        let spread_ask = amm
            .ask_price(
                amm.reserve_price().unwrap(),
                amm.long_spread,
                amm.reference_price_offset,
            )
            .unwrap();
        let window_limit = spread_ask + 1;
        assert!(window_limit < ask_top);
        let levels =
            vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &[], Some(window_limit)).unwrap();
        assert!(levels.is_empty());
        println!("TS_MIRROR spread_long_window_limit limit={}", window_limit);
    }

    #[test]
    fn amm_is_copy_so_a_view_ix_can_quote_without_mutating() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<AMM>();
    }

    /// The 100-unit AMM at peg 50 with no spread.
    fn no_spread_amm() -> AMM {
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
        amm
    }

    #[test]
    fn print_ladder_for_ts_mirror() {
        let amm = no_spread_amm();
        for (label, direction) in [("long", DirectionV0::Long), ("short", DirectionV0::Short)] {
            let levels =
                vamm_quote_levels(&amm, direction, 10 * BASE_PRECISION_U64, 1, &[], None).unwrap();
            let encoded: Vec<String> = levels
                .iter()
                .map(|l| format!("{}:{}", l.price, l.size))
                .collect();
            println!("TS_MIRROR {} {}", label, encoded.join(","));
        }

        // A take past the per-fill reserve throttle. The ladder covers
        // `calculate_amm_available_liquidity` and not the size asked for, so
        // this is the case that catches a mirror which quotes the room to the
        // hard reserve bound instead.
        for (label, direction) in [
            ("long_capped", DirectionV0::Long),
            ("short_capped", DirectionV0::Short),
        ] {
            let levels =
                vamm_quote_levels(&amm, direction, 40 * BASE_PRECISION_U64, 1, &[], None).unwrap();
            let total: u64 = levels.iter().map(|l| l.size).sum();
            let encoded: Vec<String> = levels
                .iter()
                .map(|l| format!("{}:{}", l.price, l.size))
                .collect();
            println!("TS_MIRROR {} total={} {}", label, total, encoded.join(","));
        }

        // Rivals at +1% that the curve reaches inside the take. The shade sits
        // mid-ladder, with honest rungs on both sides.
        for (label, depth) in [
            ("long_rival", BASE_PRECISION_U64 / 10),
            ("long_dust_rival_shallow", BASE_PRECISION_U64 / 1000),
        ] {
            let rival = [PriceLevelV0 {
                price: 50 * PEG_PRECISION as u64 * 101 / 100,
                size: depth,
            }];
            let books = [QuoterBook {
                priority: 10,
                levels: &rival,
                withheld: PriceLevelV0::default(),
            }];
            let levels = vamm_quote_levels(
                &amm,
                DirectionV0::Long,
                10 * BASE_PRECISION_U64,
                1,
                &books,
                None,
            )
            .unwrap();
            let encoded: Vec<String> = levels
                .iter()
                .map(|l| format!("{}:{}", l.price, l.size))
                .collect();
            println!("TS_MIRROR {} {}", label, encoded.join(","));
        }

        // A dust rival near the band edge on a deep curve. The take never
        // reaches the rival price, so the ladder is the honest curve.
        let mut deep = amm;
        deep.base_asset_reserve = 1_000_000 * AMM_RESERVE_PRECISION;
        deep.quote_asset_reserve = 1_000_000 * AMM_RESERVE_PRECISION;
        deep.terminal_quote_asset_reserve = 1_000_000 * AMM_RESERVE_PRECISION;
        deep.sqrt_k = 1_000_000 * AMM_RESERVE_PRECISION;
        deep.min_base_asset_reserve = 500_000 * AMM_RESERVE_PRECISION;
        deep.max_base_asset_reserve = 2_000_000 * AMM_RESERVE_PRECISION;
        deep.seed_no_spread_quote_state();
        let top = 50 * PEG_PRECISION as u64;
        for (label, direction, rival_price) in [
            ("long_dust_rival", DirectionV0::Long, top + top * 49 / 1000),
            (
                "short_dust_rival",
                DirectionV0::Short,
                top - top * 49 / 1000,
            ),
        ] {
            let rival = [PriceLevelV0 {
                price: rival_price,
                size: BASE_PRECISION_U64 / 1000,
            }];
            let books = [QuoterBook {
                priority: 10,
                levels: &rival,
                withheld: PriceLevelV0::default(),
            }];
            let levels =
                vamm_quote_levels(&deep, direction, 100 * BASE_PRECISION_U64, 1, &books, None)
                    .unwrap();
            let encoded: Vec<String> = levels
                .iter()
                .map(|l| format!("{}:{}", l.price, l.size))
                .collect();
            println!("TS_MIRROR {} {}", label, encoded.join(","));
        }
    }

    /// Rival depth that the split fills before a rung: a level inside the top,
    /// a better rung, and a level past the first one out of order.
    #[test]
    fn print_rival_depth_ladders_for_ts_mirror() {
        let amm = no_spread_amm();
        let top = 50 * PEG_PRECISION as u64;
        let book_cases: [(&str, u64, [PriceLevelV0; 2]); 3] = [
            (
                "long_inside_top_wall",
                6 * BASE_PRECISION_U64,
                [
                    PriceLevelV0 {
                        price: top - top / 500,
                        size: 5 * BASE_PRECISION_U64,
                    },
                    PriceLevelV0 {
                        price: top + top / 50,
                        size: 5 * BASE_PRECISION_U64,
                    },
                ],
            ),
            (
                "long_two_rungs",
                2 * BASE_PRECISION_U64,
                [
                    PriceLevelV0 {
                        price: top + top / 200,
                        size: BASE_PRECISION_U64 / 10,
                    },
                    PriceLevelV0 {
                        price: top + top / 50,
                        size: 5 * BASE_PRECISION_U64,
                    },
                ],
            ),
            (
                "long_out_of_order",
                10 * BASE_PRECISION_U64,
                [
                    PriceLevelV0 {
                        price: top + top / 100,
                        size: BASE_PRECISION_U64 / 10,
                    },
                    PriceLevelV0 {
                        price: top + top / 200,
                        size: 10 * BASE_PRECISION_U64,
                    },
                ],
            ),
        ];
        for (label, size, rival) in book_cases {
            let books = [QuoterBook {
                priority: 10,
                levels: &rival,
                withheld: PriceLevelV0::default(),
            }];
            let levels = vamm_quote_levels(&amm, DirectionV0::Long, size, 1, &books, None).unwrap();
            println!("TS_MIRROR {} {}", label, encode(&levels));
        }
    }
}
