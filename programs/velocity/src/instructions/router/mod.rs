//! Router fill for the PropAMM order flow: the CPI-backed execute leg the
//! fill entrypoint threads into the fill controller.

pub mod cpi_executor;
pub mod initialize_router_quote_buffer;
pub mod quote_router;

pub use {cpi_executor::*, initialize_router_quote_buffer::*, quote_router::*};
