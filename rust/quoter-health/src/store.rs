//! The state a router keeps about the quoters it carries.
//!
//! On the hot path a router asks whether one entry may go on one route.
//! Everything else in this module makes that answer correct, and lets an
//! operator see and override how it was reached.
//!
//! Sampling is deterministic. A throttled quoter appears on one route in N
//! rather than on a random subset. Two processes reading the same state
//! therefore carry the same quoter at the same rate, and a count of how often
//! it was carried is exact.

use {
    crate::{
        metrics::Metrics,
        observe::{FailReason, Observation, Report},
        parse::Verdict,
        score::{
            advance, on_program_upgrade, Admission, Cause, Pin, Policy, State, Transition, Window,
        },
    },
    dashmap::DashMap,
    solana_sdk::pubkey::Pubkey,
    std::{
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
        time::{SystemTime, UNIX_EPOCH},
    },
};

/// Milliseconds since the epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One quoter's record.
#[derive(Debug, Default)]
pub struct Entry {
    pub state: State,
    pub window: Window,
    pub pin: Option<Pin>,
    /// The market this quoter is registered for. A registry entry serves one
    /// market, so this is a label rather than a dimension.
    pub market: u16,
    /// Routes considered since this quoter was last carried. Drives the
    /// deterministic sampler.
    sample_counter: AtomicU64,
}

impl Entry {
    /// The admission in force, with an operator's pin taking precedence.
    pub fn effective(&self, now_ms: u64) -> Admission {
        match &self.pin {
            Some(pin) if pin.is_live(now_ms) => pin.admission,
            _ => self.state.admission,
        }
    }
}

/// What a router sees about a quoter, without holding a lock on it.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub quoter: Pubkey,
    pub market: u16,
    pub admission: Admission,
    pub pinned: bool,
    pub state: State,
    pub sim_failure_rate: f64,
    pub execute_failure_rate: f64,
    pub mean_cu: f64,
    pub clamp_ratio: f64,
    pub fill_shortfall: f64,
    pub mean_slip_bps: f64,
}

/// Per-process quoter health.
///
/// Every method takes `&self`, so a router wraps this in an `Arc` and shares
/// it with every task that simulates.
#[derive(Debug)]
pub struct Health {
    entries: DashMap<Pubkey, Entry>,
    policy: Policy,
    /// Failures no quoter was proven to have caused, by reason. This measures
    /// what the router failed to attribute, not a maker's behaviour.
    unattributed: DashMap<&'static str, u64>,
    transitions: parking_lot_free::Log,
    /// Set when the host registered a prometheus surface. It is held here
    /// rather than beside the router. One call then records both the decision
    /// and the series behind it, so the two cannot drift.
    metrics: Option<Arc<Metrics>>,
}

/// A bounded audit log that needs no lock crate.
mod parking_lot_free {
    use {super::Transition, std::sync::Mutex};

    /// The last transitions, newest last. Bounded so a flapping quoter cannot
    /// grow the process without limit.
    #[derive(Debug, Default)]
    pub struct Log(Mutex<Vec<Transition>>);

    const CAP: usize = 2_048;

    impl Log {
        pub fn push(&self, transition: Transition) {
            let mut guard = match self.0.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };

            if guard.len() >= CAP {
                guard.remove(0);
            }

            guard.push(transition);
        }

        pub fn snapshot(&self) -> Vec<Transition> {
            match self.0.lock() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }
    }
}

impl Health {
    pub fn new(policy: Policy) -> Self {
        Self {
            entries: DashMap::new(),
            policy,
            unattributed: DashMap::new(),
            transitions: parking_lot_free::Log::default(),
            metrics: None,
        }
    }

    /// Build a `Health` that reports to prometheus as well as deciding.
    pub fn with_metrics(policy: Policy, metrics: Arc<Metrics>) -> Self {
        Self {
            metrics: Some(metrics),
            ..Self::new(policy)
        }
    }

    pub fn metrics(&self) -> Option<&Arc<Metrics>> {
        self.metrics.as_ref()
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// May this quoter ride on the route being built now.
    ///
    /// This consumes one step of the quoter's sampler, so call it once per
    /// route per quoter. An unknown quoter is admitted. Refusing a quoter
    /// nobody has measured would stop it ever being measured.
    pub fn admits(&self, quoter: &Pubkey) -> bool {
        let now = now_ms();
        let Some(entry) = self.entries.get(quoter) else {
            return true;
        };
        let rate = entry.effective(now).sample_rate(now);
        if rate >= 1.0 {
            return true;
        }
        if rate <= 0.0 {
            return false;
        }

        // One route in N, counted rather than drawn, so the rate is exact and
        // two processes agree.
        let stride = (1.0 / rate).round().max(1.0) as u64;
        entry.sample_counter.fetch_add(1, Ordering::Relaxed) % stride == 0
    }

    /// Quoters excluded outright right now. A router passes these to the
    /// instruction builder so a quarantined or denied quoter never enters a
    /// simulation. Throttled quoters are not here; [`Health::admits`]
    /// decides those once per route.
    pub fn excluded(&self) -> Vec<Pubkey> {
        let now = now_ms();
        self.entries
            .iter()
            .filter(|kv| kv.value().effective(now).sample_rate(now) <= 0.0)
            .map(|kv| *kv.key())
            .collect()
    }

    /// The admission in force, without consuming a sampler step.
    pub fn admission(&self, quoter: &Pubkey) -> Admission {
        self.entries
            .get(quoter)
            .map(|entry| entry.effective(now_ms()))
            .unwrap_or(Admission::Admit)
    }

    /// True when this quoter must not be the only source covering a size.
    pub fn needs_backup(&self, quoter: &Pubkey) -> bool {
        self.admission(quoter).needs_backup()
    }

    /// Filter a route's candidate entries down to those admitted now.
    pub fn admitted<'a>(&self, quoters: impl IntoIterator<Item = &'a Pubkey>) -> Vec<Pubkey> {
        quoters
            .into_iter()
            .filter(|quoter| self.admits(quoter))
            .copied()
            .collect()
    }

    fn with_entry<T>(&self, quoter: &Pubkey, f: impl FnOnce(&mut Entry) -> T) -> T {
        let mut entry = self.entries.entry(*quoter).or_default();
        f(entry.value_mut())
    }

    /// Record one observation and advance that quoter's ladder.
    pub fn record(&self, report: Report) {
        self.record_at(now_ms(), report)
    }

    pub fn record_at(&self, now: u64, report: Report) {
        if let Some(metrics) = &self.metrics {
            metrics.observe(&report);
        }

        let policy = self.policy;
        let transition = self.with_entry(&report.quoter, |entry| {
            entry.state.last_seen_ms = now;
            entry.market = report.market;
            if matches!(
                report.observation,
                Observation::SimOk { .. } | Observation::ExecuteOk { .. }
            ) {
                entry.state.last_success_ms = now;
                entry.state.clean_streak = entry.state.clean_streak.saturating_add(1);
            }

            entry
                .window
                .record(now, policy.half_life_ms, report.observation);

            let before = entry.state.admission;
            advance(now, &mut entry.state, &entry.window, &policy);
            (before != entry.state.admission).then(|| Transition {
                quoter: report.quoter.to_string(),
                at_ms: now,
                from: before,
                to: entry.state.admission,
                cause: entry.state.last_cause.unwrap_or(Cause::CleanStreak),
                actor: "auto".into(),
                detail: format!(
                    "sim_fail_rate={:.3} exec_fail_rate={:.3} violations={:.1} \
                     mean_cu={:.0} clamp={:.2} slip_bps={:.1}",
                    entry.window.sim_failure_rate(now, policy.half_life_ms),
                    entry.window.execute_failure_rate(now, policy.half_life_ms),
                    entry.window.violations.value(now, policy.half_life_ms),
                    entry.window.mean_cu(now, policy.half_life_ms),
                    entry.window.clamp_ratio(now, policy.half_life_ms),
                    entry.window.mean_slip_bps(now, policy.half_life_ms),
                ),
            })
        });

        if let Some(transition) = transition {
            self.note(transition);
        }
    }

    /// Write a transition to the audit trail and to prometheus.
    fn note(&self, transition: Transition) {
        if let Some(metrics) = &self.metrics {
            metrics.observe_transition(
                &transition.quoter,
                transition.to.as_str(),
                transition.cause.as_str(),
                &transition.actor,
            );
        }

        self.transitions.push(transition);
    }

    /// Record everything one failed simulation proved.
    ///
    /// Only charges are recorded. A suspect is a hypothesis the caller should
    /// settle by re-simulating without it, and an unproven suspicion must not
    /// count against a maker.
    pub fn record_verdict(&self, market: u16, verdict: &Verdict) {
        for charge in &verdict.charges {
            if !charge.proof.is_actionable() {
                continue;
            }

            self.record(Report::new(
                charge.quoter,
                market,
                Observation::SimFail {
                    reason: charge.reason,
                    proof: charge.proof,
                },
            ));
        }

        if let Some(reason) = verdict.unattributed {
            self.record_unattributed(reason);
        }
    }

    /// Note a failure no quoter was proven to have caused.
    pub fn record_unattributed(&self, reason: FailReason) {
        if let Some(metrics) = &self.metrics {
            metrics.observe_unattributed(reason);
        }

        *self.unattributed.entry(reason.as_str()).or_insert(0) += 1;
    }

    pub fn unattributed_counts(&self) -> Vec<(&'static str, u64)> {
        self.unattributed
            .iter()
            .map(|kv| (*kv.key(), *kv.value()))
            .collect()
    }

    /// Note the deploy slot of a quoter's program.
    ///
    /// A change drops the counters and starts probation, because the score
    /// described code that no longer runs.
    pub fn observe_program_slot(&self, quoter: &Pubkey, slot: u64) {
        let now = now_ms();
        let policy = self.policy;
        let transition = self.with_entry(quoter, |entry| {
            let before = entry.state.admission;
            on_program_upgrade(now, slot, &mut entry.state, &mut entry.window, &policy);
            (before != entry.state.admission).then(|| Transition {
                quoter: quoter.to_string(),
                at_ms: now,
                from: before,
                to: entry.state.admission,
                cause: Cause::ProgramUpgraded,
                actor: "auto".into(),
                detail: format!("deploy_slot={slot}"),
            })
        });

        if let Some(transition) = transition {
            self.note(transition);
        }
    }

    /// Override a quoter's admission. The scorer will not move it while the
    /// pin is live.
    pub fn pin(&self, quoter: &Pubkey, pin: Pin) {
        let now = now_ms();
        let before = self.admission(quoter);
        let to = pin.admission;
        let (reason, actor) = (pin.reason.clone(), pin.actor.clone());
        self.with_entry(quoter, |entry| entry.pin = Some(pin));
        self.note(Transition {
            quoter: quoter.to_string(),
            at_ms: now,
            from: before,
            to,
            cause: Cause::Pinned,
            actor,
            detail: reason,
        });
    }

    /// Drop a pin. The computed state stayed underneath the pin, so automatic
    /// handling resumes at once.
    pub fn clear_pin(&self, quoter: &Pubkey) {
        let now = now_ms();
        let before = self.admission(quoter);
        let after = self.with_entry(quoter, |entry| {
            entry.pin = None;
            entry.state.admission
        });

        self.note(Transition {
            quoter: quoter.to_string(),
            at_ms: now,
            from: before,
            to: after,
            cause: Cause::PinCleared,
            actor: "operator".into(),
            detail: String::new(),
        });
    }

    /// Advance every ladder and drop quoters nobody has mentioned in a while.
    ///
    /// Run this on a timer. Expiry is evaluated when a quoter is read, so
    /// without the timer a quarantine on a quoter no route touches would
    /// never expire.
    pub fn sweep(&self) {
        let now = now_ms();
        let policy = self.policy;
        let mut transitions = Vec::new();
        for mut kv in self.entries.iter_mut() {
            let quoter = kv.key().to_string();
            let entry = kv.value_mut();
            let before = entry.state.admission;
            advance(now, &mut entry.state, &entry.window, &policy);
            if before != entry.state.admission {
                transitions.push(Transition {
                    quoter,
                    at_ms: now,
                    from: before,
                    to: entry.state.admission,
                    cause: entry.state.last_cause.unwrap_or(Cause::QuarantineExpired),
                    actor: "auto".into(),
                    detail: String::new(),
                });
            }
        }
        for transition in transitions {
            self.note(transition);
        }

        // A quiet quoter is forgotten, but never one an operator pinned or
        // one still serving a quarantine. Dropping either would readmit it.
        self.entries.retain(|_, entry| {
            entry.pin.is_some()
                || matches!(entry.state.admission, Admission::Quarantined { .. })
                || now.saturating_sub(entry.state.last_seen_ms) < policy.entry_ttl_ms
        });
    }

    pub fn snapshot(&self) -> Vec<Snapshot> {
        let now = now_ms();
        let hl = self.policy.half_life_ms;
        self.entries
            .iter()
            .map(|kv| {
                let entry = kv.value();
                Snapshot {
                    quoter: *kv.key(),
                    market: entry.market,
                    admission: entry.effective(now),
                    pinned: entry.pin.as_ref().map(|p| p.is_live(now)).unwrap_or(false),
                    state: entry.state,
                    sim_failure_rate: entry.window.sim_failure_rate(now, hl),
                    execute_failure_rate: entry.window.execute_failure_rate(now, hl),
                    mean_cu: entry.window.mean_cu(now, hl),
                    clamp_ratio: entry.window.clamp_ratio(now, hl),
                    fill_shortfall: entry.window.fill_shortfall(now, hl),
                    mean_slip_bps: entry.window.mean_slip_bps(now, hl),
                }
            })
            .collect()
    }

    pub fn transitions(&self) -> Vec<Transition> {
        self.transitions.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{observe::Attribution, parse::Charge},
    };

    fn key(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    fn health() -> Health {
        Health::new(Policy::default())
    }

    #[test]
    fn an_unmeasured_quoter_is_carried() {
        // Refusing what has never been measured would stop it ever being
        // measured.
        assert!(health().admits(&key(1)));
    }

    #[test]
    fn a_pin_overrides_the_score_and_clearing_it_restores_the_score() {
        let health = health();
        let quoter = key(2);
        health.record(Report::new(quoter, 0, Observation::SimOk { cu: 1 }));
        assert!(health.admits(&quoter));

        health.pin(
            &quoter,
            Pin {
                admission: Admission::Denied,
                reason: "under investigation".into(),
                expires_ms: None,
                actor: "operator".into(),
                set_at_ms: 0,
            },
        );

        assert!(!health.admits(&quoter));

        health.clear_pin(&quoter);
        assert!(health.admits(&quoter));
    }

    #[test]
    fn an_expired_pin_stops_binding() {
        let health = health();
        let quoter = key(3);
        health.pin(
            &quoter,
            Pin {
                admission: Admission::Denied,
                reason: "temporary".into(),
                expires_ms: Some(1),
                actor: "operator".into(),
                set_at_ms: 0,
            },
        );

        assert!(health.admits(&quoter));
    }

    #[test]
    fn throttling_carries_the_quoter_at_the_stated_rate() {
        let health = health();
        let quoter = key(4);
        health.pin(
            &quoter,
            Pin {
                admission: Admission::Throttled { sample_rate: 0.25 },
                reason: "test".into(),
                expires_ms: None,
                actor: "operator".into(),
                set_at_ms: 0,
            },
        );

        let carried = (0..100).filter(|_| health.admits(&quoter)).count();
        assert_eq!(carried, 25);
    }

    #[test]
    fn only_a_proven_charge_counts_against_a_quoter() {
        let health = health();
        let charged = key(5);
        let suspected = key(6);
        let verdict = Verdict {
            charges: vec![
                Charge {
                    quoter: charged,
                    reason: FailReason::Cpi,
                    proof: Attribution::Named,
                },
                Charge {
                    quoter: suspected,
                    reason: FailReason::Cpi,
                    proof: Attribution::Bracketed,
                },
            ],

            suspects: vec![],
            unattributed: None,
        };

        health.record_verdict(0, &verdict);
        let seen: Vec<Pubkey> = health.snapshot().into_iter().map(|s| s.quoter).collect();
        assert!(seen.contains(&charged));
        assert!(!seen.contains(&suspected));
    }

    #[test]
    fn an_unattributed_failure_is_charged_to_the_router() {
        let health = health();
        health.record_verdict(
            0,
            &Verdict {
                charges: vec![],
                suspects: vec![],
                unattributed: Some(FailReason::Unknown),
            },
        );

        assert!(health.snapshot().is_empty());
        assert_eq!(health.unattributed_counts(), vec![("unknown", 1)]);
    }

    #[test]
    fn a_state_change_is_written_to_the_audit_trail() {
        let health = health();
        let quoter = key(7);
        for _ in 0..40 {
            health.record(Report::new(quoter, 0, Observation::SimOk { cu: 1 }));
        }
        for _ in 0..20 {
            health.record(Report::new(
                quoter,
                0,
                Observation::SimFail {
                    reason: FailReason::Cpi,
                    proof: Attribution::Named,
                },
            ));
        }

        let transitions = health.transitions();
        assert!(!transitions.is_empty());
        let last = transitions.last().unwrap();
        assert_eq!(last.actor, "auto");
        assert!(last.detail.contains("sim_fail_rate"));
    }

    #[test]
    fn the_sweep_never_forgets_a_quarantined_or_pinned_quoter() {
        // Dropping either would readmit it.
        let health = Health::new(Policy {
            entry_ttl_ms: 0,
            ..Policy::default()
        });
        let pinned = key(8);
        let quiet = key(9);
        health.record(Report::new(quiet, 0, Observation::SimOk { cu: 1 }));
        health.pin(
            &pinned,
            Pin {
                admission: Admission::Denied,
                reason: "hold".into(),
                expires_ms: None,
                actor: "operator".into(),
                set_at_ms: 0,
            },
        );

        health.sweep();
        let seen: Vec<Pubkey> = health.snapshot().into_iter().map(|s| s.quoter).collect();
        assert!(seen.contains(&pinned));
        assert!(!seen.contains(&quiet));
    }
}
