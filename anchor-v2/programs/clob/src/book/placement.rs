//! The operations that add, fill and remove single orders: `place`, `cancel`,
//! `cancel_all`, `evict_worst`, `remove_expired` and `fill`.

use {
    super::{
        arena::{
            alloc_node, insert_order, live_order, remove_order, unlink_order,
            validate_single_removal,
        },
        both_or_neither,
        hints::holds_expiry_hint,
        reservation::{evictable_order, is_bound},
        walk::{check_side_count, is_live, walk_side, Walk},
        BookHeader, ClobBook, NodeArena,
    },
    crate::{
        error::ClobError,
        state::{
            CancelAllOutcomeV0, CancelSidesV0, ClobMarketV0, ClobOrderRefV0, FilledOrderV0,
            OrderBitFlag, OrderNodeV0, PlaceOrderParams, RemovedOrderV0, SideV0, UserRefV0,
            CANCEL_ALL_ORDERS_CEILING, NIL, ZERO_ADDRESS,
        },
    },
    anchor_lang::{address_eq, prelude::*},
};

/// Insert with price-time priority. The walk starts at the best of book and
/// passes every order at an equal or better price, so the new order queues
/// behind its own level.
///
/// Each side owns half the arena. A full side rejects every placement, even
/// a better-priced one. Eviction runs through velocity as a crank (see
/// [`evict_worst`]), which keeps the evicted maker's margin
/// aggregates exact. The soft-cap buffer makes the hard cap an operations
/// failure rather than a normal state.
pub(super) fn place(book: &mut ClobMarketV0, params: PlaceOrderParams) -> Result<ClobOrderRefV0> {
    let side = params.side;
    check_order_params(book, &params)?;

    if params.reject_if_crossed {
        require!(
            !crosses_opposite_best(book, side, params.price, params.now)?,
            ClobError::OrderWouldCross
        );
    }

    let per_side = (book.capacity() / 2) as u32;
    let count_before = book.node_count(side);
    require!(count_before < per_side, ClobError::SideAtCapacity);

    let at = insertion_point(book, side, params.price)?;
    let index = alloc_node(book)?;
    let order_id = book.consume_order_id()?;
    book.write_node(index, resting_node(&params, order_id, at))?;
    insert_order(book, side, index, at.prev, at.next, params.taker_origin)?;
    book.set_node_count(
        side,
        count_before
            .checked_add(1)
            .ok_or(ClobError::BookInvariantViolated)?,
    )?;

    validate_placement(book, &params, index, order_id, at)?;
    book.fold_wake_hints(params.max_ts, params.activation_slot, params.placed_slot)?;
    // Placement is the book's most frequent write and it knows the slot, so
    // it is the place that drops an activation hint the chain has passed.
    book.expire_activation_hint(params.placed_slot)?;
    book.validate_book()?;

    Ok(ClobOrderRefV0 {
        node_index: index,
        order_id,
    })
}

/// Where a new order goes on its side: behind `prev`, the last order at an
/// equal or better price, and in front of `next`. [`NIL`] for either means the
/// order lands at that end of the side.
#[derive(Clone, Copy)]
struct InsertionPoint {
    prev: u32,
    next: u32,
}

/// The rules an order must meet before the book holds it.
fn check_order_params(book: &ClobMarketV0, params: &PlaceOrderParams) -> Result<()> {
    let PlaceOrderParams {
        price,
        base_asset_amount,
        user,
        activation_slot,
        placed_slot,
        ..
    } = *params;

    require!(
        price != 0 && base_asset_amount != 0 && !address_eq(&user.authority, &ZERO_ADDRESS),
        ClobError::InvalidOrderParams
    );
    require!(
        placed_slot <= activation_slot,
        ClobError::InvalidOrderParams
    );
    require!(
        base_asset_amount >= book.min_order_size,
        ClobError::OrderTooSmall
    );
    require!(
        price % book.order_tick_size == 0,
        ClobError::PriceNotTickAligned
    );
    require!(
        base_asset_amount % book.order_step_size == 0,
        ClobError::SizeNotStepAligned
    );

    Ok(())
}

/// Whether an order of `side` at `price` would cross the best order on the
/// other side. An order inside its activation delay counts, because it is
/// resting liquidity a moment from now. An expired order does not, or one cheap
/// order could refuse every post-only placement on the other side.
fn crosses_opposite_best(
    book: &mut ClobMarketV0,
    side: SideV0,
    price: u64,
    now: i64,
) -> Result<bool> {
    let mut crossed = false;
    walk_side(book, side.opposite(), |_, _, node| {
        if node.is_expired(now) {
            return Ok(Walk::Continue);
        }

        crossed = side.is_crossed_by(price, node.price);
        Ok(Walk::Stop)
    })?;

    Ok(crossed)
}

fn insertion_point(book: &mut ClobMarketV0, side: SideV0, price: u64) -> Result<InsertionPoint> {
    let mut at = InsertionPoint {
        prev: NIL,
        next: NIL,
    };

    walk_side(book, side, |_, index, node| {
        if side.is_worse_price(node.price, price) {
            at.next = index;
            Ok(Walk::Stop)
        } else {
            at.prev = index;
            Ok(Walk::Continue)
        }
    })?;

    Ok(at)
}

/// The node a placement writes, linked at `at`.
fn resting_node(params: &PlaceOrderParams, order_id: u64, at: InsertionPoint) -> OrderNodeV0 {
    OrderNodeV0 {
        authority: params.user.authority,
        price: params.price,
        base_asset_amount: params.base_asset_amount,
        activation_slot: params.activation_slot,
        placed_slot: params.placed_slot,
        max_ts: params.max_ts,
        order_id,
        prev: at.prev,
        next: at.next,
        // Written by `insert_order` when the order is taker-origin, and never
        // read otherwise.
        taker_origin_prev: NIL,
        taker_origin_next: NIL,
        bit_flags: OrderBitFlag::Open as u8
            | OrderBitFlag::Ask.bit_if(params.side == SideV0::Ask)
            | OrderBitFlag::TakerOrigin.bit_if(params.taker_origin)
            | OrderBitFlag::ReduceOnly.bit_if(params.reduce_only),
        padding0: 0,
        sub_account_id: params.user.sub_account_id,
        client_order_id: params.client_order_id,
    }
}

/// Postcondition for a placement. The node holds the order, and its
/// neighbours or the side's endpoints point at it.
fn validate_placement(
    book: &ClobMarketV0,
    params: &PlaceOrderParams,
    index: u32,
    order_id: u64,
    at: InsertionPoint,
) -> Result<()> {
    let side = params.side;
    let placed = book.read_node(index)?;
    require!(
        placed.order_id == order_id
            && placed.is_bit_flag_set(OrderBitFlag::Open)
            && placed.side() == side
            && placed.is_taker_origin() == params.taker_origin,
        ClobError::BookInvariantViolated
    );
    require!(
        both_or_neither(at.prev == NIL, book.best(side) == index),
        ClobError::BookInvariantViolated
    );
    require!(
        both_or_neither(at.next == NIL, book.worst(side) == index),
        ClobError::BookInvariantViolated
    );

    if at.prev != NIL {
        require!(
            book.read_node(at.prev)?.next == index,
            ClobError::BookInvariantViolated
        );
    }

    if at.next != NIL {
        require!(
            book.read_node(at.next)?.prev == index,
            ClobError::BookInvariantViolated
        );
    }

    Ok(())
}

/// Fails closed on a stale hint. The node may be out of range, free, or
/// hold a different order. `user` must own the order.
///
/// A taker-origin remainder is refused unless `force` while [`is_bound`]
/// holds. See [`ClobError::TakerOriginBound`] for why it binds. The bind does
/// not affect `crank_taker_origin_cross`, which removes an order through
/// `fill` and never through this path.
pub(super) fn cancel(
    book: &mut ClobMarketV0,
    user: UserRefV0,
    order_ref: ClobOrderRefV0,
    slot: u64,
    force: bool,
) -> Result<RemovedOrderV0> {
    let node = live_order(book, order_ref)?;
    require!(node.user_ref() == user, ClobError::OrderUserMismatch);
    require!(
        force || !is_bound(&node, slot, book.reservation_grace_slots),
        ClobError::TakerOriginBound
    );

    let removed = removed_order(&node);
    remove_order(book, order_ref.node_index)?;
    validate_single_removal(book, &node, order_ref.node_index)?;
    book.validate_book()?;
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
/// [`CancelAllOutcomeV0::exhaustive`] reports whether the walk reached the end
/// of every requested side. It is false only when the cap stopped the walk,
/// which is the one case where orders of this user are still resting. The
/// caller must repeat the call until it comes back true.
///
/// Each removed order's id goes to `removed_ids` as the walk frees it, in
/// book order per side. The handler streams those into the cancel record's
/// log buffer. A sink rather than a returned `Vec` keeps this off the heap
/// on a path that can touch a hundred orders.
pub(super) fn cancel_all(
    book: &mut ClobMarketV0,
    user: UserRefV0,
    sides: CancelSidesV0,
    slot: u64,
    force: bool,
    removed_ids: &mut dyn FnMut(u32) -> Result<()>,
) -> Result<CancelAllOutcomeV0> {
    let ceiling = CANCEL_ALL_ORDERS_CEILING as u32;
    let mut outcome = CancelAllOutcomeV0 {
        user,
        ..Default::default()
    };
    let mut capped = false;
    let mut skipped_bound = false;
    // The count runs across both sides, so the cap bounds the call rather than
    // each side of it.
    let mut total_removed = 0u32;
    // One repair for the whole call. A maker's ladder shares one `max_ts`, so
    // repairing per removal re-walks the live orders almost every time and made
    // a full sweep quadratic.
    let mut owes_expiry_repair = false;

    for side in sides.sides().iter().copied() {
        if capped {
            break;
        }

        let count_before = book.node_count(side);
        let mut removed = SideTotals::default();
        walk_side(book, side, |book, index, node| {
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
                capped = true;
                return Ok(Walk::Stop);
            }

            removed.add(node)?;
            total_removed += 1;
            removed_ids(node.client_order_id)?;
            owes_expiry_repair |= holds_expiry_hint(book, node);
            unlink_order(book, index)?;
            Ok(Walk::Continue)
        })?;

        check_side_count(book, side, count_before, removed.orders)?;
        removed.write_into(&mut outcome, side);
    }

    outcome.exhaustive = !capped && !skipped_bound;
    if owes_expiry_repair {
        book.recompute_wake_hints(true, None)?;
    }

    book.validate_book()?;
    Ok(outcome)
}

/// What a `cancel_all` sweep took from one side.
#[derive(Default)]
struct SideTotals {
    base_asset_amount: u64,
    orders: u32,
    reduce_only_orders: u32,
}

impl SideTotals {
    fn add(&mut self, node: &OrderNodeV0) -> Result<()> {
        self.base_asset_amount = self
            .base_asset_amount
            .checked_add(node.base_asset_amount)
            .ok_or(ClobError::MathError)?;
        self.orders += 1;
        self.reduce_only_orders += u32::from(node.is_reduce_only());
        Ok(())
    }

    fn write_into(&self, outcome: &mut CancelAllOutcomeV0, side: SideV0) {
        match side {
            SideV0::Bid => {
                outcome.bid_base_asset_amount = self.base_asset_amount;
                outcome.bid_orders = self.orders;
                outcome.bid_reduce_only_orders = self.reduce_only_orders;
            }
            SideV0::Ask => {
                outcome.ask_base_asset_amount = self.base_asset_amount;
                outcome.ask_orders = self.orders;
                outcome.ask_reduce_only_orders = self.reduce_only_orders;
            }
        }
    }
}

/// Eviction, run as a crank. It takes the order [`evictable_order`] names,
/// and only while the side holds at least `evict_threshold_per_side`
/// orders. The crank works the soft-cap buffer down so placements never
/// reach the hard cap. Velocity is the caller and loads the evicted maker's
/// `User`, so aggregates stay exact.
pub(super) fn evict_worst(
    book: &mut ClobMarketV0,
    side: SideV0,
    slot: u64,
) -> Result<RemovedOrderV0> {
    let count = book.node_count(side);
    require!(
        count > 0 && count >= book.evict_threshold_per_side,
        ClobError::BelowEvictThreshold
    );

    let index = evictable_order(book, side, slot)?;
    require!(index != NIL, ClobError::TakerOriginBound);

    let node = book.read_node(index)?;
    require!(
        node.is_bit_flag_set(OrderBitFlag::Open) && node.side() == side,
        ClobError::BookInvariantViolated
    );

    let removed = removed_order(&node);
    remove_order(book, index)?;
    require!(
        book.node_count(side) == count - 1,
        ClobError::BookInvariantViolated
    );

    validate_single_removal(book, &node, index)?;
    book.validate_book()?;
    Ok(removed)
}

/// Expiry reclamation, run as a crank. Execute only skips an expired order.
/// A removal without the maker's `User` loaded is the aggregate leak this
/// design removes. Fails closed on a stale hint.
pub(super) fn remove_expired(
    book: &mut ClobMarketV0,
    order_ref: ClobOrderRefV0,
    now: i64,
) -> Result<RemovedOrderV0> {
    let node = live_order(book, order_ref)?;
    require!(node.is_expired(now), ClobError::OrderNotExpired);
    let removed = removed_order(&node);
    remove_order(book, order_ref.node_index)?;
    validate_single_removal(book, &node, order_ref.node_index)?;
    book.validate_book()?;
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
pub(super) fn fill(
    book: &mut ClobMarketV0,
    order_ref: ClobOrderRefV0,
    base_asset_amount: u64,
    slot: u64,
    now: i64,
) -> Result<FilledOrderV0> {
    let node = live_order(book, order_ref)?;
    require!(node.is_taker_origin(), ClobError::OrderNotTakerOrigin);
    require!(is_live(&node, slot, now), ClobError::OrderNotLive);
    require!(
        base_asset_amount > 0 && base_asset_amount <= node.base_asset_amount,
        ClobError::FillExceedsOrder
    );

    let remainder = node.base_asset_amount - base_asset_amount;
    let culls = remainder > 0 && remainder < book.min_order_size;
    let mut filled = FilledOrderV0 {
        order_id: node.order_id,
        client_order_id: node.client_order_id,
        base_asset_amount,
        culled_base_asset_amount: 0,
        removed: false,
    };

    if remainder == 0 || culls {
        filled.culled_base_asset_amount = remainder;
        filled.removed = true;
        remove_order(book, order_ref.node_index)?;
        validate_single_removal(book, &node, order_ref.node_index)?;
    } else {
        let mut reduced = node;
        reduced.base_asset_amount = remainder;
        book.write_node(order_ref.node_index, reduced)?;
    }

    book.validate_book()?;
    Ok(filled)
}

fn removed_order(node: &OrderNodeV0) -> RemovedOrderV0 {
    RemovedOrderV0 {
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
