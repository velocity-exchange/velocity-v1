//! Router fill for the PropAMM order flow: the CPI-backed execute leg the
//! fill entrypoint threads into the fill controller, plus the test-build
//! CU/wire probe for the quoter CPI legs (which dies once the router fill
//! covers its ground end to end in integration tests).

pub mod cpi_executor;
#[cfg(feature = "anchor-test")]
pub mod probe_router;

pub use cpi_executor::*;
#[cfg(feature = "anchor-test")]
pub use probe_router::*;
