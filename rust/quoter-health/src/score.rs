//! Decaying counters, the admission ladder, and the operator's pins.
//!
//! Counters decay. A quoter that failed last month must not still count as
//! failing now, and a bad rollout must not hide under a large good history.
//! Decay makes the score describe the code that runs now rather than every
//! version that ever ran.
//!
//! The decay is exponential, by half-life. The program's own rolling sums
//! (`math::stats::calculate_rolling_sum`) decay linearly to zero at the
//! window edge. Linear decay is cheap on chain, where the alternative costs
//! compute. Off chain the cliff is the difference that matters. A rate that
//! reaches the window edge drops to zero in one step, and that readmits a
//! quoter that has not improved.
//!
//! Every automatic exclusion carries an expiry, so a quoter recovers without
//! an operator. An operator's pin sits in a separate layer the scorer never
//! writes, so clearing a pin returns a quoter to automatic handling with no
//! state to rebuild.

use {
    crate::observe::Observation,
    serde::{Deserialize, Serialize},
};

/// How much of a router's flow a quoter may take.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Admission {
    /// Full flow.
    Admit,
    /// Carried on a fraction of routes.
    Throttled { sample_rate: f64 },
    /// Carried rarely, and never as the only source that can cover the size.
    Probation { sample_rate: f64 },
    /// Excluded until the given time. Expiry needs no operator.
    Quarantined { until_ms: u64 },
    /// Excluded until an operator clears it.
    Denied,
}

impl Admission {
    /// The share of routes this quoter may appear on.
    pub fn sample_rate(self, now_ms: u64) -> f64 {
        match self {
            Self::Admit => 1.0,
            Self::Throttled { sample_rate } | Self::Probation { sample_rate } => sample_rate,
            Self::Quarantined { until_ms } => {
                if now_ms >= until_ms {
                    1.0
                } else {
                    0.0
                }
            }
            Self::Denied => 0.0,
        }
    }

    /// True when this quoter must never be the only source covering a size.
    ///
    /// A quoter on probation is being retried, not trusted. Letting it carry
    /// a fill alone would make its next failure the taker's problem again.
    pub fn needs_backup(self) -> bool {
        matches!(self, Self::Probation { .. })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admit => "admit",
            Self::Throttled { .. } => "throttled",
            Self::Probation { .. } => "probation",
            Self::Quarantined { .. } => "quarantined",
            Self::Denied => "denied",
        }
    }

    /// Rank for a gauge, worst last.
    pub fn rank(self) -> i64 {
        match self {
            Self::Admit => 0,
            Self::Throttled { .. } => 1,
            Self::Probation { .. } => 2,
            Self::Quarantined { .. } => 3,
            Self::Denied => 4,
        }
    }
}

/// A sum that forgets at a fixed half-life.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Decaying {
    value: f64,
    last_ms: u64,
}

impl Decaying {
    /// The sum as it stands now, without writing.
    pub fn value(&self, now_ms: u64, half_life_ms: u64) -> f64 {
        if half_life_ms == 0 || self.value == 0.0 {
            return self.value;
        }
        let elapsed = now_ms.saturating_sub(self.last_ms) as f64;
        self.value * 0.5_f64.powf(elapsed / half_life_ms as f64)
    }

    pub fn add(&mut self, now_ms: u64, half_life_ms: u64, sample: f64) {
        self.value = self.value(now_ms, half_life_ms) + sample;
        self.last_ms = now_ms;
    }

    pub fn reset(&mut self) {
        self.value = 0.0;
        self.last_ms = 0;
    }
}

/// Everything measured about one quoter.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Window {
    pub sim_attempts: Decaying,
    pub sim_failures: Decaying,
    /// Contract violations, weighed apart from plain reverts. A quoter that
    /// answers off its own quote broke the response contract. A quoter that
    /// reverts only wasted a simulation.
    pub violations: Decaying,
    pub execute_attempts: Decaying,
    pub execute_failures: Decaying,
    pub cu_total: Decaying,
    pub cu_samples: Decaying,
    pub quoted_base: Decaying,
    pub admitted_base: Decaying,
    pub allocated_base: Decaying,
    pub filled_base: Decaying,
    pub slip_bps_total: Decaying,
    pub slip_samples: Decaying,
}

impl Window {
    pub fn record(&mut self, now_ms: u64, half_life_ms: u64, observation: Observation) {
        let hl = half_life_ms;
        match observation {
            Observation::SimOk { cu } => {
                self.sim_attempts.add(now_ms, hl, 1.0);
                self.cu_total.add(now_ms, hl, cu as f64);
                self.cu_samples.add(now_ms, hl, 1.0);
            }
            Observation::SimFail { reason, .. } => {
                self.sim_attempts.add(now_ms, hl, 1.0);
                self.sim_failures.add(now_ms, hl, 1.0);
                if reason.is_contract_violation() {
                    self.violations.add(now_ms, hl, 1.0);
                }
            }
            Observation::ExecuteOk {
                allocated_base,
                filled_base,
            } => {
                self.execute_attempts.add(now_ms, hl, 1.0);
                self.allocated_base.add(now_ms, hl, allocated_base as f64);
                self.filled_base.add(now_ms, hl, filled_base as f64);
            }
            Observation::ExecuteFail { reason } => {
                self.execute_attempts.add(now_ms, hl, 1.0);
                self.execute_failures.add(now_ms, hl, 1.0);
                if reason.is_contract_violation() {
                    self.violations.add(now_ms, hl, 1.0);
                }
            }
            Observation::DepthClamped {
                quoted_base,
                admitted_base,
            } => {
                self.quoted_base.add(now_ms, hl, quoted_base as f64);
                self.admitted_base.add(now_ms, hl, admitted_base as f64);
            }
            Observation::RouteLanded {
                quoted_price,
                executed_price,
                taker_long,
            } => {
                // Positive means the taker did worse than the route promised.
                let delta = executed_price as f64 - quoted_price as f64;
                let adverse = if taker_long { delta } else { -delta };
                let bps = if quoted_price == 0 {
                    0.0
                } else {
                    adverse / quoted_price as f64 * 10_000.0
                };
                self.slip_bps_total.add(now_ms, hl, bps);
                self.slip_samples.add(now_ms, hl, 1.0);
            }
        }
    }

    /// Wipe every counter. A redeploy of the quoter's program calls this,
    /// because the old numbers describe code that no longer runs.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn sim_failure_rate(&self, now_ms: u64, hl: u64) -> f64 {
        let attempts = self.sim_attempts.value(now_ms, hl);
        if attempts <= 0.0 {
            return 0.0;
        }
        self.sim_failures.value(now_ms, hl) / attempts
    }

    pub fn execute_failure_rate(&self, now_ms: u64, hl: u64) -> f64 {
        let attempts = self.execute_attempts.value(now_ms, hl);
        if attempts <= 0.0 {
            return 0.0;
        }
        self.execute_failures.value(now_ms, hl) / attempts
    }

    pub fn mean_cu(&self, now_ms: u64, hl: u64) -> f64 {
        let samples = self.cu_samples.value(now_ms, hl);
        if samples <= 0.0 {
            return 0.0;
        }
        self.cu_total.value(now_ms, hl) / samples
    }

    /// The share of quoted depth the router had to cut before quoting it.
    pub fn clamp_ratio(&self, now_ms: u64, hl: u64) -> f64 {
        let quoted = self.quoted_base.value(now_ms, hl);
        if quoted <= 0.0 {
            return 0.0;
        }
        let admitted = self.admitted_base.value(now_ms, hl);
        ((quoted - admitted) / quoted).clamp(0.0, 1.0)
    }

    /// The share of allocated base the quoter did not deliver.
    pub fn fill_shortfall(&self, now_ms: u64, hl: u64) -> f64 {
        let allocated = self.allocated_base.value(now_ms, hl);
        if allocated <= 0.0 {
            return 0.0;
        }
        let filled = self.filled_base.value(now_ms, hl);
        ((allocated - filled) / allocated).clamp(0.0, 1.0)
    }

    pub fn mean_slip_bps(&self, now_ms: u64, hl: u64) -> f64 {
        let samples = self.slip_samples.value(now_ms, hl);
        if samples <= 0.0 {
            return 0.0;
        }
        self.slip_bps_total.value(now_ms, hl) / samples
    }
}

/// Thresholds. They sit apart from the code because what counts as
/// misbehaviour changes, and changing it must not need a release.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub half_life_ms: u64,
    pub sim_failure_rate: f64,
    pub sim_min_attempts: f64,
    pub execute_failure_rate: f64,
    pub execute_min_attempts: f64,
    /// A quoter reaching this many contract violations is quarantined even if
    /// its rate looks fine.
    pub violation_count: f64,
    pub cu_share: f64,
    pub cu_budget: u64,
    pub clamp_ratio: f64,
    pub slip_bps: f64,
    pub quarantine_base_ms: u64,
    pub quarantine_max_ms: u64,
    pub throttled_sample_rate: f64,
    pub probation_sample_rate: f64,
    /// Clean observations needed to leave probation.
    pub promote_after_clean: u32,
    /// Time at full admission after which the backoff level resets, so a
    /// quoter that behaved for long enough starts its next mistake from zero.
    pub backoff_reset_ms: u64,
    pub entry_ttl_ms: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            half_life_ms: 15 * 60 * 1_000,
            sim_failure_rate: 0.05,
            sim_min_attempts: 20.0,
            execute_failure_rate: 0.05,
            execute_min_attempts: 10.0,
            violation_count: 3.0,
            cu_share: 0.25,
            cu_budget: 1_400_000,
            clamp_ratio: 0.5,
            slip_bps: 10.0,
            quarantine_base_ms: 60 * 1_000,
            quarantine_max_ms: 60 * 60 * 1_000,
            throttled_sample_rate: 0.5,
            probation_sample_rate: 0.1,
            promote_after_clean: 20,
            backoff_reset_ms: 6 * 60 * 60 * 1_000,
            entry_ttl_ms: 24 * 60 * 60 * 1_000,
        }
    }
}

/// Why a quoter's state last moved. Carried into the audit record and the
/// alert, so an operator reads the cause and not just the effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Cause {
    SimFailureRate,
    ExecuteFailureRate,
    ContractViolations,
    ComputeShare,
    PhantomDepth,
    AdverseSlip,
    ProbationFailure,
    QuarantineExpired,
    CleanStreak,
    ProgramUpgraded,
    Pinned,
    PinCleared,
}

impl Cause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SimFailureRate => "sim_failure_rate",
            Self::ExecuteFailureRate => "execute_failure_rate",
            Self::ContractViolations => "contract_violations",
            Self::ComputeShare => "compute_share",
            Self::PhantomDepth => "phantom_depth",
            Self::AdverseSlip => "adverse_slip",
            Self::ProbationFailure => "probation_failure",
            Self::QuarantineExpired => "quarantine_expired",
            Self::CleanStreak => "clean_streak",
            Self::ProgramUpgraded => "program_upgraded",
            Self::Pinned => "pinned",
            Self::PinCleared => "pin_cleared",
        }
    }
}

/// The ladder position of one quoter.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct State {
    pub admission: Admission,
    /// Doublings applied to the next quarantine.
    pub backoff_level: u32,
    pub clean_streak: u32,
    pub last_transition_ms: u64,
    pub last_success_ms: u64,
    pub last_seen_ms: u64,
    /// Deploy slot of the quoter's program when it was last checked. A change
    /// means the running code is not the code the counters describe.
    pub program_deploy_slot: u64,
    pub last_cause: Option<Cause>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            admission: Admission::Admit,
            backoff_level: 0,
            clean_streak: 0,
            last_transition_ms: 0,
            last_success_ms: 0,
            last_seen_ms: 0,
            program_deploy_slot: 0,
            last_cause: None,
        }
    }
}

impl State {
    fn quarantine_ms(&self, policy: &Policy) -> u64 {
        let doublings = self.backoff_level.min(16);
        policy
            .quarantine_base_ms
            .saturating_mul(1u64 << doublings)
            .min(policy.quarantine_max_ms)
    }

    fn enter(&mut self, now_ms: u64, admission: Admission, cause: Cause) {
        self.admission = admission;
        self.last_transition_ms = now_ms;
        self.last_cause = Some(cause);
    }

    /// Put the quoter in quarantine and make the next one longer.
    fn quarantine(&mut self, now_ms: u64, policy: &Policy, cause: Cause) {
        let until_ms = now_ms + self.quarantine_ms(policy);
        self.backoff_level = self.backoff_level.saturating_add(1);
        self.clean_streak = 0;
        self.enter(now_ms, Admission::Quarantined { until_ms }, cause);
    }
}

/// Which threshold a window breaches, worst first. `None` means clean.
fn breach(now_ms: u64, window: &Window, policy: &Policy) -> Option<Cause> {
    let hl = policy.half_life_ms;
    if window.violations.value(now_ms, hl) >= policy.violation_count {
        return Some(Cause::ContractViolations);
    }
    if window.execute_attempts.value(now_ms, hl) >= policy.execute_min_attempts
        && window.execute_failure_rate(now_ms, hl) > policy.execute_failure_rate
    {
        return Some(Cause::ExecuteFailureRate);
    }
    if window.sim_attempts.value(now_ms, hl) >= policy.sim_min_attempts
        && window.sim_failure_rate(now_ms, hl) > policy.sim_failure_rate
    {
        return Some(Cause::SimFailureRate);
    }
    if policy.cu_budget > 0
        && window.mean_cu(now_ms, hl) / policy.cu_budget as f64 > policy.cu_share
    {
        return Some(Cause::ComputeShare);
    }
    if window.clamp_ratio(now_ms, hl) > policy.clamp_ratio {
        return Some(Cause::PhantomDepth);
    }
    if window.mean_slip_bps(now_ms, hl) > policy.slip_bps {
        return Some(Cause::AdverseSlip);
    }
    None
}

/// True when a breach is severe enough to exclude rather than throttle.
fn is_exclusion(cause: Cause) -> bool {
    matches!(
        cause,
        Cause::ContractViolations | Cause::SimFailureRate | Cause::ExecuteFailureRate
    )
}

/// Advance one quoter's ladder position.
///
/// Called after every observation and on a timer, so a quarantine expires
/// even for a quoter nobody is routing to.
pub fn advance(now_ms: u64, state: &mut State, window: &Window, policy: &Policy) {
    let breached = breach(now_ms, window, policy);

    if let Admission::Quarantined { until_ms } = state.admission {
        if now_ms < until_ms {
            return;
        }
        // Quarantine expires into probation, never straight back to full
        // flow. A quoter that has not been tried since it failed has not
        // shown anything yet.
        state.clean_streak = 0;
        state.enter(
            now_ms,
            Admission::Probation {
                sample_rate: policy.probation_sample_rate,
            },
            Cause::QuarantineExpired,
        );
        return;
    }

    if let Some(cause) = breached {
        match state.admission {
            // A failure while on probation restarts the quarantine, longer.
            Admission::Probation { .. } => {
                state.quarantine(now_ms, policy, Cause::ProbationFailure)
            }
            Admission::Denied => {}
            _ if is_exclusion(cause) => state.quarantine(now_ms, policy, cause),
            Admission::Throttled { .. } => {}
            _ => state.enter(
                now_ms,
                Admission::Throttled {
                    sample_rate: policy.throttled_sample_rate,
                },
                cause,
            ),
        }
        return;
    }

    match state.admission {
        Admission::Probation { .. } | Admission::Throttled { .. } => {
            if state.clean_streak >= policy.promote_after_clean {
                state.clean_streak = 0;
                state.enter(now_ms, Admission::Admit, Cause::CleanStreak);
            }
        }
        Admission::Admit => {
            // Behaving for long enough forgets the backoff, so the next
            // mistake is not punished with the length of the last one.
            if state.backoff_level > 0
                && now_ms.saturating_sub(state.last_transition_ms) >= policy.backoff_reset_ms
            {
                state.backoff_level = 0;
            }
        }
        Admission::Quarantined { .. } | Admission::Denied => {}
    }
}

/// Note that a quoter's program was redeployed.
///
/// The counters describe code that no longer runs, so they are dropped. The
/// quoter enters probation rather than full flow. A fresh deploy has proved
/// nothing yet, and a broken rollout is the case this catches. A program
/// serving many entries moves all of them, because the upgrade changed every
/// tenant's behaviour.
pub fn on_program_upgrade(
    now_ms: u64,
    slot: u64,
    state: &mut State,
    window: &mut Window,
    policy: &Policy,
) {
    if state.program_deploy_slot == slot {
        return;
    }
    let first_sighting = state.program_deploy_slot == 0;
    state.program_deploy_slot = slot;
    if first_sighting {
        return;
    }
    window.reset();
    state.backoff_level = 0;
    state.clean_streak = 0;
    state.enter(
        now_ms,
        Admission::Probation {
            sample_rate: policy.probation_sample_rate,
        },
        Cause::ProgramUpgraded,
    );
}

/// An operator's override.
///
/// Pins live outside the scorer. The scorer never writes one and never moves
/// a quoter that has one, so clearing a pin restores automatic handling with
/// nothing to rebuild.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pin {
    pub admission: Admission,
    pub reason: String,
    pub expires_ms: Option<u64>,
    pub actor: String,
    pub set_at_ms: u64,
}

impl Pin {
    pub fn is_live(&self, now_ms: u64) -> bool {
        self.expires_ms.map(|at| now_ms < at).unwrap_or(true)
    }
}

/// One entry in the audit trail.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transition {
    /// Base58 of the registry entry. A transition is an audit record that is
    /// read and shipped, not a hot-path key, so text costs nothing here.
    pub quoter: String,
    pub at_ms: u64,
    pub from: Admission,
    pub to: Admission,
    pub cause: Cause,
    pub actor: String,
    /// The numbers that triggered it, so the record explains itself later.
    pub detail: String,
}

#[cfg(test)]
mod tests {
    use {super::*, crate::observe::FailReason};

    const MIN: u64 = 60 * 1_000;

    fn clean_sims(window: &mut Window, policy: &Policy, now_ms: u64, n: usize) {
        for _ in 0..n {
            window.record(
                now_ms,
                policy.half_life_ms,
                Observation::SimOk { cu: 10_000 },
            );
        }
    }

    #[test]
    fn a_decaying_sum_halves_over_its_half_life() {
        let mut d = Decaying::default();
        d.add(0, 1_000, 8.0);
        assert!((d.value(1_000, 1_000) - 4.0).abs() < 1e-9);
        assert!((d.value(3_000, 1_000) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_quiet_quoter_is_not_judged_on_too_few_attempts() {
        let policy = Policy::default();
        let mut window = Window::default();
        // Every attempt failed, but there were only three of them.
        for _ in 0..3 {
            window.record(
                0,
                policy.half_life_ms,
                Observation::SimFail {
                    reason: FailReason::Cpi,
                    proof: crate::observe::Attribution::Named,
                },
            );
        }
        let mut state = State::default();
        advance(0, &mut state, &window, &policy);
        assert_eq!(state.admission, Admission::Admit);
    }

    #[test]
    fn a_reverting_quoter_is_quarantined() {
        let policy = Policy::default();
        let mut window = Window::default();
        clean_sims(&mut window, &policy, 0, 30);
        for _ in 0..10 {
            window.record(
                0,
                policy.half_life_ms,
                Observation::SimFail {
                    reason: FailReason::Cpi,
                    proof: crate::observe::Attribution::Named,
                },
            );
        }
        let mut state = State::default();
        advance(0, &mut state, &window, &policy);
        assert!(matches!(state.admission, Admission::Quarantined { .. }));
        assert_eq!(state.last_cause, Some(Cause::SimFailureRate));
    }

    #[test]
    fn three_contract_violations_are_enough_on_their_own() {
        // A quoter that answers off its own quote broke the response
        // contract. A large clean denominator does not excuse it.
        let policy = Policy::default();
        let mut window = Window::default();
        clean_sims(&mut window, &policy, 0, 1_000);
        for _ in 0..3 {
            window.record(
                0,
                policy.half_life_ms,
                Observation::SimFail {
                    reason: FailReason::OffQuote,
                    proof: crate::observe::Attribution::Named,
                },
            );
        }
        let mut state = State::default();
        advance(0, &mut state, &window, &policy);
        assert_eq!(state.last_cause, Some(Cause::ContractViolations));
        assert!(matches!(state.admission, Admission::Quarantined { .. }));
    }

    #[test]
    fn a_quarantine_expires_into_probation_and_promotes_when_clean() {
        let policy = Policy::default();
        let mut window = Window::default();
        clean_sims(&mut window, &policy, 0, 30);
        for _ in 0..10 {
            window.record(
                0,
                policy.half_life_ms,
                Observation::SimFail {
                    reason: FailReason::Cpi,
                    proof: crate::observe::Attribution::Named,
                },
            );
        }
        let mut state = State::default();
        advance(0, &mut state, &window, &policy);
        let Admission::Quarantined { until_ms } = state.admission else {
            panic!("expected quarantine");
        };

        // Doing nothing is enough to get the quoter tried again.
        advance(until_ms, &mut state, &window, &policy);
        assert!(matches!(state.admission, Admission::Probation { .. }));
        assert_eq!(state.last_cause, Some(Cause::QuarantineExpired));

        // The failures have decayed and the quoter has been clean since.
        let later = until_ms + 200 * MIN;
        let mut fresh = Window::default();
        clean_sims(&mut fresh, &policy, later, 50);
        state.clean_streak = policy.promote_after_clean;
        advance(later, &mut state, &fresh, &policy);
        assert_eq!(state.admission, Admission::Admit);
        assert_eq!(state.last_cause, Some(Cause::CleanStreak));
    }

    #[test]
    fn a_failure_during_probation_doubles_the_quarantine() {
        let policy = Policy::default();
        let mut state = State {
            admission: Admission::Probation { sample_rate: 0.1 },
            backoff_level: 1,
            ..State::default()
        };
        let mut window = Window::default();
        clean_sims(&mut window, &policy, 0, 30);
        for _ in 0..10 {
            window.record(
                0,
                policy.half_life_ms,
                Observation::SimFail {
                    reason: FailReason::Cpi,
                    proof: crate::observe::Attribution::Named,
                },
            );
        }
        advance(0, &mut state, &window, &policy);
        let Admission::Quarantined { until_ms } = state.admission else {
            panic!("expected quarantine");
        };
        assert_eq!(until_ms, policy.quarantine_base_ms * 2);
        assert_eq!(state.backoff_level, 2);
        assert_eq!(state.last_cause, Some(Cause::ProbationFailure));
    }

    #[test]
    fn the_quarantine_length_is_capped() {
        let policy = Policy::default();
        let state = State {
            backoff_level: 30,
            ..State::default()
        };
        assert_eq!(state.quarantine_ms(&policy), policy.quarantine_max_ms);
    }

    #[test]
    fn behaving_for_long_enough_forgets_the_backoff() {
        let policy = Policy::default();
        let mut state = State {
            backoff_level: 4,
            last_transition_ms: 0,
            ..State::default()
        };
        let window = Window::default();
        advance(policy.backoff_reset_ms, &mut state, &window, &policy);
        assert_eq!(state.backoff_level, 0);
    }

    #[test]
    fn a_redeploy_drops_the_score_and_starts_probation() {
        // The old numbers describe code that is no longer running.
        let policy = Policy::default();
        let mut state = State {
            program_deploy_slot: 100,
            backoff_level: 3,
            ..State::default()
        };
        let mut window = Window::default();
        clean_sims(&mut window, &policy, 0, 40);
        on_program_upgrade(MIN, 200, &mut state, &mut window, &policy);
        assert!(matches!(state.admission, Admission::Probation { .. }));
        assert_eq!(state.last_cause, Some(Cause::ProgramUpgraded));
        assert_eq!(state.backoff_level, 0);
        assert_eq!(window.sim_attempts.value(MIN, policy.half_life_ms), 0.0);
    }

    #[test]
    fn first_sight_of_a_program_is_not_an_upgrade() {
        let policy = Policy::default();
        let mut state = State::default();
        let mut window = Window::default();
        clean_sims(&mut window, &policy, 0, 40);
        on_program_upgrade(MIN, 200, &mut state, &mut window, &policy);
        assert_eq!(state.admission, Admission::Admit);
        assert!(window.sim_attempts.value(MIN, policy.half_life_ms) > 0.0);
    }

    #[test]
    fn phantom_depth_throttles_rather_than_excludes() {
        // Offering depth the account cannot carry wastes a route. It does not
        // break one, so it does not earn a quarantine.
        let policy = Policy::default();
        let mut window = Window::default();
        window.record(
            0,
            policy.half_life_ms,
            Observation::DepthClamped {
                quoted_base: 1_000,
                admitted_base: 100,
            },
        );
        let mut state = State::default();
        advance(0, &mut state, &window, &policy);
        assert!(matches!(state.admission, Admission::Throttled { .. }));
        assert_eq!(state.last_cause, Some(Cause::PhantomDepth));
    }

    #[test]
    fn slip_is_measured_against_the_taker_on_both_sides() {
        let policy = Policy::default();
        let hl = policy.half_life_ms;

        // A long taker paying more than quoted is adverse.
        let mut long = Window::default();
        long.record(
            0,
            hl,
            Observation::RouteLanded {
                quoted_price: 100_000,
                executed_price: 101_000,
                taker_long: true,
            },
        );
        assert!((long.mean_slip_bps(0, hl) - 100.0).abs() < 1e-6);

        // A short taker receiving less than quoted is the same harm.
        let mut short = Window::default();
        short.record(
            0,
            hl,
            Observation::RouteLanded {
                quoted_price: 100_000,
                executed_price: 99_000,
                taker_long: false,
            },
        );
        assert!((short.mean_slip_bps(0, hl) - 100.0).abs() < 1e-6);
    }

    #[test]
    fn a_probation_quoter_may_not_carry_a_fill_alone() {
        assert!(Admission::Probation { sample_rate: 0.1 }.needs_backup());
        assert!(!Admission::Throttled { sample_rate: 0.5 }.needs_backup());
        assert!(!Admission::Admit.needs_backup());
    }

    #[test]
    fn an_expired_quarantine_stops_excluding_even_before_the_ladder_runs() {
        let a = Admission::Quarantined { until_ms: 500 };
        assert_eq!(a.sample_rate(499), 0.0);
        assert_eq!(a.sample_rate(500), 1.0);
    }

    #[test]
    fn a_live_pin_outlives_nothing_it_was_not_given() {
        let pin = Pin {
            admission: Admission::Denied,
            reason: "manual".into(),
            expires_ms: Some(100),
            actor: "operator".into(),
            set_at_ms: 0,
        };
        assert!(pin.is_live(99));
        assert!(!pin.is_live(100));
    }
}
