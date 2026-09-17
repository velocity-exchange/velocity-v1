//! Quoter registry lifecycle. The entry types live in `crate::state::prop_amm`.
//!
//! The quoted user's authority creates a `Custom` staging entry, so creation is
//! the consent. That authority keeps `is_active` as a kill switch for the life
//! of the entry. Every other entry answers to the State admin roles. The key
//! that registered it holds nothing. See [`check_quoter_config_authority`].
//!
//! No entry fills until the admin copies the staged config into the market's
//! `QuoterSlabV0`. See `update_quoter_approved`. A later staging edit stays
//! inert until the admin copies again. The approved copy keeps serving.
//!
//! Three fields write through to the approved copy with no new approval.
//! `is_active` must take effect at once. The admin sets `priority`. The oracle
//! band can only tighten a bound the admin approved.

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
/// `is_active` is that maker's kill switch. Velocity never moves the key and
/// offers no handoff.
///
/// Every other entry belongs to the protocol. Its stored key is only the admin
/// key that signed the registration, so the State admin roles decide instead.
/// After an admin key rotation the new key controls the entry, and the key it
/// replaced controls nothing.
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
