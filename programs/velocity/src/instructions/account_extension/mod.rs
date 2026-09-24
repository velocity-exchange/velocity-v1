//! Account-size migration for zero-copy accounts.
//!
//! A program upgrade that appends fields to a zero-copy struct leaves every
//! existing account at the old, smaller size. The loader then fails its
//! `size_of` slice on first touch. `extend_account` grows such an account to
//! the size the deployed program compiles in for its type. The payer tops up
//! the rent, the runtime zero-fills the new tail, and the account loads again.
//!
//! The `AccountExtension` hot role gates the crank, and the warm and cold
//! admins are also accepted. A larger account raises fetch bandwidth and any
//! future per-byte transaction cost, so the protocol decides when the growth
//! happens. An account already at the target size is a no-op, so a repeated
//! crank is harmless and a batch never fails wholesale.
//!
//! `extend_account_devnet` grows an account to an arbitrary larger size. It
//! exercises the flow before a real struct extension exists. Production
//! mainnet builds compile it out. See `docs/ACCOUNT-EXTENSION.md`.

mod extend_account;
#[cfg(any(feature = "anchor-test", not(feature = "mainnet-beta")))]
mod extend_account_devnet;

pub use extend_account::*;
#[cfg(any(feature = "anchor-test", not(feature = "mainnet-beta")))]
pub use extend_account_devnet::*;
