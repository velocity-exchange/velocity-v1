//! Anchor instruction handlers: account constraints, deserialization, and delegation to `controller`.
//! `user.rs` = trading (orders, deposits, LP positions).
//! `keeper.rs` = crank instructions (funding updates, PnL settlement, liquidations, fills).
//! `admin.rs` = governance (market init/update, oracle config, fees, insurance).
//! AMM-specific admin ixs (repeg, update_k, recenter, spread/jit config, fee-pool plumbing) live in `crate::vlp::amm::admin`.
//! LP-pool management ixs live in `crate::vlp::hedge::{admin, instructions}`.
//! `constraints.rs` = shared Anchor account constraint helpers.

pub use {
    crate::vlp::{
        amm::admin::*,
        hedge::{admin::*, instructions::*, settle::*},
    },
    account_extension::*,
    admin::*,
    clob::*,
    constraints::*,
    if_staker::*,
    keeper::*,
    protocol_fees::*,
    pyth_lazer_oracle::*,
    quoter_registry::*,
    router::*,
    user::*,
};

mod account_extension;
mod admin;
mod clob;
pub mod constraints;
mod if_staker;
mod keeper;
pub mod optional_accounts;
mod protocol_fees;
mod pyth_lazer_oracle;
mod quoter_registry;
mod router;
mod user;
