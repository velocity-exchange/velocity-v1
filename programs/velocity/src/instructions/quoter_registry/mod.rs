//! Quoter registry lifecycle (see `state::prop_amm`). For Custom quoters the
//! quoted user's authority creates the staging entry — creation is consent —
//! and keeps a permanent kill switch (`is_active`, written through to the
//! approved copy). A book's entry answers to the State admin roles instead of
//! to the key that registered it; see [`check_quoter_config_authority`].
//! Nothing fills until the admin copies the staged config
//! into the market's `QuoterSlabV0` (`update_quoter_approved`); a later
//! staging edit stays inert until the admin copies again, while the vetted
//! copy keeps serving. Three fields write through without re-vetting:
//! `is_active` (the kill switch must land at once), `priority` (admin-set),
//! and the oracle band (it can only tighten a bound the admin vetted).

use {
    crate::{
        auth::check_warm,
        error::ErrorCode,
        msg,
        state::{
            prop_amm::{QuoterConfigV0, QuoterType},
            state::State,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

/// Check that `signer` may write this entry's staging config.
///
/// A `Custom` entry answers to the key stored at its creation, for the whole
/// life of the entry. That key is the quoted user's own authority, and
/// `is_active` on the entry is that maker's kill switch, so velocity never
/// moves the key and offers no handoff.
///
/// Every other entry is the protocol's own. Its stored key is only the admin
/// key that signed the registration, so the State admin roles decide instead.
/// A rotated admin key then still holds the market book slot, and the key it
/// replaced holds nothing.
pub fn check_quoter_config_authority(
    config: &QuoterConfigV0,
    signer: &Pubkey,
    state: Option<&AccountLoader<State>>,
) -> Result<()> {
    if config.quoter_type == QuoterType::Custom {
        validate!(
            config.authority == *signer,
            ErrorCode::InvalidQuoterAuthority,
            "quoter entry answers to {}",
            config.authority
        )?;
        return Ok(());
    }
    let state = state.ok_or_else(|| {
        msg!(
            "a {:?} entry answers to the admin roles",
            config.quoter_type
        );
        error!(ErrorCode::InvalidQuoterAuthority)
    })?;
    validate!(
        check_warm(signer, state)?,
        ErrorCode::InvalidQuoterAuthority,
        "only the admin may configure a {:?} quoter",
        config.quoter_type
    )?;
    Ok(())
}

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
