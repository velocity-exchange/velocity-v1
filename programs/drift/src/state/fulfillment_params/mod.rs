//! Fulfillment parameter types for routing spot orders to external DEX venues
//! (Drift AMM, OpenBook V2). Each variant carries the venue-specific accounts needed at fill time.

pub mod drift;
pub mod openbook_v2;
