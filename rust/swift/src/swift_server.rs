use {
    crate::{
        super_slot_subscriber::SuperSlotSubscriber,
        types::{
            messages::{
                DepositAndPlaceRequest, IncomingSignedMessage, OrderMetadataAndMessage,
                ProcessOrderResponse, PROCESS_ORDER_RESPONSE_ERROR_MSG_AUCTION_OUTSIDE_ORACLE_BAND,
                PROCESS_ORDER_RESPONSE_ERROR_MSG_DELISTED_MARKET,
                PROCESS_ORDER_RESPONSE_ERROR_MSG_DELIVERY_FAILED,
                PROCESS_ORDER_RESPONSE_ERROR_MSG_INVALID_ORDER,
                PROCESS_ORDER_RESPONSE_ERROR_MSG_INVALID_ORDER_AMOUNT,
                PROCESS_ORDER_RESPONSE_ERROR_MSG_ORDER_SLOT_TOO_OLD,
                PROCESS_ORDER_RESPONSE_ERROR_MSG_VERIFY_SIGNATURE,
                PROCESS_ORDER_RESPONSE_IGNORE_PUBKEY, PROCESS_ORDER_RESPONSE_INVALID_UUID_UTF8,
                PROCESS_ORDER_RESPONSE_MESSAGE_SUCCESS,
            },
            types::{unix_now_ms, RequestContext},
        },
        user_account_fetcher::UserAccountFetcher,
        util::{
            headers::XSwiftClientConsumer,
            metrics::{metrics_handler, MetricsServerParams, SwiftServerMetrics},
        },
    },
    anchor_lang::{AccountDeserialize, Discriminator},
    axum::{
        extract::State,
        http::{self, Method, StatusCode},
        routing::{get, post},
        Json, Router,
    },
    base64::Engine,
    dotenv::dotenv,
    log::warn,
    prometheus::Registry,
    redis::{aio::MultiplexedConnection, AsyncCommands},
    solana_account_decoder_client_types::UiAccountEncoding,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_message::v0::Message,
    solana_pubkey::Pubkey,
    solana_rpc_client_api::{
        client_error,
        config::{RpcSimulateTransactionAccountsConfig, RpcSimulateTransactionConfig},
        response::RpcSimulateTransactionResult,
    },
    solana_signature::Signature,
    solana_signer::Signer,
    solana_system_interface::instruction as system_instruction,
    std::{
        collections::HashSet,
        env,
        net::SocketAddr,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc,
        },
        time::{Duration, SystemTime},
    },
    tower_http::cors::{Any, CorsLayer},
    velocity_rs::{
        constants::state_account,
        event_subscriber::PubsubClient,
        math::{
            account_list_builder::AccountsListBuilder,
            constants::{BASE_PRECISION_U64, PRICE_PRECISION_I64},
        },
        program::math::time::{Millis, SlotClock},
        swift_order_subscriber::{SignedMessageInfo, SignedOrderType},
        types::{
            accounts::User, errors::ErrorCode, CommitmentConfig, MarketId, MarketStatus,
            MarketType, MarketTypeExt, OrderParams, OrderParamsExt, OrderType, PositionDirection,
            ProgramError, SdkError, SdkResult, SignedMsgTriggerOrderParams, VersionedMessage,
            VersionedTransaction,
        },
        velocity_idl, Context, RpcClient, TransactionBuilder, VelocityClient, Wallet,
    },
};

/// Accept orders under-collaterized upto this ratio.
const COLLATERAL_BUFFER: f64 = 1.01;

struct Config {
    /// RPC tx simulation on/off
    disable_rpc_sim: AtomicBool,
    /// RPC tx simulation timeout
    simulation_timeout: Duration,
    /// Fee payer used for RPC tx simulation. Never signs (sim runs with
    /// `sig_verify: false`) but must exist and hold rent-exempt SOL on-chain,
    /// otherwise `simulateTransaction` fails at account-loading with
    /// `AccountNotFound` before the program runs. Defaults to the
    /// gas-station-maintained fee payer; override with `SIM_FEE_PAYER`.
    sim_fee_payer: Pubkey,
    /// Reject signed orders whose auction start/end prices sit more than this
    /// many bps from the live oracle — a server-side fat-finger / stale-order
    /// guard. The program preserves signed A/B auctions verbatim (it no longer
    /// re-prices them), so genuinely-off auctions are caught here instead of
    /// on-chain, protecting the client without hijacking a well-formed
    /// aggressive one. `0` disables. Override with `AUCTION_ORACLE_BAND_BPS`
    /// (default 300 = 3%).
    auction_oracle_band_bps: u32,
    /// Skip the oracle-band guard, failing open, when the server's own oracle
    /// is more than this far behind the latest slot. `0` disables the gate.
    /// Override with `AUCTION_ORACLE_MAX_STALENESS_SLOTS` (default 10, about
    /// 4 seconds of wall clock at 400ms per slot).
    auction_oracle_max_staleness_slots: u64,
}

/// Gas-station-maintained fee payer (see infrastructure-v3 gas-station-bot,
/// which tops it up above a 2 SOL threshold). Used as the default sim fee payer.
const DEFAULT_SIM_FEE_PAYER: Pubkey =
    solana_pubkey::pubkey!("feezFJywCs7LZXXi6dyLKpr3XKgtf7KXXKZ2y6vzTSQ");

impl Config {
    fn from_env() -> Self {
        Self {
            disable_rpc_sim: AtomicBool::new(
                std::env::var("DISABLE_RPC_SIM").unwrap_or("false".to_string()) == "true",
            ),
            simulation_timeout: Duration::from_millis(300),
            sim_fee_payer: std::env::var("SIM_FEE_PAYER")
                .ok()
                .and_then(|s| s.parse::<Pubkey>().ok())
                .unwrap_or(DEFAULT_SIM_FEE_PAYER),
            auction_oracle_band_bps: std::env::var("AUCTION_ORACLE_BAND_BPS")
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(300),
            auction_oracle_max_staleness_slots: std::env::var("AUCTION_ORACLE_MAX_STALENESS_SLOTS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(10),
        }
    }
}

pub struct ServerParams {
    velocity: velocity_rs::VelocityClient,
    attest: crate::attest::AttestContext,
    route: crate::route::RouteContext,
    slot_subscriber: Arc<SuperSlotSubscriber>,
    metrics: SwiftServerMetrics,
    redis_pool: MultiplexedConnection,
    user_account_fetcher: UserAccountFetcher,
    config: Arc<Config>,
    farmer_pubkeys: HashSet<Pubkey>,
    rpc_health_cache: RpcHealthCache,
}

impl ServerParams {
    pub fn route(&self) -> &crate::route::RouteContext {
        &self.route
    }

    pub fn attest(&self) -> &crate::attest::AttestContext {
        &self.attest
    }
}

/// TTL for the cached RPC `get_health` result. k8s liveness/readiness probes
/// hit `/health` far more often than the RPC's health can meaningfully change,
/// so we only re-probe the RPC at most once per this window.
const RPC_HEALTH_CACHE_TTL_MS: u64 = 60_000;

/// Caches the result of the RPC `get_health` probe so `/health` doesn't
/// round-trip to the RPC on every k8s probe. All other health signals (ws,
/// slot subscriber, redis, market subs) are already read from local
/// subscribed state, so they stay live on every call.
#[derive(Default)]
struct RpcHealthCache {
    /// unix-ms timestamp of the last real RPC probe; 0 means never probed.
    last_checked_ms: AtomicU64,
    healthy: AtomicBool,
}

impl RpcHealthCache {
    /// Returns the cached health if the last probe is within the TTL, else
    /// `None` to signal the caller should re-probe.
    fn get_fresh(&self, now_ms: u64) -> Option<bool> {
        let last = self.last_checked_ms.load(Ordering::Relaxed);
        if last != 0 && now_ms.saturating_sub(last) < RPC_HEALTH_CACHE_TTL_MS {
            Some(self.healthy.load(Ordering::Relaxed))
        } else {
            None
        }
    }

    fn store(&self, now_ms: u64, healthy: bool) {
        self.healthy.store(healthy, Ordering::Relaxed);
        self.last_checked_ms.store(now_ms, Ordering::Relaxed);
    }
}

pub async fn fallback(uri: axum::http::Uri) -> impl axum::response::IntoResponse {
    (axum::http::StatusCode::NOT_FOUND, format!("No route {uri}"))
}

pub async fn process_order_wrapper(
    x_swift_client_header: Option<axum_extra::TypedHeader<XSwiftClientConsumer>>,
    State(server_params): State<&'static ServerParams>,
    Json(incoming_message): Json<IncomingSignedMessage>,
) -> impl axum::response::IntoResponse {
    let context = RequestContext::from_incoming_message(&incoming_message);
    if context.is_err() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(ProcessOrderResponse {
                message: PROCESS_ORDER_RESPONSE_INVALID_UUID_UTF8,
                error: None,
            }),
        );
    }
    let context = context.unwrap();

    let (status, resp) = match process_order(server_params, incoming_message, false, &context).await
    {
        Ok(order_metadata) => {
            let metrics_labels = &[
                context.market_type,
                &context.market_index.to_string(),
                match order_metadata.will_sanitize {
                    true => "true",
                    false => "false",
                },
            ];
            let topic = format!("swift_orders_{}_{}", metrics_labels[0], metrics_labels[1]);
            let payload = order_metadata.encode();
            // The order is now attestable. A keeper may request the
            // flow-authority co-signature once the hold window elapses.
            server_params.attest.record(
                order_metadata.uuid,
                order_metadata.ts,
                order_metadata.order_signature,
            );

            server_params
                .publish_order(
                    &topic,
                    &payload,
                    order_metadata.uuid(),
                    metrics_labels,
                    &context,
                )
                .await
        }
        Err(err) => err,
    };

    log::info!(
        target: "server",
        "{} status={status} err={} ui={} uuid={} taker={}",
        context.log_prefix,
        resp.error.as_deref().unwrap_or(""),
        x_swift_client_header.is_some_and(|x| x.is_app_order()),
        context.order_uuid,
        context.taker_authority,
    );
    (status, Json(resp))
}

pub async fn process_order(
    server_params: &'static ServerParams,
    incoming_message: IncomingSignedMessage,
    skip_sim: bool,
    context: &RequestContext,
) -> Result<OrderMetadataAndMessage, (http::StatusCode, ProcessOrderResponse)> {
    let IncomingSignedMessage {
        taker_pubkey,
        signature: taker_signature,
        message: _,
        signing_authority,
        taker_authority,
    } = incoming_message;

    let taker_authority = if taker_authority == Pubkey::default() {
        taker_pubkey
    } else {
        taker_authority
    };

    if server_params.farmer_pubkeys.contains(&taker_authority) {
        log::debug!(
            target: "server",
            "Ignoring order from farmer pubkey: {taker_authority}"
        );
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            ProcessOrderResponse {
                message: PROCESS_ORDER_RESPONSE_IGNORE_PUBKEY,
                error: None,
            },
        ));
    }

    server_params.metrics.taker_orders_counter.inc();

    let signing_pubkey = if signing_authority == Pubkey::default() {
        taker_authority
    } else {
        signing_authority
    };

    log::trace!(
        target: "server",
        "{}: Received order with signing pubkey: {signing_pubkey}",
        context.log_prefix,
    );

    let signed_msg = match incoming_message.verify_and_get_signed_message() {
        Ok(m) => m,
        Err(e) => {
            log::warn!(
                "{}: Error verifying signed message: {e:?}, signer: {}, taker_authority: {}",
                context.log_prefix,
                incoming_message.signing_authority,
                incoming_message.taker_authority
            );
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                ProcessOrderResponse {
                    message: PROCESS_ORDER_RESPONSE_ERROR_MSG_VERIFY_SIGNATURE,
                    error: Some(e.to_string()),
                },
            ));
        }
    };
    let delegate_signer = if signed_msg.is_delegated() {
        Some(&signing_pubkey)
    } else {
        None
    };

    // Both reach the log and the keeper feed. The network tag is what the
    // program validates. The route is the custom quoters the taker signed
    // for, because the CLOB and vAMM baseline is implicit.
    let network = signed_msg.network();
    let route = signed_msg.route().map(<[_]>::to_vec);

    let current_slot = server_params.slot_subscriber.current_slot();
    let (
        SignedMessageInfo {
            slot: taker_slot,
            order_params,
            taker_pubkey,
            uuid,
        },
        max_margin_ratio,
        isolated_position_deposit,
    ) = extract_signed_message_info(
        signed_msg,
        &taker_authority,
        current_slot,
        server_params.velocity.slot_clock(),
    )?;

    log::info!(
        target: "server",
        "{} signer={} taker_subaccount={} slot={} side={:?} base={} price={} order_type={:?} reduce_only={} post_only={:?} iso={:?} delegate_signer={:?} network={:?} route={:?}",
        context.log_prefix,
        signing_pubkey,
        taker_pubkey,
        taker_slot,
        order_params.direction,
        order_params.base_asset_amount,
        order_params.price,
        order_params.order_type,
        order_params.reduce_only,
        order_params.post_only,
        isolated_position_deposit,
        delegate_signer,
        network.map(|tag| tag as char),
        route,
    );

    // check the order is valid for execution by program
    let market = server_params
        .velocity
        .try_get_perp_market_account(order_params.market_index);

    if market
        .as_ref()
        .is_ok_and(|m| matches!(m.status, MarketStatus::Delisted | MarketStatus::Settlement))
    {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            ProcessOrderResponse {
                message: PROCESS_ORDER_RESPONSE_ERROR_MSG_DELISTED_MARKET,
                error: format!("market {} delisted", order_params.market_index).into(),
            },
        ));
    }

    if let Err(err) = validate_signed_order_params(
        &order_params,
        market.map(|m| m.market_stats.min_order_size).unwrap_or(0),
    ) {
        log::warn!(
            target: "server",
            "{}: Order did not validate: {err:?}, {order_params:?}",
            context.log_prefix
        );
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            ProcessOrderResponse {
                message: PROCESS_ORDER_RESPONSE_ERROR_MSG_INVALID_ORDER,
                error: Some(err.to_string()),
            },
        ));
    }

    // Server-side stale / fat-finger guard: reject auctions priced far off the
    // live oracle. Replaces the on-chain sanitizer for signed A/B orders, which
    // the program now preserves verbatim. Skips itself (fail open) if the
    // server's own oracle is stale — see validate_auction_within_oracle_band.
    server_params.validate_auction_within_oracle_band(&order_params, current_slot, context)?;

    if !skip_sim {
        match server_params
            .simulate_taker_order_rpc(
                &taker_pubkey,
                &order_params,
                delegate_signer,
                current_slot,
                max_margin_ratio,
                isolated_position_deposit,
                context,
            )
            .await
        {
            Ok(sim_res) => {
                server_params
                    .metrics
                    .rpc_simulation_status
                    .with_label_values(&[sim_res.as_str()])
                    .inc();
            }
            Err((status, sim_err_str, logs)) => {
                server_params
                    .metrics
                    .rpc_simulation_status
                    .with_label_values(&["invalid"])
                    .inc();
                log::warn!(
                    target: "server",
                    "{}: Order sim failed (taker: {taker_pubkey:?}, delegate: {delegate_signer:?}, market: {}-{}): {sim_err_str}. Logs: {logs:?}",
                    context.log_prefix,
                    order_params.market_type.as_str(),
                    order_params.market_index,
                );
                log::warn!(
                    target: "server",
                    "{}: failed order params: {order_params:?}",
                    context.log_prefix,
                );
                return Err((
                    status,
                    ProcessOrderResponse {
                        message: PROCESS_ORDER_RESPONSE_ERROR_MSG_INVALID_ORDER,
                        error: Some(sim_err_str),
                    },
                ));
            }
        };
    }

    if let Some(order_message_str) = signed_msg.raw() {
        // If fat fingered order that requires sanitization, then just send the order
        let will_sanitize =
            server_params.simulate_will_auction_params_sanitize(&order_params, context);
        let order_metadata = OrderMetadataAndMessage {
            market_index: order_params.market_index,
            market_type: order_params.market_type,
            signing_authority: signing_pubkey,
            taker_authority,
            order_message_str: order_message_str.to_owned(),
            order_signature: taker_signature.into(),
            ts: context.recv_ts,
            uuid,
            will_sanitize,
        };

        server_params
            .metrics
            .current_slot_gauge
            .set(current_slot as f64);
        // This is recorded on accept rather than on publish. It is the one
        // point both HTTP entry points reach with `order_params` still in
        // scope. A failed publish still counts here; `swift_redis_publish_fail_count`
        // measures that gap, and it is normally zero.
        server_params.record_order_notional(&order_params, context);

        Ok(order_metadata)
    } else {
        Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            ProcessOrderResponse {
                message: "missing order message str",
                error: None,
            },
        ))
    }
}

pub async fn send_heartbeat(server_params: &'static ServerParams) {
    let heartbeat_time = unix_now_ms();
    let log_prefix = format!("[heartbeat: {heartbeat_time}]");

    let mut conn = server_params.redis_pool.clone();
    let topic = "heartbeat";
    let start = std::time::Instant::now();
    let result: redis::RedisResult<i64> = conn
        .publish(topic.to_string(), "love you".to_string())
        .await;
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    server_params
        .metrics
        .redis_publish_latency
        .observe(elapsed_ms);

    match result {
        Ok(receivers) => {
            log::trace!(
                target: "redis",
                "{log_prefix}: published heartbeat receivers={receivers} latency_ms={elapsed_ms:.2}"
            );
            server_params
                .metrics
                .order_type_counter
                .with_label_values(&["_", "heartbeat", "_"])
                .inc();
            server_params
                .metrics
                .redis_publish_success_counter
                .with_label_values(&[topic])
                .inc();
            server_params
                .metrics
                .redis_publish_subscribers
                .with_label_values(&[topic])
                .set(receivers);
        }
        Err(e) => {
            log::error!(
                target: "redis",
                "{log_prefix}: failed to publish heartbeat, error: {e:?}"
            );
            server_params
                .metrics
                .redis_publish_fail_counter
                .with_label_values(&[topic])
                .inc();
        }
    }
}

pub async fn deposit_trade(
    State(server_params): State<&'static ServerParams>,
    Json(req): Json<DepositAndPlaceRequest>,
) -> impl axum::response::IntoResponse {
    let context = RequestContext::from_incoming_message(&req.swift_order);
    if context.is_err() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(ProcessOrderResponse {
                message: PROCESS_ORDER_RESPONSE_INVALID_UUID_UTF8,
                error: None,
            }),
        );
    }
    let context = context.unwrap();
    let current_slot = server_params.slot_subscriber.current_slot();

    let signed_order_info = req
        .swift_order
        .order()
        .info(&req.swift_order.taker_authority);
    let max_margin_ratio = match extract_signed_message_info(
        &req.swift_order.order(),
        &req.swift_order.taker_authority,
        current_slot,
        server_params.velocity.slot_clock(),
    ) {
        Ok((_info, max_margin_ratio, _is_isolated)) => max_margin_ratio,
        Err((_status, err)) => return (StatusCode::BAD_REQUEST, Json(err)),
    };

    log::info!(
        target: "server",
        "{} depositToTrade request | authority={:?},subaccount={:?}",
        context.log_prefix,
        req.swift_order.taker_authority,
        req.swift_order.taker_pubkey
    );

    if req.deposit_tx.signatures.is_empty()
        || req.deposit_tx.verify_with_results().iter().any(|x| !*x)
    {
        log::info!(target: "server", "{} invalid deposit tx", context.log_prefix);
        return (
            StatusCode::BAD_REQUEST,
            Json(ProcessOrderResponse {
                message: "",
                error: Some("invalid deposit tx".into()),
            }),
        );
    }

    // verify place order ix exists
    let mut has_place_ix = false;
    for ix in req.deposit_tx.message.instructions() {
        if ix.data.len() > 8
            && &ix.data[..8] == velocity_idl::instructions::PlaceSignedMsgTakerOrder::DISCRIMINATOR
        {
            has_place_ix = true;
        }
    }

    if !has_place_ix {
        log::info!(target: "server", "{} missing place order ix", context.log_prefix);
        return (
            StatusCode::BAD_REQUEST,
            Json(ProcessOrderResponse {
                message: "",
                error: Some("missing placeSignedMsgTakerOrder ix".into()),
            }),
        );
    }

    // ensure deposit tx is valid
    let mut user_after_deposit = None;
    match simulate_tx(
        &server_params.velocity,
        req.deposit_tx.message.clone(),
        &[req.swift_order.taker_pubkey],
    )
    .await
    {
        Ok(res) => {
            if let Some(err) = res.err {
                log::info!(
                    target: "server",
                    "{} deposit sim failed: {err:?}, logs: {:?}",
                    context.log_prefix,
                    res.logs
                );
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ProcessOrderResponse {
                        message: "",
                        error: Some("invalid deposit tx".into()),
                    }),
                );
            }
            if let Some(acc) = res.accounts {
                user_after_deposit = acc
                    .first()
                    .and_then(|a| a.as_ref())
                    .and_then(|a| a.data.decode())
                    .and_then(|data| User::try_deserialize(&mut data.as_slice()).ok());
            }
        }
        Err(err) => {
            log::info!(
                target: "server",
                "{} deposit sim network err: {err:?}",
                context.log_prefix,
            );
        }
    }

    if let Some(user) = user_after_deposit {
        if !server_params.simulate_taker_order_local(
            &signed_order_info.order_params,
            &user,
            signed_order_info.slot,
            max_margin_ratio,
            &context,
        ) {
            log::info!(target: "server", "{} local order sim failed", context.log_prefix);
            return (
                StatusCode::BAD_REQUEST,
                Json(ProcessOrderResponse {
                    message: "",
                    error: Some("invalid order".into()),
                }),
            );
        }
    }

    // TODO: deposit tx should enable sim to pass, if it didn't before otherwise order is invalid
    let (status, resp) = match process_order(server_params, req.swift_order, true, &context).await {
        Ok(order_metadata) => {
            let metrics_labels = &[
                context.market_type,
                &context.market_index.to_string(),
                match order_metadata.will_sanitize {
                    true => "true",
                    false => "false",
                },
            ];
            let topic = format!(
                "swift_orders_deposit_{}_{}",
                metrics_labels[0], metrics_labels[1]
            );
            let payload = serde_json::json!({
                "deposit": base64::prelude::BASE64_STANDARD
                .encode(bincode::serialize(&req.deposit_tx).unwrap()),
                "order": order_metadata.encode(),
            })
            .to_string();

            server_params
                .publish_order(
                    &topic,
                    &payload,
                    order_metadata.uuid(),
                    metrics_labels,
                    &context,
                )
                .await
        }
        Err(err) => err,
    };

    (status, Json(resp))
}

pub async fn health_check(
    State(server_params): State<&'static ServerParams>,
) -> impl axum::response::IntoResponse {
    let ws_healthy = server_params.velocity.ws().is_running();
    let slot_sub_healthy = !server_params.slot_subscriber.is_stale();

    // Check if optional accounts are healthy
    let user_account_fetcher_redis_health = if server_params.user_account_fetcher.redis.is_some() {
        server_params
            .user_account_fetcher
            .check_redis_health()
            .await
    } else {
        true
    };

    let redis_health = {
        let mut conn = server_params.redis_pool.clone();
        let ping_result: redis::RedisResult<String> = conn.ping().await;
        ping_result.is_ok()
    };

    // Check if server has metadata available for all spot and perp markets
    let market_subs_healthy = server_params.velocity.state_account().is_ok_and(|s| {
        s.number_of_spot_markets
            == server_params
                .velocity
                .program_data()
                .spot_market_configs()
                .len() as u16
            && s.number_of_markets
                == server_params
                    .velocity
                    .program_data()
                    .perp_market_configs()
                    .len() as u16
    });

    // Check if rpc is healthy, caching the result so k8s probes don't
    // round-trip to the RPC's getHealth on every hit.
    let now_ms = unix_now_ms();
    let rpc_healthy = match server_params.rpc_health_cache.get_fresh(now_ms) {
        Some(cached) => cached,
        None => {
            let healthy = server_params.velocity.rpc().get_health().await.is_ok();
            server_params.rpc_health_cache.store(now_ms, healthy);
            healthy
        }
    };

    if ws_healthy
        && slot_sub_healthy
        && user_account_fetcher_redis_health
        && redis_health
        && rpc_healthy
        && market_subs_healthy
    {
        (axum::http::StatusCode::OK, "ok".into())
    } else {
        let msg = format!(
            "slot_sub_healthy={slot_sub_healthy} | ws_sub_healthy={ws_healthy} 
            | user_account_fetcher_healthy={user_account_fetcher_redis_health} |
            redis_healthy={redis_health}|rpc_healthy={rpc_healthy}|market_subs={market_subs_healthy}",
        );
        log::error!(target: "server", "Failed health check {}", &msg);
        (axum::http::StatusCode::PRECONDITION_FAILED, msg)
    }
}

pub async fn start_server() {
    // Start server
    dotenv().ok();

    let velocity_env = env::var("ENV").unwrap_or("devnet".to_string());

    log::info!(target: "server", "ENV: {velocity_env}");

    let redis_pool = {
        let elasticache_host =
            env::var("ELASTICACHE_HOST").unwrap_or_else(|_| "localhost".to_string());
        let elasticache_port = env::var("ELASTICACHE_PORT").unwrap_or_else(|_| "6379".to_string());
        let use_ssl = env::var("USE_SSL")
            .unwrap_or_else(|_| "false".to_string())
            .to_lowercase()
            == "true";
        let connection_string = if use_ssl {
            format!("rediss://{}:{}", elasticache_host, elasticache_port)
        } else {
            format!("redis://{}:{}", elasticache_host, elasticache_port)
        };
        log::info!(target: "redis", "connecting to redis at {connection_string}");
        let client = redis::Client::open(connection_string).expect("valid redis URL");
        client
            .get_multiplexed_tokio_connection()
            .await
            .expect("redis connected")
    };

    let rpc_endpoint =
        velocity_rs::utils::get_http_url(&env::var("ENDPOINT").expect("valid rpc endpoint"))
            .expect("valid RPC endpoint");

    let registry = Registry::new();
    let metrics = SwiftServerMetrics::new();
    metrics.register(&registry);

    let context = match velocity_env.as_str() {
        "devnet" => Context::DevNet,
        "mainnet-beta" => Context::MainNet,
        _ => panic!("Invalid velocity environment: {velocity_env}"),
    };
    let wallet = Wallet::new(Keypair::new());
    let client = VelocityClient::new(context, RpcClient::new(rpc_endpoint.clone()), wallet)
        .await
        .expect("initialized client");

    let user_account_fetcher = UserAccountFetcher::from_env(client.clone()).await;

    let mut ws_clients = vec![];
    for (_k, ws_endpoint) in std::env::vars().filter(|(k, _v)| k.starts_with("WS_ENDPOINT")) {
        ws_clients.push(Arc::new(PubsubClient::new(&ws_endpoint).await.unwrap()));
    }
    assert!(
        !ws_clients.is_empty(),
        "no slot subscribers provided: set WS_ENDPOINT_*"
    );
    let mut slot_subscriber = SuperSlotSubscriber::new(ws_clients, client.rpc());
    slot_subscriber.subscribe();

    let ignore_pubkeys = env::var("IGNORE_PUBKEYS").unwrap_or_else(|_| "".to_string());
    let pubkeys = ignore_pubkeys
        .split(',')
        .map(|s| s.trim()) // remove extra whitespace
        .filter_map(|s| match s.parse::<Pubkey>() {
            Ok(key) => Some(key),
            Err(_) => {
                log::warn!(target: "server", "Warning: invalid pubkey skipped for ignore pubkeys: {s:?}");
                None
            }
        });

    let state: &'static ServerParams = Box::leak(Box::new(ServerParams {
        attest: crate::attest::AttestContext::from_env(),
        route: crate::route::RouteContext::with_metrics(
            rpc_endpoint,
            velocity_rs::constants::PROGRAM_ID,
            &registry,
        ),
        velocity: client,
        slot_subscriber: Arc::new(slot_subscriber),
        metrics,
        redis_pool,
        user_account_fetcher,
        config: Arc::new(Config::from_env()),
        farmer_pubkeys: HashSet::from_iter(pubkeys),
        rpc_health_cache: RpcHealthCache::default(),
    }));

    // start oracle/market subscriptions (async)
    tokio::spawn(async move {
        let mut all_markets = state.velocity.get_all_market_ids();

        // keep markets in settlement mode for tx simulation
        for market in state.velocity.program_data().perp_market_configs() {
            if market.status == MarketStatus::Settlement {
                all_markets.push(MarketId::perp(market.market_index));
            }
        }

        for market in state.velocity.program_data().spot_market_configs() {
            if market.status == MarketStatus::Settlement {
                all_markets.push(MarketId::spot(market.market_index));
            }
        }

        log::info!("subscribing markets: {:?}", &all_markets);
        if let Err(err) = state.velocity.subscribe_markets(&all_markets).await {
            log::error!("couldn't subscribe markets: {err:?}, RPC sim disabled!");
            state.disable_rpc_sim();
        }
        if let Err(err) = state.velocity.subscribe_oracles(&all_markets).await {
            log::error!("couldn't subscribe oracles: {err:?}, RPC sim disabled!");
            state.disable_rpc_sim();
        }

        if let Err(err) = state.velocity.subscribe_blockhashes().await {
            log::error!("couldn't subscribe to blockhashes: {err:?}, RPC sim disabled!");
            state.disable_rpc_sim();
        }
    });

    let host = env::var("HOST").unwrap_or("0.0.0.0".to_string());
    let port = env::var("PORT").unwrap_or("3000".to_string());
    let cors = CorsLayer::new()
        .allow_methods([Method::POST, Method::GET, Method::OPTIONS])
        .allow_headers(Any)
        .allow_origin(Any);
    let addr: SocketAddr = format!("{host}:{port}").parse().unwrap();
    let app = Router::new()
        .fallback(fallback)
        .route("/orders", post(process_order_wrapper))
        .route("/route", get(crate::route::route_quote))
        .route("/attest", post(crate::attest::attest))
        .route("/depositTrade", post(deposit_trade))
        .route("/health", get(health_check))
        .layer(cors)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    log::info!("Swift server on {}", listener.local_addr().unwrap());

    let registry = Arc::new(registry);
    let server_metrics_state = MetricsServerParams {
        registry,
        quoter_health: Some(state.route.health().clone()),
    };
    let metrics_addr: SocketAddr = format!(
        "0.0.0.0:{}",
        env::var("METRICS_PORT").unwrap_or("9464".to_string())
    )
    .parse()
    .unwrap();
    let metrics_app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(server_metrics_state);

    let listener_metrics = tokio::net::TcpListener::bind(&metrics_addr).await.unwrap();
    log::info!(
        "Swift metrics server on {}",
        listener_metrics.local_addr().unwrap()
    );

    // Avoids RPC cold starts when orders are infrequent: builds one tx and
    // resigns it with a fresh blockhash each tick.
    let rpc_sim_loop = tokio::spawn(async {
        let sender = Keypair::new();
        let receiver = Keypair::new();
        let instruction =
            system_instruction::transfer(&sender.pubkey(), &receiver.pubkey(), 1_000_000_000u64);

        let mut interval = tokio::time::interval(Duration::from_secs(5));

        loop {
            interval.tick().await;
            let message = Message::try_compile(
                &sender.pubkey(),
                std::slice::from_ref(&instruction),
                &[],
                Hash::default(),
            )
            .unwrap();
            let versioned_message = VersionedMessage::V0(message);
            let _ = state
                .velocity
                .rpc()
                .simulate_transaction_with_config(
                    &VersionedTransaction {
                        message: versioned_message,
                        // must provide a signature for the RPC call to work
                        signatures: vec![Signature::new_unique()],
                    },
                    RpcSimulateTransactionConfig {
                        sig_verify: false,
                        replace_recent_blockhash: true,
                        ..Default::default()
                    },
                )
                .await;
        }
    });

    let send_heartbeat_loop = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            send_heartbeat(state).await;
        }
    });

    let axum_server = tokio::spawn(async { axum::serve(listener, app).await });
    let metrics_server = tokio::spawn(async { axum::serve(listener_metrics, metrics_app).await });

    let _ = tokio::try_join!(
        rpc_sim_loop,
        axum_server,
        metrics_server,
        send_heartbeat_loop
    );
}

/// Simple validation from program's `handle_signed_order_ix`
fn validate_signed_order_params(
    taker_order_params: &OrderParams,
    min_order_size: u64,
) -> Result<(), ErrorCode> {
    if !matches!(
        taker_order_params.order_type,
        OrderType::Market | OrderType::Oracle | OrderType::Limit
    ) {
        return Err(ErrorCode::InvalidOrderMarketType);
    }

    if !matches!(taker_order_params.market_type, MarketType::Perp) {
        return Err(ErrorCode::InvalidOrderMarketType);
    }

    if taker_order_params.base_asset_amount < min_order_size {
        // can always close reduce_only
        if !taker_order_params.reduce_only {
            log::info!(target: "server", "{} < {min_order_size}", taker_order_params.base_asset_amount);
            return Err(ErrorCode::InvalidOrderSizeTooSmall);
        }
    }

    // has_valid_auction_params
    if taker_order_params.auction_duration.is_some()
        && taker_order_params.auction_start_price.is_some()
        && taker_order_params.auction_end_price.is_some()
    {
        let start_price = taker_order_params.auction_start_price.unwrap();
        let end_price = taker_order_params.auction_end_price.unwrap();

        if taker_order_params.direction == PositionDirection::Long && start_price <= end_price
            || taker_order_params.direction == PositionDirection::Short && start_price >= end_price
        {
            Ok(())
        } else {
            log::info!(target: "server", "auction price reversed");
            Err(ErrorCode::InvalidOrderAuction)
        }
    } else if taker_order_params.order_type == OrderType::Limit
        && taker_order_params.auction_duration.is_none()
        && taker_order_params.auction_start_price.is_none()
        && taker_order_params.auction_end_price.is_none()
    {
        Ok(())
    } else {
        Err(ErrorCode::InvalidOrderAuction)
    }
}

/// True when the order carries a fully-specified auction (duration + start +
/// end), i.e. there are prices to bound against the oracle.
fn has_bounded_auction(order_params: &OrderParams) -> bool {
    order_params.auction_duration.is_some()
        && order_params.auction_start_price.is_some()
        && order_params.auction_end_price.is_some()
}

/// Pure check: are the order's auction start & end prices within `band_bps` of
/// `oracle_price`? `OrderType::Oracle` auctions carry oracle-relative offsets
/// (already stale-immune); every other type carries absolute prices, which are
/// normalised to a signed distance from oracle before comparison. Returns true
/// when there is nothing to bound (band disabled, no auction, or bad oracle).
fn auction_within_oracle_band(
    order_params: &OrderParams,
    oracle_price: i64,
    band_bps: u32,
) -> bool {
    if band_bps == 0 || oracle_price <= 0 || !has_bounded_auction(order_params) {
        return true;
    }
    let start = order_params.auction_start_price.unwrap();
    let end = order_params.auction_end_price.unwrap();

    let band = (oracle_price as i128 * band_bps as i128 / 10_000) as i64;
    let is_offset = order_params.order_type == OrderType::Oracle;
    let distance_from_oracle = |p: i64| {
        if is_offset {
            p
        } else {
            p.saturating_sub(oracle_price)
        }
    };

    distance_from_oracle(start).abs() <= band && distance_from_oracle(end).abs() <= band
}

#[derive(Debug)]
pub enum SimulationStatus {
    /// Success sim'd locally
    Success,
    Degraded,
    Timeout,
    Disabled,
    /// Success but sim'd over RPC
    SuccessRpc,
    /// Given leniency for collateral error
    SuccessCollateralBuffer,
}

impl SimulationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Degraded => "degraded",
            Self::Timeout => "timeout",
            Self::Disabled => "disabled",
            Self::SuccessRpc => "successRpc",
            Self::SuccessCollateralBuffer => "successBuffer",
        }
    }
}

impl ServerParams {
    /// Toggle RPC simulation off
    pub fn disable_rpc_sim(&self) {
        self.config
            .disable_rpc_sim
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    /// True if RPC simulation is set disabled
    pub fn is_rpc_sim_disabled(&self) -> bool {
        self.config
            .disable_rpc_sim
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    /// Run the off-chain pre-trade simulation, returning `true` only on a clean
    /// local success. A `false` (build error, sim error, or **panic**) makes the
    /// caller fall back to RPC simulation — the local sim is best-effort and must
    /// never take down the request. The native zero-copy loaders cast account
    /// bytes by reference (`bytemuck::from_bytes`), which panics rather than
    /// errors on an unexpected layout/alignment; we catch that here, log it, and
    /// degrade to RPC rather than letting the panic drop the connection (-> 502).
    fn simulate_taker_order_local(
        &self,
        order_params: &OrderParams,
        user: &velocity_rs::types::accounts::User,
        signing_slot: Slot,
        max_margin_ratio: Option<u16>,
        context: &RequestContext,
    ) -> bool {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.simulate_taker_order_local_inner(
                order_params,
                user,
                signing_slot,
                max_margin_ratio,
                context,
            )
        })) {
            Ok(ok) => ok,
            Err(panic) => {
                let msg = panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("<non-string panic payload>");
                log::error!(
                    target: "sim",
                    "{}: local sim panicked, falling back to rpc sim: {msg}",
                    context.log_prefix
                );
                false
            }
        }
    }

    fn simulate_taker_order_local_inner(
        &self,
        order_params: &OrderParams,
        user: &velocity_rs::types::accounts::User,
        signing_slot: Slot,
        max_margin_ratio: Option<u16>,
        context: &RequestContext,
    ) -> bool {
        let state_bytes = match self.velocity.account_raw(state_account()) {
            Ok(b) => b,
            Err(err) => {
                log::warn!(
                    target: "sim",
                    "{}: state account fetch failed: {err:?}",
                    context.log_prefix
                );
                return false;
            }
        };

        let mut accounts_builder = AccountsListBuilder::default();
        let accounts = match accounts_builder.try_build(
            &self.velocity,
            user,
            &[MarketId::new(
                order_params.market_index,
                order_params.market_type,
            )],
        ) {
            Ok(a) => a,
            Err(err) => {
                log::warn!(
                    target: "sim",
                    "{}: couldn't build accounts for sim: {err:?}",
                    context.log_prefix
                );
                return false;
            }
        };

        match crate::util::local_sim::simulate_detached_perp_order(
            user,
            accounts,
            &state_bytes,
            *order_params,
            signing_slot,
            max_margin_ratio,
        ) {
            Ok(()) => true,
            Err(err) => {
                log::debug!(
                    target: "sim",
                    "{}: local sim failed: {err:?}",
                    context.log_prefix
                );
                false
            }
        }
    }
    /// Simulate the taker placing a perp order via RPC, tries local sim first
    async fn simulate_taker_order_rpc(
        &self,
        taker_subaccount_pubkey: &Pubkey,
        taker_order_params: &OrderParams,
        delegate_signer: Option<&Pubkey>,
        slot: Slot,
        max_margin_ratio: Option<u16>,
        isolated_deposit: Option<u64>,
        context: &RequestContext,
    ) -> Result<SimulationStatus, (axum::http::StatusCode, String, Option<Vec<String>>)> {
        let mut sim_result = SimulationStatus::Disabled;

        let t0 = SystemTime::now();

        if let Some(delegate) = delegate_signer {
            // trace, not debug: the INFO order line already prints
            // delegate_signer, so this would be per-order noise at sim=debug.
            log::trace!(
                target: "sim",
                "{}: delegate signer for sim: {delegate}",
                context.log_prefix
            );
        }

        let user_with_timeout = tokio::time::timeout(
            self.config.simulation_timeout,
            self.user_account_fetcher
                .get_user(taker_subaccount_pubkey, slot),
        )
        .await;

        if user_with_timeout.is_err() {
            sim_result = SimulationStatus::Timeout;
            warn!(
                target: "sim",
                "{}: simulateTransaction degraded (timeout)",
                context.log_prefix
            );
            return Ok(sim_result);
        }

        let user_result = user_with_timeout.unwrap();
        let user = user_result.map_err(|err| {
            (
                axum::http::StatusCode::NOT_FOUND,
                format!("unable to fetch user: {err:?}"),
                None,
            )
        })?;

        // check the account delegate matches the signer
        if delegate_signer.is_some_and(|d| d != &user.delegate) {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                "signer is not configured delegate".to_string(),
                None,
            ));
        }

        log::info!(
            target: "server",
            "{:?}: max_leverage={},activate_hlm={}",
            user.authority,
            taker_order_params.base_asset_amount == u64::MAX,
            taker_order_params.high_leverage_mode(),
        );

        if self.is_rpc_sim_disabled() {
            return Ok(sim_result);
        }

        let t1 = SystemTime::now();
        log::info!(
            target: "sim",
            "{}: fetch user: {:?}",
            context.log_prefix,
            SystemTime::now().duration_since(t0)
        );

        // TODO: isolated deposits need changes for local simming
        if isolated_deposit.is_none()
            && self.simulate_taker_order_local(
                taker_order_params,
                &user,
                slot,
                max_margin_ratio,
                context,
            )
        {
            sim_result = SimulationStatus::Success;
            log::info!(
                target: "sim",
                "{}: simulate tx (local): {:?}",
                context.log_prefix,
                SystemTime::now().duration_since(t1)
            );
            return Ok(sim_result);
        }

        // fallback to network sim
        let mut tx = TransactionBuilder::new(
            self.velocity.program_data(),
            *taker_subaccount_pubkey,
            std::borrow::Cow::Owned(user),
            false,
        )
        .with_priority_fee(5_000, Some(1_400_000));
        if let Some(margin_ratio) = max_margin_ratio {
            tx = tx.update_user_perp_position_custom_margin_ratio(
                taker_order_params.market_index,
                margin_ratio,
            );
        }
        if let Some(amount) = isolated_deposit {
            tx = tx.transfer_isolated_perp_position_deposit(
                amount as i64,
                taker_order_params.market_index,
            );
        }

        // The order routes as it places, so the simulation must carry the
        // market's book accounts to reproduce what the real placement does.
        let Some(book) =
            velocity_rs::market_book(&self.velocity, taker_order_params.market_index).await
        else {
            return Err((
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "market {} has no approved book on its quoter slab",
                    taker_order_params.market_index
                ),
                None,
            ));
        };

        // always set fee payer to some other account with SOL
        // supports privey wallets and how a swift order is intended to be placed anyway
        let message = tx
            .place_and_take(*taker_order_params, book.accounts, None)
            .fee_payer(self.config.sim_fee_payer)
            .build();

        let simulate_result_with_timeout = tokio::time::timeout(
            self.config.simulation_timeout,
            self.velocity.rpc().simulate_transaction_with_config(
                &VersionedTransaction {
                    message,
                    // must provide placerholder signature(s) for the RPC call to work
                    // signer + fee payer
                    signatures: vec![Signature::new_unique(), Signature::new_unique()],
                },
                RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    commitment: Some(CommitmentConfig::confirmed()),
                    min_context_slot: Some(slot - 30), // allow tx sim on up to 30 slots stale context
                    ..Default::default()
                },
            ),
        )
        .await;

        match simulate_result_with_timeout {
            Ok(Ok(res)) => {
                if let Some(simulate_err) = res.value.err {
                    log::warn!(
                        target: "sim",
                        "{}: program sim error: {simulate_err:?}",
                        context.log_prefix
                    );
                    // A tx-level `AccountNotFound` means an account failed to
                    // load before the program ran (empty logs). It has many
                    // causes, but a common (and confusing) one is an unfunded /
                    // garbage-collected sim fee payer — it never signs but must
                    // exist and be rent-exempt. Surface its balance as a hint
                    // when this hits; it doesn't prove the fee payer is at fault.
                    // Fire-and-forget so the check never delays the error we
                    // return to the client (the order is already failing).
                    if matches!(
                        client_error::TransactionError::from(simulate_err.to_owned()),
                        client_error::TransactionError::AccountNotFound
                    ) {
                        let rpc = self.velocity.rpc();
                        let fee_payer = self.config.sim_fee_payer;
                        let log_prefix = context.log_prefix.clone();
                        tokio::spawn(async move {
                            match rpc.get_balance(&fee_payer).await {
                                Ok(0) => log::error!(
                                    target: "sim",
                                    "{log_prefix}: sim AccountNotFound and sim fee payer {fee_payer} is unfunded (0 SOL) — likely the cause; fund it",
                                ),
                                Ok(lamports) => log::warn!(
                                    target: "sim",
                                    "{log_prefix}: sim AccountNotFound; sim fee payer {fee_payer} is funded ({} SOL) so the missing account is elsewhere",
                                    lamports as f64 / 1e9,
                                ),
                                Err(err) => log::warn!(
                                    target: "sim",
                                    "{log_prefix}: sim AccountNotFound; could not check sim fee payer {fee_payer} balance: {err:?}",
                                ),
                            }
                        });
                    }
                    let err = SdkError::Rpc(Box::new(client_error::Error {
                        request: None,
                        kind: Box::new(client_error::ErrorKind::TransactionError(
                            simulate_err.to_owned().into(),
                        )),
                    }));
                    match err.to_anchor_error_code() {
                        Some(code) => {
                            // insufficient collateral is prone to precision errors, allow the order through with some leniency
                            // EXCEPT for isolated deposits, where we want to return the error to the client
                            if code == ProgramError::Velocity(ErrorCode::InsufficientCollateral)
                                && isolated_deposit.is_none()
                            {
                                if let Some(ref logs) = res.value.logs {
                                    if let Some(collateral_ratio) = extract_collateral_ratio(logs) {
                                        if collateral_ratio <= COLLATERAL_BUFFER {
                                            log::info!(
                                                target: "sim",
                                                "{}: accepting undercollateralized order: {collateral_ratio}",
                                                context.log_prefix
                                            );
                                            log::info!(
                                                target: "sim",
                                                "{}: simulate tx (rpc): {:?}",
                                                context.log_prefix,
                                                SystemTime::now().duration_since(t1)
                                            );
                                            return Ok(SimulationStatus::SuccessCollateralBuffer);
                                        }
                                    }
                                }
                                if log::log_enabled!(target: "accountState", log::Level::Debug) {
                                    dump_account_state(
                                        &self.velocity,
                                        taker_subaccount_pubkey,
                                        user,
                                        taker_order_params,
                                        res.context.slot,
                                        context,
                                    );
                                }
                            }
                            Err((
                                axum::http::StatusCode::BAD_REQUEST,
                                format!("invalid order. error code: {code:?}"),
                                res.value.logs,
                            ))
                        }
                        None => Err((
                            axum::http::StatusCode::BAD_REQUEST,
                            format!("invalid order: {simulate_err:?}"),
                            res.value.logs,
                        )),
                    }
                } else {
                    log::info!(
                        target: "sim",
                        "{}: simulate tx (rpc): {:?}",
                        context.log_prefix,
                        SystemTime::now().duration_since(t1)
                    );
                    sim_result = SimulationStatus::SuccessRpc;
                    Ok(sim_result)
                }
            }
            Ok(Err(err)) => {
                log::warn!(
                    target: "sim",
                    "{}: network sim error: {err:?}",
                    context.log_prefix
                );
                sim_result = SimulationStatus::Degraded;
                Ok(sim_result)
            }
            Err(_) => {
                sim_result = SimulationStatus::Timeout;
                Ok(sim_result)
            }
        }
    }

    /// Simulate if auction params will be sanitized
    /// Server-side stale / fat-finger guard. Rejects a signed order whose
    /// auction prices sit outside `config.auction_oracle_band_bps` of the live
    /// oracle. The program preserves signed A/B auctions verbatim, so this is
    /// where a genuinely-off auction (stale data, fat finger) is stopped —
    /// off-program, still saving the client, without re-pricing a well-formed
    /// aggressive auction.
    ///
    /// The guard is designed to never itself become a source of rejections:
    /// it **fails open** if the oracle can't be read, and it **skips the check**
    /// (also failing open) when the server's own oracle is more than
    /// `auction_oracle_max_staleness_slots` behind the latest slot — a lagging
    /// swift-side oracle must not start bouncing otherwise-valid orders. Every
    /// rejection logs the oracle's staleness (oracle slot vs current slot) for
    /// debuggability.
    fn validate_auction_within_oracle_band(
        &self,
        order_params: &OrderParams,
        current_slot: Slot,
        context: &RequestContext,
    ) -> Result<(), (axum::http::StatusCode, ProcessOrderResponse)> {
        let band_bps = self.config.auction_oracle_band_bps;
        if band_bps == 0 || !has_bounded_auction(order_params) {
            return Ok(());
        }

        let market_index_str = order_params.market_index.to_string();
        let record = |outcome: &str| {
            self.metrics
                .auction_band_guard
                .with_label_values(&[&market_index_str, outcome])
                .inc();
        };

        let market_id = MarketId::new(order_params.market_index, order_params.market_type);
        let oracle = match self.velocity.try_get_oracle_price_data_and_slot(market_id) {
            Some(o) => o,
            None => {
                // fail open: don't block on a missing oracle read
                record("skip_oracle_missing");
                log::warn!(
                    target: "server",
                    "{}: oracle price None (market {market_id:?}); skipping auction band check",
                    context.log_prefix
                );
                return Ok(());
            }
        };

        let oracle_slot = oracle.slot;
        let oracle_staleness_slots = current_slot.saturating_sub(oracle_slot);
        self.metrics
            .auction_oracle_staleness_slots
            .with_label_values(&[&market_index_str])
            .set(oracle_staleness_slots as f64);

        // Fail open whenever we can't trust our own freshness: a stale slot
        // subscriber (which makes `current_slot` — and therefore the staleness
        // measurement — unreliable) or a stale oracle must never turn this guard
        // into a source of rejections for otherwise-valid orders.
        let slot_subscriber_stale = self.slot_subscriber.is_stale();
        // The env knob is configured in 400ms units. The measured age is
        // wall clock, integrated over each slot duration regime.
        let max_staleness =
            Millis::from_stored_units(self.config.auction_oracle_max_staleness_slots);
        let oracle_age = self
            .velocity
            .slot_clock()
            .elapsed(oracle_slot, current_slot);
        let oracle_stale = max_staleness != Millis::ZERO && oracle_age > max_staleness;
        if slot_subscriber_stale || oracle_stale {
            record(if slot_subscriber_stale {
                "skip_slot_subscriber_stale"
            } else {
                "skip_oracle_stale"
            });
            log::warn!(
                target: "server",
                "{}: skipping auction band check (fail open) — slot_subscriber_stale={slot_subscriber_stale} \
                 oracle_stale_by={oracle_staleness_slots} slots (oracle_slot={oracle_slot} \
                 current_slot={current_slot} max={}ms)",
                context.log_prefix,
                max_staleness.as_ms(),
            );
            return Ok(());
        }

        if auction_within_oracle_band(order_params, oracle.data.price, band_bps) {
            return Ok(());
        }
        record("reject");

        log::warn!(
            target: "server",
            "{}: rejecting order — auction outside oracle band: start={:?} end={:?} \
             oracle_price={} oracle_slot={oracle_slot} current_slot={current_slot} \
             oracle_staleness_slots={oracle_staleness_slots} band_bps={band_bps}",
            context.log_prefix,
            order_params.auction_start_price,
            order_params.auction_end_price,
            oracle.data.price,
        );
        Err((
            axum::http::StatusCode::BAD_REQUEST,
            ProcessOrderResponse {
                message: PROCESS_ORDER_RESPONSE_ERROR_MSG_AUCTION_OUTSIDE_ORACLE_BAND,
                error: None,
            },
        ))
    }

    /// Record the notional USD of an accepted order, as `base_asset_amount`
    /// times the oracle price. This reads the locally subscribed oracle and
    /// makes no RPC call, so it is safe in the request path.
    ///
    /// Two cases skip the record and count the skip, because a number there
    /// would be worse than a gap.
    ///   - `base_asset_amount == u64::MAX` is the max-leverage sentinel, which
    ///     the program resolves to a real size on chain. Multiplying it out
    ///     would add about 1.8e10 base units of imaginary notional per order,
    ///     which hides every real number.
    ///   - No readable oracle price. This is the same condition that makes the
    ///     auction band guard fail open.
    fn record_order_notional(&self, order_params: &OrderParams, context: &RequestContext) {
        let market_index_str = order_params.market_index.to_string();
        let labels = [order_params.market_type.as_str(), &market_index_str];
        let skip = |reason: &str| {
            self.metrics
                .order_notional_skipped
                .with_label_values(&[labels[0], labels[1], reason])
                .inc();
        };

        if order_params.base_asset_amount == u64::MAX {
            skip("max_leverage");
            return;
        }

        let market_id = MarketId::new(order_params.market_index, order_params.market_type);
        let Some(oracle) = self.velocity.try_get_oracle_price_data_and_slot(market_id) else {
            skip("no_oracle");
            log::debug!(
                target: "server",
                "{}: no oracle price for {market_id:?}; order notional not recorded",
                context.log_prefix
            );
            return;
        };
        if oracle.data.price <= 0 {
            skip("no_oracle");
            return;
        }

        // Base is 1e9-scaled and price is 1e6-scaled. The product of the raw
        // integers overflows i64 for a large order, so each one is divided
        // down before the multiply.
        let base = order_params.base_asset_amount as f64 / BASE_PRECISION_U64 as f64;
        let price = oracle.data.price as f64 / PRICE_PRECISION_I64 as f64;
        self.metrics
            .order_notional_usd
            .with_label_values(&labels)
            .inc_by(base * price);
    }

    fn simulate_will_auction_params_sanitize(
        &self,
        order_params: &OrderParams,
        context: &RequestContext,
    ) -> bool {
        let perp_market = match self
            .velocity
            .try_get_perp_market_account(order_params.market_index)
        {
            Ok(m) => m,
            Err(err) => {
                log::debug!(
                    target: "sim",
                    "{}: couldn't get perp market: {err:?}",
                    context.log_prefix
                );
                return false;
            }
        };

        let market_id = MarketId::new(order_params.market_index, order_params.market_type);
        let oracle_data = match self.velocity.try_get_oracle_price_data_and_slot(market_id) {
            Some(p) => p,
            None => {
                log::debug!(
                    target: "sim",
                    "{}: oracle price is None",
                    context.log_prefix
                );
                return false;
            }
        };

        // Mirrors `OrderParams::update_perp_auction_params`, the sanitize step
        // every placement runs: returns true when the program would adjust the
        // auction params at placement time.
        let mut params = order_params.clone();
        match params.update_perp_auction_params(&perp_market, oracle_data.data.price, true) {
            Ok(sanitized) => sanitized,
            Err(err) => {
                log::debug!(
                    target: "sim",
                    "{}: local sim failed: {err:?}",
                    context.log_prefix
                );
                true
            }
        }
    }

    async fn publish_order(
        &self,
        topic: &str,
        payload: &String,
        uuid: &str,
        metrics_labels: &[&str; 3],
        context: &RequestContext,
    ) -> (axum::http::StatusCode, ProcessOrderResponse) {
        let mut conn = self.redis_pool.clone();
        let publish_start = std::time::Instant::now();
        let result: redis::RedisResult<i64> =
            conn.publish(topic.to_string(), payload.to_string()).await;
        let publish_rtt_ms = publish_start.elapsed().as_secs_f64() * 1000.0;
        self.metrics.redis_publish_latency.observe(publish_rtt_ms);

        match result {
            Ok(receivers) => {
                self.metrics
                    .order_type_counter
                    .with_label_values(metrics_labels)
                    .inc();
                self.metrics
                    .redis_publish_success_counter
                    .with_label_values(&[topic])
                    .inc();
                self.metrics
                    .redis_publish_subscribers
                    .with_label_values(&[topic])
                    .set(receivers);
                self.metrics
                    .response_time_histogram
                    .observe((unix_now_ms() - context.recv_ts) as f64);

                let publish_latency = unix_now_ms().saturating_sub(context.recv_ts);
                if receivers == 0 && topic != "heartbeat" {
                    log::warn!(
                        target: "redis",
                        "{} topic={topic}: published order {uuid} latency_ms={publish_latency} publish_rtt_ms={publish_rtt_ms:.2} receivers=0",
                        context.log_prefix
                    );
                } else {
                    log::info!(
                        target: "redis",
                        "{} topic={topic}: published order {uuid} latency_ms={publish_latency} publish_rtt_ms={publish_rtt_ms:.2} receivers={receivers}",
                        context.log_prefix
                    );
                }
                (
                    axum::http::StatusCode::OK,
                    ProcessOrderResponse {
                        message: PROCESS_ORDER_RESPONSE_MESSAGE_SUCCESS,
                        error: None,
                    },
                )
            }
            Err(e) => {
                log::error!(
                    target: "redis",
                    "{} topic={topic}: failed to publish order {uuid}, error: {e:?}",
                    context.log_prefix
                );
                self.metrics
                    .redis_publish_fail_counter
                    .with_label_values(&[topic])
                    .inc();
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    ProcessOrderResponse {
                        message: PROCESS_ORDER_RESPONSE_ERROR_MSG_DELIVERY_FAILED,
                        error: Some(format!("redis publish error: {e:?}")),
                    },
                )
            }
        }
    }
}

/// extract collateral ratio from program sim logs
fn extract_collateral_ratio(logs: &[String]) -> Option<f64> {
    for line in logs {
        if line.contains("Program log: total_collateral=") {
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() >= 2 {
                // Extract total_collateral
                let total_collateral_part = parts[0];
                let margin_requirement_part = parts[1];

                let total_collateral = total_collateral_part
                    .split('=')
                    .nth(1)?
                    .trim()
                    .parse::<f64>()
                    .ok()?;

                let margin_requirement = margin_requirement_part
                    .split('=')
                    .nth(1)?
                    .trim()
                    .parse::<f64>()
                    .ok()?;

                if total_collateral != 0.0 {
                    return Some(margin_requirement / total_collateral);
                }
            }
        }
    }
    None
}

fn validate_order(
    stop_loss: Option<&SignedMsgTriggerOrderParams>,
    take_profit: Option<&SignedMsgTriggerOrderParams>,
    taker_slot: Slot,
    current_slot: Slot,
    slot_clock: SlotClock,
) -> Result<(), (axum::http::StatusCode, ProcessOrderResponse)> {
    // Validate order parameters
    if stop_loss.is_some_and(|x| x.base_asset_amount == 0 || x.trigger_price == 0)
        || take_profit.is_some_and(|x| x.base_asset_amount == 0 || x.trigger_price == 0)
    {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            ProcessOrderResponse {
                message: PROCESS_ORDER_RESPONSE_ERROR_MSG_INVALID_ORDER_AMOUNT,
                error: None,
            },
        ));
    }

    // Validate the slot against about 200 seconds of wall-clock age,
    // integrated over each slot duration regime. This mirrors the program's
    // signed-msg staleness gate.
    if slot_clock.elapsed(taker_slot, current_slot) > Millis::from_secs(200) {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            ProcessOrderResponse {
                message: PROCESS_ORDER_RESPONSE_ERROR_MSG_ORDER_SLOT_TOO_OLD,
                error: Some(PROCESS_ORDER_RESPONSE_ERROR_MSG_ORDER_SLOT_TOO_OLD.to_string()),
            },
        ));
    }

    Ok(())
}

fn extract_signed_message_info(
    signed_msg: &SignedOrderType,
    taker_authority: &Pubkey,
    current_slot: Slot,
    slot_clock: SlotClock,
) -> Result<
    (SignedMessageInfo, Option<u16>, Option<u64>),
    (axum::http::StatusCode, ProcessOrderResponse),
> {
    match signed_msg {
        SignedOrderType::Delegated { inner, .. } => {
            validate_order(
                inner.stop_loss_order_params.as_ref(),
                inner.take_profit_order_params.as_ref(),
                inner.slot,
                current_slot,
                slot_clock,
            )?;
            Ok((
                SignedMessageInfo {
                    taker_pubkey: inner.taker_pubkey,
                    order_params: inner.signed_msg_order_params,
                    uuid: inner.uuid,
                    slot: inner.slot,
                },
                inner.max_margin_ratio,
                inner.isolated_position_deposit,
            ))
        }
        SignedOrderType::Authority { inner, .. } => {
            validate_order(
                inner.stop_loss_order_params.as_ref(),
                inner.take_profit_order_params.as_ref(),
                inner.slot,
                current_slot,
                slot_clock,
            )?;
            Ok((
                SignedMessageInfo {
                    taker_pubkey: Wallet::derive_user_account(
                        taker_authority,
                        inner.sub_account_id,
                    ),
                    order_params: inner.signed_msg_order_params,
                    uuid: inner.uuid,
                    slot: inner.slot,
                },
                inner.max_margin_ratio,
                inner.isolated_position_deposit,
            ))
        }
    }
}

fn dump_account_state(
    velocity: &VelocityClient,
    taker_subaccount_pubkey: &Pubkey,
    user: User,
    taker_order_params: &OrderParams,
    slot: Slot,
    context: &RequestContext,
) {
    log::info!(
    target: "accountState",
    "{}: dumping account state: user:{},authority:{},slot:{}",
    context.log_prefix,
    taker_subaccount_pubkey,
    user.authority,
    slot
    );
    let mut debug_log = String::with_capacity(8192 * 2);
    debug_log.push_str("user:");
    base64::engine::general_purpose::STANDARD.encode_string(
        velocity_rs::utils::zero_account_to_bytes(user),
        &mut debug_log,
    );
    debug_log.push('|');
    for p in user.spot_positions.iter().filter(|p| !p.is_available()) {
        if let Ok(market) = velocity.try_get_spot_market_account(p.market_index) {
            debug_log.push_str(&format!("spotMarket-{}:", p.market_index,));
            base64::engine::general_purpose::STANDARD.encode_string(
                velocity_rs::utils::zero_account_to_bytes(market),
                &mut debug_log,
            );
            debug_log.push('|');
        }
        if let Some(oracle) =
            velocity.try_get_oracle_price_data_and_slot(MarketId::spot(p.market_index))
        {
            debug_log.push_str(&format!("oracle-{:?}-{}:", oracle.source, oracle.pubkey));
            base64::engine::general_purpose::STANDARD.encode_string(oracle.raw, &mut debug_log);
            debug_log.push('|');
        }
    }
    for p in user.perp_positions.iter().filter(|p| p.is_open_position()) {
        if let Ok(market) = velocity.try_get_perp_market_account(p.market_index) {
            debug_log.push_str(&format!("perpMarket-{}:", p.market_index,));
            base64::engine::general_purpose::STANDARD.encode_string(
                velocity_rs::utils::zero_account_to_bytes(market),
                &mut debug_log,
            );
            debug_log.push('|');
        }
        if let Some(oracle) =
            velocity.try_get_oracle_price_data_and_slot(MarketId::perp(p.market_index))
        {
            debug_log.push_str(&format!("oracle-{:?}-{}:", oracle.source, oracle.pubkey));
            base64::engine::general_purpose::STANDARD.encode_string(oracle.raw, &mut debug_log);
            debug_log.push('|');
        }
    }

    if let Ok(market) = velocity.try_get_perp_market_account(taker_order_params.market_index) {
        debug_log.push_str(&format!("perpMarket-{}:", taker_order_params.market_index,));
        base64::engine::general_purpose::STANDARD.encode_string(
            velocity_rs::utils::zero_account_to_bytes(market),
            &mut debug_log,
        );
        debug_log.push('|');
    }

    if let Some(oracle) =
        velocity.try_get_oracle_price_data_and_slot(MarketId::perp(taker_order_params.market_index))
    {
        debug_log.push_str(&format!("oracle-{:?}-{}:", oracle.source, oracle.pubkey,));
        base64::engine::general_purpose::STANDARD.encode_string(oracle.raw, &mut debug_log);
        debug_log.push('|');
    }

    let compressed = zstd::encode_all(debug_log.as_bytes(), 0).expect("encoded");
    log::debug!(target: "accountState", "{}", base64::engine::general_purpose::STANDARD.encode(compressed));
}

/// Simulate the tx on remote RPC node
pub async fn simulate_tx(
    velocity: &VelocityClient,
    tx: VersionedMessage,
    accounts: &[Pubkey],
) -> SdkResult<RpcSimulateTransactionResult> {
    let response = velocity
        .rpc()
        .simulate_transaction_with_config(
            &VersionedTransaction {
                message: tx,
                // must provide a signature for the RPC call to work
                signatures: vec![Signature::new_unique()],
            },
            RpcSimulateTransactionConfig {
                sig_verify: false,
                replace_recent_blockhash: true,
                accounts: Some(RpcSimulateTransactionAccountsConfig {
                    encoding: Some(UiAccountEncoding::Base64Zstd),
                    addresses: accounts.iter().map(|x| x.to_string()).collect(),
                }),
                ..Default::default()
            },
        )
        .await;
    response.map(|r| r.value).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        ed25519_dalek::Signature as Ed25519Signature,
        solana_native_token::LAMPORTS_PER_SOL,
        std::collections::HashMap,
        velocity_rs::{
            program::math::time::SlotDuration,
            swift_order_subscriber::expected_network_tag,
            types::{
                accounts::User, SignedMsgOrderParamsDelegateMessage, SignedMsgOrderParamsMessage,
                SignedMsgTriggerOrderParams,
            },
        },
    };

    fn is_isolated_deposit(signed_msg: &SignedOrderType) -> bool {
        match signed_msg {
            SignedOrderType::Delegated { inner, .. } => inner
                .isolated_position_deposit
                .is_some_and(|amount| amount > 0),
            SignedOrderType::Authority { inner, .. } => inner
                .isolated_position_deposit
                .is_some_and(|amount| amount > 0),
        }
    }

    fn create_test_order_params(
        order_type: OrderType,
        market_type: MarketType,
        base_asset_amount: u64,
        direction: PositionDirection,
        auction_params: Option<(u8, i64, i64)>, // (duration, start_price, end_price)
    ) -> OrderParams {
        let (auction_duration, auction_start_price, auction_end_price) =
            auction_params.unwrap_or((0, 0, 0));
        OrderParams {
            market_index: 0,
            market_type,
            order_type,
            base_asset_amount,
            price: 1_000,
            direction,
            auction_duration: if auction_duration > 0 {
                Some(auction_duration)
            } else {
                None
            },
            auction_start_price: if auction_start_price > 0 {
                Some(auction_start_price)
            } else {
                None
            },
            auction_end_price: if auction_end_price > 0 {
                Some(auction_end_price)
            } else {
                None
            },
            ..Default::default()
        }
    }

    #[test]
    fn test_auction_within_oracle_band() {
        let oracle = 10_000_i64; // arbitrary price units; the check is proportional
        let band_bps = 300; // 3% -> band of 300 price units

        // Absolute (Market) auction hugging oracle: start -1%, end +1% -> inside.
        let near = create_test_order_params(
            OrderType::Market,
            MarketType::Perp,
            1_000_000_000,
            PositionDirection::Long,
            Some((5, 9_900, 10_100)),
        );
        assert!(auction_within_oracle_band(&near, oracle, band_bps));

        // Fat-finger end at +10% -> outside the band -> rejected.
        let far = create_test_order_params(
            OrderType::Market,
            MarketType::Perp,
            1_000_000_000,
            PositionDirection::Long,
            Some((5, 9_900, 11_000)),
        );
        assert!(!auction_within_oracle_band(&far, oracle, band_bps));

        // band_bps == 0 disables the guard entirely.
        assert!(auction_within_oracle_band(&far, oracle, 0));

        // A resting limit with no auction has nothing to bound.
        let no_auction = create_test_order_params(
            OrderType::Limit,
            MarketType::Perp,
            1_000_000_000,
            PositionDirection::Long,
            None,
        );
        assert!(auction_within_oracle_band(&no_auction, oracle, band_bps));

        // Oracle-type auctions carry oracle-relative offsets: +2% end offset is
        // inside, +5% is outside — no dependence on absolute oracle level.
        let offset_near = create_test_order_params(
            OrderType::Oracle,
            MarketType::Perp,
            1_000_000_000,
            PositionDirection::Long,
            Some((5, 1, 200)),
        );
        assert!(auction_within_oracle_band(&offset_near, oracle, band_bps));

        let offset_far = create_test_order_params(
            OrderType::Oracle,
            MarketType::Perp,
            1_000_000_000,
            PositionDirection::Long,
            Some((5, 1, 500)),
        );
        assert!(!auction_within_oracle_band(&offset_far, oracle, band_bps));
    }

    #[test]
    fn test_validate_market_type() {
        let min_order_size = 1 * LAMPORTS_PER_SOL;

        // Test valid market type
        let params = create_test_order_params(
            OrderType::Market,
            MarketType::Perp,
            min_order_size,
            PositionDirection::Long,
            Some((1, 99, 100)),
        );
        assert!(validate_signed_order_params(&params, min_order_size).is_ok());

        // Test invalid market type
        let params = create_test_order_params(
            OrderType::Market,
            MarketType::Spot,
            min_order_size,
            PositionDirection::Long,
            Some((1, 99, 100)),
        );
        assert_eq!(
            validate_signed_order_params(&params, min_order_size),
            Err(ErrorCode::InvalidOrderMarketType)
        );
    }

    #[test]
    fn test_validate_order_size() {
        let min_order_size = 1 * LAMPORTS_PER_SOL;

        // Test valid order size
        let params = create_test_order_params(
            OrderType::Market,
            MarketType::Perp,
            min_order_size,
            PositionDirection::Long,
            Some((1, 99, 100)),
        );
        assert!(validate_signed_order_params(&params, min_order_size).is_ok());

        // Test invalid order size
        let params = create_test_order_params(
            OrderType::Market,
            MarketType::Perp,
            min_order_size - 1,
            PositionDirection::Long,
            None,
        );
        assert_eq!(
            validate_signed_order_params(&params, min_order_size),
            Err(ErrorCode::InvalidOrderSizeTooSmall)
        );
    }

    #[test]
    fn test_validate_auction_params() {
        let min_order_size = 1 * LAMPORTS_PER_SOL;

        // Test valid auction params for long position
        let params = create_test_order_params(
            OrderType::Limit,
            MarketType::Perp,
            min_order_size,
            PositionDirection::Long,
            Some((100, 1000, 1100)), // start < end for long
        );
        assert!(validate_signed_order_params(&params, min_order_size).is_ok());

        // Test valid auction params for short position
        let params = create_test_order_params(
            OrderType::Limit,
            MarketType::Perp,
            min_order_size,
            PositionDirection::Short,
            Some((100, 1100, 1000)), // start > end for short
        );
        assert!(validate_signed_order_params(&params, min_order_size).is_ok());

        // Test invalid auction params for long position
        let params = create_test_order_params(
            OrderType::Limit,
            MarketType::Perp,
            min_order_size,
            PositionDirection::Long,
            Some((100, 1100, 1000)), // start > end for long (invalid)
        );
        assert_eq!(
            validate_signed_order_params(&params, min_order_size),
            Err(ErrorCode::InvalidOrderAuction)
        );

        // Test invalid auction params for short position
        let params = create_test_order_params(
            OrderType::Limit,
            MarketType::Perp,
            min_order_size,
            PositionDirection::Short,
            Some((100, 1000, 1100)), // start < end for short (invalid)
        );
        assert_eq!(
            validate_signed_order_params(&params, min_order_size),
            Err(ErrorCode::InvalidOrderAuction)
        );

        // Test limit order with no auction params
        let params = create_test_order_params(
            OrderType::Limit,
            MarketType::Perp,
            min_order_size,
            PositionDirection::Long,
            None,
        );
        assert!(validate_signed_order_params(&params, min_order_size).is_ok());

        let params = create_test_order_params(
            OrderType::Limit,
            MarketType::Perp,
            min_order_size,
            PositionDirection::Long,
            Some((100, 1000, 1100)),
        );
        assert_eq!(
            validate_signed_order_params(&params, min_order_size),
            Ok(())
        );
    }

    #[test]
    fn test_request_context_from_incoming_message_valid_utf8() {
        let taker = Pubkey::new_unique();
        let uuid_valid: [u8; 8] = [b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h'];
        let authority_msg = SignedOrderType::authority(SignedMsgOrderParamsMessage {
            sub_account_id: 0,
            signed_msg_order_params: OrderParams {
                market_index: 2,
                market_type: MarketType::Perp,
                order_type: OrderType::Market,
                base_asset_amount: LAMPORTS_PER_SOL,
                price: 1000,
                direction: PositionDirection::Long,
                ..Default::default()
            },
            uuid: uuid_valid,
            slot: 1000,
            stop_loss_order_params: None,
            take_profit_order_params: None,
            max_margin_ratio: None,
            builder_fee_tenth_bps: None,
            builder_idx: None,
            isolated_position_deposit: None,
            network: Some(expected_network_tag()),
            route: None,
        });
        let msg = IncomingSignedMessage {
            taker_pubkey: taker,
            signature: Ed25519Signature::from_bytes(&[0u8; 64]).unwrap(),
            message: authority_msg,
            signing_authority: Pubkey::default(),
            taker_authority: Pubkey::default(),
        };
        let ctx = RequestContext::from_incoming_message(&msg).expect("valid utf8 uuid");
        assert_eq!(ctx.order_uuid, "abcdefgh");
        assert_eq!(ctx.market_index, 2);
        assert_eq!(ctx.market_type, "perp");
        assert_eq!(ctx.taker_authority, taker);
    }

    #[test]
    fn test_request_context_from_incoming_message_invalid_utf8() {
        let taker = Pubkey::new_unique();
        let uuid_invalid: [u8; 8] = [0xFF; 8];
        let authority_msg = SignedOrderType::authority(SignedMsgOrderParamsMessage {
            sub_account_id: 0,
            signed_msg_order_params: OrderParams {
                market_index: 0,
                market_type: MarketType::Perp,
                order_type: OrderType::Market,
                base_asset_amount: LAMPORTS_PER_SOL,
                price: 1000,
                direction: PositionDirection::Long,
                ..Default::default()
            },
            uuid: uuid_invalid,
            slot: 1000,
            stop_loss_order_params: None,
            take_profit_order_params: None,
            max_margin_ratio: None,
            builder_fee_tenth_bps: None,
            builder_idx: None,
            isolated_position_deposit: None,
            network: Some(expected_network_tag()),
            route: None,
        });
        let msg = IncomingSignedMessage {
            taker_pubkey: taker,
            signature: Ed25519Signature::from_bytes(&[0u8; 64]).unwrap(),
            message: authority_msg,
            signing_authority: Pubkey::default(),
            taker_authority: Pubkey::default(),
        };
        assert!(RequestContext::from_incoming_message(&msg).is_err());
    }

    #[test]
    fn test_extract_signed_message_info_delegated() {
        let taker_authority = Pubkey::new_unique();
        let current_slot = 1000;

        // Test successful case
        let delegated_msg = SignedOrderType::delegated(SignedMsgOrderParamsDelegateMessage {
            taker_pubkey: Pubkey::new_unique(),
            signed_msg_order_params: OrderParams {
                market_index: 0,
                market_type: MarketType::Perp,
                order_type: OrderType::Market,
                base_asset_amount: LAMPORTS_PER_SOL,
                price: 1000,
                direction: PositionDirection::Long,
                ..Default::default()
            },
            uuid: [1; 8],
            slot: current_slot,
            stop_loss_order_params: None,
            take_profit_order_params: None,
            max_margin_ratio: None,
            builder_fee_tenth_bps: None,
            builder_idx: None,
            isolated_position_deposit: None,
            network: Some(expected_network_tag()),
            route: None,
        });

        let result = extract_signed_message_info(
            &delegated_msg,
            &taker_authority,
            current_slot,
            SlotClock::baseline(),
        );
        assert!(result.is_ok_and(|(info, _, _)| {
            info.slot == current_slot
                && info.order_params.base_asset_amount == LAMPORTS_PER_SOL
                && info.order_params.order_type == OrderType::Market
        }));

        // Test invalid order amount case
        let delegated_msg = SignedOrderType::delegated(SignedMsgOrderParamsDelegateMessage {
            taker_pubkey: Pubkey::new_unique(),
            signed_msg_order_params: OrderParams {
                market_index: 0,
                market_type: MarketType::Perp,
                order_type: OrderType::Market,
                base_asset_amount: LAMPORTS_PER_SOL,
                price: 1000,
                direction: PositionDirection::Long,
                ..Default::default()
            },
            uuid: [1; 8],
            slot: current_slot,
            stop_loss_order_params: Some(SignedMsgTriggerOrderParams {
                base_asset_amount: 0,
                ..Default::default()
            }),
            take_profit_order_params: None,
            max_margin_ratio: None,
            builder_fee_tenth_bps: None,
            builder_idx: None,
            isolated_position_deposit: None,
            network: Some(expected_network_tag()),
            route: None,
        });

        let result = extract_signed_message_info(
            &delegated_msg,
            &taker_authority,
            current_slot,
            SlotClock::baseline(),
        );
        assert!(result.is_err_and(|x| {
            x.0 == axum::http::StatusCode::BAD_REQUEST
                && x.1.message == PROCESS_ORDER_RESPONSE_ERROR_MSG_INVALID_ORDER_AMOUNT
                && x.1.error.is_none()
        }));
    }

    #[test]
    fn test_extract_signed_message_info_authority() {
        let taker_authority = Pubkey::new_unique();
        let current_slot = 1000;
        let sub_account_id = 1;

        // Test successful case
        let authority_msg = SignedOrderType::authority(SignedMsgOrderParamsMessage {
            sub_account_id,
            signed_msg_order_params: OrderParams {
                market_index: 0,
                market_type: MarketType::Perp,
                order_type: OrderType::Market,
                base_asset_amount: LAMPORTS_PER_SOL,
                price: 1000,
                direction: PositionDirection::Long,
                ..Default::default()
            },
            uuid: [1; 8],
            slot: current_slot,
            stop_loss_order_params: None,
            take_profit_order_params: None,
            max_margin_ratio: None,
            builder_fee_tenth_bps: None,
            builder_idx: None,
            isolated_position_deposit: None,
            network: Some(expected_network_tag()),
            route: None,
        });

        let result = extract_signed_message_info(
            &authority_msg,
            &taker_authority,
            current_slot,
            SlotClock::baseline(),
        );
        assert!(result.is_ok_and(|(info, _margin_ratio, _is_isolated)| {
            info.slot == current_slot
                && info.order_params.base_asset_amount == LAMPORTS_PER_SOL
                && info.order_params.order_type == OrderType::Market
                && info.taker_pubkey
                    == Wallet::derive_user_account(&taker_authority, sub_account_id)
        }));

        // Test invalid order amount case
        let authority_msg = SignedOrderType::authority(SignedMsgOrderParamsMessage {
            sub_account_id,
            signed_msg_order_params: OrderParams {
                market_index: 0,
                market_type: MarketType::Perp,
                order_type: OrderType::Market,
                base_asset_amount: LAMPORTS_PER_SOL,
                price: 1000,
                direction: PositionDirection::Long,
                ..Default::default()
            },
            uuid: [1; 8],
            slot: current_slot,
            stop_loss_order_params: None,
            take_profit_order_params: Some(SignedMsgTriggerOrderParams {
                base_asset_amount: 0,
                ..Default::default()
            }),
            max_margin_ratio: None,
            builder_fee_tenth_bps: None,
            builder_idx: None,
            isolated_position_deposit: None,
            network: Some(expected_network_tag()),
            route: None,
        });

        let result = extract_signed_message_info(
            &authority_msg,
            &taker_authority,
            current_slot,
            SlotClock::baseline(),
        );
        assert!(result.is_err_and(|x| {
            x.0 == axum::http::StatusCode::BAD_REQUEST
                && x.1.message == PROCESS_ORDER_RESPONSE_ERROR_MSG_INVALID_ORDER_AMOUNT
                && x.1.error.is_none()
        }));
    }

    #[test]
    fn test_extract_signed_message_info_slot_validation() {
        let taker_authority = Pubkey::new_unique();
        let current_slot = 1000;

        // Test slot too old
        let delegated_msg = SignedOrderType::delegated(SignedMsgOrderParamsDelegateMessage {
            taker_pubkey: Pubkey::new_unique(),
            signed_msg_order_params: OrderParams {
                market_index: 0,
                market_type: MarketType::Perp,
                order_type: OrderType::Market,
                base_asset_amount: LAMPORTS_PER_SOL,
                price: 1000,
                direction: PositionDirection::Long,
                ..Default::default()
            },
            uuid: [1; 8],
            slot: current_slot - 501, // Slot too old
            stop_loss_order_params: None,
            take_profit_order_params: None,
            max_margin_ratio: None,
            builder_fee_tenth_bps: None,
            builder_idx: None,
            isolated_position_deposit: None,
            network: Some(expected_network_tag()),
            route: None,
        });

        let result = extract_signed_message_info(
            &delegated_msg,
            &taker_authority,
            current_slot,
            SlotClock::baseline(),
        );
        assert!(result.is_err_and(|x| x
            == (
                axum::http::StatusCode::BAD_REQUEST,
                ProcessOrderResponse {
                    message: PROCESS_ORDER_RESPONSE_ERROR_MSG_ORDER_SLOT_TOO_OLD,
                    error: Some(PROCESS_ORDER_RESPONSE_ERROR_MSG_ORDER_SLOT_TOO_OLD.into())
                }
            )));
    }

    #[test]
    fn test_is_isolated_deposit() {
        let taker_authority = Pubkey::new_unique();
        let current_slot = 1000;

        // Test delegated order with no isolated deposit
        let delegated_msg = SignedOrderType::delegated(SignedMsgOrderParamsDelegateMessage {
            taker_pubkey: Pubkey::new_unique(),
            signed_msg_order_params: OrderParams {
                market_index: 0,
                market_type: MarketType::Perp,
                order_type: OrderType::Market,
                base_asset_amount: LAMPORTS_PER_SOL,
                price: 1000,
                direction: PositionDirection::Long,
                ..Default::default()
            },
            uuid: [1; 8],
            slot: current_slot,
            stop_loss_order_params: None,
            take_profit_order_params: None,
            max_margin_ratio: None,
            builder_fee_tenth_bps: None,
            builder_idx: None,
            isolated_position_deposit: None,
            network: Some(expected_network_tag()),
            route: None,
        });
        assert!(!is_isolated_deposit(&delegated_msg));
        let result = extract_signed_message_info(
            &delegated_msg,
            &taker_authority,
            current_slot,
            SlotClock::baseline(),
        );
        assert!(result.is_ok_and(|(_, _, is_isolated)| is_isolated.is_none()));

        // Test delegated order with isolated deposit of 0 (should be false)
        let delegated_msg = SignedOrderType::delegated(SignedMsgOrderParamsDelegateMessage {
            taker_pubkey: Pubkey::new_unique(),
            signed_msg_order_params: OrderParams {
                market_index: 0,
                market_type: MarketType::Perp,
                order_type: OrderType::Market,
                base_asset_amount: LAMPORTS_PER_SOL,
                price: 1000,
                direction: PositionDirection::Long,
                ..Default::default()
            },
            uuid: [1; 8],
            slot: current_slot,
            stop_loss_order_params: None,
            take_profit_order_params: None,
            max_margin_ratio: None,
            builder_fee_tenth_bps: None,
            builder_idx: None,
            isolated_position_deposit: Some(0),
            network: Some(expected_network_tag()),
            route: None,
        });
        assert!(!is_isolated_deposit(&delegated_msg));
        let result = extract_signed_message_info(
            &delegated_msg,
            &taker_authority,
            current_slot,
            SlotClock::baseline(),
        );
        // `extract_signed_message_info` returns the raw `isolated_position_deposit` field, so a
        // zero deposit comes back as `Some(0)` (not `None`). The "zero == not isolated" semantics
        // live in `is_isolated_deposit()`, asserted above.
        assert!(result.is_ok_and(|(_, _, is_isolated)| is_isolated == Some(0)));

        // Test delegated order with isolated deposit > 0
        let delegated_msg = SignedOrderType::delegated(SignedMsgOrderParamsDelegateMessage {
            taker_pubkey: Pubkey::new_unique(),
            signed_msg_order_params: OrderParams {
                market_index: 0,
                market_type: MarketType::Perp,
                order_type: OrderType::Market,
                base_asset_amount: LAMPORTS_PER_SOL,
                price: 1000,
                direction: PositionDirection::Long,
                ..Default::default()
            },
            uuid: [1; 8],
            slot: current_slot,
            stop_loss_order_params: None,
            take_profit_order_params: None,
            max_margin_ratio: None,
            builder_fee_tenth_bps: None,
            builder_idx: None,
            isolated_position_deposit: Some(100_000_000), // 0.1 SOL
            network: Some(expected_network_tag()),
            route: None,
        });
        assert!(is_isolated_deposit(&delegated_msg));
        let result = extract_signed_message_info(
            &delegated_msg,
            &taker_authority,
            current_slot,
            SlotClock::baseline(),
        );
        assert!(result.is_ok_and(|(_, _, is_isolated)| is_isolated.is_some()));

        // Test authority order with no isolated deposit
        let authority_msg = SignedOrderType::authority(SignedMsgOrderParamsMessage {
            sub_account_id: 0,
            signed_msg_order_params: OrderParams {
                market_index: 0,
                market_type: MarketType::Perp,
                order_type: OrderType::Market,
                base_asset_amount: LAMPORTS_PER_SOL,
                price: 1000,
                direction: PositionDirection::Long,
                ..Default::default()
            },
            uuid: [1; 8],
            slot: current_slot,
            stop_loss_order_params: None,
            take_profit_order_params: None,
            max_margin_ratio: None,
            builder_fee_tenth_bps: None,
            builder_idx: None,
            isolated_position_deposit: None,
            network: Some(expected_network_tag()),
            route: None,
        });
        assert!(!is_isolated_deposit(&authority_msg));
        let result = extract_signed_message_info(
            &authority_msg,
            &taker_authority,
            current_slot,
            SlotClock::baseline(),
        );
        assert!(result.is_ok_and(|(_, _, is_isolated)| is_isolated.is_none()));

        // Test authority order with isolated deposit > 0
        let authority_msg = SignedOrderType::authority(SignedMsgOrderParamsMessage {
            sub_account_id: 0,
            signed_msg_order_params: OrderParams {
                market_index: 0,
                market_type: MarketType::Perp,
                order_type: OrderType::Market,
                base_asset_amount: LAMPORTS_PER_SOL,
                price: 1000,
                direction: PositionDirection::Long,
                ..Default::default()
            },
            uuid: [1; 8],
            slot: current_slot,
            stop_loss_order_params: None,
            take_profit_order_params: None,
            max_margin_ratio: None,
            builder_fee_tenth_bps: None,
            builder_idx: None,
            isolated_position_deposit: Some(50_000_000), // 0.05 SOL
            network: Some(expected_network_tag()),
            route: None,
        });
        assert!(is_isolated_deposit(&authority_msg));
        let result = extract_signed_message_info(
            &authority_msg,
            &taker_authority,
            current_slot,
            SlotClock::baseline(),
        );
        assert!(result.is_ok_and(|(_, _, is_isolated)| is_isolated.is_some()));
    }

    #[cfg(feature = "rpc_tests")]
    #[tokio::test]
    async fn test_simulate_taker_order_rpc() {
        let _ = env_logger::try_init();
        // Create mock server params
        let velocity = VelocityClient::new(
            velocity_rs::Context::DevNet,
            RpcClient::new("https://api.devnet.solana.com".to_string()),
            Keypair::new().into(),
        )
        .await
        .unwrap();

        let taker_pubkey = Keypair::new().pubkey();
        let taker_pubkey2 = Keypair::new().pubkey();
        let delegate_pubkey = Keypair::new().pubkey();
        let users: HashMap<Pubkey, User> = [
            (
                taker_pubkey,
                User {
                    authority: taker_pubkey,
                    delegate: Pubkey::default(),
                    ..Default::default()
                },
            ),
            (
                taker_pubkey2,
                User {
                    authority: taker_pubkey2,
                    delegate: delegate_pubkey,
                    ..Default::default()
                },
            ),
        ]
        .into();

        dbg!(users.contains_key(&taker_pubkey));
        dbg!(users.contains_key(&taker_pubkey2));

        let redis_pool = redis::Client::open("redis://localhost:6379")
            .expect("valid redis URL")
            .get_multiplexed_tokio_connection()
            .await
            .expect("redis connected");
        let server_params = ServerParams {
            slot_subscriber: Arc::new(SuperSlotSubscriber::new(vec![], velocity.rpc())),
            metrics: SwiftServerMetrics::new(),
            user_account_fetcher: UserAccountFetcher::mock(users),
            config: Arc::new(crate::swift_server::Config::from_env()),
            attest: crate::attest::AttestContext::from_env(),
            route: crate::route::RouteContext::new(
                velocity.rpc().url(),
                velocity_rs::constants::PROGRAM_ID,
            ),
            velocity,
            farmer_pubkeys: Default::default(),
            redis_pool,
            rpc_health_cache: RpcHealthCache::default(),
        };

        // Create mock order params
        let order_params = OrderParams {
            market_index: 0,
            market_type: MarketType::Perp,
            order_type: OrderType::Market,
            base_asset_amount: 1 * LAMPORTS_PER_SOL,
            price: 1_000,
            direction: PositionDirection::Short,
            ..Default::default()
        };

        // Test
        let context_primary = RequestContext {
            recv_ts: unix_now_ms(),
            log_prefix: format!("[test-order {}]", taker_pubkey),
            market_index: order_params.market_index,
            market_type: "perp",
            taker_authority: taker_pubkey,
            order_uuid: "TESTORD0".into(),
        };

        let result = server_params
            .simulate_taker_order_rpc(
                &taker_pubkey,
                &order_params,
                Some(&delegate_pubkey),
                1_000,
                None,
                None,
                &context_primary,
            )
            .await;
        assert!(result.is_err_and(|(status, msg, _)| {
            dbg!(&msg);
            status == axum::http::StatusCode::BAD_REQUEST
                && msg.contains("signer is not configured delegate")
        }));

        let context_secondary = RequestContext {
            recv_ts: unix_now_ms(),
            log_prefix: format!("[test-order {}]", taker_pubkey2),
            market_index: order_params.market_index,
            market_type: "perp",
            taker_authority: taker_pubkey2,
            order_uuid: "TESTORD1".into(),
        };

        let result = server_params
            .simulate_taker_order_rpc(
                &taker_pubkey2,
                &order_params,
                Some(&delegate_pubkey),
                1_000,
                None,
                None,
                &context_secondary,
            )
            .await;
        // it fails later at remote sim since the account is not a real velocity
        // account (surfaces as a program error, e.g. 3007 AccountOwnedByWrongProgram,
        // formatted "invalid order. error code: ..."). Match the stable "invalid order"
        // prefix so a delegate-rejection regression (different message) still fails here.
        assert!(result.is_err_and(|(status, msg, _)| {
            dbg!(&msg);
            status == axum::http::StatusCode::BAD_REQUEST && msg.contains("invalid order")
        }));

        // Tear down the client's auto-started account/market subscriptions before the
        // test ends. VelocityClient::new spawns background polling tasks; if one's RPC
        // call is in flight (its DNS resolves on an uncancellable tokio blocking thread)
        // when the test's runtime is dropped, runtime teardown blocks on that thread and
        // the test binary never exits. unsubscribe() stops the polling tasks (async
        // cleanup that can't live in Drop).
        server_params.velocity.unsubscribe().await.unwrap();
    }
}
