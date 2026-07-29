//! Router fill for the PropAMM order flow: the CPI-backed execute leg the
//! fill entrypoint threads into the fill controller.

pub mod cpi_executor;

pub use cpi_executor::*;
