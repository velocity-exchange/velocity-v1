use {
    super::*,
    crate::{
        signer::{find_clob_authority, find_quoter_signer},
        state::pdas,
    },
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
    let entry = Pubkey::new_unique();
    let (quoter_signer, _) = find_quoter_signer(&entry);
    assert_eq!(quoter_signer, pdas::quoter_signer(&entry));
    assert_ne!(quoter_signer, pdas::velocity_signer());
    // Nor is it the protocol account's authority, which is derived from the
    // vault authority.
    let (protocol_user, protocol_user_stats) = pdas::protocol_user_pair();
    assert_ne!(pdas::user(&quoter_signer, 0), protocol_user);
    assert_ne!(pdas::user_stats(&quoter_signer), protocol_user_stats);
}

/// A quoter's signature must authenticate velocity at that quoter and nowhere
/// else. Two things follow, and both are what stop a quoter reaching past its
/// own program: the key differs per registry entry, so forwarding it to a
/// second quoter proves nothing there; and none of them is the books' place
/// authority, which may place and cancel on any market for any user.
#[test]
fn every_entry_signs_as_its_own_key_and_none_of_them_is_the_book_authority() {
    let (first, second) = (Pubkey::new_unique(), Pubkey::new_unique());
    let (first_signer, _) = find_quoter_signer(&first);
    let (second_signer, _) = find_quoter_signer(&second);
    let (clob_authority, _) = find_clob_authority();

    assert_ne!(first_signer, second_signer);
    assert_ne!(first_signer, clob_authority);
    assert_ne!(second_signer, clob_authority);
    assert_eq!(clob_authority, pdas::clob_authority());
}

/// Only the quoter signer's slot is handed signer privilege. The vault
/// authority is passed unprivileged even if it somehow reached a stored list
/// (entries registered before the reserved-key check shipped).
#[test]
fn only_the_quoter_signer_slot_is_a_signer() {
    let vault_authority = pdas::velocity_signer();
    let (quoter_signer, _) = find_quoter_signer(&Pubkey::new_unique());
    let book = Pubkey::new_unique();
    let taker_wallet = Pubkey::new_unique();

    let registered = [
        meta(book, true),
        meta(quoter_signer, false),
        meta(vault_authority, false),
        meta(taker_wallet, false),
    ];
    let mut metas = Vec::new();
    write_quoter_account_metas(&mut metas, &registered, &quoter_signer);

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
    let (quoter_signer, _) = find_quoter_signer(&Pubkey::new_unique());
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

/// `State::signer` for the subject tests — a key none of the fixtures use, so
/// it isolates the protocol-user rule from the rest.
fn protocol() -> Pubkey {
    Pubkey::new_from_array([0x5a; 32])
}

/// A Custom entry fills against exactly one margin account. Every other
/// loaded user — the taker, a rival quoter's maker, another sub-account of
/// the same authority — is off limits, however well-formed the response.
#[test]
fn a_custom_quoter_may_only_move_its_registry_user() {
    let taker = user_ref(7, 0);
    let subjects = QuoterSubjects::Account(key(1));
    assert!(subjects.permits(&user_ref(1, 0), &key(1), &taker, &protocol()));
    assert!(!subjects.permits(&user_ref(2, 0), &key(2), &taker, &protocol()));
    // Same authority, different sub-account: a different `User` account,
    // so a different subject.
    assert!(!subjects.permits(&user_ref(1, 1), &key(9), &taker, &protocol()));
}

/// Self-trade prevention is a rule, not a request on the wire: even an
/// entry registered *for* the taker cannot settle the taker against
/// themselves.
#[test]
fn the_taker_is_never_a_subject() {
    let taker = user_ref(7, 0);
    assert!(!QuoterSubjects::Account(key(7)).permits(&taker, &key(7), &taker, &protocol()));
    assert!(!QuoterSubjects::Book.permits(&taker, &key(7), &taker, &protocol()));
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
    // ...nor the protocol `User`, which is the inventory-free taker the cross
    // and liquidation cranks fill through. A book that could name it would
    // move a position onto protocol funds at a price of its own choosing.
    let protocol_user = ClobUserRefV0 {
        authority: protocol(),
        sub_account_id: 0,
    };
    assert!(!QuoterSubjects::Book.permits(&protocol_user, &key(3), &taker, &protocol()));
    // A different sub-account of the protocol authority is an ordinary user.
    assert!(QuoterSubjects::Book.permits(
        &ClobUserRefV0 {
            authority: protocol(),
            sub_account_id: 1,
        },
        &key(4),
        &taker,
        &protocol()
    ));
    assert!(QuoterSubjects::Book.permits(&user_ref(1, 0), &key(1), &taker, &protocol()));
    assert!(QuoterSubjects::Book.permits(&user_ref(2, 9), &key(2), &taker, &protocol()));
    assert!(!QuoterSubjects::Book.permits(&taker, &key(7), &taker, &protocol()));
}

/// A Custom entry stays bound to the one account its registration consented
/// for, which velocity reads off the registry rather than off the quoter.
#[test]
fn a_custom_quoter_still_moves_only_its_registered_user() {
    let taker = user_ref(7, 0);
    let subjects = QuoterSubjects::Account(key(1));
    assert!(subjects.permits(&user_ref(1, 0), &key(1), &taker, &protocol()));
    assert!(!subjects.permits(&user_ref(2, 0), &key(2), &taker, &protocol()));
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
        limit_price: 0,
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
        price: 1_700,
        client_order_id: 420,
        user: ClobUserRefV0 {
            authority,
            sub_account_id: 1,
        },
        _pad: [0; 2],
    }];
    let completed = [
        CompletedOrderV0 {
            order_id: 9,
            change_index: 0,
            client_order_id: 0,
        },
        CompletedOrderV0 {
            order_id: 10,
            change_index: 1,
            client_order_id: 0,
        },
    ];

    let bytes = quoter_spec::wincode::serialize(&quoter_spec::ExecuteResponseV0 {
        changes: &changes,
        cancelled: &cancelled,
        completed: &completed,
        partial: &[],
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
        limit_price: u64::MAX,
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
    // The quote is the wider leg, by the price bound execute does not carry.
    assert_eq!(
        quoter_spec::args_size(&quote).unwrap() + 8,
        QUOTER_CPI_DATA_MAX
    );
    assert_eq!(
        quoter_spec::args_size(&execute).unwrap() + 8,
        quoter_cpi_data_len(MAX_QUOTER_WIRE_USERS, true)
    );
    assert_eq!(
        QUOTER_CPI_DATA_MAX,
        quoter_cpi_data_len(MAX_QUOTER_WIRE_USERS, true) + 8
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
            assert_eq!(bytes.len() + 8, quote_cpi_data_len(count, taker.is_some()));
        }
    }
}

/// The two encoders on the CLOB wire agree, byte for byte.
///
/// Velocity writes these args with anchor's borsh and the book reads them with
/// wincode. One declaration in `clob-wire` is what stops the *shapes* drifting
/// apart; this is what stops the *encodings* drifting apart, which no shared
/// declaration can catch. Every field is given a value that would move if a
/// width or an order changed.
#[test]
fn the_clob_wire_encodes_the_same_under_borsh_and_wincode() {
    use anchor_lang::AnchorSerialize;

    fn agree<T>(what: &str, value: &T)
    where
        T: AnchorSerialize + quoter_spec::wincode::SchemaWrite<quoter_spec::ArgsConfig, Src = T>,
    {
        let mut borsh = Vec::new();
        value.serialize(&mut borsh).unwrap();
        let mut wincode = Vec::new();
        quoter_spec::write_args(&mut wincode, value).unwrap();
        assert_eq!(
            borsh, wincode,
            "{what} encodes differently on the two sides"
        );
    }

    let user = user_ref(0xAB, 0x1234);
    let order_ref = ClobOrderRefV0 {
        node_index: 0x0102_0304,
        order_id: 0x0506_0708_090A_0B0C,
    };

    agree("OrderRefV0", &order_ref);
    agree(
        "PlaceOrderArgsV0",
        &ClobPlaceOrderArgsV0 {
            side: ClobSide::Ask,
            price: 0x1122_3344_5566_7788,
            base_asset_amount: 0x99AA_BBCC_DDEE_FF00,
            activation_delay_slots: Some(0x0A0B_0C0D),
            max_ts: -0x0102_0304_0506_0708,
            user,
            taker_origin: true,
            client_order_id: 0x0102_0304,
            reject_if_crossed: true,
        },
    );
    // The absent-option arm encodes its tag differently; both are on the wire.
    agree(
        "PlaceOrderArgsV0 (no delay)",
        &ClobPlaceOrderArgsV0 {
            side: ClobSide::Bid,
            price: 1,
            base_asset_amount: 2,
            activation_delay_slots: None,
            max_ts: 0,
            user,
            taker_origin: false,
            client_order_id: 0,
            reject_if_crossed: false,
        },
    );
    agree(
        "CancelOrderArgsV0",
        &ClobCancelOrderArgsV0 { order_ref, user },
    );
    agree(
        "CancelAllArgsV0",
        &ClobCancelAllArgsV0 {
            user,
            sides: ClobCancelSides::Both,
        },
    );
    agree(
        "EvictWorstArgsV0",
        &ClobEvictWorstArgsV0 {
            side: ClobSide::Bid,
        },
    );
    agree(
        "RemoveExpiredArgsV0",
        &ClobRemoveExpiredArgsV0 { order_ref },
    );
    agree(
        "RemovedOrderV0",
        &ClobRemovedOrderV0 {
            user,
            order_id: 0x1111_2222_3333_4444,
            client_order_id: 0x0A0B_0C0D,
            price: 0x5555_6666_7777_8888,
            base_asset_amount: 0x9999_AAAA_BBBB_CCCC,
            side: ClobSide::Ask,
            taker_origin: true,
            max_ts: 0x0102_0304_0506_0708,
        },
    );
    agree(
        "CancelAllOutcomeV0",
        &ClobCancelAllOutcomeV0 {
            user,
            bid_base_asset_amount: 0x0102_0304_0506_0708,
            ask_base_asset_amount: 0x090A_0B0C_0D0E_0F10,
            bid_orders: 0x1112_1314,
            ask_orders: 0x1516_1718,
            exhaustive: true,
        },
    );
}
