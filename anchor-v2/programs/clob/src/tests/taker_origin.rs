//! Taker-origin orders: the marker on the node, its report on the removal
//! wire, and the gate that stops a crossed taker remainder being taken at its
//! own price — in execute, which fails, and in quote, which must publish only
//! the depth execute can still deliver.

use {
    super::{
        market::{params, place, place_taker_origin, user, TestMarket},
        response::{encode_quote, streamed},
    },
    crate::{
        book::{ClobBook, NodeArena},
        state::{
            ClobMarketV0, Direction, OrderBitFlag, OrderRefV0, PlaceOrderParams, PriceLevel, Side,
            UserRefV0,
        },
    },
};

/// The levels a quote published, decoded from the response region.
fn quoted(book: &mut ClobMarketV0, direction: Direction, size: u64, slot: u64) -> Vec<u8> {
    let pointer = book.quote(direction, size, &[], None, slot, 0).unwrap();
    streamed(book, pointer)
}

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
/// 101 — that maker's 99 belongs to the taker. It comes out of the matchable set
/// while the cross stands, the way an expired order does.
#[test]
fn a_crossed_taker_remainder_is_passed_over() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    place_taker_origin(&mut book, Side::Bid, 101, 5, taker);
    let counterparty = place(&mut book, Side::Ask, 99, 5, maker);

    // Nothing else rests on the bid side, so a taker going that way finds no
    // depth at all — but the call lands, it just fills nothing.
    let outcome = book.execute(Direction::Short, 5, &[], None, 0, 0).unwrap();
    assert!(outcome.fills.is_empty());
    assert_eq!(
        quoted(&mut book, Direction::Short, u64::MAX, 0),
        encode_quote(vec![])
    );
    // And it is still resting, untouched, waiting for its counterparty.
    assert_eq!(book.node_count(Side::Bid), 1);

    // With the ask gone the remainder is uncrossed and takeable again.
    book.cancel(maker, counterparty).unwrap();
    assert_eq!(
        book.execute(Direction::Short, 5, &[], None, 0, 0)
            .unwrap()
            .fills
            .len(),
        1
    );
}

/// The liveness property skipping buys, and the reason it beats failing the
/// call: a crossed remainder rests at a slippage bound, so it is normally at or
/// near the front of its side. Passing over it leaves everything behind it
/// tradeable; failing on it would have taken the whole side dark for as long as
/// the cross stood.
#[test]
fn a_crossed_remainder_does_not_shadow_the_depth_behind_it() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    // The remainder is the best bid; an ordinary maker bid rests behind it.
    place_taker_origin(&mut book, Side::Bid, 101, 5, taker);
    place(&mut book, Side::Bid, 98, 7, maker);
    place(&mut book, Side::Ask, 99, 5, maker);

    assert_eq!(
        quoted(&mut book, Direction::Short, u64::MAX, 0),
        encode_quote(vec![PriceLevel { price: 98, size: 7 }])
    );
    let outcome = book.execute(Direction::Short, 7, &[], None, 0, 0).unwrap();
    assert_eq!(outcome.fills.len(), 1);
    // The maker's bid filled; the remainder is still there.
    assert_eq!(book.node_count(Side::Bid), 1);
    assert!(book
        .read_node(book.best(Side::Bid))
        .unwrap()
        .is_taker_origin());
}

/// An ordinary maker×maker cross is unclaimed arbitrage, not somebody's
/// improvement, and costing takers anything over it would be a self-inflicted
/// outage.
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

/// A taker remainder with nothing crossing it is ordinary depth: quotable and
/// takeable at its own price like any other resting order. This is the fallback
/// when no maker lines up during the auction window, and it is how the remainder
/// eventually fills if the mechanism finds nobody — so it must not be gated.
#[test]
fn an_uncrossed_taker_remainder_is_quotable_and_takeable() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    place_taker_origin(&mut book, Side::Bid, 101, 5, taker);
    // Best ask is above the bid, so nothing crosses.
    place(&mut book, Side::Ask, 105, 5, maker);

    assert_eq!(
        quoted(&mut book, Direction::Short, u64::MAX, 0),
        encode_quote(vec![PriceLevel {
            price: 101,
            size: 5
        }])
    );
    let outcome = book.execute(Direction::Short, 5, &[], None, 0, 0).unwrap();
    assert_eq!(outcome.fills.len(), 1);
    assert_eq!(book.node_count(Side::Bid), 0);

    // An empty other side is not a counterparty either.
    let market = TestMarket::new(16);
    let mut book = market.book();
    place_taker_origin(&mut book, Side::Bid, 101, 5, taker);
    assert_eq!(
        quoted(&mut book, Direction::Short, u64::MAX, 0),
        encode_quote(vec![PriceLevel {
            price: 101,
            size: 5
        }])
    );
    assert_eq!(
        book.execute(Direction::Short, 5, &[], None, 0, 0)
            .unwrap()
            .fills
            .len(),
        1
    );
}

/// The gate turns on exactly when the counterparty could actually match.
///
/// An order still inside its activation delay — or already expired — cannot be
/// matched by anyone, so a cross that involves one puts no improvement within
/// reach and holding the remainder back then would cost the book depth for
/// nothing.
#[test]
fn only_a_counterparty_that_could_match_this_slot_gates_the_fill() {
    let taker = user(0xA);
    let maker = user(0xB);

    // Counterparty inside its auction window: the remainder is takeable at 101
    // until the ask activates, and held back from that slot on.
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
    assert!(book
        .execute(Direction::Short, 1, &[], None, 10, 0)
        .unwrap()
        .fills
        .is_empty());

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
    assert!(book
        .execute(Direction::Short, 1, &[], None, 0, 1_000)
        .unwrap()
        .fills
        .is_empty());
    assert_eq!(
        book.execute(Direction::Short, 1, &[], None, 0, 1_001)
            .unwrap()
            .fills
            .len(),
        1
    );
}

/// Velocity's cross resolution, end to end on the book: consume the
/// counterparty with an ordinary execute at its own price — it is a maker and is
/// never skipped — then lift the remainder off with a cancel, which reports
/// which of the two was the aggressor. Skipping must not get in the way of
/// this, in either leg.
#[test]
fn the_cross_resolution_path_still_works() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    let remainder = place_taker_origin(&mut book, Side::Bid, 101, 5, taker);
    place(&mut book, Side::Ask, 99, 5, maker);

    // The counterparty is quotable and fillable at its own 99 while the cross
    // stands — this is the leg velocity runs, and the price the pair settles at.
    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, 0),
        encode_quote(vec![PriceLevel { price: 99, size: 5 }])
    );
    let outcome = book.execute(Direction::Long, 5, &[], None, 0, 0).unwrap();
    assert_eq!(outcome.fills.len(), 1);
    assert_eq!(book.node_count(Side::Ask), 0);

    // Then the remainder comes off, saying it was the aggressor.
    let removed = book.cancel(taker, remainder).unwrap();
    assert!(removed.taker_origin);
    assert_eq!((removed.price, removed.base_asset_amount), (101, 5));
    assert_eq!(book.node_count(Side::Bid), 0);
}

/// The gate is scoped to the order being filled, not to the book: a sweep hits
/// the maker in front of a crossed remainder, passes over the remainder, and
/// carries on into whatever is behind it.
#[test]
fn a_sweep_fills_around_the_remainder() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    // Asks: a maker at 99, a taker remainder at 100, a maker at 102 — the bid at
    // 101 crosses the first two.
    place(&mut book, Side::Ask, 99, 5, maker);
    place_taker_origin(&mut book, Side::Ask, 100, 5, taker);
    place(&mut book, Side::Ask, 102, 5, maker);
    place(&mut book, Side::Bid, 101, 5, user(0xC));

    let outcome = book.execute(Direction::Long, 15, &[], None, 0, 0).unwrap();
    assert_eq!(
        outcome
            .fills
            .iter()
            .map(|fill| fill.base_size)
            .collect::<Vec<_>>(),
        vec![5, 5]
    );
    // Both makers gone, the remainder still resting.
    assert_eq!(book.node_count(Side::Ask), 1);
    assert!(book
        .read_node(book.best(Side::Ask))
        .unwrap()
        .is_taker_origin());
}

/// A taker remainder skipped for a reason of the *caller's* — self-trade
/// prevention, or a user the caller did not load — never reaches the gate, and a
/// caller's own exclusions never make it look uncrossed either: the counterparty
/// scan deliberately ignores them.
#[test]
fn the_gate_reads_the_book_not_the_callers_set() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    place_taker_origin(&mut book, Side::Bid, 101, 5, taker);
    place(&mut book, Side::Ask, 99, 5, maker);

    // The remainder's own owner sweeping the bid side skips it for self-trade
    // prevention, before the gate is consulted at all.
    assert!(book
        .execute(Direction::Short, 5, &[], Some(&taker), 0, 0)
        .unwrap()
        .fills
        .is_empty());

    // A caller that loaded only the remainder's user still sees the maker's ask
    // as a live counterparty, even though it could not settle a fill against it:
    // the cross is a property of the book, not of the caller.
    assert!(book
        .execute(Direction::Short, 5, &[taker], None, 0, 0)
        .unwrap()
        .fills
        .is_empty());
}

/// A router allocates from the quote and then executes against the allocation,
/// and velocity binds the second to the first — so the depth the two see has to
/// agree. Both skip the crossed remainder, and both keep the ordinary maker depth
/// on either side of it.
#[test]
fn quote_and_execute_skip_the_same_order() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    place(&mut book, Side::Ask, 99, 5, maker);
    place_taker_origin(&mut book, Side::Ask, 100, 5, taker);
    place(&mut book, Side::Ask, 102, 5, maker);
    place(&mut book, Side::Bid, 101, 5, user(0xC));

    // The remainder's level is absent; the maker levels in front of and behind
    // it are both published, because execute really can deliver them.
    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, 0),
        encode_quote(vec![
            PriceLevel { price: 99, size: 5 },
            PriceLevel {
                price: 102,
                size: 5
            },
        ])
    );
    // And that is exactly what the fill delivers — 10 base, not 15.
    let outcome = book
        .execute(Direction::Long, u64::MAX, &[], None, 0, 0)
        .unwrap();
    assert_eq!(
        outcome.fills.iter().map(|fill| fill.base_size).sum::<u64>(),
        10
    );

    // The crossing bid is ordinary depth for a taker going the other way.
    assert_eq!(
        quoted(&mut book, Direction::Short, u64::MAX, 0),
        encode_quote(vec![PriceLevel {
            price: 101,
            size: 5
        }])
    );
}

/// Nothing about the order changed — only that a counterparty was standing
/// against it. Once the cross is gone the remainder is ordinary depth again, at
/// its own price.
#[test]
fn the_same_book_quotes_that_depth_once_the_cross_is_gone() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker, crosser) = (user(0xA), user(0xB), user(0xC));
    place(&mut book, Side::Ask, 99, 5, maker);
    place_taker_origin(&mut book, Side::Ask, 100, 5, taker);
    place(&mut book, Side::Ask, 102, 5, maker);
    let crossing_bid = place(&mut book, Side::Bid, 101, 5, crosser);

    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, 0),
        encode_quote(vec![
            PriceLevel { price: 99, size: 5 },
            PriceLevel {
                price: 102,
                size: 5
            },
        ])
    );

    book.cancel(crosser, crossing_bid).unwrap();
    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, 0),
        encode_quote(vec![
            PriceLevel { price: 99, size: 5 },
            PriceLevel {
                price: 100,
                size: 5
            },
            PriceLevel {
                price: 102,
                size: 5
            },
        ])
    );
    // Execute agrees, which is the whole point of the two sharing a predicate.
    assert_eq!(
        book.execute(Direction::Long, 15, &[], None, 0, 0)
            .unwrap()
            .fills
            .len(),
        3
    );
}

/// The gate turns on with the counterparty's activation slot in quote exactly as
/// it does in execute: while the crossing ask is inside its own auction window
/// the remainder is ordinary depth, and it drops out of the quote the slot that
/// ask could match — leaving the maker bid behind it published either way.
#[test]
fn quote_follows_the_counterpartys_activation_slot() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    place_at(&mut book, Side::Bid, 101, 5, taker, 0, true);
    place_at(&mut book, Side::Bid, 98, 5, maker, 0, false);
    place_at(&mut book, Side::Ask, 99, 5, maker, 10, false);

    assert_eq!(
        quoted(&mut book, Direction::Short, u64::MAX, 9),
        encode_quote(vec![
            PriceLevel {
                price: 101,
                size: 5
            },
            PriceLevel { price: 98, size: 5 },
        ])
    );
    assert_eq!(
        quoted(&mut book, Direction::Short, u64::MAX, 10),
        encode_quote(vec![PriceLevel { price: 98, size: 5 }])
    );
}
