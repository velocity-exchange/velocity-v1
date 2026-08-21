//! The order book: a node arena with a free list plus two best-first sorted
//! intrusive doubly-linked lists, one per side, over the market slab defined
//! in [`crate::state`].
//!
//! ## Arena access is centralized
//!
//! Every read or write of an arena slot goes through [`NodeArena`]
//! ([`NodeArena::read_node`], [`NodeArena::write_node`],
//! [`NodeArena::update_node`], and the two sentinel-tolerant link setters).
//! Each validates the index against the live arena before touching memory,
//! so a corrupted or hostile link produces [`ClobError::NodeIndexOutOfRange`]
//! instead of a read/write outside the arena. No code in the crate indexes
//! the slab directly, and link surgery is confined to [`insert_order`] /
//! [`remove_order`]. The one other way a slot is written is `Slab::try_push`,
//! which appends within the tail it owns and is used only to lay out a fresh
//! (or freshly grown) arena.
//!
//! ## Traversal
//!
//! Both walks over a side (the placement scan, and the quote/execute sweep)
//! go through [`walk_side`], which validates each hop and refuses to walk
//! further than the arena can hold — a corrupt list cannot spin forever.
//!
//! ## Invariants
//!
//! Every mutating operation ends by re-checking what it wrote:
//! [`ClobBook::validate_book`] covers the O(1) header/endpoint invariants
//! (counts sum to capacity, endpoints are live nodes of the right side with
//! null outer links, free head agrees with free count) and each operation
//! adds its own postcondition. The exhaustive O(n) version — full list walk,
//! price ordering, every slot accounted for — runs in the unit tests after
//! every operation rather than on-chain.
//!
//! Quote and execute additionally check what they are about to *report*: every
//! level or fill carries a nonzero price and size, and the sequence runs
//! best-price-first for the side. A response that broke either would be worth
//! more to the router than the book can honour — a zero-priced level wins any
//! routing waterfall outright — so it fails the instruction rather than ship.
//! See [`write_level`] and [`check_fill_price`].
//!
//! One order neither instruction will trade: one marked
//! [`OrderBitFlag::TakerOrigin`] while a counterparty on the other side crosses
//! it — see [`TakerOriginGate`], which both of them ask, so the depth quote
//! publishes is always depth execute can deliver. Both simply pass over it, the
//! way they pass over an expired or not-yet-activated order, so the rest of the
//! side stays tradeable. Every order still fills at its own stored price; the
//! book neither reprices a cross nor resolves one, it only declines to sell the
//! taker's improvement to whoever gets there first, and reports the flag on
//! [`crate::state::RemovedOrderV0`] so velocity can settle the cross at the
//! counterparty's price.
//!
//! Execute is also held to its own quote on the way out, by velocity: the
//! response's total quote must be the notional of these same orders at the
//! prices `quote` published, to within the one unavoidable division. That is
//! why fills are priced by differencing a running total rather than rounded
//! one at a time (see `quote_size` in [`ClobBook::execute`]).

use {
    crate::{
        error::ClobError,
        events::FillSlimV0,
        state::{
            response_pointer, CancelAllOutcome, CancelSidesExt, CancelSidesV0,
            CancelledRemainderV0, ClobDirectionExt, ClobHeaderV0, ClobMarketV0, ClobSideExt,
            CompletedOrderV0, Direction, ExecuteOutcome, MarketConfigV0, OrderBitFlag, OrderNodeV0,
            OrderRefV0, PlaceOrderParams, PriceLevel, RemovedOrder, ResponsePointerV0, Side,
            UserBalanceChangeV0, UserCapsV0, UserRefV0, BASE_PRECISION, CANCEL_ALL_ORDERS_CEILING,
            EXECUTE_FILLS_CEILING, EXECUTE_USERS_CEILING, QUOTE_LEVELS_CEILING, USER_CAPS_CAPACITY,
            USER_EXCLUSION_BITMAP_BYTES, USER_SET_CAPACITY, ZERO_ADDRESS,
        },
    },
    anchor_lang_v2::{address_eq, prelude::*},
    quoter_spec::{ExecuteWriter, QuoteWriter},
};

/// Null link sentinel. The account zero-inits and 0 is a valid node index,
/// so `initialize` must thread the free list before the book is usable.
pub const NIL: u32 = u32::MAX;

/// Two conditions that must agree: either both hold or neither does.
///
/// Most of the book's structural invariants have this shape — an empty side
/// has a `NIL` best link *and* a zero count, and either one alone is a
/// corrupt book. Written directly the condition becomes `(a == b) == (c == d)`,
/// which reads as a typo for `&&` rather than as a claim about equivalence.
#[inline(always)]
const fn both_or_neither(a: bool, b: bool) -> bool {
    a == b
}

/// Book operations over the market slab. A trait because inherent impls
/// aren't allowed on the foreign `Slab` type.
pub trait ClobBook {
    fn initialize(
        &mut self,
        new_authority: Address,
        new_place_authority: Address,
        config: MarketConfigV0,
    ) -> Result<()>;
    fn place(&mut self, params: PlaceOrderParams) -> Result<OrderRefV0>;
    fn cancel(&mut self, user: UserRefV0, order_ref: OrderRefV0) -> Result<RemovedOrder>;
    fn cancel_all(
        &mut self,
        user: UserRefV0,
        sides: CancelSidesV0,
        removed_ids: &mut dyn FnMut(u64) -> Result<()>,
    ) -> Result<CancelAllOutcome>;
    fn evict_worst(&mut self, side: Side) -> Result<RemovedOrder>;
    fn remove_expired(&mut self, order_ref: OrderRefV0, now: i64) -> Result<RemovedOrder>;
    #[allow(clippy::too_many_arguments)]
    fn quote(
        &mut self,
        direction: Direction,
        size: u64,
        users: &[UserRefV0],
        caps: &UserCapsV0,
        reference_price: i64,
        taker: Option<&UserRefV0>,
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
        slot: u64,
        now: i64,
    ) -> Result<ExecuteOutcome>;
    fn grow_free_list(&mut self) -> Result<()>;

    /// Live order count on `side`.
    fn node_count(&self, side: Side) -> u32;
    /// Head of `side`: the best-priced, oldest order. [`NIL`] when empty.
    fn best(&self, side: Side) -> u32;
    /// Tail of `side`: the worst-priced, youngest order there. [`NIL`] when
    /// empty. Kept so eviction of the worst order is O(1).
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
}

impl BookHeader for ClobMarketV0 {
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
}

/// The whole of the program's arena access. Each method validates the index
/// against the live arena length, so no caller can address a slot that isn't
/// there — the arena is reached only through these five methods.
pub(crate) trait NodeArena {
    /// Copy a node out. Copying rather than borrowing is what lets a
    /// traversal visitor keep mutating the book while it holds the node.
    fn read_node(&self, index: u32) -> Result<OrderNodeV0>;
    fn write_node(&mut self, index: u32, node: OrderNodeV0) -> Result<()>;
    fn update_node(&mut self, index: u32, edit: impl FnOnce(&mut OrderNodeV0)) -> Result<()>;
    /// Point `index`'s successor link at `next`. [`NIL`] for `index` means
    /// "no such neighbour" and is a no-op, so link surgery doesn't repeat
    /// the branch at every call site.
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

/// Whether a book walk continues past the node just visited.
pub(crate) enum Walk {
    Continue,
    Stop,
}

/// Walk one side from the best of book outward, handing each node to
/// `visit` by copy along with its arena index.
///
/// The successor link is read *before* `visit` runs, so a visitor may unlink
/// the node it is looking at (execute does) without losing its place. Every
/// hop is bounds-validated by [`NodeArena::read_node`], and the walk refuses
/// to take more hops than the arena has slots — a list corrupted into a
/// cycle errors out instead of burning the compute budget.
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

impl ClobBook for ClobMarketV0 {
    /// Exhaustive header destructure (the zero-copy `set_inner`): adding a
    /// header field without initializing it here is a compile error. Then
    /// fill the tail to capacity, threading the free list.
    fn initialize(
        &mut self,
        new_authority: Address,
        new_place_authority: Address,
        config: MarketConfigV0,
    ) -> Result<()> {
        let cap = self.capacity() as u32;
        require!(cap >= 2, ClobError::InvalidCapacity);
        require!(config.base_precision != 0, ClobError::InvalidConfig);
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
            padding,
            response,
        } = &mut **self;
        *authority = new_authority;
        *place_authority = new_place_authority;
        *order_tick_size = config.order_tick_size;
        *order_step_size = config.order_step_size;
        *min_order_size = config.min_order_size;
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
        padding.fill(0);
        response.fill(0);

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

    /// Insert with price-time priority: walk from the best of book past every
    /// order at an equal-or-better price, so the new order queues behind its
    /// own level.
    ///
    /// A full side (half the arena each) rejects every placement, even
    /// better-priced ones: eviction is crank-mediated through velocity (see
    /// [`Self::evict_worst`]) so the evicted maker's margin aggregates stay
    /// exact, and the soft-cap buffer exists so the hard cap is an ops
    /// failure, not a normal state.
    fn place(&mut self, params: PlaceOrderParams) -> Result<OrderRefV0> {
        let PlaceOrderParams {
            side,
            price,
            base_asset_amount,
            user,
            activation_slot,
            placed_slot,
            max_ts,
            taker_origin,
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
                bit_flags: OrderBitFlag::Open as u8
                    | side.side_bit()
                    | OrderBitFlag::TakerOrigin.bit_if(taker_origin),
                padding0: 0,
                sub_account_id: user.sub_account_id,
                padding: [0; 4],
            },
        )?;
        insert_order(self, side, index, prev, next)?;
        self.set_node_count(
            side,
            count_before
                .checked_add(1)
                .ok_or(ClobError::BookInvariantViolated)?,
        )?;

        // The node is spliced where the scan said it should be, and the
        // side's endpoints followed.
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
        self.validate_book()?;

        Ok(OrderRefV0 {
            node_index: index,
            order_id,
        })
    }

    /// Fails closed on a stale hint: node out of range, free, or holding a
    /// different order. `user` must own the order.
    fn cancel(&mut self, user: UserRefV0, order_ref: OrderRefV0) -> Result<RemovedOrder> {
        let node = live_order(self, order_ref)?;
        require!(node.user_ref() == user, ClobError::OrderUserMismatch);
        let removed = removed_order(&node);
        remove_order(self, order_ref.node_index)?;
        validate_single_removal(self, &node, order_ref.node_index)?;
        self.validate_book()?;
        Ok(removed)
    }

    /// Withdraw every order `user` holds on the requested sides in one pass.
    ///
    /// The book has no per-user index — user identity lives inline on the node
    /// and there is deliberately no seat table (see [`OrderNodeV0`]) — so this
    /// is a full walk of each requested side, O(orders on the side) rather than
    /// O(the user's orders). That is still the cheap direction: the alternative
    /// a maker has is one instruction per order, and the walk costs a fraction
    /// of one CPI round trip per hop.
    ///
    /// Removals are capped at [`CANCEL_ALL_ORDERS_CEILING`] per call, and
    /// [`CancelAllOutcome::exhaustive`] reports whether the walk reached the end
    /// of every requested side. It is false only when the cap stopped it, which
    /// is the one case where orders of this user are still resting — the
    /// caller's contract is to repeat the call until it comes back true.
    ///
    /// Each removed order's id goes to `removed_ids` as the walk frees it, in
    /// book order per side. The handler streams those into the cancel record's
    /// log buffer; taking a sink rather than returning a `Vec` keeps this off
    /// the heap on a path that can touch a hundred orders.
    fn cancel_all(
        &mut self,
        user: UserRefV0,
        sides: CancelSidesV0,
        removed_ids: &mut dyn FnMut(u64) -> Result<()>,
    ) -> Result<CancelAllOutcome> {
        let ceiling = CANCEL_ALL_ORDERS_CEILING as u32;
        let mut outcome = CancelAllOutcome {
            exhaustive: true,
            ..Default::default()
        };
        // Running across both sides, so the cap bounds the call rather than
        // each side of it.
        let mut total_removed = 0u32;
        for side in sides.sides().iter().copied() {
            if !outcome.exhaustive {
                break;
            }
            let count_before = self.node_count(side);
            let mut base_removed = 0u64;
            let mut orders_removed = 0u32;
            walk_side(self, side, |book, index, node| {
                if node.user_ref() != user {
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
                removed_ids(node.order_id)?;
                remove_order(book, index)?;
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
                }
                Side::Ask => {
                    outcome.ask_base_asset_amount = base_removed;
                    outcome.ask_orders = orders_removed;
                }
            }
        }
        self.validate_book()?;
        Ok(outcome)
    }

    /// Crank-mediated eviction: only the side's tail (worst price, youngest
    /// there), and only while the side holds at least
    /// `evict_threshold_per_side` orders — the crank works the soft-cap
    /// buffer down so placements never hit the hard cap. Velocity is the
    /// caller and loads the evicted maker's `User`, so aggregates stay exact.
    fn evict_worst(&mut self, side: Side) -> Result<RemovedOrder> {
        let count = self.node_count(side);
        require!(
            count > 0 && count >= self.evict_threshold_per_side,
            ClobError::BelowEvictThreshold
        );
        let tail = self.worst(side);
        let node = self.read_node(tail)?;
        require!(
            node.is_bit_flag_set(OrderBitFlag::Open) && node.side() == side,
            ClobError::BookInvariantViolated
        );
        let removed = removed_order(&node);
        remove_order(self, tail)?;

        // The tail moved off the evicted slot (to the previous order, or to
        // `NIL` if that was the last one) and the slot is free.
        require!(
            self.worst(side) != tail && self.worst(side) == node.prev,
            ClobError::BookInvariantViolated
        );
        require!(
            self.node_count(side) == count - 1,
            ClobError::BookInvariantViolated
        );
        validate_single_removal(self, &node, tail)?;
        self.validate_book()?;
        Ok(removed)
    }

    /// Crank-mediated expiry reclamation (execute only skips expired orders;
    /// removal without the maker's `User` loaded is exactly the aggregate
    /// leak this design eliminates). Fails closed on a stale hint.
    fn remove_expired(&mut self, order_ref: OrderRefV0, now: i64) -> Result<RemovedOrder> {
        let node = live_order(self, order_ref)?;
        require!(node.is_expired(now), ClobError::OrderNotExpired);
        let removed = removed_order(&node);
        remove_order(self, order_ref.node_index)?;
        validate_single_removal(self, &node, order_ref.node_index)?;
        self.validate_book()?;
        Ok(removed)
    }

    /// Aggregate the levels a taker of `direction`/`size` would clear,
    /// best-first, capped at the market's `max_quote_levels`, and stream them
    /// into the response region as borsh [`crate::state::QuoteResponseV0`].
    /// Skips expired orders, orders still inside their activation delay, and
    /// the taker's own orders (self-trade prevention — same rule as
    /// [`Self::execute`], shared through [`is_matchable`]). Applies the same
    /// unknown-user grace rule as execute so the router's split math matches
    /// what execute will deliver.
    ///
    /// Also skips an order [`TakerOriginGate`] holds back — a taker remainder a
    /// counterparty currently crosses — which [`Self::execute`] skips too, so the
    /// depth published here is always depth the fill can deliver.
    #[allow(clippy::too_many_arguments)]
    fn quote(
        &mut self,
        direction: Direction,
        size: u64,
        users: &[UserRefV0],
        caps: &UserCapsV0,
        reference_price: i64,
        taker: Option<&UserRefV0>,
        slot: u64,
        now: i64,
    ) -> Result<ResponsePointerV0> {
        let side = direction.book_side();
        let max_levels = self.max_quote_levels.min(QUOTE_LEVELS_CEILING) as usize;
        // A quote promises what `execute` can deliver, so it spends `execute`'s
        // budget as it walks — not just its own level cap. The two are counted
        // in different units: a level aggregates however many orders sit at one
        // price, so a ladder capped only on levels can stand on more orders
        // than one execute is allowed to touch. Quoting depth execute would
        // then decline is a quote that lied, and the caller cannot tell.
        let max_fills = self.max_execute_fills.min(EXECUTE_FILLS_CEILING) as usize;
        let max_users = self.max_execute_users.min(EXECUTE_USERS_CEILING) as usize;
        let grace_slots = self.unknown_user_grace_slots;
        let mut writer = QuoteWriter::new();
        let mut levels = 0usize;
        // Orders promised so far, against `execute`'s budget rather than this
        // walk's own.
        let mut fills = 0usize;
        // The level being accumulated, written out only once the price
        // changes (or the walk ends) — orders at one price are contiguous, so
        // a level costs one 16-byte append however many orders it holds.
        let mut open: Option<(u64, u64)> = None;
        // Price of the last level actually written, for the best-first check
        // in `write_level`.
        let mut written: Option<u64> = None;
        let mut remaining = size;
        let mut gate = TakerOriginGate::new(side, slot, now);
        // Spent by this walk exactly as `execute` spends it, so the ladder
        // stands only on orders the fill can settle.
        let mut budget = UserBudget::new(caps, side, reference_price);
        // The other half of that budget: `execute` writes one balance-change
        // record per distinct user filled and stops when the next one would
        // not fit, so a quote that walked past that point would promise depth
        // the fill declines. Counted the same way and stopped in the same
        // place, by set index rather than by user ref — a ref is 34 bytes and
        // this frame has no room for a table of them.
        //
        // Bounding the walk rather than the caller's set is what keeps a busy
        // book fillable. A set wider than this cap is not an error: the extra
        // users are ordinary loaded accounts (makers on other venues, a
        // referrer) that this book may never fill, and refusing them would
        // leave a book holding more distinct makers than the cap with no
        // assembly that works at all — pass them and the call is refused, omit
        // one and its aged order is a stale set.
        //
        // An unrestricted (discovery) quote has no set to index and stays
        // advisory, exactly as it was.
        let mut seen_users = [0u8; USER_EXCLUSION_BITMAP_BYTES];
        let mut distinct_users = 0usize;
        // Where the walk gave up for want of a loaded user, if it did.
        let mut withheld: Option<PriceLevel> = None;

        walk_side(self, side, |book, _, node| {
            // Mirrors `execute`'s own stop, in the same place in the walk, so
            // the ladder ends exactly where the fill would.
            if remaining == 0 || fills == max_fills {
                return Ok(Walk::Stop);
            }
            // Reasons to pass over an order, cheapest first: the order's own
            // state, then the crossed-remainder gate, then whether the caller
            // can settle for its owner. Settleability comes last so an order
            // the walk would skip anyway never costs the caller two accounts.
            if !is_matchable(node, taker, slot, now) || gate.skips(book, node)? {
                return Ok(Walk::Continue);
            }
            let owner = users.iter().position(|u| *u == node.user_ref());
            match settleable(users, owner, node, grace_slots, slot) {
                Settleable::Yes => {}
                Settleable::TooFresh => return Ok(Walk::Continue),
                Settleable::Withheld => {
                    withheld = Some(PriceLevel {
                        price: node.price,
                        size: node.base_asset_amount,
                    });
                    return Ok(Walk::Stop);
                }
            }
            let take = budget.allow(owner, remaining.min(node.base_asset_amount), node.price);
            if take == 0 {
                // Out of room: this owner's remaining orders cannot settle,
                // and the depth behind them still can.
                return Ok(Walk::Continue);
            }
            if let Some(index) = owner.filter(|index| *index < USER_SET_CAPACITY) {
                let (byte, bit) = (index / 8, 1u8 << (index % 8));
                if seen_users[byte] & bit == 0 {
                    if distinct_users == max_users {
                        return Ok(Walk::Stop);
                    }
                    seen_users[byte] |= bit;
                    distinct_users += 1;
                }
            }
            fills += 1;
            match open {
                Some((price, aggregate)) if price == node.price => {
                    open = Some((
                        price,
                        aggregate.checked_add(take).ok_or(ClobError::MathError)?,
                    ));
                }
                _ => {
                    if levels == max_levels {
                        return Ok(Walk::Stop);
                    }
                    if let Some((price, aggregate)) = open {
                        write_level(book, &mut writer, side, &mut written, price, aggregate)?;
                    }
                    open = Some((node.price, take));
                    levels += 1;
                }
            }
            remaining -= take;
            Ok(if remaining == 0 {
                Walk::Stop
            } else {
                Walk::Continue
            })
        })?;
        if let Some((price, aggregate)) = open {
            write_level(self, &mut writer, side, &mut written, price, aggregate)?;
        }

        // `finish` backfills the ladder's count and writes the withheld report
        // behind it, which is the shape `QuoteResponseV0` declares.
        let len = writer
            .finish(&mut self.response, withheld.unwrap_or_default())
            .map_err(ClobError::from)?;
        Ok(response_pointer(len))
    }

    /// Consume matchable orders best-first, removing filled orders and
    /// streaming each maker's share into the response region as borsh
    /// [`crate::state::ExecuteResponseV0`] for velocity to apply. Expired
    /// orders are skipped, never removed here — reclamation goes through
    /// [`Self::remove_expired`] so the maker's aggregates update. A partial
    /// fill that leaves a remainder below `min_order_size` culls the order
    /// (dust can't hold an arena slot); the cull rides the wire response
    /// since that maker was just filled and is therefore loaded. Orders
    /// whose user is outside the caller's set are skipped inside the grace
    /// window and end the walk past it (see [`settleable`]); the taker's own
    /// orders are skipped unconditionally (self-trade prevention). No price bound: the router already chose this quoter's
    /// allocation from its quote.
    ///
    /// Fills merge by user: the records already written into the response
    /// *are* the accumulator, so a repeat maker patches their record's
    /// totals in place instead of a heap `Vec` of balance changes.
    ///
    /// One more order it passes over, alongside the expired and the
    /// not-yet-activated: a taker-origin order that has a live crossing
    /// counterparty on the other side. See [`TakerOriginGate`], which
    /// [`Self::quote`] reads too so the two never disagree about what is
    /// takeable.
    #[allow(clippy::too_many_arguments)]
    fn execute(
        &mut self,
        direction: Direction,
        size: u64,
        users: &[UserRefV0],
        caps: &UserCapsV0,
        reference_price: i64,
        taker: Option<&UserRefV0>,
        slot: u64,
        now: i64,
    ) -> Result<ExecuteOutcome> {
        let side = direction.book_side();
        let max_fills = self.max_execute_fills.min(EXECUTE_FILLS_CEILING) as usize;
        let max_users = self.max_execute_users.min(EXECUTE_USERS_CEILING) as usize;
        let grace_slots = self.unknown_user_grace_slots;
        let min_order_size = self.min_order_size;
        let base_precision = self.base_precision.max(1) as u128;
        let count_before = self.node_count(side);

        let mut writer = ExecuteWriter::new();
        // Only the event needs per-order detail (the response merges by
        // user), so this is the one collection execute still builds.
        let mut fills: Vec<FillSlimV0> = Vec::with_capacity(max_fills);
        let mut cancelled: Option<CancelledRemainderV0> = None;
        // Bounded by the same thing `fills` is — a fill consumes at most one
        // order — so it reserves the same, rather than doubling its way up
        // beside a sibling that does not.
        let mut completed: Vec<CompletedOrderV0> = Vec::with_capacity(max_fills);
        let mut removals = 0u32;
        let mut remaining = size;
        // Price of the last order consumed, for the best-first check in
        // `check_fill_price`.
        let mut filled: Option<u64> = None;
        // The sweep's notional so far (before the divide) and the quote already
        // attributed to earlier fills. See `quote_size` below.
        let mut swept = 0u128;
        let mut paid = 0u128;
        let mut gate = TakerOriginGate::new(side, slot, now);
        let mut budget = UserBudget::new(caps, side, reference_price);

        walk_side(self, side, |book, index, node| {
            if remaining == 0 || fills.len() == max_fills {
                return Ok(Walk::Stop);
            }
            // The same order of reasons `quote` applies, so the fill ends
            // exactly where the ladder did.
            if !is_matchable(node, taker, slot, now) || gate.skips(book, node)? {
                return Ok(Walk::Continue);
            }
            let user = node.user_ref();
            let owner = users.iter().position(|u| *u == user);
            match settleable(users, owner, node, grace_slots, slot) {
                Settleable::Yes => {}
                Settleable::TooFresh => return Ok(Walk::Continue),
                Settleable::Withheld => return Ok(Walk::Stop),
            }
            let take = budget.allow(owner, remaining.min(node.base_asset_amount), node.price);
            if take == 0 {
                return Ok(Walk::Continue);
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
            check_fill_price(side, filled, node.price, take)?;
            filled = Some(node.price);
            // Each fill's quote is the *difference of running floors*, not the
            // floor of its own notional: the sweep's total then comes out as
            // the floor of the whole sweep's notional rather than the sum of
            // per-fill floors, which can sit a unit lower per fill. Velocity
            // holds the total to the prices this book quoted for these same
            // orders moments earlier and admits exactly that one rounding, and
            // the dust a per-fill truncation loses would come out of the
            // makers.
            swept = swept
                .checked_add(
                    (node.price as u128)
                        .checked_mul(take as u128)
                        .ok_or(ClobError::MathError)?,
                )
                .ok_or(ClobError::MathError)?;
            let swept_quote = swept / base_precision;
            let quote_size: u64 = (swept_quote - paid)
                .try_into()
                .map_err(|_| ClobError::MathError)?;
            paid = swept_quote;
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
                base_size: take,
            });

            if take == node.base_asset_amount {
                // The id names the change it belongs to and rides a section of
                // its own, written once the changes are done. Growing the
                // record in place would mean shifting every record after it on
                // every consumed order.
                completed.push(CompletedOrderV0 {
                    order_id: node.order_id,
                    change_index,
                    _pad: 0,
                });
                remove_order(book, index)?;
                removals += 1;
            } else {
                let remainder = node.base_asset_amount - take;
                if remainder < min_order_size {
                    // A partial fill only happens once `remaining` runs out,
                    // which ends the walk — so there is at most one cull and
                    // `cancelled` needs no growable storage. Fail loudly if
                    // that ever stops holding rather than dropping a cull
                    // velocity must unwind.
                    require!(cancelled.is_none(), ClobError::BookInvariantViolated);
                    cancelled = Some(CancelledRemainderV0 {
                        order_id: node.order_id,
                        base_asset_amount: remainder,
                        user: node.user_ref(),
                        _pad: [0; 6],
                    });
                    remove_order(book, index)?;
                    removals += 1;
                } else {
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

        // `finish` backfills the change count and writes the two remaining
        // sections in the order `ExecuteResponseV0` declares them.
        let response = response_pointer(
            writer
                .finish(&mut self.response, cancelled.as_slice(), &completed)
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
        self.validate_book()?;

        Ok(ExecuteOutcome {
            response,
            fills,
            cancelled_order_id: cancelled.map(|cull| cull.order_id),
        })
    }

    /// After a capacity grow: push zeroed nodes for the new slots and thread
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

    /// O(1) postcondition for every mutating operation: the three counts
    /// account for the whole arena, the free head agrees with the free
    /// count, and each side's endpoints are live nodes of that side with
    /// null outer links.
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
        [Side::Bid, Side::Ask].into_iter().try_for_each(|side| {
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
        })
    }
}

/// Resolve an order hint to its live node, failing closed when the node is
/// out of range, free, or has been reused for a different order. An
/// out-of-range hint reports as stale rather than as arena corruption: the
/// hint comes from the caller, and a node index that was valid before a
/// shrink is exactly a stale handle.
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
        price: node.price,
        base_asset_amount: node.base_asset_amount,
        side: node.side(),
        taker_origin: node.is_taker_origin(),
    }
}

/// Whether an order can take part in a fill right now. Quote and execute
/// share this so the router's split math can't diverge from what execute
/// delivers.
///
/// Everything here is a property of the order itself or of the caller, so it
/// needs no access to the book. [`TakerOriginGate`] is the other half of the same
/// decision — the reason to pass over an order that depends on what is resting on
/// the *other* side — and both call sites ask the two together.
/// `index` is where this order's owner sits in `users`, resolved once by the
/// caller: membership and the per-user budget both need it, and the set is 48
/// wide, so resolving it twice per order is a walk of the set nobody needs.
fn is_matchable(node: &OrderNodeV0, taker: Option<&UserRefV0>, slot: u64, now: i64) -> bool {
    if node.is_expired(now) || !node.is_active(slot) {
        return false;
    }
    !taker.is_some_and(|t| *t == node.user_ref())
}

/// Whether the caller can settle for this order's owner, and if not, why that
/// matters.
enum Settleable {
    /// The owner is in the caller's set, or the set is unrestricted.
    Yes,
    /// Absent, but the order is younger than the grace window: the caller
    /// cannot be expected to have heard of it yet. Passed over, and the walk
    /// carries on to the depth behind it.
    TooFresh,
    /// Absent, and old enough that the caller had every chance to carry it.
    /// The walk ends here.
    Withheld,
}

/// A transaction locks at most 64 accounts and a maker costs two, so no
/// caller can carry every user a book might hold. Ending the walk is what
/// makes that survivable: the caller fills as deep as the users it brought
/// and the rest stays resting.
///
/// Ending it rather than stepping over it is what keeps the choice honest.
/// The walk is best-first, so stopping at the first missing owner means a
/// caller cannot leave out the maker who would have won and go on to fill the
/// one behind it. It can trade less of the book, never a worse part of it.
///
/// Whether the caller *should* have brought more users is not a question this
/// book can answer — it cannot see the transaction. It reports where it
/// stopped instead, and the caller's own checks decide.
fn settleable(
    users: &[UserRefV0],
    index: Option<usize>,
    node: &OrderNodeV0,
    grace_slots: u32,
    slot: u64,
) -> Settleable {
    if users.is_empty() || index.is_some() {
        return Settleable::Yes;
    }
    if slot.saturating_sub(node.placed_slot) <= grace_slots as u64 {
        return Settleable::TooFresh;
    }
    Settleable::Withheld
}

/// Per-user room for one walk, spent as it goes.
///
/// The caller names how much base each constrained user may still take on the
/// side being swept; anyone unnamed is unconstrained. A user out of room is
/// passed over, and one with less room than an order holds is filled only as
/// far as the room goes.
///
/// The point is that `quote` and `execute` spend the *same* budget in the
/// same place, so a ladder never promises depth standing on a user the fill
/// would then decline. It is not a trust boundary — a book that ignored it
/// would leave its caller exactly where it stands without it — but honouring
/// it is what keeps a maker who cannot be settled against from stopping every
/// fill that reaches them.
///
/// Holds indices into the caller's set rather than copies of the refs it
/// names. A ref is 34 bytes and this lives on a walk's frame inside a 4 KB
/// SBF stack that the fixed-width args have already spent most of — copying
/// them in overflowed it.
/// The caller's per-user budgets, spent as the walk fills.
///
/// A budget is quote the user may lose, not base it may take, because the
/// caller cannot convert one into the other without knowing the price each
/// order fills at. This walk knows those prices, so it does the conversion.
struct UserBudget {
    excluded: [u8; USER_EXCLUSION_BITMAP_BYTES],
    any_excluded: bool,
    /// `(index into the caller's set, quote still available)` for the users
    /// with *some* room. Indices rather than refs: a ref is 34 bytes and this
    /// lives on a walk's frame inside a 4 KB SBF stack the fixed-width args
    /// have already spent most of.
    entries: [(u8, u64); USER_CAPS_CAPACITY],
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
            entries: [(0, 0); USER_CAPS_CAPACITY],
            len: 0,
            side,
            reference_price: reference_price.max(0) as u64,
        };
        for cap in caps.as_slice() {
            budget.entries[budget.len] = (cap.index, cap.budget);
            budget.len += 1;
        }
        budget
    }

    /// What one base of an order at `price` costs its owner: the distance the
    /// fill puts between what they pay and what the mark says they hold. A
    /// price in the owner's favour costs nothing.
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
    fn allow(&mut self, index: Option<usize>, want: u64, price: u64) -> u64 {
        if !self.any_excluded && self.len == 0 {
            return want;
        }
        let Some(index) = index else {
            // Unrestricted set: nobody is named, so nobody is capped.
            return want;
        };
        if index < USER_SET_CAPACITY && self.excluded[index / 8] & (1 << (index % 8)) != 0 {
            return 0;
        }
        for slot in 0..self.len {
            let (named, room) = self.entries[slot];
            if named as usize != index {
                continue;
            }
            if room == u64::MAX {
                return want;
            }
            let cost_per_base = self.cost_per_base(price);
            if cost_per_base == 0 {
                // The fill does not move against this owner, so it draws on
                // nothing and the whole order is available.
                return want;
            }
            // Round the affordable base down and the spend back up, so a long
            // run of orders cannot creep past the budget one remainder at a
            // time.
            let affordable = (room as u128 * BASE_PRECISION as u128) / cost_per_base as u128;
            let allowed = want.min(affordable.min(u64::MAX as u128) as u64);
            let spent = (allowed as u128 * cost_per_base as u128).div_ceil(BASE_PRECISION as u128);
            self.entries[slot].1 = room.saturating_sub(spent.min(u64::MAX as u128) as u64);
            return allowed;
        }
        want
    }
}

/// Whether a taker-origin order has to be passed over right now, because a
/// counterparty on the other side crosses it.
///
/// A taker-origin order rests at the worst price its owner agreed to tolerate,
/// and the activation delay before it becomes matchable is an auction: makers
/// line up inside the window, and the best-priced one is meant to get the cross
/// — at *its* price, so the improvement over the resting price goes to the
/// taker. Letting anyone take the order at its own price while that
/// counterparty is standing there hands the improvement to whoever lands a
/// transaction in the activation slot instead, which is the latency race the
/// window exists to replace with a price race. So the order is not available to
/// a taker until the cross is resolved.
///
/// **A skip, not a rejection.** The order comes out of the book's matchable set
/// while it is crossed, exactly as an expired or not-yet-activated one does: a
/// taker sweeping past it fills the depth behind it instead. That protects the
/// remainder just as completely — it still cannot be taken at its limit — while
/// leaving the rest of the side tradeable, which matters because a remainder
/// rests at a slippage bound and therefore usually near the front, so failing
/// the call would take the whole side dark with it for as long as the cross
/// stood. Failing was the original shape and
/// [`ClobError::TakerOriginCrossPending`] is its deprecated remnant.
///
/// **[`ClobBook::quote`] and [`ClobBook::execute`] have to decide this with the
/// same predicate, which is why it is a type rather than a condition written
/// twice.** Both skip the order, so the depth quote publishes is depth execute
/// really can deliver. Were the two to disagree, a taker that quoted honestly,
/// was allocated the difference by the router, and then executed would get a
/// failed transaction through no fault of its own — velocity binds the execute
/// to the quoted prefix, so there is nothing it can do about a shortfall after
/// the fact. Any future change to what the gate skips has to land on both
/// instructions at once, and sharing the predicate is what makes that automatic
/// instead of remembered.
///
/// Scoped to the order being tested, not to the book: an ordinary maker×maker
/// cross is nobody's improvement to steal and must not cost takers anything, and
/// the counterparty itself is never skipped — it is an ordinary maker, and
/// consuming it is the fill velocity's cross resolution runs.
struct TakerOriginGate {
    /// The side holding the orders being tested: the one a taker of this
    /// direction consumes.
    consumed: Side,
    slot: u64,
    now: i64,
    /// Best price on the other side that could match this slot, resolved on
    /// first need and then reused for the rest of the walk. Hoisted out of the
    /// per-order path in the sense that matters — one lookup answers every order
    /// — and it is *correct* to hold it, because the other side cannot change
    /// while a walk of `consumed` is in flight. Resolved lazily rather than
    /// before the walk because eager resolution inlines a second `walk_side`
    /// into `execute`'s prologue, and the spills that costs its frame measured
    /// far worse than the lookup itself: see the CU benchmarks in
    /// `tests/clob_tests.rs`.
    counterparty: Option<Option<u64>>,
}

impl TakerOriginGate {
    fn new(consumed: Side, slot: u64, now: i64) -> Self {
        Self {
            consumed,
            slot,
            now,
            counterparty: None,
        }
    }

    /// The one question both instructions ask of each order they are about to
    /// trade. Quote asks it once per level across a whole-side walk, so the
    /// answer for an order that is not taker-origin at all — every order on an
    /// ordinary book — stays a bit test at the call site, and everything behind
    /// it is out of line.
    #[inline(always)]
    fn skips(&mut self, book: &mut ClobMarketV0, node: &OrderNodeV0) -> Result<bool> {
        if !node.is_taker_origin() {
            return Ok(false);
        }
        self.crossed(book, node.price)
    }

    #[inline(never)]
    fn crossed(&mut self, book: &mut ClobMarketV0, price: u64) -> Result<bool> {
        let counterparty = match self.counterparty {
            Some(cached) => cached,
            None => {
                let resolved =
                    best_actionable_price(book, self.consumed.opposite(), self.slot, self.now)?;
                self.counterparty = Some(resolved);
                resolved
            }
        };
        Ok(counterparty.is_some_and(|opposite| self.consumed.is_crossed_by(price, opposite)))
    }
}

/// Price of the best order on `side` that could be matched this slot at all.
///
/// Deliberately blind to the caller's user set and its self-trade exclusion,
/// which say whether *this* caller may fill an order, not whether the order is
/// a live counterparty. An order still inside its activation delay (or already
/// expired) is skipped, because nothing can match it yet: a cross that involves
/// one is not actionable by anyone, so there is no improvement within reach to
/// protect — and firing on it would freeze the book for the whole auction
/// window, which is precisely when a migrated taker remainder sits unactivated
/// in front of the resting book.
fn best_actionable_price(
    book: &mut ClobMarketV0,
    side: Side,
    slot: u64,
    now: i64,
) -> Result<Option<u64>> {
    let mut best = None;
    walk_side(book, side, |_, _, node| {
        if node.is_expired(now) || !node.is_active(slot) {
            return Ok(Walk::Continue);
        }
        best = Some(node.price);
        Ok(Walk::Stop)
    })?;
    Ok(best)
}

/// Append one borsh `PriceLevel` to the quote response, after re-checking on
/// the way out what the wire type promises: levels are best-price-first and
/// every one is a real, fillable level.
///
/// A response carrying a zero price, a zero size, or a level that improves on
/// the one before it would win a routing waterfall it cannot honour — the
/// router picks a quoter by exactly these numbers — so the instruction fails
/// instead. None of the three is producible by a book that holds its
/// invariants (`place` rejects a zero price or size, and a side is a
/// price-sorted list whose equal-priced orders are contiguous, so aggregation
/// leaves the written prices strictly monotone); this is the check that says
/// so.
fn write_level(
    book: &mut ClobMarketV0,
    writer: &mut QuoteWriter,
    side: Side,
    written: &mut Option<u64>,
    price: u64,
    size: u64,
) -> Result<()> {
    require!(price != 0 && size != 0, ClobError::InvalidResponseLevel);
    require!(
        written.is_none_or(|before| side.is_worse_price(price, before)),
        ClobError::InvalidResponseLevel
    );
    writer
        .push_level(&mut book.response, PriceLevel { price, size })
        .map_err(ClobError::from)?;
    *written = Some(price);
    Ok(())
}

/// The same self-check for a fill entering the execute response. Execute
/// reports per-maker balance changes rather than levels, so the price is not
/// on the wire — but it values the fill (`price × base`), and the sweep is the
/// same best-first walk, so the ordering still has to hold. Equal consecutive
/// prices are expected here: one level is contiguous orders, each its own
/// fill.
fn check_fill_price(side: Side, filled: Option<u64>, price: u64, take: u64) -> Result<()> {
    require!(price != 0 && take != 0, ClobError::InvalidResponseLevel);
    require!(
        filled.is_none_or(|before| !side.is_worse_price(before, price)),
        ClobError::InvalidResponseLevel
    );
    Ok(())
}

/// Postcondition for an operation that removed exactly one order: the list
/// closed over the gap (or the side's endpoint moved, if the order was one),
/// and the slot is zeroed at the head of the free list so its handle can
/// never verify again. Execute removes up to `max_execute_fills` nodes in one
/// call, so it leans on the O(1) [`ClobBook::validate_book`] instead of
/// paying this per removal.
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
/// leaves free arena, so this should never be the binding check — it is here
/// so an exhausted or corrupt free list is a clean error instead of a write
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
fn insert_order(
    book: &mut ClobMarketV0,
    side: Side,
    index: u32,
    prev: u32,
    next: u32,
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
    Ok(())
}

/// Unlink a live order and push the node onto the free list, zeroed so its
/// old order id can never verify again. Every removal path (cancel, evict,
/// expiry reclaim, execute) funnels through here.
///
/// It refuses up front to remove a node that isn't live or whose side count
/// is already zero — a double free would otherwise desynchronize the counts.
/// The structural postcondition is checked once per operation by
/// [`ClobBook::validate_book`] rather than per removal, which matters because
/// execute removes up to `max_execute_fills` nodes in one call: its free-head
/// check lands on this node (removal makes it the head), so "the slot really
/// was freed" is covered there.
fn remove_order(book: &mut ClobMarketV0, index: u32) -> Result<()> {
    let node = book.read_node(index)?;
    require!(
        node.is_bit_flag_set(OrderBitFlag::Open),
        ClobError::BookInvariantViolated
    );
    let side = node.side();
    let count = book.node_count(side);
    require!(count > 0, ClobError::BookInvariantViolated);

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
    Ok(())
}
