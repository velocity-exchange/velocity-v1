//! Taker-origin orders: the marker on the node, its report on the removal
//! wire, and the gate that stops a crossed taker remainder being taken at its
//! own price.

use {
    super::market::{assert_err, params, place, place_taker_origin, user, TestMarket},
    crate::{
        book::{ClobBook, NodeArena},
        error::ClobError,
        state::{
            ClobMarketV0, Direction, OrderBitFlag, OrderRefV0, PlaceOrderParams, Side, UserRefV0,
        },
    },
};

/// Place with an explicit activation slot, so a test can hold one side of a
/// pair inside its auction window.
fn place_at(
    book: &mut ClobMarketV0,
    side: Side,
    price: u64,
    size: u64,
    user: UserRefV0,
    activation_slot: u64,
    taker_origin: bool,
) -> OrderRefV0 {
    book.place(PlaceOrderParams {
        activation_slot,
        taker_origin,
        ..params(side, price, size, user)
    })
    .expect("placement succeeds")
}

#[test]
fn the_flag_rides_the_node_and_every_removal_reports_it() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let maker = user(0xA);

    let remainder = place_taker_origin(&mut book, Side::Bid, 100, 5, maker);
    let ordinary = place(&mut book, Side::Bid, 90, 5, maker);
    let node = book.read_node(remainder.node_index).unwrap();
    assert!(node.is_taker_origin());
    assert!(node.is_bit_flag_set(OrderBitFlag::TakerOrigin));
    assert_eq!(node.side(), Side::Bid);
    // The bit is per-order, not per-book.
    assert!(!book
        .read_node(ordinary.node_index)
        .unwrap()
        .is_taker_origin());

    // Cancel is the removal velocity's cross resolution uses, and the one that
    // has to say which side was the aggressor.
    assert!(book.cancel(maker, remainder).unwrap().taker_origin);
    assert!(!book.cancel(maker, ordinary).unwrap().taker_origin);

    // Evict and expire report it too: a taker remainder is an ordinary resting
    // order in every other respect, so it can be the worst on its side or run
    // past its `max_ts` like any other.
    let evictable = place_taker_origin(&mut book, Side::Ask, 100, 5, maker);
    let evicted = book.evict_worst(Side::Ask).unwrap();
    assert_eq!(evicted.order_id, evictable.order_id);
    assert!(evicted.taker_origin);

    let expiring = book
        .place(PlaceOrderParams {
            max_ts: 1_000,
            taker_origin: true,
            ..params(Side::Ask, 100, 5, maker)
        })
        .unwrap();
    assert!(book.remove_expired(expiring, 1_001).unwrap().taker_origin);
}

/// The whole point of the marker: a taker remainder resting at 101 with a maker
/// ask at 99 standing against it must not be sold to whoever gets there first at
/// 101 — that maker's 99 belongs to the taker.
#[test]
fn a_crossed_taker_remainder_cannot_be_taken() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    place_taker_origin(&mut book, Side::Bid, 101, 5, taker);
    place(&mut book, Side::Ask, 99, 5, maker);

    assert_err(
        book.execute(Direction::Short, 5, &[], None, 0, 0),
        ClobError::TakerOriginCrossPending,
    );

    // The other direction stays open, and it has to: consuming the
    // counterparty is how velocity resolves the cross (an ordinary fill at the
    // maker's own 99, then `cancel_order_v0` lifts the remainder off the book).
    let outcome = book.execute(Direction::Long, 5, &[], None, 0, 0).unwrap();
    assert_eq!(outcome.fills.len(), 1);
    // With the ask gone the remainder is uncrossed and takeable again.
    assert_eq!(
        book.execute(Direction::Short, 5, &[], None, 0, 0)
            .unwrap()
            .fills
            .len(),
        1
    );
}

/// An ordinary maker×maker cross is unclaimed arbitrage, not somebody's
/// improvement, and freezing takers over it would be a self-inflicted outage.
#[test]
fn a_maker_only_cross_gates_nothing() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (maker_a, maker_b) = (user(0xA), user(0xB));
    place(&mut book, Side::Bid, 101, 5, maker_a);
    place(&mut book, Side::Ask, 99, 5, maker_b);

    assert_eq!(
        book.execute(Direction::Short, 5, &[], None, 0, 0)
            .unwrap()
            .fills
            .len(),
        1
    );
    assert_eq!(
        book.execute(Direction::Long, 5, &[], None, 0, 0)
            .unwrap()
            .fills
            .len(),
        1
    );
}

/// A taker remainder with nothing crossing it is takeable at its own price —
/// that is what a resting order is, and the activation delay is what gave
/// counterparties their chance to beat it.
#[test]
fn an_uncrossed_taker_remainder_fills_normally() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    place_taker_origin(&mut book, Side::Bid, 101, 5, taker);
    place(&mut book, Side::Ask, 105, 5, maker);

    let outcome = book.execute(Direction::Short, 5, &[], None, 0, 0).unwrap();
    assert_eq!(outcome.fills.len(), 1);
}

/// The gate turns on exactly when the counterparty could actually match.
///
/// An order still inside its activation delay — or already expired — cannot be
/// matched by anyone, so a cross that involves one puts no improvement within
/// reach and gating on it would freeze the taker remainder for the whole window
/// for nothing.
#[test]
fn only_a_counterparty_that_could_match_this_slot_gates_the_fill() {
    let taker = user(0xA);
    let maker = user(0xB);

    // Counterparty inside its auction window: the remainder is takeable at 101
    // until the ask activates, and gated from that slot on.
    let market = TestMarket::new(16);
    let mut book = market.book();
    place_at(&mut book, Side::Bid, 101, 5, taker, 0, true);
    place_at(&mut book, Side::Ask, 99, 5, maker, 10, false);
    assert_eq!(
        book.execute(Direction::Short, 1, &[], None, 9, 0)
            .unwrap()
            .fills
            .len(),
        1
    );
    assert_err(
        book.execute(Direction::Short, 1, &[], None, 10, 0),
        ClobError::TakerOriginCrossPending,
    );

    // An expired counterparty is not one either: execute never matches it, and
    // reclaiming it goes through `remove_expired_v0`.
    let market = TestMarket::new(16);
    let mut book = market.book();
    place_at(&mut book, Side::Bid, 101, 5, taker, 0, true);
    book.place(PlaceOrderParams {
        max_ts: 1_000,
        ..params(Side::Ask, 99, 5, maker)
    })
    .unwrap();
    assert_err(
        book.execute(Direction::Short, 1, &[], None, 0, 1_000),
        ClobError::TakerOriginCrossPending,
    );
    assert_eq!(
        book.execute(Direction::Short, 1, &[], None, 0, 1_001)
            .unwrap()
            .fills
            .len(),
        1
    );
}

/// The gate is scoped to the order being filled, not to the book: a sweep that
/// stops short of the crossed remainder is honest liquidity taking and lands,
/// and it is also the exact sweep velocity's cross resolution runs.
#[test]
fn a_sweep_that_stops_short_of_the_remainder_still_lands() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    // Asks: a maker at 99 in front of a taker remainder at 100, both crossed
    // by the bid at 101.
    place(&mut book, Side::Ask, 99, 5, maker);
    place_taker_origin(&mut book, Side::Ask, 100, 5, taker);
    place(&mut book, Side::Bid, 101, 5, user(0xC));

    assert_eq!(
        book.execute(Direction::Long, 5, &[], None, 0, 0)
            .unwrap()
            .fills
            .len(),
        1
    );
    // Reaching past it is what the gate refuses.
    let market = TestMarket::new(16);
    let mut book = market.book();
    place(&mut book, Side::Ask, 99, 5, maker);
    place_taker_origin(&mut book, Side::Ask, 100, 5, taker);
    place(&mut book, Side::Bid, 101, 5, user(0xC));
    assert_err(
        book.execute(Direction::Long, 6, &[], None, 0, 0),
        ClobError::TakerOriginCrossPending,
    );
}

/// A taker remainder skipped for a reason of the *caller's* — self-trade
/// prevention, or a user the caller did not load — never reaches the gate, and
/// a caller's own exclusions never make it look uncrossed either: the
/// counterparty scan deliberately ignores them.
#[test]
fn the_gate_reads_the_book_not_the_callers_set() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    place_taker_origin(&mut book, Side::Bid, 101, 5, taker);
    place(&mut book, Side::Ask, 99, 5, maker);

    // The remainder's own owner sweeping the bid side skips it (self-trade
    // prevention) and fills nothing — no gate, because nothing was filled.
    let outcome = book
        .execute(Direction::Short, 5, &[], Some(&taker), 0, 0)
        .unwrap();
    assert!(outcome.fills.is_empty());

    // A caller that loaded only the remainder's user still sees the maker's ask
    // as a live counterparty, even though it could not settle a fill against
    // it: the cross is a property of the book.
    assert_err(
        book.execute(Direction::Short, 5, &[taker], None, 0, 0),
        ClobError::TakerOriginCrossPending,
    );
}
