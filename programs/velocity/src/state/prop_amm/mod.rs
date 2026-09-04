//! Quoter registry: [`QuoterV0`] staging entries and the per-market
//! [`QuoterSlabV0`] of approved configs name an external quoter program
//! (CLOB, Midpoint, custom PropAMMs; the vAMM is in-program) plus the CPI
//! surface velocity needs to call it — discriminators, one registered account
//! list with per-leg index lists, and the response account.
//!
//! One flat namespace, four files by concern:
//!
//! - [`registry`] — the config and its staging entry: [`QuoterType`],
//!   [`QuoterConfigV0`], [`QuoterV0`], and the reserved-key check
//!   registration and approval share.
//! - [`slab`] — the approved set: [`QuoterSlabV0`], its slot region, and the
//!   accessors every fill and crank resolves slots through.
//! - [`wire`] — the generic quoter CPI every source shares:
//!   [`QuoterConfigV0::quote`]/[`QuoterConfigV0::execute`], the request and
//!   response shapes `quoter-spec` declares, and the executor the router
//!   fill drives.
//! - [`clob`] — the velocity-mediated CLOB surface: [`ClobMarket`] /
//!   [`ClobReader`], the book's wire shapes, and the aggregate unwinds a
//!   removal drives.
//!
//! Every CPI out of this module signs as one of velocity's two external-CPI
//! identities — the book's `clob_authority`, or the entry's own
//! `quoter_signer` (see `crate::signer`), never as the vault authority.

mod clob;
mod registry;
mod slab;
mod wire;

#[cfg(test)]
mod tests;

pub use {clob::*, registry::*, slab::*, wire::*};
