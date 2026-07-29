//! Account-size migration for zero-copy accounts.
//!
//! When a program upgrade appends fields to a zero-copy account struct, every
//! existing on-chain account is still the old, smaller size and fails the
//! loader's `size_of` slice on first touch. `extend_account` is the migration
//! crank that grows such an account to the size the deployed program compiles
//! in for its type: the payer tops up rent, the runtime zero-fills the new
//! tail, and the account becomes loadable again. Gated on the
//! `AccountExtension` hot role (warm/cold fallback): growing accounts inflates
//! fetch bandwidth and any future per-byte transaction costs, so the protocol
//! decides when it happens. Already-at-size accounts are a no-op so the crank
//! is idempotent and batches never fail wholesale.
//! `extend_account_devnet` additionally grows an account to an arbitrary
//! larger size so the flow can be exercised before a real extension exists;
//! it is compiled out of mainnet builds. See `docs/ACCOUNT-EXTENSION.md`.
//!
//! One file per instruction: account context at the top, handler below.

mod extend_account;
#[cfg(not(feature = "mainnet-beta"))]
mod extend_account_devnet;

pub use extend_account::*;
#[cfg(not(feature = "mainnet-beta"))]
pub use extend_account_devnet::*;
