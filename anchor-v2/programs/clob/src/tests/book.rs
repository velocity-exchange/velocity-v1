//! Book unit tests: the guards and postconditions the litesvm suite can't
//! reach from outside — index validation on every arena access, the free-list
//! guards, order-id issuance, and the invariant checks that end each
//! operation (including against a deliberately tampered header).

use {
    super::market::{
        assert_consistent, assert_err, place, place_raw, test_config, user, TestMarket,
    },
    crate::{
        book::{BookHeader, ClobBook, NodeArena, NIL},
        error::ClobError,
        state::{Direction, OrderBitFlag, PlaceOrderParams, Side},
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
