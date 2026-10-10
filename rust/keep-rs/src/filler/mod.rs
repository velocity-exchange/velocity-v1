//! Perp filler bot
//!
//! The filler watches the DLOB, the gRPC slot feed, the pyth-lazer feed and the swift order
//! stream. On every slot it builds a `MarketView` per market, the market as the program will
//! see it when a tx sent now lands, and runs the passes in order: auction fills, standalone
//! triggers, limit uncrosses and resting-order fills against the AMM. Each pass plans its txs on
//! the view, asking the program's own gate and sizing functions about the AMM leg, then sends
//! them through the tx worker, which simulates every fill before it goes out. Swift orders are
//! filled on arrival when they cross, placed onchain when they do not, and held when their
//! message slot is still ahead of the chain.

use {
    crate::{
        common::{
            keeper::Keeper,
            metrics::{FeedHealth, Metrics},
            oracle::{pyth_update_is_fresh, ExchangeState, PythPriceUpdate},
            tx::{OrderSlotLimiter, TxIntent, TxWorker},
        },
        Config, UseMarkets,
    },
    futures_util::StreamExt,
    pyth_lazer_protocol::router::TimestampUs,
    std::{
        borrow::Cow,
        collections::{BTreeMap, HashSet},
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    },
    stream::{setup_grpc, MAX_CONSECUTIVE_ORACLE_MISSES},
    swift::{
        evaluate_swift_crosses, fill_swift_order, place_swift_order_onchain, swift_order_expired,
        SwiftDecision, SwiftFeed,
    },
    velocity_rs::{
        dlob::{OrderKind, DLOB},
        priority_fee_subscriber::PriorityFeeSubscriber,
        swift_order_subscriber::SignedOrderInfo,
        types::{
            accounts::{State as IdlState, User},
            MarketId, MarketStatus, MarketType,
        },
        Pubkey, TransactionBuilder, VelocityClient,
    },
};

mod auction;
mod market;
mod passes;
mod stream;
mod swift;

pub(crate) const TARGET: &str = "filler";

/// Wall-clock age past which a cached pyth price is not posted. A frozen feed leaves the cache
/// holding an arbitrarily old price with no signal in the update itself, so every read checks
/// the timestamp. Strictly tighter than the program's `PYTH_LAZER_MAX_STALENESS_SECONDS` (15s),
/// so the bot stops trusting a price before the program would reject it.
const PYTH_PRICE_MAX_AGE_US: u64 = 10_000_000;

/// The gRPC slot feed delivers ~2.5 slots a second. A feed that dies silently (half-open, no
/// error and no `None`) leaves `slot_rx.recv()` pending forever while the process reports
/// healthy, so this much silence exits the process for a restart with fresh subscriptions.
const SLOT_FEED_STALE_LIMIT: Duration = Duration::from_secs(60);

/// The inputs every pass reads for one slot.
pub(super) struct SlotTick {
    pub slot: u64,
    /// A tx sent now lands about one slot later (per the `tx_event` `latency_slots` telemetry).
    pub landing_slot: u64,
    pub priority_fee: u64,
    pub unix_now: i64,
    pub exchange: ExchangeState,
}

/// The shared keeper handles plus the subaccount the filler sends from and its CU limits.
#[derive(Clone)]
pub(super) struct Filler {
    pub keeper: Keeper,
    pub subaccount: Pubkey,
    pub fill_cu_limit: u32,
    pub trigger_cu_limit: u32,
    pub swift_cu_limit: u32,
}

impl Filler {
    /// The filler's own account. Missing from the cache it is a lost subscription or a
    /// misconfiguration that would silently no-op every fill, so this panics for a restart.
    fn account(&self) -> User {
        self.keeper
            .cached_user(&self.subaccount)
            .expect("filler subaccount in cache; restart")
    }

    fn tx_builder<'a>(
        &self,
        account: &'a User,
        priority_fee: u64,
        cu_limit: u32,
    ) -> TransactionBuilder<'a> {
        self.keeper.tx_builder(
            self.subaccount,
            Cow::Borrowed(account),
            priority_fee,
            cu_limit,
        )
    }

    /// Trigger one order without filling it. The triggered order then rests, and a later slot's
    /// auction pass fills it.
    ///
    /// The trigger instruction reads the exchange oracle, so a trigger decided on a posted
    /// pyth-lazer price must post `oracle_update` first, or it would read the older chain price
    /// and fail.
    async fn send_trigger(
        &self,
        priority_fee: u64,
        market_index: u16,
        taker: Pubkey,
        order_id: u32,
        slot: u64,
        oracle_update: Option<&PythPriceUpdate>,
    ) {
        let filler_account = self.account();
        let Some(taker_account) = self.keeper.cached_user(&taker) else {
            log::warn!(target: TARGET, "trigger: taker account {taker} not in cache, skipping");
            return;
        };

        log::info!(target: TARGET, "attempting standalone trigger: order_id={order_id}, taker={taker}");
        // the post's signature verification needs the fill limit's headroom
        let cu_limit = if oracle_update.is_some() {
            self.fill_cu_limit
        } else {
            self.trigger_cu_limit
        };
        let mut tx_builder = self.tx_builder(&filler_account, priority_fee, cu_limit);
        if let Some(update) = oracle_update {
            tx_builder =
                tx_builder.post_pyth_lazer_oracle_update(&[update.feed_id], &update.message);
        }
        let tx = tx_builder
            .trigger_order(
                taker,
                &taker_account,
                order_id,
                (market_index, MarketType::Perp),
            )
            .build();

        self.keeper
            .tx
            .send_tx(
                tx,
                TxIntent::Trigger {
                    market_index,
                    order_id,
                    taker_user: taker,
                    slot,
                },
                cu_limit as u64,
            )
            .await;
    }
}

/// What the run loop wakes up for.
// Built once per loop iteration and matched right away; boxing the swift order would add an
// allocation per order.
#[allow(clippy::large_enum_variant)]
enum LoopEvent {
    Swift(Option<SignedOrderInfo>),
    SwiftReconnect,
    Slot(Option<u64>),
    Pyth(Option<PythPriceUpdate>),
    Watchdog,
}

/// The run loop's state between events.
struct SlotState {
    slot: u64,
    /// False until the first slot arrives when the startup RPC slot lookup failed.
    slot_is_known: bool,
    /// The last `State` read. A transient cache miss keeps it rather than falling back to
    /// defaults mid-run.
    exchange: ExchangeState,
    limiter: OrderSlotLimiter<40>,
    pyth_prices: BTreeMap<u16, PythPriceUpdate>,
    /// Consecutive slots each market was missing from the cache.
    cache_misses: BTreeMap<u16, u32>,
    /// Last logged oracle staleness per market. Staleness is logged on transition only, so a
    /// stale oracle shows as two edges, not a line per slot.
    oracle_stale: BTreeMap<u16, bool>,
    /// Last logged pyth price staleness per market, logged the same way.
    pyth_stale: BTreeMap<u16, bool>,
    /// Scratch buffer for each market's triggerable orders, reused across slots.
    triggerable: Vec<(Pubkey, u32)>,
    last_slot_update: std::time::Instant,
    /// On contention between a buffered slot and a ready swift order, alternate which one the
    /// biased select takes, so neither a busy swift feed nor a slot backlog starves the other.
    prefer_slot_on_contention: bool,
}

impl SlotState {
    fn known_slot(&self) -> Option<u64> {
        self.slot_is_known.then_some(self.slot)
    }
}

pub struct FillerBot {
    filler: Filler,
    dlob: &'static DLOB,
    market_ids: Vec<MarketId>,
    priority_fees: Arc<PriorityFeeSubscriber>,
    feed_health: Arc<FeedHealth>,
    slot_rx: tokio::sync::mpsc::Receiver<u64>,
    pyth_feed: tokio::sync::mpsc::Receiver<PythPriceUpdate>,
    /// Keeps the pyth channel open when the feed is disabled, so its arm never fires.
    _pyth_feed_disabled: Option<tokio::sync::mpsc::Sender<PythPriceUpdate>>,
    swift: SwiftFeed,
    state: SlotState,
}

impl FillerBot {
    pub async fn new(
        config: Config,
        velocity: VelocityClient,
        metrics: Arc<Metrics>,
        feed_health: Arc<FeedHealth>,
    ) -> Self {
        let velocity: &'static VelocityClient = Box::leak(Box::new(velocity));
        let dlob: &'static DLOB = Box::leak(Box::new(DLOB::default()));
        let tx = TxWorker::new(velocity.clone(), metrics.clone(), config.dry, None, None)
            .run(tokio::runtime::Handle::current());

        let market_ids = fillable_market_ids(velocity, &config);
        let market_pubkeys: Vec<Pubkey> = market_ids
            .iter()
            .map(|market| {
                velocity
                    .program_data()
                    .perp_market_config_by_index(market.index())
                    .unwrap()
                    .pubkey
            })
            .collect();
        let priority_fees =
            PriorityFeeSubscriber::new(velocity.rpc().url(), &market_pubkeys).subscribe();
        let subaccount = velocity.wallet.sub_account(config.sub_account_id);

        let swift = SwiftFeed::subscribe(velocity, market_ids.clone(), &feed_health).await;

        velocity.subscribe_blockhashes().await.expect("subscribed");
        let slot_rx = setup_grpc(
            velocity.clone(),
            dlob,
            tx.clone(),
            market_ids.clone(),
            subaccount,
        )
        .await;
        // start the liveness clock at subscription time, so a feed that never delivers a single
        // slot still trips the health check
        feed_health.touch_slot();
        log::info!(target: TARGET, "subscribed gRPC");

        let (pyth_feed, pyth_feed_disabled) = if config.no_pyth {
            log::info!(target: TARGET, "pyth price feed disabled");
            let (sender, receiver) = tokio::sync::mpsc::channel(1);
            (receiver, Some(sender))
        } else {
            let token = std::env::var("PYTH_LAZER_TOKEN").expect("pyth access token");
            let client = pyth_lazer_client::LazerClient::new(
                "wss://pyth-lazer.dourolabs.app/v1/stream",
                token.as_str(),
            )
            .expect("pyth price feed connects");
            let feed = crate::common::oracle::subscribe_price_feeds(client, &market_ids, &[], &[]);
            // start the liveness clock at subscription time, so a feed that never delivers a
            // single update still trips the health check
            feed_health.touch_pyth();
            log::info!(target: TARGET, "subscribed pyth price feeds");
            (feed, None)
        };

        // seed with the real chain slot, so a bot started after a slot duration switch reflects
        // it immediately
        let startup_slot = velocity.get_slot().await;
        let exchange = ExchangeState::load(velocity).unwrap_or_else(|| {
            log::warn!(target: TARGET, "State account not cached at startup, using defaults until it arrives");
            ExchangeState::from_idl(&IdlState::default())
        });
        dlob.update_slot_clock(exchange.slot_clock);

        FillerBot {
            filler: Filler {
                keeper: Keeper {
                    velocity,
                    tx,
                    metrics,
                },
                subaccount,
                fill_cu_limit: config.fill_cu_limit,
                trigger_cu_limit: config.trigger_cu_limit,
                swift_cu_limit: config.swift_cu_limit,
            },
            dlob,
            market_ids,
            priority_fees,
            feed_health,
            slot_rx,
            pyth_feed,
            _pyth_feed_disabled: pyth_feed_disabled,
            swift,
            state: SlotState {
                slot: startup_slot.unwrap_or(0),
                slot_is_known: startup_slot.is_some(),
                exchange,
                limiter: OrderSlotLimiter::new(),
                pyth_prices: BTreeMap::new(),
                cache_misses: BTreeMap::new(),
                oracle_stale: BTreeMap::new(),
                pyth_stale: BTreeMap::new(),
                triggerable: Vec::new(),
                last_slot_update: std::time::Instant::now(),
                prefer_slot_on_contention: true,
            },
        }
    }

    pub async fn run(mut self) {
        let mut watchdog = tokio::time::interval(Duration::from_secs(15));
        watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            // Matured deferrals rejoin the arrival path at the top of the iteration, so no
            // select arm can skip them.
            self.swift.release_matured(
                self.state.known_slot(),
                self.state.exchange.slot_clock,
                &self.filler.keeper.metrics,
            );

            // non-blocking: consuming here would skip the slot's fill work
            let slot_update_pending = !self.slot_rx.is_empty();
            let poll_swift = swift::should_poll_swift(
                self.swift.live,
                slot_update_pending,
                self.state.prefer_slot_on_contention,
            );
            let swift_reconnect_at = self.swift.reconnect_at;
            let swift_live = self.swift.live;

            let event = tokio::select! {
                biased;
                order = self.swift.stream.next(), if poll_swift => LoopEvent::Swift(order),
                _ = tokio::time::sleep_until(swift_reconnect_at), if !swift_live => LoopEvent::SwiftReconnect,
                slot = self.slot_rx.recv() => LoopEvent::Slot(slot),
                update = self.pyth_feed.recv() => LoopEvent::Pyth(update),
                _ = watchdog.tick() => LoopEvent::Watchdog,
            };

            match event {
                LoopEvent::Swift(order) => {
                    self.state.prefer_slot_on_contention = true;
                    self.swift.on_message(order, &self.feed_health);
                }
                LoopEvent::SwiftReconnect => {
                    self.swift
                        .reconnect(self.filler.keeper.velocity, &self.feed_health)
                        .await;
                }
                LoopEvent::Slot(None) => {
                    log::error!(target: TARGET, "slot subscriber failed");
                    break;
                }
                LoopEvent::Slot(Some(slot)) => self.on_slot(slot).await,
                LoopEvent::Pyth(Some(update)) => {
                    self.feed_health.touch_pyth();
                    self.state.pyth_prices.insert(update.market_id, update);
                }
                LoopEvent::Pyth(None) => {
                    log::error!(target: TARGET, "pyth price feed disconnected, shutting down");
                    break;
                }
                LoopEvent::Watchdog => self.check_feeds().await,
            }

            self.process_swift_orders().await;
        }

        self.filler.keeper.velocity.grpc_unsubscribe();
        log::info!(target: TARGET, "filler shutting down...");
    }

    /// Run every market's fill passes for a new slot.
    async fn on_slot(&mut self, slot: u64) {
        self.state.slot = slot;
        self.state.slot_is_known = true;
        self.state.prefer_slot_on_contention = false;
        self.state.last_slot_update = std::time::Instant::now();
        self.feed_health.touch_slot();
        log::trace!(target: TARGET, "got slot update: {slot}");

        // one `State` read per slot, before any market decision, so a slot's decisions never
        // split across two clocks or thresholds
        if let Some(exchange) = ExchangeState::load(self.filler.keeper.velocity) {
            self.state.exchange = exchange;
            self.dlob.update_slot_clock(exchange.slot_clock);
        }

        let started = SystemTime::now();
        let tick = SlotTick {
            slot,
            landing_slot: slot + 1,
            // the slot parity makes consecutive resubmissions of one tx hash differently
            priority_fee: self.priority_fees.priority_fee_nth(0.5) + slot % 2,
            unix_now: started.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64,
            exchange: self.state.exchange,
        };

        for market in self.market_ids.clone() {
            self.fill_market(&tick, market.index()).await;
        }

        let elapsed_ms = SystemTime::now()
            .duration_since(started)
            .unwrap_or_default()
            .as_millis();
        log::trace!(target: TARGET, "checked fills at {slot}: {elapsed_ms}ms");
    }

    /// The fill passes for one market in one slot.
    async fn fill_market(&mut self, tick: &SlotTick, market_index: u16) {
        let velocity = self.filler.keeper.velocity;
        let pyth_update = self.fresh_pyth_update(market_index);

        // Skip the market this slot on a transient cache miss, and let the next slot retry. A
        // persistent miss panics (the main loop exits, and the process restarts) rather than
        // silently never filling the market again.
        let view = match market::MarketView::load(velocity, tick, market_index, pyth_update) {
            Ok(view) => {
                self.state.cache_misses.insert(market_index, 0);
                view
            }
            Err(err) => {
                let count = self.state.cache_misses.entry(market_index).or_insert(0);
                *count += 1;
                log::warn!(target: TARGET, "no perp market/oracle for market {market_index} ({count} consecutive): {err:?}, skipping fills this slot");
                assert!(
                    *count < MAX_CONSECUTIVE_ORACLE_MISSES,
                    "market {market_index} unavailable for {count} consecutive slots"
                );
                return;
            }
        };
        self.log_oracle_staleness(&view);

        let mut auction_crosses = self.dlob.find_crosses_for_auctions(
            market_index,
            MarketType::Perp,
            tick.slot,
            // an auction fill posts the update, so it prices at the posted safe oracle
            view.posted.price(),
            Some(&view.posted.quote_market),
            view.posted.trigger_price,
            None,
        );
        // order_id is a per-user counter, so the limiter keys on the full (user, order_id)
        // identity: a bare order_id would suppress another user's fill
        let limiter = &mut self.state.limiter;
        auction_crosses.crosses.retain(|(order, _)| {
            limiter.allow_event(tick.slot, order_dedup_key(&order.user, order.order_id))
        });

        // Resting orders the AMM quote crosses (at most one per side) come out of the same
        // find pass, with the top makers so the fill can route to a better-priced user maker.
        // Take them before the auction pass consumes the struct. Their fills post nothing, so
        // when the posted view differs from the chain view they are found again on the chain
        // view.
        let mut amm_crossed = [
            auction_crosses.take_vamm_crossed_bid(),
            auction_crosses.take_vamm_crossed_ask(),
        ];
        let mut top_makers = (
            auction_crosses.top_maker_asks.to_vec(),
            auction_crosses.top_maker_bids.to_vec(),
        );
        if !view.posted_is_chain {
            let mut chain_crosses = self.dlob.find_crosses_for_auctions(
                market_index,
                MarketType::Perp,
                tick.slot,
                view.chain.price(),
                Some(&view.chain.quote_market),
                view.chain.trigger_price,
                None,
            );
            amm_crossed = [
                chain_crosses.take_vamm_crossed_bid(),
                chain_crosses.take_vamm_crossed_ask(),
            ];
            top_makers = (
                chain_crosses.top_maker_asks.to_vec(),
                chain_crosses.top_maker_bids.to_vec(),
            );
        }

        // Crossing trigger orders are triggered and filled in one tx by the auction pass, so
        // the standalone trigger pass leaves them out.
        let crossing_triggers: HashSet<(Pubkey, u32)> = auction_crosses
            .crosses
            .iter()
            .filter(|(order, _)| {
                matches!(
                    order.kind,
                    OrderKind::TriggerMarket | OrderKind::TriggerLimit
                )
            })
            .map(|(order, _)| (order.user, order.order_id))
            .collect();

        if !auction_crosses.crosses.is_empty() {
            log::info!(target: TARGET, "found auction crosses. market={market_index} oracle={} delay={} amm={} trigger={} stale_for_amm={} crosses={auction_crosses:?}", view.posted.price(), view.chain.oracle.safe.delay, view.market.market_stats.mm_oracle_price, view.posted.trigger_price, view.oracle_stale_for_amm);
            auction::fill_auction_crosses(&self.filler, tick, &view, auction_crosses).await;
        }

        self.dlob.find_triggerable_orders(
            market_index,
            MarketType::Perp,
            // a standalone trigger tx posts nothing, so it reads the chain exchange oracle
            view.chain.trigger_price,
            &mut self.state.triggerable,
        );
        passes::trigger_resting_orders(
            &self.filler,
            tick,
            &view,
            &self.state.triggerable,
            &crossing_triggers,
            &mut self.state.limiter,
        )
        .await;
        self.state.triggerable.clear();

        // every other slot, to bound the uncross tx rate
        if tick.slot.is_multiple_of(2) {
            // an uncross posts nothing, so it prices at the chain view
            if let Some(crosses) = self.dlob.find_crossing_region(
                view.chain.price(),
                market_index,
                MarketType::Perp,
                Some(&view.chain.quote_market),
            ) {
                log::info!(target: TARGET, "found limit crosses (market={market_index}) oracle={} delay={}, top bid: {:?}, top ask: {:?}", view.chain.price(), view.chain.oracle.safe.delay, crosses.crossing_bids.first(), crosses.crossing_asks.first());
                passes::uncross_limits(&self.filler, tick, &view, crosses).await;
            }
        }

        // A lone resting limit that comes to cross the AMM after placement (the price moved, or
        // a stale oracle recovered) matches neither the auction pass (no live auction) nor the
        // uncross pass (needs both book sides).
        if amm_crossed.iter().any(Option::is_some) {
            passes::fill_amm_takers(
                &self.filler,
                tick,
                &view,
                amm_crossed,
                top_makers,
                &mut self.state.limiter,
            )
            .await;
        }
    }

    /// The market's cached pyth price when it is fresh. Updates the age metric and logs
    /// staleness transitions.
    fn fresh_pyth_update(&mut self, market_index: u16) -> Option<PythPriceUpdate> {
        let update = self.state.pyth_prices.get(&market_index)?;
        // capture the clock per market: earlier markets in the slot await fill txs, so a
        // slot-start timestamp can be seconds behind
        let now = TimestampUs::now();
        let age_ms = now.saturating_us_since(update.ts) / 1_000;
        let stale = !pyth_update_is_fresh(update.ts, now, PYTH_PRICE_MAX_AGE_US);

        let previous = self.state.pyth_stale.insert(market_index, stale);
        if previous != Some(stale) {
            if stale {
                log::warn!(target: TARGET, "pyth price went stale market={market_index} age_ms={age_ms} falling back to chain oracle");
            } else if previous.is_some() {
                log::info!(target: TARGET, "pyth price recovered market={market_index} age_ms={age_ms}");
            }
        }
        self.filler
            .keeper
            .metrics
            .pyth_price_age_ms
            .with_label_values(&[&market_index.to_string()])
            .set(age_ms as i64);

        // a same-price post still refreshes the oracle slot, which the immediate AMM leg needs
        (!stale).then(|| update.clone())
    }

    fn log_oracle_staleness(&mut self, view: &market::MarketView) {
        let stale = view.oracle_stale_for_amm;
        let previous = self.state.oracle_stale.insert(view.market_index, stale);
        if previous == Some(stale) {
            return;
        }
        if stale {
            log::warn!(target: TARGET, "oracle went stale market={} delay={} oracle={} amm={}", view.market_index, view.chain.oracle.safe.delay, view.chain.oracle.safe.price, view.market.market_stats.mm_oracle_price);
        } else if previous.is_some() {
            log::info!(target: TARGET, "oracle recovered market={} delay={}", view.market_index, view.chain.oracle.safe.delay);
        }
    }

    /// Exit on a dead slot feed, and resubscribe a swift feed that has gone quiet.
    async fn check_feeds(&mut self) {
        let silence = self.state.last_slot_update.elapsed();
        if silence > SLOT_FEED_STALE_LIMIT {
            log::error!(target: TARGET, "no slot updates for {}s: gRPC slot feed is dead, exiting for supervisor restart", silence.as_secs());
            std::process::exit(1);
        }
        self.swift
            .resubscribe_if_quiet(self.filler.keeper.velocity, &self.feed_health)
            .await;
    }

    /// Fill or place each swift order whose message slot has arrived.
    async fn process_swift_orders(&mut self) {
        let slot = self.state.slot;
        let exchange = self.state.exchange;
        let ready = self
            .swift
            .take_ready(self.state.known_slot(), exchange.slot_clock);
        if ready.is_empty() {
            return;
        }

        let velocity = self.filler.keeper.velocity;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let tick = SlotTick {
            slot,
            landing_slot: slot + 1,
            priority_fee: self.priority_fees.priority_fee_nth(0.6),
            unix_now: now.as_secs() as i64,
            exchange,
        };

        for signed_order in ready {
            let order_params = signed_order.order_params();
            // A held order can age out while the slot feed stalls, and a new order can already
            // be late on arrival, so check before either path spends a tx.
            if swift_order_expired(
                signed_order.slot(),
                order_params.auction_duration.unwrap_or(0),
                order_params.max_ts.unwrap_or(0),
                slot,
                tick.unix_now,
                exchange.slot_clock,
            ) {
                log::info!(target: TARGET, "swift order expired before processing, dropping. uuid={}", signed_order.order_uuid_str());
                self.filler.keeper.metrics.swift_place_skipped.inc();
                continue;
            }

            let market_index = order_params.market_index;
            log::info!(target: TARGET, "new swift order. uuid={}, market={market_index}", signed_order.order_uuid_str());
            log::debug!(target: TARGET, "details: {signed_order:?}");

            // a transient cache miss drops this order and keeps the feed flowing
            let view = match market::MarketView::load(velocity, &tick, market_index, None) {
                Ok(view) => view,
                Err(err) => {
                    log::warn!(target: TARGET, "no perp market/oracle for market {market_index}: {err:?}, skipping swift order. uuid={}", signed_order.order_uuid_str());
                    continue;
                }
            };

            match evaluate_swift_crosses(velocity, self.dlob, &tick, &view, &signed_order) {
                SwiftDecision::Fillable(crosses) => {
                    log::info!(target: TARGET, "found resting cross. market={market_index} oracle={} delay={} crosses={crosses:?}", view.chain.oracle.safe.price, view.chain.oracle.safe.delay);
                    fill_swift_order(&self.filler, tick.priority_fee, signed_order, crosses).await;
                }
                SwiftDecision::NotFillable(reason) => {
                    // Well-formed but not marketable yet: place it onchain, so it rests and
                    // the per-slot passes fill it while it stays live.
                    log::info!(target: TARGET, "swift order not fillable yet ({reason}), placing onchain. uuid={}", signed_order.order_uuid_str());
                    place_swift_order_onchain(&self.filler, tick.priority_fee, signed_order, slot)
                        .await;
                    self.filler.keeper.metrics.swift_placed.inc();
                }
                // malformed or unsupported, already logged by `evaluate_swift_crosses`
                SwiftDecision::Drop => {}
            }
        }
    }
}

/// The configured perp markets, without bet markets and markets still initializing.
fn fillable_market_ids(velocity: &VelocityClient, config: &Config) -> Vec<MarketId> {
    let mut market_ids = match config.use_markets() {
        UseMarkets::All => velocity.get_all_perp_market_ids(),
        UseMarkets::Subset(markets) => markets,
    };
    market_ids.retain(|market| {
        let market_config = velocity
            .program_data()
            .perp_market_config_by_index(market.index())
            .unwrap();
        let name = core::str::from_utf8(&market_config.name)
            .unwrap()
            .to_ascii_lowercase();
        !name.contains("bet") && market_config.status != MarketStatus::Initialized
    });
    market_ids
}

/// Fold a `(user, order_id)` pair into a single u32 for the `OrderSlotLimiter` (which keys on
/// u32). `order_id` is a per-user counter, so a bare order_id collides across users; mixing in
/// the user pubkey prefix makes cross-user collisions negligible.
fn order_dedup_key(user: &Pubkey, order_id: u32) -> u32 {
    let b = user.to_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]]) ^ order_id
}

#[cfg(test)]
mod tests {
    use super::{order_dedup_key, Pubkey};

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
