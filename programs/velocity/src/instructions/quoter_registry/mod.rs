//! Quoter registry lifecycle (see `state::prop_amm`). For Custom quoters the
//! quoted user's authority creates the staging entry — creation is consent —
//! and keeps a permanent kill switch (`is_active`, written through to the
//! approved copy). Nothing fills until the admin copies the staged config
//! into the market's `QuoterSlabV0` (`update_quoter_approved`); a later
//! staging edit stays inert until the admin copies again, while the vetted
//! copy keeps serving. Three fields write through without re-vetting:
//! `is_active` (the kill switch must land at once), `priority` (admin-set),
//! and the oracle band (it can only tighten a bound the admin vetted).

pub mod initialize_quoter;
pub mod initialize_quoter_slab;
pub mod update_quoter_accounts;
pub mod update_quoter_active;
pub mod update_quoter_approved;
pub mod update_quoter_config;
pub mod update_quoter_max_oracle_deviation;
pub mod update_quoter_priority;
pub mod update_quoter_watch;

pub use {
    initialize_quoter::*, initialize_quoter_slab::*, update_quoter_accounts::*,
    update_quoter_active::*, update_quoter_approved::*, update_quoter_config::*,
    update_quoter_max_oracle_deviation::*, update_quoter_priority::*, update_quoter_watch::*,
};
