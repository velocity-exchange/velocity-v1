//! Router split (S6 waterfall): combine quoter books into per-quoter
//! allocations for a taker of `direction`/`size`. At each price, CLOB depth
//! fills first (price-time priority lives inside the CLOB); the remaining
//! demand at that price splits pro rata across the other quoters' depth.
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
    /// CLOB books take priority at a price level.
    pub is_clob: bool,
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
    is_clob: bool,
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

/// Book `i`'s cached available depth, iff it's a non-clob book quoting
/// exactly `price`.
fn other_available_at(top: Option<(u64, u64)>, is_clob: bool, price: u64) -> Option<u64> {
    if is_clob {
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
            is_clob: book.is_clob,
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

        // CLOB depth at this price fills first. Indexed loops here and
        // below: `take` mutates three parallel structures.
        for i in 0..cursors.len() {
            if remaining == 0 {
                break;
            }
            if !cursors[i].is_clob {
                continue;
            }
            let Some((p, available)) = tops[i] else {
                continue;
            };
            if p != price {
                continue;
            }
            let amount = remaining.min(available);
            take(
                &mut cursors[i],
                &mut tops[i],
                &mut allocations[i],
                price,
                amount,
            )?;
            remaining = remaining.safe_sub(amount)?;
        }

        // Remaining demand at this price splits pro rata across the others.
        let total_other = cursors
            .iter()
            .zip(&tops)
            .filter_map(|(cursor, top)| other_available_at(*top, cursor.is_clob, price))
            .try_fold(0u64, |acc, available| acc.safe_add(available))?;
        if remaining > 0 && total_other > 0 {
            let demand = remaining.min(total_other);
            let mut given: u64 = 0;
            for i in 0..cursors.len() {
                let Some(available) = other_available_at(tops[i], cursors[i].is_clob, price) else {
                    continue;
                };
                let share = (demand as u128)
                    .safe_mul(available as u128)?
                    .safe_div(total_other as u128)?
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
            // first non-clob quoter at this price with spare depth.
            let mut dust = demand.safe_sub(given)?;
            for i in 0..cursors.len() {
                if dust == 0 {
                    break;
                }
                let Some(available) = other_available_at(tops[i], cursors[i].is_clob, price) else {
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

    fn split(
        direction: Direction,
        size: u64,
        books: &[(bool, Vec<PriceLevel>)],
    ) -> Vec<QuoterAllocation> {
        let books: Vec<QuoterBook> = books
            .iter()
            .map(|(is_clob, levels)| QuoterBook {
                is_clob: *is_clob,
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
            &[(true, vec![level(100, 2 * B), level(101, 2 * B)])],
        );
        assert_eq!(out[0].base, 3 * B);
        // 2 @ 100 + 1 @ 101, prices are per base unit at BASE_PRECISION.
        assert_eq!(out[0].quote, 2 * 100 + 101);

        let out = split(Direction::Long, 10 * B, &[(true, vec![level(100, 2 * B)])]);
        assert_eq!(out[0].base, 2 * B); // capped at depth
    }

    #[test]
    fn clob_priority_at_a_shared_price() {
        let out = split(
            Direction::Long,
            3 * B,
            &[
                (false, vec![level(100, 4 * B)]),
                (true, vec![level(100, 2 * B)]),
            ],
        );
        // CLOB's 2 first, custom gets the remaining 1.
        assert_eq!(out[1].base, 2 * B);
        assert_eq!(out[0].base, B);
    }

    #[test]
    fn pro_rata_across_customs_with_dust() {
        let out = split(
            Direction::Long,
            3 * B,
            &[
                (false, vec![level(100, 2 * B)]),
                (false, vec![level(100, 4 * B)]),
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
            &[(false, vec![level(100, 3)]), (false, vec![level(100, 3)])],
        );
        assert_eq!(out[0].base + out[1].base, 5);
    }

    #[test]
    fn walks_prices_best_first_across_books() {
        let out = split(
            Direction::Long,
            3 * B,
            &[
                (true, vec![level(101, 2 * B)]),
                (false, vec![level(100, B), level(102, 5 * B)]),
            ],
        );
        // 1 @ 100 (custom), 2 @ 101 (clob); 102 never reached.
        assert_eq!(out[1].base, B);
        assert_eq!(out[0].base, 2 * B);

        // Short: best is the highest bid.
        let out = split(
            Direction::Short,
            2 * B,
            &[
                (true, vec![level(99, B)]),
                (false, vec![level(100, B), level(98, B)]),
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
                (false, vec![level(100, B), level(90, 100 * B)]),
                // Degenerate levels skipped.
                (false, vec![level(0, 5 * B), level(101, B), level(102, 0)]),
                (true, vec![level(103, 10 * B)]),
            ],
        );
        assert_eq!(out[0].base, B); // only its monotone prefix
        assert_eq!(out[1].base, B); // only its valid level
        assert_eq!(out[2].base, 2 * B); // clob fills the rest
    }

    #[test]
    fn empty_books_and_zero_size() {
        let out = split(Direction::Long, B, &[(true, vec![])]);
        assert_eq!(out[0], QuoterAllocation::default());
        let out = split(Direction::Long, 0, &[(true, vec![level(100, B)])]);
        assert_eq!(out[0], QuoterAllocation::default());
    }
}
