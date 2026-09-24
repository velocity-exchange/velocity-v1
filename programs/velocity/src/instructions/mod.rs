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
    liq_relay::*,
    protocol_fees::*,
    pyth_lazer_oracle::*,
    quoter_registry::*,
    relay_harness::*,
    router::*,
    trigger_relay::*,
    user::*,
};

mod account_extension;
mod admin;
mod clob;
pub mod constraints;
pub mod crank_treasury;
mod if_staker;
pub mod initialize_relay_scratch;
mod keeper;
mod liq_relay;
pub mod optional_accounts;
mod protocol_fees;
mod pyth_lazer_oracle;
mod quoter_registry;
pub mod relay_harness;
mod router;
pub mod sync_user_conditions;
mod trigger_relay;
mod user;

pub use {crank_treasury::*, initialize_relay_scratch::*, sync_user_conditions::*};
