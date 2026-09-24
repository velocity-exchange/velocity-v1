//! Host-side unit tests for the book and the response encoder.
//!
//! These run under plain `cargo test` (no SVM, no `.so`) against a
//! `ClobMarketV0` backed by a stack account buffer — see [`market`]. They
//! cover the pieces the litesvm suite can't reach from outside: index
//! validation, the free-list guards, the post-operation invariants, that the
//! streamed response bytes are exactly the wincode encoding of the declared
//! response types, and that the stack-buffer event path emits exactly what
//! anchor's `Event::data()` would.

mod book;
mod config;
mod emit;
mod market;
mod parity;
mod randomized;
mod response;
mod taker_origin;
mod wire;

/// A slot past every activation slot these tests place at, so a cancel is
/// never refused for a taker-origin order still inside its window. A test that
/// exercises that binding passes its own slot instead.
pub(crate) const ACTIVE_SLOT: u64 = u64::MAX;
