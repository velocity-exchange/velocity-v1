//! Quoter registry lifecycle (see `state::prop_amm`). For Custom quoters the
//! quoted user's authority creates the entry — creation is consent — and
//! keeps a permanent kill switch (`is_active`). Nothing fills until the
//! admin vets the CPI surface (`is_approved`), and a config change clears that
//! approval for re-vetting. One field is exempt: the oracle band a maker
//! declares can only tighten a bound the admin already vetted, so declaring it
//! does not send the entry back for approval.

pub mod initialize_quoter;
pub mod update_quoter_accounts;
pub mod update_quoter_active;
pub mod update_quoter_approved;
pub mod update_quoter_config;
pub mod update_quoter_max_oracle_deviation;
pub mod update_quoter_priority;
pub mod update_quoter_watch;

pub use {
    initialize_quoter::*, update_quoter_accounts::*, update_quoter_active::*,
    update_quoter_approved::*, update_quoter_config::*, update_quoter_max_oracle_deviation::*,
    update_quoter_priority::*, update_quoter_watch::*,
};
