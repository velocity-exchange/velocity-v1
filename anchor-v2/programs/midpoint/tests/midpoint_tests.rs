//! litesvm integration tests. Require the SBF build first:
//! `bun run program:build:midpoint` (cargo-build-sbf --tools-version v1.52).

use {
    anchor_v2_testing::{
        Keypair, LiteSVM, Message, Signer, VersionedMessage, VersionedTransaction,
    },
    litesvm::types::{FailedTransactionMetadata, TransactionMetadata},
    midpoint::{
        accounts,
        anchor_lang_v2::{
            prelude::Address,
            solana_program::instruction::{AccountMeta, Instruction},
        },
        instruction,
        state::{
            Direction, MidpointQuoterV0, QuoterConfigV0, SplineLevelInputV0, UserRefV0,
            RESPONSE_OFFSET,
        },
        ExecuteArgsV0, QuoteArgsV0, SetLevelsArgsV0, SetMidArgsV0, UpdateQuoterArgsV0,
    },
    solana_clock::Clock,
    solana_pubkey::Pubkey,
};

const SO_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/deploy/midpoint.so"
);

/// PRICE_PRECISION-ish scale used throughout: mid = 100_000_000 ($100 at 1e6).
const MID: u64 = 100_000_000;
const BASE_PRECISION: u64 = 1_000_000_000;
const UNIT: u64 = BASE_PRECISION;

fn program_id() -> Pubkey {
    "eb3Kwmht4evPGGonNHCQs1h7ng63ZUwZ9TyV1qPo23D"
        .parse()
        .unwrap()
}

fn addr(pk: Pubkey) -> Address {
    Address::new_from_array(pk.to_bytes())
}

fn system_program() -> Pubkey {
    "11111111111111111111111111111111".parse().unwrap()
}

fn instructions_sysvar() -> Pubkey {
    "Sysvar1nstructions1111111111111111111111111"
        .parse()
        .unwrap()
}

struct Ctx {
    svm: LiteSVM,
    payer: Keypair,
    /// The quoted wallet (config authority).
    authority: Keypair,
    hot: Keypair,
    execute_auth: Keypair,
    flow: Keypair,
    quoter: Pubkey,
}

fn quoter_pda(market_index: u16, authority: &Pubkey, sub_account_id: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"midpoint",
            market_index.to_le_bytes().as_ref(),
            authority.as_ref(),
            sub_account_id.to_le_bytes().as_ref(),
        ],
        &program_id(),
    )
    .0
}

fn config() -> QuoterConfigV0 {
    QuoterConfigV0 {
        market_index: 0,
        user_sub_account_id: 0,
        base_precision: BASE_PRECISION,
        max_mid_staleness_slots: 25,
        price_tick_size: 100,
        size_step: 1_000,
        min_quote_size: 10_000,
        require_attested_flow: false,
    }
}

fn setup() -> Ctx {
    setup_with_config(config())
}

fn setup_with_config(config: QuoterConfigV0) -> Ctx {
    let mut svm = anchor_v2_testing::svm();
    svm.add_program_from_file(program_id(), SO_PATH)
        .expect("midpoint.so missing — run `bun run program:build:midpoint` first");
    let payer = Keypair::new();
    let authority = Keypair::new();
    let hot = Keypair::new();
    let execute_auth = Keypair::new();
    let flow = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    let quoter = quoter_pda(
        config.market_index,
        &authority.pubkey(),
        config.user_sub_account_id,
    );
    let require_flow = config.require_attested_flow;
    let ix =
        instruction::InitializeQuoterV0 { config }.to_instruction(accounts::InitializeQuoterV0 {
            payer: addr(payer.pubkey()),
            authority: addr(authority.pubkey()),
            execute_authority: addr(execute_auth.pubkey()),
            hot_authority: addr(hot.pubkey()),
            flow_authority: require_flow.then(|| addr(flow.pubkey())),
            quoter: addr(quoter),
            system_program: addr(system_program()),
        });
    let mut ctx = Ctx {
        svm,
        payer,
        authority,
        hot,
        execute_auth,
        flow,
        quoter,
    };
    send(&mut ctx, ix).unwrap();
    ctx
}

/// Sign with payer plus whichever of the known keys the metas mark as signer.
fn send(ctx: &mut Ctx, ix: Instruction) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    ctx.svm.expire_blockhash();
    let blockhash = ctx.svm.latest_blockhash();
    let msg = Message::new_with_blockhash(&[ix.clone()], Some(&ctx.payer.pubkey()), &blockhash);
    let mut signers: Vec<&dyn anchor_v2_testing::Signer> = vec![&ctx.payer];
    for kp in [&ctx.authority, &ctx.hot, &ctx.execute_auth, &ctx.flow] {
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

fn parse_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

fn parse_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

fn read_response(ctx: &Ctx, meta: &TransactionMetadata) -> Vec<u8> {
    assert_eq!(
        meta.return_data.program_id.to_bytes(),
        program_id().to_bytes()
    );
    let offset = parse_u32(&meta.return_data.data) as usize;
    assert_eq!(offset, RESPONSE_OFFSET);
    let len = parse_u32(&meta.return_data.data[4..]) as usize;
    let account = ctx.svm.get_account(&ctx.quoter).unwrap();
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

/// ExecuteResponseV0 { balance_changes, cancelled } with at most one change
/// and no completed ids: (authority, base, quote) per change.
fn parse_execute(b: &[u8]) -> Vec<([u8; 32], u64, u64)> {
    let count = parse_u32(b) as usize;
    let mut off = 4;
    (0..count)
        .map(|_| {
            let authority: [u8; 32] = b[off..off + 32].try_into().unwrap();
            let base = parse_u64(&b[off + 34..]);
            let quote = parse_u64(&b[off + 42..]);
            let completed = parse_u32(&b[off + 50..]) as usize;
            assert_eq!(completed, 0, "midpoint never completes orders");
            off += 32 + 2 + 8 + 8 + 4;
            (authority, base, quote)
        })
        .collect()
}

fn set_mid_ix(ctx: &Ctx, mid: u64, sequence: u64) -> Instruction {
    instruction::SetMidV0 {
        args: SetMidArgsV0 { mid, sequence },
    }
    .to_instruction(accounts::SetMidV0 {
        quoter: addr(ctx.quoter),
        hot_authority: addr(ctx.hot.pubkey()),
    })
}

fn set_levels_ix(ctx: &Ctx, args: SetLevelsArgsV0) -> Instruction {
    instruction::SetLevelsV0 { args }.to_instruction(accounts::SetLevelsV0 {
        quoter: addr(ctx.quoter),
        hot_authority: addr(ctx.hot.pubkey()),
    })
}

fn send_levels(
    ctx: &mut Ctx,
    args: SetLevelsArgsV0,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    let ix = set_levels_ix(ctx, args);
    send(ctx, ix)
}

fn levels(inputs: &[(u64, u64)]) -> Vec<SplineLevelInputV0> {
    inputs
        .iter()
        .map(|(offset_ppm, size)| SplineLevelInputV0 {
            offset_ppm: *offset_ppm,
            size: *size,
        })
        .collect()
}

/// 10 bps / 30 bps rungs on both sides, one unit each.
fn arm(ctx: &mut Ctx) {
    send_levels(
        ctx,
        SetLevelsArgsV0 {
            mid: Some(MID),
            sequence: None,
            bids: Some(levels(&[(1_000, UNIT), (3_000, UNIT)])),
            asks: Some(levels(&[(1_000, UNIT), (3_000, UNIT)])),
        },
    )
    .unwrap();
}

fn quote_ix(ctx: &Ctx, direction: Direction, size: u64) -> Instruction {
    instruction::QuoteV0 {
        args: QuoteArgsV0 {
            direction,
            size,
            users: None,
            taker: None,
        },
    }
    .to_instruction(accounts::QuoteV0 {
        quoter: addr(ctx.quoter),
        instructions_sysvar: addr(instructions_sysvar()),
    })
}

fn execute_ix(ctx: &Ctx, direction: Direction, size: u64) -> Instruction {
    instruction::ExecuteV0 {
        args: ExecuteArgsV0 {
            direction,
            size,
            users: None,
            taker: None,
        },
    }
    .to_instruction(accounts::ExecuteV0 {
        quoter: addr(ctx.quoter),
        execute_authority: addr(ctx.execute_auth.pubkey()),
        instructions_sysvar: addr(instructions_sysvar()),
    })
}

fn quote_levels(ctx: &mut Ctx, direction: Direction, size: u64) -> Vec<(u64, u64)> {
    let ix = quote_ix(ctx, direction, size);
    let meta = send(ctx, ix).unwrap();
    let response = read_response(ctx, &meta);
    parse_levels(&response)
}

#[test]
fn spline_quotes_both_sides_around_mid() {
    let mut ctx = setup();
    arm(&mut ctx);

    // Ask side (taker Long): mid + 10bps = 100_100_000, + 30bps = 100_300_000.
    let asks = quote_levels(&mut ctx, Direction::Long, 2 * UNIT);
    assert_eq!(asks, vec![(100_100_000, UNIT), (100_300_000, UNIT)]);
    // Bid side: mid - 10bps, - 30bps.
    let bids = quote_levels(&mut ctx, Direction::Short, 2 * UNIT);
    assert_eq!(bids, vec![(99_900_000, UNIT), (99_700_000, UNIT)]);

    // Quote truncates at the taker's size: half a unit consumes only the
    // first rung.
    let asks = quote_levels(&mut ctx, Direction::Long, UNIT / 2);
    assert_eq!(asks, vec![(100_100_000, UNIT / 2)]);
}

#[test]
fn prices_round_away_from_mid_to_the_tick() {
    let mut ctx = setup();
    // Offset 1 ppm of 100e6 = 100 exactly; offset 3 ppm = 300. With tick
    // 100 both land on ticks; use offset 7 ppm = 700 → tick-aligned too.
    // Force rounding with an off-tick mid instead.
    {
        let ix = set_mid_ix(&ctx, 100_000_050, 0);
        send(&mut ctx, ix).unwrap();
    }
    send_levels(
        &mut ctx,
        SetLevelsArgsV0 {
            mid: None,
            sequence: None,
            bids: Some(levels(&[(0, UNIT)])),
            asks: Some(levels(&[(0, UNIT)])),
        },
    )
    .unwrap();
    // Offset 0: raw price = mid on both sides; ask rounds up to the next
    // tick, bid rounds down.
    let asks = quote_levels(&mut ctx, Direction::Long, UNIT);
    assert_eq!(asks, vec![(100_000_100, UNIT)]);
    let bids = quote_levels(&mut ctx, Direction::Short, UNIT);
    assert_eq!(bids, vec![(100_000_000, UNIT)]);
}

#[test]
fn stale_mid_stops_quoting_and_a_fresh_write_resumes() {
    let mut ctx = setup();
    arm(&mut ctx);
    assert_eq!(quote_levels(&mut ctx, Direction::Long, UNIT).len(), 1);

    // Warp past the staleness window: the book goes silent.
    let clock: Clock = ctx.svm.get_sysvar();
    ctx.svm.warp_to_slot(clock.slot + 26);
    assert!(quote_levels(&mut ctx, Direction::Long, UNIT).is_empty());

    // A fresh mid write revives it.
    {
        let ix = set_mid_ix(&ctx, MID, 0);
        send(&mut ctx, ix).unwrap();
    }
    assert_eq!(quote_levels(&mut ctx, Direction::Long, UNIT).len(), 1);
}

#[test]
fn nonzero_mid_sequence_must_increase() {
    let mut ctx = setup();
    {
        let ix = set_mid_ix(&ctx, MID, 5);
        send(&mut ctx, ix).unwrap();
    }
    // Equal and lower nonzero sequences are stale.
    {
        let ix = set_mid_ix(&ctx, MID + 1, 5);
        assert!(send(&mut ctx, ix).is_err());
    }
    {
        let ix = set_mid_ix(&ctx, MID + 1, 4);
        assert!(send(&mut ctx, ix).is_err());
    }
    // Higher passes; zero always passes (opt-out).
    {
        let ix = set_mid_ix(&ctx, MID + 1, 6);
        send(&mut ctx, ix).unwrap();
    }
    {
        let ix = set_mid_ix(&ctx, MID + 2, 0);
        send(&mut ctx, ix).unwrap();
    }
    let quoter: MidpointQuoterV0 = read_quoter(&ctx);
    assert_eq!(quoter.mid_price, MID + 2);
    assert_eq!(quoter.mid_sequence, 6);
}

fn read_quoter(ctx: &Ctx) -> MidpointQuoterV0 {
    let account = ctx.svm.get_account(&ctx.quoter).unwrap();
    let mut quoter = [0u8; core::mem::size_of::<MidpointQuoterV0>()];
    quoter.copy_from_slice(&account.data[8..8 + core::mem::size_of::<MidpointQuoterV0>()]);
    unsafe { core::mem::transmute::<_, MidpointQuoterV0>(quoter) }
}

#[test]
fn only_the_hot_authority_writes_mid_and_levels() {
    let mut ctx = setup();
    let mut ix = set_mid_ix(&ctx, MID, 0);
    // Swap the signer for the (also known) authority key: address mismatch.
    ix.accounts[1] = AccountMeta::new_readonly(ctx.authority.pubkey(), true);
    assert!(send(&mut ctx, ix).is_err());

    // Rotate the hot key through update, then the old key fails.
    let new_hot = Keypair::new();
    let ix = instruction::UpdateQuoterV0 {
        args: UpdateQuoterArgsV0::default(),
    }
    .to_instruction(accounts::UpdateQuoterV0 {
        quoter: addr(ctx.quoter),
        authority: addr(ctx.authority.pubkey()),
        new_hot_authority: Some(addr(new_hot.pubkey())),
        new_flow_authority: None,
    });
    send(&mut ctx, ix).unwrap();
    {
        let ix = set_mid_ix(&ctx, MID, 0);
        assert!(send(&mut ctx, ix).is_err());
    }
}

#[test]
fn execute_consumes_the_spline_and_reports_one_balance_change() {
    let mut ctx = setup();
    arm(&mut ctx);

    // Take 1.5 units of the ask side: full first rung + half the second.
    let ix = execute_ix(&ctx, Direction::Long, UNIT + UNIT / 2);
    let meta = send(&mut ctx, ix).unwrap();
    let changes = parse_execute(&read_response(&ctx, &meta));
    assert_eq!(changes.len(), 1);
    let (authority, base, quote) = changes[0];
    assert_eq!(authority, ctx.authority.pubkey().to_bytes());
    assert_eq!(base, UNIT + UNIT / 2);
    // 1.0 @ 100_100_000 + 0.5 @ 100_300_000, floored per level.
    assert_eq!(quote, 100_100_000 + 100_300_000 / 2);

    // The consumed intent stays consumed: the next quote starts at the
    // second rung's remainder.
    let asks = quote_levels(&mut ctx, Direction::Long, 2 * UNIT);
    assert_eq!(asks, vec![(100_300_000, UNIT / 2)]);

    // Rewriting the side resets `filled`.
    send_levels(
        &mut ctx,
        SetLevelsArgsV0 {
            mid: None,
            sequence: None,
            bids: None,
            asks: Some(levels(&[(1_000, UNIT)])),
        },
    )
    .unwrap();
    let asks = quote_levels(&mut ctx, Direction::Long, 2 * UNIT);
    assert_eq!(asks, vec![(100_100_000, UNIT)]);
}

#[test]
fn execute_requires_the_execute_authority() {
    let mut ctx = setup();
    arm(&mut ctx);
    let mut ix = execute_ix(&ctx, Direction::Long, UNIT);
    ix.accounts[1] = AccountMeta::new_readonly(ctx.hot.pubkey(), true);
    assert!(send(&mut ctx, ix).is_err());
}

#[test]
fn quoted_user_gates_apply() {
    let mut ctx = setup();
    arm(&mut ctx);
    let quoted = UserRefV0 {
        authority: addr(ctx.authority.pubkey()),
        sub_account_id: 0,
    };
    let stranger = UserRefV0 {
        authority: addr(Pubkey::new_unique()),
        sub_account_id: 0,
    };

    // A user set that can't settle the quoted user sees an empty book.
    let ix = instruction::QuoteV0 {
        args: QuoteArgsV0 {
            direction: Direction::Long,
            size: UNIT,
            users: Some(vec![stranger]),
            taker: None,
        },
    }
    .to_instruction(accounts::QuoteV0 {
        quoter: addr(ctx.quoter),
        instructions_sysvar: addr(instructions_sysvar()),
    });
    let meta = send(&mut ctx, ix).unwrap();
    assert!(parse_levels(&read_response(&ctx, &meta)).is_empty());

    // The quoted user's own flow sees an empty book (self-trade).
    let ix = instruction::QuoteV0 {
        args: QuoteArgsV0 {
            direction: Direction::Long,
            size: UNIT,
            users: None,
            taker: Some(quoted),
        },
    }
    .to_instruction(accounts::QuoteV0 {
        quoter: addr(ctx.quoter),
        instructions_sysvar: addr(instructions_sysvar()),
    });
    let meta = send(&mut ctx, ix).unwrap();
    assert!(parse_levels(&read_response(&ctx, &meta)).is_empty());

    // A set including the quoted user quotes normally.
    let ix = instruction::QuoteV0 {
        args: QuoteArgsV0 {
            direction: Direction::Long,
            size: UNIT,
            users: Some(vec![stranger, quoted]),
            taker: Some(stranger),
        },
    }
    .to_instruction(accounts::QuoteV0 {
        quoter: addr(ctx.quoter),
        instructions_sysvar: addr(instructions_sysvar()),
    });
    let meta = send(&mut ctx, ix).unwrap();
    assert_eq!(parse_levels(&read_response(&ctx, &meta)).len(), 1);
}

#[test]
fn attested_flow_requires_the_flow_authority_signature() {
    let mut ctx = setup_with_config(QuoterConfigV0 {
        require_attested_flow: true,
        ..config()
    });
    arm(&mut ctx);

    // Unattested: empty book.
    assert!(quote_levels(&mut ctx, Direction::Long, UNIT).is_empty());

    // The flow authority co-signing the transaction (as an extra signer
    // meta on the ix) opens the book — introspection sees the signer bit.
    let mut ix = quote_ix(&ctx, Direction::Long, UNIT);
    ix.accounts
        .push(AccountMeta::new_readonly(ctx.flow.pubkey(), true));
    let meta = send(&mut ctx, ix).unwrap();
    assert_eq!(parse_levels(&read_response(&ctx, &meta)).len(), 1);

    // Execute is gated the same way.
    let ix = execute_ix(&ctx, Direction::Long, UNIT);
    let meta = send(&mut ctx, ix).unwrap();
    assert!(parse_execute(&read_response(&ctx, &meta)).is_empty());
    let mut ix = execute_ix(&ctx, Direction::Long, UNIT);
    ix.accounts
        .push(AccountMeta::new_readonly(ctx.flow.pubkey(), true));
    let meta = send(&mut ctx, ix).unwrap();
    assert_eq!(parse_execute(&read_response(&ctx, &meta)).len(), 1);
}

#[test]
fn dusty_remainders_and_misaligned_sizes_do_not_quote() {
    let mut ctx = setup();
    // step 1_000, min 10_000: a 10_500 level floors to 10_000 (quotable);
    // a 9_999 level floors to 9_000 < min (silent).
    send_levels(
        &mut ctx,
        SetLevelsArgsV0 {
            mid: Some(MID),
            sequence: None,
            bids: None,
            asks: Some(levels(&[(1_000, 10_500), (2_000, 9_999)])),
        },
    )
    .unwrap();
    let asks = quote_levels(&mut ctx, Direction::Long, UNIT);
    assert_eq!(asks, vec![(100_100_000, 10_000)]);
}

#[test]
fn level_validation_rejects_bad_shapes() {
    let mut ctx = setup();
    // Descending offsets.
    let ix = set_levels_ix(
        &ctx,
        SetLevelsArgsV0 {
            mid: None,
            sequence: None,
            bids: None,
            asks: Some(levels(&[(3_000, UNIT), (1_000, UNIT)])),
        },
    );
    assert!(send(&mut ctx, ix).is_err());
    // Zero size.
    let ix = set_levels_ix(
        &ctx,
        SetLevelsArgsV0 {
            mid: None,
            sequence: None,
            bids: Some(levels(&[(1_000, 0)])),
            asks: None,
        },
    );
    assert!(send(&mut ctx, ix).is_err());
    // Equal offsets (must be strictly ascending).
    let ix = set_levels_ix(
        &ctx,
        SetLevelsArgsV0 {
            mid: None,
            sequence: None,
            bids: Some(levels(&[(1_000, UNIT), (1_000, UNIT)])),
            asks: None,
        },
    );
    assert!(send(&mut ctx, ix).is_err());
}

#[test]
fn paused_quoter_is_silent() {
    let mut ctx = setup();
    arm(&mut ctx);
    let ix = instruction::UpdateQuoterV0 {
        args: UpdateQuoterArgsV0 {
            is_paused: Some(true),
            ..Default::default()
        },
    }
    .to_instruction(accounts::UpdateQuoterV0 {
        quoter: addr(ctx.quoter),
        authority: addr(ctx.authority.pubkey()),
        new_hot_authority: None,
        new_flow_authority: None,
    });
    send(&mut ctx, ix).unwrap();
    assert!(quote_levels(&mut ctx, Direction::Long, UNIT).is_empty());
    let meta = {
        let ix = execute_ix(&ctx, Direction::Long, UNIT);
        send(&mut ctx, ix).unwrap()
    };
    assert!(parse_execute(&read_response(&ctx, &meta)).is_empty());
}

/// THE number this program exists for: a mid write must be near the compute
/// floor so makers can track fair value tick-by-tick for ~free. The budget
/// is deliberately above the measured cost (headroom for anchor-v2 drift)
/// but low enough that a regression that adds real work fails loudly.
#[test]
fn set_mid_cu_stays_near_the_floor() {
    let mut ctx = setup();
    arm(&mut ctx);
    let ix = set_mid_ix(&ctx, MID + 50, 0);
    let meta = send(&mut ctx, ix).unwrap();
    let set_mid_cu = meta.compute_units_consumed;

    let meta = send_levels(
        &mut ctx,
        SetLevelsArgsV0 {
            mid: Some(MID),
            sequence: None,
            bids: Some(levels(
                &(0..64).map(|i| (1_000 + i, UNIT)).collect::<Vec<_>>(),
            )),
            asks: Some(levels(
                &(0..64).map(|i| (1_000 + i, UNIT)).collect::<Vec<_>>(),
            )),
        },
    )
    .unwrap();
    let set_levels_cu = meta.compute_units_consumed;

    println!("CU — set_mid: {set_mid_cu}, set_levels(64×2 + mid): {set_levels_cu}");
    assert!(set_mid_cu <= 800, "set_mid regressed: {set_mid_cu} CU");
    assert!(
        set_levels_cu <= 10_000,
        "set_levels regressed: {set_levels_cu} CU"
    );
}
