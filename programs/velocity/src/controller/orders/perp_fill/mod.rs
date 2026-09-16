//! Filling one perp order, in three layers.
//!
//! Each layer has its own subject, and each takes the layer below as a step
//! rather than wrapping it:
//!
//! * [`order`] governs **the order** — where it lives, whether the market and
//!   the taker admit a fill of it, and what the fill leaves behind.
//! * [`taker_risk`] governs **the taker's risk limits** — the gates the fill
//!   must pass before it moves anything, and the checks both seats are held to
//!   after it does.
//! * [`liquidity`] governs **liquidity** — quote, split, execute, settle.
//!
//! [`context`] holds the vocabulary all three speak: the account bundles, the
//! exchange rules, and the market-oracle reading.

mod context;
mod liquidity;
mod order;
mod taker_risk;

/// The layer-test seam: a test that drives the taker layer directly builds
/// these itself.
#[cfg(test)]
pub use context::{FillConditions, OfferedLiquidity};
pub use {
    context::{FillAmounts, FillParties, TakerRefs},
    order::{
        fill_perp_order, fill_perp_order_without_external_books, FillRequest, FillTarget,
        PerpFillAccounts,
    },
    taker_risk::{fill_within_taker_risk_limits, TakerRiskLimits},
};
