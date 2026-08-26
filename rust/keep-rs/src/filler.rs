//! Filler Bot
use {
    crate::{
        http::{FeedHealth, Metrics},
        util::{
            pyth_update_is_fresh, swift_placement_expired, OrderSlotLimiter, PendingTxMeta,
            PendingTxs, PythPriceUpdate, TxIntent,
        },
        Config, UseMarkets,
    },
    anchor_lang::Discriminator,
    dashmap::DashMap,
    futures_util::StreamExt,
    pyth_lazer_protocol::router::TimestampUs,
    solana_account_decoder_client_types::UiAccountEncoding,
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_rpc_client_api::config::{
        RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcTransactionConfig,
    },
    solana_sdk::{
        instruction::InstructionError, signature::Signature, transaction::TransactionError,
    },
    solana_transaction_status_client_types::{UiTransactionEncoding, UiTransactionError},
    std::{
        collections::{BTreeMap, HashSet},
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    },
    tokio::{runtime::Handle, sync::RwLock},
    velocity_rs::{
        constants::PROGRAM_ID,
        dlob::{
            CrossesAndTopMakers, CrossingRegion, DLOBNotifier, L3Order, MakerCrosses, OrderKind,
            TakerOrder, DLOB,
        },
        event_subscriber::VelocityEvent,
        grpc::{
            grpc_subscriber::{AccountFilter, GrpcConnectionOpts},
            AccountUpdate, TransactionUpdate,
        },
        priority_fee_subscriber::PriorityFeeSubscriber,
        program::math::{
            auction::calculate_auction_price,
            constants::MM_ORACLE_MIN_WRITE_GAP,
            time::{Millis, SlotClock},
        },
        swift_order_subscriber::{SignedOrderInfo, SwiftOrderStream},
        types::{
            accounts::{PerpMarket, User, UserStats},
            CommitmentConfig, FeeTier, MarketId, MarketPrecision, MarketStatus, MarketType, Order,
            OrderParamsExt, OrderTriggerCondition, OrderType, PositionDirection, PostOnlyParam,
            RpcSendTransactionConfig, StateExt, VersionedMessage, VersionedTransaction, AMM,
        },
        GrpcSubscribeOpts, Pubkey, TransactionBuilder, VelocityClient, Wallet,
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
            // start the liveness clock at subscription time so a feed that never
            // delivers a single update still trips the health check
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
        let mut limiter = self.limiter;
        let velocity: &'static VelocityClient = Box::leak(Box::new(self.velocity));
        let dlob = self.dlob;
        let market_ids = self.market_ids;
        let filler_subaccount = self.filler_subaccount;
        let config = self.config.clone();
        let tx_worker_ref = self.tx_worker_ref.clone();
        let priority_fee_subscriber = Arc::clone(&self.priority_fee_subscriber);
        let metrics = Arc::clone(&self.metrics);
        let feed_health = Arc::clone(&self.feed_health);
        // reused per-slot scratch buffer for triggerable order ids (avoids per-slot allocation)
        let mut triggerable_buf: Vec<(Pubkey, u32)> = Vec::new();
        let mut slot = 0;
        // refresh state config on elapsed slots, not `slot % N == 0`: a skipped
        // exact multiple would otherwise stall the refresh for another window.
        const CONFIG_REFRESH_SLOTS: u64 = 300;
        let mut last_config_refresh_slot: u64 = 0;
        let mut use_median_trigger_price = velocity
            .state_account()
            .map(|s| s.has_median_trigger_price_feature())
            .unwrap_or(false);
        // seed with the real chain slot so a bot started after a gate switch
        // reflects it immediately, not only after the first config refresh
        let startup_slot = velocity.get_slot().await.unwrap_or(0);
        let mut slot_duration = crate::util::client_slot_duration(velocity, startup_slot);
        let mut slot_clock = velocity.slot_clock();
        dlob.update_slot_clock(slot_clock);
        // effective (actual-slot) staleness threshold: the onchain value is in
        // 400ms baseline units and inflated by slot_duration, mirroring
        // `oracle_validity`
        let mut slots_before_stale_for_amm = velocity
            .state_account()
            .map(|s| {
                Millis::from_stored_units(
                    s.oracle_guard_rails
                        .validity
                        .slots_before_stale_for_amm
                        .max(0) as u64,
                )
                .to_slots(slot_duration) as i64
            })
            .unwrap_or(10);
        let mut pyth_oracle_prices = BTreeMap::<u16, PythPriceUpdate>::new();
        // per-market consecutive perp-market/oracle cache-miss counters (see slot loop)
        let mut cache_misses = BTreeMap::<u16, u32>::new();
        // Per-market last-known oracle-stale state. Staleness is logged on transition
        // (fresh<->stale) instead of every slot, so a stale oracle shows as two edges
        // rather than a wall of per-slot lines during the exact window you're debugging.
        let mut oracle_stale_state = BTreeMap::<u16, bool>::new();
        // Per-market last-known pyth-price-stale state, for the same transition-only
        // logging as `oracle_stale_state`.
        let mut pyth_price_stale_state = BTreeMap::<u16, bool>::new();

        // Create a dummy receiver that never sends when pyth is disabled
        let (_dummy_tx, dummy_rx) = tokio::sync::mpsc::channel::<PythPriceUpdate>(1);
        let mut pyth_price_feed = self.pyth_price_feed.unwrap_or(dummy_rx);

        // Wall-clock age gate for cached pyth prices: a frozen feed leaves this cache
        // holding a price that's arbitrarily old with no signal of that in the update
        // itself, so a per-market timestamp check on every read is the only way to
        // catch it. Must stay strictly tighter than the program's
        // `PYTH_LAZER_MAX_STALENESS_SECONDS` (15s) so the bot stops trusting a price
        // before the program would reject it on-chain.
        const PYTH_PRICE_MAX_AGE_US: u64 = 10_000_000;

        // Swift reconnect backoff state (reset on successful resubscribe / first order)
        let mut retries = 0u32;

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
        loop {
            tokio::select! {
                biased;
                swift_order = swift_order_stream.next() => {
                    match swift_order {
                        Some(signed_order) => {
                            // reset
                            retries = 0;
                            last_swift_msg = std::time::Instant::now();

                            let order_params = signed_order.order_params();
                            let market_index = order_params.market_index;
                            log::info!(target: TARGET, "new swift order. uuid={}, market={}", signed_order.order_uuid_str(), market_index);
                            log::debug!(target: TARGET, "details: {signed_order:?}");
                            // transient cache misses must not kill the fill loop; drop this
                            // order (the swift feed keeps flowing) rather than panic
                            let Ok(perp_market) = velocity.try_get_perp_market_account(market_index) else {
                                log::warn!(target: TARGET, "no perp market {market_index} for swift order, skipping. uuid={}", signed_order.order_uuid_str());
                                continue;
                            };
                            // a fill tx sent now lands ~1 slot ahead (per tx_event
                            // latency_slots telemetry); evaluate fillability at landing, on the
                            // state the program will actually see. Overestimating here assumes a
                            // higher auction price than the program will compute and sends fill
                            // legs that no-op on-chain, so stay at the observed latency.
                            let landing_slot = slot + 1;
                            let Ok(oracle_price_data) = velocity.try_get_mmoracle_for_perp_market(market_index, landing_slot) else {
                                log::warn!(target: TARGET, "no oracle price for market {market_index}, skipping swift order. uuid={}", signed_order.order_uuid_str());
                                continue;
                            };
                            // Project the AMM to the state the program quotes at fill time
                            // (`AmmQuoter::setup`: curve snap + spread refresh). No oracle
                            // override: swift fill txs don't post the pyth price, so the
                            // program sees the chain oracle as-is.
                            let perp_market = velocity
                                .try_get_projected_perp_market(market_index, landing_slot, None)
                                .unwrap_or(perp_market);

                            // try an immediate fill against resting liquidity
                            match evaluate_swift_crosses(dlob, &signed_order, &perp_market, oracle_price_data.price, oracle_price_data.delay, landing_slot, slots_before_stale_for_amm, slot_clock) {
                                SwiftEval::Fillable(crosses) => {
                                    log::info!(target: TARGET, "found resting cross. market={market_index} oracle={} delay={} crosses={crosses:?}", oracle_price_data.price, oracle_price_data.delay);
                                    let pf = priority_fee_subscriber.priority_fee_nth(0.6);
                                    try_swift_fill(
                                        velocity,
                                        pf,
                                        config.swift_cu_limit,
                                        filler_subaccount,
                                        signed_order,
                                        crosses,
                                        tx_worker_ref.clone(),
                                    ).await;
                                }
                                SwiftEval::NotFillable(reason) => {
                                    // Well-formed but not marketable yet. Rather than dropping it,
                                    // place it on-chain (no fill) so it becomes a regular resting
                                    // order that the normal per-slot fill path will pick up while
                                    // it remains live. Skip if it can no longer be placed (the
                                    // program would reject/no-op it) to avoid wasting gas.
                                    let now_ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
                                    let order_slot = signed_order.slot();
                                    let auction_duration = order_params.auction_duration.unwrap_or(0);
                                    let max_ts = order_params.max_ts.unwrap_or(0);
                                    if swift_placement_expired(order_slot, auction_duration, max_ts, slot, now_ts, slot_clock) {
                                        log::debug!(target: TARGET, "swift order past placement window, not placing. uuid={}", signed_order.order_uuid_str());
                                        metrics.swift_place_skipped.inc();
                                    } else {
                                        log::info!(target: TARGET, "swift order not fillable yet ({reason}), placing on-chain. uuid={}", signed_order.order_uuid_str());
                                        let pf = priority_fee_subscriber.priority_fee_nth(0.6);
                                        try_swift_place(
                                            velocity,
                                            pf,
                                            config.swift_cu_limit,
                                            filler_subaccount,
                                            signed_order,
                                            slot,
                                            tx_worker_ref.clone(),
                                        ).await;
                                        metrics.swift_placed.inc();
                                    }
                                }
                                SwiftEval::Drop => {
                                    // malformed / unsupported; already logged in evaluate_swift_crosses
                                }
                            }
                        }
                        None => {
                            // Reconnect forever with capped backoff. Giving up after N
                            // retries left the bot permanently deaf to swift flow while
                            // reporting healthy — a swift-server outage longer than the
                            // retry budget must not require a manual restart.
                            feed_health.set_swift_connected(false);
                            retries += 1;
                            let backoff = 2u64.saturating_pow(retries.min(5)).min(30);
                            log::warn!(target: "swift", "feed disconnected, retry {retries} in {backoff}s");
                            tokio::time::sleep(Duration::from_secs(backoff)).await;

                            // keep the same ws url override as the initial subscription,
                            // otherwise a reconnect silently switches to the default host
                            match velocity
                                .subscribe_swift_orders(&market_ids, Some(true), None, std::env::var("SWIFT_WS_URL").ok())
                                .await
                            {
                                Ok(stream) => {
                                    log::info!(target: "swift", "feed resubscribed after {retries} attempt(s)");
                                    swift_order_stream = stream;
                                    retries = 0;
                                    last_swift_msg = std::time::Instant::now();
                                    feed_health.set_swift_connected(true);
                                }
                                Err(e) => {
                                    log::error!(target: "swift", "resubscribe failed: {e:?}");
                                    continue;
                                }
                            }
                        }
                    }
                }
                new_slot = slot_rx.recv() => {
                    if new_slot.is_none() {
                        log::error!(target: TARGET, "slot subscriber failed");
                        break;
                    }
                    slot = new_slot.expect("got slot update");
                    last_slot_update = std::time::Instant::now();
                    feed_health.touch_slot();
                    log::trace!(target: TARGET, "got slot update: {slot}");

                    let priority_fee = priority_fee_subscriber.priority_fee_nth(0.5) + slot % 2; // add entropy to produce unique tx hash on conseuctive tx resubmission
                    let t0 = std::time::SystemTime::now();
                    let unix_now = t0.duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64;

                    // check for auction and limit crosses in all markets
                    for market in &market_ids {
                        let market_index = market.index();

                        // skip the market this slot on a transient cache miss; next slot
                        // retries. a persistent miss panics (main loop => process restart)
                        // rather than silently never filling the market again
                        let cache_result = velocity
                            .try_get_perp_market_account(market_index)
                            .and_then(|perp_market| {
                                velocity
                                    .try_get_mmoracle_for_perp_market(market_index, slot)
                                    .map(|oracle| (perp_market, oracle))
                            });
                        let (perp_market, chain_oracle_data) = match cache_result {
                            Ok(x) => {
                                cache_misses.insert(market_index, 0);
                                x
                            }
                            Err(err) => {
                                let count = cache_misses.entry(market_index).or_insert(0);
                                *count += 1;
                                log::warn!(target: TARGET, "no perp market/oracle for market {market_index} ({count} consecutive): {err:?}, skipping fills this slot");
                                assert!(
                                    *count < MAX_CONSECUTIVE_ORACLE_MISSES,
                                    "market {market_index} unavailable for {count} consecutive slots"
                                );
                                continue;
                            }
                        };
                        let oracle_stale_for_amm = chain_oracle_data.delay > slots_before_stale_for_amm;
                        // Log staleness only on transition; the per-slot price/staleness dump
                        // was pure spam. The oracle price at the moment of an actual decision
                        // is carried on the fill/uncross events below instead.
                        let prev_stale = oracle_stale_state.insert(market_index, oracle_stale_for_amm);
                        if prev_stale != Some(oracle_stale_for_amm) {
                            if oracle_stale_for_amm {
                                log::warn!(target: TARGET, "oracle went stale market={market_index} delay={} oracle={} amm={}", chain_oracle_data.delay, chain_oracle_data.price, perp_market.market_stats.mm_oracle_price);
                            } else if prev_stale.is_some() {
                                log::info!(target: TARGET, "oracle recovered market={market_index} delay={}", chain_oracle_data.delay);
                            }
                        }
                        let mut oracle_price = chain_oracle_data.price as u64;
                        let trigger_price = perp_market.get_trigger_price(oracle_price as i64, unix_now, use_median_trigger_price).unwrap_or(oracle_price);
                        let mut pyth_update = None;
                        if let Some(p) = pyth_oracle_prices.get(&market_index) {
                            // capture the clock per market, not per slot: earlier markets in
                            // this loop await fill txs, so a slot-start timestamp can be
                            // seconds behind by the time later markets are evaluated
                            let now_us = TimestampUs::now();
                            let age_us = now_us.saturating_us_since(p.ts);
                            let is_stale = !pyth_update_is_fresh(p.ts, now_us, PYTH_PRICE_MAX_AGE_US);
                            // Log staleness only on transition, matching `oracle_stale_state` above.
                            let prev_stale = pyth_price_stale_state.insert(market_index, is_stale);
                            if prev_stale != Some(is_stale) {
                                if is_stale {
                                    log::warn!(target: TARGET, "pyth price went stale market={market_index} age_ms={} falling back to chain oracle", age_us / 1_000);
                                } else if prev_stale.is_some() {
                                    log::info!(target: TARGET, "pyth price recovered market={market_index} age_ms={}", age_us / 1_000);
                                }
                            }
                            metrics
                                .pyth_price_age_ms
                                .with_label_values(&[&market_index.to_string()])
                                .set((age_us / 1_000) as i64);
                            if !is_stale && oracle_price != p.price {
                                oracle_price = p.price;
                                pyth_update = Some(p.clone());
                            }
                        }
                        // Project the AMM to the state the program quotes at fill time
                        // (`AmmQuoter::setup`: curve snap + spread refresh, at the expected
                        // landing slot); crossing checks against the cached account state
                        // mis-price the vAMM quote and send fills that no-op on-chain with
                        // "taker does not cross amm".
                        //
                        // The view depends on the tx shape: auction fills post the pyth-lazer
                        // price in the same tx (fresh exchange oracle at landing), vamm-taker
                        // fills don't (the program sees the chain oracle as-is) — so project
                        // each view.
                        let chain_view_market = velocity
                            .try_get_projected_perp_market(market_index, slot + 1, None)
                            .unwrap_or(perp_market);
                        let perp_market = if pyth_update.is_some() {
                            velocity
                                .try_get_projected_perp_market(market_index, slot + 1, Some(oracle_price as i64))
                                .unwrap_or(perp_market)
                        } else {
                            chain_view_market
                        };

                        let mut crosses_and_top_makers = dlob.find_crosses_for_auctions(market_index, MarketType::Perp, slot, oracle_price, Some(&perp_market), trigger_price, None);
                        // key on the full (user, order_id) identity: order_id is a per-user
                        // counter, so a bare order_id collides across users and would wrongly
                        // suppress another user's fill
                        crosses_and_top_makers.crosses.retain(|(o, _)| limiter.allow_event(slot, order_dedup_key(&o.user, o.order_id)));

                        // resting orders crossed by the vAMM quote (at most one per side),
                        // computed by the same find pass; extract (along with the top makers, so
                        // the fill can route to a better-priced user maker) before
                        // `try_auction_fill` consumes the struct
                        let vamm_crossed_bid = crosses_and_top_makers.take_vamm_crossed_bid();
                        let vamm_crossed_ask = crosses_and_top_makers.take_vamm_crossed_ask();
                        let vamm_taker_top_makers = (
                            crosses_and_top_makers.top_maker_asks.to_vec(),
                            crosses_and_top_makers.top_maker_bids.to_vec(),
                        );

                        // Trigger orders that already cross are triggered+filled atomically by
                        // the auction path below (which sends a standalone trigger instead when
                        // it decides to skip the fill); capture their ids so the standalone
                        // trigger pass doesn't double-trigger (and waste) them. Keyed on the full
                        // (user, order_id) identity: order_id is a per-user counter, so a bare
                        // order_id collides across users and would wrongly suppress another
                        // user's trigger.
                        let crossing_trigger_ids: HashSet<(Pubkey, u32)> = crosses_and_top_makers
                            .crosses
                            .iter()
                            .filter(|(o, _)| matches!(o.kind, OrderKind::TriggerMarket | OrderKind::TriggerLimit))
                            .map(|(o, _)| (o.user, o.order_id))
                            .collect();

                        if !crosses_and_top_makers.crosses.is_empty() {
                            log::info!(target: TARGET, "found auction crosses. market={market_index} oracle={oracle_price} delay={} amm={} trigger={trigger_price} stale_for_amm={oracle_stale_for_amm} crosses={crosses_and_top_makers:?}", chain_oracle_data.delay, perp_market.market_stats.mm_oracle_price);
                            try_auction_fill(
                                velocity,
                                priority_fee,
                                config.fill_cu_limit,
                                config.trigger_cu_limit,
                                market_index,
                                filler_subaccount,
                                crosses_and_top_makers,
                                tx_worker_ref.clone(),
                                pyth_update,
                                trigger_price,
                                perp_market,
                                oracle_stale_for_amm,
                                chain_oracle_data.delay,
                            ).await;
                        }

                        // Trigger-only pass: trigger orders whose condition is met but that do
                        // not (yet) cross any liquidity. `find_crosses_for_auctions` only
                        // surfaces trigger orders whose post-trigger price immediately crosses,
                        // so without this pass e.g. stop/take-profit *limit* orders that rest
                        // after triggering would never be triggered. The send path simulates
                        // first, so an order that isn't actually triggerable on-chain (oracle
                        // view drift) is dropped at simulation rather than wasting a real tx.
                        dlob.find_triggerable_orders(market_index, MarketType::Perp, trigger_price, &mut triggerable_buf);
                        if !triggerable_buf.is_empty() {
                            log::info!(target: TARGET, "found {} triggerable order(s) (market: {market_index})", triggerable_buf.len());
                        }
                        for (taker_subaccount, order_id) in triggerable_buf.drain(..) {
                            // already handled atomically by the auction fill above
                            if crossing_trigger_ids.contains(&(taker_subaccount, order_id)) {
                                continue;
                            }
                            // Rate-limit re-sends by the full (user, order_id) identity. The
                            // limiter keys on u32, so fold the user pubkey in to avoid colliding
                            // with another user's order_id. The auction-fill pass uses the same
                            // key, so a trigger+fill and a standalone trigger of the same order
                            // share one rate-limit window.
                            if !limiter.allow_event(slot, order_dedup_key(&taker_subaccount, order_id)) {
                                continue;
                            }
                            try_trigger_order(
                                velocity,
                                priority_fee,
                                config.trigger_cu_limit,
                                market_index,
                                filler_subaccount,
                                taker_subaccount,
                                order_id,
                                slot + 1,
                                tx_worker_ref.clone(),
                            ).await;
                        }

                        // ghetto rate limit
                        if slot % 2 == 0 {
                            if let Some(crosses) = dlob.find_crossing_region(oracle_price, market_index, MarketType::Perp, Some(&perp_market)) {
                                log::info!(target: TARGET, "found limit crosses (market={market_index}) oracle={oracle_price} delay={}, top bid: {:?}, top ask: {:?}", chain_oracle_data.delay, crosses.crossing_bids.first(), crosses.crossing_asks.first());
                                try_uncross(velocity, slot + 1, priority_fee, config.fill_cu_limit, market_index, filler_subaccount, crosses, &tx_worker_ref).await;
                            }
                        }

                        // Resting-limit-vs-vAMM fills: a lone resting limit order that comes to
                        // cross the vAMM only after placement (price moved, or the mm-oracle was
                        // stale and later recovered) matches neither the auction path (needs a
                        // live auction) nor the uncross path (needs both book sides populated).
                        // The find pass above already detects these; previously its
                        // vamm_taker_bid/ask results were never consumed.
                        if vamm_crossed_bid.is_some() || vamm_crossed_ask.is_some() {
                            try_vamm_taker_fill(
                                velocity,
                                slot,
                                priority_fee,
                                config.fill_cu_limit,
                                market_index,
                                filler_subaccount,
                                [vamm_crossed_bid, vamm_crossed_ask],
                                vamm_taker_top_makers,
                                // vamm-taker txs don't post the pyth price: validate against
                                // the chain-oracle view or the fillable check passes on quotes
                                // the program won't reproduce
                                &chain_view_market,
                                oracle_stale_for_amm,
                                chain_oracle_data.delay,
                                &mut limiter,
                                &tx_worker_ref,
                            ).await;
                        }

                        // check state config ~every minute (elapsed-slot based)
                        if slot.saturating_sub(last_config_refresh_slot) >= CONFIG_REFRESH_SLOTS {
                            last_config_refresh_slot = slot;
                            use_median_trigger_price = velocity
                                .state_account()
                                .map(|s| s.has_median_trigger_price_feature())
                                .unwrap_or(false);
                            slot_duration = crate::util::client_slot_duration(velocity, slot);
                            slot_clock = velocity.slot_clock();
                            dlob.update_slot_clock(slot_clock);
                            slots_before_stale_for_amm = velocity
                                .state_account()
                                .map(|s| {
                                    Millis::from_stored_units(
                                        s.oracle_guard_rails.validity.slots_before_stale_for_amm.max(0) as u64,
                                    )
                                    .to_slots(slot_duration) as i64
                                })
                                .unwrap_or(10);
                        }
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
// ~2min of slot ticks at 400ms (shrinks in wall-clock as slot time drops —
// deliberate: this is a dead-feed restart tripwire, firing sooner is fine)
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

/// Evaluate whether a swift order will cross resting liquidity / the vAMM when a fill tx
/// sent now lands.
///
/// Returns `Fillable(crosses)` when the order should fill at landing, `NotFillable(reason)`
/// when it is well-formed but won't cross yet (the caller places it on-chain for the
/// per-slot fill loop to pick up), or `Drop` for malformed/unsupported orders.
///
/// `landing_slot` is the slot the fill tx is expected to land at. The taker's auction is
/// priced on the on-chain clock — the program starts a signed-msg order's auction at the
/// *message* slot (`signed_msg_taker_order_slot`), not the placement slot — so by landing
/// time the auction has already progressed a few price steps.
///
/// `perp_market` must have its AMM projected onto the oracle the program will quote with
/// at landing (see `try_get_projected_perp_market`); the program re-snaps the curve before
/// quoting bid/ask at fill time, so the cached reserves mis-price the vAMM quote.
///
/// `oracle_price` is the oracle price the program will see at landing (including any lazer
/// update posted with the fill); `oracle_delay` its age in slots, used to skip vAMM-only
/// fills when the oracle is stale for the AMM.
fn evaluate_swift_crosses(
    dlob: &DLOB,
    signed_order: &SignedOrderInfo,
    perp_market: &PerpMarket,
    oracle_price: i64,
    oracle_delay: i64,
    landing_slot: u64,
    slots_before_stale_for_amm: i64,
    slot_clock: SlotClock,
) -> SwiftEval {
    let mut order_params = signed_order.order_params();
    let _ = order_params.update_perp_auction_params(perp_market, oracle_price, true);

    // Post-only limits are maker orders: never taker-fill them, but do place them on-chain so
    // they rest on the book (the program cancels/amends them if they'd cross on placement).
    if order_params.order_type == OrderType::Limit && order_params.post_only != PostOnlyParam::None
    {
        return SwiftEval::NotFillable("post-only limit (maker order)".into());
    }

    let (start_price, end_price, duration) = (
        order_params.auction_start_price.unwrap_or_default(),
        order_params.auction_end_price.unwrap_or_default(),
        order_params.auction_duration.unwrap_or_default(),
    );
    // On-chain the auction clock starts at the signed message slot, not when the order is
    // placed. `min` guards `calculate_auction_price`'s elapsed-slot underflow when the
    // taker's slot is ahead of our slot subscriber.
    let order_slot = signed_order.slot().min(landing_slot);
    let order = Order {
        slot: order_slot,
        price: order_params.price,
        base_asset_amount: order_params.base_asset_amount,
        trigger_price: order_params.trigger_price.unwrap_or_default(),
        auction_duration: duration,
        auction_start_price: start_price,
        auction_end_price: end_price,
        max_ts: order_params.max_ts.unwrap_or_default(),
        oracle_price_offset: order_params.oracle_price_offset.unwrap_or_default(),
        market_index: order_params.market_index,
        order_type: order_params.order_type,
        market_type: order_params.market_type,
        direction: order_params.direction,
        reduce_only: order_params.reduce_only,
        post_only: order_params.post_only != PostOnlyParam::None,
        immediate_or_cancel: order_params.immediate_or_cancel(),
        trigger_condition: order_params.trigger_condition,
        bit_flags: order_params.bit_flags,
        ..Default::default()
    };

    let reserve_price = perp_market.amm.reserve_price().unwrap_or(0);
    let vamm_price = if order_params.direction == PositionDirection::Long {
        perp_market
            .amm
            .ask_price(
                reserve_price,
                perp_market.amm.long_spread,
                perp_market.amm.reference_price_offset,
            )
            .unwrap_or(0)
    } else {
        perp_market
            .amm
            .bid_price(
                reserve_price,
                perp_market.amm.short_spread,
                perp_market.amm.reference_price_offset,
            )
            .unwrap_or(0)
    };

    let price = match order_params.order_type {
        OrderType::Market | OrderType::Oracle => {
            match calculate_auction_price(
                &order,
                landing_slot,
                perp_market.price_tick(),
                Some(oracle_price),
                slot_clock,
            ) {
                Ok(p) => p,
                Err(err) => {
                    log::warn!(target: TARGET, "could not get auction price {err:?}, params: {order_params:?}, dropping...");
                    return SwiftEval::Drop;
                }
            }
        }
        OrderType::Limit => {
            match order.get_limit_price(
                Some(oracle_price),
                Some(vamm_price),
                landing_slot,
                perp_market.price_tick(),
                slot_clock,
            ) {
                Ok(Some(p)) => p,
                // No resolvable limit price at this slot (e.g. auction-limit with no final
                // price). Can't evaluate crossing without one, but the order is still valid
                // on-chain — place it rather than dropping it.
                _ => {
                    log::debug!(target: TARGET, "no limit price yet: {order_params:?}");
                    return SwiftEval::NotFillable("no resolvable limit price yet".into());
                }
            }
        }
        // Swift orders should never be trigger/unknown types; previously this panicked via
        // `unreachable!()`. Defensively drop instead so untrusted feed input can't crash the bot.
        other => {
            log::warn!(target: TARGET, "unsupported swift order type {other:?}, dropping. uuid={}", signed_order.order_uuid_str());
            return SwiftEval::Drop;
        }
    };

    let taker_order = TakerOrder::from_order_params(order_params, price);
    let crosses = dlob.find_crosses_for_taker_order(
        landing_slot,
        oracle_price as u64,
        taker_order,
        Some(perp_market),
        None,
    );
    // Well-formed but not (yet) fillable -> NotFillable, so the caller can place it on-chain.
    if crosses.is_empty() {
        let vamm_side = if order_params.direction == PositionDirection::Long {
            "ask"
        } else {
            "bid"
        };
        return SwiftEval::NotFillable(format!(
            "no cross at landing slot {landing_slot}: taker_price={price} vamm_{vamm_side}={vamm_price} auction_elapsed={}/{duration}",
            landing_slot.saturating_sub(order_slot),
        ));
    }
    // vAMM-only cross with a stale oracle: don't fill against the vAMM now, but the order is
    // still well-formed, so let the caller place it (it may fill once the oracle refreshes).
    if crosses.orders.is_empty()
        && crosses.has_vamm_cross
        && oracle_delay > slots_before_stale_for_amm
    {
        return SwiftEval::NotFillable(format!(
            "vAMM-only cross but oracle stale for AMM (delay={oracle_delay} oracle={oracle_price})"
        ));
    }
    SwiftEval::Fillable(crosses)
}

/// Outcome of evaluating a swift order against current liquidity.
enum SwiftEval {
    /// Crosses resting liquidity / vAMM right now: fill it immediately.
    Fillable(MakerCrosses),
    /// Well-formed but not taker-fillable now (not marketable yet, post-only maker order, or no
    /// resolvable limit price): place it on-chain so the slot loop can fill it later. Carries a
    /// human-readable reason for the placement log.
    NotFillable(String),
    /// Malformed / unsupported (bad auction price, non-market/limit type): drop it.
    Drop,
}

/// Trigger a single trigger order whose condition is met but that does not yet cross.
///
/// Sends a standalone `trigger_order` tx (no fill). The triggered order then becomes a regular
/// on-chain order that the normal per-slot auction-fill path will pick up. Failures to load the
/// accounts are logged and skipped rather than panicking.
async fn try_trigger_order(
    velocity: &'static VelocityClient,
    priority_fee: u64,
    cu_limit: u32,
    market_index: u16,
    filler_subaccount: Pubkey,
    taker_subaccount: Pubkey,
    order_id: u32,
    slot: u64,
    tx_worker_ref: TxSender,
) {
    // the bot's own account missing from cache is structural (lost subscription /
    // misconfig) and would silently no-op every fill: panic so the service restarts
    let filler_account_data = velocity
        .try_get_account::<User>(&filler_subaccount)
        .expect("filler subaccount in cache; restart");
    let taker_account_data = match velocity.try_get_account::<User>(&taker_subaccount) {
        Ok(a) => a,
        Err(err) => {
            log::warn!(target: TARGET, "trigger: failed to load taker account {taker_subaccount}: {err:?}");
            return;
        }
    };

    log::info!(target: TARGET, "attempting standalone trigger: order_id={order_id}, taker={taker_subaccount}");
    let tx_builder = TransactionBuilder::new(
        velocity.program_data(),
        filler_subaccount,
        std::borrow::Cow::Borrowed(&filler_account_data),
        false,
    )
    .with_priority_fee(priority_fee, Some(cu_limit))
    .trigger_order(
        taker_subaccount,
        &taker_account_data,
        order_id,
        (market_index, MarketType::Perp),
    );
    let tx = tx_builder.build();

    tx_worker_ref
        .send_tx(
            tx,
            TxIntent::Trigger {
                market_index,
                order_id,
                taker_user: taker_subaccount,
                slot,
            },
            cu_limit as u64,
        )
        .await;
}

/// Try to fill a swift order
async fn try_swift_fill(
    velocity: &'static VelocityClient,
    priority_fee: u64,
    cu_limit: u32,
    filler_subaccount: Pubkey,
    swift_order: SignedOrderInfo,
    crosses: MakerCrosses,
    tx_worker_ref: TxSender,
) {
    log::info!(target: TARGET, "try fill swift order: {}", swift_order.order_uuid_str());
    let taker_order = swift_order.order_params();
    let taker_subaccount = swift_order.taker_subaccount();
    let taker_authority = swift_order.taker_authority;

    // the bot's own account missing from cache is structural (lost subscription /
    // misconfig) and would silently no-op every fill: panic so the service restarts
    let filler_account_data = velocity
        .try_get_account::<User>(&filler_subaccount)
        .expect("filler subaccount in cache; restart");
    let taker_stats = Wallet::derive_stats_account(&taker_authority);
    let (taker_account_data, taker_stats) = match tokio::try_join!(
        velocity.get_account_value::<User>(&taker_subaccount),
        velocity.get_account_value::<UserStats>(&taker_stats)
    ) {
        Ok(accounts) => accounts,
        Err(err) => {
            log::warn!(target: TARGET, "swift fill: failed to load taker accounts {taker_subaccount}: {err:?}");
            return;
        }
    };
    let tx_builder = TransactionBuilder::new(
        velocity.program_data(),
        filler_subaccount,
        std::borrow::Cow::Borrowed(&filler_account_data),
        false,
    );

    let maker_accounts: Vec<User> = crosses
        .orders
        .iter()
        .filter(|m| m.0.user != taker_subaccount) // can't fill itself
        // drop makers not yet in cache rather than panicking; a missing maker just
        // shrinks the cross (handled by the empty-cross check below)
        .filter_map(|(m, _fill_size)| velocity.try_get_account::<User>(&m.user).ok())
        .collect();

    if maker_accounts.is_empty() && !crosses.has_vamm_cross {
        log::warn!("invalid cross: {crosses:?}");
        return;
    }

    // let taker_order_id = taker_account_data.next_order_id;
    let tx_builder = tx_builder
        .with_priority_fee(priority_fee, Some(cu_limit))
        .place_swift_order(&swift_order, &taker_account_data);
    let mut tx_builder =
        with_spot_interest_cranks(tx_builder, velocity, &taker_account_data, &maker_accounts)
            .fill_perp_order(
                taker_order.market_index,
                taker_subaccount,
                &taker_account_data,
                &taker_stats,
                None, // Some(taker_order_id), // assuming we're fast enough that its the taker_order_id, should be ok for retail
                maker_accounts.as_slice(),
                Some(swift_order.has_builder()),
            );

    // large accounts list, bump CU limit to compensate
    let mut effective_cu_limit = cu_limit;
    if let Some(ix) = tx_builder.ixs().last() {
        if ix.accounts.len() >= 30 {
            effective_cu_limit = cu_limit * 2;
            tx_builder = tx_builder.set_ix(
                1,
                ComputeBudgetInstruction::set_compute_unit_limit(effective_cu_limit),
            );
        }
    }
    let tx = tx_builder.build();

    tx_worker_ref
        .send_tx(
            tx,
            TxIntent::SwiftFill {
                uuid: swift_order.order_uuid(),
                market_index: taker_order.market_index,
                taker_user: taker_subaccount,
                maker_crosses: crosses,
            },
            effective_cu_limit as u64,
        )
        .await;
}

/// Place a swift order on-chain without filling it.
///
/// Used when the order is not immediately fillable on arrival: placing it makes it a regular
/// resting on-chain order that the normal per-slot fill path (and other keepers) can fill while
/// it remains live, instead of dropping it. Emits a `swift_place` wide event at tx
/// confirmation so the gas spent on placements can be measured against the fills they yield.
async fn try_swift_place(
    velocity: &'static VelocityClient,
    priority_fee: u64,
    cu_limit: u32,
    filler_subaccount: Pubkey,
    swift_order: SignedOrderInfo,
    slot: u64,
    tx_worker_ref: TxSender,
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

    let tx = TransactionBuilder::new(
        velocity.program_data(),
        filler_subaccount,
        std::borrow::Cow::Borrowed(&filler_account_data),
        false,
    )
    .with_priority_fee(priority_fee, Some(cu_limit))
    .place_swift_order(&swift_order, &taker_account_data)
    .build();

    tx_worker_ref
        .send_tx(
            tx,
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
/// The program refuses a fill when the taker, or any maker, carries a borrow in a
/// spot market whose interest has not accrued recently enough
/// (`SpotMarketInterestStaleForMargin`): the margin check values that borrow
/// through a stale index and understates the debt. `fill_perp_order`
/// receives those markets read-only and cannot refresh them, so the permissionless
/// crank rides in the same transaction. Call this before `fill_perp_order`, which
/// also keeps the fill as the last instruction for the account-count check.
///
/// A market this misses only costs a reverted fill.
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

/// Build the broadcast transaction and, when needed, a guarded simulation variant.
///
/// The worker requires a matching nonzero `OrderFill` event from this simulation because
/// `RevertFill` only proves that the filler was active sometime in the current slot; activity from
/// an earlier transaction can otherwise produce a false positive. Ordinary fills strip
/// `RevertFill` after simulation to reduce transaction size and compute. Pyth-update transactions
/// retain it as an additional execution-time rollback guard.
fn build_fill_tx(
    tx_builder: TransactionBuilder<'_>,
    retain_revert_fill: bool,
) -> (VersionedMessage, Option<VersionedMessage>) {
    if retain_revert_fill {
        (tx_builder.revert_fill().build(), None)
    } else {
        let tx = tx_builder.clone().build();
        let simulation_tx = tx_builder.revert_fill().build();
        (tx, Some(simulation_tx))
    }
}

/// Try to fill an auction order
///
/// - `auction_crosses` list of one or more crosses to fill
async fn try_auction_fill(
    velocity: &'static VelocityClient,
    priority_fee: u64,
    cu_limit: u32,
    trigger_cu_limit: u32,
    market_index: u16,
    filler_subaccount: Pubkey,
    auction_crosses: CrossesAndTopMakers,
    tx_worker_ref: TxSender,
    oracle_update: Option<PythPriceUpdate>,
    trigger_price: u64,
    perp_market: PerpMarket,
    oracle_stale_for_amm: bool,
    oracle_delay: i64,
) {
    // the bot's own account missing from cache is structural (lost subscription /
    // misconfig) and would silently no-op every fill: panic so the service restarts
    let filler_account_data = velocity
        .try_get_account::<User>(&filler_subaccount)
        .expect("filler subaccount in cache; restart");

    // drop makers not yet in cache rather than panicking; a shorter top-maker
    // list just means fewer fallback makers on the fill
    let top_maker_asks: Vec<User> = auction_crosses
        .top_maker_asks
        .iter()
        .filter_map(|m| velocity.try_get_account::<User>(m).ok())
        .collect();

    let top_maker_bids: Vec<User> = auction_crosses
        .top_maker_bids
        .iter()
        .filter_map(|m| velocity.try_get_account::<User>(m).ok())
        .collect();
    let mut sent_oracle_update = false;
    for (taker_order, crosses) in auction_crosses.crosses {
        log::info!(target: TARGET, "try fill auction order: {taker_order:?}");
        let taker_subaccount = taker_order.user;

        let Some((taker_account_data, taker_stats)) =
            fetch_user_and_stats(velocity, &taker_subaccount, "auction fill")
        else {
            continue;
        };

        let mut tx_builder = TransactionBuilder::new(
            velocity.program_data(),
            filler_subaccount,
            std::borrow::Cow::Borrowed(&filler_account_data),
            false,
        );

        tx_builder = tx_builder.with_priority_fee(priority_fee, Some(cu_limit));

        let mut includes_oracle_update = false;
        if let Some(ref update_msg) = oracle_update {
            if !sent_oracle_update {
                tx_builder = tx_builder
                    .post_pyth_lazer_oracle_update(&[update_msg.feed_id], &update_msg.message);
                sent_oracle_update = true;
                includes_oracle_update = true;
            }
        }

        let taker_is_trigger = matches!(
            taker_order.kind,
            OrderKind::TriggerMarket | OrderKind::TriggerLimit
        );
        if taker_is_trigger {
            // The order may have been triggered/filled/cancelled between the DLOB snapshot and
            // this fetch; skip rather than panic the run loop.
            let actual_order = match taker_account_data
                .orders
                .iter()
                .find(|o| o.order_id == taker_order.order_id)
            {
                Some(o) => o,
                None => {
                    log::debug!(target: TARGET, "trigger order {} gone before fill, skipping", taker_order.order_id);
                    continue;
                }
            };

            let trigger_above = matches!(
                actual_order.trigger_condition,
                OrderTriggerCondition::Above | OrderTriggerCondition::TriggeredAbove
            );

            let can_trigger = if trigger_above && trigger_price > actual_order.trigger_price {
                true
            } else if !trigger_above && trigger_price < actual_order.trigger_price {
                true
            } else {
                false
            };
            if !can_trigger {
                continue;
            }
            log::info!(
                target: TARGET,
                "attempting trigger and fill: trigger_price={trigger_price}, order_price={}, {:?}/{:?}",
                actual_order.trigger_price,
                taker_order.order_id,
                taker_order.user
            );
            tx_builder = tx_builder.trigger_order(
                taker_subaccount,
                &taker_account_data,
                taker_order.order_id,
                (market_index, MarketType::Perp),
            );
        }

        let mut maker_accounts: Vec<User> = crosses
            .orders
            .iter()
            .filter(|m| m.0.user != taker_subaccount) // can't fill itself
            // drop makers not yet in cache rather than panicking; a missing maker just
            // shrinks the cross (handled by the empty-cross check below)
            .filter_map(|(m, _fill_size)| velocity.try_get_account::<User>(&m.user).ok())
            .collect();

        // The on-chain order backing this cross; used for the program's low-risk rule and the
        // AMM fill sizing. It may be gone (filled/cancelled since the DLOB snapshot) — then the
        // vAMM leg can't be validated, so it doesn't count towards sending the fill.
        let actual_order = taker_account_data
            .orders
            .iter()
            .find(|o| o.order_id == taker_order.order_id);

        // Mirror the program's AMM availability gates (`amm_fill_gates_ok` +
        // `amm_fill_timing_ok`): drawdown and oracle staleness hard-block; an order not yet
        // "low risk" (placed within the oracle delay, `User::is_low_risk_for_amm`) additionally
        // needs the AMM to want to JIT-make in the taker direction. Inputs are hoisted into
        // locals so the cross-decision wide event can carry each one.
        let drawdown = perp_market.has_too_much_drawdown().unwrap_or(false);
        let order_low_risk = actual_order
            .is_some_and(|o| (crosses.slot as i64).saturating_sub(oracle_delay) > o.slot as i64);
        let wants_jit = amm_wants_to_jit_make(
            &perp_market.amm,
            perp_market.order_step_size,
            crosses.taker_direction,
        );
        // JIT leg validates the MM oracle at the landing slot (crosses were snapshotted at
        // `crosses.slot`; the fill lands ~next slot). A same-slot snapshot that looks fresh
        // routinely lands one slot stale under the immediate threshold, so measure at landing.
        let landing_slot = crosses.slot.saturating_add(1);
        let mm_stale_immediate =
            mm_oracle_stale_for_amm_immediate(&perp_market, landing_slot, velocity.slot_clock());
        let mut vamm_usable = crosses.has_vamm_cross
            && vamm_can_fill_taker(
                drawdown,
                oracle_stale_for_amm,
                order_low_risk,
                wants_jit,
                mm_stale_immediate,
            );

        // vAMM-fillable size when it was computable; carried on the wide event either way
        let mut vamm_fillable: Option<u64> = None;
        if vamm_usable {
            if let (Ok(pos), Some(order)) = (
                taker_account_data.get_perp_position(market_index),
                actual_order,
            ) {
                if let Ok((base_asset_amount, _limit_price)) =
                    velocity_rs::program::math::orders::calculate_base_asset_amount_for_amm_to_fulfill(
                        order,
                        &perp_market,
                        None,
                        None,
                        pos.base_asset_amount,
                        &FeeTier::default(),
                    )
                {
                    vamm_fillable = Some(base_asset_amount);
                    // if user position is less than min order size, step size is the threshold
                    let amm_size_threshold = if !taker_order.is_reduce_only()
                        && pos.base_asset_amount.unsigned_abs()
                            > perp_market.market_stats.min_order_size
                    {
                        perp_market.market_stats.min_order_size
                    } else {
                        perp_market.order_step_size
                    };
                    if base_asset_amount < amm_size_threshold {
                        vamm_usable = false;
                    }
                }
            }
        }

        let action = classify_cross(
            crosses.has_vamm_cross,
            vamm_usable,
            !maker_accounts.is_empty(),
        );
        emit_cross_decision_event(
            market_index,
            &taker_subaccount,
            taker_order.order_id,
            crosses.slot,
            &action,
            crosses.has_vamm_cross,
            oracle_stale_for_amm,
            oracle_delay,
            drawdown,
            order_low_risk,
            wants_jit,
            mm_stale_immediate,
            vamm_fillable,
            maker_accounts.len(),
        );
        match action {
            CrossAction::Skip => {
                if oracle_stale_for_amm && crosses.has_vamm_cross {
                    log::info!(target: TARGET, "skip vAMM fill: oracle stale for AMM (market={market_index})");
                } else {
                    log::debug!(target: TARGET, "skip cross (vamm gated, no makers): {crosses:?}");
                }
                // The fill tx (and the trigger ix piggybacked on it) is dropped, but the
                // taker is a trigger order whose condition is met. The run loop's
                // standalone trigger pass suppresses crossing trigger orders on the
                // assumption this path triggers them atomically — so send the trigger
                // alone here, or the order stays untriggered until another keeper acts.
                if taker_is_trigger {
                    log::info!(
                        target: TARGET,
                        "cross skipped but taker trigger condition met; sending standalone trigger: market={market_index}, order={}/{}, slot={}",
                        taker_order.order_id,
                        taker_subaccount,
                        crosses.slot,
                    );
                    try_trigger_order(
                        velocity,
                        priority_fee,
                        trigger_cu_limit,
                        market_index,
                        filler_subaccount,
                        taker_subaccount,
                        taker_order.order_id,
                        crosses.slot + 1,
                        tx_worker_ref.clone(),
                    )
                    .await;
                }
                continue;
            }
            CrossAction::FillMakersOnly => {
                log::debug!(target: TARGET, "vamm leg gated, filling against makers only: {crosses:?}");
            }
            CrossAction::FillWithVamm => {}
        }

        if maker_accounts.len() < 3 {
            if crosses.taker_direction == PositionDirection::Long {
                maker_accounts = top_maker_asks.clone();
            } else {
                maker_accounts = top_maker_bids.clone();
            }
        }

        tx_builder =
            with_spot_interest_cranks(tx_builder, velocity, &taker_account_data, &maker_accounts)
                .fill_perp_order(
                    market_index,
                    taker_subaccount,
                    &taker_account_data,
                    &taker_stats,
                    Some(taker_order.order_id),
                    maker_accounts.as_slice(),
                    None,
                );

        // large accounts list, bump CU limit to compensate
        let mut effective_cu_limit = cu_limit;
        if let Some(ix) = tx_builder.ixs().last() {
            if ix.accounts.len() >= 20 {
                effective_cu_limit = cu_limit * 2;
                tx_builder = tx_builder.set_ix(
                    1,
                    ComputeBudgetInstruction::set_compute_unit_limit(effective_cu_limit),
                );
            }
        }

        let (tx, simulation_tx) = build_fill_tx(tx_builder, includes_oracle_update);

        tx_worker_ref
            .send_fill_tx(
                tx,
                simulation_tx,
                TxIntent::AuctionFill {
                    market_index,
                    taker_order_id: taker_order.order_id,
                    taker_user: taker_subaccount,
                    maker_crosses: crosses,
                    has_trigger: taker_is_trigger,
                },
                effective_cu_limit as u64,
            )
            .await;
    }
}

/// Try to uncross top of book
///
/// - `crosses` list of one or more crosses to fill
async fn try_uncross(
    velocity: &VelocityClient,
    slot: u64,
    priority_fee: u64,
    cu_limit: u32,
    market_index: u16,
    filler_subaccount: Pubkey,
    crosses: CrossingRegion,
    tx_worker_ref: &TxSender,
) {
    // the bot's own account missing from cache is structural (lost subscription /
    // misconfig) and would silently no-op every fill: panic so the service restarts
    let filler_account_data = velocity
        .try_get_account::<User>(&filler_subaccount)
        .expect("filler subaccount in cache; restart");

    let best_bid = &crosses.crossing_bids.first();
    let best_ask = &crosses.crossing_asks.first();

    if best_bid.is_none() || best_ask.is_none() {
        return;
    }

    let best_bid = best_bid.unwrap();
    let best_ask = best_ask.unwrap();

    // Only a resting limit order can act as a maker on-chain (`is_maker_for_taker`
    // requires `Order::is_resting_limit_order`): DLOB kinds Limit/FloatingLimit.
    // Market/Oracle (auction) and untriggered trigger orders never match as makers —
    // attaching them just lands a no-op fill that burns the tx fee.
    fn is_maker_eligible(o: &L3Order) -> bool {
        matches!(o.kind, OrderKind::Limit | OrderKind::FloatingLimit)
    }

    // crossing counterparty orders considered as makers for each taker leg (the program
    // picks the actual maker orders to match; these determine the accounts attached)
    let maker_ask_orders: Vec<&L3Order> = crosses
        .crossing_asks
        .iter()
        .take(3)
        .filter(|x| x.user != best_bid.user)
        .collect();

    let maker_bid_orders: Vec<&L3Order> = crosses
        .crossing_bids
        .iter()
        .take(3)
        .filter(|x| x.user != best_ask.user)
        .collect();

    let maker_asks: Vec<User> = maker_ask_orders
        .iter()
        .filter(|x| is_maker_eligible(x))
        .filter_map(|x| velocity.try_get_account::<User>(&x.user).ok())
        .collect();

    let maker_bids: Vec<User> = maker_bid_orders
        .iter()
        .filter(|x| is_maker_eligible(x))
        .filter_map(|x| velocity.try_get_account::<User>(&x.user).ok())
        .collect();

    log::info!(target: TARGET, "try uncross book={market_index},slot={slot}");
    log::debug!(
        target: TARGET,
        "X asks: {:?}, X bids: {:?}",
        &crosses.crossing_asks.iter().take(3),
        &crosses.crossing_bids.iter().take(3),
    );

    // try valid combinations of taker/maker with all crossing asks/bids
    for (taker_order, maker_orders, makers) in [
        (best_ask, &maker_bid_orders, maker_bids),
        (best_bid, &maker_ask_orders, maker_asks),
    ] {
        if taker_order.is_post_only() {
            emit_uncross_attempt_event(
                market_index,
                slot,
                "skip_taker_post_only",
                taker_order,
                maker_orders,
                makers.len(),
            );
            continue;
        }

        if makers.is_empty() {
            // distinguish "nothing on the other side" from "counterparties exist but
            // none can act as a maker on-chain" (per-maker `eligible` in the event)
            let action = if maker_orders.is_empty() {
                "skip_no_makers"
            } else {
                "skip_no_eligible_makers"
            };
            log::debug!(target: TARGET, "no eligible makers to uncross (market={market_index})");
            emit_uncross_attempt_event(market_index, slot, action, taker_order, maker_orders, 0);
            continue;
        }

        let taker_order_id = taker_order.order_id;
        let taker_subaccount = taker_order.user;
        let Some((taker_account_data, taker_stats)) =
            fetch_user_and_stats(velocity, &taker_subaccount, "uncross")
        else {
            continue;
        };

        let mut tx_builder = TransactionBuilder::new(
            velocity.program_data(),
            filler_subaccount,
            std::borrow::Cow::Borrowed(&filler_account_data),
            false,
        );
        tx_builder = tx_builder.with_priority_fee(priority_fee, Some(cu_limit));
        tx_builder = with_spot_interest_cranks(tx_builder, velocity, &taker_account_data, &makers)
            .fill_perp_order(
                market_index,
                taker_subaccount,
                &taker_account_data,
                &taker_stats,
                Some(taker_order_id),
                makers.as_slice(),
                None,
            );

        // large accounts list, bump CU limit to compensate
        let mut effective_cu_limit = cu_limit;
        if let Some(ix) = tx_builder.ixs().last() {
            if ix.accounts.len() >= 40 {
                effective_cu_limit = (cu_limit * 25) / 10;
                tx_builder = tx_builder.set_ix(
                    1,
                    ComputeBudgetInstruction::set_compute_unit_limit(effective_cu_limit),
                );
            }
        }
        let (tx, simulation_tx) = build_fill_tx(tx_builder, false);

        emit_uncross_attempt_event(
            market_index,
            slot,
            "sent",
            taker_order,
            maker_orders,
            makers.len(),
        );
        tx_worker_ref
            .send_fill_tx(
                tx,
                simulation_tx,
                TxIntent::LimitUncross {
                    slot,
                    market_index,
                    taker_order_id,
                    taker_user: taker_subaccount,
                    maker_order_id: maker_orders.first().map(|m| m.order_id).unwrap_or(0),
                },
                effective_cu_limit as u64,
            )
            .await;
    }
}

/// Fill resting limit orders that the vAMM quote crosses (`find_crosses_for_auctions`'
/// `vamm_taker_bid`/`vamm_taker_ask` results).
///
/// Sends a `fill_perp_order` with NO maker accounts: the program then sources the fill from
/// the vAMM, dispatching on `order.post_only` (`math/fulfillment.rs`):
/// - non-post-only: ordinary vAMM fill — the resting order takes against the AMM quote
/// - post-only: vAMM-taker fill — the AMM crosses the resting maker at the maker's price
///   (`determine_perp_fulfillment_methods_for_maker`)
///
/// Before sending, the cross is re-validated with the program's own
/// `calculate_base_asset_amount_for_amm_to_fulfill`, capped at the order's current limit
/// price (with the post-only maker-rebate buffer applied inside), so a nonzero result here
/// matches the on-chain outcome; residual view drift is dropped by the send path's
/// pre-simulation.
///
/// When the mm-oracle is stale for AMM fills, only orders strictly older than the oracle data
/// (`slot - oracle_delay > order.slot`) are attempted — the program's low-risk rule: such
/// orders cannot be exploiting staleness and fill against the exchange-oracle fallback.
/// Orders that only become fillable once the mm-oracle recovers are picked up on a later
/// slot's pass.
///
/// Expired crossing orders are attempted too: the program converts the fill into an
/// expiry-cancel (flat filler reward), which clears them off the book.
#[allow(clippy::too_many_arguments)]
async fn try_vamm_taker_fill(
    velocity: &'static VelocityClient,
    slot: u64,
    priority_fee: u64,
    cu_limit: u32,
    market_index: u16,
    filler_subaccount: Pubkey,
    // the crossed resting bid and ask (at most one per book side)
    candidates: [Option<L3Order>; 2],
    // (top ask makers, top bid makers): each candidate's fill includes the opposite-side
    // makers so the program can route to a better price than the vAMM if one crosses too
    top_makers: (Vec<Pubkey>, Vec<Pubkey>),
    perp_market: &PerpMarket,
    oracle_stale_for_amm: bool,
    oracle_delay: i64,
    limiter: &mut OrderSlotLimiter<40>,
    tx_worker_ref: &TxSender,
) {
    // the bot's own account missing from cache is structural (lost subscription /
    // misconfig) and would silently no-op every fill: panic so the service restarts
    let filler_account_data = velocity
        .try_get_account::<User>(&filler_subaccount)
        .expect("filler subaccount in cache; restart");

    for l3_order in candidates.into_iter().flatten() {
        let user_subaccount = l3_order.user;
        let Some((user_account, user_stats)) =
            fetch_user_and_stats(velocity, &user_subaccount, "vamm-taker fill")
        else {
            continue;
        };
        let Some(order) = user_account
            .orders
            .iter()
            .find(|o| o.order_id == l3_order.order_id)
            .copied()
        else {
            continue;
        };

        // during mm-oracle staleness only orders strictly older than the oracle data can
        // fill, against the exchange-oracle fallback (`User::is_low_risk_for_amm`:
        // `clock_slot - mm_oracle_delay > order.slot`); newer ones wait for recovery
        if oracle_stale_for_amm && (slot as i64).saturating_sub(oracle_delay) <= order.slot as i64 {
            emit_vamm_taker_decision_event(
                market_index,
                &user_subaccount,
                l3_order.order_id,
                slot,
                "skip_order_too_new",
                order.post_only,
                order.slot,
                oracle_stale_for_amm,
                oracle_delay,
                l3_order.price,
                None,
                None,
            );
            continue;
        }

        let existing_base = user_account
            .get_perp_position(market_index)
            .map(|p| p.base_asset_amount)
            .unwrap_or(0);

        let Ok((fillable, _)) =
            velocity_rs::program::math::orders::calculate_base_asset_amount_for_amm_to_fulfill(
                &order,
                perp_market,
                Some(l3_order.price),
                None,
                existing_base,
                &FeeTier::default(),
            )
        else {
            continue;
        };

        // same size threshold as the auction-path vamm fill: if the user's position is
        // less than min order size, step size is the threshold
        let amm_size_threshold = if !order.reduce_only
            && existing_base.unsigned_abs() > perp_market.market_stats.min_order_size
        {
            perp_market.market_stats.min_order_size
        } else {
            perp_market.order_step_size
        };
        if fillable < amm_size_threshold {
            emit_vamm_taker_decision_event(
                market_index,
                &user_subaccount,
                l3_order.order_id,
                slot,
                "skip_too_small",
                order.post_only,
                order.slot,
                oracle_stale_for_amm,
                oracle_delay,
                l3_order.price,
                Some(fillable),
                Some(amm_size_threshold),
            );
            continue;
        }

        // rate-limited re-attempts are not wide-logged (see `emit_vamm_taker_decision_event`)
        if !limiter.allow_event(slot, order_dedup_key(&user_subaccount, l3_order.order_id)) {
            continue;
        }

        log::info!(
            target: TARGET,
            "try vamm-taker fill: market={market_index} user={user_subaccount} order={} limit={} fillable={fillable} stale_for_amm={oracle_stale_for_amm}",
            l3_order.order_id,
            l3_order.price,
        );
        emit_vamm_taker_decision_event(
            market_index,
            &user_subaccount,
            l3_order.order_id,
            slot,
            "sent",
            order.post_only,
            order.slot,
            oracle_stale_for_amm,
            oracle_delay,
            l3_order.price,
            Some(fillable),
            Some(amm_size_threshold),
        );

        // opposite-side top makers: if a user maker crosses too, the program routes the
        // fill to the best price among the provided makers and the vAMM
        let (ref top_maker_asks, ref top_maker_bids) = top_makers;
        let maker_accounts: Vec<User> = if l3_order.is_long() {
            top_maker_asks
        } else {
            top_maker_bids
        }
        .iter()
        .filter(|m| **m != user_subaccount) // can't fill itself
        .filter_map(|m| velocity.try_get_account::<User>(m).ok())
        .collect();

        let tx_builder = TransactionBuilder::new(
            velocity.program_data(),
            filler_subaccount,
            std::borrow::Cow::Borrowed(&filler_account_data),
            false,
        )
        .with_priority_fee(priority_fee, Some(cu_limit));
        let mut tx_builder =
            with_spot_interest_cranks(tx_builder, velocity, &user_account, &maker_accounts)
                .fill_perp_order(
                    market_index,
                    user_subaccount,
                    &user_account,
                    &user_stats,
                    Some(l3_order.order_id),
                    maker_accounts.as_slice(),
                    None,
                );

        // large accounts list, bump CU limit to compensate
        let mut effective_cu_limit = cu_limit;
        if let Some(ix) = tx_builder.ixs().last() {
            if ix.accounts.len() >= 20 {
                effective_cu_limit = cu_limit * 2;
                tx_builder = tx_builder.set_ix(
                    1,
                    ComputeBudgetInstruction::set_compute_unit_limit(effective_cu_limit),
                );
            }
        }
        let (tx, simulation_tx) = build_fill_tx(tx_builder, false);

        tx_worker_ref
            .send_fill_tx(
                tx,
                simulation_tx,
                TxIntent::VAMMTakerFill {
                    slot,
                    market_index,
                    maker_order_id: l3_order.order_id,
                    taker_user: user_subaccount,
                },
                effective_cu_limit as u64,
            )
            .await;
    }
}

/// Fetch a fill counterparty's user account and stats from the local cache.
///
/// Returns `None` (with a warn log) when either is missing — the account may have been
/// closed between the DLOB snapshot and now, or its stats subscription hasn't landed yet;
/// callers skip the fill rather than panic. Shared by the auction, uncross, and
/// vamm-taker fill paths.
fn fetch_user_and_stats(
    velocity: &VelocityClient,
    subaccount: &Pubkey,
    context: &str,
) -> Option<(User, UserStats)> {
    let Ok(user) = velocity.try_get_account::<User>(subaccount) else {
        log::warn!(target: TARGET, "{context}: user account {subaccount} not in cache, skipping");
        return None;
    };
    match velocity.try_get_account::<UserStats>(&Wallet::derive_stats_account(&user.authority)) {
        Ok(stats) => Some((user, stats)),
        Err(_) => {
            log::warn!(target: TARGET, "{context}: failed to fetch user stats: {:?}, skipping", user.authority);
            None
        }
    }
}

/// Fold a `(user, order_id)` pair into a single u32 for the `OrderSlotLimiter` (which keys on
/// u32). `order_id` is a per-user counter, so a bare order_id collides across users; mixing in
/// the user pubkey prefix makes cross-user collisions negligible.
fn order_dedup_key(user: &Pubkey, order_id: u32) -> u32 {
    let b = user.to_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]]) ^ order_id
}

/// Whether the vAMM can participate in filling a taker order right now, mirroring the
/// program's gates (`PerpMarket::amm_fill_gates_ok` + `amm_fill_timing_ok`):
/// - `drawdown` (or the low-risk oracle staleness) alone hard-blocks every AMM fill;
/// - a "low risk" order (rested longer than the oracle delay, `User::is_low_risk_for_amm`)
///   fills unconditionally past the hard gates;
/// - otherwise (still within the oracle delay, e.g. mid-auction) the AMM only fills via the
///   immediate JIT leg, which the program gates on `FillOrderAmmImmediate` oracle validity —
///   a *tighter* staleness bound than the low-risk one (`mm_stale_immediate`) — in addition to
///   the AMM *wanting* to JIT-make in the taker's direction.
///
/// The immediate leg reads the *MM* oracle (`market_stats.mm_oracle_slot`), which a pyth-lazer
/// update posted in the fill tx does NOT refresh, so a MM crank that lands even one slot late
/// closes it while the exchange oracle looks fresh. Gating the JIT branch only on the loose
/// low-risk staleness (as before) sent fills that no-op on-chain with "oracle not valid for
/// immediate fills" — the `vamm_taker no_fill` spam.
///
/// Best-effort economy filter only — the program re-checks everything; a `true` here that the
/// program rejects just costs a failed simulation.
fn vamm_can_fill_taker(
    drawdown: bool,
    oracle_stale_for_amm: bool,
    order_low_risk: bool,
    amm_wants_to_jit_make: bool,
    mm_stale_immediate: bool,
) -> bool {
    !drawdown
        && !oracle_stale_for_amm
        && (order_low_risk || (amm_wants_to_jit_make && !mm_stale_immediate))
}

/// MM-oracle staleness for the *immediate* (JIT) AMM-fill leg, mirroring the program's
/// `is_stale_for_amm_immediate` (`math/oracle.rs`) with the per-market
/// `oracle_slot_delay_override`: a positive override is used as-is; `override == 0` disables
/// the immediate leg entirely (always stale); negative means unset and resolves to
/// `MM_ORACLE_MIN_WRITE_GAP` for an MM-sourced price (the program refuses MM-oracle writes
/// closer together than that, so a tighter threshold is unsatisfiable). Delay is measured
/// against the *MM* oracle slot (`market_stats.mm_oracle_slot`) at the expected landing slot,
/// since that — not the exchange oracle — is what the JIT leg validates.
fn mm_oracle_stale_for_amm_immediate(
    perp_market: &PerpMarket,
    landing_slot: u64,
    slot_clock: SlotClock,
) -> bool {
    let mm_oracle_delay =
        (landing_slot as i64).saturating_sub(perp_market.market_stats.mm_oracle_slot as i64);
    // the age is wall clock, integrated per slot duration regime like
    // `oracle_validity`; thresholds are 400ms baseline units
    let mm_oracle_age = slot_clock.elapsed_slot_delta(mm_oracle_delay.max(0) as u64, landing_slot);
    let override_ = perp_market.oracle_slot_delay_override;
    if override_ > 0 {
        mm_oracle_age > Millis::from_stored_units(override_ as u64)
    } else if override_ < 0 {
        let accepted_slots =
            MM_ORACLE_MIN_WRITE_GAP.to_slots_ceil(slot_clock.slot_duration_at(landing_slot));
        mm_oracle_age > slot_clock.elapsed_slot_delta(accepted_slots, landing_slot)
    } else {
        true
    }
}

/// How to handle one auction cross given the vAMM's usability and available DLOB makers.
///
/// A cross is NOT categorically one or the other: `MakerCrosses` sets `has_vamm_cross`
/// independently of the maker orders it collected, so both legs routinely coexist. A gated
/// vAMM must therefore degrade the fill to makers-only — never drop it (the program happily
/// executes the Match steps with `amm_is_available = false`).
#[derive(Debug, PartialEq, Eq)]
enum CrossAction {
    /// send the fill relying on the vAMM (any makers ride along)
    FillWithVamm,
    /// vAMM leg gated but DLOB makers can still fill
    FillMakersOnly,
    /// no fillable counterparty at all
    Skip,
}

impl CrossAction {
    /// stable label for wide-event logging
    fn label(&self) -> &'static str {
        match self {
            CrossAction::FillWithVamm => "fill_with_vamm",
            CrossAction::FillMakersOnly => "makers_only",
            CrossAction::Skip => "skip",
        }
    }
}

fn classify_cross(has_vamm_cross: bool, vamm_usable: bool, has_makers: bool) -> CrossAction {
    if has_vamm_cross && vamm_usable {
        CrossAction::FillWithVamm
    } else if has_makers {
        CrossAction::FillMakersOnly
    } else {
        CrossAction::Skip
    }
}

fn amm_wants_to_jit_make(
    amm: &AMM,
    order_step_size: u64,
    taker_direction: PositionDirection,
) -> bool {
    let amm_wants_to_jit_make = match taker_direction {
        PositionDirection::Long => amm.base_asset_amount_with_amm < -(order_step_size as i128),
        PositionDirection::Short => amm.base_asset_amount_with_amm > order_step_size as i128,
    };
    amm_wants_to_jit_make && amm.amm_jit_intensity > 0
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
        sync_user_accounts(&velocity, &dlob_notifier),
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
    let stats_sync_result = velocity
        .rpc()
        .get_program_accounts_with_config(
            &PROGRAM_ID,
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

pub async fn sync_user_accounts(
    velocity: &VelocityClient,
    dlob_notifier: &DLOBNotifier,
) -> Result<(), solana_rpc_client_api::client_error::Error> {
    let sync_result = velocity
        .rpc()
        .get_program_accounts_with_config(
            &PROGRAM_ID,
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
                dlob_notifier.user_update(pubkey, None, &user, 0);
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
    perp_fill_fallbacks: Option<Arc<DashMap<(Pubkey, u16), ()>>>,
}

impl TxWorker {
    pub fn new(
        velocity: VelocityClient,
        metrics: Arc<Metrics>,
        dry_run: bool,
        txs_in_flight: Option<Arc<DashMap<Pubkey, HashSet<Signature>>>>,
        tx_sig_to_collateral: Option<Arc<DashMap<Signature, (u128, u64)>>>,
        free_collateral_per_subaccount: Option<Arc<DashMap<Pubkey, u128>>>,
        perp_fill_fallbacks: Option<Arc<DashMap<(Pubkey, u16), ()>>>,
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
                        self.send_tx(&rt, tx, simulation_tx, require_fill_event, intent, cu_limit);
                    }
                    TxWork::Confirm { tx, ts: _ } => {
                        self.confirm_tx(&rt, tx);
                    }
                }
            }
        });
        TxSender { tx, velocity }
    }

    fn send_tx(
        &self,
        rt: &Handle,
        signed_tx: VersionedTransaction,
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
            match velocity
                .simulate_tx_with_commitment(
                    simulation_tx,
                    Some(CommitmentConfig::processed()),
                )
                .await
            {
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
                        max_supported_transaction_version: Some(0),
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
                                for (tx_idx, log) in logs.iter().enumerate() {
                                    if let Some(event) = velocity_rs::event_subscriber::try_parse_log(
                                        log.as_str(),
                                        &sig,
                                        tx_idx,
                                    ) {
                                        if let VelocityEvent::OrderFill { ..} = event
                                        {
                                            actual_fills += 1;
                                        } else if let VelocityEvent::OrderTrigger { .. } = event {
                                            triggered = true;
                                            metrics.trigger_actual.inc();
                                        } else if log.as_str().contains("exceeded CUs meter") {
                                            metrics
                                            .tx_failed
                                            .with_label_values(&[
                                                intent_label,
                                                "insufficient_cus",
                                            ])
                                            .inc();
                                        }
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
                                // Log program logs from failed liquidation txs
                                if intent.is_liquidation() {
                                    let logs: Option<Vec<String>> = meta.log_messages.clone().into();
                                    if let Some(logs) = logs {
                                        for log_line in &logs {
                                            if log_line.contains("Error") || log_line.contains("error") || log_line.contains("failed") || log_line.contains("Program log:") {
                                                log::warn!(target: TARGET, "  tx log: {}", log_line);
                                            }
                                        }
                                    }
                                }
                                // tx failed with error
                                metrics
                                    .tx_failed
                                    .with_label_values(&[
                                        intent_label,
                                        &format!("{:?}", err),
                                    ])
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

/// Emit a wide structured event (one JSON line, log target `tx_event`) for every auction
/// cross evaluated, capturing the routing decision AND each vAMM gate input that produced it.
///
/// The per-tx event (`event: "tx"`) only exists for crosses that result in a send; this event
/// is the debugging trail for the ones that don't — why a cross was degraded to makers-only or
/// skipped (drawdown? staleness? order too fresh? no JIT appetite? vAMM fillable too small?).
/// Correlate with the tx event on (market, order_id).
#[allow(clippy::too_many_arguments)]
fn emit_cross_decision_event(
    market_index: u16,
    taker: &Pubkey,
    order_id: u32,
    slot: u64,
    action: &CrossAction,
    has_vamm_cross: bool,
    oracle_stale_for_amm: bool,
    oracle_delay: i64,
    drawdown: bool,
    order_low_risk: bool,
    amm_wants_to_jit_make: bool,
    mm_stale_immediate: bool,
    vamm_fillable: Option<u64>,
    n_makers: usize,
) {
    let event = serde_json::json!({
        "event": "cross_decision",
        "market": market_index,
        "taker": taker.to_string(),
        "order_id": order_id,
        "slot": slot,
        "action": action.label(),
        "has_vamm_cross": has_vamm_cross,
        "oracle_stale_for_amm": oracle_stale_for_amm,
        "oracle_delay": oracle_delay,
        "drawdown": drawdown,
        "order_low_risk": order_low_risk,
        "amm_wants_to_jit_make": amm_wants_to_jit_make,
        "mm_stale_immediate": mm_stale_immediate,
        "vamm_fillable": vamm_fillable,
        "n_makers": n_makers,
    });
    log::info!(target: "tx_event", "{event}");
}

/// Emit a wide structured event (one JSON line, log target `tx_event`) for each
/// resting-order-vs-vAMM fill candidate that reaches a terminal decision
/// (`action`: "sent" / "skip_order_too_new" / "skip_too_small"), with the gate inputs.
///
/// Rate-limited re-attempts are deliberately NOT emitted (one per slot for the whole limiter
/// window would drown the signal); the send attempt they throttle already produced a "sent"
/// decision plus a terminal `event: "tx"` (intent `vamm_taker`). Correlate on
/// (market, order_id).
#[allow(clippy::too_many_arguments)]
fn emit_vamm_taker_decision_event(
    market_index: u16,
    user: &Pubkey,
    order_id: u32,
    slot: u64,
    action: &str,
    post_only: bool,
    order_slot: u64,
    oracle_stale_for_amm: bool,
    oracle_delay: i64,
    limit_price: u64,
    fillable: Option<u64>,
    size_threshold: Option<u64>,
) {
    let event = serde_json::json!({
        "event": "vamm_taker_decision",
        "market": market_index,
        "user": user.to_string(),
        "order_id": order_id,
        "slot": slot,
        "action": action,
        "post_only": post_only,
        "order_slot": order_slot,
        "oracle_stale_for_amm": oracle_stale_for_amm,
        "oracle_delay": oracle_delay,
        "limit_price": limit_price,
        "fillable": fillable,
        "size_threshold": size_threshold,
    });
    log::info!(target: "tx_event", "{event}");
}

/// Emit a wide structured event (one JSON line, log target `tx_event`) for each uncross leg
/// that reaches a decision (`action`: "sent" / "skip_taker_post_only" / "skip_no_makers").
///
/// Carries the taker order and the crossing counterparty orders considered as makers, with
/// post-only/kind decoded, so a landed-but-`no_fills` `limit_uncross` tx (correlate on
/// (market, taker_order_id, slot=sent_slot)) can be analyzed without replaying the book:
/// e.g. a non-post-only counterparty can never match as a maker, an auction-phase taker may
/// not be matchable at the maker's price yet, etc. `n_maker_accounts` is how many maker user
/// accounts were actually attached to the tx (candidates missing from the account cache are
/// dropped).
fn emit_uncross_attempt_event(
    market_index: u16,
    slot: u64,
    action: &str,
    taker: &L3Order,
    maker_candidates: &[&L3Order],
    n_maker_accounts: usize,
) {
    let event = serde_json::json!({
        "event": "uncross_attempt",
        "market": market_index,
        "slot": slot,
        "action": action,
        "taker": taker.user.to_string(),
        "taker_order_id": taker.order_id,
        "taker_kind": format!("{:?}", taker.kind),
        "taker_price": taker.price,
        "taker_size": taker.size,
        "taker_post_only": taker.is_post_only(),
        "taker_is_long": taker.is_long(),
        "makers": maker_candidates
            .iter()
            .map(|m| {
                serde_json::json!({
                    "user": m.user.to_string(),
                    "order_id": m.order_id,
                    "kind": format!("{:?}", m.kind),
                    "price": m.price,
                    "size": m.size,
                    "post_only": m.is_post_only(),
                    "eligible": matches!(m.kind, OrderKind::Limit | OrderKind::FloatingLimit),
                })
            })
            .collect::<Vec<_>>(),
        "n_maker_accounts": n_maker_accounts,
    });
    log::info!(target: "tx_event", "{event}");
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
    fallbacks: Option<&Arc<DashMap<(Pubkey, u16), ()>>>,
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
        fallbacks.insert((*liquidatee, *market_index), ());
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
    logs.into_iter().flatten().enumerate().any(|(tx_idx, log)| {
        velocity_rs::event_subscriber::try_parse_log(log, "simulation", tx_idx)
            .is_some_and(|event| is_expected_fill_event(&event, intent))
    })
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

#[cfg(test)]
mod tests {
    use {
        super::{
            build_fill_tx, classify_cross, is_expected_fill_event, is_revert_fill_error,
            mm_oracle_stale_for_amm_immediate, order_dedup_key, record_perp_fill_fallback,
            vamm_can_fill_taker, CrossAction, Pubkey, TxIntent, VelocityEvent,
        },
        solana_sdk::{instruction::InstructionError, transaction::TransactionError},
        std::borrow::Cow,
        velocity_rs::{
            constants::ProgramData,
            types::accounts::{PerpMarket, SpotMarket, State, User},
            velocity_idl::types::MarketType as EventMarketType,
            TransactionBuilder,
        },
    };

    // The unset (override < 0) MM-sourced immediate threshold compares the
    // wall clock MM-oracle age against MM_ORACLE_MIN_WRITE_GAP, matching the
    // program's `oracle_validity`. The age integrates per slot duration
    // regime, so the effective slot count scales with the clock.
    #[test]
    fn unset_mm_immediate_threshold_scales_per_gate() {
        use velocity_rs::program::math::time::SlotClock;
        let mut market = PerpMarket::default();
        market.oracle_slot_delay_override = -1; // unset -> source-aware fallback
        market.market_stats.mm_oracle_slot = 1_000;
        // MM_ORACLE_MIN_WRITE_GAP = 800ms: 2 slots at 400ms, 4 at 200ms
        for (clock, threshold) in [
            (SlotClock::baseline(), 2u64),
            (SlotClock::from_state_fields([1, 0, 0, 0], 0, 0, 0), 3),
            (SlotClock::from_state_fields([1, 1, 1, 1], 0, 0, 0), 4),
        ] {
            // age exactly at the write gap is NOT stale (`age > gap`)
            let at = market.market_stats.mm_oracle_slot + threshold;
            assert!(
                !mm_oracle_stale_for_amm_immediate(&market, at, clock),
                "age == write gap should be fresh"
            );
            // one slot past is stale
            assert!(
                mm_oracle_stale_for_amm_immediate(&market, at + 1, clock),
                "age past write gap should be stale"
            );
        }
    }

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
    fn fill_tx_retains_revert_only_when_requested() {
        let program_data = ProgramData::new(
            vec![SpotMarket::default()],
            vec![PerpMarket::default()],
            vec![],
            State::default(),
        );

        let builder = TransactionBuilder::new(
            &program_data,
            Pubkey::new_unique(),
            Cow::Owned(User::default()),
            false,
        );
        let (send_tx, simulation_tx) = build_fill_tx(builder, false);
        assert_eq!(send_tx.instructions().len(), 0);
        assert_eq!(
            simulation_tx
                .expect("ordinary fills simulate with RevertFill")
                .instructions()
                .len(),
            1
        );

        let builder = TransactionBuilder::new(
            &program_data,
            Pubkey::new_unique(),
            Cow::Owned(User::default()),
            false,
        );
        let (send_tx, simulation_tx) = build_fill_tx(builder, true);
        assert_eq!(send_tx.instructions().len(), 1);
        assert!(
            simulation_tx.is_none(),
            "Pyth-update fills must retain RevertFill when sent"
        );
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

    #[test]
    fn vamm_gated_cross_degrades_to_makers_instead_of_skipping() {
        // Regression: a cross carrying both a vAMM leg and DLOB maker orders used to be
        // skipped entirely when the vAMM was gated (drawdown/staleness/timing), dropping
        // perfectly fillable maker matches. It must degrade to a makers-only fill.
        assert_eq!(
            classify_cross(true, false, true),
            CrossAction::FillMakersOnly
        );
        // vAMM gated and no makers: nothing to fill against
        assert_eq!(classify_cross(true, false, false), CrossAction::Skip);
        // vAMM usable: send the fill relying on it, with or without makers
        assert_eq!(classify_cross(true, true, false), CrossAction::FillWithVamm);
        assert_eq!(classify_cross(true, true, true), CrossAction::FillWithVamm);
        // no vAMM cross at all: plain maker fill or skip
        assert_eq!(
            classify_cross(false, false, true),
            CrossAction::FillMakersOnly
        );
        assert_eq!(classify_cross(false, false, false), CrossAction::Skip);
    }

    #[test]
    fn vamm_can_fill_taker_mirrors_program_gates() {
        // Regression: the old `is_vamm_inactive` closure computed
        // `drawdown && amm_wants_to_jit_make` — drawdown with no JIT appetite passed as
        // "active", and JIT appetite was treated as a disqualifier rather than the
        // requirement it is for non-low-risk orders.
        // args: (drawdown, oracle_stale_for_amm, order_low_risk, wants_jit, mm_stale_immediate)
        // drawdown alone hard-blocks (amm_fill_gates_ok), regardless of everything else
        assert!(!vamm_can_fill_taker(true, false, true, true, false));
        assert!(!vamm_can_fill_taker(true, false, false, false, false));
        // low-risk oracle staleness hard-blocks
        assert!(!vamm_can_fill_taker(false, true, true, true, false));
        // low-risk order fills without JIT appetite (amm_fill_timing_ok fast path), and is
        // NOT subject to the immediate-staleness gate
        assert!(vamm_can_fill_taker(false, false, true, false, true));
        // non-low-risk order requires the AMM to want to JIT-make
        assert!(vamm_can_fill_taker(false, false, false, true, false));
        assert!(!vamm_can_fill_taker(false, false, false, false, false));
        // Regression (vamm_taker no_fill spam): a non-low-risk JIT cross with the MM oracle
        // stale for immediate fills must NOT send — the program's FillOrderAmmImmediate gate
        // rejects it on-chain even though the low-risk staleness looks fine.
        assert!(!vamm_can_fill_taker(false, false, false, true, true));
    }

    #[test]
    fn order_dedup_key_distinguishes_users_with_same_order_id() {
        let a = Pubkey::new_from_array([1u8; 32]);
        let b = Pubkey::new_from_array([2u8; 32]);
        // stable for the same (user, order_id)
        assert_eq!(order_dedup_key(&a, 3), order_dedup_key(&a, 3));
        // order_id is per-user: the same id under different users must NOT collide, otherwise
        // one user's trigger would suppress another's (regression guard for the H1 bug).
        assert_ne!(order_dedup_key(&a, 3), order_dedup_key(&b, 3));
        // different order_id under the same user must differ too
        assert_ne!(order_dedup_key(&a, 3), order_dedup_key(&a, 4));
    }
}
