//! litesvm integration tests. Require the SBF build first:
//! `bun run program:build:clob` (cargo-build-sbf --tools-version v1.54).

use {
    anchor_v2_testing::{
        Keypair, LiteSVM, Message, Signer, VersionedMessage, VersionedTransaction,
    },
    clob::{
        accounts,
        anchor_lang_v2::{
            prelude::Address, solana_program::instruction::Instruction, Discriminator, Event,
        },
        events::{ExecuteRecordV0, FillSlimV0, OrdersCancelRecordV0},
        instruction,
        state::{
            CancelSidesV0, ClobDirectionExt, ClobHeaderV0, ClobMarketV0, ClobSideExt, Direction,
            MarketConfigV0, OrderBitFlag, OrderNodeV0, OrderRefV0, Side, UserCapsV0, UserRefV0,
            UserSetV0, CANCEL_ALL_ORDERS_CEILING, EXECUTE_FILLS_CEILING, ORDERS_OFFSET,
            REMOVED_ORDER_BYTES,
        },
        CancelAllArgsV0, CancelOrderArgsV0, EvictWorstArgsV0, ExecuteArgsV0, PlaceOrderArgsV0,
        QuoteArgsV0, RemoveExpiredArgsV0, ResizeMarketArgsV0, UpdateMarketArgsV0,
    },
    litesvm::types::{FailedTransactionMetadata, TransactionMetadata},
    solana_clock::Clock,
    solana_pubkey::Pubkey,
};

const SO_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/clob.so");

/// Arena capacity for test markets (chosen per market at creation).
const CAPACITY: usize = 1024;
const PER_SIDE: usize = CAPACITY / 2;

fn program_id() -> Pubkey {
    "BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU"
        .parse()
        .unwrap()
}

fn addr(pk: Pubkey) -> Address {
    Address::new_from_array(pk.to_bytes())
}

fn system_program() -> Pubkey {
    "11111111111111111111111111111111".parse().unwrap()
}

struct Ctx {
    svm: LiteSVM,
    payer: Keypair,
    admin: Keypair,
    place_auth: Keypair,
    market: Pubkey,
}

fn setup() -> Ctx {
    setup_with_capacity(CAPACITY)
}

fn setup_with_capacity(capacity: usize) -> Ctx {
    let mut svm = anchor_v2_testing::svm();
    svm.add_program_from_file(program_id(), SO_PATH)
        .expect("clob.so missing — run `bun run program:build:clob` first");

    let payer = Keypair::new();
    let admin = Keypair::new();
    let place_auth = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    // Pre-create the zeroed market account, sized for CAPACITY orders
    // (larger than CPI alloc limits, so real deploys create it the same
    // way). `zeroed` verifies the discriminator bytes are zero and stamps
    // them; the slab derives capacity from the data length.
    let market = Pubkey::new_unique();
    let space = ClobMarketV0::space_for(capacity as u32);
    let rent = svm.minimum_balance_for_rent_exemption(space);
    svm.set_account(
        market,
        solana_account::Account {
            lamports: rent,
            data: vec![0u8; space],
            owner: program_id(),
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    let ix = instruction::InitializeMarketV0 {
        config: MarketConfigV0 {
            market_index: 0,
            base_precision: 1,
            order_tick_size: 1,
            order_step_size: 1,
            min_order_size: 1,
            default_activation_delay_slots: 1,
            max_activation_delay_slots: 20,
            unknown_user_grace_slots: 2,
            evict_threshold_per_side: 6,
            max_quote_levels: 128,
            max_execute_fills: 64,
            max_execute_users: 32,
        },
    }
    .to_instruction(accounts::InitializeMarketV0 {
        authority: addr(admin.pubkey()),
        place_authority: addr(place_auth.pubkey()),
        market: addr(market),
    });
    let mut ctx = Ctx {
        svm,
        payer,
        admin,
        place_auth,
        market,
    };
    send(&mut ctx, ix).unwrap();
    ctx
}

/// Sign with payer plus whichever of the known keys the metas mark as signer.
fn send(ctx: &mut Ctx, ix: Instruction) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    send_with_budget(ctx, ix, None)
}

/// `send`, optionally preceded by a compute-budget instruction. The default
/// 200k is not enough for the ceiling cases (a full-width execute is a lot of
/// book work), and raising it in the tx is what a real caller would do too.
fn send_with_budget(
    ctx: &mut Ctx,
    ix: Instruction,
    compute_unit_limit: Option<u32>,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    // Fresh blockhash per send so identical instruction streams don't dedupe.
    ctx.svm.expire_blockhash();
    let blockhash = ctx.svm.latest_blockhash();
    let ixs: Vec<Instruction> = compute_unit_limit
        .map(|limit| Instruction {
            program_id: "ComputeBudget111111111111111111111111111111"
                .parse()
                .unwrap(),
            accounts: Vec::new(),
            // Tag 2 is SetComputeUnitLimit(u32).
            data: [&[2u8][..], &limit.to_le_bytes()[..]].concat(),
        })
        .into_iter()
        .chain(core::iter::once(ix.clone()))
        .collect();
    let msg = Message::new_with_blockhash(&ixs, Some(&ctx.payer.pubkey()), &blockhash);
    let mut signers: Vec<&dyn anchor_v2_testing::Signer> = vec![&ctx.payer];
    for kp in [&ctx.admin, &ctx.place_auth] {
        let needed = ix
            .accounts
            .iter()
            .any(|m| m.is_signer && m.pubkey.to_bytes() == kp.pubkey().to_bytes());
        if needed && kp.pubkey() != ctx.payer.pubkey() {
            signers.push(kp);
        }
    }
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &signers).unwrap();
    ctx.svm.send_transaction(tx)
}

/// Tests key users by a bare address; the wire wants the derivable form.
fn uref(user: Address) -> UserRefV0 {
    UserRefV0 {
        authority: user,
        sub_account_id: 0,
    }
}

fn place_ix(ctx: &Ctx, mut args: PlaceOrderArgsV0, user: Address) -> Instruction {
    args.user = uref(user);
    instruction::PlaceOrderV0 { args }.to_instruction(accounts::PlaceOrderV0 {
        market: addr(ctx.market),
        place_authority: addr(ctx.place_auth.pubkey()),
    })
}

fn place_args(side: Side, price: u64, size: u64) -> PlaceOrderArgsV0 {
    PlaceOrderArgsV0 {
        side,
        price,
        base_asset_amount: size,
        activation_delay_slots: None,
        max_ts: 0,
        user: uref(addr(Pubkey::default())),
        taker_origin: false,
    }
}

/// A migrated taker remainder: same placement, marked taker-origin.
fn taker_origin_args(side: Side, price: u64, size: u64) -> PlaceOrderArgsV0 {
    PlaceOrderArgsV0 {
        taker_origin: true,
        ..place_args(side, price, size)
    }
}

// --- wire parsing (borsh-compatible LE layouts) ---

fn parse_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}
fn parse_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

fn parse_order_ref(b: &[u8]) -> OrderRefV0 {
    OrderRefV0 {
        node_index: parse_u32(b),
        order_id: parse_u64(&b[4..]),
    }
}

/// Read the response bytes the returned pointer designates.
fn read_response(ctx: &Ctx, meta: &TransactionMetadata) -> Vec<u8> {
    assert_eq!(
        meta.return_data.program_id.to_bytes(),
        program_id().to_bytes()
    );
    let offset = parse_u32(&meta.return_data.data) as usize;
    let len = parse_u32(&meta.return_data.data[4..]) as usize;
    let account = ctx.svm.get_account(&ctx.market).unwrap();
    account.data[offset..offset + len].to_vec()
}

/// QuoteResponseV0 { levels: Vec<PriceLevel { price: u64, size: u64 }> }
fn parse_levels(b: &[u8]) -> Vec<(u64, u64)> {
    clob::state::QuoteResponseV0::parse(b)
        .expect("quote response")
        .levels
        .iter()
        .map(|level| (level.price, level.size))
        .collect()
}

/// The execute response, read through the layout's own parser rather than a
/// second hand-rolled walk of it. Returns `(authority, base, quote, consumed
/// order ids)` per change; tests place with sub-account 0.
fn parse_balance_changes(b: &[u8]) -> Vec<([u8; 32], u64, u64, Vec<u64>)> {
    let response = clob::state::ExecuteResponseV0::parse(b).expect("execute response");
    response
        .changes
        .iter()
        .enumerate()
        .map(|(i, change)| {
            assert_eq!(change.user.sub_account_id, 0);
            (
                *change.user.authority.as_array(),
                change.base_size,
                change.quote_size,
                response.completed_for(i).collect(),
            )
        })
        .collect()
}

fn place(ctx: &mut Ctx, args: PlaceOrderArgsV0, user: Address) -> OrderRefV0 {
    let ix = place_ix(ctx, args, user);
    let meta = send(ctx, ix).unwrap();
    parse_order_ref(&meta.return_data.data)
}

/// The wire's settleable-user set from a test's `Option<Vec<Address>>`:
/// `None` is the unrestricted set.
fn user_set(users: Option<Vec<Address>>) -> UserSetV0 {
    match users {
        None => UserSetV0::EMPTY,
        Some(users) => {
            let refs: Vec<_> = users.into_iter().map(uref).collect();
            UserSetV0::from_refs(&refs).unwrap()
        }
    }
}

fn quote_meta_users(
    ctx: &mut Ctx,
    direction: Direction,
    size: u64,
    users: Option<Vec<Address>>,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    let ix = instruction::QuoteV0 {
        args: QuoteArgsV0 {
            caps: UserCapsV0::EMPTY,
            reference_price: 0,
            direction,
            size,
            users: user_set(users),
            taker: None,
        },
    }
    .to_instruction(accounts::QuoteV0 {
        market: addr(ctx.market),
    });
    send(ctx, ix)
}

fn quote_users(
    ctx: &mut Ctx,
    direction: Direction,
    size: u64,
    users: Option<Vec<Address>>,
) -> Vec<(u64, u64)> {
    let meta = quote_meta_users(ctx, direction, size, users).unwrap();
    parse_levels(&read_response(ctx, &meta))
}

fn quote(ctx: &mut Ctx, direction: Direction, size: u64) -> Vec<(u64, u64)> {
    quote_users(ctx, direction, size, None)
}

/// `(price, base)` the quote gave up on for want of a loaded user, or `None`
/// when it reached everything it was asked for.
fn quote_withheld(
    ctx: &mut Ctx,
    direction: Direction,
    size: u64,
    users: Option<Vec<Address>>,
) -> Option<(u64, u64)> {
    let meta = quote_meta_users(ctx, direction, size, users).unwrap();
    let bytes = read_response(ctx, &meta);
    let response = clob::state::QuoteResponseV0::parse(&bytes).expect("quote response");
    (response.withheld_price != 0).then_some((response.withheld_price, response.withheld_base))
}

fn execute_meta_users(
    ctx: &mut Ctx,
    direction: Direction,
    size: u64,
    users: Option<Vec<Address>>,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    let ix = instruction::ExecuteV0 {
        args: ExecuteArgsV0 {
            caps: UserCapsV0::EMPTY,
            reference_price: 0,
            direction,
            size,
            users: user_set(users),
            taker: None,
        },
    }
    .to_instruction(accounts::ExecuteV0 {
        market: addr(ctx.market),
        place_authority: addr(ctx.place_auth.pubkey()),
    });
    send(ctx, ix)
}

fn execute_meta(
    ctx: &mut Ctx,
    direction: Direction,
    size: u64,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    execute_meta_users(ctx, direction, size, None)
}

fn execute_users(
    ctx: &mut Ctx,
    direction: Direction,
    size: u64,
    users: Option<Vec<Address>>,
) -> Vec<([u8; 32], u64, u64, Vec<u64>)> {
    let meta = execute_meta_users(ctx, direction, size, users).unwrap();
    parse_balance_changes(&read_response(ctx, &meta))
}

fn execute(ctx: &mut Ctx, direction: Direction, size: u64) -> Vec<([u8; 32], u64, u64, Vec<u64>)> {
    execute_users(ctx, direction, size, None)
}

fn evict_worst(
    ctx: &mut Ctx,
    side: Side,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    let ix = instruction::EvictWorstV0 {
        args: EvictWorstArgsV0 { side },
    }
    .to_instruction(accounts::EvictWorstV0 {
        market: addr(ctx.market),
        place_authority: addr(ctx.place_auth.pubkey()),
    });
    send(ctx, ix)
}

fn remove_expired(
    ctx: &mut Ctx,
    order_ref: OrderRefV0,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    let ix = instruction::RemoveExpiredV0 {
        args: RemoveExpiredArgsV0 { order_ref },
    }
    .to_instruction(accounts::RemoveExpiredV0 {
        market: addr(ctx.market),
        place_authority: addr(ctx.place_auth.pubkey()),
    });
    send(ctx, ix)
}

/// RemovedOrderV0 return data:
/// (user, order_id, price, base_asset_amount, side, taker_origin).
fn parse_removed(b: &[u8]) -> ([u8; 32], u64, u64, u64, u8, bool) {
    assert_eq!(u16::from_le_bytes(b[32..34].try_into().unwrap()), 0);
    assert_eq!(b.len(), REMOVED_ORDER_BYTES);
    (
        b[..32].try_into().unwrap(),
        parse_u64(&b[34..]),
        parse_u64(&b[42..]),
        parse_u64(&b[50..]),
        b[58],
        match b[59] {
            0 => false,
            1 => true,
            other => panic!("taker_origin is not a borsh bool: {other}"),
        },
    )
}

/// The response's sub-min culls, through the layout's own parser.
fn parse_cancelled(b: &[u8]) -> Vec<([u8; 32], u64, u64)> {
    let response = clob::state::ExecuteResponseV0::parse(b).expect("execute response");
    response
        .cancelled
        .iter()
        .map(|cull| {
            (
                *cull.user.authority.as_array(),
                cull.order_id,
                cull.base_asset_amount,
            )
        })
        .collect()
}

/// The single `sol_log_data` field of a transaction, decoded. Events are
/// logged as one base64 blob per `Program data:` line, which is what every
/// decoder expects — a discriminator and body split across two syscall fields
/// would show up here as two space-separated blobs.
fn program_data(meta: &TransactionMetadata) -> Vec<u8> {
    const PREFIX: &str = "Program data: ";
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let encoded = meta
        .logs
        .iter()
        .find_map(|log| log.strip_prefix(PREFIX))
        .expect("an event was logged");
    assert!(
        !encoded.contains(' '),
        "event was logged as multiple fields"
    );
    let mut bytes = Vec::new();
    let (mut accumulator, mut bits) = (0u32, 0u32);
    for byte in encoded.bytes().filter(|byte| *byte != b'=') {
        let value = ALPHABET
            .iter()
            .position(|c| *c == byte)
            .expect("base64 alphabet") as u32;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((accumulator >> bits) as u8);
        }
    }
    bytes
}

fn cancel_all_ix(ctx: &Ctx, user: Address, sides: CancelSidesV0) -> Instruction {
    instruction::CancelAllV0 {
        args: CancelAllArgsV0 {
            user: uref(user),
            sides,
        },
    }
    .to_instruction(accounts::CancelAllV0 {
        market: addr(ctx.market),
        place_authority: addr(ctx.place_auth.pubkey()),
    })
}

/// `CancelAllOutcomeV0` off return data:
/// `(bid_base, ask_base, bid_orders, ask_orders, exhaustive)`.
fn parse_cancel_all(b: &[u8]) -> (u64, u64, u32, u32, bool) {
    assert_eq!(b.len(), 34 + 8 + 8 + 4 + 4 + 1, "outcome wire width");
    (
        parse_u64(&b[34..]),
        parse_u64(&b[42..]),
        parse_u32(&b[50..]),
        parse_u32(&b[54..]),
        b[58] == 1,
    )
}

/// The `OrdersCancelRecordV0` payload: skips the fixed prefix and returns the
/// logged id list.
fn parse_cancel_all_record(bytes: &[u8]) -> (bool, Vec<u64>) {
    assert_eq!(
        &bytes[..8],
        OrdersCancelRecordV0::DISCRIMINATOR,
        "not a cancel-all record"
    );
    // [disc 8][authority 32][ts 8][bid base 8][ask base 8][market 2][sub 2]
    // [sides 1][exhaustive 1][count 4][ids…]
    const IDS: usize = 8 + 32 + 8 + 8 + 8 + 2 + 2 + 1 + 1 + 4;
    let exhaustive = bytes[IDS - 5] == 1;
    let count = parse_u32(&bytes[IDS - 4..]) as usize;
    let ids = (0..count)
        .map(|i| parse_u64(&bytes[IDS + i * 8..]))
        .collect();
    (exhaustive, ids)
}

fn market_state(ctx: &Ctx) -> ClobHeaderV0 {
    let account = ctx.svm.get_account(&ctx.market).unwrap();
    bytemuck::pod_read_unaligned(&account.data[8..8 + core::mem::size_of::<ClobHeaderV0>()])
}

fn node(ctx: &Ctx, index: u32) -> OrderNodeV0 {
    let account = ctx.svm.get_account(&ctx.market).unwrap();
    let off = ORDERS_OFFSET + index as usize * core::mem::size_of::<OrderNodeV0>();
    bytemuck::pod_read_unaligned(&account.data[off..off + core::mem::size_of::<OrderNodeV0>()])
}

fn advance_slot(ctx: &mut Ctx, by: u64) {
    let clock: Clock = ctx.svm.get_sysvar();
    ctx.svm.warp_to_slot(clock.slot + by);
}

fn set_unix_timestamp(ctx: &mut Ctx, ts: i64) {
    let mut clock: Clock = ctx.svm.get_sysvar();
    clock.unix_timestamp = ts;
    ctx.svm.set_sysvar(&clock);
}

#[track_caller]
fn assert_clob_err(result: Result<TransactionMetadata, FailedTransactionMetadata>, code: u32) {
    let err = format!("{:?}", result.expect_err("expected failure").err);
    assert!(
        err.contains(&format!("Custom({code})")),
        "expected Custom({code}), got {err}"
    );
}

fn err_code(e: clob::error::ClobError) -> u32 {
    e as u32 + 6000
}

#[test]
fn price_time_priority_and_level_aggregation() {
    let mut ctx = setup();
    let user_a = addr(Pubkey::new_unique());
    let user_b = addr(Pubkey::new_unique());

    place(&mut ctx, place_args(Side::Ask, 101, 10), user_b);
    place(&mut ctx, place_args(Side::Ask, 100, 5), user_a); // better price
    place(&mut ctx, place_args(Side::Ask, 100, 7), user_b); // same level, later
    place(&mut ctx, place_args(Side::Bid, 99, 4), user_a);
    advance_slot(&mut ctx, 1); // clear the speed bump

    assert_eq!(
        quote(&mut ctx, Direction::Long, 100),
        vec![(100, 12), (101, 10)]
    );
    // Quote caps at the requested size.
    assert_eq!(quote(&mut ctx, Direction::Long, 6), vec![(100, 6)]);

    // Execute 8: all of A's 5 (first at the level), then 3 of B's — merged
    // into one balance change per user.
    let changes = execute(&mut ctx, Direction::Long, 8);
    assert_eq!(changes.len(), 2);
    assert_eq!((changes[0].0, changes[0].1), (user_a.to_bytes(), 5));
    assert_eq!((changes[1].0, changes[1].1), (user_b.to_bytes(), 3));
    assert_eq!(
        quote(&mut ctx, Direction::Long, 100),
        vec![(100, 4), (101, 10)]
    );
    let state = market_state(&ctx);
    assert_eq!(state.ask_count, 2);
    assert_eq!(state.bid_count, 1);
}

#[test]
fn cancel_verifies_hint_and_user() {
    let mut ctx = setup();
    let user = addr(Pubkey::new_unique());
    let order_ref = place(&mut ctx, place_args(Side::Bid, 50, 1), user);

    let cancel = |ctx: &mut Ctx, user: Address, order_ref: OrderRefV0| {
        let ix = instruction::CancelOrderV0 {
            args: CancelOrderArgsV0 {
                order_ref,
                user: uref(user),
            },
        }
        .to_instruction(accounts::CancelOrderV0 {
            market: addr(ctx.market),
            place_authority: addr(ctx.place_auth.pubkey()),
        });
        send(ctx, ix)
    };

    assert_clob_err(
        cancel(
            &mut ctx,
            user,
            OrderRefV0 {
                order_id: order_ref.order_id + 1,
                ..order_ref
            },
        ),
        err_code(clob::error::ClobError::StaleOrderRef),
    );
    assert_clob_err(
        cancel(&mut ctx, addr(Pubkey::new_unique()), order_ref),
        err_code(clob::error::ClobError::OrderUserMismatch),
    );
    cancel(&mut ctx, user, order_ref).unwrap();
    // Freed node: same hint can't cancel twice.
    assert_clob_err(
        cancel(&mut ctx, user, order_ref),
        err_code(clob::error::ClobError::StaleOrderRef),
    );
    let state = market_state(&ctx);
    assert_eq!(state.bid_count, 0);
    assert_eq!(state.free_count, CAPACITY as u32);
}

#[test]
fn place_rejects_off_grid_undersized_and_bad_authority() {
    let mut ctx = setup();
    let user = addr(Pubkey::new_unique());

    // Tighten the grid via update_market_v0.
    let ix = instruction::UpdateMarketV0 {
        args: UpdateMarketArgsV0 {
            order_tick_size: Some(10),
            order_step_size: Some(5),
            min_order_size: Some(10),
            ..Default::default()
        },
    }
    .to_instruction(accounts::UpdateMarketV0 {
        market: addr(ctx.market),
        authority: addr(ctx.admin.pubkey()),
        new_place_authority: None,
    });
    send(&mut ctx, ix).unwrap();

    let try_place = |ctx: &mut Ctx, price: u64, size: u64| {
        let ix = place_ix(ctx, place_args(Side::Bid, price, size), user);
        send(ctx, ix)
    };
    assert_clob_err(
        try_place(&mut ctx, 101, 10),
        err_code(clob::error::ClobError::PriceNotTickAligned),
    );
    assert_clob_err(
        try_place(&mut ctx, 100, 12),
        err_code(clob::error::ClobError::SizeNotStepAligned),
    );
    assert_clob_err(
        try_place(&mut ctx, 100, 5),
        err_code(clob::error::ClobError::OrderTooSmall),
    );
    try_place(&mut ctx, 100, 10).unwrap();

    // A random signer can't place.
    let rando = Keypair::new();
    let ix = instruction::PlaceOrderV0 {
        args: PlaceOrderArgsV0 {
            user: uref(user),
            ..place_args(Side::Bid, 100, 10)
        },
    }
    .to_instruction(accounts::PlaceOrderV0 {
        market: addr(ctx.market),
        place_authority: addr(rando.pubkey()),
    });
    ctx.svm.airdrop(&rando.pubkey(), 1_000_000_000).unwrap();
    ctx.svm.expire_blockhash();
    let blockhash = ctx.svm.latest_blockhash();
    let msg = Message::new_with_blockhash(&[ix], Some(&ctx.payer.pubkey()), &blockhash);
    let signers: Vec<&dyn anchor_v2_testing::Signer> = vec![&ctx.payer, &rando];
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &signers).unwrap();
    let result = ctx.svm.send_transaction(tx);
    assert_clob_err(result, err_code(clob::error::ClobError::InvalidAuthority));
}

/// `execute_v0` consumes resting orders but creates no positions anywhere —
/// only velocity does that, out of the balance changes this returns. An
/// unauthorized caller could therefore wipe the book for free, so the same
/// `place_authority` gate placement uses covers it.
#[test]
fn execute_rejects_unauthorized_caller() {
    let mut ctx = setup();
    let user = addr(Pubkey::new_unique());
    ctx.svm.warp_to_slot(10);
    place(&mut ctx, place_args(Side::Ask, 100, 5), user);
    ctx.svm.warp_to_slot(11);

    let args = || ExecuteArgsV0 {
        caps: UserCapsV0::EMPTY,
        reference_price: 0,
        direction: Direction::Long,
        size: 5,
        users: UserSetV0::EMPTY,
        taker: None,
    };
    let execute_ix = |authority: Pubkey| {
        instruction::ExecuteV0 { args: args() }.to_instruction(accounts::ExecuteV0 {
            market: addr(ctx.market),
            place_authority: addr(authority),
        })
    };

    // A different signer is not the book's place authority.
    let rando = Keypair::new();
    ctx.svm.airdrop(&rando.pubkey(), 1_000_000_000).unwrap();
    ctx.svm.expire_blockhash();
    let blockhash = ctx.svm.latest_blockhash();
    let msg = Message::new_with_blockhash(
        &[execute_ix(rando.pubkey())],
        Some(&ctx.payer.pubkey()),
        &blockhash,
    );
    let signers: Vec<&dyn anchor_v2_testing::Signer> = vec![&ctx.payer, &rando];
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &signers).unwrap();
    assert_clob_err(
        ctx.svm.send_transaction(tx),
        err_code(clob::error::ClobError::InvalidAuthority),
    );

    // Naming the right authority without its signature is not enough either.
    let mut unsigned = execute_ix(ctx.place_auth.pubkey());
    for meta in &mut unsigned.accounts {
        meta.is_signer = false;
    }
    ctx.svm.expire_blockhash();
    let blockhash = ctx.svm.latest_blockhash();
    let msg = Message::new_with_blockhash(&[unsigned], Some(&ctx.payer.pubkey()), &blockhash);
    let signers: Vec<&dyn anchor_v2_testing::Signer> = vec![&ctx.payer];
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &signers).unwrap();
    let err = format!(
        "{:?}",
        ctx.svm
            .send_transaction(tx)
            .expect_err("expected failure")
            .err
    );
    assert!(
        err.contains("MissingRequiredSignature"),
        "expected MissingRequiredSignature, got {err}"
    );

    // The order is untouched, and the real authority can still take it.
    assert_eq!(market_state(&ctx).ask_count, 1);
    assert_eq!(execute(&mut ctx, Direction::Long, 5).len(), 1);
    assert_eq!(market_state(&ctx).ask_count, 0);
}

#[test]
fn hard_cap_rejects_placement_and_crank_evicts_tail() {
    let mut ctx = setup_with_capacity(16); // 8 per side, evict threshold 6
    let user = addr(Pubkey::new_unique());

    // Below the soft cap there is nothing to evict.
    for i in 0..5u64 {
        let ix = place_ix(&ctx, place_args(Side::Bid, 100 + i, 1), user);
        send(&mut ctx, ix).unwrap();
    }
    assert_clob_err(
        evict_worst(&mut ctx, Side::Bid),
        err_code(clob::error::ClobError::BelowEvictThreshold),
    );

    // At the hard cap every placement is rejected, even better-priced —
    // eviction is crank-mediated so the evicted maker's aggregates update.
    for i in 5..8u64 {
        let ix = place_ix(&ctx, place_args(Side::Bid, 100 + i, 1), user);
        send(&mut ctx, ix).unwrap();
    }
    let ix = place_ix(&ctx, place_args(Side::Bid, 200, 1), user);
    assert_clob_err(
        send(&mut ctx, ix),
        err_code(clob::error::ClobError::SideAtCapacity),
    );

    // The crank removes the tail (worst price) and reports it for velocity.
    let meta = evict_worst(&mut ctx, Side::Bid).unwrap();
    let (evicted_user, _, price, base, side, taker_origin) = parse_removed(&meta.return_data.data);
    assert_eq!(
        (evicted_user, price, base, side, taker_origin),
        (user.to_bytes(), 100, 1, Side::Bid.to_u8(), false)
    );
    place(&mut ctx, place_args(Side::Bid, 200, 1), user);
    let state = market_state(&ctx);
    assert_eq!(state.bid_count, 8);
    assert_eq!(node(&ctx, state.best_bid).price, 200);

    // Sides are independent: the empty ask side has nothing to evict.
    assert_clob_err(
        evict_worst(&mut ctx, Side::Ask),
        err_code(clob::error::ClobError::BelowEvictThreshold),
    );
}

#[test]
fn speed_bump_gates_matching_until_activation_slot() {
    let mut ctx = setup();
    let user = addr(Pubkey::new_unique());
    ctx.svm.warp_to_slot(10);
    place(&mut ctx, place_args(Side::Ask, 100, 5), user); // activates at 11

    assert!(quote(&mut ctx, Direction::Long, 5).is_empty());
    assert!(execute(&mut ctx, Direction::Long, 5).is_empty());

    ctx.svm.warp_to_slot(11);
    assert_eq!(quote(&mut ctx, Direction::Long, 5).len(), 1);
    assert_eq!(execute(&mut ctx, Direction::Long, 5).len(), 1);
}

#[test]
fn auction_delay_is_clamped_to_market_bounds() {
    let mut ctx = setup();
    let user = addr(Pubkey::new_unique());
    ctx.svm.warp_to_slot(10);

    // Above max (21 > 20) rejected; zero is allowed (velocity policy).
    let ix = place_ix(
        &ctx,
        PlaceOrderArgsV0 {
            activation_delay_slots: Some(21),
            ..place_args(Side::Ask, 100, 5)
        },
        user,
    );
    assert_clob_err(
        send(&mut ctx, ix),
        err_code(clob::error::ClobError::InvalidActivationDelay),
    );
    // A 20-slot auction: invisible at +19, live at +20.
    let ix = place_ix(
        &ctx,
        PlaceOrderArgsV0 {
            activation_delay_slots: Some(20),
            ..place_args(Side::Ask, 100, 5)
        },
        user,
    );
    send(&mut ctx, ix).unwrap();
    ctx.svm.warp_to_slot(29);
    assert!(quote(&mut ctx, Direction::Long, 5).is_empty());
    ctx.svm.warp_to_slot(30);
    assert_eq!(quote(&mut ctx, Direction::Long, 5).len(), 1);
}

#[test]
fn zero_delay_activates_immediately() {
    // Attestation policy lives in velocity; the CLOB trusts place_authority
    // to request delay 0.
    let mut ctx = setup();
    let user = addr(Pubkey::new_unique());
    ctx.svm.warp_to_slot(10);

    let ix = place_ix(
        &ctx,
        PlaceOrderArgsV0 {
            activation_delay_slots: Some(0),
            ..place_args(Side::Ask, 100, 5)
        },
        user,
    );
    send(&mut ctx, ix).unwrap();
    // Active immediately, same slot.
    assert_eq!(quote(&mut ctx, Direction::Long, 5).len(), 1);
}

#[test]
fn expired_orders_are_skipped_and_cranked_off() {
    let mut ctx = setup();
    let user = addr(Pubkey::new_unique());
    set_unix_timestamp(&mut ctx, 1_000);
    let order_ref = place(
        &mut ctx,
        PlaceOrderArgsV0 {
            max_ts: 1_020,
            ..place_args(Side::Ask, 100, 5)
        },
        user,
    );
    advance_slot(&mut ctx, 1);

    // Not expired yet: the crank can't reclaim it.
    assert_clob_err(
        remove_expired(&mut ctx, order_ref),
        err_code(clob::error::ClobError::OrderNotExpired),
    );

    set_unix_timestamp(&mut ctx, 1_021);
    // Skipped by quote/execute but NOT removed — reclamation goes through
    // velocity (remove_expired) so the maker's aggregates update.
    assert!(quote(&mut ctx, Direction::Long, 5).is_empty());
    assert!(execute(&mut ctx, Direction::Long, 5).is_empty());
    let state = market_state(&ctx);
    assert_eq!(state.ask_count, 1);
    assert_eq!(state.free_count, CAPACITY as u32 - 1);

    let meta = remove_expired(&mut ctx, order_ref).unwrap();
    let (removed_user, _, _, base, side, taker_origin) = parse_removed(&meta.return_data.data);
    assert_eq!(
        (removed_user, base, side, taker_origin),
        (user.to_bytes(), 5, Side::Ask.to_u8(), false)
    );
    let state = market_state(&ctx);
    assert_eq!(state.ask_count, 0);
    assert_eq!(state.free_count, CAPACITY as u32);
    // Freed node: the stale ref fails closed.
    assert_clob_err(
        remove_expired(&mut ctx, order_ref),
        err_code(clob::error::ClobError::StaleOrderRef),
    );

    // Placing with max_ts in the past is rejected outright.
    let ix = place_ix(
        &ctx,
        PlaceOrderArgsV0 {
            max_ts: 500,
            ..place_args(Side::Ask, 100, 5)
        },
        user,
    );
    assert_clob_err(
        send(&mut ctx, ix),
        err_code(clob::error::ClobError::MaxTsInPast),
    );
}

#[test]
fn unknown_user_grace_skips_fresh_orders_and_stops_on_aged_ones() {
    let mut ctx = setup(); // grace = 2 slots
    let user_a = addr(Pubkey::new_unique());
    let user_b = addr(Pubkey::new_unique());
    ctx.svm.warp_to_slot(10);
    place(&mut ctx, place_args(Side::Ask, 100, 5), user_a);
    place(&mut ctx, place_args(Side::Ask, 101, 7), user_b);
    ctx.svm.warp_to_slot(11); // both active, age 1 <= grace

    // A's user missing but fresh: skipped, the fill continues past it. The
    // caller could not have heard of it yet, so nothing is reported.
    assert_eq!(
        quote_users(&mut ctx, Direction::Long, 12, Some(vec![user_b])),
        vec![(101, 7)]
    );
    assert_eq!(
        quote_withheld(&mut ctx, Direction::Long, 12, Some(vec![user_b])),
        None
    );
    let changes = execute_users(&mut ctx, Direction::Long, 7, Some(vec![user_b]));
    assert_eq!(changes, vec![(user_b.to_bytes(), 7, 707, vec![2])]);
    // A's order still resting, untouched.
    assert_eq!(quote(&mut ctx, Direction::Long, 12), vec![(100, 5)]);

    // Past the grace window the walk ends at A rather than filling around it.
    // A is the best price, so a caller that left it out gets nothing from
    // this book — it can trade less of the book, never a better part of it.
    place(&mut ctx, place_args(Side::Ask, 101, 7), user_b);
    ctx.svm.warp_to_slot(14); // A age 4 > grace
    assert!(quote_users(&mut ctx, Direction::Long, 12, Some(vec![user_b])).is_empty());
    assert_eq!(
        quote_withheld(&mut ctx, Direction::Long, 12, Some(vec![user_b])),
        Some((100, 5)),
        "the book says where it stopped and what it was holding there"
    );
    assert!(
        execute_users(&mut ctx, Direction::Long, 12, Some(vec![user_b])).is_empty(),
        "leaving out the best maker forfeits the book, it does not reach past it"
    );

    // Complete user set fills both.
    let changes = execute_users(&mut ctx, Direction::Long, 12, Some(vec![user_a, user_b]));
    assert_eq!(changes.len(), 2);
}

/// Truncation is the caller's own tradeoff, and it may only cost depth. A
/// caller that carries the best maker and stops fills that far; the rest of
/// the book stays resting for a later transaction with a different set.
#[test]
fn a_short_user_set_trades_less_of_the_book_not_a_worse_part() {
    let mut ctx = setup();
    let best = addr(Pubkey::new_unique());
    let rest = addr(Pubkey::new_unique());
    ctx.svm.warp_to_slot(10);
    place(&mut ctx, place_args(Side::Ask, 100, 5), best);
    place(&mut ctx, place_args(Side::Ask, 101, 7), rest);
    ctx.svm.warp_to_slot(20); // both well past the grace window

    // Carrying the best maker alone: its level fills, and the book reports
    // the depth behind it as withheld.
    assert_eq!(
        quote_users(&mut ctx, Direction::Long, 12, Some(vec![best])),
        vec![(100, 5)]
    );
    assert_eq!(
        quote_withheld(&mut ctx, Direction::Long, 12, Some(vec![best])),
        Some((101, 7))
    );
    let changes = execute_users(&mut ctx, Direction::Long, 12, Some(vec![best]));
    assert_eq!(changes, vec![(best.to_bytes(), 5, 500, vec![1])]);

    // The maker behind is untouched and reachable by the next fill.
    assert_eq!(quote(&mut ctx, Direction::Long, 12), vec![(101, 7)]);
}

/// The report is one order deep, and deliberately so.
///
/// It says "here is the order I stopped on", not "here is everything behind
/// me". A caller holds that much back from worse-priced sources and no more,
/// which understates a deep book — genuine depth past the stop does reach a
/// worse price. The alternative is worse: a report covering everything behind
/// the stop would let anyone who can occupy the caller's account budget with
/// small orders declare an arbitrary amount of the taker's size unfillable,
/// and cancel afterwards. Under-reporting caps what that is worth to them.
#[test]
fn the_withheld_report_covers_the_order_it_stopped_on_and_no_more() {
    let mut ctx = setup();
    let carried = addr(Pubkey::new_unique());
    let first_missing = addr(Pubkey::new_unique());
    let behind = addr(Pubkey::new_unique());
    ctx.svm.warp_to_slot(10);
    place(&mut ctx, place_args(Side::Ask, 100, 5), carried);
    place(&mut ctx, place_args(Side::Ask, 101, 7), first_missing);
    place(&mut ctx, place_args(Side::Ask, 102, 900), behind);
    ctx.svm.warp_to_slot(20);

    assert_eq!(
        quote_withheld(&mut ctx, Direction::Long, 1_000, Some(vec![carried])),
        Some((101, 7)),
        "the order the walk stopped on, not the 900 sitting behind it"
    );
}

#[test]
fn partial_fill_remainder_below_min_order_size_is_culled() {
    let mut ctx = setup();
    let user = addr(Pubkey::new_unique());

    let ix = instruction::UpdateMarketV0 {
        args: UpdateMarketArgsV0 {
            min_order_size: Some(10),
            ..Default::default()
        },
    }
    .to_instruction(accounts::UpdateMarketV0 {
        market: addr(ctx.market),
        authority: addr(ctx.admin.pubkey()),
        new_place_authority: None,
    });
    send(&mut ctx, ix).unwrap();

    // 15 of 20 fills; the 5 remaining is below min and is culled with the
    // fill instead of resting as dust. The cull rides the wire response so
    // velocity can decrement the maker's aggregates.
    place(&mut ctx, place_args(Side::Ask, 100, 20), user);
    advance_slot(&mut ctx, 1);
    let meta = execute_meta(&mut ctx, Direction::Long, 15).unwrap();
    let resp = read_response(&ctx, &meta);
    assert_eq!(
        parse_balance_changes(&resp),
        vec![(user.to_bytes(), 15, 1500, vec![])]
    );
    assert_eq!(parse_cancelled(&resp), vec![(user.to_bytes(), 1, 5)]);
    assert!(quote(&mut ctx, Direction::Long, u64::MAX).is_empty());
    let state = market_state(&ctx);
    assert_eq!(state.ask_count, 0);
    assert_eq!(state.free_count, CAPACITY as u32);

    // A remainder at the min keeps resting.
    place(&mut ctx, place_args(Side::Ask, 100, 20), user);
    advance_slot(&mut ctx, 1);
    execute(&mut ctx, Direction::Long, 10);
    assert_eq!(quote(&mut ctx, Direction::Long, u64::MAX), vec![(100, 10)]);
    assert_eq!(market_state(&ctx).ask_count, 1);
}

/// The whole point of the id list: an indexer reconciling the book from events
/// gets one record naming every order that left, and it has to survive the
/// widest case the per-call cap allows — a ~1KB log buffer built in an SBF
/// stack frame, which is exactly where this program has broken before.
#[test]
fn cancel_all_logs_every_removed_order_id_at_the_ceiling() {
    let mut ctx = setup();
    let user = addr(Pubkey::new_unique());
    let other = addr(Pubkey::new_unique());
    let per_side = CANCEL_ALL_ORDERS_CEILING as u64 / 2;
    let mut expected = Vec::new();
    // Interleave a second maker through both sides, so the sweep is a filtered
    // walk rather than a truncation and the ids can't come out of order.
    for i in 0..per_side {
        expected.push(place(&mut ctx, place_args(Side::Bid, 1_000 - i, 10), user).order_id);
        place(&mut ctx, place_args(Side::Bid, 1_000 - i, 10), other);
    }
    for i in 0..per_side {
        expected.push(place(&mut ctx, place_args(Side::Ask, 2_000 + i, 10), user).order_id);
        place(&mut ctx, place_args(Side::Ask, 2_000 + i, 10), other);
    }

    let ix = cancel_all_ix(&ctx, user, CancelSidesV0::Both);
    let meta = send_with_budget(&mut ctx, ix, Some(400_000)).unwrap();
    let (bid_base, ask_base, bid_orders, ask_orders, exhaustive) =
        parse_cancel_all(&meta.return_data.data);
    assert_eq!(bid_orders as u64, per_side);
    assert_eq!(ask_orders as u64, per_side);
    assert_eq!(bid_base, per_side * 10);
    assert_eq!(ask_base, per_side * 10);
    assert!(exhaustive);

    // The record carries every id, bids first, in book order.
    let (logged_exhaustive, ids) = parse_cancel_all_record(&program_data(&meta));
    assert!(logged_exhaustive);
    assert_eq!(ids, expected);

    // Only this maker's orders left.
    let state = market_state(&ctx);
    assert_eq!(state.bid_count as u64, per_side);
    assert_eq!(state.ask_count as u64, per_side);
}

/// Placement policy lives in velocity, so the book takes a sweep only from its
/// `place_authority` — otherwise anyone could pull a maker's quotes.
#[test]
fn cancel_all_requires_the_place_authority() {
    let mut ctx = setup();
    let user = addr(Pubkey::new_unique());
    place(&mut ctx, place_args(Side::Bid, 100, 10), user);

    // The market's own admin is the sharpest version of this: a real key with
    // real authority over the book that still must not be able to pull quotes.
    let mut ix = cancel_all_ix(&ctx, user, CancelSidesV0::Both);
    ix.accounts[1].pubkey = ctx.admin.pubkey();
    assert_clob_err(
        send(&mut ctx, ix),
        err_code(clob::error::ClobError::InvalidAuthority),
    );
    assert_eq!(market_state(&ctx).bid_count, 1);
}

#[test]
fn cu_benchmarks() {
    let mut ctx = setup();
    let user = addr(Pubkey::new_unique());
    for i in 0..PER_SIDE as u64 - 1 {
        let ix = place_ix(&ctx, place_args(Side::Bid, 100 + i, 10), user);
        send(&mut ctx, ix).unwrap();
    }
    advance_slot(&mut ctx, 1);

    // Place at the best of a nearly-full book (fills the side).
    let ix = place_ix(&ctx, place_args(Side::Bid, 100 + PER_SIDE as u64, 10), user);
    let place_meta = send(&mut ctx, ix).unwrap();

    // Crank-evict the tail of the full side.
    let evict_meta = evict_worst(&mut ctx, Side::Bid).unwrap();

    // Quote sweeping the entire side (level cap applies).
    let ix = instruction::QuoteV0 {
        args: QuoteArgsV0 {
            caps: UserCapsV0::EMPTY,
            reference_price: 0,
            direction: Direction::Short,
            size: u64::MAX,
            users: UserSetV0::EMPTY,
            taker: None,
        },
    }
    .to_instruction(accounts::QuoteV0 {
        market: addr(ctx.market),
    });
    let quote_meta = send(&mut ctx, ix).unwrap();

    // Execute across 50 orders (one user, so the response stays small).
    let execute_meta = execute_meta(&mut ctx, Direction::Short, 500).unwrap();

    // Cancel: place a fresh order and remove it (one order per ix).
    let mut ctx2 = setup();
    let u2 = addr(Pubkey::new_unique());
    let oref = place(&mut ctx2, place_args(Side::Bid, 500, 10), u2);
    let cancel_ix = instruction::CancelOrderV0 {
        args: CancelOrderArgsV0 {
            order_ref: oref,
            user: uref(u2),
        },
    }
    .to_instruction(accounts::CancelOrderV0 {
        market: addr(ctx2.market),
        place_authority: addr(ctx2.place_auth.pubkey()),
    });
    let cancel_empty = send(&mut ctx2, cancel_ix).unwrap().compute_units_consumed;

    // Cancel out of a nearly-full side (relink cost at depth).
    let mut refs = Vec::new();
    for i in 0..PER_SIDE as u64 - 1 {
        refs.push(place(&mut ctx2, place_args(Side::Bid, 1_000 + i, 10), u2));
    }
    let mid_ref = refs[refs.len() / 2];
    let cancel_ix = instruction::CancelOrderV0 {
        args: CancelOrderArgsV0 {
            order_ref: mid_ref,
            user: uref(u2),
        },
    }
    .to_instruction(accounts::CancelOrderV0 {
        market: addr(ctx2.market),
        place_authority: addr(ctx2.place_auth.pubkey()),
    });
    let cancel_full = send(&mut ctx2, cancel_ix).unwrap().compute_units_consumed;

    let empty_place = {
        let mut c = setup();
        let u = addr(Pubkey::new_unique());
        let ix = place_ix(&c, place_args(Side::Bid, 100, 10), u);
        send(&mut c, ix).unwrap().compute_units_consumed
    };

    println!(
        "CU — place(empty book): {empty_place}, place(best, full book): {}, \
         cancel(only order): {cancel_empty}, cancel(mid of full side): {cancel_full}, \
         evict_worst: {}, quote(full side): {}, execute(50 orders): {}",
        place_meta.compute_units_consumed,
        evict_meta.compute_units_consumed,
        quote_meta.compute_units_consumed,
        execute_meta.compute_units_consumed,
    );
}

/// A sweep is a filtered walk of each requested side, so its cost is set by the
/// *book's* depth rather than the maker's. This measures both ends of that: a
/// maker's ladder on an otherwise-empty book, and the same ladder buried behind
/// a full side of other makers' orders — against the per-order cancel it
/// replaces.
///
/// Only the shallow case is asserted, because the deep case genuinely loses at
/// this level: the walk pays a hop per resting order while a per-order cancel is
/// O(1), so on a deep book a handful of `cancel_order_v0` calls beat one sweep
/// here. That crossover does not survive contact with velocity, which is the
/// only caller — a `cancel_clob_order` instruction costs ~11k CU of account
/// loading and margin bookkeeping around its ~1.4k of book work, so end to end
/// the sweep wins at every depth (see `cu_bench_cancel_all_beats_cancelling_\
/// order_by_order` in the integration suite). Making the walk stop once the
/// maker's orders run out would need velocity to pass a trusted count, coupling
/// the book's exhaustiveness to velocity's bookkeeping for a case that is
/// already a win.
#[test]
fn cu_benchmark_cancel_all() {
    const LADDER: u64 = 8;

    let mut ctx = setup();
    let mine = addr(Pubkey::new_unique());
    let refs: Vec<OrderRefV0> = (0..LADDER)
        .map(|i| place(&mut ctx, place_args(Side::Ask, 2_000 + i, 10), mine))
        .collect();
    let per_order_cu: u64 = refs
        .iter()
        .map(|order_ref| {
            let ix = instruction::CancelOrderV0 {
                args: CancelOrderArgsV0 {
                    order_ref: *order_ref,
                    user: uref(mine),
                },
            }
            .to_instruction(accounts::CancelOrderV0 {
                market: addr(ctx.market),
                place_authority: addr(ctx.place_auth.pubkey()),
            });
            send(&mut ctx, ix).unwrap().compute_units_consumed
        })
        .sum();

    let mut ctx = setup();
    (0..LADDER).for_each(|i| {
        place(&mut ctx, place_args(Side::Ask, 2_000 + i, 10), mine);
    });
    let ix = cancel_all_ix(&ctx, mine, CancelSidesV0::Both);
    let shallow_cu = send(&mut ctx, ix).unwrap().compute_units_consumed;

    // The same ladder with a full side of other makers' orders ahead of it,
    // which is the walk cost this design accepts in exchange for the book
    // needing no per-user index.
    let mut ctx = setup();
    let other = addr(Pubkey::new_unique());
    (0..PER_SIDE as u64 - LADDER).for_each(|i| {
        place(&mut ctx, place_args(Side::Ask, 1_000 + i, 10), other);
    });
    (0..LADDER).for_each(|i| {
        place(&mut ctx, place_args(Side::Ask, 2_000 + i, 10), mine);
    });
    let ix = cancel_all_ix(&ctx, mine, CancelSidesV0::Both);
    let deep_cu = send_with_budget(&mut ctx, ix, Some(400_000))
        .unwrap()
        .compute_units_consumed;

    println!(
        "CU — cancel_all({LADDER} orders, empty book): {shallow_cu}, \
         ({LADDER} orders behind a full {PER_SIDE}-deep side): {deep_cu}; \
         baseline {LADDER}× cancel_order_v0: {per_order_cu}"
    );
    assert!(
        shallow_cu < per_order_cu,
        "sweeping {LADDER} orders ({shallow_cu}) must beat cancelling them one at a \
         time ({per_order_cu})"
    );
}

/// Worst case for the response encoder: makers interleaved through the book,
/// so most completed order ids have to be spliced into a balance-change
/// record that already has other records written after it.
#[test]
fn cu_benchmark_interleaved_makers() {
    let mut ctx = setup();
    let makers: Vec<Address> = (0..8).map(|_| addr(Pubkey::new_unique())).collect();
    for i in 0..64u64 {
        let ix = place_ix(
            &ctx,
            place_args(Side::Ask, 100 + i, 1),
            makers[(i % 8) as usize],
        );
        send(&mut ctx, ix).unwrap();
    }
    advance_slot(&mut ctx, 1);

    let meta = execute_meta_users(&mut ctx, Direction::Long, 64, Some(makers.clone())).unwrap();
    let changes = parse_balance_changes(&read_response(&ctx, &meta));
    assert_eq!(changes.len(), 8);
    // Every maker's eight orders are fully consumed and reported.
    assert!(changes.iter().all(|change| change.3.len() == 8));
    println!(
        "CU — execute(64 orders, 8 interleaved makers): {}",
        meta.compute_units_consumed
    );
}

/// The widest event and response a market can produce, on-chain: fills and
/// users both configured at their ceilings, and every fill a distinct maker
/// whose order is fully consumed (so every balance-change record also carries a
/// completed order id).
///
/// Two things only a real SBF run can check. The response has to fit the
/// region at the configured ceiling — the point of deriving the ceiling from
/// the record width. And the event's payload is built in a stack buffer sized
/// for that same ceiling, so this is the case that catches a buffer the 4KB SBF
/// stack frame can't hold; the bytes are compared against what anchor's
/// `Event::data()` would have produced, which is the contract with every
/// decoder.
#[test]
fn an_execute_at_the_ceilings_fits_the_response_and_emits_the_record() {
    let mut ctx = setup();
    let fills = EXECUTE_FILLS_CEILING as usize;
    let ix = instruction::UpdateMarketV0 {
        args: UpdateMarketArgsV0 {
            max_execute_fills: Some(EXECUTE_FILLS_CEILING),
            max_execute_users: Some(EXECUTE_FILLS_CEILING),
            ..Default::default()
        },
    }
    .to_instruction(accounts::UpdateMarketV0 {
        market: addr(ctx.market),
        authority: addr(ctx.admin.pubkey()),
        new_place_authority: None,
    });
    send(&mut ctx, ix).unwrap();

    let orders: Vec<OrderRefV0> = (0..fills)
        .map(|i| {
            place(
                &mut ctx,
                place_args(Side::Ask, 100 + i as u64, 1),
                addr(Pubkey::new_unique()),
            )
        })
        .collect();
    advance_slot(&mut ctx, 1);

    let ix = instruction::ExecuteV0 {
        args: ExecuteArgsV0 {
            caps: UserCapsV0::EMPTY,
            reference_price: 0,
            direction: Direction::Long,
            size: fills as u64,
            users: UserSetV0::EMPTY,
            taker: None,
        },
    }
    .to_instruction(accounts::ExecuteV0 {
        market: addr(ctx.market),
        place_authority: addr(ctx.place_auth.pubkey()),
    });
    let meta = send_with_budget(&mut ctx, ix, Some(1_400_000)).unwrap();
    let response = read_response(&ctx, &meta);
    let changes = parse_balance_changes(&response);
    assert_eq!(changes.len(), fills);
    assert!(changes.iter().all(|change| change.3.len() == 1));

    let clock: Clock = ctx.svm.get_sysvar();
    let expected = ExecuteRecordV0 {
        ts: clock.unix_timestamp,
        slot: clock.slot,
        market_index: 0,
        direction: Direction::Long.to_u8(),
        fills: orders
            .iter()
            .map(|order| FillSlimV0 {
                order_id: order.order_id,
                base_size: 1,
            })
            .collect(),
        cancelled_order_ids: vec![],
    };
    assert_eq!(program_data(&meta), Event::data(&expected));
    println!(
        "CU — execute({fills} fills, {fills} makers, at the ceilings): {}, response: {} bytes",
        meta.compute_units_consumed,
        response.len()
    );
}

#[test]
fn resize_grows_arena_and_per_side_capacity() {
    let mut ctx = setup_with_capacity(16); // 8 per side
    let user = addr(Pubkey::new_unique());

    for i in 0..8u64 {
        let ix = place_ix(&ctx, place_args(Side::Bid, 100 + i, 1), user);
        send(&mut ctx, ix).unwrap();
    }
    // Side full; every placement rejected until crank/resize.
    let ix = place_ix(&ctx, place_args(Side::Bid, 100, 1), user);
    assert_clob_err(
        send(&mut ctx, ix),
        err_code(clob::error::ClobError::SideAtCapacity),
    );

    // Grow 16 → 32 (one call: 16 nodes ≈ 1.4KB realloc).
    let ix = instruction::ResizeMarketV0 {
        args: ResizeMarketArgsV0 { new_capacity: 32 },
    }
    .to_instruction(accounts::ResizeMarketV0 {
        payer: addr(ctx.payer.pubkey()),
        market: addr(ctx.market),
        authority: addr(ctx.admin.pubkey()),
        system_program: addr(system_program()),
    });
    send(&mut ctx, ix).unwrap();

    let state = market_state(&ctx);
    assert_eq!(state.free_count, 32 - 8);

    // Shrink is rejected.
    let ix = instruction::ResizeMarketV0 {
        args: ResizeMarketArgsV0 { new_capacity: 16 },
    }
    .to_instruction(accounts::ResizeMarketV0 {
        payer: addr(ctx.payer.pubkey()),
        market: addr(ctx.market),
        authority: addr(ctx.admin.pubkey()),
        system_program: addr(system_program()),
    });
    assert_clob_err(
        send(&mut ctx, ix),
        err_code(clob::error::ClobError::InvalidCapacity),
    );

    // Per-side cap is now 16: worse-priced bids fit without eviction.
    for i in 0..8u64 {
        let ix = place_ix(&ctx, place_args(Side::Bid, 90 + i, 1), user);
        send(&mut ctx, ix).unwrap();
    }
    let state = market_state(&ctx);
    assert_eq!(state.bid_count, 16);
    assert_eq!(state.free_count, 32 - 16);
}

fn quote_taker(ctx: &mut Ctx, direction: Direction, size: u64, taker: Address) -> Vec<(u64, u64)> {
    let ix = instruction::QuoteV0 {
        args: QuoteArgsV0 {
            caps: UserCapsV0::EMPTY,
            reference_price: 0,
            direction,
            size,
            users: UserSetV0::EMPTY,
            taker: Some(uref(taker)),
        },
    }
    .to_instruction(accounts::QuoteV0 {
        market: addr(ctx.market),
    });
    let meta = send(ctx, ix).unwrap();
    parse_levels(&read_response(ctx, &meta))
}

fn execute_taker(
    ctx: &mut Ctx,
    direction: Direction,
    size: u64,
    taker: Address,
) -> Vec<([u8; 32], u64, u64, Vec<u64>)> {
    let ix = instruction::ExecuteV0 {
        args: ExecuteArgsV0 {
            caps: UserCapsV0::EMPTY,
            reference_price: 0,
            direction,
            size,
            users: UserSetV0::EMPTY,
            taker: Some(uref(taker)),
        },
    }
    .to_instruction(accounts::ExecuteV0 {
        market: addr(ctx.market),
        place_authority: addr(ctx.place_auth.pubkey()),
    });
    let meta = send(ctx, ix).unwrap();
    parse_balance_changes(&read_response(ctx, &meta))
}

#[test]
fn taker_own_orders_are_skipped_for_self_trade_prevention() {
    let mut ctx = setup();
    let taker = addr(Pubkey::new_unique());
    let other = addr(Pubkey::new_unique());
    // Taker's own ask at the top of book, another maker behind it.
    place(&mut ctx, place_args(Side::Ask, 100, 5), taker);
    place(&mut ctx, place_args(Side::Ask, 101, 7), other);
    advance_slot(&mut ctx, 1);

    // Quote and execute both walk past the taker's own order — no grace
    // games, no StaleUserSet — and the fills match the quote.
    assert_eq!(
        quote_taker(&mut ctx, Direction::Long, 12, taker),
        vec![(101, 7)]
    );
    let changes = execute_taker(&mut ctx, Direction::Long, 12, taker);
    assert_eq!(changes, vec![(other.to_bytes(), 7, 707, vec![2])]);
    // The taker's own order still rests.
    assert_eq!(quote(&mut ctx, Direction::Long, 12), vec![(100, 5)]);
}

fn cancel(
    ctx: &mut Ctx,
    order_ref: OrderRefV0,
    user: Address,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    let ix = instruction::CancelOrderV0 {
        args: CancelOrderArgsV0 {
            order_ref,
            user: uref(user),
        },
    }
    .to_instruction(accounts::CancelOrderV0 {
        market: addr(ctx.market),
        place_authority: addr(ctx.place_auth.pubkey()),
    });
    send(ctx, ix)
}

/// Velocity is the only caller that can know an order is a migrated taker
/// remainder, so it says so at placement — and gets the fact back out of the
/// removal that lifts the order off the book, which is how it identifies the
/// aggressor of a cross it settles.
#[test]
fn the_taker_origin_flag_round_trips_through_place_and_removal() {
    let mut ctx = setup();
    let user = addr(Pubkey::new_unique());

    let remainder = place(&mut ctx, taker_origin_args(Side::Bid, 101, 5), user);
    let ordinary = place(&mut ctx, place_args(Side::Bid, 90, 5), user);
    assert!(node(&ctx, remainder.node_index).is_taker_origin());
    assert!(
        node(&ctx, remainder.node_index).is_bit_flag_set(OrderBitFlag::Open),
        "the marker must not displace the liveness bit"
    );
    assert!(!node(&ctx, ordinary.node_index).is_taker_origin());

    let meta = cancel(&mut ctx, remainder, user).unwrap();
    let (_, order_id, price, base, side, taker_origin) = parse_removed(&meta.return_data.data);
    assert_eq!(
        (order_id, price, base, side, taker_origin),
        (remainder.order_id, 101, 5, Side::Bid.to_u8(), true)
    );

    let meta = cancel(&mut ctx, ordinary, user).unwrap();
    assert!(!parse_removed(&meta.return_data.data).5);
}

/// The gate, on-chain, and velocity's whole cross-resolution path with it: a
/// taker remainder at 101 with a maker ask at 99 against it is passed over
/// rather than bought at 101 by whoever lands first, the counterparty is
/// ordinary fillable depth at its own 99 (the leg velocity runs), and the
/// remainder then comes off by cancel saying it was the aggressor.
#[test]
fn a_crossed_taker_remainder_is_passed_over_and_its_counterparty_is_not() {
    let mut ctx = setup();
    let taker = addr(Pubkey::new_unique());
    let maker = addr(Pubkey::new_unique());
    let remainder = place(&mut ctx, taker_origin_args(Side::Bid, 101, 5), taker);
    place(&mut ctx, place_args(Side::Ask, 99, 5), maker);
    advance_slot(&mut ctx, 1);

    // Nothing else rests on the bid side, so a taker going that way finds no
    // depth — the call lands and fills nothing, and the book is untouched.
    assert!(quote(&mut ctx, Direction::Short, u64::MAX).is_empty());
    assert!(execute(&mut ctx, Direction::Short, 5).is_empty());
    let state = market_state(&ctx);
    assert_eq!((state.bid_count, state.ask_count), (1, 1));

    // Taking the ask at its own 99 is ordinary liquidity taking, and is the
    // price the pair settles at.
    assert_eq!(quote(&mut ctx, Direction::Long, u64::MAX), vec![(99, 5)]);
    let changes = execute(&mut ctx, Direction::Long, 5);
    assert_eq!(changes, vec![(maker.to_bytes(), 5, 495, vec![2])]);

    // Then the remainder comes off, reporting which side was the aggressor.
    let meta = cancel(&mut ctx, remainder, taker).unwrap();
    let (_, _, price, base, side, taker_origin) = parse_removed(&meta.return_data.data);
    assert_eq!(
        (price, base, side, taker_origin),
        (101, 5, Side::Bid.to_u8(), true)
    );
    assert_eq!(market_state(&ctx).bid_count, 0);
}

/// Skipping rather than failing is what keeps the rest of the side alive. A
/// remainder rests at a slippage bound, so it is normally at the front — failing
/// on it would take every level behind it with it for as long as the cross stood.
#[test]
fn a_crossed_remainder_does_not_shadow_the_depth_behind_it() {
    let mut ctx = setup();
    let taker = addr(Pubkey::new_unique());
    let maker = addr(Pubkey::new_unique());
    place(&mut ctx, taker_origin_args(Side::Bid, 101, 5), taker);
    place(&mut ctx, place_args(Side::Bid, 98, 7), maker);
    place(&mut ctx, place_args(Side::Ask, 99, 5), maker);
    advance_slot(&mut ctx, 1);

    assert_eq!(quote(&mut ctx, Direction::Short, u64::MAX), vec![(98, 7)]);
    let changes = execute(&mut ctx, Direction::Short, 7);
    assert_eq!(changes, vec![(maker.to_bytes(), 7, 686, vec![2])]);
    // The maker's bid filled; the remainder is still resting.
    let state = market_state(&ctx);
    assert_eq!(state.bid_count, 1);
    assert!(node(&ctx, state.best_bid).is_taker_origin());
}

/// A taker remainder nobody crosses is ordinary depth — quotable and takeable at
/// its own price. That is the fallback when no maker lines up during the auction
/// window, and how the remainder eventually fills if none ever does.
#[test]
fn an_uncrossed_taker_remainder_is_quotable_and_takeable() {
    let mut ctx = setup();
    let taker = addr(Pubkey::new_unique());
    let maker = addr(Pubkey::new_unique());
    place(&mut ctx, taker_origin_args(Side::Bid, 101, 5), taker);
    // Best ask above the bid, so nothing crosses.
    place(&mut ctx, place_args(Side::Ask, 105, 5), maker);
    advance_slot(&mut ctx, 1);

    assert_eq!(quote(&mut ctx, Direction::Short, u64::MAX), vec![(101, 5)]);
    let changes = execute(&mut ctx, Direction::Short, 5);
    assert_eq!(changes, vec![(taker.to_bytes(), 5, 505, vec![1])]);
    assert_eq!(market_state(&ctx).bid_count, 0);
}

/// Two makers crossing is unclaimed arbitrage, not a taker's improvement, and
/// holding either side back over it would cost takers depth for nothing.
#[test]
fn a_maker_only_cross_is_not_gated() {
    let mut ctx = setup();
    let maker_a = addr(Pubkey::new_unique());
    let maker_b = addr(Pubkey::new_unique());
    place(&mut ctx, place_args(Side::Bid, 101, 5), maker_a);
    place(&mut ctx, place_args(Side::Ask, 99, 5), maker_b);
    advance_slot(&mut ctx, 1);

    assert_eq!(execute(&mut ctx, Direction::Short, 5).len(), 1);
    assert_eq!(execute(&mut ctx, Direction::Long, 5).len(), 1);
}

/// The gate turns on with the counterparty's activation slot, not with its
/// placement: an order inside its auction window cannot be matched by anyone, so
/// it puts no improvement within reach — and holding the remainder back from the
/// moment a crossed order was *placed* would cost the book that depth for the
/// whole window, which is exactly when a migrated remainder is resting there.
#[test]
fn an_unactivated_counterparty_does_not_gate_the_fill() {
    let mut ctx = setup();
    let taker = addr(Pubkey::new_unique());
    let maker = addr(Pubkey::new_unique());
    ctx.svm.warp_to_slot(10);

    // The remainder is live now; the maker's crossing ask only at slot 30.
    let ix = place_ix(
        &ctx,
        PlaceOrderArgsV0 {
            activation_delay_slots: Some(0),
            ..taker_origin_args(Side::Bid, 101, 5)
        },
        taker,
    );
    send(&mut ctx, ix).unwrap();
    let ix = place_ix(
        &ctx,
        PlaceOrderArgsV0 {
            activation_delay_slots: Some(20),
            ..place_args(Side::Ask, 99, 5)
        },
        maker,
    );
    send(&mut ctx, ix).unwrap();

    // Slot 10: the ask cannot match, so the remainder is ordinary depth.
    assert_eq!(quote(&mut ctx, Direction::Short, u64::MAX), vec![(101, 5)]);
    assert_eq!(execute(&mut ctx, Direction::Short, 1).len(), 1);

    // Slot 30: the ask is a live counterparty and the remainder drops out of
    // both the quote and the fill.
    ctx.svm.warp_to_slot(29);
    assert_eq!(execute(&mut ctx, Direction::Short, 1).len(), 1);
    ctx.svm.warp_to_slot(30);
    assert!(quote(&mut ctx, Direction::Short, u64::MAX).is_empty());
    assert!(execute(&mut ctx, Direction::Short, 1).is_empty());
}

/// Quote and the gate on-chain: the crossed remainder's level is absent from the
/// quote, the ordinary maker levels on either side of it are published, and the
/// fill delivers exactly what was published. A router allocates from the quote
/// and velocity binds the execute to it, so any disagreement between the two is a
/// reverted transaction for a taker that did nothing wrong.
#[test]
fn quote_and_execute_skip_the_same_order() {
    let mut ctx = setup();
    let taker = addr(Pubkey::new_unique());
    let maker = addr(Pubkey::new_unique());
    let crosser = addr(Pubkey::new_unique());
    place(&mut ctx, place_args(Side::Ask, 99, 5), maker);
    place(&mut ctx, taker_origin_args(Side::Ask, 100, 5), taker);
    place(&mut ctx, place_args(Side::Ask, 102, 5), maker);
    let crossing_bid = place(&mut ctx, place_args(Side::Bid, 101, 5), crosser);
    advance_slot(&mut ctx, 1);

    assert_eq!(
        quote(&mut ctx, Direction::Long, u64::MAX),
        vec![(99, 5), (102, 5)]
    );
    // Execute honours exactly that: 10 base across the two makers, and the
    // remainder still resting between them.
    let changes = execute(&mut ctx, Direction::Long, u64::MAX);
    assert_eq!(changes, vec![(maker.to_bytes(), 10, 1005, vec![1, 3])]);
    let state = market_state(&ctx);
    assert_eq!(state.ask_count, 1);
    assert!(node(&ctx, state.best_ask).is_taker_origin());

    // The crossing bid is ordinary depth the other way — the fill velocity's
    // cross resolution runs.
    assert_eq!(quote(&mut ctx, Direction::Short, u64::MAX), vec![(101, 5)]);

    // With the cross gone the remainder is ordinary depth again, at its own
    // price.
    cancel(&mut ctx, crossing_bid, crosser).unwrap();
    assert_eq!(quote(&mut ctx, Direction::Long, u64::MAX), vec![(100, 5)]);
    assert_eq!(execute(&mut ctx, Direction::Long, 5).len(), 1);
}

/// The gate's cost on quote, which unlike execute walks a whole side: the worst
/// case is an *uncrossed* taker-origin order at the head, so the counterparty
/// lookup actually happens and the walk still runs to the level cap afterwards.
#[test]
fn cu_benchmark_quote_with_a_taker_origin_head() {
    let mut ctx = setup();
    let user = addr(Pubkey::new_unique());
    for i in 0..PER_SIDE as u64 - 1 {
        let ix = place_ix(&ctx, place_args(Side::Bid, 100 + i, 10), user);
        send(&mut ctx, ix).unwrap();
    }
    // Best of the bid side, with an ask far above it so nothing crosses.
    let ix = place_ix(
        &ctx,
        taker_origin_args(Side::Bid, 100 + PER_SIDE as u64, 10),
        user,
    );
    send(&mut ctx, ix).unwrap();
    let ix = place_ix(&ctx, place_args(Side::Ask, 100_000, 10), user);
    send(&mut ctx, ix).unwrap();
    advance_slot(&mut ctx, 1);

    let ix = instruction::QuoteV0 {
        args: QuoteArgsV0 {
            caps: UserCapsV0::EMPTY,
            reference_price: 0,
            direction: Direction::Short,
            size: u64::MAX,
            users: UserSetV0::EMPTY,
            taker: None,
        },
    }
    .to_instruction(accounts::QuoteV0 {
        market: addr(ctx.market),
    });
    let meta = send(&mut ctx, ix).unwrap();
    // 64, not the 128 orders resting: the ladder stops at this market's
    // `max_execute_fills`, because a quote may only promise what one execute
    // can deliver.
    assert_eq!(parse_levels(&read_response(&ctx, &meta)).len(), 64);
    println!(
        "CU — quote(full side, uncrossed taker-origin at head): {}",
        meta.compute_units_consumed
    );
}
