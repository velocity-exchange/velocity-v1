//! Liquidator bot
//!
//! The liquidator keeps every non-dust user in memory and watches their margin on the cached
//! market state. Users within 10% of their maintenance requirement form the high-risk set, which
//! is rechecked on every oracle update; every user is rechecked at least every 30s. A user whose
//! margin breaks, and whose oracles are fresh, is queued for the worker, which plans the route in
//! `plan.rs` and sends it from `execute.rs`. A derisk loop closes whatever positions the
//! liquidator itself takes over.

use {
    crate::{
        common::{
            collateral::CollateralBook,
            keeper::{unix_now_ms, Keeper},
            metrics::{MarginStatus, Metrics, UserMarginStatus},
            oracle::{ExchangeState, PythPriceUpdate},
            tx::{TakeoverFallback, TxWorker},
        },
        Config, UseMarkets,
    },
    dashmap::DashMap,
    events::{subscribe_events, GrpcEvent},
    execute::LiquidationEngine,
    std::{
        collections::{BTreeMap, HashMap, HashSet},
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc, RwLock,
        },
    },
    tokio::sync::mpsc::error::TryRecvError,
    velocity_rs::{
        dlob::{DLOBNotifier, DLOB},
        market_state::{MarketStateData, SimplifiedMarginCalculation},
        math::liquidation::calculate_collateral,
        priority_fee_subscriber::PriorityFeeSubscriber,
        program::math::time::{Millis, SlotClock},
        types::{accounts::User, MarginRequirementType, MarketId, MarketStatus, MarketType},
        MarketState, Pubkey, VelocityClient,
    },
    worker::{
        send_liquidation, spawn_collateral_reconciler, spawn_derisk_loop, spawn_liquidation_worker,
        LiquidationRequest,
    },
};

mod events;
mod execute;
mod plan;
mod worker;

const TARGET: &str = "liquidator";

/// Threshold for considering a user high-risk: free margin < 10% of margin requirement
const HIGH_RISK_FREE_MARGIN_RATIO: f64 = 0.1;
/// Maximum oracle price age before considering stale (~20s, expressed in actual slots at the
/// current slot duration)
const MAX_ORACLE_AGE: Millis = Millis::from_secs(20);
/// Maximum age for Pyth prices in milliseconds before considering stale
const PYTH_UPDATE_MAX_AGE_MS: u64 = 5000;
/// Permanently blocked spot markets (untradable tokens)
const BLOCKED_SPOT_MARKETS: &[u16] = &[40];

/// Every this many event batches, every user is rechecked.
const RECHECK_CYCLE_INTERVAL: u32 = 1024;
/// Max wall-clock time between full user sweeps. The cycle count alone has no time bound, one
/// cycle being one batch.
const FULL_RECHECK_INTERVAL_MS: u64 = 30_000;
/// `State` is a full Borsh parse and the cached slot clock already integrates every scheduled
/// transition, so it is re-read on a wall-clock cadence, not per batch. Only a newly staged
/// transition needs the re-read.
const SLOT_CLOCK_REFRESH_INTERVAL_MS: u64 = 30_000;
const COLLATERAL_REFRESH_INTERVAL_MS: u64 = 5_000;

/// Outcome of a liquidation attempt, used to drive backoff decisions
#[derive(Debug, Clone)]
pub enum LiquidationOutcome {
    /// A transaction was built and sent to the tx worker
    TxSent,
    /// Liquidation was skipped due to missing data, no positions, no makers, etc.
    Skipped(&'static str),
}

impl LiquidationOutcome {
    pub fn is_sent(&self) -> bool {
        matches!(self, Self::TxSent)
    }

    pub fn reason(&self) -> &'static str {
        match self {
            Self::TxSent => "sent",
            Self::Skipped(reason) => reason,
        }
    }
}

/// Errors indicating data staleness
#[derive(Debug, Clone)]
enum StalenessError {
    OraclePriceStale { market: MarketId, age_slots: u64 },
    PythPriceStale,
}

pub struct LiquidatorBot {
    velocity: &'static VelocityClient,
    config: Config,
    dlob_notifier: DLOBNotifier,
    /// perp and spot market metadata and oracle prices
    market_state: Arc<RwLock<MarketState>>,
    events_rx: tokio::sync::mpsc::Receiver<GrpcEvent>,
    pyth_feed: tokio::sync::mpsc::Receiver<PythPriceUpdate>,
    liquidations: tokio::sync::mpsc::Sender<LiquidationRequest>,
    subaccounts: Vec<Pubkey>,
    collateral: CollateralBook,
    /// The live slot duration (ms), shared with the worker so its rate limit re-paces on a slot
    /// duration switch without a restart.
    slot_duration_ms: Arc<AtomicU64>,
}

/// The run loop's view of every watched user.
struct MarginWatch {
    users: BTreeMap<Pubkey, User>,
    high_risk: HashSet<Pubkey>,
    oracle_slots: HashMap<MarketId, u64>,
    pyth_perp_prices: BTreeMap<u16, PythPriceUpdate>,
    current_slot: u64,
    slot_clock: SlotClock,
    liquidation_margin_buffer_ratio: u32,
    cycle_count: u32,
    last_full_recheck_ms: u64,
    last_slot_clock_refresh_ms: u64,
    last_collateral_refresh_ms: u64,
}

impl LiquidatorBot {
    pub async fn new(config: Config, velocity: VelocityClient, metrics: Arc<Metrics>) -> Self {
        let velocity: &'static VelocityClient = Box::leak(Box::new(velocity));
        let dlob: &'static DLOB = Box::leak(Box::new(DLOB::default()));

        let perp_market_ids = liquidatable_perp_market_ids(velocity, &config);
        let spot_market_ids: Vec<MarketId> = velocity
            .program_data()
            .spot_market_configs()
            .iter()
            .map(|market| MarketId::spot(market.market_index))
            .collect();
        let market_pubkeys: Vec<Pubkey> = perp_market_ids
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

        let subaccounts: Vec<Pubkey> = config
            .get_subaccounts()
            .iter()
            .map(|id| velocity.wallet.sub_account(*id))
            .collect();
        log::info!(target: TARGET, "liquidator 🫠 bot started: authority={:?}, subaccount={:?}", velocity.wallet.authority(), subaccounts);

        velocity.subscribe_blockhashes().await.expect("subscribed");

        let collateral = CollateralBook::default();
        observe_collateral(velocity, &subaccounts, &collateral).await;
        // in-flight tracking, so concurrent liquidations do not over-commit collateral
        let takeover_fallbacks: Arc<DashMap<(Pubkey, u16), TakeoverFallback>> =
            Arc::new(DashMap::new());

        let tx = TxWorker::new(
            velocity.clone(),
            Arc::clone(&metrics),
            config.dry,
            Some(collateral.clone()),
            Some(Arc::clone(&takeover_fallbacks)),
        )
        .run(tokio::runtime::Handle::current());

        let dlob_notifier = dlob.spawn_notifier();
        let events_rx = subscribe_events(
            velocity.clone(),
            dlob_notifier.clone(),
            tx.clone(),
            perp_market_ids.clone(),
            subaccounts.clone(),
        )
        .await;
        log::info!(target: TARGET, "subscribed gRPC");

        let market_state = Arc::new(RwLock::new(MarketState::new(initial_market_state(
            velocity,
        ))));

        let pyth_token = std::env::var("PYTH_LAZER_TOKEN").expect("pyth access token");
        let pyth_client = pyth_lazer_client::LazerClient::new(
            "wss://pyth-lazer.dourolabs.app/v1/stream",
            pyth_token.as_str(),
        )
        .expect("pyth price feed connects");
        let pyth_spot_markets: &[MarketId] = if config.use_spot_liquidation {
            &spot_market_ids
        } else {
            &[]
        };
        let pyth_feed = crate::common::oracle::subscribe_price_feeds(
            pyth_client,
            &perp_market_ids,
            pyth_spot_markets,
            &[],
        );
        log::info!(target: TARGET, "subscribed pyth price feeds");

        let cu_limit = std::env::var("FILL_CU_LIMIT")
            .ok()
            .and_then(|limit| limit.parse::<u32>().ok())
            .unwrap_or(config.fill_cu_limit);

        // seeded from the real chain slot, so a restart after a slot duration switch re-paces
        // immediately; the run loop keeps it current
        let startup_slot = velocity.get_slot().await.unwrap_or(0);
        let slot_duration_ms = Arc::new(AtomicU64::new(
            velocity.slot_duration_at(startup_slot).as_ms(),
        ));

        let (liquidations, liquidation_requests) = tokio::sync::mpsc::channel(102400);
        let engine = LiquidationEngine {
            keeper: Keeper {
                velocity,
                tx: tx.clone(),
                metrics: Arc::clone(&metrics),
            },
            dlob,
            market_state: Arc::clone(&market_state),
            subaccounts: subaccounts.clone(),
            use_spot_liquidation: config.use_spot_liquidation,
            collateral: collateral.clone(),
            takeover_fallbacks,
        };
        spawn_liquidation_worker(
            Arc::new(engine),
            liquidation_requests,
            cu_limit,
            Arc::clone(&priority_fees),
            Arc::clone(&slot_duration_ms),
        );
        log::info!(target: TARGET, "spawned liquidation worker");

        spawn_derisk_loop(velocity, tx, subaccounts.clone(), priority_fees, cu_limit);
        log::info!(target: TARGET, "spawned derisk worker");

        spawn_collateral_reconciler(velocity, collateral.clone());
        log::info!(target: TARGET, "spawned collateral reconciler");

        LiquidatorBot {
            velocity,
            config,
            dlob_notifier,
            market_state,
            events_rx,
            pyth_feed,
            liquidations,
            subaccounts,
            collateral,
            slot_duration_ms,
        }
    }

    pub async fn run(mut self) {
        let exchange =
            ExchangeState::load(self.velocity).expect("State account in cache at startup");
        let startup_slot = self.velocity.get_slot().await.unwrap_or(0);
        self.slot_duration_ms.store(
            self.velocity.slot_duration_at(startup_slot).as_ms(),
            Ordering::Relaxed,
        );

        let now = unix_now_ms();
        let mut watch = MarginWatch {
            users: BTreeMap::new(),
            high_risk: HashSet::new(),
            oracle_slots: HashMap::new(),
            pyth_perp_prices: BTreeMap::new(),
            current_slot: 0,
            slot_clock: self.velocity.slot_clock(),
            liquidation_margin_buffer_ratio: exchange.liquidation_margin_buffer_ratio,
            cycle_count: 0,
            last_full_recheck_ms: now,
            last_slot_clock_refresh_ms: now,
            last_collateral_refresh_ms: 0,
        };
        self.load_users(&mut watch);

        let mut events = Vec::<GrpcEvent>::with_capacity(64);
        log::info!(target: TARGET, "entering main event loop");
        loop {
            if !self.drain_pyth_prices(&mut watch) {
                return;
            }
            if self.events_rx.recv_many(&mut events, 64).await == 0 {
                log::error!(target: TARGET, "grpc event channel closed, exiting liquidator loop");
                return;
            }

            // refresh on wall clock, not on user traffic, so an oracle-only period still picks
            // up a newly staged slot duration transition; a transient miss keeps the old clock
            let batch_now_ms = unix_now_ms();
            if batch_now_ms.saturating_sub(watch.last_slot_clock_refresh_ms)
                >= SLOT_CLOCK_REFRESH_INTERVAL_MS
            {
                watch.last_slot_clock_refresh_ms = batch_now_ms;
                if let Some(exchange) = ExchangeState::load(self.velocity) {
                    watch.slot_clock = exchange.slot_clock;
                }
            }

            let mut oracle_updated = false;
            for event in events.drain(..) {
                match event {
                    GrpcEvent::UserUpdate { pubkey, user, slot } => {
                        self.on_user_update(&mut watch, pubkey, user, slot).await;
                    }
                    GrpcEvent::OracleUpdate {
                        oracle_price_data,
                        market,
                        slot,
                    } => {
                        // a per-update firehose, kept at trace so it stays retrievable
                        log::trace!(target: TARGET, "oracle update received: market={market:?}, slot={slot}");
                        if slot < watch.current_slot {
                            continue;
                        }
                        if oracle_price_data.price > 0 {
                            let state = self.market_state.write().unwrap();
                            if market.is_perp() {
                                state.set_perp_oracle_price(market.index(), oracle_price_data);
                            } else {
                                state.set_spot_oracle_price(market.index(), oracle_price_data);
                            }
                        }
                        watch.oracle_slots.insert(market, slot);
                        watch.current_slot = slot;
                        oracle_updated = true;
                    }
                    GrpcEvent::PerpMarketUpdate { market, slot } => {
                        if slot >= watch.current_slot {
                            self.market_state.write().unwrap().set_perp_market(market);
                            watch.current_slot = slot;
                        }
                    }
                    GrpcEvent::SpotMarketUpdate { market, slot } => {
                        if slot >= watch.current_slot {
                            self.market_state.write().unwrap().set_spot_market(market);
                            watch.current_slot = slot;
                        }
                    }
                }
            }

            // the high-risk set is rechecked only when an oracle price moved
            if oracle_updated {
                self.recheck_high_risk(&mut watch);
            }

            // Recheck every user to find new high-risk ones. The cycle count alone has no time
            // bound, and a fast move can take a user from safe to liquidatable between sweeps,
            // so elapsed time also triggers it.
            watch.cycle_count += 1;
            let now_ms = unix_now_ms();
            if watch.cycle_count.is_multiple_of(RECHECK_CYCLE_INTERVAL)
                || now_ms.saturating_sub(watch.last_full_recheck_ms) >= FULL_RECHECK_INTERVAL_MS
            {
                watch.last_full_recheck_ms = now_ms;
                self.recheck_all_users(&mut watch);
            }
        }
    }

    /// Load every cached user that is not dust, flagging the at-risk ones.
    fn load_users(&self, watch: &mut MarginWatch) {
        log::info!(target: TARGET, "starting user account initialization");
        let mut excluded = 0;
        let mut high_risk = 0;
        self.velocity
            .backend()
            .account_map()
            .iter_accounts_with::<User>(|pubkey, user, _slot| {
                let margin = match self.margin(user, watch.liquidation_margin_buffer_ratio) {
                    Ok(margin) => margin,
                    Err(err) => {
                        // keep watching it: a market or oracle missing at startup must not hide
                        // the account until its next own update
                        log::warn!(target: TARGET, "margin calc failed at init: user={pubkey:?} error={err:?}");
                        watch.users.insert(*pubkey, *user);
                        return;
                    }
                };
                if self.is_dust(&margin) {
                    excluded += 1;
                    return;
                }
                watch.users.insert(*pubkey, *user);
                if check_margin_status(&margin).is_at_risk() {
                    watch.high_risk.insert(*pubkey);
                    high_risk += 1;
                }
            });
        log::info!(target: TARGET, "filtered #{excluded} accounts with dust collateral");
        log::info!(target: TARGET, "identified #{high_risk} high-risk accounts for monitoring");
    }

    /// Drain the pyth feed without blocking into the margin math's prices. False when the feed
    /// is gone.
    fn drain_pyth_prices(&mut self, watch: &mut MarginWatch) -> bool {
        loop {
            match self.pyth_feed.try_recv() {
                Ok(update) => {
                    let (market_type, market_index, price) =
                        (update.market_type, update.market_id, update.price);
                    if market_type == MarketType::Perp {
                        watch.pyth_perp_prices.insert(market_index, update);
                    }
                    if price > 0 {
                        let state = self.market_state.write().unwrap();
                        match market_type {
                            MarketType::Perp => {
                                state.set_perp_pyth_price(market_index, price as i64)
                            }
                            MarketType::Spot => {
                                state.set_spot_pyth_price(market_index, price as i64)
                            }
                        }
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    log::error!(target: TARGET, "pyth price feed disconnected");
                    return false;
                }
                Err(TryRecvError::Empty) => return true,
            }
        }
    }

    async fn on_user_update(&self, watch: &mut MarginWatch, pubkey: Pubkey, user: User, slot: u64) {
        let now_ms = unix_now_ms();
        if now_ms.saturating_sub(watch.last_collateral_refresh_ms) >= COLLATERAL_REFRESH_INTERVAL_MS
        {
            watch.last_collateral_refresh_ms = now_ms;
            // pick up a slot duration switch, and re-pace the worker's rate limit with it
            self.slot_duration_ms.store(
                self.velocity.slot_duration_at(slot).as_ms(),
                Ordering::Relaxed,
            );

            observe_collateral(self.velocity, &self.subaccounts, &self.collateral).await;
        }

        self.dlob_notifier
            .user_update(pubkey, watch.users.get(&pubkey), &user, slot);
        watch.users.insert(pubkey, user);

        let margin = match self.margin(&user, watch.liquidation_margin_buffer_ratio) {
            Ok(margin) => margin,
            Err(err) => {
                log::warn!(target: TARGET, "margin calc failed: user={pubkey:?} error={err:?}");
                return;
            }
        };
        if self.is_dust(&margin) {
            watch.high_risk.remove(&pubkey);
            watch.users.remove(&pubkey);
        } else if check_margin_status(&margin).is_at_risk() {
            watch.high_risk.insert(pubkey);
        } else {
            watch.high_risk.remove(&pubkey);
        }
    }

    /// Queue the liquidatable high-risk users, and drop the ones that are safe again.
    fn recheck_high_risk(&self, watch: &mut MarginWatch) {
        let mut liquidatable = Vec::new();
        for pubkey in &watch.high_risk {
            let Some(user) = watch.users.get(pubkey) else {
                continue;
            };
            // a liquidation decided on a dead price feed is more likely wrong than late
            if let Err(StalenessError::OraclePriceStale { market, age_slots }) =
                validate_data_freshness(
                    user,
                    &watch.oracle_slots,
                    watch.current_slot,
                    watch.slot_clock,
                )
            {
                log::warn!(
                    target: TARGET,
                    "skipping liquidation check: reason=stale_oracle user={pubkey:?} market={market:?} age_slots={age_slots} current_slot={}",
                    watch.current_slot,
                );
                continue;
            }
            let margin = match self.margin(user, watch.liquidation_margin_buffer_ratio) {
                Ok(margin) => margin,
                Err(err) => {
                    log::warn!(target: TARGET, "margin calc failed: user={pubkey:?} error={err:?}");
                    continue;
                }
            };
            let status = check_margin_status(&margin);
            if status.is_liquidatable() {
                liquidatable.push((*pubkey, *user, status));
            }
        }

        for (pubkey, user, status) in liquidatable {
            self.queue_liquidation(watch, pubkey, user, status);
        }

        watch.high_risk.retain(|pubkey| {
            let Some(user) = watch.users.get(pubkey) else {
                return false;
            };
            // with a stale oracle the margin picture is unreliable, so keep watching
            if validate_data_freshness(
                user,
                &watch.oracle_slots,
                watch.current_slot,
                watch.slot_clock,
            )
            .is_err()
            {
                return true;
            }
            match self.margin(user, watch.liquidation_margin_buffer_ratio) {
                Ok(margin) => check_margin_status(&margin).is_at_risk(),
                Err(err) => {
                    log::warn!(target: TARGET, "margin calc failed: user={pubkey:?} error={err:?}");
                    false
                }
            }
        });
    }

    /// Sweep the users outside the high-risk set: queue the liquidatable ones and add the
    /// at-risk ones.
    fn recheck_all_users(&self, watch: &mut MarginWatch) {
        let started_ms = unix_now_ms();
        let mut newly_high_risk = 0;
        let mut liquidatable = Vec::new();

        for (pubkey, user) in watch.users.iter() {
            if watch.high_risk.contains(pubkey)
                || validate_data_freshness(
                    user,
                    &watch.oracle_slots,
                    watch.current_slot,
                    watch.slot_clock,
                )
                .is_err()
            {
                continue;
            }
            let margin = match self.margin(user, watch.liquidation_margin_buffer_ratio) {
                Ok(margin) => margin,
                Err(err) => {
                    log::warn!(target: TARGET, "margin calc failed: user={pubkey:?} error={err:?}");
                    continue;
                }
            };
            let status = check_margin_status(&margin);
            if status.is_liquidatable() {
                log::info!(
                    target: TARGET,
                    "found liquidatable user: user={pubkey:?} total_collateral={} margin_requirement={} status={status:?} slot={}",
                    margin.total_collateral,
                    margin.margin_requirement,
                    watch.current_slot,
                );
                liquidatable.push((*pubkey, *user, status));
            } else if status.is_at_risk() {
                watch.high_risk.insert(*pubkey);
                newly_high_risk += 1;
            }
        }

        for (pubkey, user, status) in liquidatable {
            watch.high_risk.insert(pubkey);
            newly_high_risk += 1;
            self.queue_liquidation(watch, pubkey, user, status);
        }

        log::debug!(
            target: TARGET,
            "margin recheck: users={} newly_high_risk={newly_high_risk} high_risk_total={} took_ms={}",
            watch.users.len(),
            watch.high_risk.len(),
            unix_now_ms() - started_ms,
        );
    }

    fn queue_liquidation(
        &self,
        watch: &MarginWatch,
        pubkey: Pubkey,
        user: User,
        status: UserMarginStatus,
    ) {
        send_liquidation(
            &self.liquidations,
            LiquidationRequest {
                pubkey,
                pyth_price_updates: fresh_pyth_updates_for_user(&user, &watch.pyth_perp_prices),
                user,
                slot: watch.current_slot,
                timestamp_ms: unix_now_ms(),
                status,
            },
        );
    }

    /// The user's maintenance margin, with the liquidation buffer, on the cached market state.
    fn margin(
        &self,
        user: &User,
        liquidation_margin_buffer_ratio: u32,
    ) -> Result<SimplifiedMarginCalculation, impl std::fmt::Debug> {
        self.market_state
            .read()
            .unwrap()
            .calculate_simplified_margin_requirement(
                user,
                MarginRequirementType::Maintenance,
                Some(liquidation_margin_buffer_ratio),
            )
    }

    /// Both the collateral and the requirement are below `min_collateral`, so the user is not
    /// worth watching.
    fn is_dust(&self, margin: &SimplifiedMarginCalculation) -> bool {
        margin.total_collateral < self.config.min_collateral as i128
            && margin.margin_requirement < self.config.min_collateral as u128
    }
}

/// The configured perp markets, without bet markets and markets still initializing.
fn liquidatable_perp_market_ids(velocity: &VelocityClient, config: &Config) -> Vec<MarketId> {
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

/// Every market and its cached oracle price, for the margin math.
fn initial_market_state(velocity: &VelocityClient) -> MarketStateData {
    let mut market_state = MarketStateData {
        // the pyth price is only used when it differs from the oracle by more than 5 bps
        pyth_oracle_diff_threshold_bps: 5,
        ..Default::default()
    };
    let oracles = velocity.backend().oracle_map();
    for market in velocity.program_data().perp_market_configs() {
        market_state.set_perp_market(*market);
        if let Some(oracle) = oracles.get_by_market(&MarketId::perp(market.market_index)) {
            market_state.set_perp_oracle_price(market.market_index, oracle.data);
        }
    }
    for market in velocity.program_data().spot_market_configs() {
        market_state.set_spot_market(*market);
        if let Some(oracle) = oracles.get_by_market(&MarketId::spot(market.market_index)) {
            market_state.set_spot_oracle_price(market.market_index, oracle.data);
        }
    }
    market_state
}

/// Validate that the oracle prices backing the user's positions are fresh enough.
///
/// Deliberately does NOT check user-account age: gRPC only pushes account updates on change, so
/// an idle account is old but not stale. Oracle age is integrated across slot duration regimes,
/// not converted with the duration at one endpoint.
fn validate_data_freshness(
    user: &User,
    oracle_slots: &HashMap<MarketId, u64>,
    current_slot: u64,
    slot_clock: SlotClock,
) -> Result<(), StalenessError> {
    let perp_markets = user
        .perp_positions
        .iter()
        .filter(|position| position.base_asset_amount != 0)
        .map(|position| MarketId::perp(position.market_index));
    let spot_markets = user
        .spot_positions
        .iter()
        .filter(|position| !position.is_available())
        .map(|position| MarketId::spot(position.market_index));

    for market in perp_markets.chain(spot_markets) {
        if let Some(&oracle_slot) = oracle_slots.get(&market) {
            if slot_clock.elapsed(oracle_slot, current_slot) > MAX_ORACLE_AGE {
                return Err(StalenessError::OraclePriceStale {
                    market,
                    age_slots: current_slot.saturating_sub(oracle_slot),
                });
            }
        }
    }
    Ok(())
}

/// A pyth price older than `PYTH_UPDATE_MAX_AGE_MS`, or that far in the future, is stale. A
/// future timestamp passes the program's own check but is skipped by its post handler, so the
/// liquidation would run on the old onchain price.
fn validate_pyth_price_freshness(pyth_update: &PythPriceUpdate) -> Result<(), StalenessError> {
    // pyth timestamps are in microseconds
    let pyth_ts_ms = pyth_update.ts.0 / 1000;
    if unix_now_ms().abs_diff(pyth_ts_ms) > PYTH_UPDATE_MAX_AGE_MS {
        return Err(StalenessError::PythPriceStale);
    }
    Ok(())
}

/// Check margin status (liquidatable, high-risk, or safe)
///
/// Mirrors the program's liquidation eligibility checks
/// (`MarginCalculation::meets_cross_margin_requirement` /
/// `IsolatedMarginCalculation::meets_margin_requirement`): liquidatable iff
/// `total_collateral < margin_requirement`, using the unbuffered requirement.
/// Insolvent positions (`total_collateral <= 0`) are liquidatable, not skippable.
fn check_margin_status(margin_info: &SimplifiedMarginCalculation) -> UserMarginStatus {
    let status = |total_collateral: i128, margin_requirement: u128| {
        if total_collateral < margin_requirement as i128 {
            MarginStatus::Liquidatable
        } else if margin_requirement > 0 {
            let free_margin = total_collateral - margin_requirement as i128;
            if (free_margin as f64 / margin_requirement as f64) < HIGH_RISK_FREE_MARGIN_RATIO {
                MarginStatus::HighRisk
            } else {
                MarginStatus::Safe
            }
        } else {
            MarginStatus::Safe
        }
    };

    let isolated = margin_info
        .isolated_margin_calculations
        .iter()
        .filter(|calc| !calc.is_empty())
        .filter_map(|calc| {
            let status = status(calc.total_collateral, calc.margin_requirement);
            (status != MarginStatus::Safe).then_some((calc.market_index, status))
        })
        .collect();

    UserMarginStatus {
        cross: status(margin_info.total_collateral, margin_info.margin_requirement),
        isolated,
    }
}

/// Fresh pyth prices for every perp market the user holds a position in, keyed by market
/// index. Carried per market, so each position the plan selects (an isolated position is not
/// necessarily the largest one) can post its own market's update.
fn fresh_pyth_updates_for_user(
    user: &User,
    pyth_perp_prices: &BTreeMap<u16, PythPriceUpdate>,
) -> BTreeMap<u16, PythPriceUpdate> {
    user.perp_positions
        .iter()
        .filter(|position| position.base_asset_amount != 0)
        .filter_map(|position| {
            let update = pyth_perp_prices.get(&position.market_index)?;
            if validate_pyth_price_freshness(update).is_ok() {
                Some((position.market_index, update.clone()))
            } else {
                log::debug!(target: TARGET, "skipping stale pyth price: market={}", position.market_index);
                None
            }
        })
        .collect()
}

/// Record each liquidator subaccount's free collateral with the slot of the account data it
/// was computed from, which tells the book which settled reservations the snapshot includes.
async fn observe_collateral(
    velocity: &VelocityClient,
    subaccounts: &[Pubkey],
    book: &CollateralBook,
) {
    for &subaccount in subaccounts {
        let user = match velocity.get_user_account_with_slot(&subaccount).await {
            Ok(user) => user,
            Err(err) => {
                log::warn!(target: TARGET, "Failed to load subaccount {subaccount}: {err:?}");
                continue;
            }
        };
        match calculate_collateral(velocity, &user.data, MarginRequirementType::Maintenance) {
            Ok(info) => {
                log::debug!(
                    target: TARGET,
                    "Subaccount {subaccount}: free_collateral = {}, total_collateral = {}, slot = {}",
                    info.free,
                    info.total,
                    user.slot
                );
                book.observe(subaccount, info.free, user.slot);
            }
            Err(err) => {
                log::warn!(target: TARGET, "Failed to calculate collateral for subaccount {subaccount}: {err:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, pyth_lazer_protocol::router::TimestampUs,
        velocity_rs::market_state::IsolatedMarginCalculation,
    };

    fn margin_calc(
        total_collateral: i128,
        margin_requirement: u128,
    ) -> SimplifiedMarginCalculation {
        SimplifiedMarginCalculation {
            total_collateral,
            total_collateral_buffer: 0,
            margin_requirement,
            margin_requirement_plus_buffer: margin_requirement,
            isolated_margin_calculations: Default::default(),
            with_perp_isolated_liability: false,
            with_spot_isolated_liability: false,
        }
    }

    fn iso_calc(
        market_index: u16,
        total_collateral: i128,
        margin_requirement: u128,
    ) -> IsolatedMarginCalculation {
        IsolatedMarginCalculation {
            market_index,
            margin_requirement,
            total_collateral,
            total_collateral_buffer: 0,
            margin_requirement_plus_buffer: margin_requirement,
        }
    }

    #[test]
    fn cross_liquidatable_below_maintenance_margin() {
        // mirrors program: liquidatable iff total_collateral < margin_requirement
        let status = check_margin_status(&margin_calc(99, 100));
        assert_eq!(status.cross, MarginStatus::Liquidatable);
        assert!(status.is_liquidatable());
    }

    #[test]
    fn cross_insolvent_is_liquidatable() {
        let status = check_margin_status(&margin_calc(-500, 100));
        assert_eq!(status.cross, MarginStatus::Liquidatable);
    }

    #[test]
    fn cross_exactly_at_requirement_is_not_liquidatable() {
        // program uses >=: meeting the requirement exactly is not liquidatable
        let status = check_margin_status(&margin_calc(100, 100));
        assert_ne!(status.cross, MarginStatus::Liquidatable);
        // but zero free margin is high risk
        assert_eq!(status.cross, MarginStatus::HighRisk);
    }

    #[test]
    fn cross_high_risk_and_safe_thresholds() {
        // free margin ratio 5% < 10% threshold
        assert_eq!(
            check_margin_status(&margin_calc(105, 100)).cross,
            MarginStatus::HighRisk
        );
        // free margin ratio 50%
        assert_eq!(
            check_margin_status(&margin_calc(150, 100)).cross,
            MarginStatus::Safe
        );
        // empty account
        assert_eq!(
            check_margin_status(&margin_calc(0, 0)).cross,
            MarginStatus::Safe
        );
    }

    #[test]
    fn insolvent_isolated_position_is_liquidatable() {
        // regression: insolvent (total_collateral <= 0) isolated positions were
        // skipped entirely and never flagged liquidatable
        let mut calc = margin_calc(1_000, 100); // cross is safe
        calc.isolated_margin_calculations[0] = iso_calc(3, -50, 100);
        calc.isolated_margin_calculations[1] = iso_calc(7, 0, 100);

        let status = check_margin_status(&calc);
        assert!(status.isolated.contains(&(3, MarginStatus::Liquidatable)));
        assert!(status.isolated.contains(&(7, MarginStatus::Liquidatable)));
        assert!(status.is_liquidatable());
    }

    #[test]
    fn healthy_isolated_position_not_flagged() {
        let mut calc = margin_calc(1_000, 100);
        calc.isolated_margin_calculations[0] = iso_calc(3, 200, 100);
        let status = check_margin_status(&calc);
        assert!(status.isolated.is_empty());
        assert!(!status.is_liquidatable());
    }

    fn pyth_update(market_id: u16, ts_us: u64) -> PythPriceUpdate {
        PythPriceUpdate {
            market_type: MarketType::Perp,
            market_id,
            feed_id: market_id as u32,
            price: 42,
            message: vec![],
            ts: TimestampUs(ts_us),
        }
    }

    #[test]
    fn pyth_updates_are_carried_per_market() {
        let mut user = User::default();
        user.perp_positions[0].market_index = 0;
        user.perp_positions[0].base_asset_amount = 1_000;
        user.perp_positions[0].quote_asset_amount = 500;
        user.perp_positions[1].market_index = 1;
        user.perp_positions[1].base_asset_amount = -100_000;
        user.perp_positions[1].quote_asset_amount = -50_000;

        let now_us = unix_now_ms() * 1_000;
        let mut prices: BTreeMap<u16, PythPriceUpdate> = [0u16, 1, 9]
            .into_iter()
            .map(|market_id| (market_id, pyth_update(market_id, now_us)))
            .collect();

        // one update per held market, so each selected position (isolated ones included)
        // ships its own market's price; markets the user does not hold are not carried
        let updates = fresh_pyth_updates_for_user(&user, &prices);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates.get(&0).unwrap().market_id, 0);
        assert_eq!(updates.get(&1).unwrap().market_id, 1);

        // a stale market drops out individually, the fresh one stays
        prices.insert(
            1,
            pyth_update(1, now_us - (PYTH_UPDATE_MAX_AGE_MS + 1_000) * 1_000),
        );
        let updates = fresh_pyth_updates_for_user(&user, &prices);
        assert_eq!(updates.len(), 1);
        assert!(updates.contains_key(&0));
    }

    #[test]
    fn future_dated_pyth_update_is_stale() {
        // the program skips a post whose timestamp is ahead of its clock, so the liquidation
        // would read the old onchain price
        let now_us = unix_now_ms() * 1_000;
        let ahead = pyth_update(0, now_us + (PYTH_UPDATE_MAX_AGE_MS + 1_000) * 1_000);
        assert!(validate_pyth_price_freshness(&ahead).is_err());
        let slightly_ahead = pyth_update(0, now_us + 100_000);
        assert!(validate_pyth_price_freshness(&slightly_ahead).is_ok());
    }
}
