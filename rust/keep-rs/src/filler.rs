//! Filler Bot
use {
    crate::{
        http::{FeedHealth, Metrics},
        util::{
            is_resting_swift_limit, should_poll_swift, swift_order_expired,
            swift_slot_wait_if_known, OrderSlotLimiter, PendingTxMeta, PendingTxs,
            PerpFillFallback, PythPriceUpdate, SwiftSlotWait, TxIntent,
        },
        Config, UseMarkets,
    },
    anchor_lang::Discriminator,
    dashmap::DashMap,
    futures_util::StreamExt,
    solana_account_decoder_client_types::UiAccountEncoding,
    solana_compute_budget_interface::id as compute_budget_id,
    solana_instruction::{error::InstructionError, AccountMeta},
    solana_rpc_client_api::{
        config::{RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcTransactionConfig},
        response::RpcSimulateTransactionResult,
    },
    solana_signature::Signature,
    solana_transaction::TransactionError,
    solana_transaction_status_client_types::{UiTransactionEncoding, UiTransactionError},
    std::{
        collections::{BTreeMap, HashSet},
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    },
    tokio::{runtime::Handle, sync::RwLock},
    velocity_quoter_health::{
        attribute,
        observe::{Observation, Report},
        RouteContext,
    },
    velocity_rs::{
        constants::{derive_quoter_slab, PROGRAM_ID},
        dlob::{DLOBNotifier, DLOB},
        event_subscriber::{parse_velocity_logs, VelocityEvent},
        grpc::{
            grpc_subscriber::{AccountFilter, GrpcConnectionOpts},
            AccountUpdate, TransactionUpdate,
        },
        priority_fee_subscriber::PriorityFeeSubscriber,
        program::{
            math::time::Millis,
            state::prop_amm::{ClobUserRefV0, QuoterConfigV0, QuoterSlotV0},
            FlowAttestationV0,
        },
        slot_clock_from_state,
        swift_order_subscriber::{SignedOrderInfo, SwiftOrderStream},
        types::{
            accounts::User, CommitmentConfig, MarketId, MarketStatus, OrderType, PositionDirection,
            RpcSendTransactionConfig, SdkResult, StateExt, VersionedMessage, VersionedTransaction,
        },
        utils::clob_slot_config,
        ClobFillAccounts, GrpcSubscribeOpts, Pubkey, TransactionBuilder, VelocityClient, Wallet,
    },
};

const TARGET: &str = "filler";

pub struct FillerBot {
    velocity: VelocityClient,
    dlob: &'static DLOB,
    filler_subaccount: Pubkey,
    slot_rx: tokio::sync::mpsc::Receiver<u64>,
    swift_order_stream: SwiftOrderStream,
    limiter: OrderSlotLimiter<40>,
    market_ids: Vec<MarketId>,
    config: Config,
    tx_worker_ref: TxSender,
    priority_fee_subscriber: Arc<PriorityFeeSubscriber>,
    pyth_price_feed: Option<tokio::sync::mpsc::Receiver<PythPriceUpdate>>,
    metrics: Arc<Metrics>,
    feed_health: Arc<FeedHealth>,
}

impl FillerBot {
    pub async fn new(
        config: Config,
        velocity: VelocityClient,
        metrics: Arc<Metrics>,
        feed_health: Arc<FeedHealth>,
    ) -> Self {
        let dlob: &'static DLOB = Box::leak(Box::new(DLOB::default()));
        let tx_worker = TxWorker::new(
            velocity.clone(),
            metrics.clone(),
            config.dry,
            None,
            None,
            None,
            None,
        );
        let rt = tokio::runtime::Handle::current();
        let tx_worker_ref = tx_worker.run(rt);

        let mut market_ids = match config.use_markets() {
            UseMarkets::All => velocity.get_all_perp_market_ids(),
            UseMarkets::Subset(m) => m,
        };
        // remove bet perp markets
        market_ids.retain(|x| {
            let market = velocity
                .program_data()
                .perp_market_config_by_index(x.index())
                .unwrap();
            let name = core::str::from_utf8(&market.name)
                .unwrap()
                .to_ascii_lowercase();

            !name.contains("bet") && market.status != MarketStatus::Initialized
        });

        let market_pubkeys: Vec<Pubkey> = market_ids
            .iter()
            .map(|x| {
                velocity
                    .program_data()
                    .perp_market_config_by_index(x.index())
                    .unwrap()
                    .pubkey
            })
            .collect();

        let priority_fee_subscriber =
            PriorityFeeSubscriber::new(velocity.rpc().url(), &market_pubkeys);
        let priority_fee_subscriber = priority_fee_subscriber.subscribe();

        let filler_subaccount = velocity.wallet.sub_account(config.sub_account_id);

        // SWIFT_WS_URL overrides the swift ws server base url (velocity-rs appends
        // `/ws?pubkey=`). None => SDK default (SWIFT_DEVNET_WS_URL /
        // SWIFT_MAINNET_WS_URL, the in-cluster velocity swift hosts).
        let swift_ws_url = std::env::var("SWIFT_WS_URL").ok();
        log::info!(target: TARGET, "subscribing swift orders (ws url override: {swift_ws_url:?})");
        let swift_order_stream = velocity
            .subscribe_swift_orders(&market_ids, Some(true), None, swift_ws_url)
            .await
            .expect("subscribed swift orders");
        feed_health.set_swift_connected(true);
        log::info!(target: TARGET, "subscribed swift orders");

        velocity.subscribe_blockhashes().await.expect("subscribed");
        let slot_rx = setup_grpc(
            velocity.clone(),
            dlob,
            tx_worker_ref.clone(),
            market_ids.clone(),
            filler_subaccount,
        )
        .await;
        // start the grpc liveness clock at subscription time so a feed that never
        // delivers a single slot still trips the health check
        feed_health.touch_slot();
        log::info!(target: TARGET, "subscribed gRPC");

        let pyth_price_feed = if !config.no_pyth {
            let pyth_access_token = std::env::var("PYTH_LAZER_TOKEN").expect("pyth access token");
            let pyth_feed_cli = pyth_lazer_client::LazerClient::new(
                "wss://pyth-lazer.dourolabs.app/v1/stream",
                pyth_access_token.as_str(),
            )
            .expect("pyth price feed connects");
            let feed = crate::util::subscribe_price_feeds(pyth_feed_cli, &market_ids, &[], &[]);
            // Start the liveness clock at subscription time, so a feed that never
            // delivers an update still fails the health check.
            feed_health.touch_pyth();
            log::info!(target: TARGET, "subscribed pyth price feeds");
            Some(feed)
        } else {
            log::info!(target: TARGET, "pyth price feed disabled");
            None
        };

        FillerBot {
            velocity,
            dlob,
            filler_subaccount,
            slot_rx,
            swift_order_stream,
            limiter: OrderSlotLimiter::new(),
            market_ids,
            config,
            tx_worker_ref,
            priority_fee_subscriber,
            pyth_price_feed,
            metrics,
            feed_health,
        }
    }

    pub async fn run(self) {
        let mut swift_order_stream = self.swift_order_stream;
        let mut slot_rx = self.slot_rx;
        let _limiter = self.limiter;
        let velocity: &'static VelocityClient = Box::leak(Box::new(self.velocity));
        // Attested flow runs only when the chain names a flow authority and a
        // swift endpoint is reachable. Without either one, fills run unattested.
        let attest: Option<&'static crate::attest::AttestClient> = velocity
            .state_account()
            .ok()
            .map(|state| state.hot_flow_authority)
            .filter(|flow| *flow != Pubkey::default())
            .and_then(crate::attest::AttestClient::from_env)
            .map(|client| &*Box::leak(Box::new(client)));
        let dlob = self.dlob;
        let market_ids = self.market_ids;
        let filler_subaccount = self.filler_subaccount;
        let config = self.config.clone();
        let tx_worker_ref = self.tx_worker_ref.clone();
        let priority_fee_subscriber = Arc::clone(&self.priority_fee_subscriber);
        let metrics = Arc::clone(&self.metrics);
        let feed_health = Arc::clone(&self.feed_health);
        // reused per-slot scratch buffer for triggerable order ids (avoids per-slot allocation)
        let _triggerable_buf: Vec<(Pubkey, u32)> = Vec::new();
        // Refresh the state config on elapsed slots and not on `slot % N == 0`. A
        // skipped exact multiple would stall the refresh for another window.
        const CONFIG_REFRESH_SLOTS: u64 = 300;
        let mut last_config_refresh_slot: u64 = 0;
        let mut use_median_trigger_price = velocity
            .state_account()
            .map(|s| s.has_median_trigger_price_feature())
            .unwrap_or(false);
        // Seed with the real chain slot, so a bot started after a gate switch reads
        // the new value at once and not only after the first config refresh.
        let startup_slot = velocity.get_slot().await;
        let mut slot = startup_slot.unwrap_or(0);
        let mut slot_is_known = startup_slot.is_some();
        let mut slot_clock = velocity.slot_clock();
        dlob.update_slot_clock(slot_clock);
        // The AMM staleness window as wall clock. The on-chain value is in 400ms
        // baseline units. The comparison sites integrate oracle age across slot
        // duration regimes, which mirrors `oracle_validity`.
        let mut stale_for_amm_threshold = velocity
            .state_account()
            .map(|s| {
                Millis::from_stored_units(
                    s.oracle_guard_rails
                        .validity
                        .slots_before_stale_for_amm
                        .max(0) as u64,
                )
            })
            .unwrap_or(Millis::from_stored_units(10));
        let mut pyth_oracle_prices = BTreeMap::<u16, PythPriceUpdate>::new();
        // per-market consecutive perp-market/oracle cache-miss counters (see slot loop)
        let _cache_misses = BTreeMap::<u16, u32>::new();
        // Per-market last-known oracle-stale state. Staleness is logged on transition
        // (fresh<->stale) instead of every slot, so a stale oracle shows as two edges
        // rather than a wall of per-slot lines during the exact window you're debugging.
        let _oracle_stale_state = BTreeMap::<u16, bool>::new();
        // Per-market last-known pyth-price-stale state, for the same transition-only
        // logging as `oracle_stale_state`.
        let _pyth_price_stale_state = BTreeMap::<u16, bool>::new();

        // Create a dummy receiver that never sends when pyth is disabled
        let (_dummy_tx, dummy_rx) = tokio::sync::mpsc::channel::<PythPriceUpdate>(1);
        let mut pyth_price_feed = self.pyth_price_feed.unwrap_or(dummy_rx);

        // Swift reconnect backoff state (reset on successful resubscribe / first order)
        let mut retries = 0u32;
        // A disconnected stream stays ready forever, and this select is `biased`, so
        // polling it would block every arm below it, slots included, for as long as the
        // feed stayed down. Gate the arm on liveness instead, and drive the retry from
        // its own timer.
        let mut swift_feed_live = true;
        let mut swift_reconnect_at = tokio::time::Instant::now();

        // Swift-feed liveness. A half-open ws never yields an error or `None` — the
        // stream just goes quiet, which on the order channel is indistinguishable from
        // a quiet market (server heartbeats are consumed inside the SDK). Reconnecting
        // is cheap, so after SWIFT_FEED_STALE_LIMIT of silence just tear the stream
        // down and resubscribe rather than trusting the socket.
        const SWIFT_FEED_STALE_LIMIT: Duration = Duration::from_secs(300);
        let mut last_swift_msg = std::time::Instant::now();

        // Slot-feed liveness watchdog. Slots arrive ~2.5/s from the gRPC subscription;
        // if the feed dies *silently* (half-open connection — no error, no None, just
        // no more messages) `slot_rx.recv()` pends forever and every per-slot fill
        // pass stops while the process still reports healthy. Same restart policy as
        // MAX_CONSECUTIVE_ORACLE_MISSES: exit so the supervisor restarts the bot with
        // fresh subscriptions.
        const SLOT_FEED_STALE_LIMIT: Duration = Duration::from_secs(60);
        let mut slot_watchdog = tokio::time::interval(Duration::from_secs(15));
        slot_watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_slot_update = std::time::Instant::now();
        // Swift orders whose stamped message slot has not arrived yet, held until it
        // does. The feed delivers each order exactly once, so an order the program
        // cannot accept yet has to be kept somewhere or it is lost.
        let mut deferred_swift_orders: Vec<SignedOrderInfo> = Vec::new();
        // Cap so a stuck slot feed cannot grow the queue without bound.
        const MAX_DEFERRED_SWIFT_ORDERS: usize = 1_024;
        // An auction order stamped further ahead than this is not a signing buffer. The
        // UI's buffer is a few slots. Refuse to hold such an order rather than trust an
        // unbounded future slot. A resting limit is never held. See `swift_slot_wait`.
        const MAX_SWIFT_ORDER_DEFERRAL: Millis = Millis::from_secs(10);
        // Swift orders to run the arrival path on: whatever the feed delivered, plus any
        // deferral whose slot has arrived. Outlives the iteration, so an arm that
        // short-circuits before the processing pass cannot lose an order.
        let mut swift_orders: Vec<SignedOrderInfo> = Vec::new();
        // When a buffered slot and a ready Swift order both wait, alternate which
        // stream the biased select lets win. Without this, a busy Swift feed or a
        // permanent slot backlog blocks the other stream.
        let mut prefer_slot_on_contention = true;
        loop {
            // Non-blocking because consuming here would skip the per-slot fill work in
            // the select arm below.
            let slot_update_pending = !slot_rx.is_empty();
            let poll_swift = should_poll_swift(
                swift_feed_live,
                slot_update_pending,
                prefer_slot_on_contention,
            );
            // Matured deferrals rejoin the arrival path. At the top of the iteration so
            // no select arm can skip it.
            if !deferred_swift_orders.is_empty() {
                let mut waiting = Vec::with_capacity(deferred_swift_orders.len());
                for order in deferred_swift_orders.drain(..) {
                    match swift_slot_wait_if_known(
                        order.slot(),
                        slot_is_known.then_some(slot),
                        MAX_SWIFT_ORDER_DEFERRAL,
                        is_resting_swift_limit(&order.order_params()),
                        slot_clock,
                    ) {
                        SwiftSlotWait::Ready => swift_orders.push(order),
                        SwiftSlotWait::Wait => waiting.push(order),
                        SwiftSlotWait::TooFarAhead => {
                            log::warn!(target: TARGET, "deferred swift order is too far ahead of slot {slot}, dropping. uuid={}", order.order_uuid_str());
                            metrics.swift_place_skipped.inc();
                        }
                    }
                }
                deferred_swift_orders = waiting;
            }
            tokio::select! {
                biased;
                swift_order = swift_order_stream.next(), if poll_swift => {
                    prefer_slot_on_contention = true;
                    match swift_order {
                        Some(signed_order) => {
                            // reset
                            retries = 0;
                            last_swift_msg = std::time::Instant::now();
                            // Handled after the select, together with matured deferrals.
                            swift_orders.push(signed_order);
                        }
                        None => {
                            // Reconnect forever with a capped backoff. A retry limit
                            // leaves the bot unable to see swift flow while it still
                            // reports healthy. A swift-server outage longer than the
                            // retry budget must not need a manual restart.
                            feed_health.set_swift_connected(false);
                            swift_feed_live = false;
                            retries += 1;
                            let backoff = 2u64.saturating_pow(retries.min(5)).min(30);
                            log::warn!(target: "swift", "feed disconnected, retry {retries} in {backoff}s");
                            swift_reconnect_at = tokio::time::Instant::now() + Duration::from_secs(backoff);
                        }
                    }
                }
                _ = tokio::time::sleep_until(swift_reconnect_at), if !swift_feed_live => {
                    // Keep the same websocket URL override as the first subscription.
                    // Otherwise a reconnect switches to the default host.
                    match velocity
                        .subscribe_swift_orders(&market_ids, Some(true), None, std::env::var("SWIFT_WS_URL").ok())
                        .await
                    {
                        Ok(stream) => {
                            log::info!(target: "swift", "feed resubscribed after {retries} attempt(s)");
                            swift_order_stream = stream;
                            retries = 0;
                            last_swift_msg = std::time::Instant::now();
                            swift_feed_live = true;
                            feed_health.set_swift_connected(true);
                        }
                        Err(e) => {
                            retries += 1;
                            let backoff = 2u64.saturating_pow(retries.min(5)).min(30);
                            log::error!(target: "swift", "resubscribe failed: {e:?}, retry {retries} in {backoff}s");
                            swift_reconnect_at = tokio::time::Instant::now() + Duration::from_secs(backoff);
                        }
                    }
                }
                new_slot = slot_rx.recv() => {
                    if new_slot.is_none() {
                        log::error!(target: TARGET, "slot subscriber failed");
                        break;
                    }
                    slot = new_slot.expect("got slot update");
                    slot_is_known = true;
                    prefer_slot_on_contention = false;
                    last_slot_update = std::time::Instant::now();
                    feed_health.touch_slot();
                    log::trace!(target: TARGET, "got slot update: {slot}");

                    let _priority_fee = priority_fee_subscriber.priority_fee_nth(0.5) + slot % 2; // add entropy to produce unique tx hash on conseuctive tx resubmission
                    let t0 = std::time::SystemTime::now();
                    let _unix_now = t0.duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64;

                    // Check the state config every 1 to 2 minutes, depending on the
                    // slot duration, and count elapsed slots rather than wall clock.
                    // The check runs before any market decision this tick, so one
                    // refresh cannot split a slot's fill decisions across two clocks
                    // or two thresholds.
                    if slot.saturating_sub(last_config_refresh_slot) >= CONFIG_REFRESH_SLOTS {
                        last_config_refresh_slot = slot;
                        // state_account() does a full Borsh parse, so this refreshes on a
                        // timer instead of every slot. A cache miss here keeps the previous
                        // clock. slot_clock() would substitute the 400ms baseline instead.
                        if let Ok(state) = velocity.state_account() {
                            slot_clock = slot_clock_from_state(&state);
                            dlob.update_slot_clock(slot_clock);
                        }
                        use_median_trigger_price = velocity
                            .state_account()
                            .map(|s| s.has_median_trigger_price_feature())
                            .unwrap_or(false);
                        stale_for_amm_threshold = velocity
                            .state_account()
                            .map(|s| {
                                Millis::from_stored_units(
                                    s.oracle_guard_rails.validity.slots_before_stale_for_amm.max(0) as u64,
                                )
                            })
                            .unwrap_or(Millis::from_stored_units(10));
                    }

                    let duration = std::time::SystemTime::now().duration_since(t0).unwrap().as_millis();
                    log::trace!(target: TARGET, "⏱️ checked fills at {slot}: {:?}ms", duration);
                }
                new_price = pyth_price_feed.recv() => {
                    match new_price {
                        Some(update) => {
                            feed_health.touch_pyth();
                            pyth_oracle_prices.insert(update.market_id, update);
                        }
                        None => {
                            log::error!(target: TARGET, "pyth price feed disconnected, shutting down");
                            break;  // exits the loop
                        }
                    }
                }
                _ = slot_watchdog.tick() => {
                    if last_slot_update.elapsed() > SLOT_FEED_STALE_LIMIT {
                        log::error!(
                            target: TARGET,
                            "no slot updates for {}s: gRPC slot feed is dead, exiting for supervisor restart",
                            last_slot_update.elapsed().as_secs()
                        );
                        std::process::exit(1);
                    }
                    if last_swift_msg.elapsed() > SWIFT_FEED_STALE_LIMIT {
                        log::warn!(
                            target: "swift",
                            "no swift orders for {}s, resubscribing in case the ws is half-open",
                            last_swift_msg.elapsed().as_secs()
                        );
                        // keep the same ws url override as the initial subscription
                        match velocity
                            .subscribe_swift_orders(&market_ids, Some(true), None, std::env::var("SWIFT_WS_URL").ok())
                            .await
                        {
                            Ok(stream) => {
                                log::info!(target: "swift", "feed resubscribed after stale window");
                                swift_order_stream = stream;
                                feed_health.set_swift_connected(true);
                            }
                            Err(e) => {
                                // keep the old stream; it may still be alive in a quiet
                                // market and the next tick past the limit retries anyway
                                log::error!(target: "swift", "stale resubscribe failed: {e:?}");
                            }
                        }
                        // reset either way so a failed attempt retries after a full
                        // stale window instead of every 15s tick
                        last_swift_msg = std::time::Instant::now();
                    }
                }
            }

            for signed_order in std::mem::take(&mut swift_orders) {
                // Until the stamped message slot arrives, the program accepts neither a
                // fill nor a bare placement of an auction order. Acting now spends a
                // transaction, and for a fill it spends the order's one place-and-fill
                // attempt. A resting limit is the exception. Its stamp is a deadline, so
                // it is placed at once.
                let order_slot = signed_order.slot();
                match swift_slot_wait_if_known(
                    order_slot,
                    slot_is_known.then_some(slot),
                    MAX_SWIFT_ORDER_DEFERRAL,
                    is_resting_swift_limit(&signed_order.order_params()),
                    slot_clock,
                ) {
                    SwiftSlotWait::Ready => {}
                    SwiftSlotWait::TooFarAhead => {
                        log::warn!(target: TARGET, "swift order stamped {} slots ahead of slot {slot}, dropping. uuid={}", order_slot.saturating_sub(slot), signed_order.order_uuid_str());
                        continue;
                    }
                    SwiftSlotWait::Wait => {
                        if deferred_swift_orders.len() >= MAX_DEFERRED_SWIFT_ORDERS {
                            log::warn!(target: TARGET, "deferred swift orders at capacity ({MAX_DEFERRED_SWIFT_ORDERS}), dropping. uuid={}", signed_order.order_uuid_str());
                        } else {
                            log::info!(target: TARGET, "swift order slot {order_slot} not reached (slot {slot}), deferring. uuid={}", signed_order.order_uuid_str());
                            deferred_swift_orders.push(signed_order);
                        }
                        continue;
                    }
                }
                let order_params = signed_order.order_params();
                // A held order can age out while the slot feed stalls, and a fresh
                // feed order can already be late on arrival. Check after the select so
                // it uses the newest slot handled this iteration, before either the fill
                // or placement path can spend a transaction.
                let now_ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64;
                if swift_order_expired(
                    order_slot,
                    order_params.auction_duration.unwrap_or(0),
                    order_params.max_ts.unwrap_or(0),
                    slot,
                    now_ts,
                    slot_clock,
                ) {
                    log::info!(target: TARGET, "swift order expired before processing, dropping. uuid={}", signed_order.order_uuid_str());
                    metrics.swift_place_skipped.inc();
                    continue;
                }
                let market_index = order_params.market_index;
                log::info!(target: TARGET, "new swift order. uuid={}, market={}", signed_order.order_uuid_str(), market_index);
                log::debug!(target: TARGET, "details: {signed_order:?}");
                // A transient cache miss must not stop the fill loop. Drop this order
                // rather than panic, because the swift feed keeps flowing.
                let Ok(_perp_market) = velocity.try_get_perp_market_account(market_index) else {
                    log::warn!(target: TARGET, "no perp market {market_index} for swift order, skipping. uuid={}", signed_order.order_uuid_str());
                    continue;
                };

                // A fill transaction lands about 1 slot ahead, per the tx_event
                // latency_slots telemetry. Fillability is evaluated at that landing slot.
                // A higher estimate assumes a higher auction price than the program
                // computes, and sends fill legs that do nothing on chain.
                let landing_slot = slot + 1;
                let Ok(oracle_price_data) =
                    velocity.try_get_mmoracle_for_perp_market(market_index, landing_slot)
                else {
                    log::warn!(target: TARGET, "no oracle price for market {market_index}, skipping swift order. uuid={}", signed_order.order_uuid_str());
                    continue;
                };

                // A swift order is a taker order. The placement routes it, so a
                // trigger type has no meaning on this feed and the program would
                // refuse it. Drop it here rather than spending a transaction on it.
                let order_params = signed_order.order_params();
                if !matches!(
                    order_params.order_type,
                    OrderType::Market | OrderType::Oracle | OrderType::Limit
                ) {
                    log::warn!(
                        target: TARGET,
                        "unsupported swift order type {:?}, dropping. uuid={}",
                        order_params.order_type,
                        signed_order.order_uuid_str()
                    );

                    continue;
                }

                // Placement routes the order: it fills against the book and the routed
                // quoters, and any remainder rests as a taker-origin remainder for the
                // activation-slot auction. There is no separate fill decision here.
                log::info!(
                    target: TARGET,
                    "placing swift order. market={market_index} oracle={} delay={} uuid={}",
                    oracle_price_data.price,
                    oracle_price_data.delay,
                    signed_order.order_uuid_str()
                );

                let pf = priority_fee_subscriber.priority_fee_nth(0.6);
                try_swift_place(
                    velocity,
                    pf,
                    config.swift_cu_limit,
                    filler_subaccount,
                    signed_order,
                    slot,
                    tx_worker_ref.clone(),
                    attest,
                    Arc::clone(&metrics),
                )
                .await;
                metrics.swift_placed.inc();
            }
        }
        velocity.grpc_unsubscribe();
        log::info!(target: TARGET, "filler shutting down...");
    }
}

fn on_transaction_update_fn(
    tx_worker_ref: TxSender,
) -> impl Fn(&TransactionUpdate) + Send + Sync + 'static {
    move |tx: &TransactionUpdate| {
        if let Some(sig) = tx.transaction.signatures.first() {
            tx_worker_ref.confirm_tx((sig.as_slice().try_into()).expect("valid signature"));
        } else {
            log::warn!(target: TARGET, "received tx without sig: {tx:?}");
        }
    }
}

/// Max consecutive slots a market's oracle may be missing before the process exits.
///
/// A panic here is NOT protective: this closure runs on the gRPC dispatch thread, and a
/// thread panic doesn't stop the process — the bot would keep running with a frozen book
/// (zombie). A transient miss is skipped and retried next slot; a persistent one exits the
/// process so the supervisor restarts it with fresh subscriptions.
// About 2 minutes of slot ticks at 400ms. The wall-clock window shrinks as slot
// time drops. That is acceptable, because this counter only restarts a bot whose
// feed is dead, and firing sooner is safe.
const MAX_CONSECUTIVE_ORACLE_MISSES: u32 = 300;

fn on_slot_update_fn(
    velocity: VelocityClient,
    market_ids: Vec<MarketId>,
    dlob_notifier: DLOBNotifier,
    slot_tx: tokio::sync::mpsc::Sender<u64>,
) -> impl Fn(u64) + Send + Sync + 'static {
    // single gRPC dispatch thread: the mutex is uncontended
    let consecutive_misses_ref = std::sync::Mutex::new(BTreeMap::<u16, u32>::new());
    move |new_slot| {
        for market in market_ids.iter() {
            // a transiently missing oracle must not kill the gRPC dispatch thread;
            // skip the market this slot and let the next tick retry
            let Ok(oracle_price_data) =
                velocity.try_get_mmoracle_for_perp_market(market.index(), new_slot)
            else {
                let mut misses = consecutive_misses_ref.lock().unwrap();
                let count = misses.entry(market.index()).or_insert(0);
                *count += 1;
                log::warn!(target: TARGET, "no oracle price for market {} ({count} consecutive), skipping slot update", market.index());
                if *count >= MAX_CONSECUTIVE_ORACLE_MISSES {
                    log::error!(target: TARGET, "oracle for market {} missing for {count} consecutive slots, exiting for restart", market.index());
                    std::process::exit(1);
                }
                continue;
            };
            consecutive_misses_ref
                .lock()
                .unwrap()
                .insert(market.index(), 0);
            dlob_notifier.slot_and_oracle_update(*market, new_slot, oracle_price_data.price as u64);
        }
        if let Err(err) = slot_tx.try_send(new_slot) {
            log::debug!(target: TARGET, "failed slot update: {err:?}");
        }
    }
}

fn on_account_update_fn(
    dlob_notifier: DLOBNotifier,
    velocity: VelocityClient,
) -> impl Fn(&AccountUpdate) + Send + Sync + 'static {
    move |update| {
        // Skip closed / empty-data updates rather than panic on `&data[8..]`.
        let Some(new_user) = velocity_rs::utils::try_deser_zero_copy::<User>(update.data) else {
            if update.lamports == 0 {
                // account closed/deleted: diff its last known state against an empty
                // account so its open orders are removed from the book
                if let Some(old_user) = velocity
                    .backend()
                    .account_map()
                    .account_data_and_slot::<User>(&update.pubkey)
                {
                    dlob_notifier.user_update(
                        update.pubkey,
                        Some(&old_user.data),
                        &User::default(),
                        update.slot,
                    );
                }
            }
            return;
        };
        // always feed the DLOB with the same lineage the account_map stores (this hook
        // runs before the account_map write): a slot-based skip here while the map still
        // accepts the update would desync `old_user` from the book and strand orders
        let existing = velocity
            .backend()
            .account_map()
            .account_data_and_slot::<User>(&update.pubkey);
        if let Some(ref existing) = existing {
            if existing.slot > update.slot {
                log::debug!(
                    target: TARGET,
                    "out of order user update: {} > {}",
                    existing.slot,
                    update.slot
                );
            }
        }
        dlob_notifier.user_update(
            update.pubkey,
            existing.as_ref().map(|x| &x.data),
            &new_user,
            update.slot,
        );
    }
}

/// A transaction locks 64 accounts and a maker costs two, so this caps the
/// makers carried, not the book depth. The book stops at the first maker
/// not brought, so carrying the best-priced makers first is what matters.
/// Depth behind them stays resting instead of being lost.
const CLOB_MAKERS_PER_FILL: usize = 6;

/// The `User` accounts of the makers a fill would sweep off the book.
///
/// The DLOB cross cannot name these. Its orders live in `User.orders`, so
/// finding them means reading loaded accounts. A book order lives on the book,
/// and the only record of its owner is an authority and a sub-account on the
/// order. The book answers for itself through a simulated `quote_l3_v0` leg.
/// This keeper therefore never decodes a book, and the book can change its data
/// structures without breaking it.
async fn clob_makers(
    velocity: &'static VelocityClient,
    config: &QuoterConfigV0,
    direction: PositionDirection,
    size: u64,
    taker: ClobUserRefV0,
    metrics: &Metrics,
) -> Vec<User> {
    let source = relay_chain_source::RpcSource::new(velocity.rpc().url());
    // Ask without a budget first, so the makers the budget leaves behind can be
    // counted. A book stops at the first maker the transaction did not bring, so
    // every maker dropped here is depth the taker did not get.
    let reachable = match velocity_router_sim::l3::resting_makers(
        &source,
        config,
        match direction {
            PositionDirection::Long => velocity_router_sim::Direction::Long,
            PositionDirection::Short => velocity_router_sim::Direction::Short,
        },
        size,
        usize::MAX,
    )
    .await
    {
        Ok(makers) => makers,
        Err(err) => {
            log::warn!(target: TARGET, "clob makers: {err:#}");
            return Vec::new();
        }
    };

    let reachable: Vec<ClobUserRefV0> = reachable
        .into_iter()
        .filter(|maker| *maker != taker)
        .collect();
    let carried = reachable.len().min(CLOB_MAKERS_PER_FILL);
    metrics.clob_makers_carried.inc_by(carried as u64);
    if reachable.len() > carried {
        let dropped = reachable.len() - carried;
        metrics.clob_makers_dropped.inc_by(dropped as u64);
        log::info!(
            target: TARGET,
            "clob makers: {} in reach, carrying {carried}, {dropped} left resting",
            reachable.len()
        );
    }

    let makers = &reachable[..carried];
    // A maker missing from the cache is dropped and is not an error, the same
    // way a DLOB maker is. The book stops there and the fill takes what it can
    // reach.
    makers
        .iter()
        .filter_map(|maker| {
            let key = Wallet::derive_user_account(
                &Pubkey::new_from_array(maker.authority.to_bytes()),
                maker.sub_account_id,
            );

            velocity.try_get_account::<User>(&key).ok()
        })
        .collect()
}

/// Charge a failed simulation to the quoters the program named in its logs.
///
/// Only a named failure is charged. A simulation that carries several quoters
/// fails for reasons that belong to no one, and the CPI-bracket inference that
/// resolves the rest needs the route's entry order, which the fill path does
/// not assemble. Charging a maker for an unproven failure is worse than missing
/// one, so anything the logs do not name is counted against this bot.
fn charge_quoter_failures(
    metrics: &Metrics,
    logs: &[String],
    err: &str,
    market_index: Option<u16>,
) {
    let verdict = attribute(logs, Some(err), RouteContext { entries: &[] });
    if verdict.is_empty() && verdict.unattributed.is_none() {
        return;
    }

    let market = market_index.unwrap_or_default();
    for charge in &verdict.charges {
        log::warn!(
            target: TARGET,
            "quoter {} broke a fill simulation: {:?}",
            charge.quoter,
            charge.reason
        );

        metrics.quoter_health.record(Report::new(
            charge.quoter,
            market,
            Observation::ExecuteFail {
                reason: charge.reason,
            },
        ));
    }

    if let Some(reason) = verdict.unattributed {
        metrics.quoter_health.record_unattributed(reason);
    }
}

/// The quoter section that a router fill carries. It holds the market's quoter
/// slab, then the union of the consulted slots' CPI accounts. Each slot
/// contributes its registered accounts, its response account and its program. A
/// slab slot is consulted when the fill carries its response account, so this
/// list is also the selection.
///
/// Two quoters go in, for two different reasons.
///
/// - The market's canonical CLOB is mandatory. `fill_perp_order` rejects any
///   fill on a market with a book attached that does not consult its slab slot,
///   with "router fill must include the market's CLOB quoter". The public book
///   is a baseline that a route cannot exclude. A suspended or killed slot
///   satisfies the check without being consulted.
/// - The quoters that the taker's signed route names are enforced. See
///   [`SignedOrderInfo::route`]. The program refuses a fill that omits a named
///   quoter whose slot can still quote, so the fill must consult each one.
///
/// A named quoter whose slot is gone, or that cannot quote, is dropped rather
/// than failing the fill. The program drops it the same way. A route signed
/// before an admin removed a quoter must still fill.
async fn route_quoter_metas(
    velocity: &VelocityClient,
    market_index: u16,
    clob_market: Pubkey,
    route: Option<&[Pubkey]>,
    swift_order: &SignedOrderInfo,
) -> Option<Vec<AccountMeta>> {
    let route = route.unwrap_or_default();
    if clob_market == Pubkey::default() && route.is_empty() {
        return Some(Vec::new());
    }

    // An unreadable slab abandons the attempt. The signed route is enforced on
    // chain. Every live entry the taker named must be consulted, and the
    // market's canonical CLOB is a mandatory baseline. A fill missing the slab
    // is therefore rejected, and building it only spends a transaction to
    // discover that.
    let slots = match velocity.get_quoter_slab_slots(market_index).await {
        Ok(slots) => slots,
        Err(err) => {
            log::error!(
                target: TARGET,
                "quoter slab for market {market_index} unreadable ({err:?}); abandoning the fill. uuid={}",
                swift_order.order_uuid_str()
            );

            return None;
        }
    };

    // A slot is consulted when the taker's route names its entry, or when it is
    // the market's book. `PerpMarket.clob_market` stores the book account itself,
    // so the book slot matches by its response account. A named entry with no
    // live slot is dropped, the same way the program drops it.
    let consulted: Vec<&QuoterSlotV0> = slots
        .iter()
        .filter(|slot| {
            (clob_market != Pubkey::default() && slot.config.response_account == clob_market)
                || route.contains(&slot.entry)
        })
        .filter(|slot| slot.quotes())
        .collect();

    // Writability is the OR across slots. The BTreeMap dedups a key two slots
    // register and keeps the build deterministic. Each leg resolves its
    // accounts by index into this full list, so the whole list must ride,
    // not one leg's subset.
    let mut cpi_union: BTreeMap<Pubkey, bool> = BTreeMap::new();
    for slot in &consulted {
        for meta in slot.config.registered_accounts() {
            *cpi_union.entry(meta.pubkey).or_default() |= meta.is_writable;
        }

        *cpi_union.entry(slot.config.response_account).or_default() |= true;
        cpi_union.entry(slot.config.program_id).or_default();
    }

    Some(
        std::iter::once(AccountMeta::new_readonly(
            derive_quoter_slab(market_index),
            false,
        ))
        .chain(cpi_union.iter().map(|(key, writable)| {
            if *writable {
                AccountMeta::new(*key, false)
            } else {
                AccountMeta::new_readonly(*key, false)
            }
        }))
        .collect(),
    )
}

/// Place a swift order on-chain when this bot found no resting cross for it.
///
/// The placement routes the order as it places it, so the path can fill. What
/// it cannot fill rests on the market's book as a taker-origin remainder, where
/// the activation-slot auction reaches it. The path emits a `swift_place` wide
/// event at transaction confirmation, so the gas spent on placements can be
/// measured against the fills they yield.
///
/// It carries the attestation for the same reason a fill does. This bot searched
/// only the resting liquidity it can see, so the quoters that gate on attested
/// flow are the ones it did not search. Placing unattested would route past them
/// and rest an order they would have filled.
async fn try_swift_place(
    velocity: &'static VelocityClient,
    priority_fee: u64,
    cu_limit: u32,
    filler_subaccount: Pubkey,
    swift_order: SignedOrderInfo,
    slot: u64,
    tx_worker_ref: TxSender,
    attest: Option<&'static crate::attest::AttestClient>,
    metrics: Arc<Metrics>,
) {
    let market_index = swift_order.order_params().market_index;
    let taker_subaccount = swift_order.taker_subaccount();

    // the bot's own account missing from cache is structural (lost subscription /
    // misconfig) and would silently no-op every fill: panic so the service restarts
    let filler_account_data = velocity
        .try_get_account::<User>(&filler_subaccount)
        .expect("filler subaccount in cache; restart");
    let taker_account_data = match velocity.get_account_value::<User>(&taker_subaccount).await {
        Ok(a) => a,
        Err(err) => {
            log::warn!(target: TARGET, "swift place: failed to load taker account {taker_subaccount}: {err:?}");
            return;
        }
    };

    let book_config = velocity
        .get_quoter_slab_slots(market_index)
        .await
        .ok()
        .and_then(|slots| clob_slot_config(&slots));
    let Some(book_config) = book_config else {
        log::warn!(target: TARGET, "no approved clob on the quoter slab for market {market_index}; cannot place signed-message order");
        return;
    };
    let clob_place = ClobFillAccounts {
        market_index,
        quoter_slab: derive_quoter_slab(market_index),
        clob_market: book_config.response_account,
        clob_program: book_config.program_id,
        crank_conditions: None,
    };

    // The book's own makers. The placement routes as it places, and a fill
    // settles only for the users it carries. A book maker left out is depth
    // the order passes over, and the book stops at the first maker it was not
    // handed, so leaving out the best one gives up the rest of the book too.
    let taker_params = swift_order.order_params();
    let maker_accounts = clob_makers(
        velocity,
        &book_config,
        taker_params.direction,
        taker_params.base_asset_amount,
        ClobUserRefV0 {
            authority: anchor_lang::prelude::Pubkey::new_from_array(
                swift_order.taker_authority.to_bytes(),
            ),

            sub_account_id: taker_account_data.sub_account_id,
        },
        &metrics,
    )
    .await;

    // The quoters this placement must consult. The program refuses a route that
    // omits a live quoter the taker's signed route named, and the market's own
    // book is a baseline no route can exclude.
    let Some(quoter_metas) = route_quoter_metas(
        velocity,
        market_index,
        book_config.response_account,
        swift_order.route(),
        &swift_order,
    )
    .await
    else {
        return;
    };

    let flow_attestation: Option<FlowAttestationV0> = match attest {
        Some(client) => match client.attest(swift_order.order_uuid_str()).await {
            Ok(attestation) => Some(attestation),
            Err(reason) => {
                log::warn!(
                    target: TARGET,
                    "attestation fell through ({reason}); placing unattested. uuid={}",
                    swift_order.order_uuid_str()
                );

                None
            }
        },
        None => None,
    };

    let mut tx_builder = with_spot_interest_cranks(
        TransactionBuilder::new(
            velocity.program_data(),
            filler_subaccount,
            std::borrow::Cow::Borrowed(&filler_account_data),
            false,
        )
        .with_priority_fee(priority_fee, Some(cu_limit)),
        velocity,
        &taker_account_data,
        &maker_accounts,
    )
    .place_swift_order(
        &swift_order,
        &taker_account_data,
        &maker_accounts,
        clob_place,
        flow_attestation,
    );

    // The quoter section rides the placement's remaining accounts. The program
    // reads everything past the map, user and escrow sections as registry
    // entries plus their CPI accounts.
    if !quoter_metas.is_empty() {
        let last = tx_builder.ixs().len() - 1;
        let mut place_ix = tx_builder.ixs()[last].clone();
        place_ix.accounts.extend(quoter_metas.iter().cloned());
        tx_builder = tx_builder.set_ix(last, place_ix);
    }

    tx_worker_ref
        .send_tx(
            tx_builder.build(),
            TxIntent::SwiftPlace {
                uuid: swift_order.order_uuid(),
                market_index,
                taker_user: taker_subaccount,
                slot,
            },
            cu_limit as u64,
        )
        .await;
}

/// Add the interest cranks a fill needs, ahead of the fill instruction.
///
/// The program refuses a fill when the taker or a maker holds a stale-interest
/// borrow, with error `SpotMarketInterestStaleForMargin`. The margin check
/// then values that borrow through the stale index and understates the debt.
/// `fill_perp_order` receives those markets read-only, so the permissionless
/// crank rides in the same transaction. Call this before `fill_perp_order`,
/// which also keeps the fill as the last instruction for the account-count
/// check. A market this misses only costs a reverted fill.
fn with_spot_interest_cranks<'a>(
    mut tx_builder: TransactionBuilder<'a>,
    velocity: &VelocityClient,
    taker: &User,
    makers: &[User],
) -> TransactionBuilder<'a> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default();

    let mut users = Vec::with_capacity(makers.len() + 1);
    users.push(taker);
    users.extend(makers.iter());

    for market_index in velocity.stale_spot_interest_markets(&users, now) {
        tx_builder = tx_builder.update_spot_market_cumulative_interest(market_index);
    }

    tx_builder
}

/// Setup gRPC subscriptions
///
/// Syncs User orders and UserStat accounts
pub async fn setup_grpc(
    velocity: VelocityClient,
    dlob: &'static DLOB,
    tx_worker_ref: TxSender,
    market_ids: Vec<MarketId>,
    filler_subaccount: Pubkey,
) -> tokio::sync::mpsc::Receiver<u64> {
    let dlob_notifier = dlob.spawn_notifier();

    let _ = tokio::try_join!(
        sync_stats_accounts(&velocity),
        sync_user_accounts(&velocity, Some(&dlob_notifier)),
    );

    let (slot_tx, slot_rx) = tokio::sync::mpsc::channel(64);

    subscribe_grpc(
        velocity,
        dlob_notifier,
        slot_tx,
        tx_worker_ref,
        market_ids,
        filler_subaccount,
    )
    .await;

    slot_rx
}

pub async fn sync_stats_accounts(
    velocity: &VelocityClient,
) -> Result<(), solana_rpc_client_api::client_error::Error> {
    let stats_sync_result = get_program_accounts_decoded(
        velocity,
        RpcProgramAccountsConfig {
            filters: Some(vec![velocity_rs::memcmp::get_user_stats_filter()]),
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64Zstd),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await;

    match stats_sync_result {
        Ok(accounts) => {
            for (pubkey, account) in accounts {
                velocity.backend().account_map().on_account_fn()(&AccountUpdate {
                    pubkey,
                    data: &account.data,
                    lamports: account.lamports,
                    owner: PROGRAM_ID,
                    rent_epoch: u64::MAX,
                    executable: false,
                    slot: 0,
                    write_version: 0,
                });
            }
            log::info!(target: "dlob", "syncd stats accounts");
            Ok(())
        }
        Err(err) => {
            log::error!(target: "dlob", "dlob sync error: {err:?}");
            Err(err)
        }
    }
}

/// Load every non-idle `User` into the account map.
///
/// `dlob_notifier` is the filler's book, which is seeded from the same pass.
/// The liquidator keeps no book and passes `None`, so it pays for the account
/// map alone.
pub async fn sync_user_accounts(
    velocity: &VelocityClient,
    dlob_notifier: Option<&DLOBNotifier>,
) -> Result<(), solana_rpc_client_api::client_error::Error> {
    let sync_result = get_program_accounts_decoded(
        velocity,
        RpcProgramAccountsConfig {
            filters: Some(vec![
                velocity_rs::memcmp::get_non_idle_user_filter(),
                velocity_rs::memcmp::get_user_filter(),
            ]),
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64Zstd),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await;

    match sync_result {
        Ok(accounts) => {
            for (pubkey, account) in accounts {
                let user = velocity_rs::utils::deser_zero_copy::<User>(&account.data);
                if let Some(dlob_notifier) = dlob_notifier {
                    dlob_notifier.user_update(pubkey, None, &user, 0);
                }

                velocity.backend().account_map().on_account_fn()(&AccountUpdate {
                    pubkey,
                    data: &account.data,
                    lamports: account.lamports,
                    owner: PROGRAM_ID,
                    rent_epoch: u64::MAX,
                    executable: false,
                    slot: 0,
                    write_version: 0,
                });
            }
            log::info!(target: "dlob", "synced initial orders");
            Ok(())
        }
        Err(err) => {
            log::error!(target: "dlob", "dlob sync error: {err:?}");
            Err(err)
        }
    }
}

/// `RpcClient::get_program_accounts_with_config` was removed in solana-rpc-client 4.2.
/// Decode `UiAccount`s from the replacement so callers still see binary `Account` data.
///
/// Anza's own helper panics on an account it cannot decode. This one runs on the
/// filler and liquidator startup path, where an RPC that ignores the requested
/// `Base64Zstd` encoding should degrade like any other sync failure, so skip those
/// accounts and report how many were lost.
async fn get_program_accounts_decoded(
    velocity: &VelocityClient,
    config: RpcProgramAccountsConfig,
) -> Result<Vec<(Pubkey, solana_account::Account)>, solana_rpc_client_api::client_error::Error> {
    let ui_accounts = velocity
        .rpc()
        .get_program_ui_accounts_with_config(&PROGRAM_ID, config)
        .await?;
    let returned = ui_accounts.len();
    let accounts: Vec<(Pubkey, solana_account::Account)> = ui_accounts
        .into_iter()
        .filter_map(|(pubkey, ui)| ui.to_account().map(|account| (pubkey, account)))
        .collect();
    if accounts.len() < returned {
        log::warn!(
            target: "dlob",
            "skipped {} of {returned} program accounts: not returned in a binary encoding",
            returned - accounts.len()
        );
    }

    Ok(accounts)
}

async fn subscribe_grpc(
    velocity: VelocityClient,
    dlob_notifier: DLOBNotifier,
    slot_tx: tokio::sync::mpsc::Sender<u64>,
    transaction_tx: TxSender,
    market_ids: Vec<MarketId>,
    filler_subaccount: Pubkey,
) {
    let _res = velocity
        .grpc_subscribe(
            std::env::var("GRPC_ENDPOINT")
                .unwrap_or_else(|_| "https://api.rpcpool.com".to_string())
                .into(),
            std::env::var("GRPC_X_TOKEN").expect("GRPC_X_TOKEN set"),
            GrpcSubscribeOpts::default()
                .commitment(solana_commitment_config::CommitmentLevel::Processed)
                .connection_opts(GrpcConnectionOpts::default().enable_compression())
                .usermap_on()
                .statsmap_on()
                // must watch the subaccount fills are actually sent from: with a non-zero
                // configured sub_account_id the default subaccount never appears in the
                // bot's txs and confirmations would never fire
                .transaction_include_accounts(vec![filler_subaccount])
                .on_transaction(on_transaction_update_fn(transaction_tx.clone()))
                .on_slot(on_slot_update_fn(
                    velocity.clone(),
                    market_ids,
                    dlob_notifier.clone(),
                    slot_tx.clone(),
                ))
                .on_account(
                    AccountFilter::partial().with_discriminator(User::DISCRIMINATOR),
                    on_account_update_fn(dlob_notifier.clone(), velocity.clone()),
                ),
            true,
        )
        .await;
}

pub enum TxWork {
    Send {
        tx: VersionedTransaction,
        /// The verdict `queue_tx` already received, so the worker does not ask
        /// twice. `queue_tx` simulates to size the compute limit, and the same
        /// reply says whether the transaction is worth sending.
        presimulated: Option<SdkResult<RpcSimulateTransactionResult>>,
        simulation_tx: Option<VersionedMessage>,
        require_fill_event: bool,
        ts: u64,
        intent: TxIntent,
        cu_limit: u64,
    },
    Confirm {
        tx: Signature,
        ts: u64,
    },
}

pub struct TxWorker {
    velocity: &'static VelocityClient,
    pending_txs: Arc<RwLock<PendingTxs<1024>>>,
    metrics: Arc<Metrics>,
    dry_run: bool,
    txs_in_flight: Option<Arc<DashMap<Pubkey, HashSet<Signature>>>>,
    tx_sig_to_collateral: Option<Arc<DashMap<Signature, (u128, u64)>>>,
    free_collateral_per_subaccount: Option<Arc<DashMap<Pubkey, u128>>>,
    perp_fill_fallbacks: Option<Arc<DashMap<(Pubkey, u16), PerpFillFallback>>>,
}

impl TxWorker {
    pub fn new(
        velocity: VelocityClient,
        metrics: Arc<Metrics>,
        dry_run: bool,
        txs_in_flight: Option<Arc<DashMap<Pubkey, HashSet<Signature>>>>,
        tx_sig_to_collateral: Option<Arc<DashMap<Signature, (u128, u64)>>>,
        free_collateral_per_subaccount: Option<Arc<DashMap<Pubkey, u128>>>,
        perp_fill_fallbacks: Option<Arc<DashMap<(Pubkey, u16), PerpFillFallback>>>,
    ) -> Self {
        Self {
            velocity: Box::leak(Box::new(velocity)),
            pending_txs: Arc::new(RwLock::new(PendingTxs::new())),
            metrics,
            dry_run,
            txs_in_flight,
            tx_sig_to_collateral,
            free_collateral_per_subaccount,
            perp_fill_fallbacks,
        }
    }

    pub fn run(self, rt: tokio::runtime::Handle) -> TxSender {
        let (tx, rx) = crossbeam::channel::bounded(1024);
        let velocity = self.velocity;
        std::thread::spawn(move || {
            let _ = env_logger::try_init();
            while let Ok(work) = rx.recv() {
                match work {
                    TxWork::Send {
                        tx,
                        presimulated,
                        simulation_tx,
                        require_fill_event,
                        ts: _,
                        intent,
                        cu_limit,
                    } => {
                        if self.dry_run {
                            log::debug!(target: TARGET, "skip tx dry run: {intent:?}");
                            continue;
                        }

                        self.send_tx(
                            &rt,
                            tx,
                            presimulated,
                            simulation_tx,
                            require_fill_event,
                            intent,
                            cu_limit,
                        );
                    }
                    TxWork::Confirm { tx, ts: _ } => {
                        self.confirm_tx(&rt, tx);
                    }
                }
            }
        });
        TxSender { tx, velocity }
    }

    #[allow(clippy::too_many_arguments)]
    fn send_tx(
        &self,
        rt: &Handle,
        signed_tx: VersionedTransaction,
        presimulated: Option<SdkResult<RpcSimulateTransactionResult>>,
        simulation_tx: Option<VersionedMessage>,
        require_fill_event: bool,
        intent: TxIntent,
        cu_limit: u64,
    ) {
        log::debug!(target: TARGET, "txworker send tx: {intent:?}");
        let velocity = self.velocity;
        let pending_txs = Arc::clone(&self.pending_txs);
        let metrics = self.metrics.clone();
        let perp_fill_fallbacks = self.perp_fill_fallbacks.clone();
        let intent_label = intent.label();

        metrics.tx_sent.with_label_values(&[intent_label]).inc();
        metrics
            .fill_expected
            .with_label_values(&[intent_label])
            .inc();
        if intent.expected_trigger() {
            metrics.trigger_expected.inc();
        }

        rt.spawn(async move {
            let simulation_tx = simulation_tx.unwrap_or_else(|| signed_tx.message.clone());
            // simulate first, against processed state: the tx was built from the
            // processed-commitment gRPC view, so a default-commitment (finalized)
            // preflight lags ~32 slots and rejects valid fills for the whole
            // finalization window (e.g. `OrderMustBeTriggeredFirst` on fills of a
            // just-triggered order)
            let simulation = match presimulated {
                Some(result) => result,
                None => {
                    velocity
                        .simulate_tx_with_commitment(
                            simulation_tx,
                            Some(CommitmentConfig::processed()),
                        )
                        .await
                }
            };

            match simulation {
                Ok(sim_result) => {
                    if let Some(err) = sim_result.err {
                        record_perp_fill_fallback(
                            &intent,
                            &TransactionError::from(err.clone()),
                            perp_fill_fallbacks.as_ref(),
                        );
                        if is_revert_fill_error(&err) {
                            log::debug!(
                                target: TARGET,
                                "fill produced no fills during simulation, intent: {intent_label}"
                            );
                            emit_tx_event(
                                &intent,
                                None,
                                "no_fills",
                                intent.crosses_and_slot().1,
                                None,
                                intent.expected_fill_count(),
                                0,
                                false,
                                cu_limit,
                                None,
                                None,
                                Some("RevertFill"),
                            );
                            return;
                        }
                        log::warn!(
                            target: TARGET,
                            "sim failed: {err:?}, intent: {intent_label}, liquidatee: {:?}, slot: {:?}",
                            intent.liquidatee(),
                            intent.slot()
                        );

                        // The program names the quoter it could not use, so a
                        // simulation that a maker broke can be told apart from
                        // one this bot broke. Nothing lands, so these logs are
                        // the only record of the failure.
                        if let Some(logs) = sim_result.logs.as_deref() {
                            charge_quoter_failures(
                                &metrics,
                                logs,
                                &format!("{err:?}"),
                                intent.market_index(),
                            );
                        }

                        // Log simulation logs for liquidation and uncross intents to help
                        // diagnose failures
                        if intent.is_liquidation()
                            || matches!(intent, TxIntent::LimitUncross { .. })
                        {
                            if let Some(logs) = sim_result.logs {
                                for log_line in &logs {
                                    if log_line.contains("Error") || log_line.contains("error") || log_line.contains("failed") || log_line.contains("Program log:") {
                                        log::warn!(target: TARGET, "  sim log: {}", log_line);
                                    }
                                }
                            }
                        }
                        metrics
                            .tx_failed
                            .with_label_values(&[intent_label, "sim_failed"])
                            .inc();
                        emit_tx_event(
                            &intent,
                            None,
                            "sim_failed",
                            intent.crosses_and_slot().1,
                            None,
                            intent.expected_fill_count(),
                            0,
                            false,
                            cu_limit,
                            None,
                            None,
                            Some(&format!("{err:?}")),
                        );
                        return;
                    }

                    if require_fill_event
                        && !simulation_has_expected_fill(sim_result.logs.as_deref(), &intent)
                    {
                        log::debug!(
                            target: TARGET,
                            "fill simulation emitted no matching fill event, intent: {intent_label}"
                        );
                        emit_tx_event(
                            &intent,
                            None,
                            "no_fills",
                            intent.crosses_and_slot().1,
                            None,
                            intent.expected_fill_count(),
                            0,
                            false,
                            cu_limit,
                            None,
                            None,
                            Some("NoFillEvent"),
                        );
                        return;
                    }
                }
                Err(err) => {
                    log::warn!(
                        target: TARGET,
                        "sim rpc error: {err}, intent: {intent_label}, liquidatee: {:?}",
                        intent.liquidatee()
                    );
                    metrics
                        .tx_failed
                        .with_label_values(&[intent_label, "sim_rpc_error"])
                        .inc();
                    emit_tx_event(
                        &intent,
                        None,
                        "sim_rpc_error",
                        intent.crosses_and_slot().1,
                        None,
                        intent.expected_fill_count(),
                        0,
                        false,
                        cu_limit,
                        None,
                        None,
                        Some(&format!("{err}")),
                    );
                    return;
                }
            }

            let config = RpcSendTransactionConfig {
                skip_preflight: true,
                max_retries: Some(0),
                ..Default::default()
            };

            match velocity
                .rpc()
                .send_transaction_with_config(&signed_tx, config)
                .await
            {
                Ok(sig) => {
                    log::info!(
                        target: TARGET,
                        r#"{{"intent": "{}", "txn": "{}", "observed_slot": {}}}"#,
                        intent_label,
                        sig,
                        intent.slot().unwrap_or(0)
                    );
                    let mut pending = pending_txs.write().await;
                    pending.insert(PendingTxMeta::new(sig, intent, cu_limit));
                }
                Err(err) => {
                    log::info!(target: TARGET, "fill failed 🐢: {err}");
                    metrics
                        .tx_failed
                        .with_label_values(&[intent_label, "send_error"])
                        .inc();
                    emit_tx_event(
                        &intent,
                        None,
                        "send_error",
                        intent.crosses_and_slot().1,
                        None,
                        intent.expected_fill_count(),
                        0,
                        false,
                        cu_limit,
                        None,
                        None,
                        Some(&format!("{err}")),
                    );
                }
            }
        });
    }

    fn confirm_tx(&self, rt: &Handle, tx: Signature) {
        // TODO: if CU limit is too low send it again with higher amount
        log::debug!(target: TARGET, "txworker confirm tx: {tx:?}");
        let velocity = self.velocity;
        let pending_txs = Arc::clone(&self.pending_txs);
        let metrics = self.metrics.clone();

        let txs_in_flight = self.txs_in_flight.clone();
        let tx_sig_to_collateral = self.tx_sig_to_collateral.clone();
        let free_collateral = self.free_collateral_per_subaccount.clone();
        let perp_fill_fallbacks = self.perp_fill_fallbacks.clone();

        rt.spawn(async move {
            let pending_tx_meta = {
                let mut pending = pending_txs.write().await;
                pending.confirm(&tx)
            };
            if pending_tx_meta.is_none() {
                return;
            }
            let PendingTxMeta {
                signature,
                intent,
                cu_limit: sent_cu_limit,
                ts: _,
            } = pending_tx_meta.unwrap();

            let intent_label = intent.label();
            let expected_fill_count = intent.expected_fill_count();
            let (_, sent_slot) = intent.crosses_and_slot();
            let _ = tokio::time::sleep(Duration::from_secs(1)).await;
            match velocity
                .rpc()
                .get_transaction_with_config(
                    &tx,
                    RpcTransactionConfig {
                        encoding: Some(UiTransactionEncoding::Base64),
                        commitment: Some(CommitmentConfig::confirmed()),
                        max_supported_transaction_version: Some(1),
                    },
                )
                .await
            {
                Ok(tx_log) => {
                    if let Some(meta) = tx_log.transaction.meta {
                        match meta.err.map(TransactionError::from) {
                            None => {
                                // tx confirmed ok
                                let sig = tx.to_string();
                                let logs = meta.log_messages.unwrap();
                                let tx_confirmed_slot = tx_log.slot;
                                let mut actual_fills = 0;
                                let mut triggered = false;
                                for event in parse_velocity_logs(
                                    logs.iter().map(String::as_str),
                                    &sig,
                                ) {
                                    if let VelocityEvent::OrderFill { .. } = event {
                                        actual_fills += 1;
                                    } else if let VelocityEvent::OrderTrigger { .. } = event {
                                        triggered = true;
                                        metrics.trigger_actual.inc();
                                    }
                                }
                                let confirmation_slots = tx_confirmed_slot - sent_slot;
                                log::debug!(target: TARGET, "txworker: {tx:?} confirmed after {confirmation_slots} slots");
                                metrics
                                    .fill_actual
                                    .with_label_values(&[intent_label])
                                    .inc();
                                metrics
                                    .confirmation_slots
                                    .with_label_values(&[intent_label])
                                    .observe(confirmation_slots as f64);
                                let cu_consumed: Option<u64> =
                                    meta.compute_units_consumed.clone().into();
                                // saturating: the account-count heuristic can raise the tx's
                                // actual CU limit above `sent_cu_limit` recorded at send time
                                let cus_spent = sent_cu_limit.saturating_sub(cu_consumed.unwrap_or(0));
                                metrics
                                    .cu_spent
                                    .with_label_values(&[intent_label])
                                    .observe(cus_spent as f64);

                                // For placement/trigger intents, success is not measured by
                                // fills, so don't mislabel them "no_fills". Detect the program's
                                // silent no-ops (uuid dedup / past placement window for a swift
                                // place; already-triggered for a trigger) so wasted gas is
                                // distinguishable from a real placement/trigger in the events.
                                let status = if expected_fill_count == 0 {
                                    match &intent {
                                        TxIntent::SwiftPlace { .. } => {
                                            if logs.iter().any(|l| l.contains("already exists")) {
                                                "place_noop_dup"
                                            } else if logs.iter().any(|l| l.contains("max_slot")) {
                                                "place_noop_expired"
                                            } else {
                                                "placed"
                                            }
                                        }
                                        TxIntent::Trigger { .. } => {
                                            if triggered {
                                                "triggered"
                                            } else {
                                                "trigger_noop"
                                            }
                                        }
                                        _ => "ok",
                                    }
                                } else if actual_fills == 0 {
                                    "no_fills"
                                } else if actual_fills < expected_fill_count as u64 {
                                    "partial"
                                } else {
                                    "ok"
                                };
                                metrics
                                    .tx_confirmed
                                    .with_label_values(&[intent_label, status])
                                    .inc();

                                emit_tx_event(
                                    &intent,
                                    Some(&sig),
                                    status,
                                    sent_slot,
                                    Some(tx_confirmed_slot),
                                    expected_fill_count,
                                    actual_fills,
                                    triggered,
                                    sent_cu_limit,
                                    cu_consumed,
                                    Some(meta.fee),
                                    None,
                                );

                                match intent {
                                    TxIntent::LiquidateWithFill { .. } => {
                                        metrics.liquidation_success.with_label_values(&["perp"]).inc();
                                    }
                                    TxIntent::LiquidateSpot { .. } => {
                                        metrics.liquidation_success.with_label_values(&["spot"]).inc();
                                    }
                                    _ => {}
                                }
                            }
                            Some(
                                TransactionError::InsufficientFundsForFee
                                | TransactionError::InsufficientFundsForRent { .. },
                            ) => {
                                log::error!(target: TARGET, "bot needs more SOL!");
                                metrics
                                    .tx_failed
                                    .with_label_values(&[
                                        intent_label,
                                        "insufficient_funds",
                                    ])
                                    .inc();
                                emit_tx_event(
                                    &intent,
                                    Some(&tx.to_string()),
                                    "insufficient_funds",
                                    sent_slot,
                                    Some(tx_log.slot),
                                    expected_fill_count,
                                    0,
                                    false,
                                    sent_cu_limit,
                                    meta.compute_units_consumed.clone().into(),
                                    Some(meta.fee),
                                    None,
                                );
                            }
                            Some(err) => {
                                record_perp_fill_fallback(
                                    &intent,
                                    &err,
                                    perp_fill_fallbacks.as_ref(),
                                );
                                log::warn!(
                                    target: TARGET,
                                    "tx failed: {err:?}, intent: {intent_label}, liquidatee: {:?}, sig: {signature}",
                                    intent.liquidatee()
                                );
                                let logs: Option<Vec<String>> = meta.log_messages.clone().into();
                                // Log program logs from failed liquidation txs
                                if intent.is_liquidation() {
                                    if let Some(logs) = logs.as_ref() {
                                        for log_line in logs {
                                            if log_line.contains("Error") || log_line.contains("error") || log_line.contains("failed") || log_line.contains("Program log:") {
                                                log::warn!(target: TARGET, "  tx log: {}", log_line);
                                            }
                                        }
                                    }
                                }

                                // The transaction failed with an error. Compute exhaustion
                                // lands here too. The VM reports it as
                                // ProgramFailedToComplete, which it shares with other
                                // faults, so the log line is what identifies it.
                                let reason = if logs
                                    .as_ref()
                                    .is_some_and(|logs| logs.iter().any(|l| l.contains("exceeded CUs meter")))
                                {
                                    "insufficient_cus".to_string()
                                } else {
                                    format!("{err:?}")
                                };
                                metrics
                                    .tx_failed
                                    .with_label_values(&[intent_label, &reason])
                                    .inc();
                                emit_tx_event(
                                    &intent,
                                    Some(&signature.to_string()),
                                    "failed",
                                    sent_slot,
                                    Some(tx_log.slot),
                                    expected_fill_count,
                                    0,
                                    false,
                                    sent_cu_limit,
                                    meta.compute_units_consumed.clone().into(),
                                    Some(meta.fee),
                                    Some(&format!("{err:?}")),
                                );
                                match intent {
                                    TxIntent::LiquidateWithFill { .. } => {
                                        metrics.liquidation_failed.with_label_values(&["perp"]).inc();
                                    }
                                    TxIntent::LiquidateSpot { .. } => {
                                        metrics.liquidation_failed.with_label_values(&["spot"]).inc();
                                    }
                                    _ => {}
                                }

                                if let (Some(tx_sig_map), Some(txs_map), Some(free_map)) =
                                    (&tx_sig_to_collateral, &txs_in_flight, &free_collateral)
                                {
                                    if let Some((_sig, (collateral, _ts))) = tx_sig_map.remove(&signature) {
                                        for mut entry in txs_map.iter_mut() {
                                            if entry.value_mut().remove(&signature) {
                                                if let Some(mut free) = free_map.get_mut(entry.key()) {
                                                    *free = free.saturating_add(collateral);
                                                }
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        log::warn!(target: TARGET, "tx metadata missing");
                        metrics
                            .tx_failed
                            .with_label_values(&[intent_label, "metadata_missing"])
                            .inc();
                        emit_tx_event(
                            &intent,
                            Some(&tx.to_string()),
                            "metadata_missing",
                            sent_slot,
                            Some(tx_log.slot),
                            expected_fill_count,
                            0,
                            false,
                            sent_cu_limit,
                            None,
                            None,
                            None,
                        );
                    }
                }
                Err(err) => {
                    log::info!(target: TARGET, "tx confirmation failed 🐢: {err}");
                    metrics
                        .tx_failed
                        .with_label_values(&[intent_label, "confirmation_failed"])
                        .inc();
                    emit_tx_event(
                        &intent,
                        Some(&tx.to_string()),
                        "confirmation_failed",
                        sent_slot,
                        None,
                        expected_fill_count,
                        0,
                        false,
                        sent_cu_limit,
                        None,
                        None,
                        Some(&format!("{err}")),
                    );
                }
            }
        });
    }
}

/// Emit a single wide structured event (one JSON line, log target `tx_event`) capturing the
/// full outcome of a transaction.
///
/// This is the canonical per-tx event: every order placement, fill and trigger the bot sends
/// produces exactly one terminal event here (at confirmation, or at sim/send failure), carrying
/// enough dimensions — intent, market, order id / swift uuid, expected vs actual fills, trigger
/// flag, CU limit/consumed, and the exact `fee_lamports` paid — to attribute gas spend. In
/// particular it makes it possible to measure how much gas the `swift_place` (place-on-chain)
/// path costs versus the fills those placements ultimately yield.
#[allow(clippy::too_many_arguments)]
fn emit_tx_event(
    intent: &TxIntent,
    sig: Option<&str>,
    status: &str,
    sent_slot: u64,
    confirmed_slot: Option<u64>,
    expected_fills: usize,
    actual_fills: u64,
    triggered: bool,
    cu_limit: u64,
    cu_consumed: Option<u64>,
    fee_lamports: Option<u64>,
    error: Option<&str>,
) {
    let latency_slots = confirmed_slot.map(|c| c.saturating_sub(sent_slot));
    let uuid = intent
        .swift_uuid()
        .map(|u| String::from_utf8_lossy(&u).into_owned());
    let event = serde_json::json!({
        "event": "tx",
        "intent": intent.label(),
        "market": intent.market_index(),
        "order_id": intent.order_id(),
        "user": intent.user().map(|u| u.to_string()),
        "uuid": uuid,
        "sig": sig,
        "status": status,
        "sent_slot": sent_slot,
        "confirmed_slot": confirmed_slot,
        "latency_slots": latency_slots,
        "expected_fills": expected_fills,
        "actual_fills": actual_fills,
        "triggered": triggered,
        "cu_limit": cu_limit,
        "cu_consumed": cu_consumed,
        "fee_lamports": fee_lamports,
        "error": error,
    });
    log::info!(target: "tx_event", "{event}");
}

fn is_revert_fill_error(error: &UiTransactionError) -> bool {
    matches!(
        TransactionError::from(error.clone()),
        TransactionError::InstructionError(_, InstructionError::Custom(6239))
    )
}

fn record_perp_fill_fallback(
    intent: &TxIntent,
    error: &TransactionError,
    fallbacks: Option<&Arc<DashMap<(Pubkey, u16), PerpFillFallback>>>,
) {
    let TransactionError::InstructionError(_, InstructionError::Custom(code)) = error else {
        return;
    };
    let expected = velocity_rs::program::error::ErrorCode::LiquidationOrderFailedToFill as u32
        + anchor_lang::error::ERROR_CODE_OFFSET;
    if *code != expected {
        return;
    }
    let TxIntent::LiquidateWithFill {
        market_index,
        liquidatee,
        ..
    } = intent
    else {
        return;
    };
    if let Some(fallbacks) = fallbacks {
        fallbacks.insert(
            (*liquidatee, *market_index),
            PerpFillFallback {
                recorded_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                attempts: 0,
            },
        );
        log::info!(
            target: TARGET,
            "recorded perp takeover fallback: liquidatee={liquidatee:?} market={market_index}"
        );
    }
}

fn is_expected_fill_event(event: &VelocityEvent, intent: &TxIntent) -> bool {
    matches!(
        event,
        VelocityEvent::OrderFill {
            taker,
            taker_order_id,
            base_asset_amount_filled,
            market_index,
            ..
        } if *base_asset_amount_filled > 0
            && *taker == intent.user()
            && Some(*taker_order_id) == intent.order_id()
            && Some(*market_index) == intent.market_index()
    )
}

fn simulation_has_expected_fill(logs: Option<&[String]>, intent: &TxIntent) -> bool {
    logs.map(|logs| {
        parse_velocity_logs(logs.iter().map(String::as_str), "simulation")
            .into_iter()
            .any(|event| is_expected_fill_event(&event, intent))
    })
    .unwrap_or(false)
}

#[derive(Clone)]
pub struct TxSender {
    tx: crossbeam::channel::Sender<TxWork>,
    velocity: &'static VelocityClient,
}

impl TxSender {
    pub fn confirm_tx(&self, tx: Signature) {
        self.tx
            .send(TxWork::Confirm {
                tx,
                ts: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
            })
            .expect("sent");
    }

    pub async fn send_tx(
        &self,
        tx: VersionedMessage,
        intent: TxIntent,
        cu_limit: u64,
    ) -> Option<Signature> {
        self.queue_tx(tx, None, false, intent, cu_limit).await
    }

    pub async fn send_fill_tx(
        &self,
        tx: VersionedMessage,
        simulation_tx: Option<VersionedMessage>,
        intent: TxIntent,
        cu_limit: u64,
    ) -> Option<Signature> {
        self.queue_tx(tx, simulation_tx, true, intent, cu_limit)
            .await
    }

    async fn queue_tx(
        &self,
        tx: VersionedMessage,
        simulation_tx: Option<VersionedMessage>,
        require_fill_event: bool,
        intent: TxIntent,
        cu_limit: u64,
    ) -> Option<Signature> {
        // The compute limit is sized from what the transaction burns, not a guess
        // with headroom. Simulating here, not in the worker, keeps it to one call.
        // The same reply also says whether the transaction is worth sending. Sizing
        // happens before signing, since the limit sits inside the signed message.
        // A fill path simulates its own message with `revert_fill` appended, so a
        // fill producing nothing fails simulation instead of landing empty. The
        // worker judges that case. The simulated compute already includes the
        // marker instruction, so it still sizes the real transaction correctly.
        let mut tx = tx;
        let probe = simulation_tx.clone().unwrap_or_else(|| tx.clone());
        let simulation = self
            .velocity
            .simulate_tx_with_commitment(probe, Some(CommitmentConfig::processed()))
            .await;
        let cu_limit = match &simulation {
            Ok(result) if result.err.is_none() => match result.units_consumed {
                Some(units) => {
                    let sized = size_compute_limit(units);
                    set_compute_unit_limit(&mut tx, sized);
                    u64::from(sized)
                }
                None => cu_limit,
            },

            // A transaction the simulation rejected is not sent, so what it
            // would have asked for never matters. The worker reports it.
            _ => cu_limit,
        };

        // no blockhash = subscription dead AND rpc fallback failed; silently dropping
        // every tx from here would be worse than a restart
        let blockhash = self
            .velocity
            .get_latest_blockhash()
            .await
            .expect("blockhash available; restart");
        let signed_tx = self.velocity.wallet().sign_tx(tx, blockhash).ok()?;
        let sig = signed_tx.signatures[0];

        self.tx
            .send(TxWork::Send {
                tx: signed_tx,
                presimulated: Some(simulation),
                simulation_tx,
                require_fill_event,
                ts: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
                intent,
                cu_limit,
            })
            .ok()?;

        Some(sig)
    }
}

/// The most one transaction may request.
pub const MAX_COMPUTE_UNITS: u64 = 1_400_000;

/// Twenty percent over covers a fill whose on-chain path differs a little
/// from the simulated one, such as a moved maker account or an extra
/// oracle branch. The floor keeps a small transaction from asking for less
/// than it takes to start.
fn size_compute_limit(units: u64) -> u32 {
    let sized = units.saturating_mul(12) / 10;
    sized.clamp(1_000, MAX_COMPUTE_UNITS) as u32
}

/// Rewrite the `SetComputeUnitLimit` instruction in place.
///
/// The limit is four bytes of instruction data, so the message keeps its shape.
/// No account moves and nothing is re-indexed. The transaction that gets signed
/// is the one that was simulated, except for the number it asks for. This does
/// nothing when the message carries no such instruction.
fn set_compute_unit_limit(message: &mut VersionedMessage, units: u32) {
    let compute_budget = compute_budget_id();
    let keys = message.static_account_keys().to_vec();
    let instructions = match message {
        VersionedMessage::Legacy(legacy) => &mut legacy.instructions,
        VersionedMessage::V0(v0) => &mut v0.instructions,
        VersionedMessage::V1(v1) => &mut v1.instructions,
    };

    for ix in instructions.iter_mut() {
        let is_compute_budget = keys
            .get(ix.program_id_index as usize)
            .is_some_and(|key| *key == compute_budget);
        // 2: set compute unit limit.
        if is_compute_budget && ix.data.first() == Some(&2) && ix.data.len() == 5 {
            ix.data[1..].copy_from_slice(&units.to_le_bytes());
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{
            is_expected_fill_event, is_revert_fill_error, record_perp_fill_fallback, Pubkey,
            TxIntent, VelocityEvent,
        },
        solana_instruction::error::InstructionError,
        solana_transaction::TransactionError,
        velocity_rs::velocity_idl::types::MarketType as EventMarketType,
    };

    // An unset override, below 0, makes the MM-sourced immediate threshold compare
    // the wall clock MM-oracle age against MM_ORACLE_MIN_WRITE_GAP. That matches the
    // program's `oracle_validity`. The age integrates per slot duration regime, so the
    // effective slot count scales with the clock.
    fn order_fill_event(taker: Pubkey, order_id: u32, base_filled: u64) -> VelocityEvent {
        VelocityEvent::OrderFill {
            maker: None,
            maker_fee: 0,
            maker_order_id: 0,
            maker_side: None,
            taker: Some(taker),
            taker_fee: 0,
            taker_order_id: order_id,
            taker_side: None,
            base_asset_amount_filled: base_filled,
            quote_asset_amount_filled: 1,
            market_index: 7,
            market_type: EventMarketType::Perp,
            oracle_price: 1,
            signature: "simulation".to_string(),
            tx_idx: 0,
            ts: 0,
            bit_flags: 0,
        }
    }

    #[test]
    fn fill_event_is_transaction_local_success_proof() {
        let taker = Pubkey::new_unique();
        let intent = TxIntent::LimitUncross {
            slot: 10,
            market_index: 7,
            taker_order_id: 42,
            taker_user: taker,
            maker_order_id: 9,
        };

        assert!(is_expected_fill_event(
            &order_fill_event(taker, 42, 1),
            &intent
        ));
        assert!(!is_expected_fill_event(
            &order_fill_event(taker, 42, 0),
            &intent
        ));
        assert!(!is_expected_fill_event(
            &order_fill_event(Pubkey::new_unique(), 42, 1),
            &intent
        ));
    }

    #[test]
    fn recognizes_revert_fill_simulation_error() {
        assert!(is_revert_fill_error(
            &TransactionError::InstructionError(3, InstructionError::Custom(6239)).into()
        ));
        assert!(!is_revert_fill_error(
            &TransactionError::InstructionError(3, InstructionError::Custom(6240)).into()
        ));
    }

    #[test]
    fn failed_liquidation_fill_records_takeover_fallback() {
        let liquidatee = Pubkey::new_unique();
        let intent = TxIntent::LiquidateWithFill {
            market_index: 7,
            liquidatee,
            slot: 42,
        };
        let fallbacks = std::sync::Arc::new(dashmap::DashMap::new());
        let error_code = velocity_rs::program::error::ErrorCode::LiquidationOrderFailedToFill
            as u32
            + anchor_lang::error::ERROR_CODE_OFFSET;

        record_perp_fill_fallback(
            &intent,
            &TransactionError::InstructionError(2, InstructionError::Custom(error_code)),
            Some(&fallbacks),
        );

        assert!(fallbacks.contains_key(&(liquidatee, 7)));
    }

    #[test]
    fn unrelated_failure_does_not_record_takeover_fallback() {
        let liquidatee = Pubkey::new_unique();
        let intent = TxIntent::LiquidateWithFill {
            market_index: 7,
            liquidatee,
            slot: 42,
        };
        let fallbacks = std::sync::Arc::new(dashmap::DashMap::new());

        record_perp_fill_fallback(
            &intent,
            &TransactionError::InstructionError(2, InstructionError::Custom(6239)),
            Some(&fallbacks),
        );

        assert!(fallbacks.is_empty());
    }
}
