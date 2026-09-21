//! litesvm integration tests. Require the SBF build first:
//! `bun run program:build:midpoint` (cargo-build-sbf --tools-version v1.52).

// Every send helper returns litesvm's `FailedTransactionMetadata` by value;
// boxing a test harness's error type buys nothing.
#![allow(clippy::result_large_err)]

use {
    anchor_v2_testing::{Keypair, LiteSVM, Signer},
    litesvm::types::{FailedTransactionMetadata, TransactionMetadata},
    midpoint::{
        accounts,
        anchor_lang::solana_program::instruction::{AccountMeta, Instruction},
        instruction,
        state::{
            CancelSidesV0, Direction, MidpointQuoterV0, QuoterConfigV0, SplineLevelInputV0,
            UserCapsV0, UserRefV0, RESPONSE_OFFSET,
        },
        CancelAllArgsV0, ExecuteArgsV0, QuoteArgsV0, SetLevelsArgsV0, SetMidArgsV0,
        UpdateQuoterArgsV0,
    },
    quoter_test_support::{addr, system_program},
    solana_clock::Clock,
    solana_pubkey::Pubkey,
};

const SO_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/deploy/midpoint.so"
);

/// Mid at the market's price precision: 100_000_000 = $100 at 1e6.
const MID: u64 = 100_000_000;
const BASE_PRECISION: u64 = 1_000_000_000;
const UNIT: u64 = BASE_PRECISION;

fn program_id() -> Pubkey {
    "eb3Kwmht4evPGGonNHCQs1h7ng63ZUwZ9TyV1qPo23D"
        .parse()
        .unwrap()
}

struct Ctx {
    svm: LiteSVM,
    payer: Keypair,
    /// The maker's config key.
    authority: Keypair,
    /// The quoted wallet — signs creation, seeds the PDA, receives the fills.
    user_authority: Keypair,
    hot: Keypair,
    execute_auth: Keypair,
    quoter: Pubkey,
}

fn quoter_pda(market_index: u16, user_authority: &Pubkey, sub_account_id: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"midpoint",
            market_index.to_le_bytes().as_ref(),
            user_authority.as_ref(),
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
        // A one percent band around velocity's oracle.
        max_mid_deviation_ppm: 10_000,
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
    let user_authority = Keypair::new();
    let hot = Keypair::new();
    let execute_auth = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    let quoter = quoter_pda(
        config.market_index,
        &user_authority.pubkey(),
        config.user_sub_account_id,
    );
    let ix =
        instruction::InitializeQuoterV0 { config }.to_instruction(accounts::InitializeQuoterV0 {
            payer: addr(payer.pubkey()),
            authority: addr(authority.pubkey()),
            user_authority: addr(user_authority.pubkey()),
            execute_authority: addr(execute_auth.pubkey()),
            hot_authority: addr(hot.pubkey()),
            quoter: addr(quoter),
            system_program: addr(system_program()),
        });
    let mut ctx = Ctx {
        svm,
        payer,
        authority,
        user_authority,
        hot,
        execute_auth,
        quoter,
    };

    send(&mut ctx, ix).unwrap();
    ctx
}

/// Sign with payer plus whichever of the known keys the metas mark as signer.
fn send(ctx: &mut Ctx, ix: Instruction) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    send_signed_by(ctx, ix, None)
}

fn send_signed_by(
    ctx: &mut Ctx,
    mut ix: Instruction,
    co_signer: Option<&Keypair>,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    if let Some(co_signer) = co_signer {
        ix.accounts
            .push(AccountMeta::new_readonly(co_signer.pubkey(), true));
    }

    let candidates: Vec<&Keypair> = [
        &ctx.authority,
        &ctx.user_authority,
        &ctx.hot,
        &ctx.execute_auth,
    ]
    .into_iter()
    .chain(co_signer)
    .collect();

    quoter_test_support::send(&mut ctx.svm, &ctx.payer, &candidates, ix, None)
}

fn read_response(ctx: &Ctx, meta: &TransactionMetadata) -> Vec<u8> {
    let response = quoter_test_support::read_response(&ctx.svm, program_id(), ctx.quoter, meta);
    assert_eq!(response.offset, RESPONSE_OFFSET);
    response.bytes
}

/// The quote response, through the layout's own parser rather than a second
/// hand-rolled walk of it.
fn parse_levels(b: &[u8]) -> Vec<(u64, u64)> {
    midpoint::state::QuoteResponseV0::parse(b)
        .expect("quote response")
        .levels
        .iter()
        .map(|level| (level.price, level.size))
        .collect()
}

/// The execute response: `(user authority, base, quote)` per change. The
/// midpoint never completes an order and never culls a remainder, and both are
/// asserted here rather than assumed.
fn parse_execute(b: &[u8]) -> Vec<([u8; 32], u64, u64)> {
    let response = midpoint::state::ExecuteResponseV0::parse(b).expect("execute response");
    assert!(
        response.completed.is_empty(),
        "midpoint never completes orders"
    );
    assert!(
        response.cancelled.is_empty(),
        "midpoint never culls remainders"
    );

    response
        .changes
        .iter()
        .map(|change| {
            (
                *change.user.authority.as_array(),
                change.base_size,
                change.quote_size,
            )
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

fn quote_args<'a>(direction: Direction, size: u64) -> QuoteArgsV0<'a> {
    QuoteArgsV0 {
        taker_served_window: true,
        include_taker_origin_reservations: false,
        direction,
        size,
        users: &[],
        caps: UserCapsV0::EMPTY,
        reference_price: 0,
        taker: None,
        limit_price: 0,
    }
}

fn quote_ix_with(ctx: &Ctx, args: QuoteArgsV0<'_>) -> Instruction {
    instruction::QuoteV0 { args }.to_instruction(accounts::QuoteV0 {
        quoter: addr(ctx.quoter),
    })
}

fn quote_ix(ctx: &Ctx, direction: Direction, size: u64) -> Instruction {
    quote_ix_with(ctx, quote_args(direction, size))
}

fn execute_ix(ctx: &Ctx, direction: Direction, size: u64) -> Instruction {
    execute_ix_served(ctx, direction, size, true)
}

fn execute_ix_served(
    ctx: &Ctx,
    direction: Direction,
    size: u64,
    taker_served_window: bool,
) -> Instruction {
    instruction::ExecuteV0 {
        args: ExecuteArgsV0 {
            taker_served_window,
            include_taker_origin_reservations: false,
            direction,
            size,
            users: &[],
            caps: UserCapsV0::EMPTY,
            reference_price: 0,
            taker: None,
        },
    }
    .to_instruction(accounts::ExecuteV0 {
        quoter: addr(ctx.quoter),
        execute_authority: addr(ctx.execute_auth.pubkey()),
    })
}

fn update_ix(ctx: &Ctx, args: UpdateQuoterArgsV0, new_hot: Option<Pubkey>) -> Instruction {
    instruction::UpdateQuoterV0 { args }.to_instruction(accounts::UpdateQuoterV0 {
        quoter: addr(ctx.quoter),
        authority: addr(ctx.authority.pubkey()),
        new_hot_authority: new_hot.map(addr),
    })
}

fn cancel_all_ix(ctx: &Ctx, signer: Pubkey, sides: CancelSidesV0, clear_mid: bool) -> Instruction {
    instruction::CancelAllV0 {
        args: CancelAllArgsV0 { sides, clear_mid },
    }
    .to_instruction(accounts::CancelAllV0 {
        quoter: addr(ctx.quoter),
        authority: addr(signer),
    })
}

/// The `CancelAllOutcomeV0` the instruction returns, off return data:
/// `(bid_rungs, ask_rungs, mid_cleared)`.
fn parse_cancel_all(data: &[u8]) -> (u8, u8, bool) {
    assert_eq!(data.len(), 3, "outcome wire width");
    (data[0], data[1], data[2] == 1)
}

fn quote_levels(ctx: &mut Ctx, direction: Direction, size: u64) -> Vec<(u64, u64)> {
    let ix = quote_ix(ctx, direction, size);
    let meta = send(ctx, ix).unwrap();
    let response = read_response(ctx, &meta);
    parse_levels(&response)
}

fn read_quoter(ctx: &Ctx) -> MidpointQuoterV0 {
    let account = ctx.svm.get_account(&ctx.quoter).unwrap();
    let mut quoter = [0u8; core::mem::size_of::<MidpointQuoterV0>()];
    quoter.copy_from_slice(&account.data[8..8 + core::mem::size_of::<MidpointQuoterV0>()]);
    unsafe { core::mem::transmute::<_, MidpointQuoterV0>(quoter) }
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

    // Higher passes; a later zero is a rejected downgrade, not an opt-out.
    {
        let ix = set_mid_ix(&ctx, MID + 1, 6);
        send(&mut ctx, ix).unwrap();
    }

    {
        let ix = set_mid_ix(&ctx, MID + 2, 0);
        assert!(send(&mut ctx, ix).is_err());
    }

    let quoter: MidpointQuoterV0 = read_quoter(&ctx);
    assert_eq!(quoter.mid_price, MID + 1);
    assert_eq!(quoter.mid_sequence, 6);
}

/// The panic button works on an instance that runs sequences. A maker whose
/// hot key leaks fires `cancel_all_v0` with `clear_mid`, and the whole
/// instruction has to land: a revert here leaves the mid and both ladders
/// live. The withdrawal consumes no sequence, so a later real mid still has
/// to beat the last real one.
#[test]
fn the_panic_button_clears_a_mid_that_carries_a_sequence() {
    let mut ctx = setup();
    arm(&mut ctx);
    let ix = set_mid_ix(&ctx, MID, 5);
    send(&mut ctx, ix).unwrap();

    let ix = cancel_all_ix(&ctx, ctx.authority.pubkey(), CancelSidesV0::Both, true);
    let meta = send(&mut ctx, ix).unwrap();
    assert_eq!(parse_cancel_all(&meta.return_data.data), (2, 2, true));
    let quoter = read_quoter(&ctx);
    assert_eq!(quoter.mid_price, 0);
    assert_eq!((quoter.bid_count, quoter.ask_count), (0, 0));
    // The withdrawal left the sequence where the last real write put it.
    assert_eq!(quoter.mid_sequence, 5);

    // A real mid still has to beat 5.
    let ix = set_mid_ix(&ctx, MID, 5);
    assert!(send(&mut ctx, ix).is_err());
    let ix = set_mid_ix(&ctx, MID, 6);
    send(&mut ctx, ix).unwrap();
    assert_eq!(read_quoter(&ctx).mid_price, MID);
}

/// A writer that races the sequence to the top would make every later mid
/// write fail for the life of the instance. The config key resets the
/// counter, so the maker recovers without a new PDA.
#[test]
fn the_config_key_resets_a_runaway_mid_sequence() {
    let mut ctx = setup();
    arm(&mut ctx);
    let ix = set_mid_ix(&ctx, MID, u64::MAX);
    send(&mut ctx, ix).unwrap();
    // Nothing can beat u64::MAX.
    let ix = set_mid_ix(&ctx, MID + 1, u64::MAX);
    assert!(send(&mut ctx, ix).is_err());

    // The hot key cannot reset it.
    let mut ix = update_ix(
        &ctx,
        UpdateQuoterArgsV0 {
            mid_sequence: Some(0),
            ..Default::default()
        },
        None,
    );

    ix.accounts[1] = AccountMeta::new_readonly(ctx.hot.pubkey(), true);
    assert!(send(&mut ctx, ix).is_err());

    let ix = update_ix(
        &ctx,
        UpdateQuoterArgsV0 {
            mid_sequence: Some(0),
            ..Default::default()
        },
        None,
    );

    send(&mut ctx, ix).unwrap();
    assert_eq!(read_quoter(&ctx).mid_sequence, 0);

    let ix = set_mid_ix(&ctx, MID + 1, 1);
    send(&mut ctx, ix).unwrap();
    let quoter = read_quoter(&ctx);
    assert_eq!((quoter.mid_price, quoter.mid_sequence), (MID + 1, 1));
}

#[test]
fn only_the_hot_authority_writes_mid_and_levels() {
    let mut ctx = setup();
    let mut ix = set_mid_ix(&ctx, MID, 0);
    // Swap the signer for the (also known) config authority: address mismatch.
    ix.accounts[1] = AccountMeta::new_readonly(ctx.authority.pubkey(), true);
    assert!(send(&mut ctx, ix).is_err());

    // Rotate the hot key through update, then the old key fails.
    let new_hot = Keypair::new();
    let ix = update_ix(&ctx, UpdateQuoterArgsV0::default(), Some(new_hot.pubkey()));
    send(&mut ctx, ix).unwrap();
    {
        let ix = set_mid_ix(&ctx, MID, 0);
        assert!(send(&mut ctx, ix).is_err());
    }
}

/// The maker's config key and the quoted wallet are separate identities: the
/// config key reconfigures and cannot receive fills, the quoted wallet
/// receives fills and cannot reconfigure.
#[test]
fn the_config_authority_is_independent_of_the_quoted_wallet() {
    let mut ctx = setup();
    let quoter = read_quoter(&ctx);
    assert_ne!(
        ctx.authority.pubkey().to_bytes(),
        ctx.user_authority.pubkey().to_bytes()
    );

    assert_eq!(quoter.authority, addr(ctx.authority.pubkey()));
    assert_eq!(quoter.user_authority, addr(ctx.user_authority.pubkey()));

    // The quoted wallet cannot reconfigure.
    let mut ix = update_ix(
        &ctx,
        UpdateQuoterArgsV0 {
            is_paused: Some(true),
            ..Default::default()
        },
        None,
    );

    ix.accounts[1] = AccountMeta::new_readonly(ctx.user_authority.pubkey(), true);
    assert!(send(&mut ctx, ix).is_err());

    // The config key can, and the pause takes hold.
    arm(&mut ctx);
    let ix = update_ix(
        &ctx,
        UpdateQuoterArgsV0 {
            is_paused: Some(true),
            ..Default::default()
        },
        None,
    );

    send(&mut ctx, ix).unwrap();
    assert!(quote_levels(&mut ctx, Direction::Long, UNIT).is_empty());
}

/// Creation is still consent: the instance lives at a PDA seeded by the
/// quoted wallet, and that wallet must sign. An operator holding only its own
/// config key cannot stand up an instance quoting somebody else.
#[test]
fn creating_an_instance_needs_the_quoted_wallets_signature() {
    let mut ctx = setup();
    let victim = Keypair::new();
    let squatted = quoter_pda(1, &victim.pubkey(), 0);
    let config = QuoterConfigV0 {
        market_index: 1,
        ..config()
    };
    let mut ix =
        instruction::InitializeQuoterV0 { config }.to_instruction(accounts::InitializeQuoterV0 {
            payer: addr(ctx.payer.pubkey()),
            authority: addr(ctx.authority.pubkey()),
            user_authority: addr(victim.pubkey()),
            execute_authority: addr(ctx.execute_auth.pubkey()),
            hot_authority: addr(ctx.hot.pubkey()),
            quoter: addr(squatted),
            system_program: addr(system_program()),
        });
    // Strip the victim's signer bit: `send` only signs for keys it holds, so
    // this is the squatter's best attempt.
    for meta in ix.accounts.iter_mut() {
        if meta.pubkey.to_bytes() == victim.pubkey().to_bytes() {
            meta.is_signer = false;
        }
    }

    assert!(send(&mut ctx, ix).is_err());
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
    // The fill settles against the quoted wallet, not the config key.
    assert_eq!(authority, ctx.user_authority.pubkey().to_bytes());
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

/// A shrinking rewrite must leave no stale rung behind — the post-write
/// invariant scan requires a zeroed tail, and the book must agree.
#[test]
fn a_shrinking_rewrite_drops_the_tail() {
    let mut ctx = setup();
    send_levels(
        &mut ctx,
        SetLevelsArgsV0 {
            mid: Some(MID),
            sequence: None,
            bids: None,
            asks: Some(levels(&[(1_000, UNIT), (2_000, UNIT), (3_000, UNIT)])),
        },
    )
    .unwrap();
    let ix = execute_ix(&ctx, Direction::Long, UNIT + UNIT / 2);
    send(&mut ctx, ix).unwrap();

    send_levels(
        &mut ctx,
        SetLevelsArgsV0 {
            mid: None,
            sequence: None,
            bids: None,
            asks: Some(levels(&[(5_000, UNIT)])),
        },
    )
    .unwrap();
    let quoter = read_quoter(&ctx);
    assert_eq!(quoter.ask_count, 1);
    assert_eq!(quoter.asks[0].offset_ppm, 5_000);
    for level in &quoter.asks[1..] {
        assert_eq!((level.offset_ppm, level.size, level.filled), (0, 0, 0));
    }

    let asks = quote_levels(&mut ctx, Direction::Long, 10 * UNIT);
    assert_eq!(asks, vec![(100_500_000, UNIT)]);
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
        authority: addr(ctx.user_authority.pubkey()),
        sub_account_id: 0,
    };
    let stranger = UserRefV0 {
        authority: addr(Pubkey::new_unique()),
        sub_account_id: 0,
    };

    // A user set that can't settle the quoted user sees an empty book.
    let ix = quote_ix_with(
        &ctx,
        QuoteArgsV0 {
            taker_served_window: true,
            include_taker_origin_reservations: false,
            users: &[stranger],
            ..quote_args(Direction::Long, UNIT)
        },
    );
    let meta = send(&mut ctx, ix).unwrap();
    assert!(parse_levels(&read_response(&ctx, &meta)).is_empty());

    // The quoted user's own flow sees an empty book (self-trade).
    let ix = quote_ix_with(
        &ctx,
        QuoteArgsV0 {
            taker_served_window: true,
            include_taker_origin_reservations: false,
            caps: UserCapsV0::EMPTY,
            reference_price: 0,
            taker: Some(quoted),
            ..quote_args(Direction::Long, UNIT)
        },
    );
    let meta = send(&mut ctx, ix).unwrap();
    assert!(parse_levels(&read_response(&ctx, &meta)).is_empty());

    // A set including the quoted user quotes normally.
    let ix = quote_ix_with(
        &ctx,
        QuoteArgsV0 {
            taker_served_window: true,
            include_taker_origin_reservations: false,
            users: &[stranger, quoted],
            caps: UserCapsV0::EMPTY,
            reference_price: 0,
            taker: Some(stranger),
            ..quote_args(Direction::Long, UNIT)
        },
    );
    let meta = send(&mut ctx, ix).unwrap();
    assert_eq!(parse_levels(&read_response(&ctx, &meta)).len(), 1);
}

/// A protected-flow instance serves only takers velocity marks as having
/// served a window. The claim rides the wire (`taker_served_window`): this
/// program trusts its caller for it, as it does for `users` and `caps`.
#[test]
fn a_protected_flow_instance_refuses_an_unprotected_taker() {
    let mut ctx = setup_with_config(QuoterConfigV0 {
        require_attested_flow: true,
        ..config()
    });

    arm(&mut ctx);

    // Unprotected flow sees an empty book.
    let ix = quote_ix_with(
        &ctx,
        QuoteArgsV0 {
            taker_served_window: false,
            include_taker_origin_reservations: false,
            ..quote_args(Direction::Long, UNIT)
        },
    );
    let meta = send(&mut ctx, ix).unwrap();
    assert!(parse_levels(&read_response(&ctx, &meta)).is_empty());

    // Protected flow quotes.
    let ix = quote_ix(&ctx, Direction::Long, UNIT);
    let meta = send(&mut ctx, ix).unwrap();
    assert_eq!(parse_levels(&read_response(&ctx, &meta)).len(), 1);

    // Execute is gated the same way.
    let ix = execute_ix_served(&ctx, Direction::Long, UNIT, false);
    let meta = send(&mut ctx, ix).unwrap();
    assert!(parse_execute(&read_response(&ctx, &meta)).is_empty());
    let ix = execute_ix(&ctx, Direction::Long, UNIT);
    let meta = send(&mut ctx, ix).unwrap();
    assert_eq!(parse_execute(&read_response(&ctx, &meta)).len(), 1);
}

/// An instance that does not require protected flow ignores the flag: the
/// wire field constrains nothing unless the maker opted in.
#[test]
fn an_unprotected_instance_ignores_the_flag() {
    let mut ctx = setup();
    arm(&mut ctx);
    let ix = quote_ix_with(
        &ctx,
        QuoteArgsV0 {
            taker_served_window: false,
            include_taker_origin_reservations: false,
            ..quote_args(Direction::Long, UNIT)
        },
    );
    let meta = send(&mut ctx, ix).unwrap();
    assert_eq!(parse_levels(&read_response(&ctx, &meta)).len(), 1);
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
    // More rungs than the ladder holds.
    let ix = set_levels_ix(
        &ctx,
        SetLevelsArgsV0 {
            mid: None,
            sequence: None,
            bids: None,
            asks: Some(levels(
                &(0..65).map(|i| (1_000 + i, UNIT)).collect::<Vec<_>>(),
            )),
        },
    );

    assert!(send(&mut ctx, ix).is_err());
}

#[test]
fn paused_quoter_is_silent() {
    let mut ctx = setup();
    arm(&mut ctx);
    let ix = update_ix(
        &ctx,
        UpdateQuoterArgsV0 {
            is_paused: Some(true),
            ..Default::default()
        },
        None,
    );

    send(&mut ctx, ix).unwrap();
    assert!(quote_levels(&mut ctx, Direction::Long, UNIT).is_empty());
    let meta = {
        let ix = execute_ix(&ctx, Direction::Long, UNIT);
        send(&mut ctx, ix).unwrap()
    };

    assert!(parse_execute(&read_response(&ctx, &meta)).is_empty());
}

#[test]
fn cancel_all_withdraws_the_named_side_and_leaves_the_other_quoting() {
    let mut ctx = setup();
    arm(&mut ctx);
    // A partly-consumed rung is withdrawn like any other.
    let ix = execute_ix(&ctx, Direction::Short, UNIT / 2);
    send(&mut ctx, ix).unwrap();

    let ix = cancel_all_ix(&ctx, ctx.hot.pubkey(), CancelSidesV0::Bids, false);
    let meta = send(&mut ctx, ix).unwrap();
    let (bid_rungs, ask_rungs, mid_cleared) = parse_cancel_all(&meta.return_data.data);
    assert_eq!((bid_rungs, ask_rungs), (2, 0));
    assert!(!mid_cleared);

    // The bid side quotes nothing; the ask side is untouched, mid intact.
    assert!(quote_levels(&mut ctx, Direction::Short, u64::MAX).is_empty());
    assert_eq!(quote_levels(&mut ctx, Direction::Long, u64::MAX).len(), 2);
    let quoter = read_quoter(&ctx);
    assert_eq!((quoter.bid_count, quoter.ask_count), (0, 2));
    assert_eq!(quoter.mid_price, MID);

    // Re-shaping the withdrawn side brings it straight back, with `filled`
    // reset — a withdrawal leaves no residue behind.
    send_levels(
        &mut ctx,
        SetLevelsArgsV0 {
            mid: None,
            sequence: None,
            bids: Some(levels(&[(1_000, UNIT)])),
            asks: None,
        },
    )
    .unwrap();
    assert_eq!(
        quote_levels(&mut ctx, Direction::Short, u64::MAX),
        vec![(99_900_000, UNIT)]
    );
}

#[test]
fn cancel_all_can_take_both_sides_and_the_mid_at_once() {
    let mut ctx = setup();
    arm(&mut ctx);

    let ix = cancel_all_ix(&ctx, ctx.hot.pubkey(), CancelSidesV0::Both, true);
    let meta = send(&mut ctx, ix).unwrap();
    let (bid_rungs, ask_rungs, mid_cleared) = parse_cancel_all(&meta.return_data.data);
    assert_eq!((bid_rungs, ask_rungs), (2, 2));
    assert!(mid_cleared);

    let quoter = read_quoter(&ctx);
    assert_eq!((quoter.bid_count, quoter.ask_count), (0, 0));
    assert_eq!(quoter.mid_price, 0);
    assert!(quote_levels(&mut ctx, Direction::Long, u64::MAX).is_empty());
    assert!(quote_levels(&mut ctx, Direction::Short, u64::MAX).is_empty());

    // A zeroed mid silences the spline even once the ladders are back, so
    // re-arming shape alone can't accidentally resume quoting.
    send_levels(
        &mut ctx,
        SetLevelsArgsV0 {
            mid: None,
            sequence: None,
            bids: Some(levels(&[(1_000, UNIT)])),
            asks: Some(levels(&[(1_000, UNIT)])),
        },
    )
    .unwrap();
    assert!(quote_levels(&mut ctx, Direction::Long, u64::MAX).is_empty());
    let ix = set_mid_ix(&ctx, MID, 0);
    send(&mut ctx, ix).unwrap();
    assert_eq!(quote_levels(&mut ctx, Direction::Long, u64::MAX).len(), 1);
}

/// Both of the maker's keys can withdraw, and nobody else can. The cold key
/// matters because a maker's panic button must work when the hot key is what
/// they no longer trust.
#[test]
fn either_maker_key_can_cancel_all_and_no_one_else() {
    let mut ctx = setup();
    arm(&mut ctx);

    let stranger = Keypair::new();
    ctx.svm.airdrop(&stranger.pubkey(), 1_000_000_000).unwrap();
    let ix = cancel_all_ix(&ctx, stranger.pubkey(), CancelSidesV0::Both, false);
    assert!(send_signed_by(&mut ctx, ix, Some(&stranger)).is_err());
    // The quoted wallet is not a config key either — it consented to being
    // quoted, which is not authority over the shape.
    let ix = cancel_all_ix(
        &ctx,
        ctx.user_authority.pubkey(),
        CancelSidesV0::Both,
        false,
    );

    assert!(send(&mut ctx, ix).is_err());
    assert_eq!(read_quoter(&ctx).bid_count, 2);

    let ix = cancel_all_ix(&ctx, ctx.hot.pubkey(), CancelSidesV0::Bids, false);
    send(&mut ctx, ix).unwrap();
    let ix = cancel_all_ix(&ctx, ctx.authority.pubkey(), CancelSidesV0::Asks, false);
    send(&mut ctx, ix).unwrap();
    let quoter = read_quoter(&ctx);
    assert_eq!((quoter.bid_count, quoter.ask_count), (0, 0));
}

/// Firing it twice is not an error: a maker hitting their kill switch again
/// must not get a failed transaction that reads as "something is wrong".
#[test]
fn cancel_all_is_idempotent() {
    let mut ctx = setup();
    arm(&mut ctx);
    let ix = cancel_all_ix(&ctx, ctx.hot.pubkey(), CancelSidesV0::Both, true);
    send(&mut ctx, ix).unwrap();
    let ix = cancel_all_ix(&ctx, ctx.hot.pubkey(), CancelSidesV0::Both, true);
    let meta = send(&mut ctx, ix).unwrap();
    assert_eq!(parse_cancel_all(&meta.return_data.data), (0, 0, true));
}

/// THE number this program exists for: a mid write must be near the compute
/// floor so makers can track fair value tick-by-tick for ~free. The budget
/// is deliberately above the measured cost (headroom for anchor-v2 drift)
/// but low enough that a regression that adds real work fails loudly.
/// `set_levels` and the quote/execute legs are budgeted the same way — the
/// post-write invariant scans are bounded and must stay cheap.
#[test]
fn set_mid_cu_stays_near_the_floor() {
    let mut ctx = setup();
    arm(&mut ctx);
    let ix = set_mid_ix(&ctx, MID + 50, 0);
    let meta = send(&mut ctx, ix).unwrap();
    let set_mid_cu = meta.compute_units_consumed;

    let full_side = (0..64).map(|i| (1_000 + i, UNIT)).collect::<Vec<_>>();
    let meta = send_levels(
        &mut ctx,
        SetLevelsArgsV0 {
            mid: Some(MID),
            sequence: None,
            bids: Some(levels(&full_side)),
            asks: Some(levels(&full_side)),
        },
    )
    .unwrap();
    let set_levels_cu = meta.compute_units_consumed;

    let thirty_two = (0..32).map(|i| (1_000 + i, UNIT)).collect::<Vec<_>>();
    let cu32_one_side = send_levels(
        &mut ctx,
        SetLevelsArgsV0 {
            mid: None,
            sequence: None,
            bids: Some(levels(&thirty_two)),
            asks: None,
        },
    )
    .unwrap()
    .compute_units_consumed;
    let cu32_both = send_levels(
        &mut ctx,
        SetLevelsArgsV0 {
            mid: Some(MID),
            sequence: None,
            bids: Some(levels(&thirty_two)),
            asks: Some(levels(&thirty_two)),
        },
    )
    .unwrap()
    .compute_units_consumed;
    let one_rung = send_levels(
        &mut ctx,
        SetLevelsArgsV0 {
            mid: None,
            sequence: None,
            bids: Some(levels(&[(1_000, UNIT)])),
            asks: None,
        },
    )
    .unwrap()
    .compute_units_consumed;
    let clear_side = send_levels(
        &mut ctx,
        SetLevelsArgsV0 {
            mid: None,
            sequence: None,
            bids: Some(levels(&[])),
            asks: None,
        },
    )
    .unwrap()
    .compute_units_consumed;
    println!(
        "CU — set_levels(1 rung): {one_rung}, set_levels(clear one side): {clear_side}, \
         set_levels(32 one side): {cu32_one_side}, set_levels(32×2 + mid): {cu32_both}"
    );

    let ix = quote_ix(&ctx, Direction::Long, u64::MAX);
    let quote_cu = send(&mut ctx, ix).unwrap().compute_units_consumed;
    let ix = execute_ix(&ctx, Direction::Long, u64::MAX);
    let execute_cu = send(&mut ctx, ix).unwrap().compute_units_consumed;

    println!(
        "CU — set_mid: {set_mid_cu}, set_levels(64×2 + mid): {set_levels_cu}, \
         quote(64 rungs): {quote_cu}, execute(64 rungs): {execute_cu}"
    );

    assert!(set_mid_cu <= 800, "set_mid regressed: {set_mid_cu} CU");
    assert!(
        set_levels_cu <= 10_000,
        "set_levels regressed: {set_levels_cu} CU"
    );
    assert!(quote_cu <= 18_000, "quote regressed: {quote_cu} CU");
    assert!(execute_cu <= 32_000, "execute regressed: {execute_cu} CU");
}

/// Withdrawing quotes is the other move a maker makes under time pressure, so
/// it is budgeted like the mid write.
///
/// Two things are pinned, both against the way this was done before
/// (`set_levels_v0` with empty sides, plus a separate `set_mid_v0` to stop the
/// spline outright): a one-sided withdrawal, and the atomic full stop. The cost
/// must also track the rungs actually pulled rather than the ladder's capacity,
/// which is what makes a two-rung desk cheap.
#[test]
fn cancel_all_cu_beats_the_set_levels_it_replaces() {
    let full_side = (0..64).map(|i| (1_000 + i, UNIT)).collect::<Vec<_>>();
    let shapes: [(&str, &[(u64, u64)]); 3] = [
        ("empty", &[]),
        ("2 rungs/side", &[(1_000, UNIT), (3_000, UNIT)]),
        ("64 rungs/side", &full_side),
    ];
    let mut one_side = Vec::new();
    for (label, side) in shapes {
        // Baseline: clearing both sides the old way, two `set_levels_v0` args
        // and a full both-ladder re-scan.
        let mut ctx = setup();
        send_levels(
            &mut ctx,
            SetLevelsArgsV0 {
                mid: Some(MID),
                sequence: None,
                bids: Some(levels(side)),
                asks: Some(levels(side)),
            },
        )
        .unwrap();
        let set_levels_cu = send_levels(
            &mut ctx,
            SetLevelsArgsV0 {
                mid: None,
                sequence: None,
                bids: Some(levels(&[])),
                asks: Some(levels(&[])),
            },
        )
        .unwrap()
        .compute_units_consumed;
        // ...and the one-sided baseline, for the one-sided comparison.
        let mut ctx = setup();
        send_levels(
            &mut ctx,
            SetLevelsArgsV0 {
                mid: Some(MID),
                sequence: None,
                bids: Some(levels(side)),
                asks: Some(levels(side)),
            },
        )
        .unwrap();
        let set_levels_one_side_cu = send_levels(
            &mut ctx,
            SetLevelsArgsV0 {
                mid: None,
                sequence: None,
                bids: Some(levels(&[])),
                asks: None,
            },
        )
        .unwrap()
        .compute_units_consumed;

        let mut ctx = setup();
        send_levels(
            &mut ctx,
            SetLevelsArgsV0 {
                mid: Some(MID),
                sequence: None,
                bids: Some(levels(side)),
                asks: Some(levels(side)),
            },
        )
        .unwrap();
        // One side only, which is what a maker pulling a single quote pays.
        let ix = cancel_all_ix(&ctx, ctx.hot.pubkey(), CancelSidesV0::Bids, false);
        let one_side_cu = send(&mut ctx, ix).unwrap().compute_units_consumed;

        // The like-for-like against the baseline: both sides, mid untouched.
        let mut ctx = setup();
        send_levels(
            &mut ctx,
            SetLevelsArgsV0 {
                mid: Some(MID),
                sequence: None,
                bids: Some(levels(side)),
                asks: Some(levels(side)),
            },
        )
        .unwrap();
        let ix = cancel_all_ix(&ctx, ctx.hot.pubkey(), CancelSidesV0::Both, false);
        let both_cu = send(&mut ctx, ix).unwrap().compute_units_consumed;
        // The same sweep taking the mid with it — the atomic full stop, whose
        // baseline is `set_levels(clear both)` *plus* a `set_mid(0)`, since
        // clearing the ladders alone leaves the spline armed.
        let mut ctx = setup();
        send_levels(
            &mut ctx,
            SetLevelsArgsV0 {
                mid: Some(MID),
                sequence: None,
                bids: Some(levels(side)),
                asks: Some(levels(side)),
            },
        )
        .unwrap();
        let ix = cancel_all_ix(&ctx, ctx.hot.pubkey(), CancelSidesV0::Both, true);
        let both_and_mid_cu = send(&mut ctx, ix).unwrap().compute_units_consumed;
        let ix = set_mid_ix(&ctx, 0, 0);
        let set_mid_cu = send(&mut ctx, ix).unwrap().compute_units_consumed;
        let full_stop_baseline = set_levels_cu + set_mid_cu;

        println!(
            "CU — cancel_all({label}): one side {one_side_cu}, both {both_cu}, \
             both + mid {both_and_mid_cu}; baseline: set_levels(clear one) \
             {set_levels_one_side_cu}, set_levels(clear both) {set_levels_cu}, \
             + set_mid(0) = {full_stop_baseline}"
        );

        assert!(
            one_side_cu < set_levels_one_side_cu,
            "{label}: withdrawing one side ({one_side_cu}) must beat clearing it via \
             set_levels ({set_levels_one_side_cu}) — it writes only the live rungs and \
             re-checks only the side it pulled"
        );
        assert!(
            both_and_mid_cu < full_stop_baseline,
            "{label}: the atomic full stop ({both_and_mid_cu}) must beat the two \
             instructions it replaces ({full_stop_baseline})"
        );
        assert!(
            one_side_cu < both_cu,
            "{label}: withdrawing one side ({one_side_cu}) must cost less than both \
             ({both_cu}) — the cost is meant to track the rungs actually written"
        );

        // Two-sided vs set_levels costs cross around 50 rungs;
        // only one-sided withdrawal and atomic stop scale predictably.
        one_side.push(one_side_cu);
    }

    // The design property: cost tracks the rungs actually pulled rather than the
    // ladder's capacity, which is what makes a normal desk's shape cheap. A flat
    // cost here would mean the withdrawal had stopped being proportional.
    let [empty, shallow, deep] = one_side[..] else {
        panic!("one measurement per shape");
    };

    assert!(
        empty < shallow && shallow * 2 < deep,
        "withdrawal cost must scale with the rungs pulled, got {empty} (none), \
         {shallow} (2 rungs), {deep} (64 rungs)"
    );
}

/// Creation refuses a config a fill could never settle, and one that would
/// leave the maker unprotected.
///
/// A base denominator other than the constant prices the instance's quotes on
/// one scale while velocity settles them on another, so every routed fill
/// reverts. A zero deviation band lets a compromised hot key fill the maker at
/// any mid it likes, and an instance must not run that way before a separate
/// call arrives.
#[test]
fn creation_refuses_a_foreign_denominator_and_a_missing_deviation_band() {
    let mut ctx = setup();
    let create = |ctx: &mut Ctx, wallet: &Keypair, config: QuoterConfigV0| {
        let quoter = quoter_pda(config.market_index, &wallet.pubkey(), 0);
        let ix = instruction::InitializeQuoterV0 { config }.to_instruction(
            accounts::InitializeQuoterV0 {
                payer: addr(ctx.payer.pubkey()),
                authority: addr(ctx.authority.pubkey()),
                user_authority: addr(wallet.pubkey()),
                execute_authority: addr(ctx.execute_auth.pubkey()),
                hot_authority: addr(ctx.hot.pubkey()),
                quoter: addr(quoter),
                system_program: addr(system_program()),
            },
        );

        send_signed_by(ctx, ix, Some(wallet))
    };

    let wallet = Keypair::new();
    assert!(create(
        &mut ctx,
        &wallet,
        QuoterConfigV0 {
            market_index: 1,
            base_precision: 1_000_000,
            ..config()
        }
    )
    .is_err());
    assert!(create(
        &mut ctx,
        &wallet,
        QuoterConfigV0 {
            market_index: 2,
            base_precision: 0,
            ..config()
        }
    )
    .is_err());
    assert!(create(
        &mut ctx,
        &wallet,
        QuoterConfigV0 {
            market_index: 3,
            max_mid_deviation_ppm: 0,
            ..config()
        }
    )
    .is_err());
    create(
        &mut ctx,
        &wallet,
        QuoterConfigV0 {
            market_index: 4,
            ..config()
        },
    )
    .expect("the constant and a real band are accepted");
}

/// The band is live the moment the instance exists, with no update call. A
/// mid a long way off the caller's reference quotes and fills nothing.
#[test]
fn a_fresh_instance_already_refuses_an_off_market_mid() {
    let mut ctx = setup();
    arm(&mut ctx);
    assert_eq!(read_quoter(&ctx).max_mid_deviation_ppm, 10_000);

    // A reference within the one percent band quotes normally.
    let ix = quote_ix_with(
        &ctx,
        QuoteArgsV0 {
            reference_price: MID as i64,
            ..quote_args(Direction::Long, UNIT)
        },
    );
    let meta = send(&mut ctx, ix).unwrap();
    assert_eq!(parse_levels(&read_response(&ctx, &meta)).len(), 1);

    // A reference two percent away quotes nothing.
    let off = (MID + MID / 50) as i64;
    let ix = quote_ix_with(
        &ctx,
        QuoteArgsV0 {
            reference_price: off,
            ..quote_args(Direction::Long, UNIT)
        },
    );
    let meta = send(&mut ctx, ix).unwrap();
    assert!(parse_levels(&read_response(&ctx, &meta)).is_empty());

    // And fills nothing.
    let ix = instruction::ExecuteV0 {
        args: ExecuteArgsV0 {
            taker_served_window: true,
            include_taker_origin_reservations: false,
            direction: Direction::Long,
            size: UNIT,
            users: &[],
            caps: UserCapsV0::EMPTY,
            reference_price: off,
            taker: None,
        },
    }
    .to_instruction(accounts::ExecuteV0 {
        quoter: addr(ctx.quoter),
        execute_authority: addr(ctx.execute_auth.pubkey()),
    });

    let meta = send(&mut ctx, ix).unwrap();
    assert!(parse_execute(&read_response(&ctx, &meta)).is_empty());
}
