//! Pins the wire layout, and holds the writers to what the readers parse.

use super::*;

/// A response region on the 8-byte step a real account gives one. Backed
/// by `u64`s because the records are cast in place at both ends of the
/// wire, and a `Vec<u8>` promises no more than byte alignment.
struct Region(Vec<u64>);

impl Region {
    fn new(bytes: usize) -> Self {
        Self(vec![0; bytes.div_ceil(LEN_BYTES)])
    }

    fn bytes(&mut self) -> &mut [u8] {
        bytemuck::cast_slice_mut(&mut self.0)
    }
}

fn user(seed: u8, sub: u16) -> UserRefV0 {
    UserRefV0 {
        authority: Pubkey::new_from_array([seed; 32]),
        sub_account_id: sub,
    }
}

fn changes() -> [UserBalanceChangeV0; 2] {
    [
        UserBalanceChangeV0 {
            base_size: 1_000_000_000,
            quote_size: 101_000_000,
            user: user(7, 3),
            _pad: [0; 6],
        },
        UserBalanceChangeV0 {
            base_size: 5,
            quote_size: 6,
            user: user(8, 0),
            _pad: [0; 6],
        },
    ]
}

fn cancelled() -> [CancelledRemainderV0; 1] {
    [CancelledRemainderV0 {
        order_id: 42,
        base_asset_amount: 17,
        price: 101,
        client_order_id: 420,
        user: user(9, 1),
        flags: L3_ROW_FLAG_REDUCE_ONLY,
        _pad: [0; 1],
    }]
}

fn completed() -> [CompletedOrderV0; 2] {
    [
        CompletedOrderV0 {
            order_id: 9,
            change_index: 0,
            client_order_id: 90,
            flags: 0,
            _pad: [0; 1],
        },
        CompletedOrderV0 {
            order_id: 10,
            change_index: 0,
            client_order_id: 100,
            flags: 0,
            _pad: [0; 1],
        },
    ]
}

fn partial() -> [PartiallyFilledOrderV0; 1] {
    [PartiallyFilledOrderV0 {
        order_id: 11,
        base_filled: 3,
        client_order_id: 110,
        change_index: 0,
        _pad: [0; 2],
    }]
}

#[test]
fn round_trips_without_copying() {
    let (c, x, d, p) = (changes(), cancelled(), completed(), partial());
    let response = ExecuteResponseV0 {
        changes: &c,
        cancelled: &x,
        completed: &d,
        partial: &p,
    };
    let bytes = wincode::serialize(&response).unwrap();
    let back = ExecuteResponseV0::parse(&bytes).unwrap();
    assert_eq!(back, response);

    // The slices point into the buffer rather than at copies of it.
    let base = bytes.as_ptr() as usize;
    let borrowed = back.changes.as_ptr() as usize;
    assert!(
        borrowed > base && borrowed < base + bytes.len(),
        "changes must borrow from the response buffer"
    );
}

#[test]
fn completed_orders_attach_to_their_change() {
    let (c, x, d, p) = (changes(), cancelled(), completed(), partial());
    let bytes = wincode::serialize(&ExecuteResponseV0 {
        changes: &c,
        cancelled: &x,
        completed: &d,
        partial: &p,
    })
    .unwrap();
    let response = ExecuteResponseV0::parse(&bytes).unwrap();
    assert_eq!(
        response
            .completed_for(0)
            .map(|entry| entry.order_id)
            .collect::<Vec<_>>(),
        vec![9, 10]
    );
    assert!(response.cancelled[0].is_reduce_only());
    assert_eq!(response.completed_for(1).count(), 0);
}

/// An id naming a change that is not there would otherwise unwind whatever
/// record sits at that index.
#[test]
fn a_dangling_completed_order_is_rejected() {
    let (c, x) = (changes(), cancelled());
    let bytes = wincode::serialize(&ExecuteResponseV0 {
        changes: &c,
        cancelled: &x,
        completed: &[CompletedOrderV0 {
            order_id: 1,
            change_index: 9,
            client_order_id: 0,
            flags: 0,
            _pad: [0; 1],
        }],

        partial: &[],
    })
    .unwrap();
    assert_eq!(
        ExecuteResponseV0::parse(&bytes),
        Err(SpecError::DanglingCompletedOrder)
    );
}

#[test]
fn truncation_is_an_error_not_a_panic() {
    let (c, x, d, p) = (changes(), cancelled(), completed(), partial());
    let bytes = wincode::serialize(&ExecuteResponseV0 {
        changes: &c,
        cancelled: &x,
        completed: &d,
        partial: &p,
    })
    .unwrap();
    for cut in 0..bytes.len() {
        assert!(
            ExecuteResponseV0::parse(&bytes[..cut]).is_err(),
            "truncating to {cut} bytes must be an error"
        );
    }
}

#[test]
fn quote_round_trips() {
    let levels = [
        PriceLevelV0 {
            price: 100_000_000,
            size: 5,
        },
        PriceLevelV0 {
            price: 99_000_000,
            size: 7,
        },
    ];
    let bytes = wincode::serialize(&QuoteResponseV0 {
        levels: &levels,
        withheld: PriceLevelV0::default(),
    })
    .unwrap();
    let response = QuoteResponseV0::parse(&bytes).unwrap();
    assert_eq!(response.levels, levels.as_slice());
}

/// The writers and the reader are the two halves of this crate, and this is
/// what holds them together: bytes a writer produced must equal wincode's
/// encoding of the same response, and must parse back.
#[test]
fn the_execute_writer_writes_what_the_reader_reads() {
    let (c, x, d, p) = (changes(), cancelled(), completed(), partial());
    let mut region = Region::new(512);

    let mut writer = ExecuteWriter::new();
    // Streamed the way a fill produces them: two changes appended, then
    // the first one found again and added into, as a repeat maker does.
    let first = writer.push_change(region.bytes(), c[0]).unwrap();
    writer.push_change(region.bytes(), c[1]).unwrap();
    let found = writer
        .changes(region.bytes())
        .unwrap()
        .iter()
        .position(|change| change.user == c[0].user)
        .unwrap();
    assert_eq!(found, first as usize);
    let record = writer.change_mut(region.bytes(), first).unwrap();
    record.base_size += 3;
    record.quote_size += 4;
    let len = writer.finish(region.bytes(), &x, &d, &p).unwrap();

    let mut merged = c;
    merged[0].base_size += 3;
    merged[0].quote_size += 4;
    let expected = ExecuteResponseV0 {
        changes: &merged,
        cancelled: &x,
        completed: &d,
        partial: &p,
    };

    assert_eq!(
        &region.bytes()[..len],
        wincode::serialize(&expected).unwrap().as_slice()
    );
    assert_eq!(
        ExecuteResponseV0::parse(&region.bytes()[..len]).unwrap(),
        expected
    );
}

#[test]
fn the_quote_writer_writes_what_the_reader_reads() {
    let levels = [
        PriceLevelV0 {
            price: 100,
            size: 5,
        },
        PriceLevelV0 { price: 99, size: 7 },
    ];
    let withheld = PriceLevelV0 {
        price: 98,
        size: 11,
    };
    let mut region = Region::new(256);

    let mut writer = QuoteWriter::new();
    for level in levels {
        writer.push_level(region.bytes(), level).unwrap();
    }

    assert_eq!(writer.levels(), levels.len());
    let len = writer.finish(region.bytes(), withheld).unwrap();

    assert_eq!(
        &region.bytes()[..len],
        wincode::serialize(&QuoteResponseV0 {
            levels: &levels,
            withheld,
        })
        .unwrap()
        .as_slice()
    );

    let response = QuoteResponseV0::parse(&region.bytes()[..len]).unwrap();
    assert_eq!(response.levels, levels.as_slice());
    assert_eq!(response.withheld, withheld);
}

/// The writer holds the bound the reader checks, so a quoter fails on its
/// own bug instead of on the router's rejection of the response.
#[test]
fn a_dangling_completed_order_is_refused_at_write_time() {
    let mut region = Region::new(256);
    let mut writer = ExecuteWriter::new();
    writer.push_change(region.bytes(), changes()[0]).unwrap();
    assert_eq!(
        writer.finish(
            region.bytes(),
            &[],
            &[CompletedOrderV0 {
                order_id: 1,
                change_index: 1,
                client_order_id: 0,
                flags: 0,
                _pad: [0; 1],
            }],
            &[],
        ),
        Err(SpecError::DanglingCompletedOrder)
    );
}

/// A region the records cannot be read at is refused rather than written.
/// The response would be unreadable, and its reader is a CPI away.
#[test]
fn a_misaligned_region_is_an_error_not_a_panic() {
    let mut region = Region::new(256);
    let skewed = &mut region.bytes()[1..];
    let mut writer = QuoteWriter::new();
    assert_eq!(
        writer.push_level(skewed, PriceLevelV0 { price: 1, size: 2 }),
        Err(SpecError::RegionMisaligned)
    );

    // And a response with no records at all is refused too, on the report
    // behind the empty ladder.
    assert_eq!(
        QuoteWriter::new().finish(skewed, PriceLevelV0::default()),
        Err(SpecError::RegionMisaligned)
    );
}

/// A region too small fails on the record that does not fit, and never
/// writes past its end.
#[test]
fn a_full_region_stops_the_writer() {
    let mut region = Region::new(LEN_BYTES + 2 * PRICE_LEVEL_BYTES);
    // Room for the prefix and one rung, and nothing after them.
    let bytes = &mut region.bytes()[..LEN_BYTES + PRICE_LEVEL_BYTES];
    let mut writer = QuoteWriter::new();
    writer
        .push_level(bytes, PriceLevelV0 { price: 1, size: 2 })
        .unwrap();
    assert_eq!(
        writer.push_level(bytes, PriceLevelV0 { price: 3, size: 4 }),
        Err(SpecError::RegionTooSmall)
    );

    // And the withheld report has nowhere to go either.
    assert_eq!(
        writer.finish(bytes, PriceLevelV0::default()),
        Err(SpecError::RegionTooSmall)
    );
}

/// The request half is read in place, and its framing carries a four-byte
/// count rather than the responses' eight, so a quoter's dispatch decodes it
/// borsh-compatibly. The offsets are pinned here because velocity writes
/// these bytes from another workspace, where the agreement can only be held
/// as numbers.
#[test]
fn the_args_put_the_user_set_first_and_count_it_in_four_bytes() {
    let users = [user(1, 0), user(2, 7)];
    let args = QuoteArgsV0 {
        users: &users,
        direction: DirectionV0::Long,
        size: 12,
        caps: UserCapsV0::EMPTY,
        reference_price: Some(5),
        taker: Some(user(3, 1)),
        limit_price: 0,
        taker_served_window: true,
        include_taker_origin_reservations: false,
    };
    let bytes = wincode::config::serialize(&args, ARGS_CONFIG).unwrap();

    assert_eq!(&bytes[..4], &2u32.to_le_bytes());
    assert_eq!(&bytes[4..4 + UserRefV0::SIZE], &user(1, 0).to_bytes());
    let after_set = user_set_bytes(users.len());
    assert_eq!(bytes[after_set], 0, "Long is the zero discriminant");
    assert_eq!(
        &bytes[after_set + 1..after_set + 9],
        &12u64.to_le_bytes(),
        "size follows the direction"
    );
    assert_eq!(bytes.len(), args_size(&args).unwrap());
    assert_eq!(
        bytes.len(),
        after_set + 1 + 8 + USER_CAPS_BYTES + 1 + 8 + 1 + UserRefV0::SIZE + 8 + 1 + 1
    );

    // And it reads back as a slice into those bytes, not a copy of them.
    let read: QuoteArgsV0 = wincode::config::deserialize(&bytes, ARGS_CONFIG).unwrap();
    assert_eq!(read, args);
    assert_eq!(read.users.as_ptr() as usize, bytes[4..].as_ptr() as usize);
}

/// An empty set is what a quote view sends, and it is the case the heap
/// cared about.
#[test]
fn an_unrestricted_set_costs_four_bytes() {
    let args = ExecuteArgsV0 {
        users: &[],
        direction: DirectionV0::Short,
        size: 1,
        caps: UserCapsV0::EMPTY,
        reference_price: None,
        taker: None,
        taker_served_window: false,
        include_taker_origin_reservations: false,
    };
    let bytes = wincode::config::serialize(&args, ARGS_CONFIG).unwrap();
    assert_eq!(&bytes[..4], &0u32.to_le_bytes());
    assert_eq!(bytes.len(), 4 + 1 + 8 + USER_CAPS_BYTES + 1 + 1 + 1 + 1);
    assert_eq!(bytes.len(), args_size(&args).unwrap());

    let read: ExecuteArgsV0 = wincode::config::deserialize(&bytes, ARGS_CONFIG).unwrap();
    assert!(read.users.is_empty());
    assert!(user_set_within_capacity(read.users));
}

#[test]
fn a_set_past_the_capacity_is_refused_by_the_check_a_reader_owes_it() {
    let full = vec![user(1, 0); USER_SET_CAPACITY];
    assert!(user_set_within_capacity(&full));
    assert_eq!(user_set_bytes(full.len()), USER_SET_MAX_BYTES);
    let over = vec![user(1, 0); USER_SET_CAPACITY + 1];
    assert!(!user_set_within_capacity(&over));
}

/// The L3 leg is the one place a quoter says who is behind its ladder, so
/// a row round-trips whole and the reader borrows the rows in place.
#[test]
fn an_l3_response_round_trips_through_its_writer() {
    let rows = [
        L3RowV0 {
            price: 100,
            size: 5,
            order_id: 7,
            node_index: 7,
            user: user(1, 0),
            flags: L3_ROW_FLAG_TAKER_ORIGIN,
            _pad: [0; 1],
            placed_slot: 7,
        },
        L3RowV0 {
            price: 101,
            size: 6,
            order_id: 8,
            node_index: 8,
            user: user(2, 3),
            flags: 0,
            _pad: [0; 1],
            placed_slot: 8,
        },
    ];

    let mut region = Region::new(LEN_BYTES + rows.len() * L3_ROW_BYTES + 1);
    let bytes = region.bytes();
    let mut writer = L3Writer::new();
    for row in rows {
        writer.push_row(bytes, row).unwrap();
    }

    assert_eq!(writer.rows(), 2);
    let len = writer.finish(bytes, true).unwrap();
    assert_eq!(len, LEN_BYTES + 2 * L3_ROW_BYTES + 1);

    let response = L3ResponseV0::parse(&bytes[..len]).unwrap();
    assert_eq!(response.rows, &rows[..]);
    assert_eq!(response.more, 1, "the walk stopped on a bound");
    assert_eq!(
        response.rows.as_ptr() as usize,
        bytes[LEN_BYTES..].as_ptr() as usize,
        "rows are read in place"
    );
}

/// A row is 72 bytes with no implicit padding, which is what `Pod` and the
/// in-place read both need.
#[test]
fn the_l3_row_is_the_width_the_region_is_sized_from() {
    assert_eq!(L3_ROW_BYTES, 72);
    assert_eq!(L3_ROW_BYTES, 3 * 8 + 4 + UserRefV0::SIZE + 1 + 1 + 8);
}

#[test]
fn the_user_ref_schema_matches_its_byte_form() {
    // `UserRefV0` writes its wincode schema by hand. The two ways this
    // crate states the same 34 bytes must agree, or a quoter's response and
    // velocity's reader disagree on where every field after the ref starts.
    let subject = user(0xAB, 0x0102);
    let encoded = wincode::serialize(&subject).unwrap();

    assert_eq!(encoded.len(), UserRefV0::SIZE);
    assert_eq!(encoded.as_slice(), subject.to_bytes().as_slice());
    assert_eq!(&encoded[..32], subject.authority.as_array());
    assert_eq!(&encoded[32..], &0x0102u16.to_le_bytes());

    let decoded: UserRefV0 = wincode::deserialize(&encoded).unwrap();
    assert_eq!(decoded, subject);

    // The schema reports itself zero-copy and unpadded, which is what lets
    // the records embedding it stay `#[wincode(assert_zero_copy)]`.
    assert_eq!(
        <UserRefV0 as SchemaWrite<wincode::config::DefaultConfig>>::TYPE_META,
        TypeMeta::Static {
            size: UserRefV0::SIZE,
            zero_copy: true,
        }
    );
}

/// The writers write the prefix themselves, so what this crate says it is
/// has to be what wincode writes.
#[test]
fn the_length_prefix_is_what_wincode_writes() {
    let levels = [
        PriceLevelV0 { price: 1, size: 2 },
        PriceLevelV0 { price: 3, size: 4 },
    ];
    let bytes = wincode::serialize(&QuoteResponseV0 {
        levels: &levels,
        withheld: PriceLevelV0::default(),
    })
    .unwrap();
    assert_eq!(&bytes[..LEN_BYTES], &len_prefix(levels.len()));
    // The ladder, then the withheld report behind it.
    assert_eq!(
        bytes.len(),
        LEN_BYTES + levels.len() * PRICE_LEVEL_BYTES + 2 * 8
    );

    // And the report round trips from that tail.
    let withheld = PriceLevelV0 { price: 7, size: 9 };
    let bytes = wincode::serialize(&QuoteResponseV0 {
        levels: &levels,
        withheld,
    })
    .unwrap();
    let response = QuoteResponseV0::parse(&bytes).unwrap();
    assert_eq!(response.levels, levels.as_slice());
    assert_eq!(response.withheld, withheld);
}

/// Nine constrained users against eight slots. The eight tightest keep their
/// exact number. The ninth is excluded rather than dropped, because a drop
/// would read as unconstrained, which is the opposite of what its
/// `quote_cap` says.
#[test]
fn a_budget_that_does_not_fit_becomes_an_exclusion() {
    let caps = (0..9).map(|index| UserCapV0 {
        index,
        quote_cap: 1_000 - index as u64,
        base_cap: u64::MAX,
    });
    let set = UserCapsV0::from_caps(caps).unwrap();

    assert_eq!(set.len as usize, USER_CAPS_CAPACITY);
    // Index 0 has the loosest quote_cap of the nine, so it is the one evicted.
    assert!(set.is_excluded(0));
    for index in 1..9 {
        assert!(!set.is_excluded(index), "index {index}");
        assert_eq!(
            set.as_slice()
                .iter()
                .find(|cap| cap.index == index as u8)
                .map(|cap| cap.quote_cap),
            Some(1_000 - index as u64)
        );
    }
}

/// No room never spends a slot. It is the whole point of the bitmap, and
/// it leaves the eight for users who can still fill.
#[test]
fn no_room_costs_no_slot() {
    let set = UserCapsV0::from_caps((0..20).map(|index| UserCapV0 {
        index,
        quote_cap: 0,
        base_cap: u64::MAX,
    }))
    .unwrap();

    assert_eq!(set.len, 0);
    for index in 0..20 {
        assert!(set.is_excluded(index));
    }
}

/// An unbounded `quote_cap` is the same as saying nothing, so it costs
/// neither a slot nor a bit.
#[test]
fn an_unbounded_budget_is_not_carried() {
    let set = UserCapsV0::from_caps((0..20).map(|index| UserCapV0 {
        index,
        quote_cap: u64::MAX,
        base_cap: u64::MAX,
    }))
    .unwrap();

    assert_eq!(set.len, 0);
    assert!(!set.any_excluded());
}

/// The record layout is the wire. Pin the offsets so a reordered field
/// fails here rather than redefining what the other program reads.
#[test]
fn layout_is_pinned() {
    let change = UserBalanceChangeV0 {
        base_size: 0x0807_0605_0403_0201,
        quote_size: 0x1817_1615_1413_1211,
        user: user(0xAB, 0x0201),
        _pad: [0; 6],
    };
    let bytes = wincode::serialize(&change).unwrap();
    assert_eq!(&bytes[0..8], &0x0807_0605_0403_0201u64.to_le_bytes());
    assert_eq!(&bytes[8..16], &0x1817_1615_1413_1211u64.to_le_bytes());
    assert_eq!(&bytes[16..48], &[0xABu8; 32]);
    assert_eq!(&bytes[48..50], &[0x01, 0x02]);
    assert_eq!(bytes.len(), CHANGE_BYTES);
}

/// A cap must name one user of the set. An index past the set or an index
/// named twice would otherwise pass unbounded.
#[test]
fn a_cap_that_names_no_single_user_is_refused() {
    let cap = |index| UserCapV0 {
        index,
        quote_cap: 10,
        base_cap: u64::MAX,
    };
    assert_eq!(
        UserCapsV0::from_caps([cap(USER_SET_CAPACITY as u8)]),
        Err(SpecError::InvalidCapIndex)
    );
    assert_eq!(
        UserCapsV0::from_caps([cap(3), cap(3)]),
        Err(SpecError::InvalidCapIndex)
    );
}

#[test]
fn a_change_the_writer_has_not_written_is_out_of_range() {
    let mut region = Region::new(256);
    let mut writer = ExecuteWriter::new();
    writer.push_change(region.bytes(), changes()[0]).unwrap();
    assert!(writer.change_mut(region.bytes(), 0).is_ok());
    assert_eq!(
        writer.change_mut(region.bytes(), 1).map(|_| ()),
        Err(SpecError::ChangeIndexOutOfRange)
    );
}

#[test]
fn a_pointer_past_u32_is_refused() {
    assert_eq!(
        ResponsePointerV0::at(8, 16),
        Ok(ResponsePointerV0 { offset: 8, len: 16 })
    );
    assert_eq!(
        ResponsePointerV0::at(u32::MAX as usize + 1, 0),
        Err(SpecError::PointerOverflow)
    );
    assert_eq!(
        ResponsePointerV0::at(0, u32::MAX as usize + 1),
        Err(SpecError::PointerOverflow)
    );
}

/// A change names one order only when exactly one order stands behind it,
/// whether consumed or left partial.
#[test]
fn a_change_names_its_order_only_when_it_has_one() {
    let (c, x, p) = (changes(), cancelled(), partial());
    let one = [completed()[0]];
    let merged = completed();
    let both = ExecuteResponseV0 {
        changes: &c,
        cancelled: &x,
        completed: &one,
        partial: &p,
    };
    let consumed_only = ExecuteResponseV0 {
        partial: &[],
        ..both
    };
    let partial_only = ExecuteResponseV0 {
        completed: &[],
        ..both
    };
    let many = ExecuteResponseV0 {
        completed: &merged,
        partial: &[],
        ..both
    };

    let orders = |response: ExecuteResponseV0, index| response.orders_for(index);
    assert_eq!(orders(consumed_only, 0).sole_client_order_id, Some(90));
    assert_eq!(orders(partial_only, 0).sole_client_order_id, Some(110));
    assert_eq!(orders(both, 0).sole_client_order_id, None);
    assert_eq!(orders(many, 0).completed, 2);
    assert_eq!(orders(many, 0).sole_client_order_id, None);
    assert_eq!(orders(many, 1).completed, 0);
    assert_eq!(orders(many, 1).sole_client_order_id, None);
}

/// A set built from caps leaves the unused slots as `EMPTY` leaves them, so
/// two sets that carry the same caps are equal.
#[test]
fn the_unused_slots_match_the_empty_set() {
    let set = UserCapsV0::from_caps([UserCapV0 {
        index: 2,
        quote_cap: 10,
        base_cap: u64::MAX,
    }])
    .unwrap();

    assert_eq!(set.caps[1..], UserCapsV0::EMPTY.caps[1..]);
}

/// `users` is borrowed in place, and a `UserRefV0` needs a two-byte step. Args
/// that put the set on an odd address are refused, not read as a misaligned
/// reference.
#[test]
fn args_whose_user_set_is_misaligned_are_refused() {
    let users = [user(1, 0), user(2, 7)];
    let quote = QuoteArgsV0 {
        users: &users,
        direction: DirectionV0::Long,
        size: 12,
        caps: UserCapsV0::EMPTY,
        reference_price: Some(5),
        taker: None,
        limit_price: 0,
        taker_served_window: true,
        include_taker_origin_reservations: false,
    };
    let execute = ExecuteArgsV0 {
        users: &users,
        direction: DirectionV0::Short,
        size: 12,
        caps: UserCapsV0::EMPTY,
        reference_price: Some(5),
        taker: None,
        taker_served_window: true,
        include_taker_origin_reservations: false,
    };

    let mut quote_bytes = Vec::new();
    write_args(&mut quote_bytes, &quote).unwrap();
    let mut execute_bytes = Vec::new();
    write_args(&mut execute_bytes, &execute).unwrap();

    let mut region = Region::new(quote_bytes.len() + 1);
    let buffer = region.bytes();
    buffer[..quote_bytes.len()].copy_from_slice(&quote_bytes);
    assert_eq!(QuoteArgsV0::parse(&buffer[..quote_bytes.len()]), Ok(quote));
    buffer[1..=quote_bytes.len()].copy_from_slice(&quote_bytes);
    assert_eq!(
        QuoteArgsV0::parse(&buffer[1..=quote_bytes.len()]),
        Err(SpecError::Read)
    );

    buffer[..execute_bytes.len()].copy_from_slice(&execute_bytes);
    assert_eq!(
        ExecuteArgsV0::parse(&buffer[..execute_bytes.len()]),
        Ok(execute)
    );
    buffer[1..=execute_bytes.len()].copy_from_slice(&execute_bytes);
    assert_eq!(
        ExecuteArgsV0::parse(&buffer[1..=execute_bytes.len()]),
        Err(SpecError::Read)
    );
}

/// A maker with an unbounded budget holds a slot only for its reduce-only
/// `base_cap`. When eight budgets crowd it out, it is dropped rather than
/// excluded, so its ordinary orders still fill. It sorts loosest, so it loses
/// the slot whether it arrives first or last.
#[test]
fn an_unbounded_budget_that_does_not_fit_is_dropped_not_excluded() {
    let budgeted = (0..USER_CAPS_CAPACITY as u8).map(|index| UserCapV0 {
        index,
        quote_cap: 100 + index as u64,
        base_cap: u64::MAX,
    });
    let unbounded = UserCapV0 {
        index: USER_CAPS_CAPACITY as u8,
        quote_cap: u64::MAX,
        base_cap: 5,
    };

    let last = UserCapsV0::from_caps(budgeted.clone().chain([unbounded])).unwrap();
    let first = UserCapsV0::from_caps([unbounded].into_iter().chain(budgeted)).unwrap();
    for set in [last, first] {
        assert_eq!(set.len as usize, USER_CAPS_CAPACITY);
        assert!(!set.any_excluded(), "nobody is excluded");
        assert!(set
            .as_slice()
            .iter()
            .all(|cap| cap.index != unbounded.index));
    }
}
