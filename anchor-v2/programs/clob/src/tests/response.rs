//! Response tests.
//!
//! The point of these: `quote`/`execute` stream their records into the
//! response region as the walk produces them, so no test can read back a
//! response type the code serialized. Each one rebuilds the expected value as
//! [`QuoteResponseV0`] / [`ExecuteResponseV0`] and compares against wincode's
//! own encoding of it. A divergence between what the walk wrote and the
//! declared schema fails here rather than in velocity.
//!
//! The framing and the writers themselves are `quoter-spec`'s, and its own
//! tests hold them against its reader. What is left here is this program's
//! half: that the walk produces the right records, in the right order, in a
//! region shaped to hold them.

use {
    super::market::{assert_err, client_id, place, test_config, user, TestMarket},
    crate::{
        book::{ClobBook, NodeArena},
        error::ClobError,
        state::{
            CancelledRemainderV0, ClobMarketV0, ClobSideExt, CompletedOrderV0, Direction,
            ExecuteResponseV0, MarketConfigV0, PartiallyFilledOrderV0, PriceLevel, QuoteResponseV0,
            RemovedOrderV0, ResponsePointerV0, Side, UserBalanceChangeV0, UserCapsV0, UserRefV0,
            CANCELLED_BYTES, CHANGE_BYTES, COMPLETED_BYTES, COUNT_BYTES, EXECUTE_FILLS_CEILING,
            EXECUTE_USERS_CEILING, PRICE_LEVEL_BYTES, QUOTE_LEVELS_CEILING, REMOVED_ORDER_BYTES,
            RESPONSE_BUFFER_BYTES, RESPONSE_LEN_BYTES, RESPONSE_OFFSET, USER_CAPS_BYTES,
            USER_CAPS_CAPACITY, USER_REF_BYTES, USER_SET_CAPACITY, USER_SET_MAX_BYTES,
            WITHHELD_REPORT_BYTES,
        },
    },
};

pub(super) fn encode<T>(value: &T) -> Vec<u8>
where
    T: wincode::SchemaWrite<anchor_lang_v2::BorshConfig, Src = T> + ?Sized,
{
    let mut bytes = Vec::new();
    anchor_lang_v2::wincode::config::serialize_into(
        &mut bytes,
        value,
        anchor_lang_v2::BORSH_CONFIG,
    )
    .unwrap();
    bytes
}

pub(super) fn encode_quote(levels: &[PriceLevel]) -> Vec<u8> {
    wincode::serialize(&QuoteResponseV0 {
        levels,
        withheld: PriceLevel::default(),
    })
    .unwrap()
}

fn encode_execute(
    changes: &[UserBalanceChangeV0],
    cancelled: &[CancelledRemainderV0],
    completed: &[CompletedOrderV0],
    partial: &[PartiallyFilledOrderV0],
) -> Vec<u8> {
    wincode::serialize(&ExecuteResponseV0 {
        changes,
        cancelled,
        completed,
        partial,
    })
    .unwrap()
}

/// A change record with no completed orders attached — the ids ride their own
/// section now, so the tests name them separately.
fn change(user: UserRefV0, base_size: u64, quote_size: u64) -> UserBalanceChangeV0 {
    UserBalanceChangeV0 {
        base_size,
        quote_size,
        user,
        _pad: [0; 6],
    }
}

fn cull(
    user: UserRefV0,
    order_id: u64,
    base_asset_amount: u64,
    price: u64,
) -> CancelledRemainderV0 {
    CancelledRemainderV0 {
        order_id,
        base_asset_amount,
        price,
        client_order_id: client_id(order_id),
        user,
        _pad: [0; 2],
    }
}

fn done(change_index: u32, order_id: u64) -> CompletedOrderV0 {
    CompletedOrderV0 {
        order_id,
        change_index,
        client_order_id: client_id(order_id),
    }
}

fn part(change_index: u32, order_id: u64, base_filled: u64) -> PartiallyFilledOrderV0 {
    PartiallyFilledOrderV0 {
        order_id,
        base_filled,
        client_order_id: client_id(order_id),
        change_index,
    }
}

/// The bytes the returned pointer designates.
pub(super) fn streamed(book: &ClobMarketV0, pointer: ResponsePointerV0) -> Vec<u8> {
    assert_eq!(pointer.offset as usize, RESPONSE_OFFSET);
    book.response[..pointer.len as usize].to_vec()
}

#[test]
fn quote_streams_the_wincode_encoding_of_its_levels() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (maker_a, maker_b) = (user(0xA), user(0xB));
    place(&mut book, Side::Ask, 100, 5, maker_a);
    // Same level, later — aggregates into the first level's size.
    place(&mut book, Side::Ask, 100, 7, maker_b);
    place(&mut book, Side::Ask, 101, 10, maker_b);

    let pointer = book
        .quote(
            Direction::Long,
            100,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            0,
            0,
            0,
        )
        .unwrap();
    assert_eq!(
        streamed(&book, pointer),
        encode_quote(&[
            PriceLevel {
                price: 100,
                size: 12
            },
            PriceLevel {
                price: 101,
                size: 10
            },
        ])
    );

    // Capped at the requested size, and an empty book is an empty vec.
    let pointer = book
        .quote(
            Direction::Long,
            6,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            0,
            0,
            0,
        )
        .unwrap();
    assert_eq!(
        streamed(&book, pointer),
        encode_quote(&[PriceLevel {
            price: 100,
            size: 6
        }])
    );
    let pointer = book
        .quote(
            Direction::Short,
            10,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            0,
            0,
            0,
        )
        .unwrap();
    assert_eq!(streamed(&book, pointer), encode_quote(&[]));
}

#[test]
fn quote_stops_at_the_level_cap() {
    let config = crate::state::MarketConfigV0 {
        max_quote_levels: 2,
        ..test_config()
    };
    let market = TestMarket::new_with(16, config);
    let mut book = market.book();
    let maker = user(1);
    for price in [100, 101, 102] {
        place(&mut book, Side::Ask, price, 1, maker);
    }

    let pointer = book
        .quote(
            Direction::Long,
            u64::MAX,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            0,
            0,
            0,
        )
        .unwrap();
    assert_eq!(
        streamed(&book, pointer),
        encode_quote(&[
            PriceLevel {
                price: 100,
                size: 1
            },
            PriceLevel {
                price: 101,
                size: 1
            },
        ])
    );
}

#[test]
fn execute_streams_balance_changes_merged_by_user() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (maker_a, maker_b) = (user(0xA), user(0xB));
    let first = place(&mut book, Side::Ask, 100, 5, maker_a);
    let middle = place(&mut book, Side::Ask, 101, 5, maker_b);
    // A's second fill completes after B's record is already written. The id
    // names A's change and rides the trailing section, so nothing between them
    // moves.
    let last = place(&mut book, Side::Ask, 102, 5, maker_a);

    let outcome = book
        .execute(Direction::Long, 15, &[], &UserCapsV0::EMPTY, 0, None, 0, 0)
        .unwrap();
    assert_eq!(
        streamed(&book, outcome.response),
        encode_execute(
            &[
                change(maker_a, 10, 100 * 5 + 102 * 5),
                change(maker_b, 5, 101 * 5),
            ],
            &[],
            // Fill order, each naming the change it belongs to.
            &[
                done(0, first.order_id),
                done(1, middle.order_id),
                done(0, last.order_id),
            ],
            &[],
        )
    );
    assert_eq!(book.node_count(Side::Ask), 0);
    assert_eq!(
        outcome
            .fills
            .iter()
            .map(|fill| (fill.order_id, fill.base_size))
            .collect::<Vec<_>>(),
        vec![
            (first.order_id, 5),
            (middle.order_id, 5),
            (last.order_id, 5)
        ]
    );
    assert_eq!(outcome.cancelled_client_order_id, None);
}

#[test]
fn execute_streams_a_sub_min_cull_alongside_the_fill() {
    let config = crate::state::MarketConfigV0 {
        min_order_size: 10,
        ..test_config()
    };
    let market = TestMarket::new_with(16, config);
    let mut book = market.book();
    let maker = user(0xA);
    let order = place(&mut book, Side::Ask, 100, 20, maker);

    // 15 of 20 fills; the 5 left is below min_order_size, so the order is
    // culled with the fill instead of resting as dust.
    let outcome = book
        .execute(Direction::Long, 15, &[], &UserCapsV0::EMPTY, 0, None, 0, 0)
        .unwrap();
    assert_eq!(
        streamed(&book, outcome.response),
        encode_execute(
            &[change(maker, 15, 1500)],
            &[cull(maker, order.order_id, 5, 100)],
            &[],
            &[],
        )
    );
    assert_eq!(
        outcome.cancelled_client_order_id,
        Some(client_id(order.order_id))
    );
    assert_eq!(book.node_count(Side::Ask), 0);
}

/// A balance change merges every order of one maker, so the partial record is
/// the only place a fill says which order it left resting smaller. Without it
/// a maker whose order was half filled cannot be told from one whose order was
/// untouched.
#[test]
fn execute_streams_the_one_order_it_left_resting_smaller() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let maker = user(0xA);
    let first = place(&mut book, Side::Ask, 100, 5, maker);
    let second = place(&mut book, Side::Ask, 101, 20, maker);

    // The first order goes whole; the second gives 10 of its 20 and stays.
    let outcome = book
        .execute(Direction::Long, 15, &[], &UserCapsV0::EMPTY, 0, None, 0, 0)
        .unwrap();
    assert_eq!(
        streamed(&book, outcome.response),
        encode_execute(
            &[change(maker, 15, 100 * 5 + 101 * 10)],
            &[],
            &[done(0, first.order_id)],
            &[part(0, second.order_id, 10)],
        )
    );
    assert_eq!(book.node_count(Side::Ask), 1);
}

/// A walk that ran past an order it did not finish would report two partials,
/// and a reader applying both would credit a fill to an order the book never
/// reached. The reader refuses such a response, so the writer must never be
/// able to produce one: only the last order a walk touches can be partial,
/// because running out of taker size is what ends the walk.
#[test]
fn a_fill_reports_at_most_one_partial() {
    let maker = user(0xA);
    // Sizes that stop mid-order at every depth the book holds.
    for size in [1, 9, 11, 19, 21, 29, 31, 39] {
        let market = TestMarket::new(16);
        let mut book = market.book();
        for _ in 0..4 {
            place(&mut book, Side::Ask, 100, 10, maker);
        }
        let outcome = book
            .execute(
                Direction::Long,
                size,
                &[],
                &UserCapsV0::EMPTY,
                0,
                None,
                0,
                0,
            )
            .unwrap();
        let bytes = streamed(&book, outcome.response);
        let response = ExecuteResponseV0::parse(&bytes).expect("parses");
        assert!(
            response.partial.len() <= 1,
            "size {size} produced {} partials",
            response.partial.len()
        );
        assert_eq!(
            response.partial.len(),
            usize::from(size % 10 != 0),
            "a size that lands on an order boundary leaves nothing partial"
        );
    }
}

#[test]
fn execute_stops_at_the_user_cap() {
    let config = crate::state::MarketConfigV0 {
        max_execute_users: 1,
        ..test_config()
    };
    let market = TestMarket::new_with(16, config);
    let mut book = market.book();
    let (maker_a, maker_b) = (user(0xA), user(0xB));
    let first = place(&mut book, Side::Ask, 100, 5, maker_a);
    place(&mut book, Side::Ask, 101, 5, maker_b);

    let outcome = book
        .execute(Direction::Long, 10, &[], &UserCapsV0::EMPTY, 0, None, 0, 0)
        .unwrap();
    assert_eq!(
        streamed(&book, outcome.response),
        encode_execute(
            &[change(maker_a, 5, 500)],
            &[],
            &[done(0, first.order_id)],
            &[]
        )
    );
    // B's order is untouched — a second user would need a second record.
    assert_eq!(book.node_count(Side::Ask), 1);
}

/// The config ceilings are derived from these widths, so a field added to a
/// wire type has to move them: measure each against wincode's own encoding
/// rather than trusting the arithmetic in `state`.
#[test]
fn wire_widths_match_the_response_types() {
    let user = user(0xA);
    assert_eq!(encode(&user).len(), USER_REF_BYTES);
    assert_eq!(
        encode(&PriceLevel { price: 1, size: 2 }).len(),
        PRICE_LEVEL_BYTES
    );
    assert_eq!(encode(&cull(user, 1, 2, 3)).len(), CANCELLED_BYTES);
    // Return data rather than response bytes, but velocity reads it by offset,
    // so the width and the position of the trailing flag are both pinned.
    let removed = RemovedOrderV0 {
        user,
        order_id: 1,
        client_order_id: 5,
        price: 2,
        base_asset_amount: 3,
        side: Side::Ask,
        taker_origin: true,
        max_ts: 4,
    };
    assert_eq!(encode(&removed).len(), REMOVED_ORDER_BYTES);
    // Side and the taker-origin flag, then the expiry that trails them.
    let tail = REMOVED_ORDER_BYTES - 2 - core::mem::size_of::<i64>();
    assert_eq!(encode(&removed)[tail..tail + 2], [Side::Ask.to_u8(), 1]);
    assert_eq!(encode(&removed)[tail + 2..], 4i64.to_le_bytes());
    assert_eq!(
        encode(&RemovedOrderV0 {
            side: Side::Bid,
            taker_origin: false,
            ..removed
        })[tail..tail + 2],
        [Side::Bid.to_u8(), 0]
    );
    // Every record is one fixed stride: a change carries no ids, so it cannot
    // grow, and a consumed order is its own record in the trailing section.
    assert_eq!(encode(&change(user, 1, 2)).len(), CHANGE_BYTES);
    assert_eq!(encode(&done(0, 1)).len(), COMPLETED_BYTES);
    // And every stride keeps the section after it on the step the records are
    // cast at, which is what lets both programs read them in place.
    for width in [
        CHANGE_BYTES,
        CANCELLED_BYTES,
        COMPLETED_BYTES,
        PRICE_LEVEL_BYTES,
    ] {
        assert_eq!(width % RESPONSE_LEN_BYTES, 0, "{width}");
    }
    assert_eq!(encode(&cull(user, 1, 2, 3)).len(), CANCELLED_BYTES);
    // An empty quote is its length prefix and the withheld report behind it;
    // an empty execute response is one prefix per section.
    assert_eq!(
        encode_quote(&[]).len(),
        RESPONSE_LEN_BYTES + WITHHELD_REPORT_BYTES
    );
    assert_eq!(
        encode_execute(&[], &[], &[], &[]).len(),
        4 * RESPONSE_LEN_BYTES
    );
}

/// A sweep's total quote must be the floor of the whole sweep's notional, not
/// the sum of per-fill floors. Velocity holds the response to the prices this
/// book quoted for the same orders and admits exactly one rounding, so
/// truncating per fill would put an honest fill outside its own quote — and
/// the lost dust would come out of the makers.
///
/// First case: three fills of `3 × 4 / 10 = 1.2` quote units, where the
/// per-fill remainders never accumulate past a whole unit — per-fill floors
/// and the true total `36 / 10 = 3.6` agree at 3. Second: two fills of
/// `7 × 4 / 10 = 2.8`, where they don't — per-fill floors give 4, the true
/// total `56 / 10 = 5.6` gives 5, and that whole unit is one the makers would
/// otherwise have lost.
#[test]
fn execute_totals_the_floor_of_the_whole_sweeps_notional() {
    let market = TestMarket::new_with(
        16,
        MarketConfigV0 {
            base_precision: 10,
            ..test_config()
        },
    );
    let mut book = market.book();
    let maker = user(0xA);
    for _ in 0..3 {
        place(&mut book, Side::Ask, 3, 4, maker);
    }
    let outcome = book
        .execute(Direction::Long, 12, &[], &UserCapsV0::EMPTY, 0, None, 0, 0)
        .unwrap();
    assert_eq!(
        streamed(&book, outcome.response),
        encode_execute(
            &[change(maker, 12, 3)],
            &[],
            &[done(0, 1), done(0, 2), done(0, 3)],
            &[],
        )
    );

    let market = TestMarket::new_with(
        16,
        MarketConfigV0 {
            base_precision: 10,
            ..test_config()
        },
    );
    let mut book = market.book();
    for _ in 0..2 {
        place(&mut book, Side::Ask, 7, 4, maker);
    }
    let outcome = book
        .execute(Direction::Long, 8, &[], &UserCapsV0::EMPTY, 0, None, 0, 0)
        .unwrap();
    assert_eq!(
        streamed(&book, outcome.response),
        encode_execute(&[change(maker, 8, 5)], &[], &[done(0, 1), done(0, 2)], &[])
    );
}

/// The caps sit between the user set and the taker on the wire, so a
/// disagreement between their write schema and their read schema would land
/// on the taker — silently turning self-trade prevention off. Round-trip the
/// whole args struct, not just the caps' width.
#[test]
fn the_args_round_trip_with_caps_between_the_set_and_the_taker() {
    use crate::instructions::quote_v0::QuoteArgsV0;
    let taker = user(0xC);
    let args = QuoteArgsV0 {
        direction: crate::state::Direction::Long,
        size: 12,
        users: &[],
        caps: UserCapsV0::EMPTY,
        reference_price: 0,
        taker: Some(taker),
        limit_price: 0,
    };
    let bytes = encode(&args);
    let decoded: QuoteArgsV0 =
        anchor_lang_v2::wincode::config::deserialize(&bytes, anchor_lang_v2::BORSH_CONFIG).unwrap();
    assert_eq!(decoded.size, 12);
    assert_eq!(
        decoded.taker,
        Some(taker),
        "the taker must survive the caps that now precede it"
    );
}

#[test]
fn the_cap_list_is_fixed_width_on_the_wire() {
    assert_eq!(USER_CAPS_CAPACITY, 8);
    assert_eq!(USER_CAPS_BYTES, 79);
    assert_eq!(encode(&UserCapsV0::EMPTY).len(), USER_CAPS_BYTES);
    let mut full = UserCapsV0::EMPTY;
    full.len = USER_CAPS_CAPACITY as u8;
    assert_eq!(encode(&full).len(), USER_CAPS_BYTES);
}

/// The user set is the one wire type velocity *sends* rather than reads, and
/// this program reads it in place: `users` is a slice into the instruction
/// data, not a copy lifted out of it. The literals here are what velocity's
/// `QUOTER_USER_SET_MAX_BYTES` / `MAX_QUOTER_WIRE_USERS` say (that crate is a
/// separate workspace, so the agreement can only be pinned as numbers): a
/// four-byte count, then that many 34-byte refs, at most 48 of them.
///
/// The count is four bytes because these args ride borsh's framing. The
/// responses in this file use wincode's own configuration, whose prefix is
/// eight — reading one width for the other shifts every field behind it.
#[test]
fn the_user_set_is_read_in_place_and_costs_only_what_it_carries() {
    use crate::instructions::quote_v0::QuoteArgsV0;

    assert_eq!(USER_SET_CAPACITY, 48);
    assert_eq!(USER_SET_MAX_BYTES, 1636);
    assert_eq!(USER_SET_MAX_BYTES, 4 + USER_SET_CAPACITY * USER_REF_BYTES);

    let users = [user(0xA), user(0xB)];
    let args = QuoteArgsV0 {
        users: &users,
        direction: crate::state::Direction::Long,
        size: 7,
        caps: UserCapsV0::EMPTY,
        reference_price: 0,
        taker: None,
        limit_price: 0,
    };
    let bytes = encode(&args);

    // The set leads, counted in four bytes, so its refs land on an even
    // offset — which is the alignment a `UserRefV0` reference needs.
    assert_eq!(bytes[..4], 2u32.to_le_bytes());
    assert_eq!(bytes[4..4 + USER_REF_BYTES], encode(&user(0xA))[..]);
    // The trailing bytes: direction, size, caps, reference price, the absent
    // taker's option tag, and the price bound.
    assert_eq!(
        bytes.len(),
        4 + 2 * USER_REF_BYTES + 1 + 8 + USER_CAPS_BYTES + 8 + 1 + 8
    );

    let decoded: QuoteArgsV0 =
        anchor_lang_v2::wincode::config::deserialize(&bytes, anchor_lang_v2::BORSH_CONFIG).unwrap();
    assert_eq!(decoded.users, &users[..]);
    assert_eq!(
        decoded.users.as_ptr() as usize,
        bytes[4..].as_ptr() as usize,
        "the set is borrowed from the instruction data, not copied out of it"
    );

    // An empty set is unrestricted, and it is what a quote view sends.
    let empty = QuoteArgsV0 { users: &[], ..args };
    let bytes = encode(&empty);
    assert_eq!(bytes[..4], 0u32.to_le_bytes());
    let decoded: QuoteArgsV0 =
        anchor_lang_v2::wincode::config::deserialize(&bytes, anchor_lang_v2::BORSH_CONFIG).unwrap();
    assert!(decoded.users.is_empty());
}

/// The length is structural now, so nothing about the encoding stops a caller
/// from claiming more users than the caps can address. The book refuses such
/// a set rather than treating an unaddressable user as settleable.
#[test]
fn a_user_set_past_the_capacity_is_refused() {
    use crate::state::user_set_within_capacity;
    assert!(user_set_within_capacity(&vec![
        user(0xA);
        USER_SET_CAPACITY
    ]));
    assert!(!user_set_within_capacity(&vec![
        user(0xA);
        USER_SET_CAPACITY + 1
    ]));
}

/// A market configured at the ceilings has to be able to emit the widest
/// response the walk can produce. A maker only enters the response by being
/// filled, and a full fill also writes a completed order — so the widest
/// response is one change per fill plus one completed order each, and both
/// counts are bounded by `max_execute_fills` rather than by
/// `max_execute_users`.
#[test]
fn a_market_at_the_execute_ceilings_streams_a_full_width_response() {
    let fills = EXECUTE_FILLS_CEILING as usize;
    let config = crate::state::MarketConfigV0 {
        max_execute_fills: EXECUTE_FILLS_CEILING,
        max_execute_users: EXECUTE_USERS_CEILING,
        ..test_config()
    };
    let market = TestMarket::new_with(2 * fills as u32, config);
    let mut book = market.book();

    // One order per maker, each a full-width record: distinct user, its own
    // price level, fully consumed.
    // Seeds from 1: a zeroed authority is not a placeable user.
    let makers: Vec<UserRefV0> = (0..fills).map(|i| user(i as u8 + 1)).collect();
    let orders: Vec<_> = makers
        .iter()
        .enumerate()
        .map(|(i, maker)| place(&mut book, Side::Ask, 100 + i as u64, 1, *maker))
        .collect();

    let outcome = book
        .execute(
            Direction::Long,
            fills as u64,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            0,
            0,
        )
        .unwrap();
    let changes: Vec<_> = makers
        .iter()
        .enumerate()
        .map(|(i, maker)| change(*maker, 1, 100 + i as u64))
        .collect();
    let completed: Vec<_> = orders
        .iter()
        .enumerate()
        .map(|(i, order)| done(i as u32, order.order_id))
        .collect();
    let expected = encode_execute(&changes, &[], &completed, &[]);
    assert_eq!(streamed(&book, outcome.response), expected);
    assert_eq!(outcome.fills.len(), fills);
    assert_eq!(book.node_count(Side::Ask), 0);

    // The widest response the encoder can produce, and it fits with room to
    // spare — `ResponseTooLarge` is unreachable at the configured ceilings.
    let widest = 4 * RESPONSE_LEN_BYTES + fills * (CHANGE_BYTES + COMPLETED_BYTES);
    assert_eq!(outcome.response.len as usize, widest);
    assert!(
        widest <= RESPONSE_BUFFER_BYTES,
        "{widest} > the response region"
    );
}

/// Two asks placed 100 then 101, with the node prices overwritten afterwards —
/// a shape `place` cannot build (it rejects a zero price and keeps the list
/// sorted), so it takes hand corruption to reach the response self-check.
fn corrupted_ask_book(prices: [u64; 2]) -> TestMarket {
    let market = TestMarket::new(16);
    {
        let mut book = market.book();
        let maker = user(0xA);
        let first = place(&mut book, Side::Ask, 100, 5, maker);
        let second = place(&mut book, Side::Ask, 101, 5, maker);
        book.update_node(first.node_index, |node| node.price = prices[0])
            .unwrap();
        book.update_node(second.node_index, |node| node.price = prices[1])
            .unwrap();
    }
    market
}

/// Neither instruction may emit a response the router would misprice: a level
/// better than the one in front of it (out-of-order book), or a zero price
/// (which would win every routing waterfall for free).
#[test]
fn a_corrupt_book_cannot_produce_a_response() {
    for prices in [[100, 99], [0, 101]] {
        assert_err(
            corrupted_ask_book(prices).book().quote(
                Direction::Long,
                10,
                &[],
                &UserCapsV0::EMPTY,
                0,
                None,
                0,
                0,
                0,
            ),
            ClobError::InvalidResponseLevel,
        );
        // Execute sweeps the same list and rejects the same shapes. Fresh
        // market: the failed sweep leaves the book part-consumed.
        assert_err(
            corrupted_ask_book(prices).book().execute(
                Direction::Long,
                10,
                &[],
                &UserCapsV0::EMPTY,
                0,
                None,
                0,
                0,
            ),
            ClobError::InvalidResponseLevel,
        );
    }
}

/// Equal prices are the normal case on a level, and bids run the other way —
/// neither may trip the best-first check.
#[test]
fn quote_accepts_the_orders_a_healthy_book_produces() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let maker = user(0xA);
    place(&mut book, Side::Bid, 100, 5, maker);
    place(&mut book, Side::Bid, 100, 5, maker);
    place(&mut book, Side::Bid, 99, 5, maker);

    let pointer = book
        .quote(
            Direction::Short,
            15,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            0,
            0,
            0,
        )
        .unwrap();
    assert_eq!(
        streamed(&book, pointer),
        encode_quote(&[
            PriceLevel {
                price: 100,
                size: 10
            },
            PriceLevel { price: 99, size: 5 },
        ])
    );
    book.execute(Direction::Short, 15, &[], &UserCapsV0::EMPTY, 0, None, 0, 0)
        .unwrap();
}

/// The quote ceiling is the level count that fits the region, so the widest
/// quote response must fit too.
#[test]
fn the_quote_ceiling_fits_the_response_region() {
    // The ladder and the withheld report behind it.
    let widest =
        COUNT_BYTES + QUOTE_LEVELS_CEILING as usize * PRICE_LEVEL_BYTES + WITHHELD_REPORT_BYTES;
    assert!(
        widest <= RESPONSE_BUFFER_BYTES,
        "{widest} > the response region"
    );
    // One more level would not.
    assert!(widest + PRICE_LEVEL_BYTES > RESPONSE_BUFFER_BYTES);
}

/// The writers cast each record onto the region, so a region that does not
/// start on an 8-byte step could not hold a readable response. `RESPONSE_OFFSET`
/// carries that at compile time; this is the runtime half of it, on an account
/// laid out the way the program is handed one.
#[test]
fn the_response_region_is_aligned_for_its_records() {
    let market = TestMarket::new(4);
    let book = market.book();
    assert_eq!(book.response.as_ptr() as usize % RESPONSE_LEN_BYTES, 0);
}

/// The tests above compare the streamed bytes against wincode's encoding of
/// the same records. This one closes the loop the other way: what `execute`
/// wrote into the account has to read back through the spec's own parser,
/// which is the call velocity makes. A framing mistake here is a fill that
/// cannot be decoded rather than one that settles wrong.
#[test]
fn the_streamed_response_parses_back() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (maker_a, maker_b) = (user(0xA), user(0xB));
    let first = place(&mut book, Side::Ask, 100, 5, maker_a);
    let middle = place(&mut book, Side::Ask, 101, 5, maker_b);
    let last = place(&mut book, Side::Ask, 102, 5, maker_a);

    let outcome = book
        .execute(Direction::Long, 15, &[], &UserCapsV0::EMPTY, 0, None, 0, 0)
        .unwrap();
    let bytes = streamed(&book, outcome.response);
    let response = ExecuteResponseV0::parse(&bytes).unwrap();

    assert_eq!(response.changes.len(), 2);
    assert_eq!(response.changes[0].user, maker_a);
    assert_eq!(response.changes[0].base_size, 10);
    assert_eq!(response.changes[1].user, maker_b);
    assert_eq!(response.changes[1].base_size, 5);

    // Each consumed order resolves back to the change that owns it, which is
    // the whole point of naming the change from the id.
    assert_eq!(
        response.completed_for(0).collect::<Vec<_>>(),
        vec![first.order_id, last.order_id]
    );
    assert_eq!(
        response.completed_for(1).collect::<Vec<_>>(),
        vec![middle.order_id]
    );
    assert!(response.cancelled.is_empty());
}

/// A quote is a promise `execute` has to keep, so the two must be able to
/// deliver the same depth. They spend budgets in different units — a level
/// aggregates however many orders sit at one price, while a fill is one order —
/// so a ladder capped only on levels can stand on more orders than one execute
/// is allowed to touch. Before `quote` counted fills, this book quoted five
/// orders' worth in a single level and `execute` delivered three.
#[test]
fn a_quote_promises_no_more_depth_than_execute_can_deliver() {
    let config = MarketConfigV0 {
        max_execute_fills: 3,
        ..test_config()
    };
    let market = TestMarket::new_with(16, config);
    let mut book = market.book();
    let maker = user(0xA);
    // Five orders at one price: one level, five fills.
    for _ in 0..5 {
        place(&mut book, Side::Ask, 100, 1, maker);
    }

    let pointer = book
        .quote(
            Direction::Long,
            100,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            0,
            0,
            0,
        )
        .unwrap();
    let quote_bytes = streamed(&book, pointer);
    let quoted = QuoteResponseV0::parse(&quote_bytes).unwrap();
    let promised: u64 = quoted.levels.iter().map(|level| level.size).sum();
    assert_eq!(promised, 3, "the ladder stops at execute's fill budget");

    // And the promise holds: executing the whole quoted size fills all of it.
    let outcome = book
        .execute(
            Direction::Long,
            promised,
            &[],
            &UserCapsV0::EMPTY,
            0,
            None,
            0,
            0,
        )
        .unwrap();
    let execute_bytes = streamed(&book, outcome.response);
    let executed = ExecuteResponseV0::parse(&execute_bytes).unwrap();
    let filled: u64 = executed.changes.iter().map(|change| change.base_size).sum();
    assert_eq!(
        filled, promised,
        "execute delivered exactly what was quoted"
    );
}
