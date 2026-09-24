//! litesvm integration tests. Require the SBF build first:
//! `bun run program:build:clob` (cargo-build-sbf --tools-version v1.54).

use {
    anchor_v2_testing::{
        Keypair, LiteSVM, Message, Signer, VersionedMessage, VersionedTransaction,
    },
    clob::{
        accounts,
        anchor_lang::{
            prelude::Address, solana_program::instruction::Instruction, Discriminator, Event,
        },
        events::{
            ExecuteRecordV0, FillSlimV0, MarketSettingsV0, MarketUpdateRecordV0,
            OrdersCancelRecordV0,
        },
        instruction, relay_spec,
        state::{
            CancelSidesV0, ClobHeaderV0, ClobMarketV0, Direction, MarketConfigV0, OrderBitFlag,
            OrderNodeV0, OrderRefV0, Side, UserCapsV0, UserRefV0, BASE_PRECISION,
            CANCEL_ALL_ORDERS_CEILING, CRANK_ACTIVATION, CRANK_BLOCK_OFFSET, CRANK_CAPACITY,
            CRANK_CONDITIONS, CRANK_CROSS, CRANK_EXPIRY, EXECUTE_FILLS_CEILING,
            EXECUTE_USERS_CEILING, ORDERS_OFFSET, REMOVED_ORDER_BYTES,
            RESERVATION_GRACE_SLOTS_CEILING, ZERO_ADDRESS,
        },
        CancelAllArgsV0, CancelOrderArgsV0, ClobRemovalKindV0, CrankAccountV0,
        CrankConditionsArgsV0, CrankResolverV0, EvictWorstArgsV0, ExecuteArgsV0, NextRemovalArgsV0,
        OrderViewV0, OrdersArgsV0, PlaceOrderArgsV0, ProposeMarketAuthorityArgsV0, QuoteArgsV0,
        RemoveExpiredArgsV0, ResizeMarketArgsV0, UpdateMarketArgsV0,
    },
    litesvm::types::{FailedTransactionMetadata, TransactionMetadata},
    quoter_test_support::{addr, parse_u32, system_program},
    solana_clock::Clock,
    solana_pubkey::Pubkey,
};

const SO_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/clob.so");

/// The harness uses whole base units.
/// Sizes are scaled on input/output; prices and quotes read directly.
const UNIT: u64 = BASE_PRECISION;

/// Arena capacity for test markets (chosen per market at creation).
const CAPACITY: usize = 1024;
const PER_SIDE: usize = CAPACITY / 2;

fn program_id() -> Pubkey {
    "BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU"
        .parse()
        .unwrap()
}

struct Ctx {
    svm: LiteSVM,
    payer: Keypair,
    admin: Keypair,
    place_auth: Keypair,
    /// The market keypair. It signs initialization, which is what binds the
    /// market to whoever created the account.
    market_kp: Keypair,
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
    let market_kp = Keypair::new();
    let market = market_kp.pubkey();
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
            base_precision: BASE_PRECISION,
            order_tick_size: 1,
            order_step_size: 1,
            min_order_size: 1,
            // Off by default, so every test here reads the behaviour of a market
            // whose reserved bytes are still zero.
            blocking_min_size: 0,
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
        market_kp,
        market,
    };

    send(&mut ctx, ix).unwrap();
    ctx
}

/// Sign with payer plus whichever of the known keys the metas mark as signer.
fn send(ctx: &mut Ctx, ix: Instruction) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    send_with_budget(ctx, ix, None)
}

/// `send` with one extra signer the context does not hold — for the cases
/// that check a key is refused rather than missing.
fn send_signed(
    ctx: &mut Ctx,
    ix: Instruction,
    extra: &Keypair,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    ctx.svm.expire_blockhash();
    let blockhash = ctx.svm.latest_blockhash();
    let msg = Message::new_with_blockhash(&[ix], Some(&ctx.payer.pubkey()), &blockhash);
    let signers: Vec<&dyn anchor_v2_testing::Signer> = vec![&ctx.payer, extra];
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &signers).unwrap();
    ctx.svm.send_transaction(tx)
}

/// `send`, optionally preceded by a compute-budget instruction. The default
/// 200k is not enough for the ceiling cases (a full-width execute is a lot of
/// book work), and raising it in the tx is what a real caller would do too.
fn send_with_budget(
    ctx: &mut Ctx,
    ix: Instruction,
    compute_unit_limit: Option<u32>,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    quoter_test_support::send(
        &mut ctx.svm,
        &ctx.payer,
        &[&ctx.admin, &ctx.place_auth, &ctx.market_kp],
        ix,
        compute_unit_limit,
    )
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
    instruction::PlaceOrderV0 { args }.to_instruction(accounts::GatedMarketV0 {
        market: addr(ctx.market),
        place_authority: addr(ctx.place_auth.pubkey()),
    })
}

fn place_args(side: Side, price: u64, size: u64) -> PlaceOrderArgsV0 {
    PlaceOrderArgsV0 {
        side,
        price,
        base_asset_amount: size * UNIT,
        activation_delay_slots: None,
        max_ts: 0,
        user: uref(addr(Pubkey::default())),
        taker_origin: false,
        client_order_id: 0,
        reject_if_crossed: false,
        reduce_only: false,
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
    quoter_test_support::read_response(&ctx.svm, program_id(), ctx.market, meta).bytes
}

/// QuoteResponseV0 { levels: Vec<PriceLevel { price: u64, size: u64 }> }
fn parse_levels(b: &[u8]) -> Vec<(u64, u64)> {
    clob::state::QuoteResponseV0::parse(b)
        .expect("quote response")
        .levels
        .iter()
        .map(|level| (level.price, level.size / UNIT))
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
                change.base_size / UNIT,
                change.quote_size,
                response
                    .completed_for(i)
                    .map(|entry| entry.order_id)
                    .collect(),
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
fn user_set(users: Option<Vec<Address>>) -> Vec<UserRefV0> {
    match users {
        None => Vec::new(),
        Some(users) => users.into_iter().map(uref).collect(),
    }
}

fn quote_meta_users(
    ctx: &mut Ctx,
    direction: Direction,
    size: u64,
    users: Option<Vec<Address>>,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    quote_meta_limited(ctx, direction, size, users, 0)
}

/// A `quote_v0` argument list at the harness defaults: a served window, no
/// caps, no reference price, no user set, no taker and no limit. Each caller
/// overrides the fields its own test is about. `size` is in whole base units.
fn quote_args(direction: Direction, size: u64) -> QuoteArgsV0<'static> {
    QuoteArgsV0 {
        taker_served_window: true,
        include_taker_origin_reservations: false,
        caps: UserCapsV0::EMPTY,
        reference_price: None,
        direction,
        size: size.saturating_mul(UNIT),
        users: &[],
        taker: None,
        limit_price: 0,
    }
}

/// The same defaults for `execute_v0`, which takes no limit price.
fn execute_args(direction: Direction, size: u64) -> ExecuteArgsV0<'static> {
    ExecuteArgsV0 {
        taker_served_window: true,
        include_taker_origin_reservations: false,
        caps: UserCapsV0::EMPTY,
        reference_price: None,
        direction,
        size: size.saturating_mul(UNIT),
        users: &[],
        taker: None,
    }
}

/// [`quote_meta_users`] with a worst-acceptable-price bound; zero is none.
fn quote_meta_limited(
    ctx: &mut Ctx,
    direction: Direction,
    size: u64,
    users: Option<Vec<Address>>,
    limit_price: u64,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    let ix = instruction::QuoteV0 {
        args: QuoteArgsV0 {
            users: &user_set(users),
            limit_price,
            ..quote_args(direction, size)
        },
    }
    .to_instruction(accounts::ResponseMarketV0 {
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

/// A quote that reaches the depth a crossing taker remainder claims: the read
/// the crank that settles the cross makes.
fn quote_consuming(ctx: &mut Ctx, direction: Direction, size: u64) -> Vec<(u64, u64)> {
    let ix = instruction::QuoteV0 {
        args: QuoteArgsV0 {
            include_taker_origin_reservations: true,
            ..quote_args(direction, size)
        },
    }
    .to_instruction(accounts::ResponseMarketV0 {
        market: addr(ctx.market),
    });

    let meta = send(ctx, ix).unwrap();
    parse_levels(&read_response(ctx, &meta))
}

/// The fill half of the same read.
fn execute_consuming(
    ctx: &mut Ctx,
    direction: Direction,
    size: u64,
) -> Vec<([u8; 32], u64, u64, Vec<u64>)> {
    let ix = instruction::ExecuteV0 {
        args: ExecuteArgsV0 {
            include_taker_origin_reservations: true,
            ..execute_args(direction, size)
        },
    }
    .to_instruction(accounts::GatedMarketV0 {
        market: addr(ctx.market),
        place_authority: addr(ctx.place_auth.pubkey()),
    });

    let meta = send(ctx, ix).unwrap();
    parse_balance_changes(&read_response(ctx, &meta))
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
    (response.withheld.price != 0)
        .then_some((response.withheld.price, response.withheld.size / UNIT))
}

fn execute_meta_users(
    ctx: &mut Ctx,
    direction: Direction,
    size: u64,
    users: Option<Vec<Address>>,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    let ix = instruction::ExecuteV0 {
        args: ExecuteArgsV0 {
            users: &user_set(users),
            ..execute_args(direction, size)
        },
    }
    .to_instruction(accounts::GatedMarketV0 {
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
    .to_instruction(accounts::GatedMarketV0 {
        market: addr(ctx.market),
        place_authority: addr(ctx.place_auth.pubkey()),
    });

    send(ctx, ix)
}

/// Ask the book what it would let a caller remove next. Read-only, and what a
/// resolver runs under simulation to find work.
fn next_removal(ctx: &mut Ctx, kind: ClobRemovalKindV0) -> OrderViewV0 {
    let ix = instruction::NextRemovalV0 {
        args: NextRemovalArgsV0 { kind },
    }
    .to_instruction(accounts::MarketViewV0 {
        market: addr(ctx.market),
    });

    let meta = send(ctx, ix).expect("next_removal_v0 runs");
    anchor_lang::wincode::config::deserialize(&meta.return_data.data, anchor_lang::BORSH_CONFIG)
        .expect("decodes as NextRemovalV0")
}

fn remove_expired(
    ctx: &mut Ctx,
    order_ref: OrderRefV0,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    let ix = instruction::RemoveExpiredV0 {
        args: RemoveExpiredArgsV0 { order_ref },
    }
    .to_instruction(accounts::GatedMarketV0 {
        market: addr(ctx.market),
        place_authority: addr(ctx.place_auth.pubkey()),
    });

    send(ctx, ix)
}

/// RemovedOrderV0 return data: (user, order_id, client_order_id, price,
/// base_asset_amount, side, taker_origin).
fn parse_removed(b: &[u8]) -> ([u8; 32], u64, u32, u64, u64, u8, bool) {
    assert_eq!(u16::from_le_bytes(b[32..34].try_into().unwrap()), 0);
    assert_eq!(b.len(), REMOVED_ORDER_BYTES);
    (
        b[..32].try_into().unwrap(),
        parse_u64(&b[34..]),
        parse_u32(&b[42..]),
        parse_u64(&b[46..]),
        parse_u64(&b[54..]) / UNIT,
        b[62],
        match b[63] {
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
                cull.base_asset_amount / UNIT,
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

fn cancel_ix(ctx: &Ctx, order_ref: OrderRefV0, user: Address) -> Instruction {
    instruction::CancelOrderV0 {
        args: CancelOrderArgsV0 {
            order_ref,
            user: uref(user),
            force: false,
        },
    }
    .to_instruction(accounts::GatedMarketV0 {
        market: addr(ctx.market),
        place_authority: addr(ctx.place_auth.pubkey()),
    })
}

fn cancel_all_ix(ctx: &Ctx, user: Address, sides: CancelSidesV0) -> Instruction {
    instruction::CancelAllV0 {
        args: CancelAllArgsV0 {
            user: uref(user),
            sides,
            force: false,
        },
    }
    .to_instruction(accounts::GatedMarketV0 {
        market: addr(ctx.market),
        place_authority: addr(ctx.place_auth.pubkey()),
    })
}

/// `CancelAllOutcomeV0` off return data:
/// `(bid_base, ask_base, bid_orders, ask_orders, exhaustive)`.
fn parse_cancel_all(b: &[u8]) -> (u64, u64, u32, u32, bool) {
    assert_eq!(
        b.len(),
        34 + 8 + 8 + 4 + 4 + 4 + 4 + 1,
        "outcome wire width"
    );

    (
        parse_u64(&b[34..]),
        parse_u64(&b[42..]),
        parse_u32(&b[50..]),
        parse_u32(&b[54..]),
        // Skip the bid/ask reduce-only counts at [58..66]; exhaustive follows.
        b[66] == 1,
    )
}

/// The `OrdersCancelRecordV0` payload: skips the fixed prefix and returns the
/// logged id list.
fn parse_cancel_all_record(bytes: &[u8]) -> (bool, Vec<u32>) {
    assert_eq!(
        &bytes[..8],
        OrdersCancelRecordV0::DISCRIMINATOR,
        "not a cancel-all record"
    );

    // [disc 8][authority 32][ts 8][bid base 8][ask base 8][market 2][sub 2]
    // [sides 1][exhaustive 1][count 4][client ids…]
    const IDS: usize = 8 + 32 + 8 + 8 + 8 + 2 + 2 + 1 + 1 + 4;
    let exhaustive = bytes[IDS - 5] == 1;
    let count = parse_u32(&bytes[IDS - 4..]) as usize;
    let ids = (0..count)
        .map(|i| parse_u32(&bytes[IDS + i * 4..]))
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
        let ix = cancel_ix(ctx, order_ref, user);
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
            order_step_size: Some(5 * UNIT),
            min_order_size: Some(10 * UNIT),
            ..Default::default()
        },
    }
    .to_instruction(accounts::UpdateMarketV0 {
        market: addr(ctx.market),
        authority: addr(ctx.admin.pubkey()),
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
    .to_instruction(accounts::GatedMarketV0 {
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
        size: 5,
        ..execute_args(Direction::Long, 0)
    };
    let execute_ix = |authority: Pubkey| {
        instruction::ExecuteV0 { args: args() }.to_instruction(accounts::GatedMarketV0 {
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

    assert!(!next_removal(&mut ctx, ClobRemovalKindV0::Evictable).found());

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

    // Past the threshold the book names the tail itself, and the side: a
    // caller never reads the threshold or the counts to pick either.
    let work = next_removal(&mut ctx, ClobRemovalKindV0::Evictable);
    assert!(work.found());
    assert_eq!(work.side, Side::Bid);
    assert_eq!(node(&ctx, work.order_ref.node_index).price, 100);

    // The crank removes the tail (worst price) and reports it for velocity.
    let meta = evict_worst(&mut ctx, Side::Bid).unwrap();
    let (evicted_user, _, _, price, base, side, taker_origin) =
        parse_removed(&meta.return_data.data);
    assert_eq!(
        (evicted_user, price, base, side, taker_origin),
        (user.to_bytes(), 100, 1, Side::Bid.tag(), false)
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

    // With only the bid side over the threshold, that is the side the book
    // names.
    assert_eq!(
        next_removal(&mut ctx, ClobRemovalKindV0::Evictable).side,
        Side::Bid
    );

    // Both sides over the threshold: the book relieves the fuller one.
    for i in 0..8u64 {
        place(&mut ctx, place_args(Side::Ask, 300 + i, 1), user);
    }

    evict_worst(&mut ctx, Side::Bid).unwrap();
    let work = next_removal(&mut ctx, ClobRemovalKindV0::Evictable);
    assert_eq!(work.side, Side::Ask);
    assert_eq!(node(&ctx, work.order_ref.node_index).price, 307);
}

/// A resolver stages the eviction `next_removal_v0` names, and velocity checks
/// the removal against the maker it loaded. The two must pass over a bound
/// remainder the same way, or every such crank fails.
#[test]
fn next_removal_and_eviction_agree_over_a_bound_remainder() {
    let mut ctx = setup_with_capacity(16); // 8 per side, evict threshold 6
    let (maker, taker) = (addr(Pubkey::new_unique()), addr(Pubkey::new_unique()));

    for i in 1..=5u64 {
        place(&mut ctx, place_args(Side::Bid, 100 + i, 1), maker);
    }

    let remainder = place(&mut ctx, taker_origin_args(Side::Bid, 100, 1), taker);
    assert_eq!(market_state(&ctx).worst_bid, remainder.node_index);

    let work = next_removal(&mut ctx, ClobRemovalKindV0::Evictable);
    assert_eq!(node(&ctx, work.order_ref.node_index).price, 101);
    let meta = evict_worst(&mut ctx, Side::Bid).unwrap();
    let (evicted_user, order_id, ..) = parse_removed(&meta.return_data.data);
    assert_eq!(
        (evicted_user, order_id),
        (maker.to_bytes(), work.order_ref.order_id)
    );

    // Past the one-slot delay and the claim, both name the remainder.
    place(&mut ctx, place_args(Side::Bid, 101, 1), maker);
    advance_slot(
        &mut ctx,
        1 + clob::state::DEFAULT_RESERVATION_GRACE_SLOTS as u64,
    );
    let work = next_removal(&mut ctx, ClobRemovalKindV0::Evictable);
    assert_eq!(work.order_ref, remainder);
    let meta = evict_worst(&mut ctx, Side::Bid).unwrap();
    assert!(parse_removed(&meta.return_data.data).6);
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

/// Register a resolver for every one of the book's own conditions, the way
/// the program that owns the book's flow does at attach.
fn set_crank_conditions(ctx: &mut Ctx, resolver: Pubkey) -> u32 {
    let entry = |disc: u8, min_payment: u64| CrankResolverV0 {
        program: resolver.to_bytes(),
        disc: [disc; 8],
        min_payment,
    };
    let ix = instruction::SetCrankConditionsV0 {
        args: CrankConditionsArgsV0 {
            expiry: entry(1, 1_000),
            activation: entry(2, 2_000),
            capacity: entry(3, 3_000),
            cross: entry(4, 4_000),
            accounts: vec![
                CrankAccountV0 {
                    address: ctx.market.to_bytes(),
                    writable: true,
                },
                CrankAccountV0 {
                    address: resolver.to_bytes(),
                    writable: false,
                },
            ],
        },
    }
    .to_instruction(accounts::GatedMarketV0 {
        market: addr(ctx.market),
        place_authority: addr(ctx.place_auth.pubkey()),
    });

    let meta = send(ctx, ix).expect("set_crank_conditions_v0 runs");
    u32::from_le_bytes(meta.return_data.data[..4].try_into().unwrap())
}

/// The block on the market account, read the way a turner does: by account
/// and offset, with no knowledge of what else the account holds.
fn crank_block(ctx: &Ctx, offset: u32) -> Vec<relay_spec::ConditionV0> {
    let account = ctx.svm.get_account(&ctx.market).unwrap();
    let (header, conditions) = relay_spec::read_block(&account.data[offset as usize..], 0).unwrap();
    assert_eq!(header.num_conditions as usize, CRANK_CONDITIONS);
    conditions.to_vec()
}

/// The book keeps the wakes that describe its own state, and the program that
/// owns its flow says who answers them.
///
/// Neither half is the other's: a caller cannot maintain a hint it would have
/// to read the arena to compute, and the book cannot resolve work whose
/// consequences it does not hold.
#[test]
fn the_book_hosts_the_conditions_that_watch_its_own_state() {
    let mut ctx = setup();
    let user = addr(Pubkey::new_unique());
    let resolver = Pubkey::new_unique();
    set_unix_timestamp(&mut ctx, 1_000);
    ctx.svm.warp_to_slot(50);

    // Before registration the block is stamped but every slot is inactive: a
    // book nobody cranks wakes nobody.
    let offset = CRANK_BLOCK_OFFSET as u32;
    assert!(crank_block(&ctx, offset).iter().all(|c| !c.is_active()));

    let reported = set_crank_conditions(&mut ctx, resolver);
    assert_eq!(reported, offset, "the block reports where it sits");

    // An empty book has no expiry and no pending activation, so both wakes are
    // set to never rather than left at zero — a zero would be permanently due.
    let conditions = crank_block(&ctx, offset);
    assert!(conditions.iter().all(|c| c.is_active()));
    assert_eq!(
        conditions[CRANK_EXPIRY].wake(),
        Ok(relay_spec::WakeView::AtTimestamp { unix_ts: i64::MAX })
    );
    assert_eq!(
        conditions[CRANK_ACTIVATION].wake(),
        Ok(relay_spec::WakeView::AtSlot { slot: u64::MAX })
    );
    assert_eq!(conditions[CRANK_EXPIRY].min_payment(), 1_000);
    assert_eq!(conditions[CRANK_CROSS].min_payment(), 4_000);

    // The two watches point at this account's own fields, so a caller never
    // has to know where the counts or the side heads sit.
    let market = ctx.market.to_bytes();
    assert_eq!(
        conditions[CRANK_CAPACITY].wake(),
        Ok(relay_spec::WakeView::OnAccountChange {
            address: market,
            offset: clob::state::SIDE_COUNTS_OFFSET as u32,
            len: 8,
        })
    );
    assert_eq!(
        conditions[CRANK_CROSS].wake(),
        Ok(relay_spec::WakeView::OnAccountChange {
            address: market,
            offset: clob::state::TOP_OF_BOOK_OFFSET as u32,
            len: 8,
        })
    );

    // A placement moves both wakes, in the same instruction that changes what
    // they describe — nothing else is passed and nothing else is called.
    let order_ref = place(
        &mut ctx,
        PlaceOrderArgsV0 {
            max_ts: 1_020,
            activation_delay_slots: Some(10),
            ..place_args(Side::Ask, 100, 5)
        },
        user,
    );
    let conditions = crank_block(&ctx, offset);
    assert_eq!(
        conditions[CRANK_EXPIRY].wake(),
        Ok(relay_spec::WakeView::AtTimestamp { unix_ts: 1_020 })
    );
    assert_eq!(
        conditions[CRANK_ACTIVATION].wake(),
        Ok(relay_spec::WakeView::AtSlot { slot: 60 })
    );

    // And removing the order that held them puts both back to never.
    set_unix_timestamp(&mut ctx, 1_021);
    remove_expired(&mut ctx, order_ref).unwrap();
    let conditions = crank_block(&ctx, offset);
    assert_eq!(
        conditions[CRANK_EXPIRY].wake(),
        Ok(relay_spec::WakeView::AtTimestamp { unix_ts: i64::MAX })
    );

    // A resolver registered with a zeroed program is a condition the caller
    // does not want; the slot goes inactive rather than waking into nothing.
    let ix = instruction::SetCrankConditionsV0 {
        args: CrankConditionsArgsV0 {
            expiry: CrankResolverV0 {
                program: [0u8; 32],
                disc: [0u8; 8],
                min_payment: 0,
            },

            activation: CrankResolverV0 {
                program: resolver.to_bytes(),
                disc: [2; 8],
                min_payment: 2_000,
            },

            capacity: CrankResolverV0 {
                program: resolver.to_bytes(),
                disc: [3; 8],
                min_payment: 3_000,
            },

            cross: CrankResolverV0 {
                program: resolver.to_bytes(),
                disc: [4; 8],
                min_payment: 4_000,
            },

            accounts: vec![CrankAccountV0 {
                address: ctx.market.to_bytes(),
                writable: true,
            }],
        },
    }
    .to_instruction(accounts::GatedMarketV0 {
        market: addr(ctx.market),
        place_authority: addr(ctx.place_auth.pubkey()),
    });

    send(&mut ctx, ix).unwrap();
    let conditions = crank_block(&ctx, offset);
    assert!(!conditions[CRANK_EXPIRY].is_active());
    assert!(conditions[CRANK_CROSS].is_active());
}

/// Only the key the book's flow already belongs to may say who cranks it.
#[test]
fn registering_a_resolver_is_place_authority_only() {
    let mut ctx = setup();
    let stranger = Keypair::new();
    ctx.svm.airdrop(&stranger.pubkey(), 1_000_000_000).unwrap();
    let ix = instruction::SetCrankConditionsV0 {
        args: CrankConditionsArgsV0 {
            expiry: CrankResolverV0 {
                program: Pubkey::new_unique().to_bytes(),
                disc: [1; 8],
                min_payment: 1,
            },

            activation: CrankResolverV0 {
                program: [0u8; 32],
                disc: [0u8; 8],
                min_payment: 0,
            },

            capacity: CrankResolverV0 {
                program: [0u8; 32],
                disc: [0u8; 8],
                min_payment: 0,
            },

            cross: CrankResolverV0 {
                program: [0u8; 32],
                disc: [0u8; 8],
                min_payment: 0,
            },

            accounts: vec![],
        },
    }
    .to_instruction(accounts::GatedMarketV0 {
        market: addr(ctx.market),
        place_authority: addr(stranger.pubkey()),
    });

    assert_clob_err(
        send_signed(&mut ctx, ix, &stranger),
        err_code(clob::error::ClobError::InvalidAuthority),
    );
}

/// Ask the book about a set of refs, the way a caller that holds refs but not
/// the arena does.
fn orders(ctx: &mut Ctx, refs: &[OrderRefV0]) -> Vec<OrderViewV0> {
    let ix = instruction::OrdersV0 {
        args: OrdersArgsV0 {
            refs: refs.to_vec(),
        },
    }
    .to_instruction(accounts::MarketViewV0 {
        market: addr(ctx.market),
    });

    let meta = send(ctx, ix).expect("orders_v0 runs");
    let answer: clob::OrdersV0 = anchor_lang::wincode::config::deserialize(
        &meta.return_data.data,
        anchor_lang::BORSH_CONFIG,
    )
    .expect("decodes as OrdersV0");
    answer.orders
}

/// A caller holding refs asks the book what they still name, instead of
/// decoding its arena.
///
/// The answers are positional: a ref whose order is gone comes back empty in
/// its own slot, so the caller reads them straight against its own list.
#[test]
fn the_book_describes_the_orders_a_caller_holds_refs_for() {
    let mut ctx = setup();
    let user = addr(Pubkey::new_unique());
    let other = addr(Pubkey::new_unique());
    set_unix_timestamp(&mut ctx, 1_000);

    let bid = place(
        &mut ctx,
        PlaceOrderArgsV0 {
            max_ts: 1_500,
            ..place_args(Side::Bid, 100, 7)
        },
        user,
    );
    let ask = place(&mut ctx, place_args(Side::Ask, 300, 4), other);
    // Never placed: node 0 holds the bid, so this id names nothing.
    let ghost = OrderRefV0 {
        node_index: bid.node_index,
        order_id: 999,
    };

    let views = orders(&mut ctx, &[bid, ghost, ask]);
    assert_eq!(views.len(), 3);

    assert!(views[0].found());
    assert_eq!(views[0].order_ref, bid);
    assert_eq!(views[0].user.authority, user);
    assert_eq!(views[0].side, Side::Bid);
    assert_eq!(views[0].price, 100);
    assert_eq!(views[0].base_asset_amount, 7 * UNIT);
    assert_eq!(views[0].max_ts, 1_500);
    assert!(!views[0].taker_origin);

    // A live node holding a different order is not this caller's order. Ids
    // are never reused, so the id alone settles it.
    assert!(!views[1].found());

    assert!(views[2].found());
    assert_eq!(views[2].user.authority, other);
    assert_eq!(views[2].side, Side::Ask);
    assert_eq!(views[2].max_ts, 0, "good-till-cancelled");

    // Cancel the bid: its ref stops naming an order, and the ask is
    // unaffected.
    cancel(&mut ctx, bid, user).unwrap();
    let views = orders(&mut ctx, &[bid, ask]);
    assert!(!views[0].found());
    assert!(views[1].found());

    // More refs than the book answers about in one call is a caller error,
    // not a truncated answer.
    let ix = instruction::OrdersV0 {
        args: OrdersArgsV0 {
            refs: vec![ask; clob::ORDER_VIEW_CEILING + 1],
        },
    }
    .to_instruction(accounts::MarketViewV0 {
        market: addr(ctx.market),
    });

    assert_clob_err(
        send(&mut ctx, ix),
        err_code(clob::error::ClobError::InvalidConfig),
    );
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

    // And the book says it has no expiry work either.
    assert!(!next_removal(&mut ctx, ClobRemovalKindV0::Expired).found());

    set_unix_timestamp(&mut ctx, 1_021);
    // Now the book names the order to remove, so a caller never has to read
    // the arena to find it.
    let work = next_removal(&mut ctx, ClobRemovalKindV0::Expired);
    assert!(work.found());
    assert_eq!(work.order_ref, order_ref);
    assert_eq!(work.user.authority, user);
    assert_eq!(work.side, Side::Ask);

    // Skipped by quote/execute but NOT removed — reclamation goes through
    // velocity (remove_expired) so the maker's aggregates update.
    assert!(quote(&mut ctx, Direction::Long, 5).is_empty());
    assert!(execute(&mut ctx, Direction::Long, 5).is_empty());
    let state = market_state(&ctx);
    assert_eq!(state.ask_count, 1);
    assert_eq!(state.free_count, CAPACITY as u32 - 1);

    let meta = remove_expired(&mut ctx, order_ref).unwrap();
    let (removed_user, _, _, _, base, side, taker_origin) = parse_removed(&meta.return_data.data);
    assert_eq!(
        (removed_user, base, side, taker_origin),
        (user.to_bytes(), 5, Side::Ask.tag(), false)
    );

    let state = market_state(&ctx);
    assert_eq!(state.ask_count, 0);
    assert_eq!(state.free_count, CAPACITY as u32);
    assert!(!next_removal(&mut ctx, ClobRemovalKindV0::Expired).found());
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

/// Set the blocking floor on the test market.
fn set_blocking_min_size(ctx: &mut Ctx, size: u64) {
    let ix = instruction::UpdateMarketV0 {
        args: UpdateMarketArgsV0 {
            blocking_min_size: Some(size * UNIT),
            ..Default::default()
        },
    }
    .to_instruction(accounts::UpdateMarketV0 {
        market: addr(ctx.market),
        authority: addr(ctx.admin.pubkey()),
    });

    send(ctx, ix).unwrap();
}

/// Ending a walk is a right, and the floor is what it costs.
///
/// Without one the price is `min_order_size`: a caller carries at most
/// `max_execute_users`, so one more order than that, each on a fresh
/// sub-account at the top of book, puts the depth behind them out of reach of
/// every caller for the price of rent. Above the floor a maker keeps price
/// priority against a caller that left it out; below it the maker relies on
/// being carried, and a carried maker fills either way.
#[test]
fn an_order_under_the_blocking_floor_is_stepped_over_at_any_age() {
    let mut ctx = setup();
    set_blocking_min_size(&mut ctx, 10);

    let dust = addr(Pubkey::new_unique());
    let blocker = addr(Pubkey::new_unique());
    let carried = addr(Pubkey::new_unique());
    ctx.svm.warp_to_slot(10);
    // Best price, under the floor, owner not carried.
    place(&mut ctx, place_args(Side::Ask, 100, 5), dust);
    // Over the floor, owner not carried: this is the one that may end a walk.
    place(&mut ctx, place_args(Side::Ask, 101, 20), blocker);
    place(&mut ctx, place_args(Side::Ask, 102, 9), carried);
    // Far past the grace window, so age is not what decides this.
    ctx.svm.warp_to_slot(10_000);

    // The dust order is stepped over. The walk then reaches the order over the
    // floor, which still ends it, so the carried maker behind goes untraded.
    assert_eq!(
        quote_users(&mut ctx, Direction::Long, 40, Some(vec![carried])),
        vec![]
    );
    assert_eq!(
        quote_withheld(&mut ctx, Direction::Long, 40, Some(vec![carried])),
        Some((101, 20)),
        "the order over the floor is the one that ends the walk"
    );

    // Carrying the blocking maker too: only the dust is skipped now, and
    // everything behind it trades.
    assert_eq!(
        quote_users(&mut ctx, Direction::Long, 40, Some(vec![carried, blocker])),
        vec![(101, 20), (102, 9)]
    );
    assert_eq!(
        quote_withheld(&mut ctx, Direction::Long, 40, Some(vec![carried, blocker])),
        None,
        "nothing over the floor was left out"
    );

    // A carried maker under the floor fills normally. The floor decides who may
    // end a walk, never who may fill.
    assert_eq!(
        quote_users(
            &mut ctx,
            Direction::Long,
            40,
            Some(vec![dust, blocker, carried])
        ),
        vec![(100, 5), (101, 20), (102, 9)]
    );
}

/// The L3 read says which orders can end a walk, so a caller assembling an
/// account set never applies the floor itself.
#[test]
fn the_l3_read_flags_the_orders_that_can_end_a_walk() {
    use clob::state::{L3ArgsV0, L3ResponseV0};

    let mut ctx = setup();
    set_blocking_min_size(&mut ctx, 10);
    let maker = addr(Pubkey::new_unique());
    ctx.svm.warp_to_slot(10);
    place(&mut ctx, place_args(Side::Ask, 100, 5), maker);
    place(&mut ctx, place_args(Side::Ask, 101, 20), maker);
    ctx.svm.warp_to_slot(20);

    let ix = instruction::QuoteL3V0 {
        args: L3ArgsV0 {
            direction: Direction::Long,
            size: 0,
            max_rows: 8,
            include_taker_origin_reservations: false,
        },
    }
    .to_instruction(accounts::ResponseMarketV0 {
        market: addr(ctx.market),
    });

    let meta = send(&mut ctx, ix).unwrap();
    let bytes = read_response(&mut ctx, &meta);
    let response = L3ResponseV0::parse(&bytes).expect("l3 response");

    let gates: Vec<bool> = response
        .rows
        .iter()
        .map(|row| row.flags & quoter_spec::L3_ROW_FLAG_BLOCKS_WALK != 0)
        .collect();
    assert_eq!(
        gates,
        vec![false, true],
        "only the order at or over the floor can end a walk"
    );
}

/// Zero is what a market reads out of reserved bytes, so an untouched market
/// keeps the behaviour it had before the floor existed.
#[test]
fn a_zero_blocking_floor_lets_any_order_end_a_walk() {
    let mut ctx = setup();
    let dust = addr(Pubkey::new_unique());
    ctx.svm.warp_to_slot(10);
    place(&mut ctx, place_args(Side::Ask, 100, 1), dust);
    ctx.svm.warp_to_slot(10_000);

    assert_eq!(
        quote_withheld(
            &mut ctx,
            Direction::Long,
            30,
            Some(vec![addr(Pubkey::new_unique())])
        ),
        Some((100, 1)),
        "a one-lot order ends the walk when no floor is set"
    );
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
            min_order_size: Some(10 * UNIT),
            ..Default::default()
        },
    }
    .to_instruction(accounts::UpdateMarketV0 {
        market: addr(ctx.market),
        authority: addr(ctx.admin.pubkey()),
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
    // Distinct client ids: the record lists the caller's ids, so an assertion
    // against them is what pins that the book reported those and not its own.
    let mut client_order_id = 0u32;
    let mut next_id = || {
        client_order_id += 1;
        client_order_id
    };

    for i in 0..per_side {
        let id = next_id();
        expected.push(id);
        place(
            &mut ctx,
            PlaceOrderArgsV0 {
                client_order_id: id,
                ..place_args(Side::Bid, 1_000 - i, 10)
            },
            user,
        );
        place(&mut ctx, place_args(Side::Bid, 1_000 - i, 10), other);
    }
    for i in 0..per_side {
        let id = next_id();
        expected.push(id);
        place(
            &mut ctx,
            PlaceOrderArgsV0 {
                client_order_id: id,
                ..place_args(Side::Ask, 2_000 + i, 10)
            },
            user,
        );
        place(&mut ctx, place_args(Side::Ask, 2_000 + i, 10), other);
    }

    let ix = cancel_all_ix(&ctx, user, CancelSidesV0::Both);
    let meta = send_with_budget(&mut ctx, ix, Some(400_000)).unwrap();
    let (bid_base, ask_base, bid_orders, ask_orders, exhaustive) =
        parse_cancel_all(&meta.return_data.data);
    assert_eq!(bid_orders as u64, per_side);
    assert_eq!(ask_orders as u64, per_side);
    assert_eq!(bid_base, per_side * 10 * UNIT);
    assert_eq!(ask_base, per_side * 10 * UNIT);
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
            size: u64::MAX,
            ..quote_args(Direction::Short, 0)
        },
    }
    .to_instruction(accounts::ResponseMarketV0 {
        market: addr(ctx.market),
    });

    let quote_meta = send(&mut ctx, ix).unwrap();

    // Execute across 50 orders (one user, so the response stays small).
    let execute_meta = execute_meta(&mut ctx, Direction::Short, 500).unwrap();

    // Cancel: place a fresh order and remove it (one order per ix).
    let mut ctx2 = setup();
    let u2 = addr(Pubkey::new_unique());
    let oref = place(&mut ctx2, place_args(Side::Bid, 500, 10), u2);
    let ix = cancel_ix(&ctx2, oref, u2);
    let cancel_empty = send(&mut ctx2, ix).unwrap().compute_units_consumed;

    // Cancel out of a nearly-full side (relink cost at depth).
    let mut refs = Vec::new();
    for i in 0..PER_SIDE as u64 - 1 {
        refs.push(place(&mut ctx2, place_args(Side::Bid, 1_000 + i, 10), u2));
    }

    let mid_ref = refs[refs.len() / 2];
    let ix = cancel_ix(&ctx2, mid_ref, u2);
    let cancel_full = send(&mut ctx2, ix).unwrap().compute_units_consumed;

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
            let ix = cancel_ix(&ctx, *order_ref, mine);
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

    // With no user set, the quote counts the eight makers by ref.
    let ix = instruction::QuoteV0 {
        args: quote_args(Direction::Long, 64),
    }
    .to_instruction(accounts::ResponseMarketV0 {
        market: addr(ctx.market),
    });
    let quote_meta = send(&mut ctx, ix).unwrap();

    let meta = execute_meta_users(&mut ctx, Direction::Long, 64, Some(makers.clone())).unwrap();
    let changes = parse_balance_changes(&read_response(&ctx, &meta));
    assert_eq!(changes.len(), 8);
    // Every maker's eight orders are fully consumed and reported.
    assert!(changes.iter().all(|change| change.3.len() == 8));
    println!(
        "CU — quote(64 orders, 8 interleaved makers, no set): {}, \
         execute(64 orders, 8 interleaved makers): {}",
        quote_meta.compute_units_consumed, meta.compute_units_consumed
    );
}

/// The widest event and response a market can produce, on-chain: fills and
/// users both configured at their ceilings, and every order fully consumed.
/// The makers rest several orders each, so both ceilings bind in one execute.
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
            max_execute_users: Some(EXECUTE_USERS_CEILING),
            ..Default::default()
        },
    }
    .to_instruction(accounts::UpdateMarketV0 {
        market: addr(ctx.market),
        authority: addr(ctx.admin.pubkey()),
    });

    send(&mut ctx, ix).unwrap();

    let users = EXECUTE_USERS_CEILING as usize;
    let makers: Vec<_> = (0..users).map(|_| addr(Pubkey::new_unique())).collect();
    let orders: Vec<OrderRefV0> = (0..fills)
        .map(|i| {
            place(
                &mut ctx,
                PlaceOrderArgsV0 {
                    client_order_id: 1_000 + i as u32,
                    ..place_args(Side::Ask, 100 + i as u64, 1)
                },
                makers[i % users],
            )
        })
        .collect();
    advance_slot(&mut ctx, 1);

    let ix = instruction::ExecuteV0 {
        args: execute_args(Direction::Long, fills as u64),
    }
    .to_instruction(accounts::GatedMarketV0 {
        market: addr(ctx.market),
        place_authority: addr(ctx.place_auth.pubkey()),
    });

    let meta = send_with_budget(&mut ctx, ix, Some(1_400_000)).unwrap();
    let response = read_response(&ctx, &meta);
    let changes = parse_balance_changes(&response);
    assert_eq!(changes.len(), users);
    assert_eq!(
        changes.iter().map(|change| change.3.len()).sum::<usize>(),
        fills
    );

    let clock: Clock = ctx.svm.get_sysvar();
    let expected = ExecuteRecordV0 {
        ts: clock.unix_timestamp,
        slot: clock.slot,
        market_index: 0,
        direction: Direction::Long.tag(),
        fills: orders
            .iter()
            .enumerate()
            .map(|(i, order)| FillSlimV0 {
                order_id: order.order_id,
                base_size: UNIT,
                client_order_id: 1_000 + i as u32,
            })
            .collect(),
        cancelled_client_order_ids: vec![],
    };

    assert_eq!(program_data(&meta), Event::data(&expected));
    println!(
        "CU — execute({fills} fills, {users} makers, at the ceilings): {}, response: {} bytes",
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
            taker: Some(uref(taker)),
            ..quote_args(direction, size)
        },
    }
    .to_instruction(accounts::ResponseMarketV0 {
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
            taker: Some(uref(taker)),
            ..execute_args(direction, size)
        },
    }
    .to_instruction(accounts::GatedMarketV0 {
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
    let ix = cancel_ix(ctx, order_ref, user);
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

    let remainder = place(
        &mut ctx,
        PlaceOrderArgsV0 {
            client_order_id: 7_777,
            ..taker_origin_args(Side::Bid, 101, 5)
        },
        user,
    );
    let ordinary = place(&mut ctx, place_args(Side::Bid, 90, 5), user);
    assert!(node(&ctx, remainder.node_index).is_taker_origin());
    assert!(
        node(&ctx, remainder.node_index).is_bit_flag_set(OrderBitFlag::Open),
        "the marker must not displace the liveness bit"
    );
    assert!(!node(&ctx, ordinary.node_index).is_taker_origin());

    // Past the remainder's one-slot delay and its claim, so this is an ordinary
    // cancel rather than the bound refusal. What is under test is the flag on the
    // removal, not the bind.
    advance_slot(
        &mut ctx,
        1 + clob::state::DEFAULT_RESERVATION_GRACE_SLOTS as u64,
    );
    let meta = cancel(&mut ctx, remainder, user).unwrap();
    let (_, order_id, client_order_id, price, base, side, taker_origin) =
        parse_removed(&meta.return_data.data);
    assert_eq!(
        (order_id, client_order_id, price, base, side, taker_origin),
        (remainder.order_id, 7_777, 101, 5, Side::Bid.tag(), true)
    );

    let meta = cancel(&mut ctx, ordinary, user).unwrap();
    assert!(!parse_removed(&meta.return_data.data).6);
}

/// The reservation on-chain, in both directions, and velocity's whole
/// cross-resolution path with it: a taker remainder at 101 with a maker ask at
/// 99 against it is not bought at 101 by whoever lands first, the 99 it crosses
/// is claimed so nobody else can buy that improvement either, and the crank
/// that owes the taker the improvement reaches the ask at its own 99 and then
/// lifts the remainder off by cancel, which says it was the aggressor.
#[test]
fn a_crossed_taker_remainder_and_the_depth_it_crosses_are_both_withheld() {
    let mut ctx = setup();
    let taker = addr(Pubkey::new_unique());
    let maker = addr(Pubkey::new_unique());
    let remainder = place(&mut ctx, taker_origin_args(Side::Bid, 101, 5), taker);
    place(&mut ctx, place_args(Side::Ask, 99, 5), maker);
    advance_slot(&mut ctx, 1);

    // Nothing else rests on either side, so a taker going either way finds no
    // depth — the call lands and fills nothing, and the book is untouched.
    assert!(quote(&mut ctx, Direction::Short, u64::MAX).is_empty());
    assert!(execute(&mut ctx, Direction::Short, 5).is_empty());
    assert!(quote(&mut ctx, Direction::Long, u64::MAX).is_empty());
    assert!(execute(&mut ctx, Direction::Long, 5).is_empty());
    let state = market_state(&ctx);
    assert_eq!((state.bid_count, state.ask_count), (1, 1));

    // The crank reaches the ask at its own 99, which is the price the pair
    // settles at and the improvement the remainder came for.
    assert_eq!(
        quote_consuming(&mut ctx, Direction::Long, u64::MAX),
        vec![(99, 5)]
    );

    let changes = execute_consuming(&mut ctx, Direction::Long, 5);
    assert_eq!(changes, vec![(maker.to_bytes(), 5, 495, vec![2])]);

    // Then the remainder comes off once its claim lapses, reporting which side
    // was the aggressor.
    advance_slot(
        &mut ctx,
        clob::state::DEFAULT_RESERVATION_GRACE_SLOTS as u64,
    );
    let meta = cancel(&mut ctx, remainder, taker).unwrap();
    let (_, _, _, price, base, side, taker_origin) = parse_removed(&meta.return_data.data);
    assert_eq!(
        (price, base, side, taker_origin),
        (101, 5, Side::Bid.tag(), true)
    );
    assert_eq!(market_state(&ctx).bid_count, 0);
}

/// The grace window is market config, because 32 slots is a guess and the
/// only lever a live market has over how long a stalled crank holds claimed
/// depth. The ceiling refuses a window that outlives the transaction it
/// covers.
#[test]
fn the_reservation_grace_window_is_settable_and_bounded() {
    let mut ctx = setup();
    let set_grace = |ctx: &mut Ctx, slots: u16| {
        let ix = instruction::UpdateMarketV0 {
            args: UpdateMarketArgsV0 {
                reservation_grace_slots: Some(slots),
                ..Default::default()
            },
        }
        .to_instruction(accounts::UpdateMarketV0 {
            market: addr(ctx.market),
            authority: addr(ctx.admin.pubkey()),
        });

        send(ctx, ix)
    };

    assert_clob_err(
        set_grace(&mut ctx, RESERVATION_GRACE_SLOTS_CEILING + 1),
        err_code(clob::error::ClobError::InvalidConfig),
    );

    set_grace(&mut ctx, RESERVATION_GRACE_SLOTS_CEILING).unwrap();
    assert_eq!(
        market_state(&ctx).reservation_grace_slots,
        RESERVATION_GRACE_SLOTS_CEILING
    );

    // Zero is the tightest setting, and it reaches the book: a claim ends the
    // slot its remainder activates, so the 99 the remainder crosses is
    // ordinary depth to any taker from then on.
    set_grace(&mut ctx, 0).unwrap();
    let taker = addr(Pubkey::new_unique());
    let maker = addr(Pubkey::new_unique());
    place(&mut ctx, taker_origin_args(Side::Bid, 101, 5), taker);
    place(&mut ctx, place_args(Side::Ask, 99, 5), maker);
    advance_slot(&mut ctx, 1);
    assert_eq!(quote(&mut ctx, Direction::Long, u64::MAX), vec![(99, 5)]);
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

    // The bid the remainder crosses is claimed, so it is not ordinary depth
    // the other way either. Only the crank settling the cross reaches it.
    assert!(quote(&mut ctx, Direction::Short, u64::MAX).is_empty());
    assert_eq!(
        quote_consuming(&mut ctx, Direction::Short, u64::MAX),
        vec![(101, 5)]
    );

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
            size: u64::MAX,
            ..quote_args(Direction::Short, 0)
        },
    }
    .to_instruction(accounts::ResponseMarketV0 {
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

/// What the crossing reservation costs the two instructions that spend real
/// compute on it: a side walked to its fill cap with claimants resting against
/// it, against the same walk with nothing claimed.
///
/// The fused walk is where this design spends. It reads one node per claimant
/// for a whole walk of the side, so the number to watch is the step from no
/// claimants to several, not the step from one order to many.
#[test]
fn cu_benchmark_quote_and_execute_with_claimants_resting() {
    // Claimants on the ask side, each claiming one bid order whole.
    const CLAIMANTS: u64 = 8;
    const SIZE: u64 = 10;

    let bench = |claimants: u64| -> (u64, u64) {
        let mut ctx = setup();
        let maker = addr(Pubkey::new_unique());
        let taker = addr(Pubkey::new_unique());
        // A full bid side of distinct levels, so the walk runs to the fill cap.
        for i in 0..PER_SIDE as u64 - 1 {
            let ix = place_ix(&ctx, place_args(Side::Bid, 1_000 - i, SIZE), maker);
            send(&mut ctx, ix).unwrap();
        }

        // Remainders that cross the whole bid side, so every claim is tested
        // and honoured rather than skipped on price.
        for _ in 0..claimants {
            let ix = place_ix(&ctx, taker_origin_args(Side::Ask, 1, SIZE), taker);
            send(&mut ctx, ix).unwrap();
        }

        advance_slot(&mut ctx, 1);

        let ix = instruction::QuoteV0 {
            args: QuoteArgsV0 {
                size: u64::MAX,
                ..quote_args(Direction::Short, 0)
            },
        }
        .to_instruction(accounts::ResponseMarketV0 {
            market: addr(ctx.market),
        });

        let quote_meta = send(&mut ctx, ix).unwrap();
        // The claimed prefix is missing from the ladder, and the ladder still
        // runs to this market's `max_execute_fills`.
        let levels = parse_levels(&read_response(&ctx, &quote_meta));
        assert_eq!(levels.len(), 64);
        assert_eq!(levels[0].0, 1_000 - claimants);

        let execute_meta = execute_meta(&mut ctx, Direction::Short, 64 * SIZE).unwrap();
        (
            quote_meta.compute_units_consumed,
            execute_meta.compute_units_consumed,
        )
    };

    let (quote_bare, execute_bare) = bench(0);
    let (quote_claimed, execute_claimed) = bench(CLAIMANTS);
    println!(
        "CU — quote(full side): {quote_bare} bare, {quote_claimed} with {CLAIMANTS} claimants          ({:+}); execute(64 fills): {execute_bare} bare, {execute_claimed} with {CLAIMANTS}          claimants ({:+})",
        quote_claimed as i64 - quote_bare as i64,
        execute_claimed as i64 - execute_bare as i64,
    );
}

/// One row per resting order, with the user each stands on — the answer a
/// caller needs to know whose accounts its fill must carry, and the reason it
/// never has to decode this account from outside.
#[test]
fn quote_l3_reports_the_orders_behind_the_ladder() {
    use clob::state::{L3ArgsV0, L3ResponseV0, L3_ROWS_CEILING};

    let mut ctx = setup();
    let user_a = addr(Pubkey::new_unique());
    let user_b = addr(Pubkey::new_unique());

    place(&mut ctx, place_args(Side::Ask, 101, 10), user_b);
    place(&mut ctx, place_args(Side::Ask, 100, 5), user_a);
    place(&mut ctx, place_args(Side::Ask, 100, 7), user_b);
    advance_slot(&mut ctx, 1);

    let l3 = |ctx: &mut Ctx, size: u64, max_rows: u16| {
        let ix = instruction::QuoteL3V0 {
            args: L3ArgsV0 {
                direction: Direction::Long,
                size: size.saturating_mul(UNIT),
                max_rows,
                include_taker_origin_reservations: false,
            },
        }
        .to_instruction(accounts::ResponseMarketV0 {
            market: addr(ctx.market),
        });

        let meta = send(ctx, ix).unwrap();
        let bytes = read_response(ctx, &meta);
        let response = L3ResponseV0::parse(&bytes).expect("l3 response");
        (
            response
                .rows
                .iter()
                .map(|row| (row.price, row.size / UNIT, row.user.authority.to_bytes()))
                .collect::<Vec<_>>(),
            response.more == 1,
        )
    };

    // The ladder aggregates the two orders at 100; the rows keep them apart,
    // in the order the fill would take them.
    assert_eq!(
        quote(&mut ctx, Direction::Long, 100),
        vec![(100, 12), (101, 10)]
    );

    let (rows, more) = l3(&mut ctx, 0, L3_ROWS_CEILING);
    assert_eq!(
        rows,
        vec![
            (100, 5, user_a.to_bytes()),
            (100, 7, user_b.to_bytes()),
            (101, 10, user_b.to_bytes()),
        ]
    );

    assert!(!more, "the whole side fit");

    // Order ids are the book's own, so a caller can act on a row.
    let ix = instruction::QuoteL3V0 {
        args: L3ArgsV0 {
            direction: Direction::Long,
            size: 0,
            max_rows: L3_ROWS_CEILING,
            include_taker_origin_reservations: false,
        },
    }
    .to_instruction(accounts::ResponseMarketV0 {
        market: addr(ctx.market),
    });

    let meta = send(&mut ctx, ix).unwrap();
    let bytes = read_response(&ctx, &meta);
    let response = L3ResponseV0::parse(&bytes).expect("l3 response");
    assert!(response.rows.iter().all(|row| row.order_id != 0));

    // A size bound stops the walk where a taker of that size would stop, and
    // says depth remains.
    let (rows, more) = l3(&mut ctx, 6, L3_ROWS_CEILING);
    assert_eq!(rows.len(), 2, "the second order carries past the size");
    assert!(more);

    // So does a row bound.
    let (rows, more) = l3(&mut ctx, 0, 1);
    assert_eq!(rows.len(), 1);
    assert!(more);

    // An order that is not matchable yet is not a row: a fresh placement is
    // behind the speed bump.
    let user_c = addr(Pubkey::new_unique());
    place(&mut ctx, place_args(Side::Ask, 99, 3), user_c);
    let (rows, _) = l3(&mut ctx, 0, L3_ROWS_CEILING);
    assert_eq!(rows.len(), 3, "the unactivated order is not reported");
    advance_slot(&mut ctx, 1);
    let (rows, _) = l3(&mut ctx, 0, L3_ROWS_CEILING);
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0], (99, 3, user_c.to_bytes()), "best price first");
}

/// The rows a market may report are bounded by the response region it already
/// has, so describing a book never widens the account every CPI carries.
#[test]
fn the_l3_ceiling_fits_the_region_the_market_already_pays_for() {
    use clob::state::{L3_ROWS_CEILING, L3_ROW_BYTES, RESPONSE_BUFFER_BYTES, RESPONSE_LEN_BYTES};
    // The ceiling is derived from the region, so a wider row buys fewer rows
    // rather than a bigger account. `L3RowV0` carries `node_index` and
    // `placed_slot` for the cross resolver — a handle to the order and its rest
    // time — which is 72 bytes per row and 114 rows in the same region.
    assert_eq!(L3_ROWS_CEILING, 114);
    assert!(
        RESPONSE_LEN_BYTES + L3_ROWS_CEILING as usize * L3_ROW_BYTES + 1 <= RESPONSE_BUFFER_BYTES
    );

    // The next row would not fit: the ceiling is the region's true capacity,
    // not a round number chosen under it.
    assert!(
        RESPONSE_LEN_BYTES + (L3_ROWS_CEILING as usize + 1) * L3_ROW_BYTES + 1
            > RESPONSE_BUFFER_BYTES
    );
}

/// The grace window runs from the slot an order becomes matchable, because
/// that is the first slot anyone could have seen it.
///
/// An order inside its activation delay is invisible to every reader of this
/// book. Aging it from placement would let an auction-style order — placed
/// far enough ahead that it is already past the window when it activates —
/// end the first walk that ever sees it, forfeiting the depth behind. Since
/// the walk is best-first and stopping is free for the maker, that is a lever
/// anyone could pull on purpose: rest a well-priced order on a sub-account
/// nobody carries, and every fill stops there the moment it wakes.
#[test]
fn an_order_waking_from_its_speed_bump_gets_the_grace_window() {
    let mut ctx = setup(); // grace = 2 slots, max activation delay = 20
    let auction = addr(Pubkey::new_unique());
    let user_b = addr(Pubkey::new_unique());
    ctx.svm.warp_to_slot(10);

    // The auction order rests ten slots before it can match; the ordinary
    // one is takeable next slot.
    place(
        &mut ctx,
        PlaceOrderArgsV0 {
            activation_delay_slots: Some(10),
            ..place_args(Side::Ask, 100, 5)
        },
        auction,
    );
    place(&mut ctx, place_args(Side::Ask, 101, 7), user_b);

    // Before it wakes it is nobody's problem: not quoted, not in the way.
    ctx.svm.warp_to_slot(12);
    assert_eq!(
        quote_users(&mut ctx, Direction::Long, 12, Some(vec![user_b])),
        vec![(101, 7)]
    );

    // The slot it wakes on, it is ten slots old by placement and zero slots
    // old by visibility. A caller that read the book a moment ago could not
    // have carried it, so the walk steps over it and keeps going.
    ctx.svm.warp_to_slot(20);
    assert_eq!(
        quote_users(&mut ctx, Direction::Long, 12, Some(vec![user_b])),
        vec![(101, 7)],
        "the depth behind a just-woken order is still reachable"
    );
    assert_eq!(
        quote_withheld(&mut ctx, Direction::Long, 12, Some(vec![user_b])),
        None
    );

    let changes = execute_users(&mut ctx, Direction::Long, 7, Some(vec![user_b]));
    assert_eq!(changes.len(), 1, "the reachable maker filled");

    // The window still closes: once it has been awake longer than the grace,
    // a caller that leaves it out gets nothing past it.
    place(&mut ctx, place_args(Side::Ask, 101, 7), user_b);
    ctx.svm.warp_to_slot(24);
    assert!(quote_users(&mut ctx, Direction::Long, 12, Some(vec![user_b])).is_empty());
    assert_eq!(
        quote_withheld(&mut ctx, Direction::Long, 12, Some(vec![user_b])),
        Some((100, 5)),
        "awake and unclaimed for longer than the window: the walk stops here"
    );
}

/// The market account signs its own initialization.
///
/// The client creates the roughly 98KB account with a keypair, because it is
/// larger than a CPI can allocate. Creation and initialization may land in
/// different transactions, so without the signature anyone could initialize
/// the account first, name themselves authority, and strand the rent.
#[test]
fn initializing_a_market_needs_the_accounts_own_keypair() {
    let mut svm = anchor_v2_testing::svm();
    svm.add_program_from_file(program_id(), SO_PATH)
        .expect("clob.so missing — run `bun run program:build:clob` first");
    let payer = Keypair::new();
    let admin = Keypair::new();
    let place_auth = Keypair::new();
    let squatter = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    let market_kp = Keypair::new();
    let space = ClobMarketV0::space_for(16);
    let rent = svm.minimum_balance_for_rent_exemption(space);
    svm.set_account(
        market_kp.pubkey(),
        solana_account::Account {
            lamports: rent,
            data: vec![0u8; space],
            owner: program_id(),
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();

    let config = MarketConfigV0 {
        market_index: 0,
        base_precision: BASE_PRECISION,
        order_tick_size: 1,
        order_step_size: 1,
        min_order_size: 1,
        blocking_min_size: 0,
        default_activation_delay_slots: 1,
        max_activation_delay_slots: 20,
        unknown_user_grace_slots: 2,
        evict_threshold_per_side: 6,
        max_quote_levels: 128,
        max_execute_fills: 64,
        max_execute_users: 32,
    };
    let ix =
        instruction::InitializeMarketV0 { config }.to_instruction(accounts::InitializeMarketV0 {
            authority: addr(admin.pubkey()),
            place_authority: addr(place_auth.pubkey()),
            market: addr(market_kp.pubkey()),
        });

    // A squatter holding the config keys but not the market keypair is
    // refused.
    let blockhash = svm.latest_blockhash();
    let msg = Message::new_with_blockhash(
        core::slice::from_ref(&ix),
        Some(&payer.pubkey()),
        &blockhash,
    );
    let signers: Vec<&dyn anchor_v2_testing::Signer> = vec![&payer, &squatter];
    assert!(
        VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &signers).is_err(),
        "the market's signature cannot be produced without its keypair"
    );

    // The creator holds it and initializes. The config authority does not
    // sign, so a program PDA can hold it from the start.
    svm.expire_blockhash();
    let blockhash = svm.latest_blockhash();
    let msg = Message::new_with_blockhash(&[ix], Some(&payer.pubkey()), &blockhash);
    let signers: Vec<&dyn anchor_v2_testing::Signer> = vec![&payer, &market_kp];
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &signers).unwrap();
    svm.send_transaction(tx).expect("the creator initializes");
}

fn close_market_ix(ctx: &Ctx, authority: Pubkey, recipient: Pubkey) -> Instruction {
    instruction::CloseMarketV0 {}.to_instruction(accounts::CloseMarketV0 {
        market: addr(ctx.market),
        authority: addr(authority),
        rent_recipient: addr(recipient),
    })
}

/// A market account holds the whole arena, so its rent is worth recovering.
/// Only the market authority closes it, and only while it is empty: every
/// resting order is also an open order in the caller's margin aggregates, and
/// a close reports no removal for them.
#[test]
fn only_the_authority_closes_an_empty_market_and_takes_the_rent() {
    let mut ctx = setup_with_capacity(16);
    let user = addr(Pubkey::new_unique());
    let order = place(&mut ctx, place_args(Side::Ask, 100, 5), user);

    let recipient = Pubkey::new_unique();
    // A book with an order in it does not close.
    assert_clob_err(
        {
            let ix = close_market_ix(&ctx, ctx.admin.pubkey(), recipient);
            send(&mut ctx, ix)
        },
        err_code(clob::error::ClobError::MarketNotEmpty),
    );

    cancel(&mut ctx, order, user).unwrap();

    // The place authority is not the config authority.
    assert_clob_err(
        {
            let ix = close_market_ix(&ctx, ctx.place_auth.pubkey(), recipient);
            send(&mut ctx, ix)
        },
        err_code(clob::error::ClobError::InvalidAuthority),
    );

    let rent = ctx.svm.get_account(&ctx.market).unwrap().lamports;
    assert!(rent > 0);
    let ix = close_market_ix(&ctx, ctx.admin.pubkey(), recipient);
    send(&mut ctx, ix).expect("an empty market closes");
    assert_eq!(ctx.svm.get_account(&recipient).unwrap().lamports, rent);
}

fn propose_authority_ix(ctx: &Ctx, signer: Pubkey, proposed: Pubkey) -> Instruction {
    instruction::ProposeMarketAuthorityV0 {
        args: ProposeMarketAuthorityArgsV0 {
            proposed_authority: addr(proposed),
        },
    }
    .to_instruction(accounts::ProposeMarketAuthorityV0 {
        market: addr(ctx.market),
        authority: addr(signer),
    })
}

fn accept_authority_ix(ctx: &Ctx, signer: Pubkey) -> Instruction {
    instruction::AcceptMarketAuthorityV0 {}.to_instruction(accounts::AcceptMarketAuthorityV0 {
        market: addr(ctx.market),
        pending_authority: addr(signer),
    })
}

/// `update_market_v0` logs the settings before and after the change, so an
/// indexer sees every rule change without diffing account snapshots.
#[test]
fn a_config_update_logs_the_settings_before_and_after() {
    let mut ctx = setup();
    let before = MarketSettingsV0::of(&market_state(&ctx));
    let ix = instruction::UpdateMarketV0 {
        args: UpdateMarketArgsV0 {
            order_tick_size: Some(2),
            ..Default::default()
        },
    }
    .to_instruction(accounts::UpdateMarketV0 {
        market: addr(ctx.market),
        authority: addr(ctx.admin.pubkey()),
    });

    let meta = send(&mut ctx, ix).unwrap();
    let after = MarketSettingsV0::of(&market_state(&ctx));
    assert_eq!(after.order_tick_size, 2);

    let clock: Clock = ctx.svm.get_sysvar();
    let expected = MarketUpdateRecordV0 {
        market: addr(ctx.market),
        authority: addr(ctx.admin.pubkey()),
        ts: clock.unix_timestamp,
        before,
        after,
    };
    assert_eq!(program_data(&meta), Event::data(&expected));

    // A config the book refuses changes nothing and logs nothing.
    let ix = instruction::UpdateMarketV0 {
        args: UpdateMarketArgsV0 {
            order_step_size: Some(0),
            ..Default::default()
        },
    }
    .to_instruction(accounts::UpdateMarketV0 {
        market: addr(ctx.market),
        authority: addr(ctx.admin.pubkey()),
    });
    assert_clob_err(
        send(&mut ctx, ix),
        err_code(clob::error::ClobError::InvalidConfig),
    );
}

/// The config authority moves only when the proposed key signs, so a mistyped
/// proposal cannot strand the market.
#[test]
fn the_config_authority_rotates_only_when_the_successor_accepts() {
    let mut ctx = setup();
    let successor = Keypair::new();
    let stranger = Keypair::new();

    assert_clob_err(
        {
            let ix = accept_authority_ix(&ctx, successor.pubkey());
            send_signed(&mut ctx, ix, &successor)
        },
        err_code(clob::error::ClobError::InvalidAuthority),
    );

    let ix = propose_authority_ix(&ctx, ctx.admin.pubkey(), successor.pubkey());
    send(&mut ctx, ix).unwrap();
    assert_eq!(market_state(&ctx).authority, addr(ctx.admin.pubkey()));
    assert_eq!(
        market_state(&ctx).pending_authority,
        addr(successor.pubkey())
    );

    assert_clob_err(
        {
            let ix = accept_authority_ix(&ctx, stranger.pubkey());
            send_signed(&mut ctx, ix, &stranger)
        },
        err_code(clob::error::ClobError::InvalidAuthority),
    );

    let ix = accept_authority_ix(&ctx, successor.pubkey());
    send_signed(&mut ctx, ix, &successor).unwrap();
    let state = market_state(&ctx);
    assert_eq!(state.authority, addr(successor.pubkey()));
    assert_eq!(state.pending_authority, ZERO_ADDRESS);

    // The previous authority no longer configures the market.
    assert_clob_err(
        {
            let ix = propose_authority_ix(&ctx, ctx.admin.pubkey(), stranger.pubkey());
            send(&mut ctx, ix)
        },
        err_code(clob::error::ClobError::InvalidAuthority),
    );
}

/// An order whose `max_ts` falls inside its own activation delay expires
/// before anything can match it. It would still take an arena slot and still
/// sit at the head of its side until the expiry crank reclaims it.
#[test]
fn an_order_that_cannot_outlive_its_activation_delay_is_refused() {
    let mut ctx = setup();
    let user = addr(Pubkey::new_unique());
    set_unix_timestamp(&mut ctx, 1_000);

    // Twenty slots of delay is at least eight seconds.
    let delayed = |max_ts: i64| PlaceOrderArgsV0 {
        activation_delay_slots: Some(20),
        max_ts,
        ..place_args(Side::Ask, 100, 5)
    };

    assert_clob_err(
        {
            let ix = place_ix(&ctx, delayed(1_005), user);
            send(&mut ctx, ix)
        },
        err_code(clob::error::ClobError::MaxTsBeforeActivation),
    );

    // A lifetime that reaches past the activation is accepted, and so is a
    // good-till-cancelled order.
    let ix = place_ix(&ctx, delayed(1_100), user);
    send(&mut ctx, ix).expect("a lifetime past the activation rests");
    let ix = place_ix(&ctx, delayed(0), user);
    send(&mut ctx, ix).expect("good-till-cancelled rests");

    // With no delay the order only has to outlive the placement itself.
    let ix = place_ix(
        &ctx,
        PlaceOrderArgsV0 {
            activation_delay_slots: Some(0),
            max_ts: 1_001,
            ..place_args(Side::Ask, 100, 5)
        },
        user,
    );

    send(&mut ctx, ix).expect("no delay to outlive");
}

/// `order_rules_v0` reports the live side counts and the arena, so a caller
/// that rests a taker's remainder can see a full side coming. A placement
/// onto a full side is refused, and the refusal takes the whole fill the
/// remainder came out of with it.
#[test]
fn the_order_rules_report_what_the_sides_hold() {
    let mut ctx = setup_with_capacity(16);
    let user = addr(Pubkey::new_unique());

    let rules = |ctx: &mut Ctx| {
        let ix = instruction::OrderRulesV0 {}.to_instruction(accounts::MarketViewV0 {
            market: addr(ctx.market),
        });
        let meta = send(ctx, ix).unwrap();
        let rules: clob::OrderRulesV0 = anchor_lang::wincode::config::deserialize(
            &meta.return_data.data,
            anchor_lang::BORSH_CONFIG,
        )
        .expect("decodes as OrderRulesV0");
        rules
    };

    let before = rules(&mut ctx);
    assert_eq!(before.side_order_counts, [0, 0]);
    assert_eq!(before.arena_capacity, 16);
    assert_eq!(before.evict_threshold_per_side, 6);
    assert_eq!(before.min_order_size, 1);
    assert_eq!(before.step_size, 1);
    assert_eq!(before.place_authority, ctx.place_auth.pubkey().to_bytes());
    assert_eq!(before.authority, ctx.admin.pubkey().to_bytes());

    place(&mut ctx, place_args(Side::Ask, 100, 5), user);
    place(&mut ctx, place_args(Side::Bid, 90, 5), user);
    place(&mut ctx, place_args(Side::Bid, 89, 5), user);
    let after = rules(&mut ctx);
    assert_eq!(after.side_order_counts, [2, 1]);

    // Fill the ask side to its cap and watch the count reach it.
    for i in 0..7 {
        place(&mut ctx, place_args(Side::Ask, 101 + i, 5), user);
    }

    let full = rules(&mut ctx);
    assert_eq!(full.side_order_counts[1], full.arena_capacity / 2);
    let ix = place_ix(&ctx, place_args(Side::Ask, 200, 5), user);
    assert_clob_err(
        send(&mut ctx, ix),
        err_code(clob::error::ClobError::SideAtCapacity),
    );
}
