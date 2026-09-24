//! Book unit tests: the guards and postconditions the litesvm suite can't
//! reach from outside — index validation on every arena access, the free-list
//! guards, order-id issuance, and the invariant checks that end each
//! operation (including against a deliberately tampered header).

use {
    super::{
        market::{
            assert_consistent, assert_err, params, place, place_raw, test_config, user, TestMarket,
        },
        ACTIVE_SLOT,
    },
    crate::{
        book::{walk_side, BookHeader, ClobBook, NodeArena, Walk},
        error::ClobError,
        state::{
            CancelAllOutcome, CancelSidesV0, ClobMarketV0, Direction, MarketConfigV0, OrderBitFlag,
            PlaceOrderParams, Side, UserCapsV0, UserRefV0, BASE_PRECISION,
            CANCEL_ALL_ORDERS_CEILING, NIL,
        },
    },
    anchor_lang::prelude::*,
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
    book.cancel(maker, second, ACTIVE_SLOT, false).unwrap();
    let third = place(&mut book, Side::Bid, 100, 1, maker);
    assert_eq!(third.node_index, second.node_index);
    assert_eq!(third.order_id, 3);
    assert_eq!(book.next_order_id, 4);
    assert_err(
        book.cancel(maker, second, ACTIVE_SLOT, false),
        ClobError::StaleOrderRef,
    );
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

/// A maker that quotes through the other side has mispriced. The book rests
/// such an order by default — a crossed pair is what the cross crank exists to
/// resolve — so refusing it is something the caller asks for per placement.
#[test]
fn a_placement_can_refuse_to_rest_crossed() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (maker, taker) = (user(0xA), user(0xB));
    place(&mut book, Side::Bid, 100, 5, maker);

    let refusing = |side, price| PlaceOrderParams {
        reject_if_crossed: true,
        ..params(side, price, 5, taker)
    };

    // At the bid and through it are both crossed for an ask.
    assert_err(
        book.place(refusing(Side::Ask, 100)),
        ClobError::OrderWouldCross,
    );
    assert_err(
        book.place(refusing(Side::Ask, 99)),
        ClobError::OrderWouldCross,
    );

    // A tick above rests, and so does a crossing order that did not ask.
    book.place(refusing(Side::Ask, 101))
        .expect("uncrossed rests");
    book.place(params(Side::Ask, 100, 5, taker))
        .expect("a crossing order still rests when it did not ask otherwise");
    assert_consistent(&book);

    // Same-side depth is not a cross: only the opposite best is measured.
    book.place(refusing(Side::Bid, 90))
        .expect("resting behind one's own side is not crossing");
    assert_consistent(&book);
}

/// An expired order at the opposite head refuses nothing.
///
/// Quote and execute walk past an expired order, and the expiry crank
/// reclaims it later, so it matches nothing while it sits there. Measuring a
/// cross against it would let one cheap order refuse every post-only
/// placement on the other side until the crank lands.
#[test]
fn an_expired_opposite_head_does_not_refuse_a_placement() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (maker, taker) = (user(0xA), user(0xB));
    // A bid at 100 that died a second ago, and a live bid at 90 behind it.
    book.place(PlaceOrderParams {
        max_ts: 500,
        ..params(Side::Bid, 100, 5, maker)
    })
    .expect("placement succeeds");
    book.place(params(Side::Bid, 90, 5, maker))
        .expect("placement succeeds");

    let refusing = |price| PlaceOrderParams {
        reject_if_crossed: true,
        now: 900,
        ..params(Side::Ask, price, 5, taker)
    };

    // 95 crosses the dead bid at 100 and not the live one at 90.
    book.place(refusing(95))
        .expect("an expired head crosses nothing");
    // The live bid behind it is still measured.
    assert_err(book.place(refusing(90)), ClobError::OrderWouldCross);
    assert_consistent(&book);
}

/// An opposite side of nothing but expired orders refuses nothing either.
#[test]
fn a_wholly_expired_opposite_side_refuses_nothing() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (maker, taker) = (user(0xA), user(0xB));
    for price in [100, 99, 98] {
        book.place(PlaceOrderParams {
            max_ts: 500,
            ..params(Side::Bid, price, 5, maker)
        })
        .expect("placement succeeds");
    }

    book.place(PlaceOrderParams {
        reject_if_crossed: true,
        now: 900,
        ..params(Side::Ask, 50, 5, taker)
    })
    .expect("nothing live to cross");
    assert_consistent(&book);
}

/// An order inside its activation delay still refuses a cross. It is resting
/// liquidity a moment from now, and a caller asking not to cross does not
/// want to cross that either.
#[test]
fn an_unactivated_opposite_head_still_refuses_a_placement() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (maker, taker) = (user(0xA), user(0xB));
    book.place(PlaceOrderParams {
        activation_slot: 500,
        ..params(Side::Bid, 100, 5, maker)
    })
    .expect("placement succeeds");
    assert_err(
        book.place(PlaceOrderParams {
            reject_if_crossed: true,
            ..params(Side::Ask, 100, 5, taker)
        }),
        ClobError::OrderWouldCross,
    );
}

/// Nothing to cross means nothing to refuse.
#[test]
fn refusing_to_cross_an_empty_side_still_places() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let maker = user(0xA);
    book.place(PlaceOrderParams {
        reject_if_crossed: true,
        ..params(Side::Bid, 100, 5, maker)
    })
    .expect("an empty opposite side crosses nothing");
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

/// The levels a quote published, as `(price, size)`.
fn levels(book: &ClobMarketV0, pointer: crate::state::ResponsePointerV0) -> Vec<(u64, u64)> {
    crate::state::QuoteResponseV0::parse(&super::response::streamed(book, pointer))
        .expect("quote response")
        .levels
        .iter()
        .map(|level| (level.price, level.size))
        .collect()
}

/// What the hints would be if recomputed from scratch: the earliest expiry
/// over live orders, and the earliest activation still ahead of `slot`.
fn true_hints(book: &ClobMarketV0, slot: u64) -> (i64, u64) {
    (0..book.len() as u32)
        .filter_map(|i| book.read_node(i).ok())
        .filter(|node| node.is_bit_flag_set(OrderBitFlag::Open))
        .fold((i64::MAX, u64::MAX), |(ts, activation), node| {
            (
                if node.max_ts != 0 {
                    ts.min(node.max_ts)
                } else {
                    ts
                },
                if node.activation_slot > slot {
                    activation.min(node.activation_slot)
                } else {
                    activation
                },
            )
        })
}

/// The book's own wake hints are never *later* than the truth.
///
/// A caller reads these instead of walking the arena to find out when its next
/// crank is due, so the direction of the error is what has to hold. A hint
/// earlier than the truth costs that caller a simulation that finds nothing. A
/// hint later than the truth is work nobody is woken for, which is a liveness
/// bug rather than a cost.
#[track_caller]
fn assert_hints_are_not_late(book: &ClobMarketV0, slot: u64) {
    let (expiry, activation) = true_hints(book, slot);
    assert!(
        book.next_expiry_ts <= expiry,
        "expiry hint {} is later than the earliest live expiry {expiry}",
        book.next_expiry_ts
    );
    assert!(
        book.next_activation_slot <= activation,
        "activation hint {} is later than the earliest pending activation {activation}",
        book.next_activation_slot
    );
}

#[test]
fn the_wake_hints_are_never_later_than_the_book() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let maker = user(1);

    // An empty book has nothing pending either way.
    assert_eq!(book.next_expiry_ts, i64::MAX);
    assert_eq!(book.next_activation_slot, u64::MAX);

    let mut expiring = |book: &mut ClobMarketV0, price, max_ts, activation_slot| {
        book.place(PlaceOrderParams {
            max_ts,
            activation_slot,
            ..params(Side::Ask, price, 10, maker)
        })
        .expect("placement succeeds")
    };

    // Placement folds each order in, and only ever earlier.
    let late = expiring(&mut book, 100, 900, 50);
    assert_eq!((book.next_expiry_ts, book.next_activation_slot), (900, 50));
    let early = expiring(&mut book, 101, 300, 20);
    assert_eq!((book.next_expiry_ts, book.next_activation_slot), (300, 20));
    // A later order moves neither.
    let latest = expiring(&mut book, 102, 1_200, 80);
    assert_eq!((book.next_expiry_ts, book.next_activation_slot), (300, 20));
    assert_hints_are_not_late(&book, 0);

    // Removing an order that held neither minimum moves nothing.
    book.cancel(maker, latest, ACTIVE_SLOT, false)
        .expect("cancel succeeds");
    assert_eq!((book.next_expiry_ts, book.next_activation_slot), (300, 20));

    // Removing the expiry's holder repairs it to the next live order. The
    // activation hint is left where it is — safe, because early — until a
    // write that knows the slot moves it on.
    book.cancel(maker, early, ACTIVE_SLOT, false)
        .expect("cancel succeeds");
    assert_eq!(book.next_expiry_ts, 900);
    assert_hints_are_not_late(&book, 0);

    // An empty book goes back to nothing expiring.
    book.cancel(maker, late, ACTIVE_SLOT, false)
        .expect("cancel succeeds");
    assert_eq!(book.next_expiry_ts, i64::MAX);
    assert_hints_are_not_late(&book, 0);
}

/// An order with no expiry never becomes one.
///
/// `max_ts == 0` is good-till-cancelled, not "expires at the epoch". Folding
/// it in as a timestamp would peg the hint to zero and leave the expiry wake
/// due forever.
#[test]
fn a_good_till_cancelled_order_is_not_an_expiry() {
    let market = TestMarket::new(8);
    let mut book = market.book();
    let maker = user(1);

    place(&mut book, Side::Bid, 100, 10, maker);
    assert_eq!(book.next_expiry_ts, i64::MAX);

    let expiring = book
        .place(PlaceOrderParams {
            max_ts: 500,
            ..params(Side::Bid, 99, 10, maker)
        })
        .expect("placement succeeds");
    assert_eq!(book.next_expiry_ts, 500);

    // Removing the only order that expires leaves the book with none again,
    // rather than with the good-till-cancelled order's zero.
    book.cancel(maker, expiring, ACTIVE_SLOT, false)
        .expect("cancel succeeds");
    assert_eq!(book.next_expiry_ts, i64::MAX);
}

/// An activation the chain has passed stops being pending.
///
/// It is the one hint that goes stale with nothing writing to the book, so a
/// stored slot at or behind the current one would leave its wake permanently
/// due. A zero-delay placement must not set it at all, for the same reason.
#[test]
fn a_passed_activation_stops_being_pending() {
    let market = TestMarket::new(8);
    let mut book = market.book();
    let maker = user(1);

    // Activating on the slot it was placed on is not pending.
    place(&mut book, Side::Bid, 100, 10, maker);
    assert_eq!(book.next_activation_slot, u64::MAX);

    book.place(PlaceOrderParams {
        activation_slot: 10,
        placed_slot: 5,
        ..params(Side::Ask, 100, 10, maker)
    })
    .expect("placement succeeds");
    assert_eq!(book.next_activation_slot, 10);

    // A later placement, once slot 10 has gone by, carries the hint forward to
    // the only activation still ahead.
    book.place(PlaceOrderParams {
        activation_slot: 40,
        placed_slot: 20,
        ..params(Side::Ask, 101, 10, maker)
    })
    .expect("placement succeeds");
    assert_eq!(book.next_activation_slot, 40);
    assert_hints_are_not_late(&book, 20);

    // And once that one has gone by too, nothing is pending. This placement
    // takes no delay at all — `activation_slot == placed_slot`, which the book
    // requires to be the earliest it can be — so it adds nothing to wake for.
    book.place(PlaceOrderParams {
        activation_slot: 50,
        placed_slot: 50,
        ..params(Side::Ask, 102, 10, maker)
    })
    .expect("placement succeeds");
    assert_eq!(book.next_activation_slot, u64::MAX);
    assert_hints_are_not_late(&book, 50);
}

/// The price bound stops the walk, and stops it in the right place: at the
/// limit, not before it.
///
/// The bound exists to keep a quoter from aggregating levels the caller then
/// discards. Whoever sent the transaction pays for the compute limit it
/// requests, so a hop past the taker's worst acceptable price is billed twice
/// — once to walk it, once to throw it away.
#[test]
fn the_price_bound_stops_the_walk_at_the_limit() {
    let market = TestMarket::new(8);
    let mut book = market.book();
    let maker = user(1);
    for (price, size) in [(100, 2), (101, 2), (102, 2), (103, 2)] {
        place(&mut book, Side::Ask, price, size, maker);
    }

    let users = [maker];

    // A long taker will not pay above 101. The level at 101 is acceptable;
    // 102 and 103 are not.
    let pointer = book
        .quote(
            Direction::Long,
            u64::MAX,
            &users,
            &UserCapsV0::EMPTY,
            0,
            None,
            101,
            false,
            0,
            0,
        )
        .unwrap();
    assert_eq!(levels(&mut book, pointer), vec![(100, 2), (101, 2)]);

    // Zero is no bound: the same walk reaches the whole side.
    let pointer = book
        .quote(
            Direction::Long,
            u64::MAX,
            &users,
            &UserCapsV0::EMPTY,
            0,
            None,
            0,
            false,
            0,
            0,
        )
        .unwrap();
    assert_eq!(
        levels(&mut book, pointer),
        vec![(100, 2), (101, 2), (102, 2), (103, 2)]
    );

    // The bid side ranks the other way, so a short taker's bound cuts the
    // low prices rather than the high ones.
    let market = TestMarket::new(8);
    let mut book = market.book();
    for (price, size) in [(103, 2), (102, 2), (101, 2), (100, 2)] {
        place(&mut book, Side::Bid, price, size, maker);
    }

    let pointer = book
        .quote(
            Direction::Short,
            u64::MAX,
            &users,
            &UserCapsV0::EMPTY,
            0,
            None,
            102,
            false,
            0,
            0,
        )
        .unwrap();
    assert_eq!(levels(&mut book, pointer), vec![(103, 2), (102, 2)]);
}

/// A capped user is passed over where they rest, and the depth behind them
/// is still quoted and still filled.
///
/// This is the whole point of a per-user budget over a shorter ladder: a
/// maker the caller cannot settle against sits at the top of book, and
/// truncating in front of them would cost every order behind. Quote and
/// execute have to agree exactly, or the ladder the router split on is not
/// the fill it gets.
#[test]
fn a_user_with_no_room_is_skipped_mid_book() {
    let market = TestMarket::new(8);
    let mut book = market.book();
    let broke = user(1);
    let healthy = user(2);
    // The one who cannot settle is at the front, best price.
    place(&mut book, Side::Ask, 100, 5, broke);
    place(&mut book, Side::Ask, 101, 7, healthy);

    let users = [broke, healthy];
    let mut caps = UserCapsV0::EMPTY;
    caps.excluded[0] |= 1; // index 0 == broke

    // Control: unconstrained, both levels quote.
    let pointer = book
        .quote(
            Direction::Long,
            12,
            &users,
            &UserCapsV0::EMPTY,
            0,
            None,
            0,
            false,
            0,
            0,
        )
        .unwrap();
    assert_eq!(levels(&mut book, pointer), vec![(100, 5), (101, 7)]);

    // Capped: the front order is gone and the one behind it survives — not
    // truncated away with it.
    let pointer = book
        .quote(Direction::Long, 12, &users, &caps, 0, None, 0, false, 0, 0)
        .unwrap();
    assert_eq!(
        levels(&mut book, pointer),
        vec![(101, 7)],
        "the depth behind the skipped maker is still quoted"
    );

    // And execute spends the same budget, so the fill matches the ladder.
    let outcome = book
        .execute(Direction::Long, 12, &users, &caps, 0, None, false, 0, 0)
        .unwrap();
    let filled: u64 = outcome.fills.iter().map(|fill| fill.base_size).sum();
    assert_eq!(filled, 7);
    assert_eq!(
        book.node_count(Side::Ask),
        1,
        "the skipped maker's order is untouched, not consumed"
    );
}

/// A budget truncates one maker without ending the walk, and the book is the
/// one that turns quote into base.
///
/// The tight maker's ask sits 2 below the reference, so every base it sells
/// costs it 2. A budget of 4 therefore buys 2 base of it, and the depth
/// behind it is untouched.
#[test]
fn a_user_with_some_room_is_filled_only_that_far() {
    const UNIT: u64 = BASE_PRECISION;
    let market = TestMarket::new(8);
    let mut book = market.book();
    let tight = user(1);
    let healthy = user(2);
    place(&mut book, Side::Ask, 100, 5 * UNIT, tight);
    place(&mut book, Side::Ask, 101, 7 * UNIT, healthy);

    let users = [tight, healthy];
    let mut caps = UserCapsV0::EMPTY;
    caps.len = 1;
    caps.caps[0] = crate::state::UserCapV0 {
        index: 0,
        quote_cap: 4,
        base_cap: u64::MAX,
    };

    let pointer = book
        .quote(
            Direction::Long,
            12 * UNIT,
            &users,
            &caps,
            102,
            None,
            0,
            false,
            0,
            0,
        )
        .unwrap();
    assert_eq!(
        levels(&mut book, pointer),
        vec![(100, 2 * UNIT), (101, 7 * UNIT)],
        "capped to what its budget buys, and the rest of the book follows"
    );

    let outcome = book
        .execute(
            Direction::Long,
            12 * UNIT,
            &users,
            &caps,
            102,
            None,
            false,
            0,
            0,
        )
        .unwrap();
    let filled: u64 = outcome.fills.iter().map(|fill| fill.base_size).sum();
    assert_eq!(filled, 9 * UNIT);
}

/// A reduce-only order fills only up to its owner's authoritative base cover.
///
/// The book is position-blind, so the cover the caller carries is the only
/// thing that keeps a reduce-only fill from growing a position it should
/// shrink. The maker's reduce-only ask is capped to two units, so two fill and
/// the depth behind it takes the rest.
#[test]
fn a_reduce_only_order_fills_only_up_to_its_base_cover() {
    const UNIT: u64 = BASE_PRECISION;
    let market = TestMarket::new(8);
    let mut book = market.book();
    let capped = user(1);
    let healthy = user(2);
    book.place(PlaceOrderParams {
        reduce_only: true,
        ..params(Side::Ask, 100, 5 * UNIT, capped)
    })
    .expect("placement succeeds");
    place(&mut book, Side::Ask, 101, 7 * UNIT, healthy);

    let users = [capped, healthy];
    let mut caps = UserCapsV0::EMPTY;
    caps.len = 1;
    caps.caps[0] = crate::state::UserCapV0 {
        index: 0,
        quote_cap: u64::MAX,
        base_cap: 2 * UNIT,
    };

    let outcome = book
        .execute(
            Direction::Long,
            12 * UNIT,
            &users,
            &caps,
            100,
            None,
            false,
            0,
            0,
        )
        .unwrap();
    let filled: u64 = outcome.fills.iter().map(|fill| fill.base_size).sum();
    // Two units of the reduce-only ask (its cover) plus all seven behind it.
    assert_eq!(filled, 9 * UNIT);
}

/// A reduce-only order with no cover does not fill at all.
///
/// A cover the caller did not carry is not "unlimited" — it is unknown, and an
/// unknown cover on a position-blind book is refused. The reduce-only ask is
/// passed over; the ordinary depth behind it still fills.
#[test]
fn a_reduce_only_order_with_no_cover_does_not_fill() {
    const UNIT: u64 = BASE_PRECISION;
    let market = TestMarket::new(8);
    let mut book = market.book();
    let uncovered = user(1);
    let healthy = user(2);
    book.place(PlaceOrderParams {
        reduce_only: true,
        ..params(Side::Ask, 100, 5 * UNIT, uncovered)
    })
    .expect("placement succeeds");
    place(&mut book, Side::Ask, 101, 7 * UNIT, healthy);

    let users = [uncovered, healthy];
    // No cap entry for the reduce-only maker: uncovered, so it must not fill.
    let caps = UserCapsV0::EMPTY;

    let outcome = book
        .execute(
            Direction::Long,
            12 * UNIT,
            &users,
            &caps,
            100,
            None,
            false,
            0,
            0,
        )
        .unwrap();
    let filled: u64 = outcome.fills.iter().map(|fill| fill.base_size).sum();
    assert_eq!(filled, 7 * UNIT, "only the ordinary ask fills");
}

/// Two capped makers in one sweep end the walk instead of failing it.
///
/// A truncated order leaves a remainder, and the response has one slot to
/// report it in — so the second truncation has nowhere to go. `execute` used to
/// assert its way out of that with `BookInvariantViolated`, which took the whole
/// transaction down; `UserCapsV0` carries eight budgets and the router fills
/// them, so two capped makers in reach is an ordinary request rather than a
/// corrupt book. The fill stops short instead, and the quote stops in the same
/// place so the ladder never promises the second maker's depth.
#[test]
fn a_second_capped_maker_ends_the_sweep_rather_than_failing_it() {
    const UNIT: u64 = BASE_PRECISION;
    let market = TestMarket::new(8);
    let mut book = market.book();
    let (first, second) = (user(1), user(2));
    place(&mut book, Side::Ask, 100, 5 * UNIT, first);
    place(&mut book, Side::Ask, 101, 5 * UNIT, second);

    // Against a reference of 102 the first order costs 2 per base and the
    // second costs 1, so these budgets buy 2 and 3 units of 5 — both truncate.
    let users = [first, second];
    let mut caps = UserCapsV0::EMPTY;
    caps.len = 2;
    caps.caps[0] = crate::state::UserCapV0 {
        index: 0,
        quote_cap: 4,
        base_cap: u64::MAX,
    };

    caps.caps[1] = crate::state::UserCapV0 {
        index: 1,
        quote_cap: 3,
        base_cap: u64::MAX,
    };

    let pointer = book
        .quote(
            Direction::Long,
            12 * UNIT,
            &users,
            &caps,
            102,
            None,
            0,
            false,
            0,
            0,
        )
        .unwrap();
    assert_eq!(
        levels(&mut book, pointer),
        vec![(100, 2 * UNIT)],
        "the ladder ends where the fill will, not one maker further"
    );

    let outcome = book
        .execute(
            Direction::Long,
            12 * UNIT,
            &users,
            &caps,
            102,
            None,
            false,
            0,
            0,
        )
        .unwrap();
    let filled: u64 = outcome.fills.iter().map(|fill| fill.base_size).sum();
    assert_eq!(filled, 2 * UNIT);
    // Both orders are still on the book: the first smaller, the second whole.
    assert_eq!(book.node_count(Side::Ask), 2);
}

/// An order priced in its owner's favour draws on nothing, so a budget never
/// truncates it however small the budget is.
#[test]
fn a_fill_that_pays_the_maker_spends_no_budget() {
    const UNIT: u64 = BASE_PRECISION;
    let market = TestMarket::new(8);
    let mut book = market.book();
    let maker = user(1);
    place(&mut book, Side::Ask, 100, 5 * UNIT, maker);

    let users = [maker];
    let mut caps = UserCapsV0::EMPTY;
    caps.len = 1;
    caps.caps[0] = crate::state::UserCapV0 {
        index: 0,
        quote_cap: 1,
        base_cap: u64::MAX,
    };

    // Selling at 100 against a reference of 98 is a gain, not a loss.
    let outcome = book
        .execute(
            Direction::Long,
            5 * UNIT,
            &users,
            &caps,
            98,
            None,
            false,
            0,
            0,
        )
        .unwrap();
    let filled: u64 = outcome.fills.iter().map(|fill| fill.base_size).sum();
    assert_eq!(filled, 5 * UNIT);
}

/// A book holding more distinct makers than `max_execute_users` still fills.
///
/// This is the deadlock the cap used to create. `execute` writes one balance
/// change per distinct user and stops when the next one will not fit, so the
/// response buffer is safe whatever the caller passes. `quote` used to protect
/// the same buffer by refusing a wider *set* instead, and the two are not the
/// same quantity: the set also carries makers on other venues and a referrer,
/// none of whom this book will fill.
///
/// The consequence was that a book with more makers in reach than the cap had
/// no assembly that worked. Pass them all and quote refuses the set; leave one
/// out and its aged order is a stale set. Both answers are the same error, and
/// the order could not be filled by anyone.
#[test]
fn a_set_wider_than_the_user_cap_still_quotes() {
    let mut config = crate::tests::market::test_config();
    config.max_execute_users = 2;
    let market = TestMarket::new_with(8, config);
    let mut book = market.book();
    let first = user(1);
    let second = user(2);
    let third = user(3);
    place(&mut book, Side::Ask, 100, 5, first);
    place(&mut book, Side::Ask, 101, 5, second);
    place(&mut book, Side::Ask, 102, 5, third);

    // All three named, which is one more than the cap. The old rule failed
    // the call here.
    let users = [first, second, third];
    let pointer = book
        .quote(
            Direction::Long,
            15,
            &users,
            &UserCapsV0::EMPTY,
            0,
            None,
            0,
            false,
            0,
            0,
        )
        .unwrap();
    assert_eq!(
        levels(&mut book, pointer),
        vec![(100, 5), (101, 5)],
        "quoted up to the cap and stopped, rather than refusing the set"
    );

    // And execute delivers exactly that — the promise the cap exists to keep.
    let outcome = book
        .execute(
            Direction::Long,
            15,
            &users,
            &UserCapsV0::EMPTY,
            0,
            None,
            false,
            0,
            0,
        )
        .unwrap();
    let filled: u64 = outcome.fills.iter().map(|fill| fill.base_size).sum();
    assert_eq!(
        filled, 10,
        "the third maker is out of the cap, not the book"
    );
    assert_eq!(outcome.fills.len(), 2, "one order from each of the two");

    // The third maker's order is untouched and still resting, so a later
    // fill that names a different set can reach it.
    assert_eq!(book.node_count(Side::Ask), 1);
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
            book.quote(
                Direction::Long,
                10,
                &[],
                &UserCapsV0::EMPTY,
                0,
                None,
                0,
                false,
                0,
                0,
            ),
            ClobError::NodeIndexOutOfRange,
        );
        assert_err(
            book.execute(
                Direction::Long,
                10,
                &[],
                &UserCapsV0::EMPTY,
                0,
                None,
                false,
                0,
                0,
            ),
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
        book.quote(
            Direction::Long,
            u64::MAX,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            0,
            false,
            0,
            0,
        ),
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
    let removed = book.cancel(maker, mid, ACTIVE_SLOT, false).unwrap();
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
    assert_err(
        book.cancel(maker, mid, ACTIVE_SLOT, false),
        ClobError::StaleOrderRef,
    );
    assert_err(book.remove_expired(mid, 1), ClobError::StaleOrderRef);
}

/// Helper for the cancel-all tests: run the sweep and collect the ids it
/// reported, asserting the book is fully consistent afterwards.
#[track_caller]
fn cancel_all(
    book: &mut ClobMarketV0,
    user: UserRefV0,
    sides: CancelSidesV0,
) -> (CancelAllOutcome, Vec<u32>) {
    let mut ids = Vec::new();
    let outcome = book
        .cancel_all(user, sides, ACTIVE_SLOT, false, &mut |client_order_id| {
            ids.push(client_order_id);
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
        book.cancel_all(maker, CancelSidesV0::Both, ACTIVE_SLOT, false, &mut |_| {
            Ok(())
        }),
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
        book.cancel_all(maker, CancelSidesV0::Both, ACTIVE_SLOT, false, &mut |_| {
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

    let removed = book.evict_worst(Side::Bid, ACTIVE_SLOT).unwrap();
    assert_eq!((removed.order_id, removed.price), (tail.order_id, 100));
    assert_eq!(book.worst(Side::Bid), next_tail.node_index);
    assert!(!book
        .read_node(tail.node_index)
        .unwrap()
        .is_bit_flag_set(OrderBitFlag::Open));
    assert_consistent(&book);

    // Evicting the last order on a side clears both endpoints.
    book.evict_worst(Side::Bid, ACTIVE_SLOT).unwrap();
    book.evict_worst(Side::Bid, ACTIVE_SLOT).unwrap();
    assert_eq!(book.node_count(Side::Bid), 0);
    assert_eq!((book.best(Side::Bid), book.worst(Side::Bid)), (NIL, NIL));
    assert_err(
        book.evict_worst(Side::Bid, ACTIVE_SLOT),
        ClobError::BelowEvictThreshold,
    );
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
    assert_err(
        book.cancel(maker, order, ACTIVE_SLOT, false),
        ClobError::BookInvariantViolated,
    );
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
    assert_eq!(book.padding, [0u8; 104]);
    // The wake hints start at "nothing pending" rather than at zero, which
    // would read as an expiry at the epoch and an activation already passed.
    assert_eq!(book.next_expiry_ts, i64::MAX);
    assert_eq!(book.next_activation_slot, u64::MAX);
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

/// Initialization with the given config, for the config checks below.
fn init_with(capacity: u32, config: MarketConfigV0) -> Result<()> {
    let market = TestMarket::uninitialized(capacity);
    market.book().initialize(
        Address::new_from_array([1u8; 32]),
        Address::new_from_array([2u8; 32]),
        config,
    )
}

/// The base denominator is fixed, not per market.
///
/// A fill's quote amount is computed with it here, while the per-user budget
/// and the caller's own exact-notional check both use the constant. A market
/// on any other denominator prices its response on one scale and is settled
/// on another, so every fill it produces is refused.
#[test]
fn initialize_refuses_any_base_precision_but_the_constant() {
    assert_err(
        init_with(
            16,
            MarketConfigV0 {
                base_precision: 0,
                ..test_config()
            },
        ),
        ClobError::InvalidConfig,
    );
    assert_err(
        init_with(
            16,
            MarketConfigV0 {
                base_precision: 1_000_000,
                ..test_config()
            },
        ),
        ClobError::InvalidConfig,
    );

    init_with(16, test_config()).expect("the constant is accepted");
}

/// The eviction threshold has to leave a buffer between it and the per-side
/// cap. Zero makes every non-empty side evictable; the cap itself unlocks
/// eviction only once placements are already refused.
#[test]
fn the_eviction_threshold_is_bounded_by_the_per_side_cap() {
    // Sixteen slots is eight per side.
    let threshold = |v| MarketConfigV0 {
        evict_threshold_per_side: v,
        ..test_config()
    };

    assert_err(init_with(16, threshold(0)), ClobError::InvalidConfig);
    assert_err(init_with(16, threshold(8)), ClobError::InvalidConfig);
    assert_err(init_with(16, threshold(9)), ClobError::InvalidConfig);
    init_with(16, threshold(1)).expect("one is the lowest legal threshold");
    init_with(16, threshold(7)).expect("one below the cap still leaves a buffer");

    // The same bound is applied when the threshold is updated.
    let market = TestMarket::new(16);
    assert!(
        crate::book::validate_evict_threshold(8, market.book().capacity() as u32).is_err(),
        "an update to the cap is refused too"
    );

    crate::book::validate_evict_threshold(7, market.book().capacity() as u32)
        .expect("an update below the cap is accepted");
}
