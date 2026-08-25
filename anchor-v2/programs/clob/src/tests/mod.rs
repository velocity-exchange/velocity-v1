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
mod emit;
mod market;
mod response;
mod taker_origin;
