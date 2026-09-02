//! Shared plumbing for the CLOB instruction domain — no endpoints live here.
//! A file directly in `instructions/clob/` is an instruction (with its relay
//! resolver); a file here is called by several of them.
//!
//! - [`placement`]: the placement helpers — margin gate + aggregate reserve,
//!   then the CPI to the CLOB as its `place_authority`.
//! - [`records`]: the `OrderRecord`/`OrderActionRecord` emitters for book
//!   orders, which have no `User.orders` slot of their own.
//! - [`crank_common`]: the cranks' shared dual-mode plumbing and the accounts
//!   contexts the removal cranks and resolvers share.

pub mod crank_common;
pub mod placement;
pub mod records;

pub use {crank_common::*, placement::*, records::*};
