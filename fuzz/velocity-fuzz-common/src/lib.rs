//! Shared fixtures and invariant assertions for the velocity Crucible fuzz
//! harnesses.
//!
//! Two roles:
//!  1. Re-export `velocity` and small `..Default::default()`-based fixture
//!     builders so every harness constructs the same realistic state.
//!  2. Provide the reusable `invariants` assertions (families I–VIII of the
//!     campaign plan) that the SVM harnesses call after every action.
//!
//! This is a minimal skeleton proving the velocity(host lib) + crucible
//! integration links; builders/invariants are fleshed out per-package.

pub use velocity;

#[cfg(feature = "clob")]
pub mod clob;

/// Reusable SVM-tier invariant assertions. These read on-chain state from a
/// live `TestContext` and reconcile it against the protocol's core invariants.
/// Stubbed here; the reconciliation logic lands in the foundation before fan-out.
pub mod invariants {
    use crucible_test_context::TestContext;

    /// Family II — global quote conservation: Σ vault tokens == Σ user quote
    /// balances + pnl/fee pools + revenue pool + IF vault.
    pub fn assert_global_quote_conservation(_ctx: &TestContext) {
        // TODO(foundation): read all spot vaults + user/market accounts and reconcile.
    }
}
