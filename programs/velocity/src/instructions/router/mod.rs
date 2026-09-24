//! Router fill for the PropAMM order flow. These modules size a fill's
//! counterparties, quote the market's approved quoters, and commit what the
//! router allocates to them through CPI. [`route_fill`] runs those steps and
//! the perp fill for every entrypoint that fills through the router.

pub mod cpi_executor;
pub mod initialize_router_quote_buffer;
pub mod quote_router;
pub mod quoted_route;
pub mod route_fill;
pub mod user_caps;

pub use {
    cpi_executor::*, initialize_router_quote_buffer::*, quote_router::*, quoted_route::*,
    route_fill::*, user_caps::*,
};
