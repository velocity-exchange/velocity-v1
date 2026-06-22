//! SOL-PERP trade generator + smoke test.
//!
//! Uses two wallets — a taker (`PRIVATE_KEY`) and a maker. The maker comes from
//! `MAKER_PRIVATE_KEY`, else a persisted `maker-keypair.json`, else a freshly
//! generated keypair (persisted). Each wallet trades from its own sub-account 0.
//!
//! Setup (opt-in, `SETUP=1`): the taker tops the maker up with SOL, then each
//! wallet creates its dUSDT ATA, faucet-mints into it, initializes its
//! sub-account, and deposits dUSDT as margin collateral.
//!
//! Swift (one-shot, skip with `SKIP_SWIFT=1`): bootstraps a maker subscriber
//! and a taker that posts to the swift HTTP endpoint. Verifies WS connectivity,
//! order acceptance, and a fill attempt.
//!
//! DLOB (looped): every `INTERVAL_SECS` (default 30) the maker places a resting
//! post-only limit order and the taker crosses it with a `place_and_take` limit
//! order, exercising both sides of the book (taker buys / maker sells, then
//! taker sells / maker buys — returning both accounts to flat). Runs forever
//! unless `ITERATIONS` is set.
//!
//! Required env:
//!   PRIVATE_KEY        - base58 taker keypair
//!
//! Optional env:
//!   MAKER_PRIVATE_KEY  - base58 maker keypair (default: generated + persisted)
//!   RPC_URL            - Solana RPC (default: https://api.devnet.solana.com)
//!   MAINNET            - if set, use mainnet swift/rpc/faucet endpoints
//!   SWIFT_URL          - override the swift WS/HTTP base URL
//!   SETUP              - if set, fund maker SOL + init + faucet-fund + deposit
//!   SKIP_SWIFT         - if set, skip the one-shot swift smoke
//!   INTERVAL_SECS      - seconds between DLOB rounds (default 30)
//!   ITERATIONS         - number of DLOB rounds, 0 = forever (default 0)
//!   FAUCET_AMOUNT      - whole dUSDT to faucet-mint per account (default 1_000_000)
//!   DEPOSIT_AMOUNT     - whole dUSDT to deposit per account (default 100_000)
//!   MAKER_SOL_LAMPORTS - SOL (lamports) to top the maker up to (default 150_000_000)
//!
//! The taker must hold enough SOL to fund itself and top up the maker.

use std::borrow::Cow;
use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use futures_util::StreamExt;
use nanoid::nanoid;
use reqwest::header;
use solana_pubkey::Pubkey;
use velocity_rs::{
    constants::{ASSOCIATED_TOKEN_PROGRAM_ID, SYSTEM_PROGRAM_ID},
    swift_order_subscriber::{SignedOrderInfo, SignedOrderType},
    types::{
        accounts::User,
        solana_sdk::{
            instruction::{AccountMeta, Instruction},
            keypair::Keypair,
        },
        MarketType, OrderParams, OrderParamsExt, OrderStatus, OrderType, PositionDirection,
        PostOnlyParam, SignedMsgOrderParamsMessage, SpotMarketExt,
    },
    Context, RpcClient, TransactionBuilder, VelocityClient, Wallet,
};

// token_faucet program IDs (mint_to_user lets anyone mint the test quote token).
const FAUCET_PROGRAM_DEVNET: Pubkey =
    Pubkey::from_str_const("V4v1mQiAdLz4qwckEb45WqHYceYizoib39cDBHSWfaB");
const FAUCET_PROGRAM_MAINNET: Pubkey =
    Pubkey::from_str_const("AmNeSW4UMPFBodCjEJD22G3kA8EraUGkhxr3GmdyEF4f");
// Anchor discriminator for token_faucet `mint_to_user` (sha256("global:mint_to_user")[..8]).
const MINT_TO_USER_DISC: [u8; 8] = [75, 194, 44, 77, 10, 65, 232, 85];
// dUSDT is the quote spot market.
const QUOTE_MARKET_INDEX: u16 = 0;
// Where a generated maker keypair is persisted (so it's stable across runs).
const MAKER_KEY_FILE: &str = "maker-keypair.json";
// Default SOL (lamports) the taker tops the maker up to during setup, so the
// maker can pay rent for its own user/stats accounts and ongoing tx fees.
const MAKER_TOPUP_LAMPORTS: u64 = 150_000_000; // 0.15 SOL

// 0.1 SOL contract in BASE_PRECISION (10^9)
const BASE_AMOUNT: u64 = 100_000_000;
// Oracle price offsets in PRICE_PRECISION (10^6). Both are tiny (< $0.01)
// so the taker order is essentially at oracle price.
const AUCTION_START: i64 = 100;
const AUCTION_END: i64 = 1_000;
const AUCTION_SLOTS: u8 = 30;
// How long to wait for the maker to receive and fill the order.
const FILL_TIMEOUT_SECS: u64 = 20;

#[tokio::main]
async fn main() {
    let _ = env_logger::init();
    let _ = dotenv::dotenv();

    let context = if std::env::var("MAINNET").is_ok() {
        Context::MainNet
    } else {
        Context::DevNet
    };
    let rpc_url =
        std::env::var("RPC_URL").unwrap_or_else(|_| "https://api.devnet.solana.com".to_string());

    let taker_wallet: Wallet = velocity_rs::utils::load_keypair_multi_format(
        &std::env::var("PRIVATE_KEY").expect("PRIVATE_KEY env var required"),
    )
    .expect("valid taker keypair")
    .into();
    let maker_wallet = resolve_maker_wallet();

    // Each wallet trades from its own sub-account 0 (independent authorities →
    // independent user_stats).
    let taker_subaccount = taker_wallet.default_sub_account();
    let maker_subaccount = maker_wallet.default_sub_account();

    let do_setup = env_flag("SETUP");
    let skip_swift = env_flag("SKIP_SWIFT");
    let interval_secs = env_u64("INTERVAL_SECS", 30);
    let iterations = env_u64("ITERATIONS", 0); // 0 = forever

    println!("=== Velocity SOL-PERP trade generator ===");
    println!("context:   {:?}", context);
    println!(
        "taker:     {}  (sub {})",
        taker_wallet.authority(),
        taker_subaccount
    );
    println!(
        "maker:     {}  (sub {})",
        maker_wallet.authority(),
        maker_subaccount
    );
    println!(
        "interval:  {interval_secs}s   iterations: {}",
        if iterations == 0 {
            "∞".to_string()
        } else {
            iterations.to_string()
        }
    );

    // Single backend (RPC + subscriptions) shared by both wallets; the maker
    // client is a clone with the maker wallet swapped in for signing.
    let velocity = VelocityClient::new(context, RpcClient::new(rpc_url), taker_wallet)
        .await
        .expect("VelocityClient initialized");
    let mut maker = velocity.clone();
    maker.wallet = maker_wallet;

    velocity
        .subscribe_blockhashes()
        .await
        .expect("blockhash subscription");
    // NB: we deliberately do NOT subscribe_account the sub-accounts. When an
    // account is subscribed, get_user_account returns the WS-cached value and
    // never falls back to RPC — which goes stale for the maker→taker handoff
    // below. Leaving them unsubscribed makes every read a fresh RPC fetch.

    let market_id = velocity.market_lookup("sol-perp").expect("sol-perp market");
    println!("market:    SOL-PERP (index {})\n", market_id.index());

    // ── optional setup: fund maker SOL + init accounts + faucet dUSDT + deposit ─
    if do_setup {
        setup_accounts(
            &velocity,
            &maker,
            context,
            taker_subaccount,
            maker_subaccount,
        )
        .await;
    }

    // ── one-shot swift smoke (optional) ───────────────────────────────────────
    if !skip_swift {
        run_swift_round(
            &velocity,
            &maker,
            context,
            market_id,
            taker_subaccount,
            maker_subaccount,
        )
        .await;
    }

    // ── continuous DLOB trade generation ──────────────────────────────────────
    println!("\n=== DLOB trade generation (both sides every {interval_secs}s) ===");
    let mut round: u64 = 0;
    loop {
        round += 1;
        println!("\n── round {round} ──");
        run_dlob_round(
            &velocity,
            &maker,
            taker_subaccount,
            maker_subaccount,
            market_id,
        )
        .await;

        if iterations != 0 && round >= iterations {
            break;
        }
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
    }

    println!("\n=== done ===");
}

/// Resolve the maker wallet: `MAKER_PRIVATE_KEY` env, else a persisted
/// `maker-keypair.json`, else generate one and persist it.
fn resolve_maker_wallet() -> Wallet {
    if let Ok(key) = std::env::var("MAKER_PRIVATE_KEY") {
        return velocity_rs::utils::load_keypair_multi_format(&key)
            .expect("valid MAKER_PRIVATE_KEY")
            .into();
    }
    if Path::new(MAKER_KEY_FILE).exists() {
        return velocity_rs::utils::load_keypair_multi_format(MAKER_KEY_FILE)
            .expect("valid maker keypair file")
            .into();
    }
    let keypair = Keypair::new();
    let bytes = keypair.to_bytes().to_vec();
    std::fs::write(
        MAKER_KEY_FILE,
        serde_json::to_string(&bytes).expect("serialize keypair"),
    )
    .expect("write maker keypair file");
    let wallet: Wallet = keypair.into();
    println!(
        "generated maker keypair → {MAKER_KEY_FILE} (authority {})",
        wallet.authority()
    );
    wallet
}

/// Run a single swift round: subscribe as maker, post a taker oracle order to
/// the swift endpoint, and wait for the maker to fill it on-chain.
async fn run_swift_round(
    velocity: &VelocityClient,
    maker: &VelocityClient,
    context: Context,
    market_id: velocity_rs::types::MarketId,
    taker_subaccount: Pubkey,
    maker_subaccount: Pubkey,
) {
    println!("=== Swift order flow ===");

    // ── 1. start maker subscriber ────────────────────────────────────────────
    println!("[1/3] Starting maker subscriber...");

    let (fill_tx, fill_rx) = tokio::sync::oneshot::channel::<Option<String>>();

    let swift_ws_override = std::env::var("SWIFT_URL").ok();
    let sub_result = velocity
        .subscribe_swift_orders(&[market_id], Some(true), None, swift_ws_override)
        .await;

    match sub_result {
        Ok(mut stream) => {
            println!("  ✓ subscribed to sol-perp swift orders");
            let velocity_maker = maker.clone();
            tokio::spawn(async move {
                while let Some(order) = stream.next().await {
                    println!("  ✓ maker received order: {}", order.order_uuid_str());
                    let sig = try_fill(velocity_maker.clone(), maker_subaccount, order).await;
                    let _ = fill_tx.send(sig);
                    break; // one fill is enough
                }
            });
        }
        Err(e) => {
            println!("  ✗ maker subscription failed: {e}");
            // fill_tx dropped → fill_rx will immediately error
        }
    }

    // brief pause for the WS subscription to propagate on the server side
    tokio::time::sleep(Duration::from_millis(800)).await;

    // ── 2. taker posts swift order ───────────────────────────────────────────
    println!("\n[2/3] Placing taker swift order on SOL-PERP...");

    let slot = velocity.rpc().get_slot().await.expect("get slot") + 200;
    let uuid: [u8; 8] = nanoid!(8).as_bytes().try_into().expect("8 char nanoid");

    let order_params = OrderParams {
        market_index: market_id.index(),
        market_type: MarketType::Perp,
        order_type: OrderType::Oracle,
        base_asset_amount: BASE_AMOUNT,
        direction: PositionDirection::Long,
        auction_start_price: Some(AUCTION_START),
        auction_end_price: Some(AUCTION_END),
        auction_duration: Some(AUCTION_SLOTS),
        ..Default::default()
    };

    let msg = SignedMsgOrderParamsMessage {
        sub_account_id: 0,
        signed_msg_order_params: order_params,
        slot,
        uuid,
        take_profit_order_params: None,
        stop_loss_order_params: None,
        max_margin_ratio: None,
        builder_idx: None,
        builder_fee_tenth_bps: None,
        isolated_position_deposit: None,
    };

    let signed_msg_hex = hex::encode(SignedOrderType::authority(msg).to_borsh());
    let signature = velocity
        .wallet
        .sign_message(signed_msg_hex.as_bytes())
        .expect("sign");

    let swift_http_url = swift_http_url(context, std::env::var("SWIFT_URL").ok().as_deref());
    let payload = serde_json::json!({
        "message": signed_msg_hex,
        "taker_authority": velocity.wallet.authority().to_string(),
        "taker_pubkey": taker_subaccount.to_string(),
        "signature": base64::prelude::BASE64_STANDARD.encode(signature.as_ref()),
    });

    println!("  posting to {swift_http_url}");
    let resp = reqwest::Client::new()
        .post(&swift_http_url)
        .header(header::CONTENT_TYPE, "application/json")
        .json(&payload)
        .send()
        .await;

    match resp {
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            if status.is_success() {
                println!("  ✓ order accepted ({status}): {body}");
            } else {
                println!("  ✗ order rejected ({status}): {body}");
            }
        }
        Err(e) => println!("  ✗ HTTP error: {e}"),
    }

    // ── 3. wait for maker fill ───────────────────────────────────────────────
    println!("\n[3/3] Waiting up to {FILL_TIMEOUT_SECS}s for maker to fill...");

    match tokio::time::timeout(Duration::from_secs(FILL_TIMEOUT_SECS), fill_rx).await {
        Ok(Ok(Some(sig))) => println!("  ✓ fill tx sent: {sig}"),
        Ok(Ok(None)) => println!("  ✗ fill tx failed (check logs above)"),
        Ok(Err(_)) => println!("  ✗ maker task ended before fill (subscription error?)"),
        Err(_) => {
            println!("  ⚠ timeout ({FILL_TIMEOUT_SECS}s) — no fill received");
            println!("    (order may have expired, or maker sub-account 1 needs collateral)");
        }
    }
}

/// Run one DLOB round: cross a resting maker limit order with a taker
/// `place_and_take` on both sides of the book (returns both sub-accounts flat).
async fn run_dlob_round(
    taker: &VelocityClient,
    maker: &VelocityClient,
    taker_subaccount: Pubkey,
    maker_subaccount: Pubkey,
    market_id: velocity_rs::types::MarketId,
) {
    match tokio::try_join!(
        taker.oracle_price(market_id),
        taker.get_perp_market_account(market_id.index()),
    ) {
        Ok((oracle, perp_market)) => {
            let tick = perp_market.order_tick_size.max(1);
            let step = perp_market.order_step_size.max(1);
            // snap the base size down to the market's step (>= one step)
            let base = ((BASE_AMOUNT / step) * step).max(step);
            println!(
                "  oracle=${:.4}  tick={}  step={}  base={}",
                oracle as f64 / 1e6,
                tick,
                step,
                base,
            );

            // taker buys / maker sells, then taker sells / maker buys (returns flat)
            let bought = cross_one_side(
                taker,
                maker,
                taker_subaccount,
                maker_subaccount,
                market_id.index(),
                PositionDirection::Long,
                oracle,
                tick,
                base,
            )
            .await;
            let sold = cross_one_side(
                taker,
                maker,
                taker_subaccount,
                maker_subaccount,
                market_id.index(),
                PositionDirection::Short,
                oracle,
                tick,
                base,
            )
            .await;

            println!(
                "  DLOB results: buy-side {}, sell-side {}",
                if bought { "✓ filled" } else { "✗ no fill" },
                if sold { "✓ filled" } else { "✗ no fill" },
            );
        }
        Err(e) => println!("  ✗ could not load oracle/market for DLOB round: {e}"),
    }
}

/// Bootstrap both trading accounts:
///
/// 1. taker tops the maker authority up with SOL (so it can pay its own rent/fees);
/// 2. each side creates its dUSDT ATA, faucet-mints into it, inits sub-account 0
///    (if missing) and deposits dUSDT — signed by, and confirmed against, its own
///    wallet.
async fn setup_accounts(
    taker: &VelocityClient,
    maker: &VelocityClient,
    context: Context,
    taker_sub: Pubkey,
    maker_sub: Pubkey,
) {
    println!("=== Setup: fund maker SOL + initialize accounts + fund dUSDT ===");

    // 1. top up the maker authority with SOL from the taker, if it's low
    let maker_authority = *maker.wallet.authority();
    let topup = env_u64("MAKER_SOL_LAMPORTS", MAKER_TOPUP_LAMPORTS);
    let balance = taker.rpc().get_balance(&maker_authority).await.unwrap_or(0);
    if balance < topup {
        let amount = topup - balance;
        println!(
            "  funding maker {maker_authority} with {:.4} SOL",
            amount as f64 / 1e9
        );
        let tx = new_authority_builder(taker, *taker.wallet.authority(), taker_sub)
            .add_ix(system_transfer_ix(
                taker.wallet.authority(),
                &maker_authority,
                amount,
            ))
            .build();
        send_confirm(taker, tx, "maker SOL top-up").await;
    } else {
        println!(
            "  maker already funded ({:.4} SOL ≥ {:.4})",
            balance as f64 / 1e9,
            topup as f64 / 1e9
        );
    }

    // 2. set up each side from its own wallet
    setup_one_side(taker, context, taker_sub, "taker").await;
    setup_one_side(maker, context, maker_sub, "maker").await;
    println!();
}

/// Fund + initialize one trading account from `client`'s wallet: create the
/// dUSDT ATA, faucet-mint into it, then init sub-account 0 (if missing) and
/// deposit dUSDT. Each step is confirmed before the next so ordering holds.
async fn setup_one_side(client: &VelocityClient, context: Context, sub: Pubkey, label: &str) {
    let authority = *client.wallet.authority();
    let spot = client
        .program_data()
        .spot_market_config_by_index(QUOTE_MARKET_INDEX)
        .expect("quote spot market synced");
    let mint = spot.mint;
    let token_program = spot.token_program();
    let ata = Wallet::derive_associated_token_address(&authority, spot);

    let unit = 10u64.pow(spot.decimals);
    let mint_amount = env_u64("FAUCET_AMOUNT", 1_000_000).saturating_mul(unit);
    let deposit_whole = env_u64("DEPOSIT_AMOUNT", 100_000);
    let deposit_amount = deposit_whole.saturating_mul(unit);

    // create ATA (idempotent) + faucet-mint into it
    let fund_tx = new_authority_builder(client, authority, sub)
        .add_ix(create_ata_idempotent_ix(
            &authority,
            &ata,
            &mint,
            &token_program,
        ))
        .add_ix(faucet_mint_to_user_ix(
            context,
            &mint,
            &ata,
            &token_program,
            mint_amount,
        ))
        .build();
    send_confirm(client, fund_tx, &format!("{label} faucet fund")).await;

    // init sub-account 0 (if missing) + deposit dUSDT
    let exists = client.get_user_account(&sub).await.is_ok();
    let mut builder = new_authority_builder(client, authority, sub);
    if !exists {
        builder = builder.initialize_user_account(0, None, None);
    }
    builder = builder.deposit(deposit_amount, QUOTE_MARKET_INDEX, None, None);
    let action = if exists { "deposit" } else { "init + deposit" };
    send_confirm(
        client,
        builder.build(),
        &format!("{label} {action} ({deposit_whole} dUSDT)"),
    )
    .await;
}

/// Sign+send a tx and poll until it confirms (or times out). `sign_and_send`
/// is fire-and-forget, so this is needed wherever a later step depends on the
/// result landing on-chain.
async fn send_confirm(
    client: &VelocityClient,
    tx: velocity_rs::types::VersionedMessage,
    label: &str,
) -> bool {
    match client.sign_and_send(tx).await {
        Ok(sig) => {
            for _ in 0..40 {
                if let Ok(true) = client.rpc().confirm_transaction(&sig).await {
                    println!("  ✓ {label}: {sig}");
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(750)).await;
            }
            println!("  ⚠ {label} sent but unconfirmed: {sig}");
            true
        }
        Err(e) => {
            println!("  ✗ {label} failed: {e}");
            false
        }
    }
}

/// System-program `Transfer` instruction (variant index 2 + u64 lamports LE).
fn system_transfer_ix(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = vec![2u8, 0, 0, 0];
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: SYSTEM_PROGRAM_ID,
        accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
        data,
    }
}

/// Build a `TransactionBuilder` for the wallet authority against `sub_account`,
/// without requiring the sub-account to exist yet (used for init/funding txs).
fn new_authority_builder(
    velocity: &VelocityClient,
    authority: Pubkey,
    sub_account: Pubkey,
) -> TransactionBuilder<'_> {
    let user = User {
        authority,
        ..Default::default()
    };
    TransactionBuilder::new(
        velocity.program_data(),
        sub_account,
        Cow::Owned(user),
        false,
    )
}

/// Associated-token-account `CreateIdempotent` instruction.
fn create_ata_idempotent_ix(
    payer: &Pubkey,
    ata: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: ASSOCIATED_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(*ata, false),
            AccountMeta::new_readonly(*payer, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1], // 1 = CreateIdempotent
    }
}

/// token_faucet `mint_to_user` instruction — mints `amount` to `user_token_account`.
fn faucet_mint_to_user_ix(
    context: Context,
    mint: &Pubkey,
    user_token_account: &Pubkey,
    token_program: &Pubkey,
    amount: u64,
) -> Instruction {
    let program_id = match context {
        Context::MainNet => FAUCET_PROGRAM_MAINNET,
        _ => FAUCET_PROGRAM_DEVNET,
    };
    let faucet_config =
        Pubkey::find_program_address(&[b"faucet_config", mint.as_ref()], &program_id).0;
    let mint_authority =
        Pubkey::find_program_address(&[b"mint_authority", mint.as_ref()], &program_id).0;

    let mut data = MINT_TO_USER_DISC.to_vec();
    data.extend_from_slice(&amount.to_le_bytes());

    Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(faucet_config, false),
            AccountMeta::new(*mint, false),
            AccountMeta::new(*user_token_account, false),
            AccountMeta::new_readonly(mint_authority, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data,
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok()
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Place a resting post-only maker limit order then cross it with a taker
/// `place_and_take` limit order on one side of the book.
///
/// `taker_direction` is the side the taker takes; the maker posts the opposite.
/// The maker rests one passive offset away from oracle and the taker prices
/// through it so the cross matches the maker (not the AMM). Returns whether the
/// taker fill landed on-chain.
async fn cross_one_side(
    taker: &VelocityClient,
    maker: &VelocityClient,
    taker_sub: Pubkey,
    maker_sub: Pubkey,
    market_index: u16,
    taker_direction: PositionDirection,
    oracle: i64,
    tick: u64,
    base: u64,
) -> bool {
    let label = match taker_direction {
        PositionDirection::Long => "taker BUY / maker SELL",
        PositionDirection::Short => "taker SELL / maker BUY",
    };
    println!("\n  ── side: {label} ──");

    let maker_direction = match taker_direction {
        PositionDirection::Long => PositionDirection::Short,
        PositionDirection::Short => PositionDirection::Long,
    };

    // passive offset: 0.1% of oracle, snapped to tick (at least one tick)
    let buffer = round_to_tick((oracle / 1000).max(tick as i64), tick) as i64;
    // maker rests one buffer away (passive); taker prices two buffers through it
    let (maker_price, taker_price) = match taker_direction {
        PositionDirection::Long => (
            round_to_tick(oracle + buffer, tick),
            round_to_tick(oracle + 2 * buffer, tick),
        ),
        PositionDirection::Short => (
            round_to_tick(oracle - buffer, tick),
            round_to_tick(oracle - 2 * buffer, tick),
        ),
    };
    println!(
        "  maker {:?}@{:.4}  taker {:?}@{:.4}",
        maker_direction,
        maker_price as f64 / 1e6,
        taker_direction,
        taker_price as f64 / 1e6,
    );

    // 1. maker posts a resting post-only limit order
    let maker_order = OrderParams {
        order_type: OrderType::Limit,
        market_index,
        market_type: MarketType::Perp,
        direction: maker_direction,
        base_asset_amount: base,
        price: maker_price,
        post_only: PostOnlyParam::MustPostOnly,
        ..Default::default()
    };
    let place_tx = match maker.init_tx(&maker_sub, false).await {
        Ok(builder) => builder.place_orders(vec![maker_order]).build(),
        Err(e) => {
            println!("  ✗ maker init_tx failed: {e}");
            return false;
        }
    };
    if !send_confirm(maker, place_tx, "maker resting order").await {
        return false;
    }

    // 2. wait for the resting order to land on-chain (so place_and_take can match it)
    let maker_account =
        match wait_for_resting_order(maker, &maker_sub, market_index, maker_direction).await {
            Some(acc) => acc,
            None => {
                println!("  ✗ maker order never appeared on-chain");
                cancel_market_orders(maker, &maker_sub, market_index).await;
                return false;
            }
        };

    // 3. taker crosses with a limit IOC place_and_take against the maker
    let taker_order = OrderParams {
        order_type: OrderType::Limit,
        market_index,
        market_type: MarketType::Perp,
        direction: taker_direction,
        base_asset_amount: base,
        price: taker_price,
        bit_flags: OrderParams::IMMEDIATE_OR_CANCEL_FLAG,
        ..Default::default()
    };
    let filled = match taker.init_tx(&taker_sub, false).await {
        Ok(builder) => {
            let tx = builder
                .place_and_take(taker_order, &[(maker_sub, maker_account)], None, None)
                .build();
            match taker.sign_and_send(tx).await {
                Ok(sig) => {
                    println!("  ✓ taker crossing fill: {sig}");
                    true
                }
                Err(e) => {
                    println!("  ✗ taker place_and_take failed: {e}");
                    false
                }
            }
        }
        Err(e) => {
            println!("  ✗ taker init_tx failed: {e}");
            false
        }
    };

    // 4. clear any residual maker order (e.g. if the cross missed)
    cancel_market_orders(maker, &maker_sub, market_index).await;
    filled
}

/// Poll a subaccount until an open order matching the market+direction appears.
async fn wait_for_resting_order(
    velocity: &VelocityClient,
    account: &Pubkey,
    market_index: u16,
    direction: PositionDirection,
) -> Option<User> {
    for _ in 0..20 {
        if let Ok(user) = velocity.get_user_account(account).await {
            let resting = user.orders.iter().any(|o| {
                o.status == OrderStatus::Open
                    && o.market_type == MarketType::Perp
                    && o.market_index == market_index
                    && o.direction == direction
            });
            if resting {
                return Some(user);
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    None
}

/// Best-effort cancel of a subaccount's perp orders on `market_index`.
async fn cancel_market_orders(velocity: &VelocityClient, account: &Pubkey, market_index: u16) {
    if let Ok(builder) = velocity.init_tx(account, false).await {
        let tx = builder
            .cancel_orders((market_index, MarketType::Perp), None)
            .build();
        let _ = velocity.sign_and_send(tx).await;
    }
}

/// Round a price to the nearest tick (>= one tick).
fn round_to_tick(price: i64, tick: u64) -> u64 {
    let tick = tick.max(1) as i64;
    (((price + tick / 2) / tick) * tick).max(tick) as u64
}

/// Try to fill an incoming swift order as the maker (sub-account 1).
async fn try_fill(
    velocity: VelocityClient,
    filler_subaccount: Pubkey,
    swift_order: SignedOrderInfo,
) -> Option<String> {
    let taker_order = swift_order.order_params();
    let taker_subaccount = swift_order.taker_subaccount();

    let (taker_account_data, taker_stats, tx_builder) = match tokio::try_join!(
        velocity.get_user_account(&taker_subaccount),
        velocity.get_user_stats(&swift_order.taker_authority),
        velocity.init_tx(&filler_subaccount, false),
    ) {
        Ok(r) => r,
        Err(e) => {
            println!("  ✗ fill setup failed: {e}");
            return None;
        }
    };

    let tx = tx_builder
        .place_and_make_swift_order(
            OrderParams {
                order_type: OrderType::Limit,
                market_index: taker_order.market_index,
                market_type: taker_order.market_type,
                // counter-direction
                direction: match taker_order.direction {
                    PositionDirection::Long => PositionDirection::Short,
                    PositionDirection::Short => PositionDirection::Long,
                },
                // fill at the taker's best price (auction start)
                price: taker_order
                    .auction_start_price
                    .expect("oracle order has start price")
                    .unsigned_abs(),
                base_asset_amount: taker_order.base_asset_amount,
                post_only: PostOnlyParam::MustPostOnly,
                bit_flags: OrderParams::IMMEDIATE_OR_CANCEL_FLAG,
                ..Default::default()
            },
            &swift_order,
            &taker_account_data,
            &taker_stats.referrer,
        )
        .build();

    match velocity.sign_and_send(tx).await {
        Ok(sig) => {
            println!("  ✓ fill tx: {sig}");
            Some(sig.to_string())
        }
        Err(e) => {
            println!("  ✗ fill tx failed: {e}");
            None
        }
    }
}

fn swift_http_url(context: Context, override_url: Option<&str>) -> String {
    if let Some(base) = override_url {
        // strip trailing ws:// or wss:// if someone passed the WS URL by mistake
        let base = base
            .trim_start_matches("wss://")
            .trim_start_matches("ws://");
        return format!("https://{base}/orders");
    }
    match context {
        Context::MainNet => "https://swift.drift.trade/orders".to_string(),
        _ => "https://master.swift.drift.trade/orders".to_string(),
    }
}
