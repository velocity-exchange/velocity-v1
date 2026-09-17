//! The publisher's prometheus endpoint and operator surface.
//!
//! The publisher simulates every market every tick, so it exercises every
//! registered quoter continuously, including ones no taker is routing to.
//! That makes it the router stack's health probe.

use {
    axum::{
        extract::{Path, State},
        http::{header::CONTENT_TYPE, StatusCode},
        response::IntoResponse,
        routing::{delete, get, post},
        Json, Router,
    },
    prometheus::{Registry, TextEncoder},
    serde::Deserialize,
    serde_json::json,
    solana_sdk::pubkey::Pubkey,
    std::{net::SocketAddr, str::FromStr, sync::Arc},
    tracing::{info, warn},
    velocity_quoter_health::{
        metrics::Metrics,
        score::{Admission, Pin},
        store::now_ms,
        Health,
    },
};

#[derive(Clone)]
struct Exporter {
    registry: Arc<Registry>,
    metrics: Arc<Metrics>,
    health: Arc<Health>,
}

async fn metrics_handler(State(state): State<Exporter>) -> impl IntoResponse {
    // Gauges describe the state as it stands, so they are refreshed at scrape
    // time. Counters were written as the observations arrived.
    state.metrics.sync(&state.health, now_ms());
    let mut body = String::new();
    if let Err(err) = TextEncoder::new().encode_utf8(&state.registry.gather(), &mut body) {
        warn!(error = %err, "encode metrics");
    }
    (
        [(CONTENT_TYPE, "text/plain;version=1.0.0;charset=utf-8")],
        body,
    )
}

/// The state of every quoter this publisher tracks.
async fn quoters_handler(State(state): State<Exporter>) -> impl IntoResponse {
    let now = now_ms();
    let rows: Vec<_> = state
        .health
        .snapshot()
        .into_iter()
        .map(|snapshot| {
            json!({
                "quoter": snapshot.quoter.to_string(),
                "market": snapshot.market,
                "admission": snapshot.admission.as_str(),
                "pinned": snapshot.pinned,
                "backoffLevel": snapshot.state.backoff_level,
                "cleanStreak": snapshot.state.clean_streak,
                "lastCause": snapshot.state.last_cause.map(|cause| cause.as_str()),
                "lastSuccessAgeSeconds":
                    now.saturating_sub(snapshot.state.last_success_ms) / 1_000,
                "simFailureRate": snapshot.sim_failure_rate,
                "executeFailureRate": snapshot.execute_failure_rate,
                "meanCu": snapshot.mean_cu,
                "clampRatio": snapshot.clamp_ratio,
                "fillShortfall": snapshot.fill_shortfall,
                "meanSlipBps": snapshot.mean_slip_bps,
            })
        })
        .collect();
    Json(json!({ "quoters": rows, "unattributed": state.health.unattributed_counts() }))
}

/// Why each quoter's admission last moved, newest last.
async fn transitions_handler(State(state): State<Exporter>) -> impl IntoResponse {
    let rows: Vec<_> = state
        .health
        .transitions()
        .into_iter()
        .map(|transition| {
            json!({
                "quoter": transition.quoter,
                "atMs": transition.at_ms,
                "from": transition.from.as_str(),
                "to": transition.to.as_str(),
                "cause": transition.cause.as_str(),
                "actor": transition.actor,
                "detail": transition.detail,
            })
        })
        .collect();
    Json(json!({ "transitions": rows }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PinRequest {
    /// admit | throttled | probation | denied.
    admission: String,
    /// The reason for the override. It is recorded in the audit trail, so a
    /// pin explains itself later.
    reason: String,
    sample_rate: Option<f64>,
    /// How long the override lasts. An override with no end turns a temporary
    /// decision into a permanent one, so set a value.
    expires_in_seconds: Option<u64>,
    actor: Option<String>,
}

/// Override a quoter's admission. The scorer will not move it while this
/// stands.
async fn pin_handler(
    State(state): State<Exporter>,
    Path(quoter): Path<String>,
    Json(request): Json<PinRequest>,
) -> impl IntoResponse {
    let Ok(quoter) = Pubkey::from_str(&quoter) else {
        return (StatusCode::BAD_REQUEST, "quoter is not a pubkey").into_response();
    };
    let sample_rate = request.sample_rate.unwrap_or(0.1);
    let admission = match request.admission.as_str() {
        "admit" => Admission::Admit,
        "throttled" => Admission::Throttled { sample_rate },
        "probation" => Admission::Probation { sample_rate },
        "denied" => Admission::Denied,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                format!("unknown admission {other}"),
            )
                .into_response()
        }
    };
    let now = now_ms();
    state.health.pin(
        &quoter,
        Pin {
            admission,
            reason: request.reason,
            expires_ms: request
                .expires_in_seconds
                .map(|seconds| now + seconds * 1_000),
            actor: request.actor.unwrap_or_else(|| "operator".to_string()),
            set_at_ms: now,
        },
    );
    info!(%quoter, admission = admission.as_str(), "quoter pinned");
    StatusCode::NO_CONTENT.into_response()
}

/// Drop an override.
///
/// The computed state was kept underneath the whole time, so automatic
/// handling resumes at once.
async fn unpin_handler(
    State(state): State<Exporter>,
    Path(quoter): Path<String>,
) -> impl IntoResponse {
    let Ok(quoter) = Pubkey::from_str(&quoter) else {
        return (StatusCode::BAD_REQUEST, "quoter is not a pubkey").into_response();
    };
    state.health.clear_pin(&quoter);
    info!(%quoter, "quoter pin cleared");
    StatusCode::NO_CONTENT.into_response()
}

/// Serve the metrics and operator endpoints until the process ends.
pub fn serve(
    addr: SocketAddr,
    registry: Arc<Registry>,
    metrics: Arc<Metrics>,
    health: Arc<Health>,
) {
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/quoters", get(quoters_handler))
        .route("/quoters/transitions", get(transitions_handler))
        .route("/quoters/{quoter}/pin", post(pin_handler))
        .route("/quoters/{quoter}/pin", delete(unpin_handler))
        .with_state(Exporter {
            registry,
            metrics,
            health,
        });
    tokio::spawn(async move {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                info!(%addr, "metrics listening");
                if let Err(err) = axum::serve(listener, app).await {
                    warn!(error = %err, "metrics server stopped");
                }
            }
            Err(err) => warn!(%addr, error = %err, "metrics server could not bind"),
        }
    });
}
