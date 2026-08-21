//! Quoter health for an off-chain router.
//!
//! PropAMM quoters are arbitrary programs the on-chain router invokes by CPI.
//! A quoter can revert, answer off its own quote, burn the transaction's
//! compute budget, or offer depth it cannot carry. The on-chain router has no
//! defence: it quotes whichever registry entries the transaction carries, and
//! one failing entry reverts the whole fill.
//!
//! The defence is the router's, and the router is off chain. It decides which
//! entries the transaction carries, so excluding a quoter is enough to route
//! around it. This crate holds the evidence a router needs to make that
//! decision, and the state machine that lets the decision reverse itself.
//!
//! Observation starts at the simulate call, not on chain. A router simulates
//! before it sends, so a quoter that reverts fails the simulation and the
//! transaction never lands. Nothing is archived and no counter moves. The
//! process holding the simulate call is the only one that can see it.
//!
//! Attribution is positive. A simulation carrying several quoters fails for
//! reasons that belong to nobody, so a quoter is charged only when the
//! evidence names it. Unproven failures count against the router instead, and
//! a rising unattributed rate reports a hole in attribution rather than a bad
//! maker.
//!
//! The evidence splits in two. Velocity names the entry whenever it refuses
//! an answer, which covers the contract violations. A quoter that never
//! answers — one that reverts, or exhausts the compute budget — ends
//! velocity's instruction before it can log, so only the runtime's CPI frame
//! survives. That frame names a program rather than an entry, so it yields a
//! suspect that a re-simulation without it turns into proof.
//!
//! Degradation is reversible by construction. Every automatic exclusion
//! expires, and the operator's pins live in a layer the scorer cannot touch,
//! so clearing a pin restores automatic behaviour with nothing lost.

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
