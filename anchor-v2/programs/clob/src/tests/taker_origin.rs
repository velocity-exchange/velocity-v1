//! Taker-origin orders: the marker on the node, its report on the removal
//! wire, the claimant list it puts the order on, and the reservation that
//! keeps a crossed taker remainder and the depth it crosses out of everyone
//! else's reach.
//!
//! The reservation is two directions of one computation, so most of these
//! tests read both: the remainder is withheld while a counterparty crosses
//! it, and the depth it crosses is withheld from every caller but the crank
//! that owes the taker its improvement.

use {
    super::{
        market::{
            assert_consistent, assert_err, params, place, place_taker_origin,
            test_config as test_market_config, user, TestMarket,
        },
        response::{encode_quote, streamed},
        ACTIVE_SLOT,
    },
    crate::{
        book::{BookHeader, ClobBook, NodeArena},
        error::ClobError,
        state::{
            CancelSidesV0, ClobMarketV0, Direction, L3ResponseV0, MarketConfigV0, OrderBitFlag,
            OrderRefV0, PlaceOrderParams, PriceLevel, QuoteResponseV0, Side, UserCapsV0, UserRefV0,
            L3_ROWS_CEILING,
        },
    },
};

/// The levels a quote published, decoded from the response region.
fn quoted(book: &mut ClobMarketV0, direction: Direction, size: u64, slot: u64) -> Vec<u8> {
    quoted_with(book, direction, size, slot, false)
}

/// The same read the crank that settles a cross makes: every claim ignored.
fn consuming_quote(book: &mut ClobMarketV0, direction: Direction, size: u64, slot: u64) -> Vec<u8> {
    quoted_with(book, direction, size, slot, true)
}

fn quoted_with(
    book: &mut ClobMarketV0,
    direction: Direction,
    size: u64,
    slot: u64,
    include_taker_origin_reservations: bool,
) -> Vec<u8> {
    let pointer = book
        .quote(
            direction,
            size,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            0,
            include_taker_origin_reservations,
            slot,
            0,
        )
        .unwrap();
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
    assert!(
        book.cancel(maker, remainder, ACTIVE_SLOT, false)
            .unwrap()
            .taker_origin
    );
    assert!(
        !book
            .cancel(maker, ordinary, ACTIVE_SLOT, false)
            .unwrap()
            .taker_origin
    );

    // Evict and expire report it too: a taker remainder is an ordinary resting
    // order in every other respect, so it can be the worst on its side or run
    // past its `max_ts` like any other.
    let evictable = place_taker_origin(&mut book, Side::Ask, 100, 5, maker);
    let evicted = book.evict_worst(Side::Ask, ACTIVE_SLOT).unwrap();
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
    let outcome = book
        .execute(
            Direction::Short,
            5,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            false,
            0,
            0,
        )
        .unwrap();
    assert!(outcome.fills.is_empty());
    assert_eq!(
        quoted(&mut book, Direction::Short, u64::MAX, 0),
        encode_quote(&[])
    );

    // And it is still resting, untouched, waiting for its counterparty.
    assert_eq!(book.node_count(Side::Bid), 1);

    // With the ask gone the remainder is uncrossed and takeable again.
    book.cancel(maker, counterparty, ACTIVE_SLOT, false)
        .unwrap();
    assert_eq!(
        book.execute(
            Direction::Short,
            5,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            false,
            0,
            0
        )
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
        encode_quote(&[PriceLevel { price: 98, size: 7 }])
    );

    let outcome = book
        .execute(
            Direction::Short,
            7,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            false,
            0,
            0,
        )
        .unwrap();
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
        book.execute(
            Direction::Short,
            5,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            false,
            0,
            0
        )
        .unwrap()
        .fills
        .len(),
        1
    );
    assert_eq!(
        book.execute(
            Direction::Long,
            5,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            false,
            0,
            0
        )
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
        encode_quote(&[PriceLevel {
            price: 101,
            size: 5
        }])
    );

    let outcome = book
        .execute(
            Direction::Short,
            5,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            false,
            0,
            0,
        )
        .unwrap();
    assert_eq!(outcome.fills.len(), 1);
    assert_eq!(book.node_count(Side::Bid), 0);

    // An empty other side is not a counterparty either.
    let market = TestMarket::new(16);
    let mut book = market.book();
    place_taker_origin(&mut book, Side::Bid, 101, 5, taker);
    assert_eq!(
        quoted(&mut book, Direction::Short, u64::MAX, 0),
        encode_quote(&[PriceLevel {
            price: 101,
            size: 5
        }])
    );
    assert_eq!(
        book.execute(
            Direction::Short,
            5,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            false,
            0,
            0
        )
        .unwrap()
        .fills
        .len(),
        1
    );
}

/// The withholding turns on exactly when the counterparty could actually
/// match.
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
        book.execute(
            Direction::Short,
            1,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            false,
            9,
            0
        )
        .unwrap()
        .fills
        .len(),
        1
    );

    assert!(book
        .execute(
            Direction::Short,
            1,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            false,
            10,
            0
        )
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
        .execute(
            Direction::Short,
            1,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            false,
            0,
            1_000
        )
        .unwrap()
        .fills
        .is_empty());
    assert_eq!(
        book.execute(
            Direction::Short,
            1,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            false,
            0,
            1_001
        )
        .unwrap()
        .fills
        .len(),
        1
    );
}

/// Velocity's cross resolution, end to end on the book: the crank reads the
/// book with the claims consumed, takes the counterparty at its own price, and
/// lifts the remainder off with a cancel that says which of the two was the
/// aggressor. A claim must not get in the way of the one caller it exists for.
#[test]
fn the_cross_resolution_path_still_works() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    let remainder = place_taker_origin(&mut book, Side::Bid, 101, 5, taker);
    place(&mut book, Side::Ask, 99, 5, maker);

    // The counterparty is claimed, so an ordinary caller sees no depth at all.
    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, 0),
        encode_quote(&[])
    );

    // The crank reaches it at its own 99, which is the price the pair settles
    // at and the improvement the remainder came for.
    assert_eq!(
        consuming_quote(&mut book, Direction::Long, u64::MAX, 0),
        encode_quote(&[PriceLevel { price: 99, size: 5 }])
    );

    let outcome = book
        .execute(
            Direction::Long,
            5,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            true,
            0,
            0,
        )
        .unwrap();
    assert_eq!(outcome.fills.len(), 1);
    assert_eq!(book.node_count(Side::Ask), 0);

    // Then the remainder comes off, saying it was the aggressor.
    let removed = book.cancel(taker, remainder, ACTIVE_SLOT, false).unwrap();
    assert!(removed.taker_origin);
    assert_eq!((removed.price, removed.base_asset_amount), (101, 5));
    assert_eq!(book.node_count(Side::Bid), 0);
}

/// The reservation is scoped to the orders it names, not to the book: a sweep
/// hits the maker in front of a crossed remainder, passes over the remainder,
/// and carries on into whatever is behind it.
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

    let outcome = book
        .execute(
            Direction::Long,
            15,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            false,
            0,
            0,
        )
        .unwrap();
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

/// A caller's own exclusions never make a remainder look uncrossed: the
/// counterparty scan deliberately ignores them. They are also applied after
/// the reservation, so two callers reading the same book put the same claim on
/// the same orders.
#[test]
fn the_gate_reads_the_book_not_the_callers_set() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    place_taker_origin(&mut book, Side::Bid, 101, 5, taker);
    place(&mut book, Side::Ask, 99, 5, maker);

    // The remainder's own owner sweeping the bid side skips it for self-trade
    // prevention, and the reservation withholds it from everyone else.
    assert!(book
        .execute(
            Direction::Short,
            5,
            &[],
            &UserCapsV0::EMPTY,
            0,
            Some(&taker),
            false,
            0,
            0
        )
        .unwrap()
        .fills
        .is_empty());

    // A caller that loaded only the remainder's user still sees the maker's ask
    // as a live counterparty, even though it could not settle a fill against it:
    // the cross is a property of the book, not of the caller.
    assert!(book
        .execute(
            Direction::Short,
            5,
            &[taker],
            &UserCapsV0::EMPTY,
            0,
            None,
            false,
            0,
            0
        )
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
        encode_quote(&[
            PriceLevel { price: 99, size: 5 },
            PriceLevel {
                price: 102,
                size: 5
            },
        ])
    );

    // And that is exactly what the fill delivers — 10 base, not 15.
    let outcome = book
        .execute(
            Direction::Long,
            u64::MAX,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            false,
            0,
            0,
        )
        .unwrap();
    assert_eq!(
        outcome.fills.iter().map(|fill| fill.base_size).sum::<u64>(),
        10
    );

    // And the bid the remainder crosses is not ordinary depth for a taker
    // going the other way: the remainder claims it, so it is withheld in that
    // direction too. The two are one computation.
    assert_eq!(
        quoted(&mut book, Direction::Short, u64::MAX, 0),
        encode_quote(&[])
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
        encode_quote(&[
            PriceLevel { price: 99, size: 5 },
            PriceLevel {
                price: 102,
                size: 5
            },
        ])
    );

    book.cancel(crosser, crossing_bid, ACTIVE_SLOT, false)
        .unwrap();
    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, 0),
        encode_quote(&[
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
        book.execute(
            Direction::Long,
            15,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            false,
            0,
            0
        )
        .unwrap()
        .fills
        .len(),
        3
    );
}

/// Quote follows the counterparty's activation slot exactly as execute does:
/// while the crossing ask is inside its own auction window the remainder is
/// ordinary depth, and it drops out of the quote the slot that ask could
/// match. The maker bid behind it is published either way.
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
        encode_quote(&[
            PriceLevel {
                price: 101,
                size: 5
            },
            PriceLevel { price: 98, size: 5 },
        ])
    );
    assert_eq!(
        quoted(&mut book, Direction::Short, u64::MAX, 10),
        encode_quote(&[PriceLevel { price: 98, size: 5 }])
    );
}

/// The bind lasts as long as the remainder's claim. A remainder rests at its
/// own slippage bound so counterparties can compete on price inside the window,
/// and its claim then holds the depth it crosses for the grace. A taker able to
/// withdraw while the claim holds would have a free option on that depth.
#[test]
fn a_bound_remainder_cannot_be_cancelled_until_its_claim_lapses() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    assert_eq!(book.reservation_grace_slots, 32);

    place(&mut book, Side::Ask, 100, 5, maker);
    let remainder = place_at(&mut book, Side::Bid, 100, 5, taker, 10, true);

    for slot in [5, 10, 20, 41] {
        assert_err(
            book.cancel(taker, remainder, slot, false),
            ClobError::TakerOriginBound,
        );
        // The claim still holds the maker's ask at every slot the cancel refuses.
        assert_eq!(
            quoted(&mut book, Direction::Long, u64::MAX, slot),
            encode_quote(&[]),
            "slot {slot}"
        );
    }

    // The claim lapses at activation plus the grace, and the bind ends with it.
    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, 42),
        encode_quote(&[PriceLevel {
            price: 100,
            size: 5
        }])
    );
    assert!(book.cancel(taker, remainder, 42, false).is_ok());
    assert_consistent(&book);
}

/// The sweep reads the same bind as the single cancel.
#[test]
fn cancel_all_keeps_a_remainder_until_its_claim_lapses() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));

    place(&mut book, Side::Ask, 100, 5, maker);
    place_at(&mut book, Side::Bid, 100, 5, taker, 10, true);

    let bound = book
        .cancel_all(taker, CancelSidesV0::Both, 41, false, &mut |_| Ok(()))
        .expect("sweep succeeds");
    assert_eq!(bound.bid_orders, 0);
    assert!(!bound.exhaustive);

    let lapsed = book
        .cancel_all(taker, CancelSidesV0::Both, 42, false, &mut |_| Ok(()))
        .expect("sweep succeeds");
    assert_eq!(lapsed.bid_orders, 1);
    assert!(lapsed.exhaustive);
    assert_consistent(&book);
}

/// Eviction is a permissionless crank, so it must not be a way for the owner
/// to pull a bound remainder. It passes over the remainder and takes the
/// worst order behind it, which still frees a slot on the side.
#[test]
fn eviction_passes_over_a_bound_remainder() {
    let market = TestMarket::new_with(
        16,
        MarketConfigV0 {
            evict_threshold_per_side: 4,
            ..test_market_config()
        },
    );
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));

    let remainder = place_at(&mut book, Side::Bid, 100, 5, taker, 10, true);
    place(&mut book, Side::Bid, 103, 5, maker);
    place(&mut book, Side::Bid, 102, 5, maker);
    let next_worst = place(&mut book, Side::Bid, 101, 5, maker);
    assert_eq!(book.worst(Side::Bid), remainder.node_index);

    let evicted = book.evict_worst(Side::Bid, 5).unwrap();
    assert_eq!(evicted.order_id, next_worst.order_id);
    assert!(!evicted.taker_origin);
    assert_eq!(book.worst(Side::Bid), remainder.node_index);
    assert_consistent(&book);

    // Once the claim lapses the remainder is an ordinary tail again.
    place(&mut book, Side::Bid, 101, 5, maker);
    let evicted = book.evict_worst(Side::Bid, 42).unwrap();
    assert_eq!(evicted.order_id, remainder.order_id);
    assert_consistent(&book);
}

/// A side of nothing but bound remainders has nothing to evict until a claim
/// lapses.
#[test]
fn eviction_refuses_a_side_of_bound_remainders() {
    let market = TestMarket::new_with(
        16,
        MarketConfigV0 {
            evict_threshold_per_side: 2,
            ..test_market_config()
        },
    );
    let mut book = market.book();
    let taker = user(0xA);

    place_at(&mut book, Side::Bid, 100, 5, taker, 10, true);
    place_at(&mut book, Side::Bid, 101, 5, taker, 10, true);

    assert_err(book.evict_worst(Side::Bid, 41), ClobError::TakerOriginBound);
    assert_eq!(book.node_count(Side::Bid), 2);
    assert!(book.evict_worst(Side::Bid, 42).unwrap().taker_origin);
    assert_consistent(&book);
}

/// Liquidation must be able to clear a bound remainder: it is an open order
/// consuming margin like any other.
#[test]
fn force_takes_a_bound_remainder() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let taker = user(0xA);

    place(&mut book, Side::Ask, 100, 5, user(0xB));
    let remainder = place_at(&mut book, Side::Bid, 100, 5, taker, 10, true);
    for slot in [0, 10, 41] {
        assert_err(
            book.cancel(taker, remainder, slot, false),
            ClobError::TakerOriginBound,
        );
    }

    assert!(
        book.cancel(taker, remainder, 20, true)
            .unwrap()
            .taker_origin
    );
}

/// The binding is about the taker-origin flag, not about the activation delay.
/// An ordinary maker quote inside its own window stays withdrawable, because
/// nobody is pricing against a promise it made.
#[test]
fn an_ordinary_order_inside_its_window_is_not_bound() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let maker = user(0xA);

    let quote = place_at(&mut book, Side::Bid, 100, 5, maker, 10, false);
    assert!(book.cancel(maker, quote, 0, false).is_ok());
}

/// A sweep passes a bound remainder over rather than failing, so a maker
/// withdrawing a ladder is not blocked by one order it cannot pull yet. The
/// call reports itself as not exhaustive, which is what says orders remain.
#[test]
fn cancel_all_passes_over_a_bound_remainder_and_says_so() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let owner = user(0xA);

    let bound = place_at(&mut book, Side::Bid, 100, 5, owner, 10, true);
    place_at(&mut book, Side::Bid, 90, 5, owner, 0, false);
    place_at(&mut book, Side::Ask, 110, 5, owner, 0, false);

    let outcome = book
        .cancel_all(owner, CancelSidesV0::Both, 0, false, &mut |_| Ok(()))
        .expect("sweep succeeds");
    // Both ordinary orders left; the bound remainder stayed, on the side it
    // shares with one of them.
    assert_eq!(outcome.bid_orders, 1);
    assert_eq!(outcome.ask_orders, 1);
    assert!(!outcome.exhaustive);
    assert!(book.read_node(bound.node_index).unwrap().is_taker_origin());

    // Force sweeps it with the rest.
    let outcome = book
        .cancel_all(owner, CancelSidesV0::Both, 0, true, &mut |_| Ok(()))
        .expect("sweep succeeds");
    assert_eq!(outcome.bid_orders, 1);
    assert!(outcome.exhaustive);
}

/// The fills an execute delivered, best price first.
fn executed(book: &mut ClobMarketV0, direction: Direction, size: u64, slot: u64) -> Vec<u64> {
    executed_with(book, direction, size, slot, false)
}

fn executed_with(
    book: &mut ClobMarketV0,
    direction: Direction,
    size: u64,
    slot: u64,
    include_taker_origin_reservations: bool,
) -> Vec<u64> {
    book.execute(
        direction,
        size,
        &[],
        &UserCapsV0::EMPTY,
        0,
        None,
        include_taker_origin_reservations,
        slot,
        0,
    )
    .expect("execute succeeds")
    .fills
    .iter()
    .map(|fill| fill.base_size)
    .collect()
}

/// The invariant every other test in this file stands on: a claim takes the
/// best-priced units of the side it crosses, in order, and nothing else.
///
/// A remainder is going to be filled at the best prices the book holds, so the
/// depth it claims has to be the prefix a fill of it would consume. Claim a
/// worse part of the side instead and a taker could still buy the best part
/// out from under it, which is the whole hole this closes.
#[test]
fn a_claim_takes_the_best_priced_prefix_of_the_side() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    place(&mut book, Side::Ask, 100, 3, maker);
    place(&mut book, Side::Ask, 101, 3, maker);
    place(&mut book, Side::Ask, 102, 3, maker);
    // Four base of demand: the whole 100 level, then one of the 101.
    place_taker_origin(&mut book, Side::Bid, 102, 4, taker);

    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, 0),
        encode_quote(&[
            PriceLevel {
                price: 101,
                size: 2
            },
            PriceLevel {
                price: 102,
                size: 3
            },
        ])
    );

    // Nothing moved on the book: a claim is computed, not stored.
    assert_eq!(book.node_count(Side::Ask), 3);
    assert_eq!(book.read_node(book.best(Side::Ask)).unwrap().price, 100);
}

/// The hole this closes. Asks at 100 and 101 with a remainder bidding 102: a
/// taker that could buy the 100 ask and repost it at 101 would sell the
/// remainder its own improvement, one tick at a time, and land the transaction
/// to do it. The 100 ask is claimed, so the take cannot reach it, and the
/// crank that owes the taker the improvement fills there.
#[test]
fn a_take_and_relist_cannot_reach_the_claimed_ask() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    place(&mut book, Side::Ask, 100, 5, maker);
    place(&mut book, Side::Ask, 101, 5, maker);
    place_taker_origin(&mut book, Side::Bid, 102, 5, taker);

    // The 100 level is not on offer at all; the 101 behind it still is.
    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, 0),
        encode_quote(&[PriceLevel {
            price: 101,
            size: 5
        }])
    );
    assert_eq!(executed(&mut book, Direction::Long, 10, 0), vec![5]);
    assert_eq!(book.node_count(Side::Ask), 1);
    assert_eq!(book.read_node(book.best(Side::Ask)).unwrap().price, 100);

    // And the crank reaches it, at 100.
    assert_eq!(
        consuming_quote(&mut book, Direction::Long, u64::MAX, 0),
        encode_quote(&[PriceLevel {
            price: 100,
            size: 5
        }])
    );
    assert_eq!(
        executed_with(&mut book, Direction::Long, 5, 0, true),
        vec![5]
    );
    assert_eq!(book.node_count(Side::Ask), 0);
}

/// A maker that improves on the claimed price inside the auction window is
/// claimed in its turn, and nothing has to be rewritten for that to happen.
/// The claim is positional: the new order is in front, so it is what the
/// remainder crosses first.
#[test]
fn a_better_priced_maker_arriving_mid_window_is_claimed() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker, improver) = (user(0xA), user(0xB), user(0xC));
    // The remainder is still inside its activation window.
    place_at(&mut book, Side::Bid, 102, 5, taker, 10, true);
    let first = place(&mut book, Side::Ask, 101, 5, maker);

    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, 5),
        encode_quote(&[])
    );

    let better = place(&mut book, Side::Ask, 100, 5, improver);
    // The improver holds the claim now, and the maker it cut in front of is
    // ordinary depth again.
    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, 5),
        encode_quote(&[PriceLevel {
            price: 101,
            size: 5
        }])
    );

    // Neither order was touched: same nodes, same ids, same sizes.
    assert_eq!(
        book.read_node(better.node_index).unwrap().order_id,
        better.order_id
    );
    assert_eq!(
        book.read_node(first.node_index).unwrap().order_id,
        first.order_id
    );
    assert_eq!(
        book.read_node(first.node_index).unwrap().base_asset_amount,
        5
    );
}

/// Cover leaving the book hands its claim to whatever the remainder crosses
/// next, and to nothing else. There is no claim to transfer, so an order the
/// remainder does not cross cannot inherit one — which is what would take a
/// competitor's quote dark along with its queue position.
#[test]
fn cancelling_the_claimed_ask_moves_the_claim_and_leaves_the_rest_alone() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    let best = place(&mut book, Side::Ask, 100, 5, maker);
    place(&mut book, Side::Ask, 101, 5, maker);
    let far = place(&mut book, Side::Ask, 105, 5, maker);
    place_taker_origin(&mut book, Side::Bid, 102, 5, taker);

    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, 0),
        encode_quote(&[
            PriceLevel {
                price: 101,
                size: 5
            },
            PriceLevel {
                price: 105,
                size: 5
            },
        ])
    );

    book.cancel(maker, best, ACTIVE_SLOT, false).unwrap();
    assert_consistent(&book);
    // The 101 is claimed because the remainder crosses it. The 105 is not,
    // because no remainder does, and it is still the order it was.
    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, 0),
        encode_quote(&[PriceLevel {
            price: 105,
            size: 5
        }])
    );

    let node = book.read_node(far.node_index).unwrap();
    assert_eq!((node.order_id, node.base_asset_amount), (far.order_id, 5));
    assert_eq!(book.read_node(book.best(Side::Ask)).unwrap().price, 101);
}

/// Two remainders resting at once are served oldest first, because the
/// claimant list is in rest order. The older one holds the best cover, which
/// is the priority the auction promised it.
///
/// A claim also lapses on its own claimant's clock. When the older one's
/// window runs out its cover becomes ordinary depth, and the younger one's
/// claim moves onto the best of what is left. Neither event needs anything
/// written to the book.
#[test]
fn two_claimants_are_served_in_rest_order_and_lapse_one_at_a_time() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (older, younger, maker) = (user(0xA), user(0xB), user(0xC));
    place(&mut book, Side::Ask, 100, 5, maker);
    place(&mut book, Side::Ask, 101, 5, maker);
    // The younger remainder is the better-priced one, and it is still served
    // second: rest order decides, not price.
    place_at(&mut book, Side::Bid, 102, 5, older, 10, true);
    place_at(&mut book, Side::Bid, 103, 5, younger, 20, true);

    // Between them they claim the whole side.
    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, 9),
        encode_quote(&[])
    );

    // The older one holds the 100, so a crank consuming it fills there.
    assert_eq!(
        consuming_quote(&mut book, Direction::Long, 5, 9),
        encode_quote(&[PriceLevel {
            price: 100,
            size: 5
        }])
    );

    // Its claim lapses `reservation_grace_slots` past its activation slot. The
    // younger claim is untouched, and it is now the best cover it holds.
    let lapsed = 10 + book.reservation_grace_slots as u64;
    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, lapsed),
        encode_quote(&[PriceLevel {
            price: 101,
            size: 5
        }])
    );

    // The slot before, both claims still stand.
    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, lapsed - 1),
        encode_quote(&[])
    );
}

/// A remainder that crosses nothing claims nothing. It is on the claimant list
/// like any other, and every order on the other side is ordinary depth.
#[test]
fn a_remainder_that_crosses_nothing_claims_nothing() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    place_taker_origin(&mut book, Side::Bid, 98, 5, taker);
    place(&mut book, Side::Ask, 100, 5, maker);
    place(&mut book, Side::Ask, 101, 5, maker);

    assert_eq!(book.claimant_count(Side::Bid), 1);
    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, 0),
        encode_quote(&[
            PriceLevel {
                price: 100,
                size: 5
            },
            PriceLevel {
                price: 101,
                size: 5
            },
        ])
    );
    assert_eq!(executed(&mut book, Direction::Long, 10, 0), vec![5, 5]);
}

/// A router allocates from the quote and then executes against the
/// allocation, and velocity binds the second to the first. So the two have to
/// withhold the same units, down to the order a claim only partly covers.
#[test]
fn quote_and_execute_withhold_the_same_units() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    place(&mut book, Side::Ask, 100, 3, maker);
    place(&mut book, Side::Ask, 101, 3, maker);
    place(&mut book, Side::Ask, 102, 3, maker);
    place_taker_origin(&mut book, Side::Bid, 102, 4, taker);

    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, 0),
        encode_quote(&[
            PriceLevel {
                price: 101,
                size: 2
            },
            PriceLevel {
                price: 102,
                size: 3
            },
        ])
    );

    // Five base, the same five the ladder published, and the claimed unit of
    // the 101 stays resting.
    assert_eq!(
        executed(&mut book, Direction::Long, u64::MAX, 0),
        vec![2, 3]
    );
    assert_eq!(book.node_count(Side::Ask), 2);
    let partial = book.read_node(book.best(Side::Ask)).unwrap();
    assert_eq!((partial.price, partial.base_asset_amount), (100, 3));
}

/// The claimant list is maintained by the two functions every placement and
/// every removal already funnel through, so no path can forget it. This drives
/// each of them over a taker-origin order and checks the list after every one.
///
/// [`assert_consistent`] is what checks it: every listed order live, on its
/// side and taker-origin, the links mutual, the ids ascending, and the count
/// and tail agreeing with the walk.
#[test]
fn every_removal_path_maintains_the_claimant_list() {
    let market = TestMarket::new_with(
        16,
        MarketConfigV0 {
            // A floor, so a fill can leave a remainder small enough to cull.
            min_order_size: 3,
            ..test_market_config()
        },
    );
    let mut book = market.book();
    let owner = user(0xA);

    // Place: two remainders on one side and one on the other.
    let cancelled = place_taker_origin(&mut book, Side::Bid, 100, 5, owner);
    let evicted = place_taker_origin(&mut book, Side::Bid, 90, 5, owner);
    let expiring = book
        .place(PlaceOrderParams {
            max_ts: 1_000,
            taker_origin: true,
            ..params(Side::Ask, 200, 5, owner)
        })
        .unwrap();
    assert_consistent(&book);
    assert_eq!(book.claimant_count(Side::Bid), 2);
    assert_eq!(book.claimant_count(Side::Ask), 1);

    // Cancel takes the head of a list of two.
    book.cancel(owner, cancelled, ACTIVE_SLOT, false).unwrap();
    assert_consistent(&book);
    assert_eq!(book.claimant_count(Side::Bid), 1);

    // Eviction takes the tail, which is now also the head.
    assert_eq!(
        book.evict_worst(Side::Bid, ACTIVE_SLOT).unwrap().order_id,
        evicted.order_id
    );

    assert_consistent(&book);
    assert_eq!(book.claimant_count(Side::Bid), 0);

    // Expiry reclamation.
    book.remove_expired(expiring, 1_001).unwrap();
    assert_consistent(&book);
    assert_eq!(book.claimant_count(Side::Ask), 0);

    // `fill_v0` shrinking a remainder in place touches no link, and culling
    // its sub-minimum leftover goes through the same unlink as the rest.
    let filled = place_taker_origin(&mut book, Side::Ask, 200, 9, owner);
    assert!(!book.fill(filled, 3, ACTIVE_SLOT, 0).unwrap().removed);
    assert_consistent(&book);
    assert_eq!(book.claimant_count(Side::Ask), 1);
    let outcome = book.fill(filled, 4, ACTIVE_SLOT, 0).unwrap();
    assert!(outcome.removed && outcome.culled_base_asset_amount == 2);
    assert_consistent(&book);
    assert_eq!(book.claimant_count(Side::Ask), 0);

    // Execute culling a sub-minimum remainder of an uncrossed remainder.
    let culled = place_taker_origin(&mut book, Side::Ask, 200, 5, owner);
    assert_eq!(executed(&mut book, Direction::Long, 3, 0), vec![3]);
    assert_consistent(&book);
    assert_eq!(book.claimant_count(Side::Ask), 0);
    assert!(book.read_node(culled.node_index).unwrap().order_id != culled.order_id);
}

/// The withheld report says how much depth a caller would reach by loading the
/// absent owner. Units a remainder claims stay out of reach either way, so the
/// report leaves them out.
#[test]
fn the_withheld_report_leaves_out_claimed_depth() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, absent_maker, loaded) = (user(0xA), user(0xB), user(0xC));

    place(&mut book, Side::Ask, 100, 5, absent_maker);
    place_taker_origin(&mut book, Side::Bid, 100, 3, taker);

    let pointer = book
        .quote(
            Direction::Long,
            u64::MAX,
            &[loaded],
            &UserCapsV0::EMPTY,
            0,
            None,
            0,
            false,
            5,
            0,
        )
        .unwrap();
    let bytes = streamed(&book, pointer);
    let response = QuoteResponseV0::parse(&bytes).unwrap();
    assert!(response.levels.is_empty());
    assert_eq!(
        response.withheld,
        PriceLevel {
            price: 100,
            size: 2
        }
    );
}

/// Velocity reports a fill only against a remainder the book offers to someone,
/// so an expired or unactivated remainder refuses it and stays untouched.
#[test]
fn fill_refuses_a_remainder_that_is_not_live() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let taker = user(0xA);

    let expired = book
        .place(PlaceOrderParams {
            max_ts: 1_000,
            taker_origin: true,
            ..params(Side::Bid, 100, 5, taker)
        })
        .unwrap();
    assert_err(
        book.fill(expired, 2, ACTIVE_SLOT, 1_001),
        ClobError::OrderNotLive,
    );
    assert!(book.fill(expired, 2, ACTIVE_SLOT, 1_000).is_ok());

    let unactivated = place_at(&mut book, Side::Ask, 110, 5, taker, 10, true);
    assert_err(book.fill(unactivated, 2, 9, 0), ClobError::OrderNotLive);
    assert_eq!(
        book.read_node(unactivated.node_index)
            .unwrap()
            .base_asset_amount,
        5
    );
    assert!(book.fill(unactivated, 2, 10, 0).is_ok());
    assert_consistent(&book);
}

/// The order-by-order view reports the claim rather than hiding it. The row's
/// size is what a caller may take, and the flag says why it is short of what
/// the order holds — which is what an account-set builder and a book display
/// each need, and what a caller resolving the cross reads through with
/// `include_taker_origin_reservations`.
#[test]
fn quote_l3_reports_the_claim_on_the_row() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    place(&mut book, Side::Ask, 100, 3, maker);
    place(&mut book, Side::Ask, 101, 3, maker);
    place_taker_origin(&mut book, Side::Bid, 102, 4, taker);

    let rows = |book: &mut ClobMarketV0, consume: bool| -> Vec<(u64, u64, bool)> {
        let pointer = book
            .quote_l3(Direction::Long, 0, L3_ROWS_CEILING, consume, 0, 0)
            .unwrap();
        let bytes = streamed(book, pointer);
        L3ResponseV0::parse(&bytes)
            .unwrap()
            .rows
            .iter()
            .map(|row| {
                (
                    row.price,
                    row.size,
                    row.flags & quoter_spec::L3_ROW_FLAG_RESERVED != 0,
                )
            })
            .collect()
    };

    // The 100 is claimed whole and the 101 in part.
    assert_eq!(rows(&mut book, false), vec![(100, 0, true), (101, 2, true)]);
    // The caller that settles the cross sees both orders as they rest.
    assert_eq!(
        rows(&mut book, true),
        vec![(100, 3, false), (101, 3, false)]
    );
}

/// A claimant whose demand outlasts one cover order must not carry that demand
/// onto cover it does not cross. With asks at 100 and 105 and a remainder
/// bidding 102 for more than the 100 holds, the leftover demand crosses
/// nothing, so the 105 is ordinary depth.
#[test]
fn leftover_demand_does_not_claim_cover_the_claimant_cannot_cross() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (taker, maker) = (user(0xA), user(0xB));
    place(&mut book, Side::Ask, 100, 5, maker);
    place(&mut book, Side::Ask, 105, 5, maker);
    place_taker_origin(&mut book, Side::Bid, 102, 10, taker);

    assert_eq!(
        quoted(&mut book, Direction::Long, u64::MAX, 0),
        encode_quote(&[PriceLevel {
            price: 105,
            size: 5
        }])
    );
    assert_eq!(executed(&mut book, Direction::Long, u64::MAX, 0), vec![5]);
}
