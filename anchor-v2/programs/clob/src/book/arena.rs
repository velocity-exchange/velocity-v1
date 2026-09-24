//! Arena access and the free list. Every read or write of a slot, and every
//! link change, goes through this file. The book's O(1) postcondition,
//! `validate_book`, lives here too, because it checks the same links.

use {
    super::{both_or_neither, hints, ClobBook, NodeArena},
    crate::{
        error::ClobError,
        state::{
            ClobHeaderV0, ClobMarketV0, ClobOrderRefV0, MarketConfigV0, OrderBitFlag, OrderNodeV0,
            SideV0, BASE_PRECISION, NIL, ZERO_ADDRESS,
        },
    },
    anchor_lang::prelude::*,
};

impl NodeArena for ClobMarketV0 {
    fn read_node(&self, index: u32) -> Result<OrderNodeV0> {
        Ok(*self
            .get(index as usize)
            .ok_or(ClobError::NodeIndexOutOfRange)?)
    }

    fn write_node(&mut self, index: u32, node: OrderNodeV0) -> Result<()> {
        *self
            .get_mut(index as usize)
            .ok_or(ClobError::NodeIndexOutOfRange)? = node;
        Ok(())
    }

    fn update_node(&mut self, index: u32, edit: impl FnOnce(&mut OrderNodeV0)) -> Result<()> {
        let node = self
            .get_mut(index as usize)
            .ok_or(ClobError::NodeIndexOutOfRange)?;
        edit(node);
        Ok(())
    }

    fn set_next(&mut self, index: u32, next: u32) -> Result<()> {
        if index == NIL {
            return Ok(());
        }

        self.update_node(index, |node| node.next = next)
    }

    fn set_prev(&mut self, index: u32, prev: u32) -> Result<()> {
        if index == NIL {
            return Ok(());
        }

        self.update_node(index, |node| node.prev = prev)
    }
}

/// Header fields the book mutates. Split from [`ClobBook`] so the operation
/// surface the instruction handlers see stays free of internal setters.
pub(crate) trait BookHeader {
    fn set_best(&mut self, side: SideV0, index: u32);
    fn set_worst(&mut self, side: SideV0, index: u32);
    fn set_node_count(&mut self, side: SideV0, count: u32) -> Result<()>;
    /// Hand out the next order id and advance the counter. The only place
    /// `next_order_id` is read or written, so ids can never be reused.
    fn consume_order_id(&mut self) -> Result<u64>;
    fn fold_wake_hints(
        &mut self,
        max_ts: i64,
        activation_slot: u64,
        placed_slot: u64,
    ) -> Result<()>;
    fn publish_wakes(&mut self) -> Result<()>;
    fn repair_expiry_hint_for(&mut self, removed: &OrderNodeV0) -> Result<()>;
    fn expire_activation_hint(&mut self, slot: u64) -> Result<()>;
    fn recompute_wake_hints(&mut self, expiry: bool, activation: Option<u64>) -> Result<()>;
    /// Oldest taker-origin order resting on `side`, or [`NIL`] when the side
    /// holds none.
    fn first_claimant(&self, side: SideV0) -> u32;
    /// Newest taker-origin order resting on `side`, or [`NIL`].
    fn last_claimant(&self, side: SideV0) -> u32;
    /// Taker-origin orders resting on `side`.
    fn claimant_count(&self, side: SideV0) -> u16;
    fn link_claimant(&mut self, side: SideV0, index: u32) -> Result<()>;
    fn unlink_claimant(&mut self, side: SideV0, index: u32, node: &OrderNodeV0) -> Result<()>;
}

impl BookHeader for ClobMarketV0 {
    fn fold_wake_hints(
        &mut self,
        max_ts: i64,
        activation_slot: u64,
        placed_slot: u64,
    ) -> Result<()> {
        hints::fold_wake_hints(self, max_ts, activation_slot, placed_slot)
    }

    fn publish_wakes(&mut self) -> Result<()> {
        hints::publish_wakes(self)
    }

    fn repair_expiry_hint_for(&mut self, removed: &OrderNodeV0) -> Result<()> {
        hints::repair_expiry_hint_for(self, removed)
    }

    fn expire_activation_hint(&mut self, slot: u64) -> Result<()> {
        hints::expire_activation_hint(self, slot)
    }

    fn recompute_wake_hints(&mut self, expiry: bool, activation: Option<u64>) -> Result<()> {
        hints::recompute_wake_hints(self, expiry, activation)
    }

    fn set_best(&mut self, side: SideV0, index: u32) {
        match side {
            SideV0::Bid => self.best_bid = index,
            SideV0::Ask => self.best_ask = index,
        }
    }

    fn set_worst(&mut self, side: SideV0, index: u32) {
        match side {
            SideV0::Bid => self.worst_bid = index,
            SideV0::Ask => self.worst_ask = index,
        }
    }

    fn set_node_count(&mut self, side: SideV0, count: u32) -> Result<()> {
        require!(
            count <= self.capacity() as u32,
            ClobError::BookInvariantViolated
        );

        match side {
            SideV0::Bid => self.bid_count = count,
            SideV0::Ask => self.ask_count = count,
        }

        Ok(())
    }

    fn consume_order_id(&mut self) -> Result<u64> {
        let order_id = self.next_order_id;
        self.next_order_id = order_id.checked_add(1).ok_or(ClobError::MathError)?;
        Ok(order_id)
    }

    fn first_claimant(&self, side: SideV0) -> u32 {
        self.taker_origin_head[side.tag() as usize]
    }

    fn last_claimant(&self, side: SideV0) -> u32 {
        self.taker_origin_tail[side.tag() as usize]
    }

    fn claimant_count(&self, side: SideV0) -> u16 {
        self.taker_origin_count[side.tag() as usize]
    }

    /// Appends to the tail, which keeps the list in rest order because
    /// `next_order_id` only increases. [`super::CrossReservation`] serves claimants in
    /// that order, so the oldest remainder is paid first.
    fn link_claimant(&mut self, side: SideV0, index: u32) -> Result<()> {
        let list = side.tag() as usize;
        let tail = self.taker_origin_tail[list];
        self.update_node(index, |node| {
            node.taker_origin_prev = tail;
            node.taker_origin_next = NIL;
        })?;

        if tail == NIL {
            self.taker_origin_head[list] = index;
        } else {
            self.update_node(tail, |node| node.taker_origin_next = index)?;
        }

        self.taker_origin_tail[list] = index;
        self.taker_origin_count[list] = self.taker_origin_count[list]
            .checked_add(1)
            .ok_or(ClobError::BookInvariantViolated)?;
        Ok(())
    }

    /// Take a taker-origin order off its side's claimant list. `node` is the
    /// order as it rested, because the slot it sat in is about to be freed.
    ///
    /// Only [`unlink_order`] calls this, and every removal path goes through
    /// that function. A cancel, an eviction, an expiry reclaim, a cull and a
    /// consumed order therefore all maintain the list without knowing it
    /// exists.
    fn unlink_claimant(&mut self, side: SideV0, index: u32, node: &OrderNodeV0) -> Result<()> {
        let list = side.tag() as usize;
        let (prev, next) = (node.taker_origin_prev, node.taker_origin_next);
        if prev == NIL {
            require!(
                self.taker_origin_head[list] == index,
                ClobError::BookInvariantViolated
            );

            self.taker_origin_head[list] = next;
        } else {
            self.update_node(prev, |node| node.taker_origin_next = next)?;
        }

        if next == NIL {
            require!(
                self.taker_origin_tail[list] == index,
                ClobError::BookInvariantViolated
            );

            self.taker_origin_tail[list] = prev;
        } else {
            self.update_node(next, |node| node.taker_origin_prev = prev)?;
        }

        self.taker_origin_count[list] = self.taker_origin_count[list]
            .checked_sub(1)
            .ok_or(ClobError::BookInvariantViolated)?;
        Ok(())
    }
}

/// A side owns half the arena, and the threshold is the count at which the crank
/// may take that side's tail. Zero lets the crank take a maker's only order. A
/// threshold at or above the per-side capacity unlocks eviction only once
/// placements are already refused.
pub(crate) fn validate_evict_threshold(threshold: u32, capacity: u32) -> Result<()> {
    let per_side = capacity / 2;
    require!(
        threshold != 0 && threshold < per_side,
        ClobError::InvalidConfig
    );

    Ok(())
}

/// Write a fresh header, then fill the tail to capacity and thread the
/// free list.
pub(super) fn initialize(
    book: &mut ClobMarketV0,
    new_authority: Address,
    new_place_authority: Address,
    config: MarketConfigV0,
) -> Result<()> {
    let cap = book.capacity() as u32;
    require!(cap >= 2, ClobError::InvalidCapacity);
    // The caps budget and velocity's exact-notional check both divide by
    // `quoter_spec::BASE_PRECISION`, so every fill on another denominator
    // fails. The field stays in the header because resting sizes are
    // denominated in it and readers of the account expect to find it.
    require!(
        config.base_precision == BASE_PRECISION,
        ClobError::InvalidConfig
    );

    write_fresh_header(book, new_authority, new_place_authority, &config)?;
    // Lay out the arena as one free list, slot 0 first. `try_push` appends
    // within the tail the slab owns and fails rather than writing past it.
    (0..cap).try_for_each(|i| -> Result<()> {
        let mut node: OrderNodeV0 = bytemuck::Zeroable::zeroed();
        node.next = if i + 1 == cap { NIL } else { i + 1 };
        book.try_push(node)
            .map_err(|_| ClobError::InvalidCapacity)?;
        Ok(())
    })?;

    crate::config::validate_market_config(book)?;
    book.validate_book()
}

/// Destructure every header field, which is the zero-copy form of
/// `set_inner`. A header field added without an initializer here is a compile
/// error. The free list it describes is laid out by `initialize`. The body runs
/// past 80 lines because it names every header field twice.
fn write_fresh_header(
    book: &mut ClobMarketV0,
    new_authority: Address,
    new_place_authority: Address,
    config: &MarketConfigV0,
) -> Result<()> {
    let cap = book.capacity() as u32;
    let ClobHeaderV0 {
        authority,
        place_authority,
        order_tick_size,
        order_step_size,
        min_order_size,
        blocking_min_size,
        base_precision,
        next_order_id,
        best_bid,
        best_ask,
        worst_bid,
        worst_ask,
        free_head,
        free_count,
        bid_count,
        ask_count,
        default_activation_delay_slots,
        max_activation_delay_slots,
        unknown_user_grace_slots,
        evict_threshold_per_side,
        market_index,
        max_quote_levels,
        max_execute_fills,
        max_execute_users,
        next_expiry_ts,
        next_activation_slot,
        pending_authority,
        padding,
        padding1,
        taker_origin_head,
        taker_origin_tail,
        taker_origin_count,
        reservation_grace_slots,
        response,
        crank,
    } = &mut **book;

    *authority = new_authority;
    *place_authority = new_place_authority;
    *pending_authority = ZERO_ADDRESS;
    *order_tick_size = config.order_tick_size;
    *order_step_size = config.order_step_size;
    *min_order_size = config.min_order_size;
    *blocking_min_size = config.blocking_min_size;
    *base_precision = config.base_precision;
    *next_order_id = 1;
    *best_bid = NIL;
    *best_ask = NIL;
    *worst_bid = NIL;
    *worst_ask = NIL;
    *bid_count = 0;
    *ask_count = 0;
    *default_activation_delay_slots = config.default_activation_delay_slots;
    *max_activation_delay_slots = config.max_activation_delay_slots;
    *unknown_user_grace_slots = config.unknown_user_grace_slots;
    *evict_threshold_per_side = config.evict_threshold_per_side;
    *market_index = config.market_index;
    *max_quote_levels = config.max_quote_levels;
    *max_execute_fills = config.max_execute_fills;
    *max_execute_users = config.max_execute_users;
    // An empty book has no expiry and no pending activation.
    *next_expiry_ts = i64::MAX;
    *next_activation_slot = u64::MAX;
    // No taker remainder rests yet, so neither side has a claimant.
    *taker_origin_head = [NIL; 2];
    *taker_origin_tail = [NIL; 2];
    *taker_origin_count = [0; 2];
    *reservation_grace_slots = crate::state::DEFAULT_RESERVATION_GRACE_SLOTS;
    padding.fill(0);
    padding1.fill(0);
    response.fill(0);
    // A block with no conditions written is inactive, which is the correct
    // state for a market that nobody has registered cranks for. Stamping it
    // here leaves `set_crank_conditions_v0` writing conditions only.
    crank
        .init(crate::state::CRANK_BLOCK_OFFSET as u32)
        .map_err(|_| ClobError::InvalidConfig)?;
    *free_head = 0;
    *free_count = cap;
    Ok(())
}

/// Push zeroed nodes for the new slots after a capacity grow, and thread
/// them into the free list.
pub(super) fn grow_free_list(book: &mut ClobMarketV0) -> Result<()> {
    while !book.is_full() {
        let index = book.len() as u32;
        let mut node: OrderNodeV0 = bytemuck::Zeroable::zeroed();
        node.next = book.free_head;
        book.try_push(node)
            .map_err(|_| ClobError::InvalidCapacity)?;
        book.free_head = index;
        book.free_count = book
            .free_count
            .checked_add(1)
            .ok_or(ClobError::InvalidCapacity)?;
    }

    book.validate_book()
}

/// The O(1) postcondition for every mutating operation. The three counts
/// account for the whole arena, the free head agrees with the free count,
/// and each side's endpoints and claimant list endpoints are sound.
pub(super) fn validate_book(book: &ClobMarketV0) -> Result<()> {
    let total = book
        .bid_count
        .checked_add(book.ask_count)
        .and_then(|live| live.checked_add(book.free_count))
        .ok_or(ClobError::BookInvariantViolated)?;
    require!(
        total == book.capacity() as u32,
        ClobError::BookInvariantViolated
    );
    require!(
        both_or_neither(book.free_count == 0, book.free_head == NIL),
        ClobError::BookInvariantViolated
    );

    if book.free_head != NIL {
        require!(
            !book
                .read_node(book.free_head)?
                .is_bit_flag_set(OrderBitFlag::Open),
            ClobError::BookInvariantViolated
        );
    }

    [SideV0::Bid, SideV0::Ask]
        .into_iter()
        .try_for_each(|side| validate_side_endpoints(book, side))?;
    [SideV0::Bid, SideV0::Ask]
        .into_iter()
        .try_for_each(|side| validate_claimant_endpoints(book, side))
}

/// A side's endpoints are live nodes of that side with null outer links, and
/// agree with its count.
fn validate_side_endpoints(book: &ClobMarketV0, side: SideV0) -> Result<()> {
    let count = book.node_count(side);
    let (best, worst) = (book.best(side), book.worst(side));
    require!(
        both_or_neither(count == 0, best == NIL) && both_or_neither(count == 0, worst == NIL),
        ClobError::BookInvariantViolated
    );

    if count == 0 {
        return Ok(());
    }

    require!(
        both_or_neither(count == 1, best == worst),
        ClobError::BookInvariantViolated
    );

    let head = book.read_node(best)?;
    let tail = book.read_node(worst)?;
    require!(
        head.prev == NIL && tail.next == NIL,
        ClobError::BookInvariantViolated
    );
    require!(
        head.is_bit_flag_set(OrderBitFlag::Open) && head.side() == side,
        ClobError::BookInvariantViolated
    );
    require!(
        tail.is_bit_flag_set(OrderBitFlag::Open) && tail.side() == side,
        ClobError::BookInvariantViolated
    );

    Ok(())
}

/// The claimant list gets the same depth of check as its side. Each endpoint
/// is a live taker-origin order of that side with a null outer link. The list
/// is a subset of the side, so its count cannot exceed the side's.
///
/// The exhaustive version walks both lists and runs in the unit tests. It
/// checks that every taker-origin order on the side is listed, that ids
/// ascend, and that links are mutual.
fn validate_claimant_endpoints(book: &ClobMarketV0, side: SideV0) -> Result<()> {
    let count = book.claimant_count(side);
    let (head, tail) = (book.first_claimant(side), book.last_claimant(side));
    require!(
        both_or_neither(count == 0, head == NIL) && both_or_neither(count == 0, tail == NIL),
        ClobError::BookInvariantViolated
    );
    require!(
        count as u32 <= book.node_count(side),
        ClobError::BookInvariantViolated
    );

    if count == 0 {
        return Ok(());
    }

    require!(
        both_or_neither(count == 1, head == tail),
        ClobError::BookInvariantViolated
    );

    let first = book.read_node(head)?;
    let last = book.read_node(tail)?;
    require!(
        first.taker_origin_prev == NIL && last.taker_origin_next == NIL,
        ClobError::BookInvariantViolated
    );
    require!(
        first.is_open() && first.is_taker_origin() && first.side() == side,
        ClobError::BookInvariantViolated
    );
    require!(
        last.is_open() && last.is_taker_origin() && last.side() == side,
        ClobError::BookInvariantViolated
    );

    Ok(())
}

/// Resolve an order hint to its live node, failing closed when the node is out
/// of range, free, or reused for a different order. An out-of-range hint
/// reports as stale rather than as arena corruption. The hint comes from the
/// caller, and a node index that was valid before a shrink is a stale handle.
pub(super) fn live_order(book: &ClobMarketV0, order_ref: ClobOrderRefV0) -> Result<OrderNodeV0> {
    let node = book
        .read_node(order_ref.node_index)
        .map_err(|_| ClobError::StaleOrderRef)?;
    require!(
        node.is_bit_flag_set(OrderBitFlag::Open) && node.order_id == order_ref.order_id,
        ClobError::StaleOrderRef
    );

    Ok(node)
}

/// Postcondition for an operation that removed exactly one order. The list
/// closed over the gap, or the side's endpoint moved when the removed order was
/// an endpoint. The slot is zeroed at the head of the free list, so its handle
/// can never verify again. Execute removes up to `max_execute_fills` nodes in
/// one call, so it uses the O(1) [`ClobBook::validate_book`] instead of paying
/// this per removal.
pub(super) fn validate_single_removal(
    book: &ClobMarketV0,
    removed: &OrderNodeV0,
    index: u32,
) -> Result<()> {
    let side = removed.side();
    let neighbour_next = if removed.prev == NIL {
        book.best(side)
    } else {
        book.read_node(removed.prev)?.next
    };

    require!(
        neighbour_next == removed.next,
        ClobError::BookInvariantViolated
    );

    let neighbour_prev = if removed.next == NIL {
        book.worst(side)
    } else {
        book.read_node(removed.next)?.prev
    };

    require!(
        neighbour_prev == removed.prev,
        ClobError::BookInvariantViolated
    );

    let freed = book.read_node(index)?;
    require!(
        freed.bit_flags == 0 && freed.order_id == 0 && book.free_head == index,
        ClobError::BookInvariantViolated
    );

    Ok(())
}

/// Take a node off the free list. `place` refuses at the per-side cap, which
/// leaves free arena, so this should never be the binding check. It is here so
/// that an exhausted or corrupt free list is a clean error instead of a write
/// through a stale index.
pub(super) fn alloc_node(book: &mut ClobMarketV0) -> Result<u32> {
    require!(book.free_count > 0, ClobError::ArenaExhausted);
    let index = book.free_head;
    require!(index != NIL, ClobError::ArenaExhausted);
    let node = book.read_node(index)?;
    require!(
        !node.is_bit_flag_set(OrderBitFlag::Open),
        ClobError::BookInvariantViolated
    );

    book.free_head = node.next;
    book.free_count -= 1;
    Ok(index)
}

/// Splice an already-written node between `prev` and `next` on `side`,
/// updating the side's endpoints when it lands at either end. One of the two
/// places link fields are written (the other is [`remove_order`]).
///
/// A taker-origin order joins its side's claimant list here too. The
/// [`OrderBitFlag::TakerOrigin`] bit never changes on a live order, so
/// membership is fixed for the node's lifetime and the two lists are maintained
/// in the same two functions. `taker_origin` is the flag the caller wrote onto
/// the node. The caller passes it rather than reading it back, because this is
/// the placement path and a node is 104 bytes to copy.
pub(super) fn insert_order(
    book: &mut ClobMarketV0,
    side: SideV0,
    index: u32,
    prev: u32,
    next: u32,
    taker_origin: bool,
) -> Result<()> {
    if prev == NIL {
        book.set_best(side, index);
    } else {
        book.set_next(prev, index)?;
    }

    if next == NIL {
        book.set_worst(side, index);
    } else {
        book.set_prev(next, index)?;
    }

    if taker_origin {
        book.link_claimant(side, index)?;
    }

    Ok(())
}

/// Unlink one node from its side, return it to the free list, and repair the
/// expiry hint if that order held it. The bulk paths use [`unlink_order`] and
/// repair once at the end, because the repair walks the live orders and one per
/// removal is quadratic in a call that frees a hundred.
pub(super) fn remove_order(book: &mut ClobMarketV0, index: u32) -> Result<()> {
    let node = unlink_order(book, index)?;
    // The order that just left may have been the one holding the expiry hint.
    // A walk is owed only then. An ordinary removal costs one comparison.
    book.repair_expiry_hint_for(&node)?;
    Ok(())
}

/// Unlink a live order and push the node onto the free list, zeroed so its old
/// order id can never verify again. Returns the node it freed. This is
/// [`remove_order`] without the hint repair, and every removal path goes
/// through it: cancel, evict, expiry reclaim and execute.
///
/// It refuses up front to remove a node that is not live, or whose side count
/// is already zero. A double free would otherwise desynchronize the counts.
/// [`ClobBook::validate_book`] checks the structural postcondition once per
/// operation rather than once per removal. That matters because execute removes
/// up to `max_execute_fills` nodes in one call. Its free-head check lands on
/// this node, because the removal makes it the head, so that check covers the
/// claim that the slot really was freed.
pub(super) fn unlink_order(book: &mut ClobMarketV0, index: u32) -> Result<OrderNodeV0> {
    let node = book.read_node(index)?;
    require!(
        node.is_bit_flag_set(OrderBitFlag::Open),
        ClobError::BookInvariantViolated
    );

    let side = node.side();
    let count = book.node_count(side);
    require!(count > 0, ClobError::BookInvariantViolated);

    if node.is_taker_origin() {
        book.unlink_claimant(side, index, &node)?;
    }

    if node.prev == NIL {
        book.set_best(side, node.next);
    } else {
        book.set_next(node.prev, node.next)?;
    }

    if node.next == NIL {
        book.set_worst(side, node.prev);
    } else {
        book.set_prev(node.next, node.prev)?;
    }

    book.set_node_count(side, count - 1)?;

    let mut freed: OrderNodeV0 = bytemuck::Zeroable::zeroed();
    freed.next = book.free_head;
    book.write_node(index, freed)?;
    book.free_head = index;
    book.free_count = book
        .free_count
        .checked_add(1)
        .ok_or(ClobError::BookInvariantViolated)?;
    Ok(node)
}
