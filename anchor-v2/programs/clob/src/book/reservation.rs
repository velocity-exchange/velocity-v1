//! The claims a crossing taker remainder holds on the depth it crosses, and the
//! rules that bind such a remainder to the book while its claim holds. An
//! ordinary caller never takes a remainder itself, before or after its claim
//! lapses.

use {
    super::{is_live, BookHeader, ClobBook, NodeArena},
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
/// cancel it without `force`, and its `max_ts` cannot expire it. The bind ends
/// exactly when the claim lapses.
pub(super) fn is_bound(node: &OrderNodeV0, slot: u64, grace_slots: u16) -> bool {
    node.is_taker_origin() && !is_claim_lapsed(node, slot, grace_slots as u64)
}

/// Past its `max_ts` and no longer bound. A remainder that could expire inside
/// its claim window would escape its claim the way a cancel would.
pub(crate) fn is_past_expiry(node: &OrderNodeV0, slot: u64, now: i64, grace_slots: u16) -> bool {
    node.is_expired(now) && !is_bound(node, slot, grace_slots)
}

/// The worst-priced order on `side` that eviction may take, or [`NIL`] while
/// that tail order is shielded. Eviction never moves toward a better price:
/// doing so would let a bound remainder protect a worse quote while an honest
/// maker pays to lose a more competitive one.
pub(crate) fn evictable_order(
    book: &ClobMarketV0,
    side: SideV0,
    slot: u64,
    now: i64,
) -> Result<u32> {
    let worst = book.worst(side);
    if worst == NIL {
        return Ok(NIL);
    }

    let tail = book.read_node(worst)?;
    Ok(if is_shielded_from_eviction(book, &tail, slot, now)? {
        NIL
    } else {
        worst
    })
}

/// A bound remainder keeps its place only while a live order of another
/// authority crosses it. A remainder that crosses nothing claims nothing.
/// Binding it would let one order at the tail block eviction on a full side.
///
/// Only the opposite best is read. A best that is inside its delay, or that
/// the tail's authority holds on any sub-account, leaves the tail evictable.
/// Velocity's cross cranks never fill one authority against itself.
fn is_shielded_from_eviction(
    book: &ClobMarketV0,
    tail: &OrderNodeV0,
    slot: u64,
    now: i64,
) -> Result<bool> {
    let grace_slots = book.reservation_grace_slots;
    if !is_bound(tail, slot, grace_slots) {
        return Ok(false);
    }

    let best = book.best(tail.side().opposite());
    if best == NIL {
        return Ok(false);
    }

    let counterparty = book.read_node(best)?;
    Ok(is_live(&counterparty, slot, now, grace_slots)
        && counterparty.authority != tail.authority
        && tail.side().is_crossed_by(tail.price, counterparty.price))
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
}

impl CrossReservation {
    pub(crate) fn new(
        book: &ClobMarketV0,
        cover: SideV0,
        slot: u64,
        include_reserved: bool,
    ) -> Self {
        let claiming = cover.opposite();
        Self {
            cover,
            slot,
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
    /// claims, or the whole of a remainder. The second is the larger answer
    /// whenever it applies, so it wins.
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
        // Whole, whether or not a book order crosses it and after its claim
        // lapses. The vAMM or a quoter can cross it too, and the book cannot
        // see either. Only the crank that routes it reads with the flag.
        if node.is_taker_origin() {
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
    /// `cover_price`. A claimant whose claim has not lapsed is bound, so its
    /// `max_ts` cannot end the claim early.
    fn honours(&self, claimant: &OrderNodeV0, cover_price: u64) -> bool {
        !self.lapsed(claimant) && self.cover.is_crossed_by(cover_price, claimant.price)
    }

    /// A claimant inside its delay is the ordinary case, and the claim is what
    /// holds its cover while it waits.
    fn lapsed(&self, claimant: &OrderNodeV0) -> bool {
        is_claim_lapsed(claimant, self.slot, self.grace_slots)
    }
}
