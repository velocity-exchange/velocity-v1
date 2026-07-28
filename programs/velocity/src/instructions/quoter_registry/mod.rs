//! Quoter registry lifecycle (see `state::prop_amm`). For Custom quoters the
//! quoted user's authority creates the entry — creation is consent — and
//! keeps a permanent kill switch (`is_active`). Nothing fills until the
//! admin vets the CPI surface (`is_approved`), and any config change clears
//! that approval for re-vetting.

pub mod initialize_quoter;
pub mod update_quoter_accounts;
pub mod update_quoter_active;
pub mod update_quoter_approved;
pub mod update_quoter_config;

pub use initialize_quoter::*;
pub use update_quoter_accounts::*;
pub use update_quoter_active::*;
pub use update_quoter_approved::*;
pub use update_quoter_config::*;
