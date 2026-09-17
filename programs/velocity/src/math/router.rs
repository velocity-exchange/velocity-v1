//! Router split: combine quoter books into per-quoter allocations for a taker
//! of `direction` and `size`. At each price the priority tiers fill in
//! ascending order. The vAMM fills first, then the CLOB, then the customs. A
//! CLOB book keeps its own price-time order inside its levels. Books that
//! share a tier fill pro rata.
//!
//! Books come from untrusted `quote_v0` responses. [`validate_quoted_levels`]
//! holds them to the level contract when velocity reads them. The walk here
//! still tolerates junk, by truncating a book rather than trusting it.
//!
//! The other half of this module runs in the reverse direction.
//! [`quoted_prefix`] and [`validate_executed_notional`] hold what a quoter's
//! `execute_v0` returned to the levels it quoted earlier in the same
//! transaction.

use crate::{
    error::{ErrorCode, VelocityResult},
    math::{casting::Cast, constants::BASE_PRECISION, safe_math::SafeMath},
    msg,
    state::prop_amm::{Direction, PriceLevel},
    validate,
};

/// Levels the walk reads per book. It ignores the rest.
pub const MAX_LEVELS_PER_BOOK: usize = 128;

#[derive(Clone, Copy, Default)]
pub struct QuoterBook<'a> {
    /// Routing tier at a shared price. A lower tier fills first. Books that
    /// share a tier fill pro rata.
    pub priority: u8,
    /// Best price first. Asks ascend for a long taker. Bids descend for a
    /// short taker.
    pub levels: &'a [PriceLevel],
    /// Depth this book says it holds at a better price than it quoted, and
    /// could not offer because the accounts of the user who owns it are not
    /// in this transaction. A zero price means none.
    ///
    /// Nobody can fill it, so it never appears in `levels` and takes no part
    /// of the split. It marks the fill as one that withheld depth, which is
    /// what [`withheld_obligation`] answers for.
    pub withheld: PriceLevel,
}

/// Writable and signer locks a full transaction holds. A filler cannot fit one
/// more maker at this count. The count covers contended locks only. Read-only
/// locks are shared and free to pad, so they cannot prove a transaction was
/// full. A fill that holds fewer writable locks had room for the maker it
/// omitted. The value sits below the writable-lock reach of a deep fill, and
/// above what a shallow padded fill holds.
pub const TX_WRITABLE_LOCK_BUDGET: usize = 40;
/// Writable locks one more CLOB maker costs: its `User` and its `UserStats`.
pub const MAKER_ACCOUNT_COST: usize = 2;
/// Writable and signer locks every perp fill holds, whatever it routes to:
/// the signer, the taker's `User` and `UserStats`, and the perp market.
pub const FILL_FIXED_WRITABLE_LOCKS: usize = 4;

/// What the fill knows about the party that built the transaction.
///
/// A book stops its walk at an order whose owner the transaction does not
/// carry. It reports the depth behind that order as withheld. Who answers for
/// the withheld depth depends on who chose the account list.
///
/// A taker that signed the transaction chose that list. A taker that did not
/// sign trusts a filler. The filler then owes the taker every maker it had
/// room for. [`withheld_obligation`] states the rule.
#[derive(Clone, Copy, Debug, Default)]
pub struct FillerObligation {
    /// The taker's own authority or delegate signs this transaction.
    pub taker_signed: bool,
    /// Distinct accounts the transaction locks. `None` when the caller passed
    /// no instructions sysvar, so the fill cannot count them.
    pub tx_accounts: Option<usize>,
    /// Quoter entries the transaction carried that the order's signed route
    /// did not name.
    ///
    /// Zero when the order signed no route. The taker then named no entries,
    /// so no carried entry is uninvited. A taker that wants this test signs a
    /// route.
    pub unrouted_quoters: usize,
}

/// Whether a filler that left a book short of an owner met its obligation.
///
/// There are four outcomes and only the last one fills.
///
/// - The transaction had room for another maker. The filler owed that maker.
/// - The transaction is full, but it carries a loaded user that filled nothing
///   and holds no role in the fill. Those accounts crowded out the maker the
///   book wanted. A filler can force a withhold this way and then take the
///   fill on a worse-priced source of its own.
/// - The transaction is full and every loaded user did something, but it
///   carries a quoter entry the signed route never named. Each entry costs
///   locks that could have carried the maker. The taker's route is the only
///   statement of which entries it wanted.
/// - The transaction is full and every loaded user did something. The filler
///   could not carry the maker, so the walk stops and the fill is short.
///
/// `attributable_locks` is the part of the transaction velocity can point at
/// work of its own. A writable meta may name any pubkey, including one that
/// holds no account. The transaction's own count therefore states what a
/// filler claims rather than what it spent. The room test runs on the smaller
/// of the two figures, which a filler cannot raise by naming more keys.
pub fn withheld_obligation(
    obligation: &FillerObligation,
    idle_loaded_users: usize,
    attributable_locks: usize,
) -> VelocityResult<()> {
    if obligation.taker_signed {
        return Ok(());
    }
    let Some(accounts) = obligation.tx_accounts else {
        msg!("a fill that withholds depth must pass the instructions sysvar");
        return Err(ErrorCode::FillerObligationUncountable);
    };
    let room_spent = accounts.min(attributable_locks);
    validate!(
        room_spent > TX_WRITABLE_LOCK_BUDGET.saturating_sub(MAKER_ACCOUNT_COST),
        ErrorCode::FillerOmittedReachableMaker,
        "transaction spends {} of {} writable locks on this fill, so it had room for a maker the book wanted",
        room_spent,
        TX_WRITABLE_LOCK_BUDGET
    )?;
    validate!(
        idle_loaded_users == 0,
        ErrorCode::FillerPaddedTheUserSet,
        "{} loaded users filled nothing while a book withheld depth",
        idle_loaded_users
    )?;
    // A full transaction whose every user worked can still be the wrong
    // transaction. A quoter entry costs locks. Carrying one the taker did not
    // ask for spends the room a maker needed, and the fill then prices against
    // that entry instead of the book. Extra entries stay free while nothing is
    // withheld, because they can only lose at their own quoted prices.
    validate!(
        obligation.unrouted_quoters == 0,
        ErrorCode::FillerCarriedUnroutedQuoter,
        "{} carried quoter entries are outside the signed route while a book withheld depth",
        obligation.unrouted_quoters
    )?;
    Ok(())
}

/// The router leg of a fill, as the entrypoint hands it to the fill
/// controller. It holds the external quoter books the entrypoint quoted
/// through CPI, and the execute leg for the allocations that land on them.
/// Books and executor share indexing.
///
/// `'info` is the account lifetime the executor reads its responses out of. It
/// is distinct from the books' `'b`, which borrows from the quoting section.
pub struct RouterLeg<'a, 'b, 'info> {
    pub books: &'a [QuoterBook<'b>],
    pub executor: &'a mut dyn crate::state::prop_amm::ExternalQuoterExecutor<'info>,
    /// What the caller answers for on this fill: the protocol authority no
    /// quoter may name, what the filler owes, and whether the caller closes
    /// the taker's exposure itself. Held whole, so a reader of any of the
    /// three can see which caller stated it.
    pub standing: crate::instructions::FillerStanding,
    /// The worst price any source of the fill executed at, written back by the
    /// pass. `None` when the fill moved no base.
    ///
    /// The fill's own return value is the base and the blended quote. The
    /// blend does not show the worst unit. A caller cannot tell a route that
    /// filled every unit inside a bound from one that averaged past it. A
    /// caller that must know whether every unit cleared a price reads this
    /// instead. An ordinary fill ignores it.
    ///
    /// The unit of measure is one settled allocation, because that is where
    /// value moves. An allocation is a maker order, one external balance
    /// change, or the vAMM slice. The vAMM slice reports its slice average,
    /// which is the only price a curve fill has.
    pub worst_fill_price: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuoterAllocation {
    /// Base size routed to this quoter, to be passed to its execute leg.
    pub base: u64,
    /// Quote notional at the quoted levels, rounded per level toward the bound
    /// that is safe for the taker. The router prices the taker's own fill at
    /// this number.
    pub quote: u64,
    /// The sum of `price * base` over the levels this allocation was cut from,
    /// before the single division into quote units. It is the exact number the
    /// quoter owes for filling the allocation.
    ///
    /// The split accrues it while it walks the ladder, so nothing downstream
    /// reads the levels again. The execute leg is held to this scalar rather
    /// than to a slice that lives only until the quoter's next CPI overwrites
    /// the buffer it came from.
    pub scaled_quote: u128,
}

/// One book's sanitized read cursor.
struct Cursor<'a> {
    priority: u8,
    levels: &'a [PriceLevel],
    index: usize,
    /// Consumed within `levels[index]`.
    consumed: u64,
    /// Available size is quantized to this step. An allocation must be a
    /// multiple of `order_step_size`, because the market validates its position
    /// counters against that size. A level's sub-step tail is dust nobody can
    /// fill, so the cursor skips the level instead of stopping the walk on it.
    step: u64,
}

impl Cursor<'_> {
    /// Return the current price and its step-aligned available size. Skip a
    /// level with a zero price or with nothing left above the step. Stop at the
    /// first level that breaks the price order, and at `MAX_LEVELS_PER_BOOK`.
    /// Return `None` once the book is exhausted.
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

/// Round toward the bound that is safe for the taker. The allocation's quote
/// is the at-or-better bar execution is held to, so it must never be tighter
/// than a level's true notional. A long taker pays at most the quote, so the
/// division rounds up. A short taker receives at least the quote, so the
/// division rounds down. The tighter rounding would reject honest fills whose
/// own terminal rounding goes against the taker, such as the AMM's one unit or
/// a maker-favored quote, whenever the quoted price has no slack for it.
fn quote_notional(direction: Direction, price: u64, base: u64) -> VelocityResult<u64> {
    let exact = (price as u128).safe_mul(base as u128)?;
    match direction {
        Direction::Long => exact.safe_div_ceil(BASE_PRECISION)?.cast::<u64>(),
        Direction::Short => exact.safe_div(BASE_PRECISION)?.cast::<u64>(),
    }
}

/// Consume `amount` at `price` from one book. Advance its cursor, shrink its
/// cached top, and accrue the allocation.
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
    allocation.scaled_quote = allocation
        .scaled_quote
        .safe_add((price as u128).safe_mul(amount as u128)?)?;
    Ok(())
}

/// A book's cached available depth, when it sits in `tier` and quotes exactly
/// `price`. `None` otherwise.
fn available_at(top: Option<(u64, u64)>, priority: u8, tier: u8, price: u64) -> Option<u64> {
    if priority != tier {
        return None;
    }
    top.and_then(|(p, available)| (p == price).then_some(available))
}

/// Split `taker_size` across the books in `step_size` quanta. Return one
/// allocation per book, in the order the books came in. The allocated base
/// sums to `min(taker_size, total usable depth)` rounded down to the step.
/// Every allocation is a step multiple by construction, so execution never
/// drops dust the taker was promised.
///
/// Depth a book withheld takes no part of the size. Withheld depth is
/// liquidity whose owner this transaction does not carry. The taker asked to
/// trade, so the size goes to the sources that can fill it. Whether the caller
/// should have carried that owner is a separate question, and
/// [`withheld_obligation`] answers it.
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
    let mut allocations = vec![QuoterAllocation::default(); cursors.len()];
    let mut remaining = taker_size;

    // The round scratch buffers sit outside the loop. Solana's bump allocator
    // never frees, so one Vec per round would hold rounds times books of heap
    // and exhaust the 32KB budget on a many-maker fill.
    let mut tops: Vec<Option<(u64, u64)>> = vec![None; cursors.len()];
    let mut tiers: Vec<u8> = Vec::with_capacity(cursors.len());
    while remaining > 0 {
        // One peek per book per price round. The code below updates the cached
        // top in place instead of walking the levels again.
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

        // The priority tiers that quote this price, in ascending order.
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
            if demand == 0 {
                // Nothing step-sized is left to give at this tier. Every
                // allocation below is floored to the step, so a round that
                // demands zero consumes nothing, advances no cursor and leaves
                // `remaining` unchanged. The next pass of the outer loop would
                // be identical, and the walk would spin until the compute
                // budget ran out.
                //
                // A step-aligned `taker_size` makes a sub-step remainder
                // impossible, but this function cannot rely on that alignment.
                // The book carries its own `order_step_size` apart from the
                // market's, and a reduce-only order is sized from a position
                // magnitude that was never standardized. The walk ends here
                // instead. The TypeScript mirror ends it the same way, so both
                // report the same allocations.
                remaining = 0;
                break;
            }
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
            // Floor division and step alignment leave a remainder below one
            // step per book. Give it to the first quoter in the tier that has
            // spare depth.
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

/// Reject a foreign `quote_v0` response outright rather than route it.
///
/// The split walk tolerates junk by truncating, but more than the split reads
/// the raw levels. The taker-limit cut, the Custom margin clamp's depth sum
/// and the vAMM's last look all see them. There a level nobody can fill still
/// moves the outcome. A zero-priced level reads as the best price in
/// existence, and it shades the vAMM's whole ladder away. So velocity holds
/// the levels to the contract the type documents at ingestion instead.
///
/// - Every price and every size is nonzero. A level nobody can fill is not a
///   quote.
/// - Prices run best-first for the taker's direction, non-strictly. Equal
///   consecutive prices are legal and normal, because a ladder's rungs come
///   from distinct offsets that can round to the same tick.
///
/// A quoter that trips this fails the fill it was quoted for. That is not new
/// exposure, because an approved quoter can fail the CPI itself. It also keeps
/// the misbehavior loud enough for the admin to pull the entry.
pub fn validate_quoted_levels(direction: Direction, levels: &[PriceLevel]) -> VelocityResult<()> {
    let mut previous: Option<u64> = None;
    for level in levels {
        validate!(
            level.price != 0 && level.size != 0,
            ErrorCode::InvalidQuoterResponse,
            "quoter level has zero price or size: {}/{}",
            level.price,
            level.size
        )?;
        if let Some(previous) = previous {
            let ordered = match direction {
                Direction::Long => level.price >= previous,
                Direction::Short => level.price <= previous,
            };
            validate!(
                ordered,
                ErrorCode::InvalidQuoterResponse,
                "quoter levels are not best-price-first: {} after {}",
                level.price,
                previous
            )?;
        }
        previous = Some(level.price);
    }
    Ok(())
}

/// The prices a quoter committed to for the best-priced `base` units of the
/// book it quoted. An execute of that size is held to them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuotedPrefix {
    /// The sum of `price * base` over the prefix, before the division by
    /// `BASE_PRECISION`. It is the quoted notional as an exact integer, so the
    /// comparison rounds once, at the end.
    pub scaled_quote: u128,
    /// The prefix's first price, which is the best one for the taker.
    pub best_price: u64,
    /// The prefix's last price. It is the worst price any unit was quoted at.
    pub worst_price: u64,
}

/// Walk `levels` best-first for `base` units and price them at the quoted
/// levels. The walk mirrors the step quantization in [`Cursor::peek`], so the
/// prefix is the one [`split_across_quoters`] allocated from.
pub fn quoted_prefix(
    levels: &[PriceLevel],
    step_size: u64,
    base: u64,
) -> VelocityResult<QuotedPrefix> {
    let step = step_size.max(1);
    let mut remaining = base;
    let mut prefix = QuotedPrefix {
        scaled_quote: 0,
        best_price: 0,
        worst_price: 0,
    };
    for level in levels.iter().take(MAX_LEVELS_PER_BOOK) {
        if remaining == 0 {
            break;
        }
        let usable = level.size - level.size % step;
        if usable == 0 {
            continue;
        }
        let take = remaining.min(usable);
        prefix.scaled_quote = prefix
            .scaled_quote
            .safe_add((level.price as u128).safe_mul(take as u128)?)?;
        if prefix.best_price == 0 {
            prefix.best_price = level.price;
        }
        prefix.worst_price = level.price;
        remaining -= take;
    }
    validate!(
        remaining == 0,
        ErrorCode::QuoterOverfilled,
        "quoter filled {} base but only quoted {}",
        base,
        base - remaining
    )?;
    Ok(prefix)
}

/// Whether `quote` lies in `[lo, hi]` once both bounds are divided by
/// `BASE_PRECISION`. The bounds admit the one rounding step that the division
/// cannot avoid, plus `slack` further quote units on each side.
fn notional_within(lo: u128, hi: u128, quote: u64, slack: u64) -> VelocityResult<bool> {
    let floor = lo.safe_div(BASE_PRECISION)?.saturating_sub(slack as u128);
    let ceil = hi
        .safe_add(BASE_PRECISION.safe_sub(1)?)?
        .safe_div(BASE_PRECISION)?
        .safe_add(slack as u128)?;
    let quote = quote as u128;
    Ok(quote >= floor && quote <= ceil)
}

/// Hold an external quoter's executed quote to the notional the split accrued
/// off its own ladder.
///
/// The check is exact and not a band. Quoting and executing happen in one
/// transaction, and a quoter's book cannot change between them, so
/// `scaled_quote` is the notional of that allocation as an integer. The single
/// division from `price * base` into quote units is the only rounding.
///
/// A quoter whose encoder divides per fill must carry the remainder forward
/// across the fills rather than truncate each one. Per-fill truncation lands
/// below this number by up to one unit per fill, and that dust comes out of
/// its makers.
pub fn validate_allocated_notional(
    allocation: &QuoterAllocation,
    quote: u64,
) -> VelocityResult<bool> {
    Ok(quote as u128 == allocation.scaled_quote.safe_div(BASE_PRECISION)?)
}

pub fn validate_executed_notional(prefix: &QuotedPrefix, quote: u64) -> VelocityResult<bool> {
    // Exact, not a band. Quoting and executing happen in one transaction and a
    // quoter's book cannot change between them, so the ladder fixes the
    // notional of any prefix of itself. Walking the ladder is arithmetic on a
    // published schedule. The single division from `price * base` into quote
    // units is the only rounding, and the quoter's own encoder must carry the
    // same one. `quote_size` in the CLOB's execute differences running floors
    // for this reason.
    Ok(quote as u128 == prefix.scaled_quote.safe_div(BASE_PRECISION)?)
}

/// Hold one balance change to the quoted prefix's price range. Every unit of
/// the change must be priced inside that range. A bound on the response total
/// alone would let a quoter overpay one maker out of another's pocket while
/// the total stayed honest. This bounds each subject's price to what the
/// quoter published.
///
/// `orders` is how many of the quoter's own orders the change merges. A merged
/// record cannot be exact even when the response total is, because the
/// remainder carried between fills lands in whichever record follows. So the
/// band admits one quote unit of slack per merged order. At quote precision
/// that slack is sub-cent dust. The response the quoter had to declare bounds
/// it, because a CLOB change names every order it consumed except the one it
/// left a remainder on.
pub fn validate_change_notional(
    prefix: &QuotedPrefix,
    base: u64,
    quote: u64,
    orders: u64,
) -> VelocityResult<bool> {
    let base = base as u128;
    let at_best = (prefix.best_price as u128).safe_mul(base)?;
    let at_worst = (prefix.worst_price as u128).safe_mul(base)?;
    // The quoter reports `orders`, so cap the rounding slack it buys. One real
    // change never merges more than the levels the router split off a book. A
    // figure past that cap is a quoter widening its own price band. The cap
    // costs an honest quoter nothing, because `MAX_LEVELS_PER_BOOK` sub-cent
    // units is already more slack than a real merge needs.
    let orders = orders.min(MAX_LEVELS_PER_BOOK as u64);
    notional_within(at_best.min(at_worst), at_best.max(at_worst), quote, orders)
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
                withheld: PriceLevel::default(),
            })
            .collect();
        split_across_quoters(direction, size, &books, 1).unwrap()
    }

    const B: u64 = 1_000_000_000; // one base unit

    /// Withheld depth takes no part of the size.
    ///
    /// A book reports depth it holds for an owner the transaction does not
    /// carry. That report drives [`withheld_obligation`] and not the division.
    /// The taker asked to trade, so the size goes to the sources that can fill
    /// it. Holding size back for a price nobody can reach would leave the taker
    /// short of a fill it could have had.
    #[test]
    fn withheld_depth_takes_no_size() {
        // The book quotes 1 @ 100 and reports 2 more @ 101 that it cannot
        // reach. The vAMM offers 5 @ 102, worse than the report.
        let quoted = vec![PriceLevel {
            price: 100,
            size: B,
        }];
        let amm = vec![PriceLevel {
            price: 102,
            size: 5 * B,
        }];
        let books = vec![
            QuoterBook {
                priority: CLOB,
                levels: &quoted,
                withheld: PriceLevel {
                    price: 101,
                    size: 2 * B,
                },
            },
            QuoterBook {
                priority: VAMM,
                levels: &amm,
                withheld: PriceLevel::default(),
            },
        ];

        let out = split_across_quoters(Direction::Long, 5 * B, &books, 1).unwrap();
        assert_eq!(out[0].base, B, "the book fills what it quoted");
        assert_eq!(out[1].base, 4 * B, "the vAMM fills the rest");
        assert_eq!(
            out[0].base + out[1].base,
            5 * B,
            "the taker is filled in full despite the withheld report"
        );
    }

    /// A taker that signed the transaction chose its own account list, so no
    /// filler owes it anything.
    #[test]
    fn a_taker_that_signed_is_owed_nothing() {
        let signed = FillerObligation {
            taker_signed: true,
            tx_accounts: None,
            unrouted_quoters: 0,
        };
        assert!(withheld_obligation(&signed, 7, 0).is_ok());
    }

    /// A fill that withholds and cannot count the transaction fails closed.
    /// Otherwise a filler would omit the sysvar to skip the check.
    #[test]
    fn an_uncountable_transaction_is_refused() {
        let blind = FillerObligation {
            taker_signed: false,
            tx_accounts: None,
            unrouted_quoters: 0,
        };
        assert_eq!(
            withheld_obligation(&blind, 0, TX_WRITABLE_LOCK_BUDGET),
            Err(ErrorCode::FillerObligationUncountable)
        );
    }

    /// Room for one more maker means the filler owed that maker.
    #[test]
    fn room_for_another_maker_is_an_omission() {
        let ceiling = TX_WRITABLE_LOCK_BUDGET - MAKER_ACCOUNT_COST;
        for accounts in [0, ceiling - 1, ceiling] {
            let roomy = FillerObligation {
                taker_signed: false,
                tx_accounts: Some(accounts),
                unrouted_quoters: 0,
            };
            assert_eq!(
                withheld_obligation(&roomy, 0, accounts),
                Err(ErrorCode::FillerOmittedReachableMaker),
                "{accounts} accounts leaves room for a maker"
            );
        }
    }

    /// A quoter entry the taker never named costs the locks the withheld maker
    /// needed, and the fill then prices against that entry instead of the book.
    #[test]
    fn a_quoter_outside_the_signed_route_is_refused() {
        let full = FillerObligation {
            taker_signed: false,
            tx_accounts: Some(TX_WRITABLE_LOCK_BUDGET),
            unrouted_quoters: 1,
        };
        assert_eq!(
            withheld_obligation(&full, 0, TX_WRITABLE_LOCK_BUDGET),
            Err(ErrorCode::FillerCarriedUnroutedQuoter)
        );
    }

    /// The three tests are independent. Only a transaction that passes all
    /// three fills.
    #[test]
    fn a_full_honest_transaction_may_withhold() {
        let honest = FillerObligation {
            taker_signed: false,
            tx_accounts: Some(TX_WRITABLE_LOCK_BUDGET),
            unrouted_quoters: 0,
        };
        assert!(withheld_obligation(&honest, 0, TX_WRITABLE_LOCK_BUDGET).is_ok());
    }

    /// A full transaction still fails when it carries a user that did nothing.
    /// The missing maker needed those locks.
    #[test]
    fn a_padded_user_set_is_refused() {
        let full = FillerObligation {
            taker_signed: false,
            tx_accounts: Some(TX_WRITABLE_LOCK_BUDGET),
            unrouted_quoters: 0,
        };
        assert_eq!(
            withheld_obligation(&full, 1, TX_WRITABLE_LOCK_BUDGET),
            Err(ErrorCode::FillerPaddedTheUserSet)
        );
        assert!(
            withheld_obligation(&full, 0, TX_WRITABLE_LOCK_BUDGET).is_ok(),
            "a full transaction whose every user filled has met the obligation"
        );
    }

    /// A writable meta may name any pubkey, so a shallow fill can claim a
    /// full transaction. The room test reads the locks velocity attributes to
    /// this fill, which padding does not raise.
    #[test]
    fn a_padded_account_list_is_refused() {
        let padded = FillerObligation {
            taker_signed: false,
            tx_accounts: Some(TX_WRITABLE_LOCK_BUDGET * 2),
            unrouted_quoters: 0,
        };
        let shallow = FILL_FIXED_WRITABLE_LOCKS + MAKER_ACCOUNT_COST;
        assert_eq!(
            withheld_obligation(&padded, 0, shallow),
            Err(ErrorCode::FillerOmittedReachableMaker),
            "a fill of {shallow} attributable locks had room for the maker it omitted"
        );
        assert!(
            withheld_obligation(&padded, 0, TX_WRITABLE_LOCK_BUDGET).is_ok(),
            "the same transaction fills once its locks are work velocity can name"
        );
    }

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
        // Demand 3 across depth 6 gives 1 and 2.
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
        // 1 @ 100 (custom) and 2 @ 101 (clob). 102 is never reached. A better
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

    #[test]
    fn a_zero_price_or_size_level_is_rejected_on_ingestion() {
        // Not merely truncated: the raw levels reach the taker-limit cut, the
        // margin clamp and the vAMM's last look, where an unfillable level
        // still moves the outcome.
        assert!(validate_quoted_levels(Direction::Long, &[level(0, B)]).is_err());
        assert!(validate_quoted_levels(Direction::Long, &[level(100, 0)]).is_err());
        assert!(
            validate_quoted_levels(Direction::Long, &[level(100, B), level(0, u64::MAX)]).is_err()
        );
        assert!(validate_quoted_levels(Direction::Long, &[level(100, B)]).is_ok());
    }

    #[test]
    fn levels_must_run_best_price_first() {
        assert!(validate_quoted_levels(Direction::Long, &[level(100, B), level(99, B)]).is_err());
        assert!(validate_quoted_levels(Direction::Short, &[level(100, B), level(101, B)]).is_err());
        assert!(validate_quoted_levels(Direction::Long, &[level(100, B), level(101, B)]).is_ok());
        assert!(validate_quoted_levels(Direction::Short, &[level(101, B), level(100, B)]).is_ok());
    }

    /// A ladder's rungs come from distinct offsets that can round to the same
    /// tick, so equal consecutive prices are normal.
    #[test]
    fn equal_consecutive_prices_are_legal() {
        assert!(validate_quoted_levels(
            Direction::Long,
            &[level(100, B), level(100, B), level(101, B)]
        )
        .is_ok());
        assert!(validate_quoted_levels(
            Direction::Short,
            &[level(100, B), level(100, B), level(99, B)]
        )
        .is_ok());
        // They also price as one price, so the notional is pinned exactly.
        let levels = [level(100 * PRICE, B), level(100 * PRICE, B)];
        let prefix = quoted_prefix(&levels, 1, 2 * B).unwrap();
        assert_eq!(prefix.best_price, prefix.worst_price);
        assert!(validate_executed_notional(&prefix, 200 * PRICE).unwrap());
        assert!(!validate_executed_notional(&prefix, 200 * PRICE + 2).unwrap());
    }

    /// The single terminal division rounds down, and nothing either side of it
    /// is admitted: a quoter that rounds the other way owes a different number
    /// than the ladder it published.
    #[test]
    fn the_prefix_notional_rounds_down_exactly() {
        // 3 base units at a price that does not divide evenly. The exact
        // notional is 1.5 quote units.
        let levels = [level(PRICE / 2, 3)];
        let prefix = quoted_prefix(&levels, 1, 3).unwrap();
        assert_eq!(prefix.scaled_quote, (PRICE as u128 / 2) * 3);
        assert!(validate_executed_notional(&prefix, 1).unwrap());
        assert!(!validate_executed_notional(&prefix, 2).unwrap());
        assert!(!validate_executed_notional(&prefix, 3).unwrap());
    }

    const PRICE: u64 = BASE_PRECISION as u64;

    fn ladder() -> [PriceLevel; 2] {
        [level(100 * PRICE, B), level(102 * PRICE, B)]
    }

    /// The volume decides which units were filled. The quoted prices of those
    /// units decide the notional. A quoter that fills only the cheap half of
    /// its allocation cannot charge the whole allocation's average.
    #[test]
    fn a_partial_fill_is_priced_at_the_prefix_it_reached() {
        let levels = ladder();
        // Full allocation: both levels, 202 quote for 2 base units.
        let full = quoted_prefix(&levels, 1, 2 * B).unwrap();
        assert_eq!(full.scaled_quote, 202u128 * PRICE as u128 * B as u128);
        assert!(validate_executed_notional(&full, 202 * PRICE).unwrap());
        assert!(!validate_executed_notional(&full, 202 * PRICE + 2).unwrap());

        // Half filled: only the 100 level was reached, so 100 is the bar. The
        // 101 average over the whole allocation is not available.
        let half = quoted_prefix(&levels, 1, B).unwrap();
        assert_eq!(half.best_price, 100 * PRICE);
        assert_eq!(half.worst_price, 100 * PRICE);
        assert!(validate_executed_notional(&half, 100 * PRICE).unwrap());
        assert!(!validate_executed_notional(&half, 101 * PRICE).unwrap());
    }

    #[test]
    fn charging_worse_than_quoted_is_rejected_in_both_directions() {
        // 2 base units off a 100/102 ladder is 202, and only 202.
        let asks = ladder();
        let long = quoted_prefix(&asks, 1, 2 * B).unwrap();
        assert!(validate_executed_notional(&long, 202 * PRICE).unwrap());
        // More than quoted overcharges the taker.
        assert!(!validate_executed_notional(&long, 203 * PRICE).unwrap());
        // Less than quoted underpays the makers.
        assert!(!validate_executed_notional(&long, 201 * PRICE).unwrap());
        assert!(!validate_executed_notional(&long, 199 * PRICE).unwrap());

        // A short taker receives quote. The ladder pins it as tightly.
        let bids = [level(100 * PRICE, B), level(98 * PRICE, B)];
        let short = quoted_prefix(&bids, 1, 2 * B).unwrap();
        assert_eq!(short.scaled_quote, 198u128 * PRICE as u128 * B as u128);
        assert!(validate_executed_notional(&short, 198 * PRICE).unwrap());
        assert!(!validate_executed_notional(&short, 197 * PRICE).unwrap());
        assert!(!validate_executed_notional(&short, 199 * PRICE).unwrap());
    }

    /// Nothing a quoter returns may exceed what the router allocated to it.
    #[test]
    fn a_fill_past_the_quoted_depth_has_no_prefix() {
        let levels = ladder();
        assert!(quoted_prefix(&levels, 1, 2 * B).is_ok());
        assert!(quoted_prefix(&levels, 1, 2 * B + 1).is_err());
    }

    /// The response total can be honest while one subject is paid out of
    /// another's pocket, so each change is held to the prefix's band too.
    #[test]
    fn a_single_change_is_held_to_the_quoted_band() {
        let levels = ladder();
        let prefix = quoted_prefix(&levels, 1, 2 * B).unwrap();
        // Inside the 100..102 band.
        assert!(validate_change_notional(&prefix, B, 100 * PRICE, 1).unwrap());
        assert!(validate_change_notional(&prefix, B, 102 * PRICE, 1).unwrap());
        // Outside it either way.
        assert!(!validate_change_notional(&prefix, B, 99 * PRICE, 1).unwrap());
        assert!(!validate_change_notional(&prefix, B, 103 * PRICE, 1).unwrap());
        // Slack is one quote unit per merged order, not a licence to reprice.
        assert!(validate_change_notional(&prefix, B, 100 * PRICE - 4, 4).unwrap());
        assert!(!validate_change_notional(&prefix, B, 100 * PRICE - 6, 4).unwrap());
    }

    /// A level whose size is not a step multiple is quantized the same way the
    /// split quantizes it, so the prefix prices the units the split routed.
    #[test]
    fn the_prefix_quantizes_levels_like_the_split_does() {
        let levels = [level(100 * PRICE, 3), level(200 * PRICE, 4)];
        // Step 2: the first level yields 2, the second 4.
        let prefix = quoted_prefix(&levels, 2, 6).unwrap();
        assert_eq!(
            prefix.scaled_quote,
            (100 * PRICE as u128) * 2 + (200 * PRICE as u128) * 4
        );
        assert!(quoted_prefix(&levels, 2, 7).is_err());
    }
}
