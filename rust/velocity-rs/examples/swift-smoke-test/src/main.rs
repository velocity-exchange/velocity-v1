//! Smoke test: end-to-end Swift SOL-PERP order flow
//!
//! Bootstraps a maker subscriber and a taker that posts to the swift HTTP
//! endpoint. Verifies WS connectivity, order acceptance, and fill attempt.
//!
//! Required env:
//!   PRIVATE_KEY        - base58 taker keypair
//!
//! Optional env:
//!   RPC_URL            - Solana RPC (default: https://api.devnet.solana.com)
//!   MAINNET            - if set, use mainnet swift/rpc endpoints
//!   SWIFT_URL          - override the swift WS/HTTP base URL
//!
//! The taker uses sub-account 0; the maker uses sub-account 1 of the same
//! wallet.  Ensure both sub-accounts exist and carry enough collateral on
//! devnet before running (USDC for margin, SOL for tx fees).

use std::time::Duration;

use base64::Engine as _;
use futures_util::StreamExt;
use nanoid::nanoid;
use reqwest::header;
use solana_pubkey::Pubkey;
use velocity_rs::{
    swift_order_subscriber::{SignedOrderInfo, SignedOrderType},
    types::{
        MarketType, OrderParams, OrderParamsExt, OrderType, PositionDirection, PostOnlyParam,
        SignedMsgOrderParamsMessage,
    },
    Context, RpcClient, VelocityClient, Wallet,
};

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
    let rpc_url = std::env::var("RPC_URL")
        .unwrap_or_else(|_| "https://api.devnet.solana.com".to_string());

    let wallet: Wallet = velocity_rs::utils::load_keypair_multi_format(
        &std::env::var("PRIVATE_KEY").expect("PRIVATE_KEY env var required"),
    )
    .expect("valid keypair")
    .into();

    // sub-account 0 = taker, sub-account 1 = maker
    let taker_subaccount = wallet.default_sub_account();
    let maker_subaccount = Wallet::derive_user_account(wallet.authority(), 1);

    println!("=== Velocity Swift Smoke Test ===");
    println!("context:  {:?}", context);
    println!("taker:    {}", taker_subaccount);
    println!("maker:    {} (sub-account 1)", maker_subaccount);

    let velocity = VelocityClient::new(context, RpcClient::new(rpc_url), wallet)
        .await
        .expect("VelocityClient initialized");
    velocity
        .subscribe_blockhashes()
        .await
        .expect("blockhash subscription");
    velocity
        .subscribe_account(&taker_subaccount)
        .await
        .expect("taker account subscription");
    let _ = velocity.subscribe_account(&maker_subaccount).await;

    let market_id = velocity.market_lookup("sol-perp").expect("sol-perp market");
    println!("market:   SOL-PERP (index {})\n", market_id.index());

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
            let velocity_maker = velocity.clone();
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

    let slot = velocity
        .rpc()
        .get_slot()
        .await
        .expect("get slot")
        + 200;
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

    println!("\n=== done ===");
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
