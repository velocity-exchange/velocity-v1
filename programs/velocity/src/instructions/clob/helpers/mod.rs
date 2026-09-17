//! Shared plumbing for the CLOB instruction domain. No endpoints live here.
//! A file directly in `instructions/clob/` is an instruction, with its relay
//! resolver. A file here is called by several of them.
//!
//! - [`placement`]: the placement helpers. They run the margin gate and the
//!   aggregate reserve, then CPI to the CLOB as its `place_authority`.
//! - [`records`]: the `OrderRecord` and `OrderActionRecord` emitters for book
//!   orders, which have no `User.orders` slot of their own.
//! - [`crank_common`]: the cranks' shared dual-mode plumbing, and the accounts
//!   contexts that the removal cranks and their resolvers share.

pub mod crank_common;
pub mod placement;
pub mod records;

pub use {crank_common::*, placement::*, records::*};
