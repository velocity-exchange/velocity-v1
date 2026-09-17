//! Quoter registry: [`QuoterV0`] staging entries and the per-market
//! [`QuoterSlabV0`] of approved configs.
//!
//! A config names an external quoter program and the CPI surface velocity
//! needs to call it: the discriminators, one registered account list with
//! per-leg index lists, and the response account. A quoter is a CLOB, the
//! midpoint, or a custom PropAMM. The vAMM is in-program and holds no entry.
//!
//! One flat namespace, four files by concern:
//!
//! - [`registry`] holds the config and its staging entry: [`QuoterType`],
//!   [`QuoterConfigV0`], [`QuoterV0`], and the reserved-key check that
//!   registration and approval share.
//! - [`slab`] holds the approved set: [`QuoterSlabV0`], its slot region, and
//!   the accessors every fill and crank resolves slots through.
//! - [`wire`] holds the generic quoter CPI every source shares:
//!   [`QuoterConfigV0::quote_in_place`], [`QuoterConfigV0::execute`], the
//!   request and response shapes `quoter-spec` declares, and the executor the
//!   router fill drives.
//! - [`clob`] holds the velocity-mediated CLOB surface: [`ClobMarket`] and
//!   [`ClobReader`], the book's wire shapes, and the aggregate unwinds a
//!   removal drives.
//!
//! Every CPI out of this module signs as velocity's one external-CPI identity,
//! which is the market's quoter slab. See `crate::signer`. No CPI here signs
//! as the vault authority.

mod clob;
mod registry;
mod slab;
mod wire;

#[cfg(test)]
mod tests;

pub use {clob::*, registry::*, slab::*, wire::*};
