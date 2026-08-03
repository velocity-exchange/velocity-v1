//! Response-encoder tests.
//!
//! The point of these: `quote`/`execute` hand-write borsh into the response
//! region instead of serializing [`QuoteResponseV0`] / [`ExecuteResponseV0`],
//! so each test rebuilds the expected value as those types and compares
//! against wincode's own encoding. A divergence between the streamed bytes
//! and the declared schema fails here rather than in velocity.

use {
    super::market::{assert_err, place, test_config, user, TestMarket},
    crate::{
        book::ClobBook,
        error::ClobError,
        response::ResponseWriter,
        state::{
            CancelledRemainderV0, ClobMarketV0, Direction, ExecuteResponseV0, PriceLevel,
            QuoteResponseV0, ResponsePointerV0, Side, UserBalanceChange, RESPONSE_BUFFER_BYTES,
            RESPONSE_OFFSET,
        },
    },
};

fn encode_quote(levels: Vec<PriceLevel>) -> Vec<u8> {
    let mut bytes = Vec::new();
    anchor_lang_v2::wincode::config::serialize_into(
        &mut bytes,
        &QuoteResponseV0 { levels },
        anchor_lang_v2::BORSH_CONFIG,
    )
    .unwrap();
    bytes
}

fn encode_execute(
    balance_changes: Vec<UserBalanceChange>,
    cancelled: Vec<CancelledRemainderV0>,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    anchor_lang_v2::wincode::config::serialize_into(
        &mut bytes,
        &ExecuteResponseV0 {
            balance_changes,
            cancelled,
        },
        anchor_lang_v2::BORSH_CONFIG,
    )
    .unwrap();
    bytes
}

/// The bytes the returned pointer designates.
fn streamed(book: &ClobMarketV0, pointer: ResponsePointerV0) -> Vec<u8> {
    assert_eq!(pointer.offset as usize, RESPONSE_OFFSET);
    book.response[..pointer.len as usize].to_vec()
}

#[test]
fn quote_streams_the_borsh_encoding_of_its_levels() {
    let market = TestMarket::new(16);
    let mut book = market.book();
    let (maker_a, maker_b) = (user(0xA), user(0xB));
    place(&mut book, Side::Ask, 100, 5, maker_a);
    // Same level, later — aggregates into the first level's size.
    place(&mut book, Side::Ask, 100, 7, maker_b);
    place(&mut book, Side::Ask, 101, 10, maker_b);

    let pointer = book.quote(Direction::Long, 100, None, None, 0, 0).unwrap();
    assert_eq!(
        streamed(&book, pointer),
        encode_quote(vec![
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
    let pointer = book.quote(Direction::Long, 6, None, None, 0, 0).unwrap();
    assert_eq!(
        streamed(&book, pointer),
        encode_quote(vec![PriceLevel {
            price: 100,
            size: 6
        }])
    );
    let pointer = book.quote(Direction::Short, 10, None, None, 0, 0).unwrap();
    assert_eq!(streamed(&book, pointer), encode_quote(vec![]));
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
        .quote(Direction::Long, u64::MAX, None, None, 0, 0)
        .unwrap();
    assert_eq!(
        streamed(&book, pointer),
        encode_quote(vec![
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
    // A's second fill completes after B's record is already written, so the
    // id has to be spliced into an earlier record.
    let last = place(&mut book, Side::Ask, 102, 5, maker_a);

    let outcome = book.execute(Direction::Long, 15, None, None, 0, 0).unwrap();
    assert_eq!(
        streamed(&book, outcome.response),
        encode_execute(
            vec![
                UserBalanceChange {
                    user: maker_a,
                    base_size: 10,
                    quote_size: 100 * 5 + 102 * 5,
                    completed_order_ids: vec![first.order_id, last.order_id],
                },
                UserBalanceChange {
                    user: maker_b,
                    base_size: 5,
                    quote_size: 101 * 5,
                    completed_order_ids: vec![middle.order_id],
                },
            ],
            vec![]
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
    assert_eq!(outcome.cancelled_order_id, None);
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
    let outcome = book.execute(Direction::Long, 15, None, None, 0, 0).unwrap();
    assert_eq!(
        streamed(&book, outcome.response),
        encode_execute(
            vec![UserBalanceChange {
                user: maker,
                base_size: 15,
                quote_size: 1500,
                completed_order_ids: vec![],
            }],
            vec![CancelledRemainderV0 {
                user: maker,
                order_id: order.order_id,
                base_asset_amount: 5,
            }]
        )
    );
    assert_eq!(outcome.cancelled_order_id, Some(order.order_id));
    assert_eq!(book.node_count(Side::Ask), 0);
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

    let outcome = book.execute(Direction::Long, 10, None, None, 0, 0).unwrap();
    assert_eq!(
        streamed(&book, outcome.response),
        encode_execute(
            vec![UserBalanceChange {
                user: maker_a,
                base_size: 5,
                quote_size: 500,
                completed_order_ids: vec![first.order_id],
            }],
            vec![]
        )
    );
    // B's order is untouched — a second user would need a second record.
    assert_eq!(book.node_count(Side::Ask), 1);
}

#[test]
fn writer_appends_are_bounded_by_the_response_region() {
    let market = TestMarket::new(4);
    let mut book = market.book();
    let mut writer = ResponseWriter::new();

    assert!(writer.is_empty());
    writer
        .append(&mut book, &vec![0u8; RESPONSE_BUFFER_BYTES - 1])
        .unwrap();
    assert_err(writer.append_u64(&mut book, 0), ClobError::ResponseTooLarge);
    assert_eq!(writer.len(), RESPONSE_BUFFER_BYTES - 1);
    assert_eq!(writer.finish().len as usize, RESPONSE_BUFFER_BYTES - 1);
}

#[test]
fn writer_patches_stay_inside_what_was_written() {
    let market = TestMarket::new(4);
    let mut book = market.book();
    let mut writer = ResponseWriter::new();

    // Nothing written yet: every patch target is out of bounds.
    assert_err(
        writer.patch_count(&mut book, 0, 1),
        ClobError::ResponseTooLarge,
    );
    assert_err(writer.read_count(&book, 0), ClobError::ResponseTooLarge);

    let count = writer.reserve_count(&mut book).unwrap();
    let value = writer.append_u64(&mut book, 7).unwrap();
    writer.patch_count(&mut book, count, 3).unwrap();
    writer.add_u64(&mut book, value, 5).unwrap();
    assert_eq!(writer.read_count(&book, count).unwrap(), 3);
    assert_eq!(writer.read_u64(&book, value).unwrap(), 12);
    // One byte past the written region is still out of bounds.
    assert_err(
        writer.patch_count(&mut book, writer.len() - 3, 1),
        ClobError::ResponseTooLarge,
    );
    assert_err(
        writer.insert_u64(&mut book, writer.len() + 1, 1),
        ClobError::ResponseTooLarge,
    );
}

#[test]
fn writer_inserts_shift_the_tail() {
    let market = TestMarket::new(4);
    let mut book = market.book();
    let mut writer = ResponseWriter::new();
    writer.append(&mut book, &[1, 2, 3, 4]).unwrap();
    writer.append(&mut book, &[9; 8]).unwrap();

    writer
        .insert_u64(&mut book, 4, u64::from_le_bytes([5; 8]))
        .unwrap();
    assert_eq!(writer.len(), 20);
    assert_eq!(&book.response[..4], &[1, 2, 3, 4]);
    assert_eq!(&book.response[4..12], &[5; 8]);
    assert_eq!(&book.response[12..20], &[9; 8]);

    // An insert at the cursor is a plain append — nothing to move.
    writer.insert_u64(&mut book, 20, 0).unwrap();
    assert_eq!(writer.len(), 28);
    assert_eq!(&book.response[20..28], &[0; 8]);
}
