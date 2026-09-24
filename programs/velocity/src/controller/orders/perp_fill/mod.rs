//! Filling one perp order, in three layers.
//!
//! Each layer has its own subject. Each one runs the layer below as a step.
//!
//! * [`order`] governs the order. It finds the order, decides whether the
//!   market and the taker admit a fill of it, and applies the bookkeeping the
//!   fill leaves behind.
//! * [`taker_risk`] governs the taker's risk limits. It runs the gates the
//!   fill must pass before it moves anything, and the checks both seats are
//!   held to after it does.
//! * [`liquidity`] governs liquidity. It quotes, splits, executes and
//!   settles.
//!
//! [`context`] holds the types all three use: the account bundles, the
//! conditions one fill runs under, and the market oracle reading.

mod context;
mod liquidity;
mod order;
mod taker_risk;

#[cfg(test)]
mod tests;

/// A test that drives the taker layer directly builds these itself.
#[cfg(test)]
pub use context::{FillConditions, OfferedLiquidity};
#[cfg(test)]
pub(crate) use order::fill_perp_order_without_external_books;
pub use {
    context::{FillAmounts, FillParties, MakerFill, MakerFills, TakerRefs},
    order::{fill_perp_order, FillRequest, PerpFillAccounts},
    taker_risk::{fill_within_taker_risk_limits, TakerRiskLimits},
};
