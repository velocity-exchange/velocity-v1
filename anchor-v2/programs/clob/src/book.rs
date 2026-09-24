//! The order book. It is a node arena with a free list, plus two best-first
//! sorted intrusive doubly-linked lists, one per side, over the market slab
//! defined in [`crate::state`]. Two more intrusive lists, one per side, thread
//! that side's taker-origin orders in rest order. [`CrossReservation`] is the
//! one reader of those two lists.
//!
//! ## Arena access is centralized
//!
//! Every read or write of an arena slot goes through [`NodeArena`]. The
//! methods are [`NodeArena::read_node`], [`NodeArena::write_node`],
//! [`NodeArena::update_node`], and the two sentinel-tolerant link setters.
//! Each validates the index against the live arena before it touches memory,
//! so a corrupt or hostile link produces [`ClobError::NodeIndexOutOfRange`]
//! instead of a read or a write outside the arena. No code in the crate
//! indexes the slab directly. Link changes are confined to [`insert_order`]
//! and [`remove_order`], the claimant lists included, which is how every
//! removal path maintains those lists without knowing they exist. The one
//! other way a slot is written is `Slab::try_push`. It appends within the tail
//! it owns, and it lays out a fresh or a freshly grown arena.
//!
//! ## Traversal
//!
//! Every walk over a side goes through [`walk_side`], or through
//! [`walk_side_ref`] where the caller holds only `&`. Both validate each hop
//! and refuse to walk further than the arena can hold, so a corrupt list
//! cannot spin forever.
//!
//! ## Invariants
//!
//! Every mutating operation ends by re-checking what it wrote.
//! [`ClobBook::validate_book`] covers the O(1) header and endpoint invariants.
//! Counts sum to capacity, endpoints are live nodes of the right side with
//! null outer links, the free head agrees with the free count, and each
//! claimant list's endpoints are live taker-origin orders of that side. Each
//! operation adds its own postcondition. The exhaustive O(n) version runs in
//! the unit tests after every operation rather than on-chain. It walks every
//! list, checks price ordering, checks that claimant ids ascend, and accounts
//! for every slot.
//!
//! Quote and execute also check what they are about to report. Every level or
//! fill carries a nonzero price and size, and the sequence runs
//! best-price-first for the side. A response that broke either rule would be
//! worth more to the router than the book can honour, because a zero-priced
//! level wins any routing waterfall outright. Such a response fails the
//! instruction instead of shipping. See [`write_level`] and
//! [`check_fill_price`].
//!
//! Neither instruction trades some of the depth. That depth is the units a
//! crossing taker remainder has claimed, and the whole of a remainder that a
//! counterparty crosses. See [`CrossReservation`], which every read of a side
//! asks, so the depth quote publishes is always depth execute can deliver.
//! Both pass over claimed units the way they pass over an expired or a
//! not-yet-activated order, so the rest of the side stays tradeable. Every
//! order still fills at its own stored price. The book neither reprices a
//! cross nor resolves one. It only declines to sell the taker's improvement to
//! whoever gets there first, and it reports the flag on
//! [`crate::state::RemovedOrderV0`] so velocity can settle the cross at the
//! counterparty's price.
//!
//! Velocity also holds execute to its own quote on the way out. The response's
//! total quote must be the notional of these same orders at the prices `quote`
//! published, to within the one unavoidable division. Fills are therefore
//! priced by differencing a running total rather than rounded one at a time.
//! See `quote_size` in [`ClobBook::execute`].

use {
    crate::{
        error::ClobError,
        events::FillSlimV0,
        state::{
            response_pointer, user_set_within_capacity, CancelAllOutcome, CancelSidesExt,
            CancelSidesV0, CancelledRemainderV0, ClobDirectionExt, ClobHeaderV0, ClobMarketV0,
            ClobSideExt, CompletedOrderV0, Direction, ExecuteOutcome, FilledOrder, L3RowV0,
            MarketConfigV0, OrderBitFlag, OrderNodeV0, OrderRefV0, PartiallyFilledOrderV0,
            PlaceOrderParams, PriceLevel, RemovedOrder, ResponsePointerV0, Side,
            UserBalanceChangeV0, UserCapsV0, UserRefV0, BASE_PRECISION, CANCEL_ALL_ORDERS_CEILING,
            EXECUTE_FILLS_CEILING, EXECUTE_USERS_CEILING, L3_ROWS_CEILING, NIL,
            QUOTE_LEVELS_CEILING, USER_CAPS_CAPACITY, USER_EXCLUSION_BITMAP_BYTES,
            USER_SET_CAPACITY, ZERO_ADDRESS,
        },
    },
    anchor_lang::{address_eq, prelude::*},
    quoter_spec::{ExecuteWriter, L3Writer, QuoteWriter},
    relay_spec::ConditionBlock,
};

/// Two conditions that must agree, such as a `NIL` best link and a zero count.
/// Written inline the test reads `(a == b) == (c == d)`, which looks like a typo
/// for `&&`.
#[inline(always)]
const fn both_or_neither(a: bool, b: bool) -> bool {
    a == b
}

/// Book operations over the market slab. This is a trait because Rust does not
/// allow an inherent impl on the foreign `Slab` type.
pub trait ClobBook {
    fn initialize(
        &mut self,
        new_authority: Address,
        new_place_authority: Address,
        config: MarketConfigV0,
    ) -> Result<()>;
    fn place(&mut self, params: PlaceOrderParams) -> Result<OrderRefV0>;
    fn cancel(
        &mut self,
        user: UserRefV0,
        order_ref: OrderRefV0,
        slot: u64,
        force: bool,
    ) -> Result<RemovedOrder>;
    fn cancel_all(
        &mut self,
        user: UserRefV0,
        sides: CancelSidesV0,
        slot: u64,
        force: bool,
        removed_ids: &mut dyn FnMut(u32) -> Result<()>,
    ) -> Result<CancelAllOutcome>;
    fn evict_worst(&mut self, side: Side, slot: u64) -> Result<RemovedOrder>;
    fn remove_expired(&mut self, order_ref: OrderRefV0, now: i64) -> Result<RemovedOrder>;
    fn fill(
        &mut self,
        order_ref: OrderRefV0,
        base_asset_amount: u64,
        slot: u64,
        now: i64,
    ) -> Result<FilledOrder>;
    #[allow(clippy::too_many_arguments)]
    fn quote(
        &mut self,
        direction: Direction,
        size: u64,
        users: &[UserRefV0],
        caps: &UserCapsV0,
        reference_price: i64,
        taker: Option<&UserRefV0>,
        limit_price: u64,
        include_taker_origin_reservations: bool,
        slot: u64,
        now: i64,
    ) -> Result<ResponsePointerV0>;
    fn quote_l3(
        &mut self,
        direction: Direction,
        size: u64,
        max_rows: u16,
        include_taker_origin_reservations: bool,
        slot: u64,
        now: i64,
    ) -> Result<ResponsePointerV0>;
    #[allow(clippy::too_many_arguments)]
    fn execute(
        &mut self,
        direction: Direction,
        size: u64,
        users: &[UserRefV0],
        caps: &UserCapsV0,
        reference_price: i64,
        taker: Option<&UserRefV0>,
        include_taker_origin_reservations: bool,
        slot: u64,
        now: i64,
    ) -> Result<ExecuteOutcome>;
    fn grow_free_list(&mut self) -> Result<()>;

    /// Live order count on `side`.
    fn node_count(&self, side: Side) -> u32;
    /// Head of `side`: the best-priced, oldest order. [`NIL`] when empty.
    fn best(&self, side: Side) -> u32;
    /// Tail of `side`: the worst-priced, youngest order there. [`NIL`] when
    /// empty. Eviction starts its search here.
    fn worst(&self, side: Side) -> u32;
    /// O(1) invariants, re-checked after every mutating operation.
    fn validate_book(&self) -> Result<()>;
}

/// Header fields the book mutates. Split from [`ClobBook`] so the operation
/// surface the instruction handlers see stays free of internal setters.
pub(crate) trait BookHeader {
    fn set_best(&mut self, side: Side, index: u32);
    fn set_worst(&mut self, side: Side, index: u32);
    fn set_node_count(&mut self, side: Side, count: u32) -> Result<()>;
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
    fn first_claimant(&self, side: Side) -> u32;
    /// Newest taker-origin order resting on `side`, or [`NIL`].
    fn last_claimant(&self, side: Side) -> u32;
    /// Taker-origin orders resting on `side`.
    fn claimant_count(&self, side: Side) -> u16;
    fn link_claimant(&mut self, side: Side, index: u32) -> Result<()>;
    fn unlink_claimant(&mut self, side: Side, index: u32, node: &OrderNodeV0) -> Result<()>;
}

impl BookHeader for ClobMarketV0 {
    /// Moves a hint earlier only. An early hint costs one wasted simulation. A
    /// late hint is work nobody is woken for.
    ///
    /// An activation at or behind its placement slot is not pending, so a
    /// zero-delay order does not peg the hint to the past.
    fn fold_wake_hints(
        &mut self,
        max_ts: i64,
        activation_slot: u64,
        placed_slot: u64,
    ) -> Result<()> {
        if max_ts != 0 && max_ts < self.next_expiry_ts {
            self.next_expiry_ts = max_ts;
        }

        if activation_slot > placed_slot && activation_slot < self.next_activation_slot {
            self.next_activation_slot = activation_slot;
        }

        self.publish_wakes()
    }

    /// Copy the two hints into the conditions that watch for them.
    ///
    /// The block holds the turner-facing copy of the same two facts, so every
    /// write to either hint ends here. A market that nobody has registered
    /// cranks for has an inactive block. No turner reads a wake written into an
    /// inactive slot, so this needs no guard.
    fn publish_wakes(&mut self) -> Result<()> {
        let (unix_ts, slot) = (self.next_expiry_ts, self.next_activation_slot);
        let mut write = |index: usize, wake: relay_spec::WakeView| {
            self.crank
                .update_condition(index, |condition| condition.set_wake(wake))
                .map_err(|_| ClobError::InvalidConfig)
        };

        write(
            crate::state::CRANK_EXPIRY,
            relay_spec::WakeView::AtTimestamp { unix_ts },
        )?;
        write(
            crate::state::CRANK_ACTIVATION,
            relay_spec::WakeView::AtSlot { slot },
        )?;

        Ok(())
    }

    /// Repairs the expiry hint only. A removal takes no clock, and the
    /// activation hint needs the current slot to repair. An early activation
    /// hint is safe, and [`Self::expire_activation_hint`] moves it on at the
    /// next write that knows the slot.
    fn repair_expiry_hint_for(&mut self, removed: &OrderNodeV0) -> Result<()> {
        if removed.max_ts != 0 && removed.max_ts <= self.next_expiry_ts {
            self.recompute_wake_hints(true, None)?;
        }

        Ok(())
    }

    /// Moves the activation hint past a slot the chain has reached. The expiry
    /// hint needs no equivalent, because a timestamp stays true as time moves. A
    /// pending activation arrives with nothing writing to the book, so a stored
    /// slot behind the current one would stay due for good.
    fn expire_activation_hint(&mut self, slot: u64) -> Result<()> {
        if self.next_activation_slot != u64::MAX && self.next_activation_slot <= slot {
            self.recompute_wake_hints(false, Some(slot))?;
        }

        Ok(())
    }

    /// One walk of the arena for whichever hints were asked for. `activation`
    /// carries the slot a pending activation has to be ahead of.
    ///
    /// The walk lives here rather than in a caller because the arena is this
    /// program's own state. A reader outside the program would have to know
    /// where a node keeps its expiry, and that is the coupling these hints
    /// exist to remove.
    fn recompute_wake_hints(&mut self, expiry: bool, activation: Option<u64>) -> Result<()> {
        let (mut min_ts, mut min_slot) = (i64::MAX, u64::MAX);
        // Walk the side lists, not the arena. The cost is `bid_count +
        // ask_count` hops instead of the arena capacity. Every removal in
        // `cancel_all` and in a deep `execute` can land here, and a full-arena
        // walk per removal put both over the compute budget on a large market.
        for side in [Side::Bid, Side::Ask] {
            walk_side_ref(self, side, |_, node| {
                if node.max_ts != 0 && node.max_ts < min_ts {
                    min_ts = node.max_ts;
                }

                if activation.is_some_and(|slot| node.activation_slot > slot)
                    && node.activation_slot < min_slot
                {
                    min_slot = node.activation_slot;
                }

                Ok(Walk::Continue)
            })?;
        }

        if expiry {
            self.next_expiry_ts = min_ts;
        }

        if activation.is_some() {
            self.next_activation_slot = min_slot;
        }

        self.publish_wakes()
    }

    fn set_best(&mut self, side: Side, index: u32) {
        match side {
            Side::Bid => self.best_bid = index,
            Side::Ask => self.best_ask = index,
        }
    }

    fn set_worst(&mut self, side: Side, index: u32) {
        match side {
            Side::Bid => self.worst_bid = index,
            Side::Ask => self.worst_ask = index,
        }
    }

    fn set_node_count(&mut self, side: Side, count: u32) -> Result<()> {
        require!(
            count <= self.capacity() as u32,
            ClobError::BookInvariantViolated
        );

        match side {
            Side::Bid => self.bid_count = count,
            Side::Ask => self.ask_count = count,
        }

        Ok(())
    }

    fn consume_order_id(&mut self) -> Result<u64> {
        let order_id = self.next_order_id;
        self.next_order_id = order_id.checked_add(1).ok_or(ClobError::MathError)?;
        Ok(order_id)
    }

    fn first_claimant(&self, side: Side) -> u32 {
        self.taker_origin_head[side.tag() as usize]
    }

    fn last_claimant(&self, side: Side) -> u32 {
        self.taker_origin_tail[side.tag() as usize]
    }

    fn claimant_count(&self, side: Side) -> u16 {
        self.taker_origin_count[side.tag() as usize]
    }

    /// Appends to the tail, which keeps the list in rest order because
    /// `next_order_id` only increases. [`CrossReservation`] serves claimants in
    /// that order, so the oldest remainder is paid first.
    fn link_claimant(&mut self, side: Side, index: u32) -> Result<()> {
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
    fn unlink_claimant(&mut self, side: Side, index: u32, node: &OrderNodeV0) -> Result<()> {
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

/// The whole of the program's arena access. Each method validates the index
/// against the live arena length, so no caller can address a slot that is not
/// there. These five methods are the only way to reach the arena.
pub(crate) trait NodeArena {
    /// Copy a node out. The copy, rather than a borrow, lets a traversal
    /// visitor keep mutating the book while it holds the node.
    fn read_node(&self, index: u32) -> Result<OrderNodeV0>;
    fn write_node(&mut self, index: u32, node: OrderNodeV0) -> Result<()>;
    fn update_node(&mut self, index: u32, edit: impl FnOnce(&mut OrderNodeV0)) -> Result<()>;
    /// Point `index`'s successor link at `next`. [`NIL`] for `index` means no
    /// such neighbour and does nothing, so a link change does not repeat the
    /// branch at every call site.
    fn set_next(&mut self, index: u32, next: u32) -> Result<()>;
    fn set_prev(&mut self, index: u32, prev: u32) -> Result<()>;
}

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
pub(crate) fn walk_side<F>(book: &mut ClobMarketV0, side: Side, mut visit: F) -> Result<()>
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
pub(crate) fn walk_side_ref<F>(book: &ClobMarketV0, side: Side, mut visit: F) -> Result<()>
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

impl ClobBook for ClobMarketV0 {
    /// Destructure every header field, which is the zero-copy form of
    /// `set_inner`. A header field added without an initializer here is a
    /// compile error. The function then fills the tail to capacity and threads
    /// the free list.
    fn initialize(
        &mut self,
        new_authority: Address,
        new_place_authority: Address,
        config: MarketConfigV0,
    ) -> Result<()> {
        let cap = self.capacity() as u32;
        require!(cap >= 2, ClobError::InvalidCapacity);
        // The caps budget divides by `quoter_spec::BASE_PRECISION` and so does
        // velocity's exact-notional check on the fill this book returns. A
        // market on any other denominator prices its own execute response on
        // one scale and is settled on another, so every fill fails. The field
        // stays in the header because resting sizes are denominated in it and
        // readers of the account expect to find it.
        require!(
            config.base_precision == BASE_PRECISION,
            ClobError::InvalidConfig
        );

        validate_evict_threshold(config.evict_threshold_per_side, cap)?;
        require!(
            config.default_activation_delay_slots <= config.max_activation_delay_slots,
            ClobError::InvalidConfig
        );
        require!(
            config.max_quote_levels != 0 && config.max_quote_levels <= QUOTE_LEVELS_CEILING,
            ClobError::InvalidConfig
        );
        require!(
            config.max_execute_fills != 0 && config.max_execute_fills <= EXECUTE_FILLS_CEILING,
            ClobError::InvalidConfig
        );
        require!(
            config.max_execute_users != 0 && config.max_execute_users <= EXECUTE_USERS_CEILING,
            ClobError::InvalidConfig
        );

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
            padding,
            padding1,
            taker_origin_head,
            taker_origin_tail,
            taker_origin_count,
            reservation_grace_slots,
            response,
            crank,
        } = &mut **self;

        *authority = new_authority;
        *place_authority = new_place_authority;
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
        // Lay out the arena as one free list, slot 0 first. `try_push` appends
        // within the tail the slab owns and fails rather than writing past it.
        (0..cap).try_for_each(|i| -> Result<()> {
            let mut node: OrderNodeV0 = bytemuck::Zeroable::zeroed();
            node.next = if i + 1 == cap { NIL } else { i + 1 };
            self.try_push(node)
                .map_err(|_| ClobError::InvalidCapacity)?;
            Ok(())
        })?;

        self.validate_book()
    }

    /// Insert with price-time priority. The walk starts at the best of book and
    /// passes every order at an equal or better price, so the new order queues
    /// behind its own level.
    ///
    /// Each side owns half the arena. A full side rejects every placement, even
    /// a better-priced one. Eviction runs through velocity as a crank (see
    /// [`Self::evict_worst`]), which keeps the evicted maker's margin
    /// aggregates exact. The soft-cap buffer makes the hard cap an operations
    /// failure rather than a normal state.
    fn place(&mut self, params: PlaceOrderParams) -> Result<OrderRefV0> {
        let PlaceOrderParams {
            side,
            price,
            base_asset_amount,
            user,
            activation_slot,
            placed_slot,
            max_ts,
            now,
            taker_origin,
            client_order_id,
            reject_if_crossed,
            reduce_only,
        } = params;

        require!(
            price != 0 && base_asset_amount != 0 && !address_eq(&user.authority, &ZERO_ADDRESS),
            ClobError::InvalidOrderParams
        );
        require!(
            placed_slot <= activation_slot,
            ClobError::InvalidOrderParams
        );
        require!(
            base_asset_amount >= self.min_order_size,
            ClobError::OrderTooSmall
        );
        require!(
            price % self.order_tick_size.max(1) == 0,
            ClobError::PriceNotTickAligned
        );
        require!(
            base_asset_amount % self.order_step_size.max(1) == 0,
            ClobError::SizeNotStepAligned
        );

        // A maker that quotes through the other side has mispriced, and would
        // rather place nothing than rest crossed. An order inside its activation
        // delay counts, because it is resting liquidity a moment from now. An
        // expired order does not, or one cheap order could refuse every
        // post-only placement on the other side.
        if reject_if_crossed {
            let mut crossed = false;
            walk_side(self, side.opposite(), |_, _, node| {
                if node.is_expired(now) {
                    return Ok(Walk::Continue);
                }

                crossed = side.is_crossed_by(price, node.price);
                Ok(Walk::Stop)
            })?;

            require!(!crossed, ClobError::OrderWouldCross);
        }

        let per_side = (self.capacity() / 2) as u32;
        let count_before = self.node_count(side);
        require!(count_before < per_side, ClobError::SideAtCapacity);

        // Insertion point: the last node the new order queues behind, and
        // the first it goes in front of.
        let mut prev = NIL;
        let mut next = NIL;
        walk_side(self, side, |_, index, node| {
            if side.is_worse_price(node.price, price) {
                next = index;
                Ok(Walk::Stop)
            } else {
                prev = index;
                Ok(Walk::Continue)
            }
        })?;

        let index = alloc_node(self)?;
        let order_id = self.consume_order_id()?;
        self.write_node(
            index,
            OrderNodeV0 {
                authority: user.authority,
                price,
                base_asset_amount,
                activation_slot,
                placed_slot,
                max_ts,
                order_id,
                prev,
                next,
                // Written by `insert_order` when the order is taker-origin,
                // and never read otherwise.
                taker_origin_prev: NIL,
                taker_origin_next: NIL,
                bit_flags: OrderBitFlag::Open as u8
                    | side.side_bit()
                    | OrderBitFlag::TakerOrigin.bit_if(taker_origin)
                    | OrderBitFlag::ReduceOnly.bit_if(reduce_only),
                padding0: 0,
                sub_account_id: user.sub_account_id,
                client_order_id,
            },
        )?;

        insert_order(self, side, index, prev, next, taker_origin)?;
        self.set_node_count(
            side,
            count_before
                .checked_add(1)
                .ok_or(ClobError::BookInvariantViolated)?,
        )?;

        let placed = self.read_node(index)?;
        require!(
            placed.order_id == order_id
                && placed.is_bit_flag_set(OrderBitFlag::Open)
                && placed.side() == side
                && placed.is_taker_origin() == taker_origin,
            ClobError::BookInvariantViolated
        );
        require!(
            both_or_neither(prev == NIL, self.best(side) == index),
            ClobError::BookInvariantViolated
        );
        require!(
            both_or_neither(next == NIL, self.worst(side) == index),
            ClobError::BookInvariantViolated
        );

        if prev != NIL {
            require!(
                self.read_node(prev)?.next == index,
                ClobError::BookInvariantViolated
            );
        }

        if next != NIL {
            require!(
                self.read_node(next)?.prev == index,
                ClobError::BookInvariantViolated
            );
        }

        self.fold_wake_hints(max_ts, activation_slot, placed_slot)?;
        // Placement is the book's most frequent write and it knows the slot, so
        // it is the place that drops an activation hint the chain has passed.
        self.expire_activation_hint(placed_slot)?;
        self.validate_book()?;

        Ok(OrderRefV0 {
            node_index: index,
            order_id,
        })
    }

    /// Fails closed on a stale hint. The node may be out of range, free, or
    /// hold a different order. `user` must own the order.
    ///
    /// A taker-origin remainder is refused unless `force` while [`is_bound`]
    /// holds. See [`ClobError::TakerOriginBound`] for why it binds. The bind does
    /// not affect `crank_taker_origin_cross`, which removes an order through
    /// `fill` and never through this path.
    fn cancel(
        &mut self,
        user: UserRefV0,
        order_ref: OrderRefV0,
        slot: u64,
        force: bool,
    ) -> Result<RemovedOrder> {
        let node = live_order(self, order_ref)?;
        require!(node.user_ref() == user, ClobError::OrderUserMismatch);
        require!(
            force || !is_bound(&node, slot, self.reservation_grace_slots),
            ClobError::TakerOriginBound
        );

        let removed = removed_order(&node);
        remove_order(self, order_ref.node_index)?;
        validate_single_removal(self, &node, order_ref.node_index)?;
        self.validate_book()?;
        Ok(removed)
    }

    /// Withdraw every order `user` holds on the requested sides in one pass.
    ///
    /// The book has no per-user index. User identity lives inline on the node
    /// and there is no seat table, by design (see [`OrderNodeV0`]). This is
    /// therefore a full walk of each requested side, O(orders on the side)
    /// rather than O(the user's orders). That is still the cheap direction. The
    /// alternative a maker has is one instruction per order, and the walk costs
    /// a fraction of one CPI round trip per hop.
    ///
    /// Removals are capped at [`CANCEL_ALL_ORDERS_CEILING`] per call, and
    /// [`CancelAllOutcome::exhaustive`] reports whether the walk reached the end
    /// of every requested side. It is false only when the cap stopped the walk,
    /// which is the one case where orders of this user are still resting. The
    /// caller must repeat the call until it comes back true.
    ///
    /// Each removed order's id goes to `removed_ids` as the walk frees it, in
    /// book order per side. The handler streams those into the cancel record's
    /// log buffer. A sink rather than a returned `Vec` keeps this off the heap
    /// on a path that can touch a hundred orders.
    fn cancel_all(
        &mut self,
        user: UserRefV0,
        sides: CancelSidesV0,
        slot: u64,
        force: bool,
        removed_ids: &mut dyn FnMut(u32) -> Result<()>,
    ) -> Result<CancelAllOutcome> {
        let ceiling = CANCEL_ALL_ORDERS_CEILING as u32;
        let mut outcome = CancelAllOutcome {
            exhaustive: true,
            ..Default::default()
        };
        let mut skipped_bound = false;
        // The count runs across both sides, so the cap bounds the call rather than
        // each side of it.
        let mut total_removed = 0u32;
        // One repair for the whole call. A maker's ladder shares one `max_ts`, so
        // repairing per removal re-walks the live orders almost every time and made
        // a full sweep quadratic.
        let mut owes_expiry_repair = false;

        for side in sides.sides().iter().copied() {
            if !outcome.exhaustive {
                break;
            }

            let count_before = self.node_count(side);
            let mut base_removed = 0u64;
            let mut orders_removed = 0u32;
            let mut reduce_only_removed = 0u32;
            walk_side(self, side, |book, index, node| {
                if node.user_ref() != user {
                    return Ok(Walk::Continue);
                }

                // Pass over a bound remainder rather than fail the sweep, so one
                // order a maker cannot pull yet does not block a whole ladder. The
                // flag is read after both sides run, because ending the walk here
                // would stop the other side too.
                if !force && is_bound(node, slot, book.reservation_grace_slots) {
                    skipped_bound = true;
                    return Ok(Walk::Continue);
                }

                if total_removed >= ceiling {
                    outcome.exhaustive = false;
                    return Ok(Walk::Stop);
                }

                base_removed = base_removed
                    .checked_add(node.base_asset_amount)
                    .ok_or(ClobError::MathError)?;
                orders_removed += 1;
                total_removed += 1;
                if node.is_reduce_only() {
                    reduce_only_removed += 1;
                }

                removed_ids(node.client_order_id)?;
                owes_expiry_repair |= holds_expiry_hint(book, node);
                unlink_order(book, index)?;
                Ok(Walk::Continue)
            })?;

            // Every removal the walk made came off this side.
            require!(
                self.node_count(side)
                    == count_before
                        .checked_sub(orders_removed)
                        .ok_or(ClobError::BookInvariantViolated)?,
                ClobError::BookInvariantViolated
            );

            match side {
                Side::Bid => {
                    outcome.bid_base_asset_amount = base_removed;
                    outcome.bid_orders = orders_removed;
                    outcome.bid_reduce_only_orders = reduce_only_removed;
                }
                Side::Ask => {
                    outcome.ask_base_asset_amount = base_removed;
                    outcome.ask_orders = orders_removed;
                    outcome.ask_reduce_only_orders = reduce_only_removed;
                }
            }
        }

        if skipped_bound {
            outcome.exhaustive = false;
        }

        if owes_expiry_repair {
            self.recompute_wake_hints(true, None)?;
        }

        self.validate_book()?;
        Ok(outcome)
    }

    /// Eviction, run as a crank. It takes the order [`evictable_order`] names,
    /// and only while the side holds at least `evict_threshold_per_side`
    /// orders. The crank works the soft-cap buffer down so placements never
    /// reach the hard cap. Velocity is the caller and loads the evicted maker's
    /// `User`, so aggregates stay exact.
    fn evict_worst(&mut self, side: Side, slot: u64) -> Result<RemovedOrder> {
        let count = self.node_count(side);
        require!(
            count > 0 && count >= self.evict_threshold_per_side,
            ClobError::BelowEvictThreshold
        );

        let index = evictable_order(self, side, slot)?;
        require!(index != NIL, ClobError::TakerOriginBound);

        let node = self.read_node(index)?;
        require!(
            node.is_bit_flag_set(OrderBitFlag::Open) && node.side() == side,
            ClobError::BookInvariantViolated
        );

        let removed = removed_order(&node);
        remove_order(self, index)?;
        require!(
            self.node_count(side) == count - 1,
            ClobError::BookInvariantViolated
        );

        validate_single_removal(self, &node, index)?;
        self.validate_book()?;
        Ok(removed)
    }

    /// Expiry reclamation, run as a crank. Execute only skips an expired order.
    /// A removal without the maker's `User` loaded is the aggregate leak this
    /// design removes. Fails closed on a stale hint.
    fn remove_expired(&mut self, order_ref: OrderRefV0, now: i64) -> Result<RemovedOrder> {
        let node = live_order(self, order_ref)?;
        require!(node.is_expired(now), ClobError::OrderNotExpired);
        let removed = removed_order(&node);
        remove_order(self, order_ref.node_index)?;
        validate_single_removal(self, &node, order_ref.node_index)?;
        self.validate_book()?;
        Ok(removed)
    }

    /// Takes size off a resting taker-origin order that velocity filled
    /// elsewhere. A remainder aggresses against prices this program cannot see,
    /// so velocity does that matching and reports it back here.
    ///
    /// The order keeps its queue place and its id, which a cancel and a fresh
    /// placement would lose. A leftover under `min_order_size` is culled and
    /// reported, so the caller can unwind the reservation it holds for it.
    ///
    /// An expired or unactivated order is refused, because no read of the book
    /// offers such an order to anyone.
    fn fill(
        &mut self,
        order_ref: OrderRefV0,
        base_asset_amount: u64,
        slot: u64,
        now: i64,
    ) -> Result<FilledOrder> {
        let node = live_order(self, order_ref)?;
        require!(node.is_taker_origin(), ClobError::OrderNotTakerOrigin);
        require!(is_live(&node, slot, now), ClobError::OrderNotLive);
        require!(
            base_asset_amount > 0 && base_asset_amount <= node.base_asset_amount,
            ClobError::FillExceedsOrder
        );

        let remainder = node.base_asset_amount - base_asset_amount;
        let culls = remainder > 0 && remainder < self.min_order_size;
        let mut filled = FilledOrder {
            order_id: node.order_id,
            client_order_id: node.client_order_id,
            base_asset_amount,
            culled_base_asset_amount: 0,
            removed: false,
        };

        if remainder == 0 || culls {
            filled.culled_base_asset_amount = remainder;
            filled.removed = true;
            remove_order(self, order_ref.node_index)?;
            validate_single_removal(self, &node, order_ref.node_index)?;
        } else {
            let mut reduced = node;
            reduced.base_asset_amount = remainder;
            self.write_node(order_ref.node_index, reduced)?;
        }

        self.validate_book()?;
        Ok(filled)
    }

    /// Aggregate the levels a taker of `direction`/`size` would clear,
    /// best-first, capped at the market's `max_quote_levels`, and stream them
    /// into the response region as wincode [`crate::state::QuoteResponseV0`].
    /// The walk skips expired orders, orders still inside their activation
    /// delay, and the taker's own orders. Skipping the taker's own orders is
    /// self-trade prevention, the same rule [`Self::execute`] applies through
    /// the shared [`is_matchable`]. The walk applies the same unknown-user
    /// grace rule as execute, so the router's split math matches what execute
    /// will deliver.
    ///
    /// It also withholds whatever [`CrossReservation`] holds back. That is the
    /// units a crossing taker remainder claims, and the whole of a remainder a
    /// counterparty crosses. [`Self::execute`] withholds the same units, so the
    /// depth published here is always depth the fill can deliver. A level that
    /// loses all of its size to a claim is not published at all.
    #[allow(clippy::too_many_arguments)]
    fn quote(
        &mut self,
        direction: Direction,
        size: u64,
        users: &[UserRefV0],
        caps: &UserCapsV0,
        reference_price: i64,
        taker: Option<&UserRefV0>,
        limit_price: u64,
        include_taker_origin_reservations: bool,
        slot: u64,
        now: i64,
    ) -> Result<ResponsePointerV0> {
        require!(user_set_within_capacity(users), ClobError::OversizedUserSet);
        let side = direction.book_side();

        let max_levels = self.max_quote_levels.min(QUOTE_LEVELS_CEILING) as usize;
        // A level aggregates however many orders sit at one price, so a ladder
        // capped on levels alone would quote depth that execute declines.
        let max_execute_fills = self.max_execute_fills.min(EXECUTE_FILLS_CEILING) as usize;
        let max_execute_users = self.max_execute_users.min(EXECUTE_USERS_CEILING) as usize;

        let mut writer = QuoteWriter::new();
        let mut open_level: Option<PriceLevel> = None;
        let mut last_written_price: Option<u64> = None;
        let mut levels_written = 0usize;

        let mut remaining = size;
        let mut promised_fills = 0usize;
        let (mut cull_slot_used, mut partial_slot_used) = (false, false);
        let mut unsettleable_level: Option<PriceLevel> = None;

        let mut reservation =
            CrossReservation::new(self, side, slot, now, include_taker_origin_reservations);
        let mut budget = UserBudget::new(caps, side, reference_price);
        let mut users_promised = DistinctUsers::new();

        walk_side(self, side, |book, _, node| {
            // Stop where `execute` stops, so the ladder ends where the fill would.
            if remaining == 0 || promised_fills == max_execute_fills {
                return Ok(Walk::Stop);
            }

            if direction.worse_than_limit(node.price, limit_price) {
                return Ok(Walk::Stop);
            }

            // Skips, cheapest test first. The reservation is read before the
            // self-trade test, because the allocation is positional.
            if !is_live(node, slot, now) {
                return Ok(Walk::Continue);
            }

            let available = reservation.available(book, node)?;
            if available == 0 || is_takers_own(node, taker) {
                return Ok(Walk::Continue);
            }

            // Hoisted out of the scan, which compares a 34-byte ref per entry.
            let user = node.user_ref();
            let owner = users.iter().position(|u| *u == user);
            match settleable(
                users,
                owner,
                node,
                book.unknown_user_grace_slots,
                book.blocking_min_size,
                slot,
            ) {
                Settleable::Yes => {}
                Settleable::SteppedOver => return Ok(Walk::Continue),
                // The report is the depth a caller that loads this owner could
                // take, which excludes what a remainder claims. `available` is
                // nonzero here, because a wholly claimed order is passed over above.
                Settleable::Withheld => {
                    unsettleable_level = Some(PriceLevel {
                        price: node.price,
                        size: available,
                    });

                    return Ok(Walk::Stop);
                }
            }

            let take = budget.allow(
                owner,
                remaining.min(available),
                node.price,
                node.is_reduce_only(),
            );

            // The owner is out of room. The depth behind it is still fillable.
            if take == 0 {
                return Ok(Walk::Continue);
            }

            // `execute` reports a truncated order in one of two single-slot
            // sections and stops at the second of either, so this walk stops too.
            if take < node.base_asset_amount {
                let remainder = node.base_asset_amount - take;
                let slot_used = if remainder < book.min_order_size {
                    &mut cull_slot_used
                } else {
                    &mut partial_slot_used
                };

                if *slot_used {
                    return Ok(Walk::Stop);
                }

                *slot_used = true;
            }

            if !users_promised.admit(owner, max_execute_users) {
                return Ok(Walk::Stop);
            }

            promised_fills += 1;
            // The side is price-sorted, so orders at one price are contiguous and
            // one open level holds them all.
            match open_level {
                Some(level) if level.price == node.price => {
                    open_level = Some(PriceLevel {
                        price: level.price,
                        size: level.size.checked_add(take).ok_or(ClobError::MathError)?,
                    });
                }
                _ => {
                    if levels_written == max_levels {
                        return Ok(Walk::Stop);
                    }

                    if let Some(level) = open_level {
                        write_level(book, &mut writer, side, &mut last_written_price, level)?;
                    }

                    open_level = Some(PriceLevel {
                        price: node.price,
                        size: take,
                    });

                    levels_written += 1;
                }
            }

            remaining -= take;
            Ok(if remaining == 0 {
                Walk::Stop
            } else {
                Walk::Continue
            })
        })?;

        if let Some(level) = open_level {
            write_level(self, &mut writer, side, &mut last_written_price, level)?;
        }

        // `finish` backfills the ladder's count and writes the withheld report
        // behind it, which is the shape `QuoteResponseV0` declares.
        let len = writer
            .finish(&mut self.response, unsettleable_level.unwrap_or_default())
            .map_err(ClobError::from)?;
        Ok(response_pointer(len))
    }

    /// Describe the resting orders behind the ladder, best price first.
    ///
    /// This is the counterpart of [`Self::quote`]. It runs the same walk under
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
    fn quote_l3(
        &mut self,
        direction: Direction,
        size: u64,
        max_rows: u16,
        include_taker_origin_reservations: bool,
        slot: u64,
        now: i64,
    ) -> Result<ResponsePointerV0> {
        let side = direction.book_side();
        let rows_wanted = max_rows.min(L3_ROWS_CEILING) as usize;
        let mut reservation =
            CrossReservation::new(self, side, slot, now, include_taker_origin_reservations);
        let mut writer = L3Writer::new();
        // Zero asks for the whole side rather than for nothing. A caller that
        // draws a book has no size in mind.
        let mut remaining = if size == 0 { u64::MAX } else { size };
        // Depth the walk left behind, so a caller knows its list is a prefix.
        let mut more = false;

        walk_side(self, side, |book, index, node| {
            if remaining == 0 || writer.rows() == rows_wanted {
                more = true;
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
            .finish(&mut self.response, more)
            .map_err(ClobError::from)?;
        Ok(response_pointer(len))
    }

    /// Consume matchable orders best-first, removing filled orders and
    /// streaming each maker's share into the response region as wincode
    /// [`crate::state::ExecuteResponseV0`] for velocity to apply. The walk
    /// skips an expired order and never removes it. Reclamation goes through
    /// [`Self::remove_expired`] so the maker's aggregates update. A partial
    /// fill that leaves a remainder below `min_order_size` culls the order,
    /// because dust must not hold an arena slot. The cull rides the wire
    /// response, because that maker was filled and is therefore loaded. An
    /// order whose user is outside the caller's set is skipped inside the grace
    /// window and ends the walk past it (see [`settleable`]). The taker's own
    /// orders are always skipped, which is self-trade prevention. There is no
    /// price bound, because the router already chose this quoter's allocation
    /// from its quote.
    ///
    /// Fills merge by user. The records already written into the response are
    /// the accumulator, so a repeat maker patches that record's totals in place
    /// instead of building a heap `Vec` of balance changes.
    ///
    /// The walk passes over two more things, alongside the expired and the
    /// not-yet-activated. They are the units a crossing taker remainder claims,
    /// and a taker-origin order that has a live crossing counterparty on the
    /// other side. See [`CrossReservation`]. [`Self::quote`] reads it too, so
    /// the two never disagree about what is takeable.
    #[allow(clippy::too_many_arguments)]
    fn execute(
        &mut self,
        direction: Direction,
        size: u64,
        users: &[UserRefV0],
        caps: &UserCapsV0,
        reference_price: i64,
        taker: Option<&UserRefV0>,
        include_taker_origin_reservations: bool,
        slot: u64,
        now: i64,
    ) -> Result<ExecuteOutcome> {
        require!(user_set_within_capacity(users), ClobError::OversizedUserSet);
        let side = direction.book_side();
        let max_fills = self.max_execute_fills.min(EXECUTE_FILLS_CEILING) as usize;
        let max_users = self.max_execute_users.min(EXECUTE_USERS_CEILING) as usize;
        let base_precision = self.base_precision.max(1) as u128;
        let count_before = self.node_count(side);

        let mut writer = ExecuteWriter::new();
        // A fill consumes at most one order, so both collections are bounded by
        // `max_fills` and neither has to grow by doubling.
        let mut fills: Vec<FillSlimV0> = Vec::with_capacity(max_fills);
        let mut completed: Vec<CompletedOrderV0> = Vec::with_capacity(max_fills);
        let mut cancelled: Option<CancelledRemainderV0> = None;
        let mut partial: Option<PartiallyFilledOrderV0> = None;

        let mut remaining = size;
        let mut removals = 0u32;
        let mut last_filled_price: Option<u64> = None;
        let mut swept_notional = 0u128;
        let mut quote_attributed = 0u128;
        // One expiry repair for the whole sweep, for the reason `cancel_all` batches
        // its own. The repair walks the live orders, and a sweep can free many.
        let mut owes_expiry_repair = false;

        let mut reservation =
            CrossReservation::new(self, side, slot, now, include_taker_origin_reservations);
        let mut budget = UserBudget::new(caps, side, reference_price);

        walk_side(self, side, |book, index, node| {
            if remaining == 0 || fills.len() == max_fills {
                return Ok(Walk::Stop);
            }

            // The same order of reasons `quote` applies, so the fill ends
            // exactly where the ladder did.
            if !is_live(node, slot, now) {
                return Ok(Walk::Continue);
            }

            let available = reservation.available(book, node)?;
            if available == 0 || is_takers_own(node, taker) {
                return Ok(Walk::Continue);
            }

            let user = node.user_ref();
            let owner = users.iter().position(|u| *u == user);
            match settleable(
                users,
                owner,
                node,
                book.unknown_user_grace_slots,
                book.blocking_min_size,
                slot,
            ) {
                Settleable::Yes => {}
                Settleable::SteppedOver => return Ok(Walk::Continue),
                Settleable::Withheld => return Ok(Walk::Stop),
            }

            let take = budget.allow(
                owner,
                remaining.min(available),
                node.price,
                node.is_reduce_only(),
            );

            if take == 0 {
                return Ok(Walk::Continue);
            }

            // A truncated order leaves either a partial or a culled remainder,
            // and the response carries one slot for each. A per-owner budget can
            // truncate two orders, so the walk ends where the response runs out
            // of room. Ending short is a smaller fill the caller reads off the
            // response, rather than a failure on an ordinary request.
            if take < node.base_asset_amount {
                let remainder = node.base_asset_amount - take;
                let slot_taken = if remainder < book.min_order_size {
                    cancelled.is_some()
                } else {
                    partial.is_some()
                };

                if slot_taken {
                    return Ok(Walk::Stop);
                }
            }

            // The records already written are the accumulator, so a repeat
            // maker is a scan of them rather than a table this frame has no
            // room for.
            let existing = writer
                .changes(&book.response)
                .map_err(ClobError::from)?
                .iter()
                .position(|change| change.user == user);
            if existing.is_none() && writer.changes_len() == max_users {
                return Ok(Walk::Stop);
            }

            check_fill_price(side, last_filled_price, node.price, take)?;
            last_filled_price = Some(node.price);

            // Each fill's quote is the difference of two running floors, not the
            // floor of its own notional. The sweep then rounds once in total,
            // instead of once per fill, and the dust a per-fill truncation loses
            // would come out of the makers.
            swept_notional = swept_notional
                .checked_add(
                    (node.price as u128)
                        .checked_mul(take as u128)
                        .ok_or(ClobError::MathError)?,
                )
                .ok_or(ClobError::MathError)?;
            let swept_quote = swept_notional / base_precision;
            let quote_size: u64 = (swept_quote - quote_attributed)
                .try_into()
                .map_err(|_| ClobError::MathError)?;
            quote_attributed = swept_quote;

            let change_index = match existing {
                Some(index) => {
                    let index = index as u32;
                    let record = writer
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
                    index
                }
                None => writer
                    .push_change(
                        &mut book.response,
                        UserBalanceChangeV0 {
                            base_size: take,
                            quote_size,
                            user,
                            _pad: [0; 6],
                        },
                    )
                    .map_err(ClobError::from)?,
            };

            fills.push(FillSlimV0 {
                order_id: node.order_id,
                client_order_id: node.client_order_id,
                base_size: take,
            });

            if take == node.base_asset_amount {
                // The id names the change it belongs to and rides a section of
                // its own, written once the changes are done. Growing the
                // record in place would mean shifting every record after it on
                // every consumed order.
                completed.push(CompletedOrderV0 {
                    order_id: node.order_id,
                    change_index: u16::try_from(change_index).map_err(|_| ClobError::MathError)?,
                    flags: removed_order_flags(node),
                    _pad: [0; 1],
                    client_order_id: node.client_order_id,
                });

                owes_expiry_repair |= holds_expiry_hint(book, node);
                unlink_order(book, index)?;
                removals += 1;
            } else {
                let remainder = node.base_asset_amount - take;
                if remainder < book.min_order_size {
                    // `remaining` running out ends the walk, so at most one cull
                    // exists and `cancelled` needs no growable storage. Fail here
                    // rather than drop a cull velocity must unwind.
                    require!(cancelled.is_none(), ClobError::BookInvariantViolated);
                    cancelled = Some(CancelledRemainderV0 {
                        order_id: node.order_id,
                        base_asset_amount: remainder,
                        price: node.price,
                        client_order_id: node.client_order_id,
                        user: node.user_ref(),
                        flags: removed_order_flags(node),
                        _pad: [0; 1],
                    });

                    owes_expiry_repair |= holds_expiry_hint(book, node);
                    unlink_order(book, index)?;
                    removals += 1;
                } else {
                    // A balance change merges every order of one maker, so this
                    // is the only place the fill says which order moved. At most
                    // one exists, for the same reason at most one cull does.
                    require!(partial.is_none(), ClobError::BookInvariantViolated);
                    partial = Some(PartiallyFilledOrderV0 {
                        order_id: node.order_id,
                        base_filled: take,
                        client_order_id: node.client_order_id,
                        change_index,
                    });

                    book.update_node(index, |n| n.base_asset_amount = remainder)?;
                }
            }

            remaining -= take;
            Ok(if remaining == 0 {
                Walk::Stop
            } else {
                Walk::Continue
            })
        })?;

        // `finish` backfills the change count and writes the remaining
        // sections in the order `ExecuteResponseV0` declares them.
        let response = response_pointer(
            writer
                .finish(
                    &mut self.response,
                    cancelled.as_slice(),
                    &completed,
                    partial.as_slice(),
                )
                .map_err(ClobError::from)?,
        );

        // Every removal the walk made came off this side.
        let expected_count = count_before
            .checked_sub(removals)
            .ok_or(ClobError::BookInvariantViolated)?;
        require!(
            self.node_count(side) == expected_count,
            ClobError::BookInvariantViolated
        );

        // The sweep's one expiry repair, owed only if something it freed was
        // holding the hint.
        if owes_expiry_repair {
            self.recompute_wake_hints(true, None)?;
        }

        // A fill is a removal path too. It consumes orders whole and culls a
        // sub-minimum remainder, and either can retire the activation the hint
        // pointed at. It is the one such path that knows the slot without being
        // handed it.
        self.expire_activation_hint(slot)?;
        self.validate_book()?;

        Ok(ExecuteOutcome {
            response,
            fills,
            cancelled_client_order_id: cancelled.map(|cull| cull.client_order_id),
        })
    }

    /// Push zeroed nodes for the new slots after a capacity grow, and thread
    /// them into the free list.
    fn grow_free_list(&mut self) -> Result<()> {
        while !self.is_full() {
            let index = self.len() as u32;
            let mut node: OrderNodeV0 = bytemuck::Zeroable::zeroed();
            node.next = self.free_head;
            self.try_push(node)
                .map_err(|_| ClobError::InvalidCapacity)?;
            self.free_head = index;
            self.free_count = self
                .free_count
                .checked_add(1)
                .ok_or(ClobError::InvalidCapacity)?;
        }

        self.validate_book()
    }

    fn node_count(&self, side: Side) -> u32 {
        match side {
            Side::Bid => self.bid_count,
            Side::Ask => self.ask_count,
        }
    }

    fn best(&self, side: Side) -> u32 {
        match side {
            Side::Bid => self.best_bid,
            Side::Ask => self.best_ask,
        }
    }

    fn worst(&self, side: Side) -> u32 {
        match side {
            Side::Bid => self.worst_bid,
            Side::Ask => self.worst_ask,
        }
    }

    /// The O(1) postcondition for every mutating operation. The three counts
    /// account for the whole arena, the free head agrees with the free count,
    /// and each side's endpoints are live nodes of that side with null outer
    /// links.
    fn validate_book(&self) -> Result<()> {
        let total = self
            .bid_count
            .checked_add(self.ask_count)
            .and_then(|live| live.checked_add(self.free_count))
            .ok_or(ClobError::BookInvariantViolated)?;
        require!(
            total == self.capacity() as u32,
            ClobError::BookInvariantViolated
        );
        require!(
            both_or_neither(self.free_count == 0, self.free_head == NIL),
            ClobError::BookInvariantViolated
        );

        if self.free_head != NIL {
            require!(
                !self
                    .read_node(self.free_head)?
                    .is_bit_flag_set(OrderBitFlag::Open),
                ClobError::BookInvariantViolated
            );
        }

        [Side::Bid, Side::Ask]
            .into_iter()
            .try_for_each(|side| -> Result<()> {
                let count = self.node_count(side);
                let (best, worst) = (self.best(side), self.worst(side));
                require!(
                    both_or_neither(count == 0, best == NIL)
                        && both_or_neither(count == 0, worst == NIL),
                    ClobError::BookInvariantViolated
                );

                if count == 0 {
                    return Ok(());
                }

                require!(
                    both_or_neither(count == 1, best == worst),
                    ClobError::BookInvariantViolated
                );

                let head = self.read_node(best)?;
                let tail = self.read_node(worst)?;
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
            })?;
        // The claimant lists get the same depth of check. An empty list has both
        // endpoints null and a zero count. A list of one has the same node at
        // both ends. Each endpoint is a live taker-origin order of that side
        // with a null outer link. The list is a subset of the side, so its count
        // cannot exceed the side's.
        //
        // The exhaustive version walks both lists and runs in the unit tests. It
        // checks that every taker-origin order on the side is listed, that ids
        // ascend, and that links are mutual.
        [Side::Bid, Side::Ask]
            .into_iter()
            .try_for_each(|side| -> Result<()> {
                let count = self.claimant_count(side);
                let (head, tail) = (self.first_claimant(side), self.last_claimant(side));
                require!(
                    both_or_neither(count == 0, head == NIL)
                        && both_or_neither(count == 0, tail == NIL),
                    ClobError::BookInvariantViolated
                );
                require!(
                    count as u32 <= self.node_count(side),
                    ClobError::BookInvariantViolated
                );

                if count == 0 {
                    return Ok(());
                }

                require!(
                    both_or_neither(count == 1, head == tail),
                    ClobError::BookInvariantViolated
                );

                let first = self.read_node(head)?;
                let last = self.read_node(tail)?;
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
            })
    }
}

/// Resolve an order hint to its live node, failing closed when the node is out
/// of range, free, or reused for a different order. An out-of-range hint
/// reports as stale rather than as arena corruption. The hint comes from the
/// caller, and a node index that was valid before a shrink is a stale handle.
fn live_order(book: &ClobMarketV0, order_ref: OrderRefV0) -> Result<OrderNodeV0> {
    let node = book
        .read_node(order_ref.node_index)
        .map_err(|_| ClobError::StaleOrderRef)?;
    require!(
        node.is_bit_flag_set(OrderBitFlag::Open) && node.order_id == order_ref.order_id,
        ClobError::StaleOrderRef
    );

    Ok(node)
}

fn removed_order(node: &OrderNodeV0) -> RemovedOrder {
    RemovedOrder {
        user: node.user_ref(),
        order_id: node.order_id,
        client_order_id: node.client_order_id,
        price: node.price,
        base_asset_amount: node.base_asset_amount,
        side: node.side(),
        taker_origin: node.is_taker_origin(),
        reduce_only: node.is_reduce_only(),
        max_ts: node.max_ts,
    }
}

/// Whether anyone at all could match this order now, which is a property of the
/// order alone. Every read of a side asks this first and [`CrossReservation`]
/// second, because a claim is allocated positionally over the matchable orders.
/// A reason belonging to the caller must be tested after that allocation.
pub(crate) fn is_live(node: &OrderNodeV0, slot: u64, now: i64) -> bool {
    !node.is_expired(now) && node.is_active(slot)
}

/// Past the window in which the book honours this remainder's claim on the
/// depth it crosses. The window runs from the activation slot, which is when the
/// auction the claim protects ends.
fn is_claim_lapsed(claimant: &OrderNodeV0, slot: u64, grace_slots: u64) -> bool {
    slot >= claimant.activation_slot.saturating_add(grace_slots)
}

/// A taker remainder whose claim the book still honours. Its owner cannot
/// cancel it without `force`. The bind ends exactly when the claim lapses.
fn is_bound(node: &OrderNodeV0, slot: u64, grace_slots: u16) -> bool {
    node.is_taker_origin() && !is_claim_lapsed(node, slot, grace_slots as u64)
}

/// The worst-priced order on `side` that eviction may take, or [`NIL`] when
/// every order there is a bound remainder. A bound remainder is passed over
/// rather than refusing the eviction, so its owner cannot use a permissionless
/// crank to pull it, and the side still frees a slot while the claim holds.
/// The search passes only bound remainders, so the side's claimant count
/// bounds it.
pub(crate) fn evictable_order(book: &ClobMarketV0, side: Side, slot: u64) -> Result<u32> {
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

/// The caller's own resting order. No read of a side offers such an order back
/// to the caller, which is self-trade prevention.
fn is_takers_own(node: &OrderNodeV0, taker: Option<&UserRefV0>) -> bool {
    taker.is_some_and(|t| *t == node.user_ref())
}

/// Whether the caller can settle for this order's owner, and if not, why that
/// matters.
enum Settleable {
    /// The owner is in the caller's set, or the set is unrestricted.
    Yes,
    /// The owner is absent, and the walk passes over the order to the depth
    /// behind it. Either the order is younger than the grace window, or it is
    /// below `blocking_min_size` at any age.
    SteppedOver,
    /// The owner is absent, the order is old enough that the caller had every
    /// chance to carry it, and it is big enough to be worth the right. The walk
    /// ends here.
    Withheld,
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

/// A transaction locks at most 64 accounts and a maker costs two, so no caller
/// can carry every user a book might hold. The caller fills as deep as the users
/// it brought. Ending the walk rather than stepping over the order keeps that
/// honest. The walk is best-first, so a caller can trade less of the book, never
/// a worse part of it.
fn settleable(
    users: &[UserRefV0],
    index: Option<usize>,
    node: &OrderNodeV0,
    grace_slots: u32,
    blocking_min_size: u64,
    slot: u64,
) -> Settleable {
    if users.is_empty() || index.is_some() {
        return Settleable::Yes;
    }

    // Ending a walk is a right, and a right that costs only `min_order_size` can
    // be bought in bulk. 49 orders on 49 fresh sub-accounts would put the depth
    // behind them out of everyone's reach for the price of rent. The floor is
    // checked before the age, because a small order never earns the right.
    if blocking_min_size != 0 && node.base_asset_amount < blocking_min_size {
        return Settleable::SteppedOver;
    }

    // The age runs from the slot the order became matchable, not from placement.
    // An order inside its activation delay is invisible to every reader, so a
    // caller cannot have carried its owner. Measuring from placement would let an
    // auction order arrive already past the window.
    if slot.saturating_sub(node.activation_slot) <= grace_slots as u64 {
        return Settleable::SteppedOver;
    }

    Settleable::Withheld
}

/// One user's remaining room in the current sweep.
#[derive(Clone, Copy)]
struct UserRoom {
    /// Index into the caller's user set.
    index: u8,
    /// Quote the user may still lose on the swept side. `u64::MAX` is unbounded.
    budget: u64,
    /// Base the book may still fill against this user's reduce-only orders on
    /// the swept side. `u64::MAX` means the user carries no reduce-only cap.
    cover: u64,
}

/// `execute` writes one balance-change record per user and stops when the next will
/// not fit, so `quote` counts the same way and stops in the same place. Membership
/// is a bitmap over set positions, because a table of 34-byte refs does not fit
/// this frame.
struct DistinctUsers {
    seen: [u8; USER_EXCLUSION_BITMAP_BYTES],
    count: usize,
}

impl DistinctUsers {
    fn new() -> Self {
        DistinctUsers {
            seen: [0u8; USER_EXCLUSION_BITMAP_BYTES],
            count: 0,
        }
    }

    /// Records the owner at `index` and reports whether the walk may go on. An
    /// owner already counted is free. A new one past `max` refuses the walk.
    fn admit(&mut self, index: Option<usize>, max: usize) -> bool {
        let Some(index) = index.filter(|index| *index < USER_SET_CAPACITY) else {
            return true;
        };
        let (byte, bit) = (index / 8, 1u8 << (index % 8));
        if self.seen[byte] & bit != 0 {
            return true;
        }

        if self.count == max {
            return false;
        }

        self.seen[byte] |= bit;
        self.count += 1;
        true
    }
}

/// The caller's per-user budgets for one walk, spent as the walk fills. A budget
/// is quote the user may lose, not base it may take, because only this walk knows
/// the price each order fills at. `quote` and `execute` spend it in the same
/// place, so a ladder never promises depth the fill would decline.
struct UserBudget {
    /// Indices into the caller's set, not copies of the refs. A ref is 34 bytes,
    /// and copies overflowed the 4 KB SBF stack this walk's frame sits in.
    excluded: [u8; USER_EXCLUSION_BITMAP_BYTES],
    any_excluded: bool,
    /// Per-user room for the users that have some room.
    entries: [UserRoom; USER_CAPS_CAPACITY],
    len: usize,
    /// The side these orders rest on, which decides which way a price has to
    /// move for the fill to cost their owner anything.
    side: Side,
    reference_price: u64,
}

impl UserBudget {
    fn new(caps: &UserCapsV0, side: Side, reference_price: i64) -> Self {
        let mut budget = UserBudget {
            excluded: caps.excluded,
            any_excluded: caps.any_excluded(),
            entries: [UserRoom {
                index: 0,
                budget: 0,
                cover: u64::MAX,
            }; USER_CAPS_CAPACITY],

            len: 0,
            side,
            reference_price: reference_price.max(0) as u64,
        };

        for cap in caps.as_slice() {
            budget.entries[budget.len] = UserRoom {
                index: cap.index,
                budget: cap.quote_cap,
                cover: cap.base_cap,
            };

            budget.len += 1;
        }

        budget
    }

    /// What one base of an order at `price` costs its owner. That is the
    /// distance the fill puts between what they pay and what the mark says they
    /// hold. A price in the owner's favour costs nothing.
    fn cost_per_base(&self, price: u64) -> u64 {
        match self.side {
            Side::Bid => price.saturating_sub(self.reference_price),
            Side::Ask => self.reference_price.saturating_sub(price),
        }
    }

    /// How much of `want` the user at `index` may still take from an order at
    /// `price`, spending their budget for it.
    ///
    /// `index` is the position the membership scan already resolved, so the
    /// bitmap costs a bit test rather than a second walk of the set.
    ///
    /// `reduce_only` is the resting order's own flag. A reduce-only order fills
    /// only against an authoritative `base_cover` on a named cap entry. The book
    /// cannot see a position, so no caps at all, an unnamed owner, and an
    /// unconstrained owner all leave the order uncovered, and it does not fill.
    /// A non-reduce-only order ignores the cover.
    fn allow(&mut self, index: Option<usize>, want: u64, price: u64, reduce_only: bool) -> u64 {
        // The uncovered fast paths are no caps at all, and an owner the set does
        // not name. Both are free for an ordinary order and refused for a
        // reduce-only one.
        if !self.any_excluded && self.len == 0 {
            return if reduce_only { 0 } else { want };
        }

        let Some(index) = index else {
            return if reduce_only { 0 } else { want };
        };

        if index < USER_SET_CAPACITY && self.excluded[index / 8] & (1 << (index % 8)) != 0 {
            return 0;
        }

        for slot in 0..self.len {
            let UserRoom {
                index: named,
                budget: room,
                cover,
            } = self.entries[slot];

            if named as usize != index {
                continue;
            }

            let cost_per_base = self.cost_per_base(price);
            // The quote budget does not bind when it is unbounded, or when the
            // cost is zero. Otherwise the base rounds down and the spend rounds
            // up, so a long run cannot creep past the budget one remainder at a
            // time.
            let budget_allowed = if room == u64::MAX || cost_per_base == 0 {
                want
            } else {
                let affordable = (room as u128 * BASE_PRECISION as u128) / cost_per_base as u128;
                want.min(affordable.min(u64::MAX as u128) as u64)
            };

            // The authoritative base cover binds only a reduce-only order.
            let allowed = if reduce_only {
                budget_allowed.min(cover)
            } else {
                budget_allowed
            };

            // Spend the quote budget for what was actually taken, and draw the
            // cover down by the same base for a reduce-only fill.
            if room != u64::MAX && cost_per_base != 0 {
                let spent =
                    (allowed as u128 * cost_per_base as u128).div_ceil(BASE_PRECISION as u128);
                self.entries[slot].budget = room.saturating_sub(spent.min(u64::MAX as u128) as u64);
            }

            if reduce_only {
                self.entries[slot].cover = cover.saturating_sub(allowed);
            }

            return allowed;
        }

        // An owner named in the set but carrying no cap entry is
        // unconstrained. It is uncovered, so a reduce-only order does not fill.
        if reduce_only {
            0
        } else {
            want
        }
    }
}

/// What a crossing taker remainder has claimed on the side being read, and what
/// `include_taker_origin_reservations` lets one caller reach. It is unrelated to
/// the margin a maker reserves at placement.
///
/// The book skips claimed units rather than refusing the call, and a claim lapses
/// [`ClobHeaderV0::reservation_grace_slots`] past the activation slot. Every read
/// of a side runs this, so no two disagree. See `docs/taker-remainder-auction.md`.
pub(crate) struct CrossReservation {
    /// The side being read. A claimant rests on the other one and takes this
    /// side as its cover.
    cover: Side,
    slot: u64,
    now: i64,
    /// See [`ClobHeaderV0::reservation_grace_slots`].
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
        cover: Side,
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
    side: Side,
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

/// Append one wincode `PriceLevel` to the quote response, re-checking on the way
/// out what the wire type promises: best-price-first, and every level fillable.
///
/// The router picks a quoter by exactly these numbers, so a zero price, a zero
/// size, or a level improving on the one before it would win a waterfall the book
/// cannot honour. A book holding its invariants produces none of the three, and
/// this check says so.
fn write_level(
    book: &mut ClobMarketV0,
    writer: &mut QuoteWriter,
    side: Side,
    last_written_price: &mut Option<u64>,
    level: PriceLevel,
) -> Result<()> {
    require!(
        level.price != 0 && level.size != 0,
        ClobError::InvalidResponseLevel
    );
    require!(
        last_written_price.is_none_or(|before| side.is_worse_price(level.price, before)),
        ClobError::InvalidResponseLevel
    );

    writer
        .push_level(&mut book.response, level)
        .map_err(ClobError::from)?;
    *last_written_price = Some(level.price);
    Ok(())
}

/// The same self-check for a fill entering the execute response. The price is not
/// on the wire, but execute values the fill as `price * base` over the same
/// best-first walk, so the ordering still has to hold. Equal consecutive prices are
/// expected, because one level is contiguous orders and each is its own fill.
fn check_fill_price(side: Side, filled: Option<u64>, price: u64, take: u64) -> Result<()> {
    require!(price != 0 && take != 0, ClobError::InvalidResponseLevel);
    require!(
        filled.is_none_or(|before| !side.is_worse_price(before, price)),
        ClobError::InvalidResponseLevel
    );

    Ok(())
}

/// Postcondition for an operation that removed exactly one order. The list
/// closed over the gap, or the side's endpoint moved when the removed order was
/// an endpoint. The slot is zeroed at the head of the free list, so its handle
/// can never verify again. Execute removes up to `max_execute_fills` nodes in
/// one call, so it uses the O(1) [`ClobBook::validate_book`] instead of paying
/// this per removal.
fn validate_single_removal(book: &ClobMarketV0, removed: &OrderNodeV0, index: u32) -> Result<()> {
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
fn alloc_node(book: &mut ClobMarketV0) -> Result<u32> {
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
fn insert_order(
    book: &mut ClobMarketV0,
    side: Side,
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
fn remove_order(book: &mut ClobMarketV0, index: u32) -> Result<()> {
    let node = unlink_order(book, index)?;
    // The order that just left may have been the one holding the expiry hint.
    // A walk is owed only then. An ordinary removal costs one comparison.
    book.repair_expiry_hint_for(&node)?;
    Ok(())
}

/// Whether `node` is holding the expiry hint, so a caller batching removals
/// knows whether it owes a repair once it is done.
fn holds_expiry_hint(book: &ClobMarketV0, node: &OrderNodeV0) -> bool {
    node.max_ts != 0 && node.max_ts <= book.next_expiry_ts
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
fn unlink_order(book: &mut ClobMarketV0, index: u32) -> Result<OrderNodeV0> {
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
