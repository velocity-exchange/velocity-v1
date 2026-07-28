//! Router split (S6 waterfall): combine quoter books into per-quoter
//! allocations for a taker of `direction`/`size`. At each price, priority
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
}

impl Cursor<'_> {
    /// Advance past degenerate/out-of-order/over-cap levels; return the
    /// current (price, available) or None when exhausted.
    fn peek(&mut self, direction: Direction) -> Option<(u64, u64)> {
        while self.index < self.levels.len().min(MAX_LEVELS_PER_BOOK) {
            let level = self.levels[self.index];
            let degenerate = level.price == 0 || level.size <= self.consumed;
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
            return Some((level.price, level.size - self.consumed));
        }
        None
    }

    fn consume(&mut self, amount: u64) {
        self.consumed += amount;
    }
}

fn quote_notional(price: u64, base: u64) -> VelocityResult<u64> {
    (price as u128)
        .safe_mul(base as u128)?
        .safe_div(BASE_PRECISION)?
        .cast::<u64>()
}

/// Consume `amount` at `price` from book `i`: advances its cursor, shrinks
/// its cached top, and accrues the allocation.
fn take(
    cursor: &mut Cursor,
    top: &mut Option<(u64, u64)>,
    allocation: &mut QuoterAllocation,
    price: u64,
    amount: u64,
) -> VelocityResult<()> {
    cursor.consume(amount);
    *top = top.and_then(|(p, available)| (available > amount).then(|| (p, available - amount)));
    allocation.base = allocation.base.safe_add(amount)?;
    allocation.quote = allocation.quote.safe_add(quote_notional(price, amount)?)?;
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

/// Split `taker_size` across the books. Returns one allocation per book (same
/// order); the sum of allocated base is `min(taker_size, total usable depth)`.
pub fn split_across_quoters(
    direction: Direction,
    taker_size: u64,
    books: &[QuoterBook],
) -> VelocityResult<Vec<QuoterAllocation>> {
    validate!(
        !books.is_empty(),
        ErrorCode::DefaultError,
        "router split needs at least one book"
    )?;
    let mut cursors: Vec<Cursor> = books
        .iter()
        .map(|book| Cursor {
            priority: book.priority,
            levels: book.levels,
            index: 0,
            consumed: 0,
        })
        .collect();
    let mut allocations = vec![QuoterAllocation::default(); books.len()];
    let mut remaining = taker_size;

    while remaining > 0 {
        // One peek per book per price round; consumption below updates the
        // cached top in place instead of re-walking the levels.
        let mut tops: Vec<Option<(u64, u64)>> =
            cursors.iter_mut().map(|c| c.peek(direction)).collect();
        let live = tops.iter().flatten().map(|&(price, _)| price);
        let Some(price) = (match direction {
            Direction::Long => live.min(),
            Direction::Short => live.max(),
        }) else {
            break;
        };

        // Priority tiers quoting this price, ascending; pro rata within a
        // tier (a single-member tier degenerates to filling it outright).
        let mut tiers: Vec<u8> = cursors
            .iter()
            .zip(&tops)
            .filter_map(|(cursor, top)| {
                top.and_then(|(p, _)| (p == price).then_some(cursor.priority))
            })
            .collect();
        tiers.sort_unstable();
        tiers.dedup();

        for tier in tiers {
            if remaining == 0 {
                break;
            }
            let total = cursors
                .iter()
                .zip(&tops)
                .filter_map(|(cursor, top)| available_at(*top, cursor.priority, tier, price))
                .try_fold(0u64, |acc, available| acc.safe_add(available))?;
            let demand = remaining.min(total);
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
                take(
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
                let amount = dust.min(available);
                take(
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
        split_across_quoters(direction, size, &books).unwrap()
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
