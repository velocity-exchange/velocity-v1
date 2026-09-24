//! The side walks, and the three reads of a side that are built on them:
//! `quote`, `quote_l3` and `execute`. `quote` and `execute` share
//! [`SweepGate`], which decides what a caller may take of each order.

use {
    super::{
        arena::unlink_order,
        budget::{settleable, DistinctUsers, Settleable, UserBudget},
        hints::holds_expiry_hint,
        reservation::CrossReservation,
        BookHeader, ClobBook, NodeArena,
    },
    crate::{
        error::ClobError,
        events::FillSlimV0,
        state::{
            response_pointer, user_set_within_capacity, CancelledRemainderV0, ClobMarketV0,
            CompletedOrderV0, DirectionV0, ExecuteOutcome, L3RowV0, OrderNodeV0,
            PartiallyFilledOrderV0, PriceLevelV0, ResponsePointerV0, SideV0, UserBalanceChangeV0,
            UserCapsV0, UserRefV0, EXECUTE_FILLS_CEILING, EXECUTE_USERS_CEILING, L3_ROWS_CEILING,
            NIL, QUOTE_LEVELS_CEILING,
        },
    },
    anchor_lang::prelude::*,
    quoter_spec::{ExecuteArgsV0, ExecuteWriter, L3Writer, QuoteArgsV0, QuoteWriter},
};

/// Whether a book walk continues past the node just visited.
pub(crate) enum Walk {
    Continue,
    Stop,
}

/// Walk one side from the best of book outward, handing each node to
/// `visit` by copy along with its arena index.
///
/// The walk reads the successor link before `visit` runs, so a visitor may
/// unlink the node it is looking at without losing its place. Execute does
/// that. [`NodeArena::read_node`] bounds-validates every hop, and the walk
/// refuses to take more hops than the arena has slots. A list corrupted into a
/// cycle therefore errors out instead of spending the whole compute budget.
pub(crate) fn walk_side<F>(book: &mut ClobMarketV0, side: SideV0, mut visit: F) -> Result<()>
where
    F: FnMut(&mut ClobMarketV0, u32, &OrderNodeV0) -> Result<Walk>,
{
    let max_hops = book.capacity();
    let mut hops = 0usize;
    let mut cursor = book.best(side);
    while cursor != NIL {
        let node = book.read_node(cursor)?;
        let next = node.next;
        hops += 1;
        require!(hops <= max_hops, ClobError::BookInvariantViolated);
        if matches!(visit(book, cursor, &node)?, Walk::Stop) {
            break;
        }

        cursor = next;
    }

    Ok(())
}

/// The read-only form of [`walk_side`], for a caller that holds only `&`.
///
/// The visitor cannot unlink, so this walk reads the successor after the
/// visit rather than before it. The hop guard is the same, so a list
/// corrupted into a cycle errors out instead of spending the whole compute
/// budget.
pub(crate) fn walk_side_ref<F>(book: &ClobMarketV0, side: SideV0, mut visit: F) -> Result<()>
where
    F: FnMut(u32, &OrderNodeV0) -> Result<Walk>,
{
    let max_hops = book.capacity();
    let mut hops = 0usize;
    let mut cursor = book.best(side);
    while cursor != NIL {
        let node = book.read_node(cursor)?;
        hops += 1;
        require!(hops <= max_hops, ClobError::BookInvariantViolated);
        if matches!(visit(cursor, &node)?, Walk::Stop) {
            break;
        }

        cursor = node.next;
    }

    Ok(())
}

/// Whether anyone at all could match this order now, which is a property of the
/// order alone. Every read of a side asks this first and [`CrossReservation`]
/// second, because a claim is allocated positionally over the matchable orders.
/// A reason belonging to the caller must be tested after that allocation.
pub(crate) fn is_live(node: &OrderNodeV0, slot: u64, now: i64) -> bool {
    !node.is_expired(now) && node.is_active(slot)
}

/// Aggregate the levels a taker of `direction`/`size` would clear,
/// best-first, capped at the market's `max_quote_levels`, and stream them
/// into the response region as wincode [`crate::state::QuoteResponseV0`].
/// Every skip goes through [`SweepGate`], which [`execute`] asks in
/// the same place, so the router's split math matches what execute will
/// deliver. That covers book-trade prevention, the unknown-user grace rule
/// and the caller's budgets.
///
/// It also withholds whatever [`CrossReservation`] holds back. That is the
/// units a crossing taker remainder claims, and the whole of a remainder a
/// counterparty crosses. A level that loses all of its size to a claim is
/// not published at all.
pub(super) fn quote(
    book: &mut ClobMarketV0,
    args: &QuoteArgsV0,
    slot: u64,
    now: i64,
) -> Result<ResponsePointerV0> {
    let side = args.direction.side();
    // A level aggregates however many orders sit at one price, so a ladder
    // capped on levels alone would quote depth that execute declines.
    let max_execute_fills = book.max_execute_fills.min(EXECUTE_FILLS_CEILING) as usize;
    let max_execute_users = book.max_execute_users.min(EXECUTE_USERS_CEILING) as usize;

    let mut gate = SweepGate::new(book, side, args.into(), slot, now)?;
    let mut ladder = QuoteLadder::new(
        side,
        book.max_quote_levels.min(QUOTE_LEVELS_CEILING) as usize,
    );
    let mut truncations = TruncationSlots::default();
    let mut users_promised = DistinctUsers::new(if args.users.is_empty() {
        max_execute_users
    } else {
        0
    });

    let mut remaining = args.size;
    let mut promised_fills = 0usize;
    let mut unsettleable_level: Option<PriceLevelV0> = None;

    walk_side(book, side, |book, _, node| {
        // Stop where `execute` stops, so the ladder ends where the fill would.
        if remaining == 0 || promised_fills == max_execute_fills {
            return Ok(Walk::Stop);
        }

        if args
            .direction
            .worse_than_limit(node.price, args.limit_price)
        {
            return Ok(Walk::Stop);
        }

        let take = match gate.offer(book, node, remaining)? {
            Offer::Take(take) => take,
            Offer::Skip => return Ok(Walk::Continue),
            Offer::Withheld { available } => {
                unsettleable_level = Some(PriceLevelV0 {
                    price: node.price,
                    size: available,
                });

                return Ok(Walk::Stop);
            }
        };

        if !truncations.claim(node, take.base, book.min_order_size)
            || !users_promised.admit(take.owner, node, max_execute_users)
        {
            return Ok(Walk::Stop);
        }

        promised_fills += 1;
        if !ladder.add(book, node.price, take.base)? {
            return Ok(Walk::Stop);
        }

        remaining -= take.base;
        Ok(if remaining == 0 {
            Walk::Stop
        } else {
            Walk::Continue
        })
    })?;

    ladder.finish(book, unsettleable_level)
}

/// Describe the resting orders behind the ladder, best price first.
///
/// This is the counterpart of [`quote`]. It runs the same walk under
/// the same skip rules, and it writes one row per order instead of one
/// level per price. It exists because a book is the one quoter whose ladder
/// stands on other people's orders. A caller that has to carry those users'
/// accounts, or draw the book, cannot get that from an aggregated ladder,
/// and the only alternative is to decode this account from outside.
///
/// There is no user set and there are no caps. The caller asks because it
/// does not know yet whose accounts to bring, so an order is reported
/// whatever the caller could settle today. The taker's own orders are
/// reported too and the caller drops them, because only the caller knows
/// who it is.
pub(super) fn quote_l3(
    book: &mut ClobMarketV0,
    direction: DirectionV0,
    size: u64,
    max_rows: u16,
    include_taker_origin_reservations: bool,
    slot: u64,
    now: i64,
) -> Result<ResponsePointerV0> {
    let side = direction.side();
    let rows_wanted = max_rows.min(L3_ROWS_CEILING) as usize;
    let mut reservation =
        CrossReservation::new(book, side, slot, now, include_taker_origin_reservations);
    let mut writer = L3Writer::new();
    // Zero asks for the whole side rather than for nothing. A caller that
    // draws a book has no size in mind.
    let mut remaining = if size == 0 { u64::MAX } else { size };
    let mut depth_left_behind = false;

    walk_side(book, side, |book, index, node| {
        if remaining == 0 || writer.rows() == rows_wanted {
            depth_left_behind = true;
            return Ok(Walk::Stop);
        }

        if !is_live(node, slot, now) {
            return Ok(Walk::Continue);
        }

        // The row stays even when a remainder claims it all. It names an owner
        // the caller may still have to carry, and the flag says why the size is
        // short.
        let withheld = reservation.withheld(book, node)?;
        // Read before the call, which borrows `book.response` mutably.
        let blocking_min_size = book.blocking_min_size;
        writer
            .push_row(
                &mut book.response,
                L3RowV0 {
                    price: node.price,
                    size: node.base_asset_amount.saturating_sub(withheld),
                    order_id: node.order_id,
                    node_index: index,
                    user: node.user_ref(),
                    flags: l3_row_flags(node, blocking_min_size, withheld != 0),
                    _pad: [0; 1],
                    placed_slot: node.placed_slot,
                },
            )
            .map_err(ClobError::from)?;
        remaining = remaining.saturating_sub(node.base_asset_amount.saturating_sub(withheld));
        Ok(Walk::Continue)
    })?;

    let len = writer
        .finish(&mut book.response, depth_left_behind)
        .map_err(ClobError::from)?;
    response_pointer(len)
}

/// Consume matchable orders best-first, removing filled orders and
/// streaming each maker's share into the response region as wincode
/// [`crate::state::ExecuteResponseV0`] for velocity to apply. The walk
/// skips an expired order and never removes it. Reclamation goes through
/// [`ClobBook::remove_expired`] so the maker's aggregates update. A partial
/// fill that leaves a remainder below `min_order_size` culls the order,
/// because dust must not hold an arena slot. The cull rides the wire
/// response, because that maker was filled and is therefore loaded. There
/// is no price bound, because the router already chose this quoter's
/// allocation from its quote.
///
/// Every skip goes through [`SweepGate`], which [`quote`] asks in the
/// same place, so the two never disagree about what is takeable.
pub(super) fn execute(
    book: &mut ClobMarketV0,
    args: &ExecuteArgsV0,
    slot: u64,
    now: i64,
) -> Result<ExecuteOutcome> {
    let side = args.direction.side();
    let max_fills = book.max_execute_fills.min(EXECUTE_FILLS_CEILING) as usize;
    let max_users = book.max_execute_users.min(EXECUTE_USERS_CEILING) as usize;

    let mut gate = SweepGate::new(book, side, args.into(), slot, now)?;
    let mut report = ExecuteReport::new(book, side, max_fills);
    let mut truncations = TruncationSlots::default();
    let mut remaining = args.size;

    walk_side(book, side, |book, index, node| {
        if remaining == 0 || report.fills.len() == max_fills {
            return Ok(Walk::Stop);
        }

        let take = match gate.offer(book, node, remaining)? {
            Offer::Take(take) => take,
            Offer::Skip => return Ok(Walk::Continue),
            Offer::Withheld { .. } => return Ok(Walk::Stop),
        };

        // A per-owner budget can truncate two orders, so the walk ends where
        // the response runs out of room. Ending short is a smaller fill the
        // caller reads off the response, rather than a failure.
        if !truncations.claim(node, take.base, book.min_order_size) {
            return Ok(Walk::Stop);
        }

        let Some(change_index) = report.merge_change(book, node, take.base, max_users)? else {
            return Ok(Walk::Stop);
        };

        report.settle_order(book, index, node, take.base, change_index)?;
        remaining -= take.base;
        Ok(if remaining == 0 {
            Walk::Stop
        } else {
            Walk::Continue
        })
    })?;

    report.finish(book, slot)
}

/// What `quote` and `execute` share of their arguments: who the caller can
/// settle against, and what it has claimed for itself.
struct SweepRequest<'a> {
    users: &'a [UserRefV0],
    caps: &'a UserCapsV0,
    reference_price: Option<u64>,
    taker: Option<&'a UserRefV0>,
    include_taker_origin_reservations: bool,
}

impl<'a> From<&'a QuoteArgsV0<'_>> for SweepRequest<'a> {
    fn from(args: &'a QuoteArgsV0<'_>) -> Self {
        Self {
            users: args.users,
            caps: &args.caps,
            reference_price: args.reference_price,
            taker: args.taker.as_ref(),
            include_taker_origin_reservations: args.include_taker_origin_reservations,
        }
    }
}

impl<'a> From<&'a ExecuteArgsV0<'_>> for SweepRequest<'a> {
    fn from(args: &'a ExecuteArgsV0<'_>) -> Self {
        Self {
            users: args.users,
            caps: &args.caps,
            reference_price: args.reference_price,
            taker: args.taker.as_ref(),
            include_taker_origin_reservations: args.include_taker_origin_reservations,
        }
    }
}

/// The rules that decide whether a sweep may take an order, and how much of
/// it. `quote` and `execute` ask it at the same point of their walks, so a
/// ladder never promises depth the fill would decline.
struct SweepGate<'a> {
    users: &'a [UserRefV0],
    taker: Option<&'a UserRefV0>,
    slot: u64,
    now: i64,
    reservation: CrossReservation,
    budget: UserBudget<'a>,
}

/// How much of an order the caller may take.
struct Take {
    base: u64,
    /// The owner's position in the caller's set. `None` only when the set is
    /// empty.
    owner: Option<usize>,
}

enum Offer {
    Take(Take),
    /// Pass over the order to the depth behind it.
    Skip,
    /// The caller cannot settle for the owner, so the walk ends here.
    /// `available` is the depth a caller that loads the owner could take.
    Withheld {
        available: u64,
    },
}

impl<'a> SweepGate<'a> {
    fn new(
        book: &ClobMarketV0,
        side: SideV0,
        request: SweepRequest<'a>,
        slot: u64,
        now: i64,
    ) -> Result<Self> {
        require!(
            user_set_within_capacity(request.users),
            ClobError::OversizedUserSet
        );

        let include_reserved = request.include_taker_origin_reservations;
        Ok(Self {
            users: request.users,
            taker: request.taker,
            slot,
            now,
            reservation: CrossReservation::new(book, side, slot, now, include_reserved),
            budget: UserBudget::new(request.caps, side, request.reference_price)?,
        })
    }

    /// Runs the skips cheapest test first. The reservation is read before the
    /// self-trade test, because its allocation is positional.
    #[inline(always)]
    fn offer(&mut self, book: &ClobMarketV0, node: &OrderNodeV0, remaining: u64) -> Result<Offer> {
        if !is_live(node, self.slot, self.now) {
            return Ok(Offer::Skip);
        }

        let available = self.reservation.available(book, node)?;
        if available == 0 || is_takers_own(node, self.taker) {
            return Ok(Offer::Skip);
        }

        let owner = self.users.iter().position(|u| node.is_owned_by(u));
        match settleable(
            self.users,
            owner,
            node,
            book.unknown_user_grace_slots,
            book.blocking_min_size,
            self.slot,
        ) {
            Settleable::Yes => {}
            Settleable::SteppedOver => return Ok(Offer::Skip),
            // `available` is nonzero here, because a wholly claimed order is
            // passed over above.
            Settleable::Withheld => return Ok(Offer::Withheld { available }),
        }

        let base = self.budget.allow(
            owner,
            remaining.min(available),
            node.price,
            node.is_reduce_only(),
        );

        // The owner is out of room. The depth behind it is still fillable.
        if base == 0 {
            return Ok(Offer::Skip);
        }

        Ok(Offer::Take(Take { base, owner }))
    }
}

/// `execute` reports a truncated order in one of two single-slot sections: a
/// cull when the remainder falls under `min_order_size`, and a partial fill
/// otherwise. Both walks stop at the order that needs a used section.
#[derive(Default)]
struct TruncationSlots {
    cull_used: bool,
    partial_used: bool,
}

impl TruncationSlots {
    /// Claims the section a fill of `take` from `node` needs. False when that
    /// section is already used. A fill of the whole order needs neither.
    #[inline(always)]
    fn claim(&mut self, node: &OrderNodeV0, take: u64, min_order_size: u64) -> bool {
        if take == node.base_asset_amount {
            return true;
        }

        let used = if node.base_asset_amount - take < min_order_size {
            &mut self.cull_used
        } else {
            &mut self.partial_used
        };

        if *used {
            return false;
        }

        *used = true;
        true
    }
}

/// The quote response being built, one level per price.
struct QuoteLadder {
    writer: QuoteWriter,
    side: SideV0,
    max_levels: usize,
    open_level: Option<PriceLevelV0>,
    last_written_price: Option<u64>,
    levels_written: usize,
}

impl QuoteLadder {
    fn new(side: SideV0, max_levels: usize) -> Self {
        Self {
            writer: QuoteWriter::new(),
            side,
            max_levels,
            open_level: None,
            last_written_price: None,
            levels_written: 0,
        }
    }

    /// Adds `take` at `price`. False when the price would open a level past
    /// `max_levels`, which ends the walk.
    #[inline(always)]
    fn add(&mut self, book: &mut ClobMarketV0, price: u64, take: u64) -> Result<bool> {
        // The side is price-sorted, so orders at one price are contiguous and
        // one open level holds them all.
        match self.open_level {
            Some(level) if level.price == price => {
                self.open_level = Some(PriceLevelV0 {
                    price,
                    size: level.size.checked_add(take).ok_or(ClobError::MathError)?,
                });
            }
            _ => {
                if self.levels_written == self.max_levels {
                    return Ok(false);
                }

                if let Some(level) = self.open_level {
                    self.write(book, level)?;
                }

                self.open_level = Some(PriceLevelV0 { price, size: take });
                self.levels_written += 1;
            }
        }

        Ok(true)
    }

    /// Writes the open level, then the withheld report behind the ladder,
    /// which is the shape `QuoteResponseV0` declares.
    fn finish(
        mut self,
        book: &mut ClobMarketV0,
        withheld: Option<PriceLevelV0>,
    ) -> Result<ResponsePointerV0> {
        if let Some(level) = self.open_level {
            self.write(book, level)?;
        }

        let len = self
            .writer
            .finish(&mut book.response, withheld.unwrap_or_default())
            .map_err(ClobError::from)?;
        response_pointer(len)
    }

    /// Append one level, re-checking on the way out what the wire type
    /// promises: best-price-first, and every level fillable.
    ///
    /// The router picks a quoter by exactly these numbers, so a zero price, a
    /// zero size, or a level improving on the one before it would win a
    /// waterfall the book cannot honour. A book holding its invariants produces
    /// none of the three, and this check says so.
    fn write(&mut self, book: &mut ClobMarketV0, level: PriceLevelV0) -> Result<()> {
        require!(
            level.price != 0 && level.size != 0,
            ClobError::InvalidResponseLevel
        );
        require!(
            self.last_written_price
                .is_none_or(|before| self.side.is_worse_price(level.price, before)),
            ClobError::InvalidResponseLevel
        );

        self.writer
            .push_level(&mut book.response, level)
            .map_err(ClobError::from)?;
        self.last_written_price = Some(level.price);
        Ok(())
    }
}

/// Prices each fill by differencing a running total. The sweep then rounds
/// once in total instead of once per fill, and the dust a per-fill truncation
/// loses would come out of the makers.
struct FillPricing {
    side: SideV0,
    base_precision: u128,
    last_filled_price: Option<u64>,
    swept_notional: u128,
    quote_attributed: u128,
}

impl FillPricing {
    /// The quote that `take` base at `price` adds to the sweep.
    #[inline(always)]
    fn quote_size(&mut self, price: u64, take: u64) -> Result<u64> {
        check_fill_price(self.side, self.last_filled_price, price, take)?;
        self.last_filled_price = Some(price);

        self.swept_notional = self
            .swept_notional
            .checked_add(
                (price as u128)
                    .checked_mul(take as u128)
                    .ok_or(ClobError::MathError)?,
            )
            .ok_or(ClobError::MathError)?;
        let swept_quote = self.swept_notional / self.base_precision;
        let quote_size: u64 = (swept_quote - self.quote_attributed)
            .try_into()
            .map_err(|_| ClobError::MathError)?;
        self.quote_attributed = swept_quote;
        Ok(quote_size)
    }
}

/// The execute response being built, and the per-order detail the execute
/// event needs.
///
/// Fills merge by user. The records already written into the response are the
/// accumulator, so a repeat maker patches that record's totals in place
/// instead of building a heap `Vec` of balance changes.
struct ExecuteReport {
    writer: ExecuteWriter,
    side: SideV0,
    count_before: u32,
    /// A fill consumes at most one order, so both lists are bounded by
    /// `max_fills` and neither has to grow by doubling.
    fills: Vec<FillSlimV0>,
    completed: Vec<CompletedOrderV0>,
    cancelled: Option<CancelledRemainderV0>,
    partial: Option<PartiallyFilledOrderV0>,
    removals: u32,
    /// One expiry repair for the whole sweep, for the reason `cancel_all`
    /// batches its own. The repair walks the live orders, and a sweep can
    /// free many.
    owes_expiry_repair: bool,
    pricing: FillPricing,
}

impl ExecuteReport {
    fn new(book: &ClobMarketV0, side: SideV0, max_fills: usize) -> Self {
        Self {
            writer: ExecuteWriter::new(),
            side,
            count_before: book.node_count(side),
            fills: Vec::with_capacity(max_fills),
            completed: Vec::with_capacity(max_fills),
            cancelled: None,
            partial: None,
            removals: 0,
            owes_expiry_repair: false,
            pricing: FillPricing {
                side,
                base_precision: book.base_precision.max(1) as u128,
                last_filled_price: None,
                swept_notional: 0,
                quote_attributed: 0,
            },
        }
    }

    /// Adds the fill to its owner's balance change, and returns the change's
    /// index. `None` when the owner is new and the response already holds
    /// `max_users` changes, which ends the walk.
    #[inline(always)]
    fn merge_change(
        &mut self,
        book: &mut ClobMarketV0,
        node: &OrderNodeV0,
        take: u64,
        max_users: usize,
    ) -> Result<Option<u16>> {
        // A scan of the records written, because this frame has no room for a
        // table of the makers seen.
        let existing = self
            .writer
            .changes(&book.response)
            .map_err(ClobError::from)?
            .iter()
            .position(|change| node.is_owned_by(&change.user));
        if existing.is_none() && self.writer.changes_len() == max_users {
            return Ok(None);
        }

        let quote_size = self.pricing.quote_size(node.price, take)?;
        let Some(index) = existing else {
            let change = UserBalanceChangeV0 {
                base_size: take,
                quote_size,
                user: node.user_ref(),
                _pad: [0; 6],
            };

            let index = self
                .writer
                .push_change(&mut book.response, change)
                .map_err(ClobError::from)?;
            return Ok(Some(index));
        };

        let index = u16::try_from(index).map_err(|_| ClobError::MathError)?;
        let record = self
            .writer
            .change_mut(&mut book.response, index)
            .map_err(ClobError::from)?;
        record.base_size = record
            .base_size
            .checked_add(take)
            .ok_or(ClobError::MathError)?;
        record.quote_size = record
            .quote_size
            .checked_add(quote_size)
            .ok_or(ClobError::MathError)?;
        Ok(Some(index))
    }

    /// Records what the fill did to the order, and takes the order off the
    /// book when nothing restable is left of it.
    #[inline(always)]
    fn settle_order(
        &mut self,
        book: &mut ClobMarketV0,
        index: u32,
        node: &OrderNodeV0,
        take: u64,
        change_index: u16,
    ) -> Result<()> {
        self.fills.push(FillSlimV0 {
            order_id: node.order_id,
            client_order_id: node.client_order_id,
            base_size: take,
        });

        let remainder = node.base_asset_amount - take;
        if remainder == 0 {
            // The id rides a section of its own, written once the changes are
            // done. Growing the change in place would shift every record after it.
            self.completed.push(CompletedOrderV0 {
                order_id: node.order_id,
                change_index,
                flags: removed_order_flags(node),
                _pad: [0; 1],
                client_order_id: node.client_order_id,
            });
        } else if remainder < book.min_order_size {
            // `TruncationSlots` admits one cull per walk. Fail here rather than
            // drop a cull velocity must unwind.
            require!(self.cancelled.is_none(), ClobError::BookInvariantViolated);
            self.cancelled = Some(CancelledRemainderV0 {
                order_id: node.order_id,
                base_asset_amount: remainder,
                price: node.price,
                client_order_id: node.client_order_id,
                user: node.user_ref(),
                flags: removed_order_flags(node),
                _pad: [0; 1],
            });
        } else {
            // A balance change merges every order of one maker, so this is the
            // only place the fill says which order moved.
            require!(self.partial.is_none(), ClobError::BookInvariantViolated);
            self.partial = Some(PartiallyFilledOrderV0 {
                order_id: node.order_id,
                base_filled: take,
                client_order_id: node.client_order_id,
                change_index,
                _pad: [0; 2],
            });

            return book.update_node(index, |n| n.base_asset_amount = remainder);
        }

        self.owes_expiry_repair |= holds_expiry_hint(book, node);
        unlink_order(book, index)?;
        self.removals += 1;
        Ok(())
    }

    /// Writes the remaining sections, in the order `ExecuteResponseV0`
    /// declares them, and re-checks the book the sweep changed.
    fn finish(self, book: &mut ClobMarketV0, slot: u64) -> Result<ExecuteOutcome> {
        let len = self
            .writer
            .finish(
                &mut book.response,
                self.cancelled.as_slice(),
                &self.completed,
                self.partial.as_slice(),
            )
            .map_err(ClobError::from)?;
        let response = response_pointer(len)?;

        check_side_count(book, self.side, self.count_before, self.removals)?;

        if self.owes_expiry_repair {
            book.recompute_wake_hints(true, None)?;
        }

        // A fill is a removal path too, and it can retire the activation the
        // hint pointed at. It is the one such path that knows the slot without
        // being handed it.
        book.expire_activation_hint(slot)?;
        book.validate_book()?;

        Ok(ExecuteOutcome {
            response,
            fills: self.fills,
            cancelled_client_order_id: self.cancelled.map(|cull| cull.client_order_id),
        })
    }
}

/// The self-check of [`QuoteLadder::write`] for a fill entering the execute response. The price is not
/// on the wire, but execute values the fill as `price * base` over the same
/// best-first walk, so the ordering still has to hold. Equal consecutive prices are
/// expected, because one level is contiguous orders and each is its own fill.
fn check_fill_price(side: SideV0, filled: Option<u64>, price: u64, take: u64) -> Result<()> {
    require!(price != 0 && take != 0, ClobError::InvalidResponseLevel);
    require!(
        filled.is_none_or(|before| !side.is_worse_price(before, price)),
        ClobError::InvalidResponseLevel
    );

    Ok(())
}

/// A walk that removed `removals` orders from `side` took every one of them
/// off that side.
pub(super) fn check_side_count(
    book: &ClobMarketV0,
    side: SideV0,
    count_before: u32,
    removals: u32,
) -> Result<()> {
    let expected = count_before
        .checked_sub(removals)
        .ok_or(ClobError::BookInvariantViolated)?;
    require!(
        book.node_count(side) == expected,
        ClobError::BookInvariantViolated
    );

    Ok(())
}

/// The caller's own resting order. No read of a side offers such an order back
/// to the caller, which is self-trade prevention.
fn is_takers_own(node: &OrderNodeV0, taker: Option<&UserRefV0>) -> bool {
    taker.is_some_and(|t| node.is_owned_by(t))
}

/// The flags a fill reports on an order it removed. The caller keeps a count of
/// the owner's reduce-only orders and disarms it from this.
fn removed_order_flags(node: &OrderNodeV0) -> u8 {
    if node.is_reduce_only() {
        quoter_spec::L3_ROW_FLAG_REDUCE_ONLY
    } else {
        0
    }
}

/// The facts about an order a caller cannot see from its price and size.
///
/// `L3_ROW_FLAG_BLOCKS_WALK` says this order can end a walk, so its owner gates
/// the depth behind it. The book reports it so the floor stays the book's rule.
/// `L3_ROW_FLAG_RESERVED` says a crossing taker remainder claims the rest of the
/// row's size.
fn l3_row_flags(node: &OrderNodeV0, blocking_min_size: u64, reserved: bool) -> u8 {
    let mut flags = 0;
    if node.is_taker_origin() {
        flags |= quoter_spec::L3_ROW_FLAG_TAKER_ORIGIN;
    }

    if reserved {
        flags |= quoter_spec::L3_ROW_FLAG_RESERVED;
    }

    if blocking_min_size == 0 || node.base_asset_amount >= blocking_min_size {
        flags |= quoter_spec::L3_ROW_FLAG_BLOCKS_WALK;
    }

    if node.is_reduce_only() {
        flags |= quoter_spec::L3_ROW_FLAG_REDUCE_ONLY;
    }

    flags
}
