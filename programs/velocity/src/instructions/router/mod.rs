//! Router fill (phase 2 of the PropAMM order-flow work). Currently only the
//! test-build CU/wire probe for the quoter CPI legs; the real fill ix
//! (quote CPIs → split → clamp → execute CPIs → apply balance changes)
//! grows here and the probe dies when it lands.

pub mod probe_router;

pub use probe_router::*;
