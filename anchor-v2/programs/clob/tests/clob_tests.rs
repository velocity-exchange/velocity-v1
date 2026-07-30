//! litesvm integration tests. Require the SBF build first:
//! `bun run program:build:clob` (cargo-build-sbf --tools-version v1.52).

use {
    anchor_v2_testing::{
        Keypair, LiteSVM, Message, Signer, VersionedMessage, VersionedTransaction,
    },
    clob::{
        accounts,
        anchor_lang_v2::{prelude::Address, solana_program::instruction::Instruction},
        instruction,
        state::{
            ClobHeaderV0, ClobMarketV0, Direction, MarketConfigV0, OrderNodeV0, OrderRefV0, Side,
            UserRefV0, ORDERS_OFFSET,
        },
        CancelOrderArgsV0, EvictWorstArgsV0, ExecuteArgsV0, PlaceOrderArgsV0, QuoteArgsV0,
        RemoveExpiredArgsV0, ResizeMarketArgsV0, UpdateMarketArgsV0,
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
    // Fresh blockhash per send so identical instruction streams don't dedupe.
    ctx.svm.expire_blockhash();
    let blockhash = ctx.svm.latest_blockhash();
    let msg = Message::new_with_blockhash(&[ix.clone()], Some(&ctx.payer.pubkey()), &blockhash);
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
    let count = parse_u32(b) as usize;
    (0..count)
        .map(|i| {
            let off = 4 + i * 16;
            (parse_u64(&b[off..]), parse_u64(&b[off + 8..]))
        })
        .collect()
}

/// ExecuteResponseV0 { balance_changes: Vec<UserBalanceChange> } — entries
/// are (user: UserRefV0 {authority: 32, sub: u16}, base_size: u64,
/// quote_size: u64, completed_order_ids: Vec<u64>), so variable-length.
/// Returns the authority as the identity (tests place with sub 0).
fn parse_balance_changes(b: &[u8]) -> Vec<([u8; 32], u64, u64, Vec<u64>)> {
    let count = parse_u32(b) as usize;
    let mut off = 4;
    (0..count)
        .map(|_| {
            let authority = b[off..off + 32].try_into().unwrap();
            assert_eq!(
                u16::from_le_bytes(b[off + 32..off + 34].try_into().unwrap()),
                0
            );
            let base = parse_u64(&b[off + 34..]);
            let quote = parse_u64(&b[off + 42..]);
            let ids = parse_u32(&b[off + 50..]) as usize;
            let completed = (0..ids)
                .map(|i| parse_u64(&b[off + 54 + i * 8..]))
                .collect();
            off += 54 + ids * 8;
            (authority, base, quote, completed)
        })
        .collect()
}

fn place(ctx: &mut Ctx, args: PlaceOrderArgsV0, user: Address) -> OrderRefV0 {
    let ix = place_ix(ctx, args, user);
    let meta = send(ctx, ix).unwrap();
    parse_order_ref(&meta.return_data.data)
}

fn quote_meta_users(
    ctx: &mut Ctx,
    direction: Direction,
    size: u64,
    users: Option<Vec<Address>>,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    let ix = instruction::QuoteV0 {
        args: QuoteArgsV0 {
            direction,
            size,
            users: users.map(|u| u.into_iter().map(uref).collect()),
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

fn execute_meta_users(
    ctx: &mut Ctx,
    direction: Direction,
    size: u64,
    users: Option<Vec<Address>>,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    let ix = instruction::ExecuteV0 {
        args: ExecuteArgsV0 {
            direction,
            size,
            users: users.map(|u| u.into_iter().map(uref).collect()),
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
/// (user, order_id, price, base_asset_amount, side).
fn parse_removed(b: &[u8]) -> ([u8; 32], u64, u64, u64, u8) {
    assert_eq!(u16::from_le_bytes(b[32..34].try_into().unwrap()), 0);
    (
        b[..32].try_into().unwrap(),
        parse_u64(&b[34..]),
        parse_u64(&b[42..]),
        parse_u64(&b[50..]),
        b[58],
    )
}

/// Trailing `cancelled` vec of ExecuteResponseV0 — entries are
/// (user: UserRefV0, order_id, base_asset_amount). Walks past the
/// variable-length balance changes first.
fn parse_cancelled(b: &[u8]) -> Vec<([u8; 32], u64, u64)> {
    let n = parse_u32(b) as usize;
    let mut off = 4;
    for _ in 0..n {
        let ids = parse_u32(&b[off + 50..]) as usize;
        off += 54 + ids * 8;
    }
    let m = parse_u32(&b[off..]) as usize;
    (0..m)
        .map(|i| {
            let o = off + 4 + i * 50;
            (
                b[o..o + 32].try_into().unwrap(),
                parse_u64(&b[o + 34..]),
                parse_u64(&b[o + 42..]),
            )
        })
        .collect()
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
    let (evicted_user, _, price, base, side) = parse_removed(&meta.return_data.data);
    assert_eq!(
        (evicted_user, price, base, side),
        (user.to_bytes(), 100, 1, Side::Bid.to_u8())
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
    let (removed_user, _, _, base, side) = parse_removed(&meta.return_data.data);
    assert_eq!(
        (removed_user, base, side),
        (user.to_bytes(), 5, Side::Ask.to_u8())
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
fn unknown_user_grace_skips_fresh_orders_and_fails_on_aged_ones() {
    let mut ctx = setup(); // grace = 2 slots
    let user_a = addr(Pubkey::new_unique());
    let user_b = addr(Pubkey::new_unique());
    ctx.svm.warp_to_slot(10);
    place(&mut ctx, place_args(Side::Ask, 100, 5), user_a);
    place(&mut ctx, place_args(Side::Ask, 101, 7), user_b);
    ctx.svm.warp_to_slot(11); // both active, age 1 <= grace

    // A's user missing but fresh: skipped, the fill continues past it.
    assert_eq!(
        quote_users(&mut ctx, Direction::Long, 12, Some(vec![user_b])),
        vec![(101, 7)]
    );
    let changes = execute_users(&mut ctx, Direction::Long, 7, Some(vec![user_b]));
    assert_eq!(changes, vec![(user_b.to_bytes(), 7, 707, vec![2])]);
    // A's order still resting, untouched.
    assert_eq!(quote(&mut ctx, Direction::Long, 12), vec![(100, 5)]);

    // Past the grace window a missing user means a stale keeper: fail the
    // whole call rather than fill around the order.
    place(&mut ctx, place_args(Side::Ask, 101, 7), user_b);
    ctx.svm.warp_to_slot(14); // A age 4 > grace
    assert_clob_err(
        quote_meta_users(&mut ctx, Direction::Long, 12, Some(vec![user_b])),
        err_code(clob::error::ClobError::StaleUserSet),
    );
    assert_clob_err(
        execute_meta_users(&mut ctx, Direction::Long, 12, Some(vec![user_b])),
        err_code(clob::error::ClobError::StaleUserSet),
    );
    // Complete user set fills both.
    let changes = execute_users(&mut ctx, Direction::Long, 12, Some(vec![user_a, user_b]));
    assert_eq!(
        changes,
        vec![
            (user_a.to_bytes(), 5, 500, vec![1]),
            (user_b.to_bytes(), 7, 707, vec![3])
        ]
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
            direction: Direction::Short,
            size: u64::MAX,
            users: None,
            taker: None,
        },
    }
    .to_instruction(accounts::QuoteV0 {
        market: addr(ctx.market),
    });
    let quote_meta = send(&mut ctx, ix).unwrap();

    // Execute across 50 orders (one user, so the response stays small).
    let execute_meta = execute_meta(&mut ctx, Direction::Short, 500).unwrap();

    println!(
        "CU — place(best, full book): {}, evict_worst: {}, quote(full side): {}, execute(50 orders): {}",
        place_meta.compute_units_consumed,
        evict_meta.compute_units_consumed,
        quote_meta.compute_units_consumed,
        execute_meta.compute_units_consumed,
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
            direction,
            size,
            users: None,
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
            direction,
            size,
            users: None,
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
