//! The claims a crossing taker remainder holds on the depth it crosses, and the
//! rules that bind such a remainder to the book while its claim holds.

use {
    super::{
        walk::{is_live, walk_side_ref, Walk},
        BookHeader, ClobBook, NodeArena,
    },
    crate::{
        error::ClobError,
        state::{ClobMarketV0, OrderNodeV0, SideV0, NIL},
    },
    anchor_lang::prelude::*,
};

/// Past the window in which the book honours this remainder's claim on the
/// depth it crosses. The window runs from the activation slot, which is when the
/// auction the claim protects ends.
fn is_claim_lapsed(claimant: &OrderNodeV0, slot: u64, grace_slots: u64) -> bool {
    slot >= claimant.activation_slot.saturating_add(grace_slots)
}

/// A taker remainder whose claim the book still honours. Its owner cannot
/// cancel it without `force`. The bind ends exactly when the claim lapses.
pub(super) fn is_bound(node: &OrderNodeV0, slot: u64, grace_slots: u16) -> bool {
    node.is_taker_origin() && !is_claim_lapsed(node, slot, grace_slots as u64)
}

/// The worst-priced order on `side` that eviction may take, or [`NIL`] when
/// every order there is a bound remainder. A bound remainder is passed over
/// rather than refusing the eviction, so its owner cannot use a permissionless
/// crank to pull it, and the side still frees a slot while the claim holds.
/// The search passes only bound remainders, so the side's claimant count
/// bounds it.
pub(crate) fn evictable_order(book: &ClobMarketV0, side: SideV0, slot: u64) -> Result<u32> {
    let mut cursor = book.worst(side);
    let mut bound_passed = 0u16;
    while cursor != NIL {
        let node = book.read_node(cursor)?;
        if !is_bound(&node, slot, book.reservation_grace_slots) {
            return Ok(cursor);
        }

        require!(
            bound_passed < book.claimant_count(side),
            ClobError::BookInvariantViolated
        );

        bound_passed += 1;
        cursor = node.prev;
    }

    Ok(NIL)
}

/// What a crossing taker remainder has claimed on the side being read, and what
/// `include_taker_origin_reservations` lets one caller reach. It is unrelated to
/// the margin a maker reserves at placement.
///
/// The book skips claimed units rather than refusing the call, and a claim lapses
/// [`crate::state::ClobHeaderV0::reservation_grace_slots`] past the activation slot. Every read
/// of a side runs this, so no two disagree. See `docs/taker-remainder-auction.md`.
pub(crate) struct CrossReservation {
    /// The side being read. A claimant rests on the other one and takes this
    /// side as its cover.
    cover: SideV0,
    slot: u64,
    now: i64,
    /// See [`crate::state::ClobHeaderV0::reservation_grace_slots`].
    grace_slots: u64,
    /// Floor on the units one claim withholds. See [`Self::claimed`].
    min_order_size: u64,
    /// The caller settles the cross itself, so it reads the book with every
    /// claim ignored. This is `include_taker_origin_reservations` on the
    /// quoter surface. Velocity signs the CPI, and the book trusts the flag
    /// the way it already trusts `users` and `caps`.
    include_reserved: bool,
    /// The claimant being allocated, or [`NIL`] once the list is spent. A
    /// [`NIL`] head is the whole cost of this type on a book that holds no
    /// remainder.
    cursor: u32,
    /// Successor of `cursor`, read with it so the cursor advances without a
    /// second read of the same node.
    cursor_next: u32,
    /// Unallocated size of the claimant at `cursor`.
    demand: u64,
    /// Price of the claimant `demand` belongs to, held so a cover order the
    /// claimant cannot cross does not consume it. Read with the claimant, so
    /// re-testing costs no second read.
    demand_price: u64,
    /// Claimants left to read. Each is read at most once for the whole walk,
    /// so the side's own count bounds what a corrupt list can cost.
    reads_left: u16,
    /// Best price on the other side that could match this slot, resolved on first
    /// need and reused. The other side cannot change while a walk of `cover` is in
    /// flight. Resolving eagerly inlines a second side walk into `execute`'s
    /// prologue, and the frame spills cost more than the lookup.
    counterparty: Option<Option<u64>>,
}

impl CrossReservation {
    pub(crate) fn new(
        book: &ClobMarketV0,
        cover: SideV0,
        slot: u64,
        now: i64,
        include_reserved: bool,
    ) -> Self {
        let claiming = cover.opposite();
        Self {
            cover,
            slot,
            now,
            grace_slots: book.reservation_grace_slots as u64,
            min_order_size: book.min_order_size,
            include_reserved,
            cursor: if include_reserved {
                NIL
            } else {
                book.first_claimant(claiming)
            },
            cursor_next: NIL,
            demand: 0,
            demand_price: 0,
            reads_left: book.claimant_count(claiming),
            counterparty: None,
        }
    }

    /// Whether this order can be withheld at all. Either some claimant still holds
    /// unallocated demand, or the order is a remainder of its own. A book with no
    /// remainder answers in two compares and a bit test, which keeps the
    /// reservation off the cost of an ordinary quote.
    #[inline(always)]
    fn may_withhold(&self, node: &OrderNodeV0) -> bool {
        self.cursor != NIL || self.demand != 0 || node.is_taker_origin()
    }

    /// Units of `node` no ordinary caller may take: the units a crossing remainder
    /// claims, or the whole of a remainder a counterparty crosses. The second is
    /// the larger answer whenever it applies, so it wins.
    #[inline(always)]
    pub(crate) fn withheld(&mut self, book: &ClobMarketV0, node: &OrderNodeV0) -> Result<u64> {
        if !self.may_withhold(node) {
            return Ok(0);
        }

        self.withheld_uncached(book, node)
    }

    #[inline(never)]
    fn withheld_uncached(&mut self, book: &ClobMarketV0, node: &OrderNodeV0) -> Result<u64> {
        if self.include_reserved {
            return Ok(0);
        }

        let claimed = self.claimed(book, node)?;
        if claimed < node.base_asset_amount
            && node.is_taker_origin()
            && !self.lapsed(node)
            && self.crossed(book, node.price)?
        {
            return Ok(node.base_asset_amount);
        }

        Ok(claimed)
    }

    /// Units of `node` this caller may fill.
    #[inline(always)]
    pub(crate) fn available(&mut self, book: &ClobMarketV0, node: &OrderNodeV0) -> Result<u64> {
        if !self.may_withhold(node) {
            return Ok(node.base_asset_amount);
        }

        Ok(node
            .base_asset_amount
            .saturating_sub(self.withheld_uncached(book, node)?))
    }

    /// Allocate the claimants' demand over one cover order, and report the
    /// units of it they hold.
    ///
    /// One cursor serves a whole walk of the cover side, and two facts make
    /// that correct.
    ///
    /// Cover prices only get worse as the walk proceeds. A claimant that does
    /// not cross the current cover price crosses no later one either, so
    /// skipping it is permanent and each claimant is read at most once for the
    /// whole walk.
    ///
    /// Claimants are served in rest order and each takes the best cover still
    /// available, so allocation runs contiguously down the cover side. Only
    /// the current claimant's unallocated demand has to be held.
    ///
    /// Ask this for every order the walk reaches that anyone could match, and
    /// ask it before any test that depends on who is asking. The allocation is
    /// positional, so a caller's own exclusions must not move a claim onto a
    /// different order.
    pub(crate) fn claimed(&mut self, book: &ClobMarketV0, node: &OrderNodeV0) -> Result<u64> {
        if self.include_reserved || (self.cursor == NIL && self.demand == 0) {
            return Ok(0);
        }

        let base = node.base_asset_amount;
        // Cover prices only get worse down the side, so a claimant that no longer
        // crosses will not cross anything behind this either. Spend its
        // unallocated demand rather than carry it onto depth it cannot trade
        // against.
        if self.demand > 0 && !self.cover.is_crossed_by(node.price, self.demand_price) {
            self.demand = 0;
            self.cursor = self.cursor_next;
        }

        let mut honoured = 0u64;
        loop {
            if self.demand == 0 {
                if self.cursor == NIL {
                    break;
                }

                require!(self.reads_left > 0, ClobError::BookInvariantViolated);
                self.reads_left -= 1;
                let claimant = book.read_node(self.cursor)?;
                if !self.honours(&claimant, node.price) {
                    self.cursor = claimant.taker_origin_next;
                    continue;
                }

                self.demand = claimant.base_asset_amount;
                self.demand_price = claimant.price;
                self.cursor_next = claimant.taker_origin_next;
            }

            if honoured >= base {
                break;
            }

            let take = self.demand.min(base - honoured);
            honoured += take;
            self.demand -= take;
            if self.demand == 0 {
                self.cursor = self.cursor_next;
            }
        }

        if honoured == 0 {
            return Ok(0);
        }

        // A claim withholds at least `min_order_size`. A fill of everything
        // around a smaller claim leaves a remainder the cull rule removes, and
        // the cull would take the claim with it.
        Ok(honoured.max(self.min_order_size).min(base))
    }

    /// Whether this claimant still holds a claim on cover priced at
    /// `cover_price`.
    fn honours(&self, claimant: &OrderNodeV0, cover_price: u64) -> bool {
        !self.lapsed(claimant)
            && !claimant.is_expired(self.now)
            && self.cover.is_crossed_by(cover_price, claimant.price)
    }

    /// A claimant inside its delay is the ordinary case, and the claim is what
    /// holds its cover while it waits.
    fn lapsed(&self, claimant: &OrderNodeV0) -> bool {
        is_claim_lapsed(claimant, self.slot, self.grace_slots)
    }

    /// Whether a counterparty that could match this slot crosses `price`.
    #[inline(never)]
    fn crossed(&mut self, book: &ClobMarketV0, price: u64) -> Result<bool> {
        let counterparty = match self.counterparty {
            Some(cached) => cached,
            None => {
                let resolved =
                    best_actionable_price(book, self.cover.opposite(), self.slot, self.now)?;
                self.counterparty = Some(resolved);
                resolved
            }
        };

        Ok(counterparty.is_some_and(|opposite| self.cover.is_crossed_by(price, opposite)))
    }
}

/// Price of the best order on `side` that could be matched this slot at all.
///
/// Blind to the caller's user set and self-trade exclusion, which say whether this
/// caller may fill an order rather than whether the order is a live counterparty.
/// Unactivated and expired orders are skipped, because a cross involving one is
/// not actionable by anyone, and firing on one would freeze the book for a whole
/// auction window.
fn best_actionable_price(
    book: &ClobMarketV0,
    side: SideV0,
    slot: u64,
    now: i64,
) -> Result<Option<u64>> {
    let mut best = None;
    walk_side_ref(book, side, |_, node| {
        if !is_live(node, slot, now) {
            return Ok(Walk::Continue);
        }

        best = Some(node.price);
        Ok(Walk::Stop)
    })?;

    Ok(best)
}
