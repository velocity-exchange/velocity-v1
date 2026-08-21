use {
    super::*,
    crate::{signer::find_quoter_signer, state::pdas},
};

fn meta(pubkey: Pubkey, is_writable: bool) -> AmmAccountMeta {
    AmmAccountMeta {
        pubkey,
        is_writable,
        padding: [0; 7],
    }
}

/// The key velocity signs quoter CPIs as must not be the key that authorizes
/// spending: signer privilege is inherited by a callee, so a quoter handed the
/// vault authority could forward it to the token program.
#[test]
fn quoter_signer_is_not_the_vault_authority() {
    let (quoter_signer, _) = find_quoter_signer();
    assert_eq!(quoter_signer, pdas::quoter_signer());
    assert_ne!(quoter_signer, pdas::velocity_signer());
    // Nor is it the protocol account's authority, which is derived from the
    // vault authority.
    let (protocol_user, protocol_user_stats) = pdas::protocol_user_pair();
    assert_ne!(pdas::user(&quoter_signer, 0), protocol_user);
    assert_ne!(pdas::user_stats(&quoter_signer), protocol_user_stats);
}

/// Only the quoter signer's slot is handed signer privilege. The vault
/// authority is passed unprivileged even if it somehow reached a stored list
/// (entries registered before the reserved-key check shipped).
#[test]
fn only_the_quoter_signer_slot_is_a_signer() {
    let vault_authority = pdas::velocity_signer();
    let (quoter_signer, _) = find_quoter_signer();
    let book = Pubkey::new_unique();
    let taker_wallet = Pubkey::new_unique();

    let registered = [
        meta(book, true),
        meta(quoter_signer, false),
        meta(vault_authority, false),
        meta(taker_wallet, false),
    ];
    let metas = quoter_account_metas(&registered, &quoter_signer);

    assert_eq!(
        metas
            .iter()
            .filter(|m| m.is_signer)
            .map(|m| m.pubkey)
            .collect::<Vec<_>>(),
        vec![quoter_signer]
    );
    assert_eq!(
        metas
            .iter()
            .filter(|m| m.is_writable)
            .map(|m| m.pubkey)
            .collect::<Vec<_>>(),
        vec![book]
    );
}

#[test]
fn registration_rejects_the_vault_authority() {
    let vault_authority = pdas::velocity_signer();
    let (quoter_signer, _) = find_quoter_signer();
    let book = Pubkey::new_unique();

    assert!(validate_quoter_accounts([book, quoter_signer].iter()).is_ok());
    assert!(validate_quoter_accounts([book, vault_authority].iter()).is_err());
    assert!(validate_quoter_accounts([vault_authority].iter()).is_err());
}

fn user_ref(byte: u8, sub_account_id: u16) -> ClobUserRefV0 {
    ClobUserRefV0 {
        authority: Pubkey::new_from_array([byte; 32]),
        sub_account_id,
    }
}

fn key(byte: u8) -> Pubkey {
    Pubkey::new_from_array([byte | 0x80; 32])
}

/// A Custom entry fills against exactly one margin account. Every other
/// loaded user — the taker, a rival quoter's maker, another sub-account of
/// the same authority — is off limits, however well-formed the response.
#[test]
fn a_custom_quoter_may_only_move_its_registry_user() {
    let taker = user_ref(7, 0);
    let subjects = QuoterSubjects::Account(key(1));
    assert!(subjects.permits(&user_ref(1, 0), &key(1), &taker));
    assert!(!subjects.permits(&user_ref(2, 0), &key(2), &taker));
    // Same authority, different sub-account: a different `User` account,
    // so a different subject.
    assert!(!subjects.permits(&user_ref(1, 1), &key(9), &taker));
}

/// Self-trade prevention is a rule, not a request on the wire: even an
/// entry registered *for* the taker cannot settle the taker against
/// themselves.
#[test]
fn the_taker_is_never_a_subject() {
    let taker = user_ref(7, 0);
    assert!(!QuoterSubjects::Account(key(7)).permits(&taker, &key(7), &taker));
    assert!(!QuoterSubjects::Book.permits(&taker, &key(7), &taker));
}

/// A book settles for whoever rests on it, and velocity cannot establish who
/// that is: the arena is the book program's own state, so holding a response
/// to it would check the program against itself. What is left is the rule
/// that does not depend on the book — never the taker — plus the loaded-user
/// bound the caller applies, the quoted-price binding, and the post-fill
/// margin check on everyone touched.
#[test]
fn a_book_may_move_any_loaded_user_but_the_taker() {
    let taker = user_ref(7, 0);
    assert!(QuoterSubjects::Book.permits(&user_ref(1, 0), &key(1), &taker));
    assert!(QuoterSubjects::Book.permits(&user_ref(2, 9), &key(2), &taker));
    assert!(!QuoterSubjects::Book.permits(&taker, &key(7), &taker));
}

/// A Custom entry stays bound to the one account its registration consented
/// for, which velocity reads off the registry rather than off the quoter.
#[test]
fn a_custom_quoter_still_moves_only_its_registered_user() {
    let taker = user_ref(7, 0);
    let subjects = QuoterSubjects::Account(key(1));
    assert!(subjects.permits(&user_ref(1, 0), &key(1), &taker));
    assert!(!subjects.permits(&user_ref(2, 0), &key(2), &taker));
}

/// One live order in a synthetic book's node arena. Mirrors
/// [`read_clob_node`]'s offsets, which is the point: these tests pin the
/// walk, and the litesvm crank tests pin the offsets against the real
/// program.
struct TestNode {
    user: ClobUserRefV0,
    price: u64,
    base_asset_amount: u64,
    activation_slot: u64,
    max_ts: i64,
    next: u32,
}

impl TestNode {
    fn live(user: ClobUserRefV0, price: u64, base_asset_amount: u64, next: u32) -> Self {
        Self {
            user,
            price,
            base_asset_amount,
            activation_slot: 0,
            max_ts: 0,
            next,
        }
    }
}

/// A book account's bytes holding `nodes`, with `side`'s head at index 0.
fn book_bytes(side: ClobSide, nodes: &[TestNode]) -> Vec<u8> {
    let mut data = vec![0u8; CLOB_ORDERS_OFFSET + nodes.len().max(1) * CLOB_NODE_LEN];
    let head_offset = match side {
        ClobSide::Bid => CLOB_BEST_BID_OFFSET,
        ClobSide::Ask => CLOB_BEST_ASK_OFFSET,
    };
    data[head_offset..head_offset + 4].copy_from_slice(&0u32.to_le_bytes());
    for (index, node) in nodes.iter().enumerate() {
        let at = CLOB_ORDERS_OFFSET + index * CLOB_NODE_LEN;
        data[at..at + 32].copy_from_slice(&node.user.authority.to_bytes());
        data[at + 32..at + 40].copy_from_slice(&node.price.to_le_bytes());
        data[at + 40..at + 48].copy_from_slice(&node.base_asset_amount.to_le_bytes());
        data[at + 48..at + 56].copy_from_slice(&node.activation_slot.to_le_bytes());
        data[at + 56..at + 64].copy_from_slice(&node.max_ts.to_le_bytes());
        data[at + 84..at + 88].copy_from_slice(&node.next.to_le_bytes());
        data[at + 88] = CLOB_ORDER_BIT_FLAG_OPEN;
        data[at + 90..at + 92].copy_from_slice(&node.user.sub_account_id.to_le_bytes());
    }
    data
}

/// The permitted set is the run of orders the fill could actually reach,
/// so the walk stops once the requested size is covered — a maker deeper
/// in the book than the fill goes is not a subject.
#[test]
fn the_resting_walk_covers_the_requested_size_and_stops() {
    let data = book_bytes(
        ClobSide::Ask,
        &[
            TestNode::live(user_ref(1, 0), 100, 5, 1),
            TestNode::live(user_ref(2, 0), 101, 5, 2),
            TestNode::live(user_ref(3, 0), 102, 5, CLOB_NIL),
        ],
    );
    let walk = |size| {
        clob_resting_prefix(
            &data,
            ClobSide::Ask,
            size,
            &[],
            &QuoterUserCapsV0::EMPTY,
            &user_ref(9, 0),
            0,
            0,
        )
    };
    assert_eq!(walk(5).len(), 1);
    assert_eq!(walk(6).len(), 2);
    assert_eq!(walk(100).len(), 3);
    let prefix = walk(6);
    assert_eq!(prefix[0].user, user_ref(1, 0));
    assert_eq!(prefix[1].price, 101);
}

/// Orders execute would pass over are passed over here too, and crucially
/// they don't end the walk: execute keeps going to the next order, so a
/// maker it really does fill has to stay in the permitted set.
#[test]
fn the_resting_walk_skips_what_execute_skips_and_keeps_going() {
    let (slot, now) = (10u64, 1_000i64);
    let unreachable = user_ref(1, 0);
    let reachable = user_ref(2, 0);
    for head in [
        TestNode {
            // Still inside its activation delay.
            activation_slot: slot + 1,
            ..TestNode::live(unreachable, 100, 5, 1)
        },
        TestNode {
            // Expired.
            max_ts: now - 1,
            ..TestNode::live(unreachable, 100, 5, 1)
        },
    ] {
        let data = book_bytes(
            ClobSide::Ask,
            &[head, TestNode::live(reachable, 101, 5, CLOB_NIL)],
        );
        let prefix = clob_resting_prefix(
            &data,
            ClobSide::Ask,
            5,
            &[],
            &QuoterUserCapsV0::EMPTY,
            &user_ref(9, 0),
            slot,
            now,
        );
        assert_eq!(prefix.len(), 1);
        assert_eq!(prefix[0].user, reachable);
    }
}

#[test]
fn the_resting_walk_skips_the_taker_and_unsettleable_makers() {
    let taker = user_ref(1, 0);
    let stranger = user_ref(2, 0);
    let loaded = user_ref(3, 0);
    let data = book_bytes(
        ClobSide::Ask,
        &[
            TestNode::live(taker, 100, 5, 1),
            TestNode::live(stranger, 101, 5, 2),
            TestNode::live(loaded, 102, 5, CLOB_NIL),
        ],
    );
    let users = quoter_wire_users([taker, loaded]).unwrap();
    let prefix = clob_resting_prefix(
        &data,
        ClobSide::Ask,
        5,
        &users,
        &QuoterUserCapsV0::EMPTY,
        &taker,
        0,
        0,
    );
    assert_eq!(prefix.len(), 1);
    assert_eq!(prefix[0].user, loaded);
}

/// A partial budget is the book's business, not this walk's. Velocity leaves
/// it alone so the prefix stays a *superset* of what execute fills: spending
/// a budget here could retire a maker's room before the book retires it, and
/// the walk would stop short of a maker execute still reaches — whose balance
/// change velocity would then refuse.
#[test]
fn a_partial_budget_does_not_shorten_the_permitted_set() {
    let capped = user_ref(1, 0);
    let behind = user_ref(2, 0);
    let data = book_bytes(
        ClobSide::Ask,
        &[
            TestNode::live(capped, 100, 10, 1),
            TestNode::live(behind, 101, 10, CLOB_NIL),
        ],
    );
    let users = quoter_wire_users([capped, behind]).unwrap();
    let caps = QuoterUserCapsV0::from_caps([QuoterUserCapV0 {
        index: 0,
        budget: 4,
    }]);
    let prefix = clob_resting_prefix(
        &data,
        ClobSide::Ask,
        10,
        &users,
        &caps,
        &user_ref(9, 0),
        0,
        0,
    );

    assert_eq!(prefix.len(), 1, "the first order already covers the sweep");
    assert_eq!(prefix[0].user, capped);

    // And when the budget does cut the first maker short on the book, the
    // maker behind them is already in the set to receive the rest.
    let deeper = clob_resting_prefix(
        &data,
        ClobSide::Ask,
        20,
        &users,
        &caps,
        &user_ref(9, 0),
        0,
        0,
    );
    assert_eq!(deeper.len(), 2);
    assert_eq!(deeper[1].user, behind);
}

/// An exclusion is the stronger case: the maker is absent from the permitted
/// set entirely, so a book that filled them anyway returns a change velocity
/// refuses.
#[test]
fn no_room_keeps_a_maker_out_of_the_permitted_set() {
    let excluded = user_ref(1, 0);
    let behind = user_ref(2, 0);
    let data = book_bytes(
        ClobSide::Ask,
        &[
            TestNode::live(excluded, 100, 10, 1),
            TestNode::live(behind, 101, 10, CLOB_NIL),
        ],
    );
    let users = quoter_wire_users([excluded, behind]).unwrap();
    let caps = QuoterUserCapsV0::from_caps([QuoterUserCapV0 {
        index: 0,
        budget: 0,
    }]);
    let prefix = clob_resting_prefix(
        &data,
        ClobSide::Ask,
        10,
        &users,
        &caps,
        &user_ref(9, 0),
        0,
        0,
    );

    assert_eq!(prefix.len(), 1);
    assert_eq!(prefix[0].user, behind);
}

/// A hostile or corrupted link list must terminate: the walk is bounded by
/// what the arena can hold, whatever the links say.
#[test]
fn the_resting_walk_terminates_on_a_cyclic_book() {
    let data = book_bytes(
        ClobSide::Ask,
        &[
            TestNode::live(user_ref(1, 0), 100, 1, 1),
            TestNode::live(user_ref(2, 0), 101, 1, 0),
        ],
    );
    let prefix = clob_resting_prefix(
        &data,
        ClobSide::Ask,
        u64::MAX,
        &[],
        &QuoterUserCapsV0::EMPTY,
        &user_ref(9, 0),
        0,
        0,
    );
    assert_eq!(prefix.len(), 2);
}

/// Fixed width, and the same width the CLOB's `UserSetV0` pins on its own
/// side — the two decode each other by offset. Critically, the borrowed
/// writer velocity actually uses must produce byte-for-byte what serializing
/// the fixed struct produces: the struct is the shape each quoter mirrors,
/// while velocity never materializes one (it does not fit in an SBF frame).
/// The cap list rides beside the user set on the same wire, pinned the same
/// way: a fixed width both sides agree on without negotiating.
///
/// The split matters more than the width. A user with no room is one bit, so
/// every user in the set can be excluded at once — which is what a sharp move
/// produces, and the moment the book most needs to stay usable. Only the
/// narrow band with *some* room spends a slot, and an overflow there costs a
/// revert that was already coming.
#[test]
fn the_cap_list_puts_no_ceiling_on_exclusions() {
    fn encode<T: AnchorSerialize>(value: &T) -> Vec<u8> {
        let mut bytes = Vec::new();
        value.serialize(&mut bytes).unwrap();
        bytes
    }
    assert_eq!(MAX_CONSTRAINED_WIRE_USERS, 8);
    assert_eq!(USER_EXCLUSION_BITMAP_BYTES, 6);
    assert_eq!(QUOTER_USER_CAPS_BYTES, 79);
    assert_eq!(
        encode(&QuoterUserCapsV0::EMPTY).len(),
        QUOTER_USER_CAPS_BYTES
    );

    // A user with no room takes a bit, not one of the scarce slots.
    let partial = QuoterUserCapV0 {
        index: 0,
        budget: 500,
    };
    let excluded = QuoterUserCapV0 {
        index: 1,
        budget: 0,
    };
    let caps = QuoterUserCapsV0::from_caps(vec![partial, excluded]);
    assert_eq!(caps.len, 1, "only the partial spends a slot");
    assert_eq!(caps.as_slice()[0], partial);
    assert!(caps.is_excluded(1));
    assert!(!caps.is_excluded(0));
    assert_eq!(encode(&caps).len(), QUOTER_USER_CAPS_BYTES);

    // The case that scales: every user in the set can be excluded at once,
    // which is what a sharp move produces. None of them touches the slots.
    let all: Vec<QuoterUserCapV0> = (0..MAX_QUOTER_WIRE_USERS as u8)
        .map(|index| QuoterUserCapV0 { index, budget: 0 })
        .collect();
    let caps = QuoterUserCapsV0::from_caps(all);
    assert_eq!(caps.len, 0);
    assert!((0..MAX_QUOTER_WIRE_USERS).all(|i| caps.is_excluded(i)));
    assert_eq!(encode(&caps).len(), QUOTER_USER_CAPS_BYTES);

    // Budgets past the ceiling drop the roomiest, keeping the tightest, and
    // the dropped ones fall back to an exclusion rather than to nothing.
    let many: Vec<QuoterUserCapV0> = (0..MAX_CONSTRAINED_WIRE_USERS as u8 + 4)
        .map(|index| QuoterUserCapV0 {
            index,
            budget: 1_000 * (index as u64 + 1),
        })
        .collect();
    let caps = QuoterUserCapsV0::from_caps(many);
    assert_eq!(caps.len as usize, MAX_CONSTRAINED_WIRE_USERS);
    assert_eq!(caps.as_slice()[0].budget, 1_000, "the tightest is kept");
    assert!(
        caps.is_excluded(MAX_CONSTRAINED_WIRE_USERS + 3),
        "the roomiest is excluded, never left unconstrained"
    );
}

/// A set costs the entries it carries, not the capacity it could carry. The
/// quote view sends none at all, and it is the path that runs once per quoter
/// per tick on a heap that never reclaims.
///
/// These are the bytes a quoter reads. The quoter programs live in another
/// workspace, so the agreement between the two can only be held as numbers —
/// the CLOB pins the same ones from its side.
#[test]
fn the_user_set_encodes_to_what_it_carries() {
    assert_eq!(MAX_QUOTER_WIRE_USERS, 48);
    assert_eq!(QUOTER_USER_SET_MAX_BYTES, 4 + 48 * CLOB_USER_REF_BYTES);
    assert_eq!(quoter_user_set_bytes(0), 4);

    let live = [user_ref(1, 0), user_ref(2, 7)];
    let args = QuoteArgsV0 {
        users: &live,
        direction: Direction::Long,
        size: 1,
        caps: QuoterUserCapsV0::EMPTY,
        reference_price: 0,
        taker: None,
    };
    let mut bytes = Vec::new();
    quoter_spec::write_args(&mut bytes, &args).unwrap();

    // The set leads, counted in four bytes, and its refs follow in order.
    assert_eq!(bytes[..4], 2u32.to_le_bytes());
    assert_eq!(bytes[4..4 + CLOB_USER_REF_BYTES], user_ref(1, 0).to_bytes());
    assert_eq!(
        bytes[4 + CLOB_USER_REF_BYTES..quoter_user_set_bytes(live.len())],
        user_ref(2, 7).to_bytes()
    );

    let empty = QuoteArgsV0 { users: &[], ..args };
    let mut bytes = Vec::new();
    quoter_spec::write_args(&mut bytes, &empty).unwrap();
    assert_eq!(
        bytes[..4],
        0u32.to_le_bytes(),
        "a length prefix, then the rest"
    );
}

/// The capacity is an upper bound on what a transaction can lock, so
/// overflowing it means the caller built something unlandable — an error,
/// not a silently truncated set a quoter would match against.
#[test]
fn the_user_set_refuses_to_truncate() {
    let full = (0..MAX_QUOTER_WIRE_USERS).map(|i| user_ref(1, i as u16));
    assert_eq!(
        quoter_wire_users(full).unwrap().len(),
        MAX_QUOTER_WIRE_USERS
    );
    let over = (0..MAX_QUOTER_WIRE_USERS + 1).map(|i| user_ref(1, i as u16));
    assert_eq!(
        quoter_wire_users(over).map(|_| ()),
        Err(ErrorCode::TooManyQuoterWireUsers)
    );
}

/// Velocity reads a response the quoters write, and neither side shares the
/// other's code — the CLOB streams the bytes itself. This pins velocity's
/// reader against the layout's own writer: what `quoter-spec` serializes is
/// what velocity reads back, field for field.
#[test]
fn the_reader_agrees_with_the_specs_writer() {
    let authority = Pubkey::new_unique();
    let other = Pubkey::new_unique();
    let changes = [
        quoter_spec::UserBalanceChangeV0 {
            base_size: 1_000_000_000,
            quote_size: 101_000_000,
            user: ClobUserRefV0 {
                authority,
                sub_account_id: 3,
            },
            _pad: [0; 6],
        },
        quoter_spec::UserBalanceChangeV0 {
            base_size: 5,
            quote_size: 6,
            user: ClobUserRefV0 {
                authority: other,
                sub_account_id: 0,
            },
            _pad: [0; 6],
        },
    ];
    let cancelled = [CancelledRemainderV0 {
        order_id: 42,
        base_asset_amount: 17,
        user: ClobUserRefV0 {
            authority,
            sub_account_id: 1,
        },
        _pad: [0; 6],
    }];
    let completed = [
        CompletedOrderV0 {
            order_id: 9,
            change_index: 0,
            _pad: 0,
        },
        CompletedOrderV0 {
            order_id: 10,
            change_index: 1,
            _pad: 0,
        },
    ];

    let bytes = quoter_spec::wincode::serialize(&quoter_spec::ExecuteResponseV0 {
        changes: &changes,
        cancelled: &cancelled,
        completed: &completed,
    })
    .unwrap();

    let response = ExecuteResponseV0::parse(&bytes).unwrap();
    assert_eq!(response.changes, changes.as_slice());
    assert_eq!(response.cancelled, cancelled.as_slice());
    // Each consumed order resolves back to the change that named it.
    assert_eq!(response.completed_for(0).collect::<Vec<_>>(), vec![9]);
    assert_eq!(response.completed_for(1).collect::<Vec<_>>(), vec![10]);
    assert_eq!(response.completed_count(0), 1);
}

/// The CPI buffer is reserved once, at exactly the width the args serialize
/// to. A `Vec` that reserves short doubles instead, and a doubling leaks its
/// old buffer on a heap that never gives memory back — the fill runs out of
/// memory rather than merely slowing down. The size and the bytes come from
/// the same schema, so the two are pinned to each other here.
#[test]
fn the_cpi_buffer_holds_exactly_what_the_args_serialize_to() {
    let all: Vec<ClobUserRefV0> = (0..MAX_QUOTER_WIRE_USERS)
        .map(|index| user_ref(index as u8, 0))
        .collect();

    // The widest each leg can be: a full user set and a taker present.
    let quote = QuoteArgsV0 {
        users: &all,
        direction: Direction::Long,
        size: u64::MAX,
        caps: QuoterUserCapsV0::EMPTY,
        reference_price: i64::MAX,
        taker: Some(user_ref(0xFF, 0)),
    };
    let execute = ExecuteArgsV0 {
        users: &all,
        direction: Direction::Long,
        size: u64::MAX,
        caps: QuoterUserCapsV0::EMPTY,
        reference_price: i64::MAX,
        taker: Some(user_ref(0xFF, 0)),
    };
    // Eight for the anchor discriminator the caller writes ahead of the args.
    assert_eq!(
        quoter_spec::args_size(&quote).unwrap() + 8,
        QUOTER_CPI_DATA_MAX
    );
    assert_eq!(
        quoter_spec::args_size(&execute).unwrap() + 8,
        QUOTER_CPI_DATA_MAX
    );

    // And a call that carries fewer users costs less, exactly. A quote view
    // carries none.
    for count in [0, 1, MAX_QUOTER_WIRE_USERS] {
        for taker in [None, Some(user_ref(0xFF, 0))] {
            let args = QuoteArgsV0 {
                users: &all[..count],
                taker,
                ..quote
            };
            let mut bytes = Vec::new();
            quoter_spec::write_args(&mut bytes, &args).unwrap();
            assert_eq!(bytes.len(), quoter_spec::args_size(&args).unwrap());
            assert_eq!(bytes.len() + 8, quoter_cpi_data_len(count, taker.is_some()));
        }
    }
}
