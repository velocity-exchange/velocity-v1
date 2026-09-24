//! Prometheus surface for quoter health.
//!
//! Counters are monotonic and are written as observations arrive. Gauges
//! describe the current state and are refreshed from a snapshot, because the
//! underlying counters decay and a decayed value is not a counter.
//!
//! Per-quoter series are pruned. A registry can hold far more entries than a
//! router carries, and a label set that is never removed holds every quoter
//! that ever quoted in the scrape forever. A quoter is exported while a
//! router uses it, and always while it is degraded.

use {
    crate::{
        observe::{FailReason, Observation, Report},
        score::Admission,
        store::{Health, Snapshot},
    },
    prometheus::{
        GaugeVec, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, Registry,
    },
    std::{collections::HashSet, sync::Mutex},
};

/// How long a quiet quoter keeps its series.
const EXPORT_TTL_MS: u64 = 60 * 60 * 1_000;

const QUOTER_LABELS: [&str; 2] = ["quoter", "market"];

/// The metric set. Register once, then hand it to [`Health::with_metrics`].
#[derive(Debug)]
pub struct Metrics {
    sim_attempts: IntCounterVec,
    sim_failures: IntCounterVec,
    execute_attempts: IntCounterVec,
    execute_failures: IntCounterVec,
    unattributed: IntCounterVec,
    attribution_method: IntCounterVec,
    transitions: IntCounterVec,
    cu_consumed: HistogramVec,
    slip_bps: HistogramVec,
    health_state: IntGaugeVec,
    pinned: IntGaugeVec,
    sim_failure_rate: GaugeVec,
    execute_failure_rate: GaugeVec,
    clamp_ratio: GaugeVec,
    fill_shortfall: GaugeVec,
    last_success_age: GaugeVec,
    /// Label sets written by the last sync, so vanished ones can be removed.
    exported: Mutex<HashSet<(String, String)>>,
}

fn counter(registry: &Registry, name: &str, help: &str, labels: &[&str]) -> IntCounterVec {
    let metric = IntCounterVec::new(Opts::new(name, help), labels).expect("valid metric");
    registry
        .register(Box::new(metric.clone()))
        .expect("unique metric");
    metric
}

fn gauge(registry: &Registry, name: &str, help: &str) -> GaugeVec {
    let metric = GaugeVec::new(Opts::new(name, help), &QUOTER_LABELS).expect("valid metric");
    registry
        .register(Box::new(metric.clone()))
        .expect("unique metric");
    metric
}

fn int_gauge(registry: &Registry, name: &str, help: &str) -> IntGaugeVec {
    let metric = IntGaugeVec::new(Opts::new(name, help), &QUOTER_LABELS).expect("valid metric");
    registry
        .register(Box::new(metric.clone()))
        .expect("unique metric");
    metric
}

fn histogram(registry: &Registry, name: &str, help: &str, buckets: Vec<f64>) -> HistogramVec {
    let metric = HistogramVec::new(
        HistogramOpts::new(name, help).buckets(buckets),
        &QUOTER_LABELS,
    )
    .expect("valid metric");
    registry
        .register(Box::new(metric.clone()))
        .expect("unique metric");
    metric
}

impl Metrics {
    pub fn register(registry: &Registry) -> Self {
        Self {
            sim_attempts: counter(
                registry,
                "quoter_sim_attempts_total",
                "Simulations a quoter took part in",
                &QUOTER_LABELS,
            ),

            sim_failures: counter(
                registry,
                "quoter_sim_failures_total",
                "Simulations a quoter was proven to have broken",
                &["quoter", "market", "reason"],
            ),

            execute_attempts: counter(
                registry,
                "quoter_execute_attempts_total",
                "Execute legs a quoter took part in",
                &QUOTER_LABELS,
            ),

            execute_failures: counter(
                registry,
                "quoter_execute_failures_total",
                "Execute legs a quoter was proven to have broken",
                &["quoter", "market", "reason"],
            ),

            unattributed: counter(
                registry,
                "quoter_sim_failures_unattributed_total",
                "Simulation failures no quoter was proven to have caused. \
                 This measures the router's attribution coverage, not a maker",
                &["reason"],
            ),

            attribution_method: counter(
                registry,
                "quoter_attribution_method_total",
                "How a failure was attributed to a quoter",
                &["method"],
            ),

            transitions: counter(
                registry,
                "quoter_state_transitions_total",
                "Admission changes, by cause and who made them",
                &["quoter", "to", "cause", "actor"],
            ),

            cu_consumed: histogram(
                registry,
                "quoter_sim_cu_consumed",
                "Compute units a simulation carrying this quoter consumed",
                vec![
                    10_000.0,
                    50_000.0,
                    100_000.0,
                    200_000.0,
                    400_000.0,
                    700_000.0,
                    1_000_000.0,
                    1_400_000.0,
                ],
            ),

            slip_bps: histogram(
                registry,
                "quoter_route_slip_bps",
                "Basis points a landed route moved against the taker, \
                 measured from the price the router published",
                vec![-50.0, -10.0, -2.0, 0.0, 2.0, 10.0, 25.0, 50.0, 100.0],
            ),

            health_state: int_gauge(
                registry,
                "quoter_health_state",
                "0 admit, 1 throttled, 2 probation, 3 quarantined, 4 denied",
            ),

            pinned: int_gauge(
                registry,
                "quoter_pinned",
                "1 while an operator override is in force",
            ),

            sim_failure_rate: gauge(
                registry,
                "quoter_sim_failure_rate",
                "Decayed share of simulations this quoter broke",
            ),

            execute_failure_rate: gauge(
                registry,
                "quoter_execute_failure_rate",
                "Decayed share of execute legs this quoter broke",
            ),

            clamp_ratio: gauge(
                registry,
                "quoter_depth_clamped_ratio",
                "Decayed share of quoted depth the router had to cut before quoting it",
            ),

            fill_shortfall: gauge(
                registry,
                "quoter_fill_shortfall_ratio",
                "Decayed share of allocated base this quoter did not deliver",
            ),

            last_success_age: gauge(
                registry,
                "quoter_last_success_age_seconds",
                "Seconds since this quoter last did anything successfully",
            ),

            exported: Mutex::new(HashSet::new()),
        }
    }

    /// Record one observation. Called from [`Health::record`].
    pub fn observe(&self, report: &Report) {
        let quoter = report.quoter.to_string();
        let market = report.market.to_string();
        let labels = [quoter.as_str(), market.as_str()];
        match report.observation {
            Observation::SimOk { cu } => {
                self.sim_attempts.with_label_values(&labels).inc();
                self.cu_consumed
                    .with_label_values(&labels)
                    .observe(cu as f64);
            }
            Observation::SimFail { reason, proof } => {
                self.sim_attempts.with_label_values(&labels).inc();
                self.sim_failures
                    .with_label_values(&[quoter.as_str(), market.as_str(), reason.as_str()])
                    .inc();
                self.attribution_method
                    .with_label_values(&[proof.as_str()])
                    .inc();
            }
            Observation::ExecuteOk { .. } => {
                self.execute_attempts.with_label_values(&labels).inc();
            }
            Observation::ExecuteFail { reason } => {
                self.execute_attempts.with_label_values(&labels).inc();
                self.execute_failures
                    .with_label_values(&[quoter.as_str(), market.as_str(), reason.as_str()])
                    .inc();
            }
            Observation::DepthClamped { .. } => {}
            Observation::RouteLanded { .. } => {
                if let Some(bps) = report.observation.adverse_slip_bps() {
                    self.slip_bps.with_label_values(&labels).observe(bps);
                }
            }
        }
    }

    /// Record a failure charged to no quoter.
    pub fn observe_unattributed(&self, reason: FailReason) {
        self.unattributed
            .with_label_values(&[reason.as_str()])
            .inc();
    }

    pub fn observe_transition(&self, quoter: &str, to: &str, cause: &str, actor: &str) {
        self.transitions
            .with_label_values(&[quoter, to, cause, actor])
            .inc();
    }

    /// Refresh the gauges from current state, and drop series for quoters
    /// that are neither in use nor degraded.
    pub fn sync(&self, health: &Health, now_ms: u64) {
        let mut live: HashSet<(String, String)> = HashSet::new();
        for snapshot in health.snapshot() {
            if !Self::should_export(&snapshot, now_ms) {
                continue;
            }

            let quoter = snapshot.quoter.to_string();
            let market = snapshot.market.to_string();
            let labels = [quoter.as_str(), market.as_str()];
            self.health_state
                .with_label_values(&labels)
                .set(snapshot.admission.rank());
            self.pinned
                .with_label_values(&labels)
                .set(snapshot.pinned as i64);
            self.sim_failure_rate
                .with_label_values(&labels)
                .set(snapshot.sim_failure_rate);
            self.execute_failure_rate
                .with_label_values(&labels)
                .set(snapshot.execute_failure_rate);
            self.clamp_ratio
                .with_label_values(&labels)
                .set(snapshot.clamp_ratio);
            self.fill_shortfall
                .with_label_values(&labels)
                .set(snapshot.fill_shortfall);
            self.last_success_age
                .with_label_values(&labels)
                .set(now_ms.saturating_sub(snapshot.state.last_success_ms) as f64 / 1_000.0);
            live.insert((quoter, market));
        }

        let mut exported = match self.exported.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        for (quoter, market) in exported.difference(&live) {
            let labels = [quoter.as_str(), market.as_str()];
            let _ = self.health_state.remove_label_values(&labels);
            let _ = self.pinned.remove_label_values(&labels);
            let _ = self.sim_failure_rate.remove_label_values(&labels);
            let _ = self.execute_failure_rate.remove_label_values(&labels);
            let _ = self.clamp_ratio.remove_label_values(&labels);
            let _ = self.fill_shortfall.remove_label_values(&labels);
            let _ = self.last_success_age.remove_label_values(&labels);
        }

        *exported = live;
    }

    /// A degraded quoter is always exported. A quiet healthy one is dropped,
    /// because an operator needs the series that says something is wrong.
    fn should_export(snapshot: &Snapshot, now_ms: u64) -> bool {
        snapshot.admission != Admission::Admit
            || snapshot.pinned
            || now_ms.saturating_sub(snapshot.state.last_seen_ms) < EXPORT_TTL_MS
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            observe::Attribution,
            score::{Pin, Policy},
            store::now_ms,
        },
        solana_sdk::pubkey::Pubkey,
    };

    fn rendered(registry: &Registry) -> String {
        let mut text = String::new();
        prometheus::TextEncoder::new()
            .encode_utf8(&registry.gather(), &mut text)
            .expect("encode");
        text
    }

    #[test]
    fn an_unattributed_failure_is_reported_apart_from_any_quoter() {
        let registry = Registry::new();
        let metrics = Metrics::register(&registry);
        metrics.observe_unattributed(FailReason::Unknown);
        let text = rendered(&registry);
        assert!(text.contains("quoter_sim_failures_unattributed_total"));
        assert!(!text.contains("quoter_sim_failures_total{"));
    }

    #[test]
    fn attribution_method_is_reported_so_coverage_can_be_watched() {
        let registry = Registry::new();
        let metrics = Metrics::register(&registry);
        metrics.observe(&Report::new(
            Pubkey::new_unique(),
            0,
            Observation::SimFail {
                reason: FailReason::Cpi,
                proof: Attribution::Resim,
            },
        ));

        assert!(rendered(&registry).contains(r#"quoter_attribution_method_total{method="resim"}"#));
    }

    #[test]
    fn a_degraded_quoter_is_exported_even_when_it_has_gone_quiet() {
        let registry = Registry::new();
        let metrics = Metrics::register(&registry);
        let health = Health::new(Policy::default());
        let quoter = Pubkey::new_unique();
        health.pin(
            &quoter,
            Pin {
                admission: Admission::Denied,
                reason: "held".into(),
                expires_ms: None,
                actor: "operator".into(),
                set_at_ms: 0,
            },
        );

        // Far past the export window, where a healthy quoter is dropped.
        metrics.sync(&health, now_ms() + 10 * EXPORT_TTL_MS);
        let text = rendered(&registry);
        assert!(text.contains("quoter_health_state"));
        assert!(text.contains("quoter_pinned"));
    }

    #[test]
    fn a_quoter_that_stops_being_carried_stops_being_exported() {
        let registry = Registry::new();
        let metrics = Metrics::register(&registry);
        let health = Health::new(Policy {
            entry_ttl_ms: 0,
            ..Policy::default()
        });
        let quoter = Pubkey::new_unique();
        health.record(Report::new(quoter, 3, Observation::SimOk { cu: 1 }));
        metrics.sync(&health, now_ms());
        assert!(rendered(&registry).contains(&quoter.to_string()));

        health.sweep();
        metrics.sync(&health, now_ms());
        assert!(!rendered(&registry).contains("quoter_health_state{"));
    }
}
