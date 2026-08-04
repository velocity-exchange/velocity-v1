//! Book unit tests: the guards and postconditions the litesvm suite can't
//! reach from outside — index validation on every arena access, the free-list
//! guards, order-id issuance, and the invariant checks that end each
//! operation (including against a deliberately tampered header).

use {
    super::market::{
        assert_consistent, assert_err, place, place_raw, test_config, user, TestMarket,
    },
    crate::{
        book::{walk_side, BookHeader, ClobBook, NodeArena, Walk, NIL},
        error::ClobError,
        state::{
            CancelAllOutcome, CancelSidesV0, ClobMarketV0, Direction, OrderBitFlag,
            PlaceOrderParams, Side, UserRefV0, CANCEL_ALL_ORDERS_CEILING,
        },
    },
    anchor_lang_v2::prelude::*,
};

#[test]
fn place_queues_by_price_then_time() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (maker_a, maker_b) = (user(0xA), user(0xB));

    let first_at_100 = place(&mut book, Side::Ask, 100, 5, maker_a);
    let worse = place(&mut book, Side::Ask, 101, 10, maker_b);
    let later_at_100 = place(&mut book, Side::Ask, 100, 7, maker_b);
    let best = place(&mut book, Side::Ask, 99, 1, maker_a);

    assert_eq!(book.node_count(Side::Ask), 4);
    assert_eq!(book.node_count(Side::Bid), 0);
    assert_eq!(book.best(Side::Ask), best.node_index);
    assert_eq!(book.worst(Side::Ask), worse.node_index);
    // The later order at 100 queues behind the earlier one at the same price.
    assert_eq!(
        book.read_node(first_at_100.node_index).unwrap().next,
        later_at_100.node_index
    );
    // Bids sort the other way: the highest price is the best of book.
    let low_bid = place(&mut book, Side::Bid, 50, 1, maker_a);
    let high_bid = place(&mut book, Side::Bid, 60, 1, maker_a);
    assert_eq!(book.best(Side::Bid), high_bid.node_index);
    assert_eq!(book.worst(Side::Bid), low_bid.node_index);
}

#[test]
fn order_ids_are_issued_once_each() {
    let market = TestMarket::new(8);
    let mut book = market.book();
    let maker = user(1);

    let first = place(&mut book, Side::Bid, 100, 1, maker);
    let second = place(&mut book, Side::Bid, 100, 1, maker);
    assert_eq!((first.order_id, second.order_id), (1, 2));

    // A recycled node gets a fresh id, so the old handle can never verify.
    book.cancel(maker, second).unwrap();
    let third = place(&mut book, Side::Bid, 100, 1, maker);
    assert_eq!(third.node_index, second.node_index);
    assert_eq!(third.order_id, 3);
    assert_eq!(book.next_order_id, 4);
    assert_err(book.cancel(maker, second), ClobError::StaleOrderRef);
}

#[test]
fn consume_order_id_refuses_to_wrap() {
    let market = TestMarket::new(8);
    let mut book = market.book();

    book.next_order_id = u64::MAX - 1;
    assert_eq!(book.consume_order_id().unwrap(), u64::MAX - 1);
    // The counter has nowhere left to go, so no id is handed out — an id
    // could otherwise be issued twice.
    assert_err(book.consume_order_id(), ClobError::MathError);
    assert_eq!(book.next_order_id, u64::MAX);
    assert_err(
        place_raw(&mut book, Side::Bid, 100, 1, user(1)),
        ClobError::MathError,
    );
}

#[test]
fn alloc_node_guards_the_free_list() {
    let market = TestMarket::new(8);
    let mut book = market.book();
    let maker = user(1);
    place(&mut book, Side::Bid, 100, 1, maker);

    // Callers check the per-side cap before allocating; these are the
    // defense-in-depth guards for a free list that disagrees with itself.
    let (head, count) = (book.free_head, book.free_count);
    book.free_count = 0;
    book.free_head = NIL;
    assert_err(
        place_raw(&mut book, Side::Bid, 100, 1, maker),
        ClobError::ArenaExhausted,
    );

    book.free_count = count;
    book.free_head = NIL;
    assert_err(
        place_raw(&mut book, Side::Bid, 100, 1, maker),
        ClobError::ArenaExhausted,
    );

    book.free_head = book.capacity() as u32;
    assert_err(
        place_raw(&mut book, Side::Bid, 100, 1, maker),
        ClobError::NodeIndexOutOfRange,
    );

    // A free head pointing at a live order would hand out an occupied slot.
    book.free_head = book.best(Side::Bid);
    assert_err(
        place_raw(&mut book, Side::Bid, 100, 1, maker),
        ClobError::BookInvariantViolated,
    );

    book.free_head = head;
    book.free_count = count;
    assert_consistent(&book);
}

#[test]
fn arena_access_validates_every_index() {
    let market = TestMarket::new(8);
    let mut book = market.book();
    let node = book.read_node(0).unwrap();
    let past_end = book.capacity() as u32;

    assert_err(book.read_node(past_end), ClobError::NodeIndexOutOfRange);
    assert_err(book.read_node(NIL), ClobError::NodeIndexOutOfRange);
    assert_err(
        book.write_node(past_end, node),
        ClobError::NodeIndexOutOfRange,
    );
    assert_err(
        book.update_node(past_end, |n| n.price = 1),
        ClobError::NodeIndexOutOfRange,
    );
    assert_err(book.set_next(past_end, 0), ClobError::NodeIndexOutOfRange);
    assert_err(book.set_prev(past_end, 0), ClobError::NodeIndexOutOfRange);
    // The sentinel means "no neighbour" for the link setters, not a slot.
    book.set_next(NIL, 0).unwrap();
    book.set_prev(NIL, 0).unwrap();
}

#[test]
fn a_link_out_of_the_arena_fails_every_walk() {
    // Just past the arena, and just short of the sentinel.
    for corrupt in [12u32, NIL - 1] {
        let market = TestMarket::new(8);
        let mut book = market.book();
        let maker = user(1);
        let head = place(&mut book, Side::Ask, 100, 5, maker);
        place(&mut book, Side::Ask, 101, 5, maker);
        book.update_node(head.node_index, |node| node.next = corrupt)
            .unwrap();

        assert_err(
            book.quote(Direction::Long, 10, &[], None, 0, 0),
            ClobError::NodeIndexOutOfRange,
        );
        assert_err(
            book.execute(Direction::Long, 10, &[], None, 0, 0),
            ClobError::NodeIndexOutOfRange,
        );
        // The placement scan walks the same list.
        assert_err(
            place_raw(&mut book, Side::Ask, 100, 5, maker),
            ClobError::NodeIndexOutOfRange,
        );
    }
}

#[test]
fn a_cycled_link_cannot_spin_the_walk() {
    let market = TestMarket::new(8);
    let mut book = market.book();
    let maker = user(1);
    let head = place(&mut book, Side::Ask, 100, 1, maker);
    // A list that points back at itself has no tail to stop at; the walk
    // gives up once it has taken more hops than the arena has slots.
    book.update_node(head.node_index, |node| node.next = head.node_index)
        .unwrap();
    assert_err(
        book.quote(Direction::Long, u64::MAX, &[], None, 0, 0),
        ClobError::BookInvariantViolated,
    );
}

#[test]
fn removals_free_the_slot_and_close_the_list() {
    let market = TestMarket::new(8);
    let mut book = market.book();
    let maker = user(1);
    let low = place(&mut book, Side::Bid, 100, 3, maker);
    let mid = place(&mut book, Side::Bid, 110, 3, maker);
    let high = place(&mut book, Side::Bid, 120, 3, maker);

    // Cancelling the middle order relinks its neighbours around it.
    let removed = book.cancel(maker, mid).unwrap();
    assert_eq!(removed.order_id, mid.order_id);
    assert_consistent(&book);
    assert_eq!(
        book.read_node(high.node_index).unwrap().next,
        low.node_index
    );
    assert_eq!(
        book.read_node(low.node_index).unwrap().prev,
        high.node_index
    );
    // The slot is zeroed and heads the free list, so the handle is dead.
    let freed = book.read_node(mid.node_index).unwrap();
    assert_eq!((freed.bit_flags, freed.order_id), (0, 0));
    assert_eq!(book.free_head, mid.node_index);
    assert_err(book.cancel(maker, mid), ClobError::StaleOrderRef);
    assert_err(book.remove_expired(mid, 1), ClobError::StaleOrderRef);
}

/// Helper for the cancel-all tests: run the sweep and collect the ids it
/// reported, asserting the book is fully consistent afterwards.
#[track_caller]
fn cancel_all(
    book: &mut ClobMarketV0,
    user: UserRefV0,
    sides: CancelSidesV0,
) -> (CancelAllOutcome, Vec<u64>) {
    let mut ids = Vec::new();
    let outcome = book
        .cancel_all(user, sides, &mut |order_id| {
            ids.push(order_id);
            Ok(())
        })
        .expect("cancel_all succeeds");
    assert_consistent(book);
    (outcome, ids)
}

/// The property the aggregate wire rests on: after a sweep of a side, that
/// side holds none of the swept user's orders, everyone else's are untouched in
/// their original order, and the reported totals are exactly what left.
#[test]
fn cancel_all_takes_one_users_side_and_leaves_the_rest() {
    let market = TestMarket::new(32);
    let mut book = market.book();
    let (mine, theirs) = (user(0xA), user(0xB));

    // Interleaved through both sides, so a sweep has to relink around
    // survivors rather than truncate a contiguous run.
    let mut survivors = Vec::new();
    for i in 0..4u64 {
        place(&mut book, Side::Bid, 100 - i, 3, mine);
        survivors.push(place(&mut book, Side::Bid, 100 - i, 7, theirs));
        place(&mut book, Side::Ask, 200 + i, 5, mine);
    }

    let (outcome, ids) = cancel_all(&mut book, mine, CancelSidesV0::Both);
    assert_eq!(outcome.bid_orders, 4);
    assert_eq!(outcome.ask_orders, 4);
    assert_eq!(outcome.bid_base_asset_amount, 12);
    assert_eq!(outcome.ask_base_asset_amount, 20);
    assert!(outcome.exhaustive);
    assert_eq!(ids.len(), 8);

    // Nothing of mine is left; every one of theirs is, best-first as placed.
    assert_eq!(book.node_count(Side::Ask), 0);
    assert_eq!(book.node_count(Side::Bid), 4);
    let mut remaining = Vec::new();
    walk_side(&mut book, Side::Bid, |_, _, node| {
        assert_eq!(node.user_ref(), theirs);
        remaining.push(node.order_id);
        Ok(Walk::Continue)
    })
    .unwrap();
    assert_eq!(
        remaining,
        survivors
            .iter()
            .map(|order| order.order_id)
            .collect::<Vec<_>>()
    );
}

#[test]
fn cancel_all_sweeps_only_the_named_sides() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let maker = user(1);
    place(&mut book, Side::Bid, 100, 2, maker);
    place(&mut book, Side::Ask, 200, 3, maker);

    let (bids, _) = cancel_all(&mut book, maker, CancelSidesV0::Bids);
    assert_eq!((bids.bid_orders, bids.ask_orders), (1, 0));
    assert_eq!(bids.bid_base_asset_amount, 2);
    assert_eq!(bids.ask_base_asset_amount, 0);
    assert_eq!(book.node_count(Side::Ask), 1);

    let (asks, _) = cancel_all(&mut book, maker, CancelSidesV0::Asks);
    assert_eq!((asks.bid_orders, asks.ask_orders), (0, 1));
    assert_eq!(asks.ask_base_asset_amount, 3);
    assert_eq!(book.node_count(Side::Bid), 0);

    // A sweep with nothing to take is not an error — it reports an empty,
    // exhaustive result, which is what makes the call idempotent.
    let (empty, ids) = cancel_all(&mut book, maker, CancelSidesV0::Both);
    assert_eq!(
        empty,
        CancelAllOutcome {
            exhaustive: true,
            ..Default::default()
        }
    );
    assert!(ids.is_empty());
}

/// Past the per-call cap the sweep stops and says so, and the orders it did
/// remove are exactly the ones it reported — so repeating the call converges.
#[test]
fn cancel_all_stops_at_the_ceiling_and_reports_it() {
    let capacity = 2 * (CANCEL_ALL_ORDERS_CEILING as u32 + 4);
    let market = TestMarket::new(capacity);
    let mut book = market.book();
    let maker = user(1);
    let total = CANCEL_ALL_ORDERS_CEILING as u64 + 4;
    for i in 0..total {
        place(&mut book, Side::Bid, 1_000 - i, 1, maker);
    }

    let (first, ids) = cancel_all(&mut book, maker, CancelSidesV0::Both);
    assert!(!first.exhaustive);
    assert_eq!(first.bid_orders, CANCEL_ALL_ORDERS_CEILING as u32);
    assert_eq!(ids.len(), CANCEL_ALL_ORDERS_CEILING as usize);
    assert_eq!(
        book.node_count(Side::Bid),
        total as u32 - CANCEL_ALL_ORDERS_CEILING as u32
    );

    // The remainder clears in one more call, which then reads as exhaustive.
    let (second, ids) = cancel_all(&mut book, maker, CancelSidesV0::Both);
    assert!(second.exhaustive);
    assert_eq!(second.bid_orders, 4);
    assert_eq!(ids.len(), 4);
    assert_eq!(book.node_count(Side::Bid), 0);
}

/// The cap is a budget for the whole call, not for each side — otherwise a
/// two-sided sweep could remove twice what the ceiling promises and overrun the
/// cancel record's log buffer.
#[test]
fn the_cancel_all_ceiling_spans_both_sides() {
    let per_side = CANCEL_ALL_ORDERS_CEILING as u32;
    let market = TestMarket::new(2 * per_side);
    let mut book = market.book();
    let maker = user(1);
    for i in 0..per_side as u64 {
        place(&mut book, Side::Bid, 1_000 - i, 1, maker);
        place(&mut book, Side::Ask, 2_000 + i, 1, maker);
    }

    let (outcome, ids) = cancel_all(&mut book, maker, CancelSidesV0::Both);
    assert!(!outcome.exhaustive);
    assert_eq!(outcome.orders(), CANCEL_ALL_ORDERS_CEILING as u32);
    assert_eq!(ids.len(), CANCEL_ALL_ORDERS_CEILING as usize);
    // Bids filled the budget, so the ask side was never touched.
    assert_eq!(outcome.ask_orders, 0);
    assert_eq!(book.node_count(Side::Ask), per_side);
}

/// The sweep ends by validating the book like every other mutating operation.
#[test]
fn cancel_all_ends_by_validating_the_book() {
    let market = TestMarket::new(8);
    let mut book = market.book();
    let maker = user(1);
    place(&mut book, Side::Bid, 100, 1, maker);
    book.free_count += 1;
    assert_err(
        book.cancel_all(maker, CancelSidesV0::Both, &mut |_| Ok(())),
        ClobError::BookInvariantViolated,
    );
}

/// A sink that refuses fails the whole sweep rather than dropping an order
/// from the record — the id list is what an indexer reconciles the book from.
#[test]
fn a_failing_id_sink_fails_the_sweep() {
    let market = TestMarket::new(8);
    let mut book = market.book();
    let maker = user(1);
    place(&mut book, Side::Bid, 100, 1, maker);
    assert_err(
        book.cancel_all(maker, CancelSidesV0::Both, &mut |_| {
            Err(ClobError::EventTooLarge.into())
        }),
        ClobError::EventTooLarge,
    );
}

#[test]
fn evict_worst_moves_the_tail_off_the_freed_slot() {
    let market = TestMarket::new(8);
    let mut book = market.book();
    let maker = user(1);
    place(&mut book, Side::Bid, 120, 1, maker);
    let tail = place(&mut book, Side::Bid, 100, 1, maker);
    let next_tail = place(&mut book, Side::Bid, 110, 1, maker);

    let removed = book.evict_worst(Side::Bid).unwrap();
    assert_eq!((removed.order_id, removed.price), (tail.order_id, 100));
    assert_eq!(book.worst(Side::Bid), next_tail.node_index);
    assert!(!book
        .read_node(tail.node_index)
        .unwrap()
        .is_bit_flag_set(OrderBitFlag::Open));
    assert_consistent(&book);

    // Evicting the last order on a side clears both endpoints.
    book.evict_worst(Side::Bid).unwrap();
    book.evict_worst(Side::Bid).unwrap();
    assert_eq!(book.node_count(Side::Bid), 0);
    assert_eq!((book.best(Side::Bid), book.worst(Side::Bid)), (NIL, NIL));
    assert_err(book.evict_worst(Side::Bid), ClobError::BelowEvictThreshold);
    assert_consistent(&book);
}

#[test]
fn expired_orders_are_reclaimed_only_once_expired() {
    let market = TestMarket::new(8);
    let mut book = market.book();
    let maker = user(1);
    let order = book
        .place(PlaceOrderParams {
            max_ts: 1_000,
            ..super::market::params(Side::Ask, 100, 5, maker)
        })
        .unwrap();

    assert_err(
        book.remove_expired(order, 1_000),
        ClobError::OrderNotExpired,
    );
    let removed = book.remove_expired(order, 1_001).unwrap();
    assert_eq!(removed.base_asset_amount, 5);
    assert_consistent(&book);
}

#[test]
fn validate_book_catches_a_tampered_header() {
    let market = TestMarket::new(8);
    let mut book = market.book();
    let order = place(&mut book, Side::Bid, 100, 1, user(1));
    let free_head = book.free_head;

    // The three counts must account for the whole arena.
    book.free_count += 1;
    assert_err(book.validate_book(), ClobError::BookInvariantViolated);
    book.free_count -= 1;
    book.bid_count += 1;
    assert_err(book.validate_book(), ClobError::BookInvariantViolated);
    book.bid_count -= 1;

    // An endpoint pointing at a freed slot.
    book.set_best(Side::Bid, free_head);
    assert_err(book.validate_book(), ClobError::BookInvariantViolated);
    book.set_best(Side::Bid, order.node_index);

    // Endpoints are null exactly when the side is empty.
    book.set_worst(Side::Bid, NIL);
    assert_err(book.validate_book(), ClobError::BookInvariantViolated);
    book.set_worst(Side::Bid, order.node_index);

    // A free head that disagrees with the free count.
    book.free_head = NIL;
    assert_err(book.validate_book(), ClobError::BookInvariantViolated);
    book.free_head = free_head;
    assert_consistent(&book);
}

#[test]
fn every_mutating_operation_ends_by_validating_the_book() {
    let market = TestMarket::new(8);
    let mut book = market.book();
    let maker = user(1);
    let order = place(&mut book, Side::Bid, 100, 1, maker);
    // Nothing in cancel's own path looks at the free count; the
    // end-of-operation invariant check is what catches it.
    book.free_count += 1;
    assert_err(book.cancel(maker, order), ClobError::BookInvariantViolated);
}

#[test]
fn place_is_rejected_at_the_per_side_cap_before_the_arena_runs_dry() {
    // 8 slots is 4 per side, so the cap trips with half the arena still free.
    let market = TestMarket::new(8);
    let mut book = market.book();
    let maker = user(1);
    for i in 0..4 {
        place(&mut book, Side::Bid, 100 + i, 1, maker);
    }
    assert_eq!(book.free_count, 4);
    assert_err(
        place_raw(&mut book, Side::Bid, 200, 1, maker),
        ClobError::SideAtCapacity,
    );
    // The other side is unaffected.
    place(&mut book, Side::Ask, 200, 1, maker);
}

#[test]
fn initialize_threads_the_whole_arena() {
    let market = TestMarket::new(4);
    let book = market.book();
    assert_eq!(book.free_count, 4);
    assert_eq!(book.next_order_id, 1);
    assert_eq!(book.padding, [0u8; 128]);
    assert_consistent(&book);
    drop(book);

    // A single-slot arena can't hold a book (each side needs a slot).
    let tiny = TestMarket::uninitialized(1);
    assert_err(
        tiny.book().initialize(
            Address::new_from_array([1u8; 32]),
            Address::new_from_array([2u8; 32]),
            test_config(),
        ),
        ClobError::InvalidCapacity,
    );
}
