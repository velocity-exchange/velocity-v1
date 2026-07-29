//! Fill engine.
//!
//! One fill path: [`router_take`]. Internal [`RouterQuoter`] books (the vAMM
//! ladder, DLOB-order bridges) plus externally-quoted CPI books are split by
//! priority tier; internal allocations settle in place, external ones are
//! returned for the CPI execute leg.
//!
//! There is no general continuous-curve clearing algorithm here — no bisection,
//! no price-domain search. Every source, the vAMM included, publishes discrete
//! price levels and the split walks them best-first, distributing the clearing
//! tier priority-first then pro-rata in whole step quanta. The vAMM's ladder is
//! built in `vlp::amm::router_adapter::vamm_quote_levels`, so the curve is
//! reduced to levels before it ever reaches the split.
//!
//! Per-quoter fills are validated by [`fill_at_or_better`]: a quoter must
//! deliver each unit at or better than the price its own book advertised.
//!
//! The two legacy engines this module used to hold (a continuous sole-vAMM path
//! and a discrete maker walk, each driven through a `Quoter` trait) are gone —
//! the router subsumes both, and the AMM-JIT-inside-a-match mode they existed
//! to serve no longer exists.

use crate::{
    controller::position::PositionDirection,
    error::{ErrorCode, VelocityResult},
    math::{
        router::{split_across_quoters, QuoterAllocation, QuoterBook},
        safe_math::SafeMath,
    },
    msg,
    state::{
        prop_amm::{Direction, PriceLevel},
        quoter::{QuoteContext, QuoterFill, RouterQuoter},
    },
    validate,
};

/// Result of [`router_take`].
#[derive(Debug)]
pub struct RouterTakeOutcome {
    /// One entry per internal quoter, same order as passed; `None` when the
    /// split allocated it nothing.
    pub internal_fills: Vec<Option<QuoterFill>>,
    /// One entry per external book, same order as passed: the allocation the
    /// caller must execute through the registry's CPI leg.
    pub external_allocations: Vec<QuoterAllocation>,
}

/// A fill is at-or-better than its allocation when its floored per-unit
/// price does not lose to the allocation's ceiled per-unit price (reversed
/// for a short taker), with one quote lamport of absolute notional slack.
/// The unit-price comparison (rather than a raw notional cross-multiply)
/// absorbs sub-unit rounding on partial fills; the single-lamport slack
/// absorbs a quoter's terminal rounding against the taker on an otherwise
/// exact notional — the AMM's +1 on Remove, a short maker's ceil — which
/// per-unit math can't hide when the fill is an exact base-precision
/// multiple. The allocation's quote is itself ceiled per level
/// (`math::router::quote_notional`), so one lamport is the whole honest
/// gap, not a tunable tolerance.
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

/// The router fill engine — the third path. Builds each internal quoter's
/// book in descending-priority order, so a quoter sees every book built
/// before it (the external CPI books plus all worse-tier internals) as
/// rivals — that ordering is what gives the top-tier vAMM last look. The
/// taker size then splits across the union of books by priority tier
/// (`math::router::split_across_quoters`), and each internal allocation is
/// settled through `try_fill_solo`/`commit_fill`, validated at-or-better
/// against its quoted allocation. External allocations are returned for the
/// caller to execute through the registry's CPI leg (and hold to the same
/// at-or-better bar).
///
/// Like [`router_take`], this touches only quoter-side state; taker position
/// updates and fee accounting stay with the surrounding fill controller.
pub fn router_take(
    quoters: &mut [&mut dyn RouterQuoter],
    ctx: &QuoteContext,
    side: PositionDirection,
    target_size: u64,
    external_books: &[QuoterBook],
    taker_limit_price: Option<u64>,
) -> VelocityResult<RouterTakeOutcome> {
    if quoters.is_empty() && external_books.is_empty() {
        return Ok(RouterTakeOutcome {
            internal_fills: vec![],
            external_allocations: vec![],
        });
    }
    let direction = match side {
        PositionDirection::Long => Direction::Long,
        PositionDirection::Short => Direction::Short,
    };
    // Books are truncated at the taker's limit before the split, so no
    // allocation can clear past it (the counterpart of the legacy paths'
    // `cumulative_size` cap / `taker_price_for_match`). Books are
    // best-first, so cutting at the first out-of-limit level is exact.
    let within_limit = |levels: &[PriceLevel]| -> usize {
        let Some(limit) = taker_limit_price else {
            return levels.len();
        };
        levels
            .iter()
            .position(|level| match side {
                PositionDirection::Long => level.price > limit,
                PositionDirection::Short => level.price < limit,
            })
            .unwrap_or(levels.len())
    };

    // Build internal books worst tier first, handing each quoter everything
    // already quoted as rivals.
    let mut build_order: Vec<usize> = (0..quoters.len()).collect();
    build_order.sort_by_key(|&i| core::cmp::Reverse(quoters[i].priority()));
    let mut internal_levels: Vec<Vec<PriceLevel>> = vec![Vec::new(); quoters.len()];
    for &i in &build_order {
        let levels = {
            let rivals: Vec<QuoterBook> = external_books
                .iter()
                .map(|book| QuoterBook {
                    priority: book.priority,
                    levels: &book.levels[..within_limit(book.levels)],
                })
                .chain(
                    quoters
                        .iter()
                        .zip(&internal_levels)
                        .filter(|(_, levels)| !levels.is_empty())
                        .map(|(quoter, levels)| QuoterBook {
                            priority: quoter.priority(),
                            levels,
                        }),
                )
                .collect();
            let mut levels = quoters[i].quote(ctx, direction, target_size, &rivals)?;
            levels.truncate(within_limit(&levels));
            levels
        };
        internal_levels[i] = levels;
    }

    // Split across the union: external books first, then internals — all
    // truncated at the taker's limit.
    let books: Vec<QuoterBook> = external_books
        .iter()
        .map(|book| QuoterBook {
            priority: book.priority,
            levels: &book.levels[..within_limit(book.levels)],
        })
        .chain(
            quoters
                .iter()
                .zip(&internal_levels)
                .map(|(quoter, levels)| QuoterBook {
                    priority: quoter.priority(),
                    levels,
                }),
        )
        .collect();
    let allocations = split_across_quoters(direction, target_size, &books, ctx.step_size)?;
    let (external_allocations, internal_allocations) = allocations.split_at(external_books.len());

    // Execute each internal allocation against its quoter — the in-program
    // execute_v0, held to the same at-or-better bar as the CPI leg.
    let mut internal_fills = Vec::with_capacity(quoters.len());
    for (i, allocation) in internal_allocations.iter().enumerate() {
        if allocation.base == 0 {
            internal_fills.push(None);
            continue;
        }
        let fill = quoters[i].execute(ctx, direction, allocation.base)?;
        validate!(
            fill.base_filled <= allocation.base,
            ErrorCode::DefaultError,
            "router quoter {} overfilled: {} > {}",
            i,
            fill.base_filled,
            allocation.base
        )?;
        validate!(
            fill_at_or_better(side, &fill, allocation, ctx.base_precision)?,
            ErrorCode::DefaultError,
            "router quoter {} filled worse than quoted",
            i
        )?;
        internal_fills.push((fill.base_filled > 0).then_some(fill));
    }

    Ok(RouterTakeOutcome {
        internal_fills,
        external_allocations: external_allocations.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            state::{
                oracle::OraclePriceData,
                perp_market::MarketStats,
                quoter::{FillFeePolicy, QuoteContext},
            },
            vlp::amm::AmmQuoter,
        },
    };

    fn make_ctx<'a>(
        stats: &'a MarketStats,
        oracle: &'a OraclePriceData,
        tick: u64,
    ) -> QuoteContext<'a> {
        // Use base_precision = 1 for StepMaker tests (unit-less values) and
        // BASE_PRECISION for tests that involve real perp markets via
        // AmmQuoter; individual tests override as needed.
        QuoteContext {
            stats,
            oracle,
            mm_oracle: None,
            oracle_validity: None,
            fee_budget: 0,
            tick,
            step_size: 1,
            slot: 0,
            base_precision: 1,
            market_status: crate::state::market_status::MarketStatus::default(),
            market_config: 0,
        }
    }

    #[test]
    fn router_take_splits_across_amm_dlob_and_external_books() {
        use crate::{
            math::constants::{AMM_RESERVE_PRECISION, BASE_PRECISION_U64, PEG_PRECISION},
            state::quoter::DlobOrderQuoter,
            vlp::amm::AMM,
        };

        let stats = MarketStats::default();
        let oracle = OraclePriceData::default();
        let mut ctx = make_ctx(&stats, &oracle, 1);
        ctx.base_precision = BASE_PRECISION_U64;

        // AMM top at 100 (peg 100, no spread).
        let mut amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,
            min_base_asset_reserve: 50 * AMM_RESERVE_PRECISION,
            max_base_asset_reserve: 200 * AMM_RESERVE_PRECISION,
            max_fill_reserve_fraction: 4,
            ..AMM::default()
        };
        // DLOB ask better than the AMM top; external book between them.
        // Both cross the AMM's top, so its last look ignores them.
        let mut dlob = router_test_ask(99 * PEG_PRECISION as u64, 2 * BASE_PRECISION_U64);
        let external_levels = [PriceLevel {
            price: 99 * PEG_PRECISION as u64 + PEG_PRECISION as u64 / 2, // 99.5
            size: BASE_PRECISION_U64,
        }];
        let external_books = [QuoterBook {
            priority: 10,
            levels: &external_levels,
        }];

        let outcome = {
            let mut amm_quoter = AmmQuoter::new_no_spread(&mut amm);
            let mut dlob_quoter = DlobOrderQuoter::new(&mut dlob, u64::MAX);
            let mut quoters: Vec<&mut dyn RouterQuoter> = vec![&mut amm_quoter, &mut dlob_quoter];
            router_take(
                &mut quoters,
                &ctx,
                PositionDirection::Long,
                4 * BASE_PRECISION_U64,
                &external_books,
                None,
            )
            .unwrap()
        };

        // Best-first: DLOB's 2 @ 99, external's 1 @ 99.5, AMM ladder for the
        // last 1.
        let dlob_fill = outcome.internal_fills[1].unwrap();
        assert_eq!(dlob_fill.base_filled, 2 * BASE_PRECISION_U64);
        assert_eq!(outcome.external_allocations[0].base, BASE_PRECISION_U64);
        let amm_fill = outcome.internal_fills[0].unwrap();
        assert_eq!(amm_fill.base_filled, BASE_PRECISION_U64);

        // Commits landed on both internal quoters.
        assert_eq!(dlob.base_asset_amount_filled, 2 * BASE_PRECISION_U64);
        assert!(amm.base_asset_amount_with_amm > 0);
    }

    #[test]
    fn router_take_vamm_last_look_shades_over_a_worse_dlob_ask() {
        use crate::{
            math::constants::{AMM_RESERVE_PRECISION, BASE_PRECISION_U64, PEG_PRECISION},
            state::quoter::DlobOrderQuoter,
            vlp::amm::AMM,
        };

        let stats = MarketStats::default();
        let oracle = OraclePriceData::default();
        let mut ctx = make_ctx(&stats, &oracle, 1);
        ctx.base_precision = BASE_PRECISION_U64;

        let mut amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,
            min_base_asset_reserve: 50 * AMM_RESERVE_PRECISION,
            max_base_asset_reserve: 200 * AMM_RESERVE_PRECISION,
            max_fill_reserve_fraction: 4,
            ..AMM::default()
        };
        // DLOB ask 0.5% above the AMM top — inside the last-look band, so
        // the vAMM shades the slice of curve cheaper than it to exactly this
        // price and wins the tie by tier.
        let rival_price = 100 * PEG_PRECISION as u64 + PEG_PRECISION as u64 / 2; // 100.5
        let mut dlob = router_test_ask(rival_price, 5 * BASE_PRECISION_U64);

        let take = 3 * BASE_PRECISION_U64;
        let outcome = {
            let mut amm_quoter = AmmQuoter::new_no_spread(&mut amm);
            let mut dlob_quoter = DlobOrderQuoter::new(&mut dlob, u64::MAX);
            let mut quoters: Vec<&mut dyn RouterQuoter> = vec![&mut amm_quoter, &mut dlob_quoter];
            router_take(&mut quoters, &ctx, PositionDirection::Long, take, &[], None).unwrap()
        };

        // The vAMM's shaded rung fills first at the rival's price; the DLOB
        // order gets only what's left at that level.
        let amm_fill = outcome.internal_fills[0].unwrap();
        let dlob_fill = outcome.internal_fills[1].unwrap();
        assert!(amm_fill.base_filled > 0);
        assert_eq!(amm_fill.base_filled + dlob_fill.base_filled, take);
        assert!(dlob_fill.base_filled < take);
        // The AMM's actual per-unit cost stays at or under the shaded price
        // — the gap is LP surplus.
        let per_unit = (amm_fill.quote_filled as u128) * BASE_PRECISION_U64 as u128
            / amm_fill.base_filled as u128;
        assert!(per_unit <= rival_price as u128);
    }

    #[test]
    fn router_take_truncates_books_at_the_taker_limit() {
        use crate::{
            math::constants::{AMM_RESERVE_PRECISION, BASE_PRECISION_U64, PEG_PRECISION},
            vlp::amm::AMM,
        };

        let stats = MarketStats::default();
        let oracle = OraclePriceData::default();
        let mut ctx = make_ctx(&stats, &oracle, 1);
        ctx.base_precision = BASE_PRECISION_U64;

        let mut amm = AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,
            min_base_asset_reserve: 50 * AMM_RESERVE_PRECISION,
            max_base_asset_reserve: 200 * AMM_RESERVE_PRECISION,
            max_fill_reserve_fraction: 4,
            ..AMM::default()
        };
        // Limit below the ladder's tail: only the cheapest rungs are
        // fillable, so a large take comes up short instead of clearing
        // through the limit. (Rungs are marginal-end prices of
        // request-sized chunks — a 20-unit take on 100-unit reserves puts
        // the first rung near 105.2 and the tail near 110.)
        let limit = 106 * PEG_PRECISION as u64;
        let take = 20 * BASE_PRECISION_U64;
        let outcome = {
            let mut amm_quoter = AmmQuoter::new_no_spread(&mut amm);
            let mut quoters: Vec<&mut dyn RouterQuoter> = vec![&mut amm_quoter];
            router_take(
                &mut quoters,
                &ctx,
                PositionDirection::Long,
                take,
                &[],
                Some(limit),
            )
            .unwrap()
        };
        let fill = outcome.internal_fills[0].unwrap();
        assert!(fill.base_filled > 0);
        assert!(fill.base_filled < take);
        // Per-unit cost never exceeds the limit.
        let per_unit =
            (fill.quote_filled as u128) * BASE_PRECISION_U64 as u128 / fill.base_filled as u128;
        assert!(per_unit <= limit as u128);
    }

    #[test]
    fn router_take_with_nothing_is_empty() {
        let stats = MarketStats::default();
        let oracle = OraclePriceData::default();
        let ctx = make_ctx(&stats, &oracle, 1);
        let mut quoters: Vec<&mut dyn RouterQuoter> = vec![];
        let outcome =
            router_take(&mut quoters, &ctx, PositionDirection::Long, 100, &[], None).unwrap();
        assert!(outcome.internal_fills.is_empty());
        assert!(outcome.external_allocations.is_empty());
    }

    fn router_test_ask(price: u64, size: u64) -> crate::state::user::Order {
        use crate::state::user::{
            MarketType, Order, OrderStatus, OrderTriggerCondition, OrderType,
        };
        Order {
            slot: 0,
            price,
            base_asset_amount: size,
            base_asset_amount_filled: 0,
            quote_asset_amount_filled: 0,
            trigger_price: 0,
            auction_start_price: 0,
            auction_end_price: 0,
            max_ts: 0,
            oracle_price_offset: 0,
            order_id: 0,
            market_index: 0,
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            market_type: MarketType::Perp,
            user_order_id: 0,
            existing_position_direction: PositionDirection::Long,
            direction: PositionDirection::Short,
            reduce_only: false,
            post_only: true,
            immediate_or_cancel: false,
            trigger_condition: OrderTriggerCondition::Above,
            auction_duration: 0,
            posted_slot_tail: 0,
            bit_flags: 0,
            padding: [0; 5],
        }
    }
}
