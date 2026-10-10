//! Code shared by the keeper bots
//!
//! The tx worker that sends, simulates and confirms every bot's transactions, the gRPC
//! subscription and account sync, the pyth-lazer feed and the oracle projection the program
//! gates read, the collateral reservations of txs that take on a position, the metrics and
//! health endpoints, and the `Keeper` handles every pass takes.

pub mod collateral;
pub mod grpc;
pub mod keeper;
pub mod metrics;
pub mod oracle;
pub mod tx;
