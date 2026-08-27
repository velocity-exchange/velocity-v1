//! HTTP and metrics server
use {
    axum::{
        extract::State,
        http::{header::CONTENT_TYPE, Response, StatusCode},
        response::{Html, IntoResponse, Json},
    },
    prometheus::{
        Encoder, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Registry,
        TextEncoder,
    },
    serde::{Deserialize, Serialize},
    std::sync::Arc,
    tokio::sync::RwLock,
    velocity_quoter_health::{
        metrics::Metrics as QuoterMetrics, store::now_ms, Health, Policy as QuoterPolicy,
    },
};

/// Margin status indicating liquidation risk level
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MarginStatus {
    /// User is liquidatable (total_collateral < margin_requirement)
    Liquidatable,
    /// User is high-risk but not yet liquidatable (free margin < 20% of margin requirement)
    HighRisk,
    /// User is safe (not liquidatable and not high-risk)
    Safe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMarginStatus {
    pub cross: MarginStatus,
    /// Only contains isolated positions that are high risk or liquidatable
    pub isolated: Vec<(u16, MarginStatus)>, // (market_index, status)
}

impl UserMarginStatus {
    pub fn is_liquidatable(&self) -> bool {
        self.cross == MarginStatus::Liquidatable
            || self
                .isolated
                .iter()
                .any(|(_, s)| *s == MarginStatus::Liquidatable)
    }

    pub fn is_at_risk(&self) -> bool {
        self.cross != MarginStatus::Safe || !self.isolated.is_empty()
    }
}

/// Market type for positions and oracles
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MarketType {
    Perp,
    Spot,
}

#[derive(Debug)]
pub struct Metrics {
    pub tx_sent: IntCounterVec,
    pub tx_confirmed: IntCounterVec,
    pub tx_failed: IntCounterVec,
    pub trigger_expected: IntCounter,
    pub trigger_actual: IntCounter,
    pub swift_placed: IntCounter,
    pub swift_place_skipped: IntCounter,
    /// Book makers a fill reached past but could not carry.
    pub clob_makers_dropped: IntCounter,
    /// Book makers a fill did carry, so the two read as a ratio.
    pub clob_makers_carried: IntCounter,
    pub fill_expected: IntCounterVec,
    pub fill_actual: IntCounterVec,
    pub liquidation_attempts: IntCounterVec,
    pub liquidation_success: IntCounterVec,
    pub liquidation_failed: IntCounterVec,
    pub liquidation_skipped: IntCounterVec,
    pub liquidation_backoff_skips: IntCounter,
    pub swap_quote_latency_ms: IntGauge,
    pub pyth_price_age_ms: IntGaugeVec,
    pub jupiter_quote_failures: IntCounter,
    pub titan_quote_failures: IntCounter,
    pub confirmation_slots: HistogramVec,
    pub cu_spent: HistogramVec,
    /// Quoter health, reported from what the filler's own simulations show.
    ///
    /// The filler sees a class of failure nothing else does: a quoter whose
    /// execute leg breaks a real fill. It does not exclude the quoter itself,
    /// because a signed route is enforced on chain and dropping an entry the
    /// taker named only trades one rejection for another. Exclusion belongs
    /// where the route is chosen, before it is signed.
    pub quoter_health: Arc<Health>,
    pub registry: Registry,
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();
        let quoter_metrics = Arc::new(QuoterMetrics::register(&registry));
        let quoter_health = Arc::new(Health::with_metrics(
            QuoterPolicy::default(),
            quoter_metrics,
        ));

        let tx_sent = IntCounterVec::new(
            prometheus::Opts::new("rfb_tx_sent_total", "Number of transactions sent"),
            &["intent"],
        )
        .unwrap();
        registry.register(Box::new(tx_sent.clone())).unwrap();

        let tx_confirmed = IntCounterVec::new(
            prometheus::Opts::new("rfb_tx_confirmed_total", "Number of transactions confirmed"),
            &["intent", "result"],
        )
        .unwrap();
        registry.register(Box::new(tx_confirmed.clone())).unwrap();

        let tx_failed = IntCounterVec::new(
            prometheus::Opts::new("rfb_tx_failed_total", "Number of transactions failed"),
            &["intent", "reason"],
        )
        .unwrap();
        registry.register(Box::new(tx_failed.clone())).unwrap();

        let fill_expected = IntCounterVec::new(
            prometheus::Opts::new("rfb_fill_expected_total", "Number of expected fills"),
            &["intent"],
        )
        .unwrap();
        registry.register(Box::new(fill_expected.clone())).unwrap();

        let fill_actual = IntCounterVec::new(
            prometheus::Opts::new("rfb_fill_actual_total", "Number of actual fills"),
            &["intent"],
        )
        .unwrap();
        registry.register(Box::new(fill_actual.clone())).unwrap();

        let trigger_expected = IntCounter::new(
            "rfb_trigger_expected_total",
            "Number of expected triggered orders",
        )
        .unwrap();
        registry
            .register(Box::new(trigger_expected.clone()))
            .unwrap();

        let trigger_actual = IntCounter::new(
            "rfb_trigger_actual_total",
            "Number of actual triggered orders",
        )
        .unwrap();
        registry.register(Box::new(trigger_actual.clone())).unwrap();

        let swift_placed = IntCounter::new(
            "rfb_swift_placed_total",
            "Swift orders placed on-chain (no immediate fill) so the slot loop can fill them later",
        )
        .unwrap();
        registry.register(Box::new(swift_placed.clone())).unwrap();

        let swift_place_skipped = IntCounter::new(
            "rfb_swift_place_skipped_total",
            "Swift orders not placed because they were already past their on-chain placement window",
        )
        .unwrap();
        registry
            .register(Box::new(swift_place_skipped.clone()))
            .unwrap();

        // A book stops at the first maker the transaction did not bring, so
        // every dropped maker is depth this fill left resting and the taker
        // did not get. Whether that is worth designing around depends on how
        // often it happens at all, which nothing measured until now.
        let clob_makers_dropped = IntCounter::new(
            "rfb_clob_makers_dropped_total",
            "CLOB makers within reach of a fill that its account budget could not carry",
        )
        .unwrap();
        registry
            .register(Box::new(clob_makers_dropped.clone()))
            .unwrap();

        let clob_makers_carried = IntCounter::new(
            "rfb_clob_makers_carried_total",
            "CLOB makers a fill carried the accounts for",
        )
        .unwrap();
        registry
            .register(Box::new(clob_makers_carried.clone()))
            .unwrap();

        let liquidation_attempts = IntCounterVec::new(
            prometheus::Opts::new(
                "rfb_liquidation_attempts_total",
                "Number of liquidation attempts",
            ),
            &["type"],
        )
        .unwrap();
        registry
            .register(Box::new(liquidation_attempts.clone()))
            .unwrap();

        let liquidation_success = IntCounterVec::new(
            prometheus::Opts::new(
                "rfb_liquidation_success_total",
                "Number of successful liquidations",
            ),
            &["type"],
        )
        .unwrap();
        registry
            .register(Box::new(liquidation_success.clone()))
            .unwrap();

        let liquidation_failed = IntCounterVec::new(
            prometheus::Opts::new(
                "rfb_liquidation_failed_total",
                "Number of failed liquidations",
            ),
            &["type"],
        )
        .unwrap();
        registry
            .register(Box::new(liquidation_failed.clone()))
            .unwrap();

        let liquidation_skipped = IntCounterVec::new(
            prometheus::Opts::new(
                "rfb_liquidation_skipped_total",
                "Number of liquidations skipped by reason",
            ),
            &["reason"],
        )
        .unwrap();
        registry
            .register(Box::new(liquidation_skipped.clone()))
            .unwrap();

        let liquidation_backoff_skips = IntCounter::new(
            "rfb_liquidation_backoff_skips_total",
            "Number of liquidation attempts skipped due to exponential backoff",
        )
        .unwrap();
        registry
            .register(Box::new(liquidation_backoff_skips.clone()))
            .unwrap();

        let swap_quote_latency_ms = IntGauge::new(
            "rfb_swap_quote_latency_ms",
            "Swap quote request latency in milliseconds",
        )
        .unwrap();
        registry
            .register(Box::new(swap_quote_latency_ms.clone()))
            .unwrap();

        let pyth_price_age_ms = IntGaugeVec::new(
            prometheus::Opts::new(
                "rfb_pyth_price_age_ms",
                "Wall-clock age of the last-consumed pyth-lazer price update, in milliseconds",
            ),
            &["market"],
        )
        .unwrap();
        registry
            .register(Box::new(pyth_price_age_ms.clone()))
            .unwrap();

        let jupiter_quote_failures = IntCounter::new(
            "rfb_jupiter_quote_failures_total",
            "Number of Jupiter quote failures",
        )
        .unwrap();
        registry
            .register(Box::new(jupiter_quote_failures.clone()))
            .unwrap();

        let titan_quote_failures = IntCounter::new(
            "rfb_titan_quote_failures_total",
            "Number of Titan quote failures",
        )
        .unwrap();
        registry
            .register(Box::new(titan_quote_failures.clone()))
            .unwrap();

        let confirmation_slots = HistogramVec::new(
            prometheus::HistogramOpts::new(
                "rfb_tx_confirmation_slots",
                "Slots taken to confirm tx",
            ),
            &["intent"],
        )
        .unwrap();
        registry
            .register(Box::new(confirmation_slots.clone()))
            .unwrap();

        let cu_spent = HistogramVec::new(
            prometheus::HistogramOpts::new("rfb_tx_cu_spent", "Compute units spent per tx"),
            &["intent"],
        )
        .unwrap();
        registry.register(Box::new(cu_spent.clone())).unwrap();

        // Pre-warm the label children the liquidator uses: a registered
        // IntCounterVec exports NO series until with_label_values() creates a
        // child, so on a quiet market these counters are absent for days and
        // the Grafana "Liquidation Attempts Rate" alert can't tell "no
        // liquidations" from "metric missing". Touching the children here makes
        // them export as 0 from startup.
        for market_type in ["perp", "spot"] {
            liquidation_attempts.with_label_values(&[market_type]);
            liquidation_success.with_label_values(&[market_type]);
            liquidation_failed.with_label_values(&[market_type]);
        }

        Self {
            tx_sent,
            tx_confirmed,
            tx_failed,
            fill_expected,
            fill_actual,
            liquidation_attempts,
            liquidation_success,
            liquidation_failed,
            liquidation_skipped,
            liquidation_backoff_skips,
            swap_quote_latency_ms,
            pyth_price_age_ms,
            jupiter_quote_failures,
            titan_quote_failures,
            confirmation_slots,
            cu_spent,
            quoter_health,
            registry,
            trigger_expected,
            trigger_actual,
            swift_placed,
            swift_place_skipped,
            clob_makers_dropped,
            clob_makers_carried,
        }
    }
}

pub async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    // Quoter gauges describe the state as it stands, so they are refreshed
    // here. The counters beside them were written as observations arrived.
    if let Some(quoter) = state.metrics.quoter_health.metrics() {
        quoter.sync(&state.metrics.quoter_health, now_ms());
    }
    let metric_families = state.metrics.registry.gather();
    let mut buffer = Vec::new();
    let encoder = TextEncoder::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();

    let response = String::from_utf8(buffer).unwrap();
    Response::builder()
        .header(CONTENT_TYPE, "text/plain;version=1.0.0;charset=utf-8")
        .body(response)
        .unwrap()
}

/// Liveness endpoint for the k8s probe: 200 while every tracked upstream feed is
/// live, 503 otherwise (with a JSON body naming the dead feed) so the kubelet
/// restarts the pod. See [`FeedHealth`].
pub async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    let swift_stream_live = state.feed_health.swift_stream_live();
    let grpc_stream_live = state.feed_health.grpc_stream_live();
    let pyth_stream_live = state.feed_health.pyth_stream_live();
    let status = if swift_stream_live && grpc_stream_live && pyth_stream_live {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(serde_json::json!({
            "swift_stream_live": swift_stream_live,
            "grpc_stream_live": grpc_stream_live,
            "pyth_stream_live": pyth_stream_live,
        })),
    )
}

/// Dashboard state shared between liquidator and HTTP server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardState {
    pub high_risk_users: Vec<HighRiskUser>,
    pub oracle_prices: Vec<OraclePriceInfo>,
    pub current_slot: u64,
    pub last_updated_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HighRiskUser {
    pub pubkey: String,
    pub authority: String,
    pub total_collateral: i128,
    pub margin_requirement: u128,
    pub free_margin: i128,
    pub free_margin_ratio: f64,
    pub status: MarginStatus,
    pub last_updated_slot: u64,
    pub last_updated_ms: u64,
    pub positions: Vec<PositionInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionInfo {
    pub market_type: MarketType,
    pub market_index: u16,
    pub base_asset_amount: i64,
    pub quote_asset_amount: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OraclePriceInfo {
    pub market_type: MarketType,
    pub market_index: u16,
    pub price: i64,
    pub last_updated_slot: u64,
    pub last_updated_ms: u64,
    pub age_slots: u64,
    pub age_ms: u64,
    pub is_stale: bool,
}

/// Shared dashboard state
pub type DashboardStateRef = Arc<RwLock<Option<DashboardState>>>;

/// Combined state for HTTP handlers
#[derive(Clone)]
pub struct AppState {
    pub metrics: Arc<Metrics>,
    pub dashboard_state: DashboardStateRef,
    pub feed_health: Arc<FeedHealth>,
}

/// Upstream feed liveness shared between a bot's event loop and `/health`.
///
/// The k8s liveness probe is the last line of defence against silent feed death:
/// unlike in-loop watchdogs, the probe handler runs on its own task, so it still
/// answers (with a failure) when the bot's event loop is wedged inside one select
/// arm and can't run any watchdog of its own. Feeds a bot doesn't register stay
/// untracked and report live, so each bot only fails health on feeds it uses.
#[derive(Debug, Default)]
pub struct FeedHealth {
    /// unix ms of the last gRPC slot update; 0 = untracked
    last_slot_update_ms: std::sync::atomic::AtomicU64,
    /// swift ws subscription state: 0 = untracked, 1 = connected, 2 = disconnected
    swift_state: std::sync::atomic::AtomicU8,
    /// unix ms of the last pyth-lazer price update; 0 = untracked
    last_pyth_update_ms: std::sync::atomic::AtomicU64,
}

impl FeedHealth {
    /// gRPC slots arrive at least ~2.5/s (faster as slot time drops); this much silence means the feed is dead
    const GRPC_STALE_LIMIT_MS: u64 = 60_000;
    /// pyth-lazer feeds tick every 50-200ms; this much silence means the feed is dead
    const PYTH_STALE_LIMIT_MS: u64 = 60_000;

    fn unix_now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// Record a gRPC slot update (marks the grpc feed tracked)
    pub fn touch_slot(&self) {
        self.last_slot_update_ms
            .store(Self::unix_now_ms(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Record the swift subscription state (marks the swift feed tracked)
    pub fn set_swift_connected(&self, connected: bool) {
        self.swift_state.store(
            if connected { 1 } else { 2 },
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Record a pyth-lazer price update (marks the pyth feed tracked)
    pub fn touch_pyth(&self) {
        self.last_pyth_update_ms
            .store(Self::unix_now_ms(), std::sync::atomic::Ordering::Relaxed);
    }

    /// false only when the grpc feed is tracked and stale
    pub fn grpc_stream_live(&self) -> bool {
        let last = self
            .last_slot_update_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        last == 0 || Self::unix_now_ms().saturating_sub(last) < Self::GRPC_STALE_LIMIT_MS
    }

    /// false only when the swift feed is tracked and disconnected
    pub fn swift_stream_live(&self) -> bool {
        self.swift_state.load(std::sync::atomic::Ordering::Relaxed) != 2
    }

    /// false only when the pyth feed is tracked and stale
    pub fn pyth_stream_live(&self) -> bool {
        let last = self
            .last_pyth_update_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        last == 0 || Self::unix_now_ms().saturating_sub(last) < Self::PYTH_STALE_LIMIT_MS
    }
}

/// API endpoint to get dashboard data
pub async fn dashboard_api_handler(State(state): State<AppState>) -> impl IntoResponse {
    let dashboard_state = state.dashboard_state.read().await;
    match dashboard_state.as_ref() {
        Some(data) => Json(data.clone()).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Dashboard data not available"})),
        )
            .into_response(),
    }
}

/// Serve the dashboard HTML page
pub async fn dashboard_handler() -> Html<&'static str> {
    Html(include_str!("../static/dashboard.html"))
}
