//! Router split: combine quoter books into per-quoter allocations for a
//! taker of `direction`/`size`. At each price, priority
//! tiers fill in ascending order (vAMM, then CLOB — whose internal
//! price-time ordering its own book preserves — then customs), pro rata
//! within a tier; a single-member tier degenerates to filling it outright.
//!
//! Books come from untrusted `quote_v0` responses, so each is sanitized to
//! its longest usable best-first prefix (capped, monotone, non-degenerate) —
//! a quoter returning garbage levels only truncates its own book. Step-size
//! alignment of allocations is not handled here: execute may partially fill
//! and the fill-time validation clamps, so dust misalignment errs safe.

use crate::error::{ErrorCode, VelocityResult};
use crate::math::casting::Cast;
use crate::math::constants::BASE_PRECISION;
use crate::math::safe_math::SafeMath;
use crate::state::prop_amm::{Direction, PriceLevel};
use crate::{msg, validate};

/// Levels processed per book; anything past this is ignored.
pub const MAX_LEVELS_PER_BOOK: usize = 128;

pub struct QuoterBook<'a> {
    /// Routing tier at a shared price: lower fills first, pro rata within.
    pub priority: u8,
    /// Best price first (ascending asks for a long taker, descending bids
    /// for a short taker).
    pub levels: &'a [PriceLevel],
}

/// Router-mode inputs the fill entrypoint threads into the fill controller:
/// the external quoter books it already quoted via CPI, and the execute leg
/// for allocations that land on them. Books and executor share indexing.
pub struct RouterFillInputs<'a, 'b> {
    pub books: &'a [QuoterBook<'b>],
    pub executor: &'a mut dyn crate::state::prop_amm::ExternalQuoterExecutor,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuoterAllocation {
    /// Base size routed to this quoter, to be passed to its execute leg.
    pub base: u64,
    /// Quote notional at the quoted levels — execute's returned balance
    /// changes must be at-or-better than this per unit.
    pub quote: u64,
}

/// One book's sanitized read cursor.
struct Cursor<'a> {
    priority: u8,
    levels: &'a [PriceLevel],
    index: usize,
    /// Consumed within `levels[index]`.
    consumed: u64,
    /// Availability is quantized to this step: allocations must be
    /// `order_step_size` multiples (the market's position counters are
    /// validated against it), so each level's sub-step tail is unfillable
    /// dust the cursor skips past rather than stranding the walk on it.
    step: u64,
}

impl Cursor<'_> {
    /// Advance past degenerate/out-of-order/over-cap/sub-step levels; return
    /// the current (price, step-aligned available) or None when exhausted.
    fn peek(&mut self, direction: Direction) -> Option<(u64, u64)> {
        while self.index < self.levels.len().min(MAX_LEVELS_PER_BOOK) {
            let level = self.levels[self.index];
            let available = level.size.saturating_sub(self.consumed);
            let usable = available - available % self.step;
            let degenerate = level.price == 0 || usable == 0;
            let out_of_order = self.index > 0 && {
                let prev = self.levels[self.index - 1].price;
                match direction {
                    Direction::Long => level.price < prev,
                    Direction::Short => level.price > prev,
                }
            };
            if out_of_order {
                // Truncate the book at its first non-monotone level.
                self.index = self.levels.len();
                return None;
            }
            if degenerate {
                self.index += 1;
                self.consumed = 0;
                continue;
            }
            return Some((level.price, usable));
        }
        None
    }

    fn consume(&mut self, amount: u64) {
        self.consumed += amount;
    }
}

/// Rounded toward the taker-conservative bound: the allocation's quote is
/// what execution is held to (at-or-better), so it must never be tighter
/// than a level's true notional. A long taker is bounded above (pays at
/// most the quote) → ceil; a short taker is bounded below (receives at
/// least the quote) → floor. The tighter rounding would reject honest fills
/// whose own terminal rounding goes against the taker (the AMM's ±1, a
/// maker-favored quote) whenever the quoted price has no slack to absorb
/// it.
fn quote_notional(direction: Direction, price: u64, base: u64) -> VelocityResult<u64> {
    let exact = (price as u128).safe_mul(base as u128)?;
    match direction {
        Direction::Long => exact.safe_div_ceil(BASE_PRECISION)?.cast::<u64>(),
        Direction::Short => exact.safe_div(BASE_PRECISION)?.cast::<u64>(),
    }
}

/// Consume `amount` at `price` from book `i`: advances its cursor, shrinks
/// its cached top, and accrues the allocation.
fn take(
    direction: Direction,
    cursor: &mut Cursor,
    top: &mut Option<(u64, u64)>,
    allocation: &mut QuoterAllocation,
    price: u64,
    amount: u64,
) -> VelocityResult<()> {
    cursor.consume(amount);
    *top = top.and_then(|(p, available)| (available > amount).then(|| (p, available - amount)));
    allocation.base = allocation.base.safe_add(amount)?;
    allocation.quote = allocation
        .quote
        .safe_add(quote_notional(direction, price, amount)?)?;
    Ok(())
}

/// A book's cached available depth, iff it sits in `tier` and quotes
/// exactly `price`.
fn available_at(top: Option<(u64, u64)>, priority: u8, tier: u8, price: u64) -> Option<u64> {
    if priority != tier {
        return None;
    }
    top.and_then(|(p, available)| (p == price).then_some(available))
}

/// Split `taker_size` across the books in `step_size` quanta. Returns one
/// allocation per book (same order); the sum of allocated base is
/// `min(taker_size, total usable depth)` rounded down to the step — every
/// allocation is a step multiple by construction, so execution never drops
/// dust the taker was promised.
pub fn split_across_quoters(
    direction: Direction,
    taker_size: u64,
    books: &[QuoterBook],
    step_size: u64,
) -> VelocityResult<Vec<QuoterAllocation>> {
    validate!(
        !books.is_empty(),
        ErrorCode::DefaultError,
        "router split needs at least one book"
    )?;
    let step = step_size.max(1);
    let mut cursors: Vec<Cursor> = books
        .iter()
        .map(|book| Cursor {
            priority: book.priority,
            levels: book.levels,
            index: 0,
            consumed: 0,
            step,
        })
        .collect();
    let mut allocations = vec![QuoterAllocation::default(); books.len()];
    let mut remaining = taker_size;

    // Round-scratch buffers hoisted out of the loop: Solana's bump allocator
    // never frees, so per-round Vecs would leak O(rounds × books) heap and
    // OOM a many-maker fill inside the 32KB budget.
    let mut tops: Vec<Option<(u64, u64)>> = vec![None; cursors.len()];
    let mut tiers: Vec<u8> = Vec::with_capacity(cursors.len());
    while remaining > 0 {
        // One peek per book per price round; consumption below updates the
        // cached top in place instead of re-walking the levels.
        for (top, cursor) in tops.iter_mut().zip(cursors.iter_mut()) {
            *top = cursor.peek(direction);
        }
        let live = tops.iter().flatten().map(|&(price, _)| price);
        let Some(price) = (match direction {
            Direction::Long => live.min(),
            Direction::Short => live.max(),
        }) else {
            break;
        };

        // Priority tiers quoting this price, ascending; pro rata within a
        // tier (a single-member tier degenerates to filling it outright).
        tiers.clear();
        tiers.extend(cursors.iter().zip(&tops).filter_map(|(cursor, top)| {
            top.and_then(|(p, _)| (p == price).then_some(cursor.priority))
        }));
        tiers.sort_unstable();
        tiers.dedup();

        for &tier in &tiers {
            if remaining == 0 {
                break;
            }
            let total = cursors
                .iter()
                .zip(&tops)
                .filter_map(|(cursor, top)| available_at(*top, cursor.priority, tier, price))
                .try_fold(0u64, |acc, available| acc.safe_add(available))?;
            let demand = {
                let d = remaining.min(total);
                d - d % step
            };
            // Indexed loops: `take` mutates three parallel structures.
            let mut given: u64 = 0;
            for i in 0..cursors.len() {
                let Some(available) = available_at(tops[i], cursors[i].priority, tier, price)
                else {
                    continue;
                };
                let share = (demand as u128)
                    .safe_mul(available as u128)?
                    .safe_div(total as u128)?
                    .cast::<u64>()?;
                let share = share - share % step;
                take(
                    direction,
                    &mut cursors[i],
                    &mut tops[i],
                    &mut allocations[i],
                    price,
                    share,
                )?;
                given = given.safe_add(share)?;
            }
            // Floor-division dust (< books.len() units): hand it to the
            // first quoter in the tier with spare depth.
            let mut dust = demand.safe_sub(given)?;
            for i in 0..cursors.len() {
                if dust == 0 {
                    break;
                }
                let Some(available) = available_at(tops[i], cursors[i].priority, tier, price)
                else {
                    continue;
                };
                let amount = {
                    let a = dust.min(available);
                    a - a % step
                };
                if amount == 0 {
                    continue;
                }
                take(
                    direction,
                    &mut cursors[i],
                    &mut tops[i],
                    &mut allocations[i],
                    price,
                    amount,
                )?;
                dust = dust.safe_sub(amount)?;
            }
            remaining = remaining.safe_sub(demand)?;
        }
    }

    Ok(allocations)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(price: u64, size: u64) -> PriceLevel {
        PriceLevel { price, size }
    }

    const VAMM: u8 = 0;
    const CLOB: u8 = 10;
    const CUSTOM: u8 = 20;

    fn split(
        direction: Direction,
        size: u64,
        books: &[(u8, Vec<PriceLevel>)],
    ) -> Vec<QuoterAllocation> {
        let books: Vec<QuoterBook> = books
            .iter()
            .map(|(priority, levels)| QuoterBook {
                priority: *priority,
                levels,
            })
            .collect();
        split_across_quoters(direction, size, &books, 1).unwrap()
    }

    const B: u64 = 1_000_000_000; // one base unit

    #[test]
    fn single_book_partial_and_full() {
        let out = split(
            Direction::Long,
            3 * B,
            &[(CLOB, vec![level(100, 2 * B), level(101, 2 * B)])],
        );
        assert_eq!(out[0].base, 3 * B);
        // 2 @ 100 + 1 @ 101, prices are per base unit at BASE_PRECISION.
        assert_eq!(out[0].quote, 2 * 100 + 101);

        let out = split(Direction::Long, 10 * B, &[(CLOB, vec![level(100, 2 * B)])]);
        assert_eq!(out[0].base, 2 * B); // capped at depth
    }

    #[test]
    fn tiers_fill_in_priority_order_at_a_shared_price() {
        let out = split(
            Direction::Long,
            3 * B,
            &[
                (CUSTOM, vec![level(100, 4 * B)]),
                (CLOB, vec![level(100, 2 * B)]),
            ],
        );
        // CLOB tier first, custom gets the remaining 1.
        assert_eq!(out[1].base, 2 * B);
        assert_eq!(out[0].base, B);

        // The vAMM tier outranks both.
        let out = split(
            Direction::Long,
            2 * B,
            &[
                (CUSTOM, vec![level(100, 4 * B)]),
                (CLOB, vec![level(100, 2 * B)]),
                (VAMM, vec![level(100, B)]),
            ],
        );
        assert_eq!(out[2].base, B); // vAMM drained first
        assert_eq!(out[1].base, B); // then CLOB
        assert_eq!(out[0].base, 0); // custom sees nothing
    }

    #[test]
    fn pro_rata_within_a_tier_with_dust() {
        let out = split(
            Direction::Long,
            3 * B,
            &[
                (CUSTOM, vec![level(100, 2 * B)]),
                (CUSTOM, vec![level(100, 4 * B)]),
            ],
        );
        // Demand 3 across depth 6 → 1 and 2.
        assert_eq!(out[0].base, B);
        assert_eq!(out[1].base, 2 * B);
        assert_eq!(out[0].base + out[1].base, 3 * B);

        // Indivisible demand: floors + dust to the first with spare depth.
        let out = split(
            Direction::Long,
            5,
            &[(CUSTOM, vec![level(100, 3)]), (CUSTOM, vec![level(100, 3)])],
        );
        assert_eq!(out[0].base + out[1].base, 5);
    }

    #[test]
    fn walks_prices_best_first_across_books() {
        let out = split(
            Direction::Long,
            3 * B,
            &[
                (CLOB, vec![level(101, 2 * B)]),
                (CUSTOM, vec![level(100, B), level(102, 5 * B)]),
            ],
        );
        // 1 @ 100 (custom), 2 @ 101 (clob); 102 never reached. A better
        // price always beats a better tier.
        assert_eq!(out[1].base, B);
        assert_eq!(out[0].base, 2 * B);

        // Short: best is the highest bid.
        let out = split(
            Direction::Short,
            2 * B,
            &[
                (CLOB, vec![level(99, B)]),
                (CUSTOM, vec![level(100, B), level(98, B)]),
            ],
        );
        assert_eq!(out[1].base, B); // 100 first
        assert_eq!(out[0].base, B); // then 99
    }

    #[test]
    fn garbage_books_only_hurt_themselves() {
        let out = split(
            Direction::Long,
            4 * B,
            &[
                // Non-monotone: truncated after the first level.
                (CUSTOM, vec![level(100, B), level(90, 100 * B)]),
                // Degenerate levels skipped.
                (CUSTOM, vec![level(0, 5 * B), level(101, B), level(102, 0)]),
                (CLOB, vec![level(103, 10 * B)]),
            ],
        );
        assert_eq!(out[0].base, B); // only its monotone prefix
        assert_eq!(out[1].base, B); // only its valid level
        assert_eq!(out[2].base, 2 * B); // clob fills the rest
    }

    #[test]
    fn empty_books_and_zero_size() {
        let out = split(Direction::Long, B, &[(CLOB, vec![])]);
        assert_eq!(out[0], QuoterAllocation::default());
        let out = split(Direction::Long, 0, &[(CLOB, vec![level(100, B)])]);
        assert_eq!(out[0], QuoterAllocation::default());
    }
}
