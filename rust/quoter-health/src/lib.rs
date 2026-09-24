//! Quoter health for an off-chain router.
//!
//! PropAMM quoters are arbitrary programs that the on-chain router invokes by
//! CPI. A quoter can revert, answer off its own quote, consume the whole
//! compute budget, or offer depth it cannot carry. The on-chain router cannot
//! refuse any of that. It quotes whichever registry entries the transaction
//! carries, and one failing entry reverts the whole fill.
//!
//! The off-chain router chooses which entries the transaction carries, so
//! leaving a quoter out is enough to avoid it. This crate holds the evidence
//! that choice needs, and the state machine that lets the choice reverse.
//!
//! Observation starts at the simulate call, not on chain. A router simulates
//! before it sends, so a quoter that reverts fails the simulation and the
//! transaction never lands. No log is archived and no counter moves. Only the
//! process that held the simulate call can see the failure.
//!
//! A quoter is charged only when the evidence names it. A simulation carries
//! several quoters, and it fails for reasons that belong to none of them.
//! Unproven failures count against the router instead. A rising unattributed
//! rate means attribution has a hole, not that a maker got worse.
//!
//! The evidence splits in two. Velocity names the entry whenever it refuses
//! an answer, which covers the contract violations. A quoter that never
//! answers ends velocity's instruction before it can log, so only the
//! runtime's CPI frame survives. That frame names a program rather than an
//! entry. It yields a suspect, and a re-simulation without that suspect turns
//! the suspicion into proof.
//!
//! Every automatic exclusion expires, so degradation reverses on its own. An
//! operator's pin lives in a layer the scorer cannot write, so clearing a pin
//! restores automatic behaviour with nothing lost.

pub mod metrics;
pub mod observe;
pub mod parse;
pub mod score;
pub mod store;

pub use {
    observe::{Attribution, FailReason, Observation, Report, Unattributed},
    parse::{attribute, Charge, EntryRef, RouteContext, Verdict},
    score::{advance, Admission, Cause, Pin, Policy, State, Transition, Window},
    store::{now_ms, Health, Snapshot},
};
