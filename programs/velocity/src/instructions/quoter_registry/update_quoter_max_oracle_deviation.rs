//! Declare (or clear) how far from oracle a quoter's fills may price.
//!
//! Velocity bounds every external leg by the market's own band. That band is
//! sized for the market, not for one maker's risk appetite, so a maker that
//! wants a tighter one asks for it here. It caps what the maker's own program
//! can lose if that program is compromised.
//!
//! Unlike the rest of the entry's config this does not reset `is_approved`.
//! The band applies as the smaller of the declaration and the market's, so no
//! value it can hold is wider than the one the admin vetted, and a maker
//! tightening it during an incident must not wait for re-vetting.

use {
    crate::{
        error::ErrorCode,
        math::constants::MARGIN_PRECISION,
        msg,
        state::prop_amm::{QuoterType, QuoterV0},
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateQuoterMaxOracleDeviation<'info> {
    /// The entry's own authority — the quoted user's wallet for Custom
    /// entries.
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority)]
    pub quoter: AccountLoader<'info, QuoterV0>,
}

/// `max_oracle_deviation_bps` is in MARGIN_PRECISION units, so one unit is one
/// basis point. Zero clears the declaration and the market's own band stands.
pub fn handle_update_quoter_max_oracle_deviation(
    ctx: Context<UpdateQuoterMaxOracleDeviation>,
    max_oracle_deviation_bps: u32,
) -> Result<()> {
    let mut quoter = ctx.accounts.quoter.load_mut()?;
    // Custom entries only. A book fills third parties, and its entry authority
    // is whoever registered it, so a band on a book would let that authority
    // revert other people's fills.
    validate!(
        quoter.quoter_type == QuoterType::Custom,
        ErrorCode::InvalidQuoterConfig,
        "an oracle band is for Custom quoters; a book's makers are bounded by the market's"
    )?;
    validate!(
        max_oracle_deviation_bps < MARGIN_PRECISION,
        ErrorCode::InvalidQuoterConfig,
        "an oracle band of {} is at or past 100%; the market's band already bounds that",
        max_oracle_deviation_bps
    )?;
    quoter.max_oracle_deviation_bps = max_oracle_deviation_bps;
    msg!(
        "quoter {} fills within {} bps of oracle, or the market's band if that is tighter",
        ctx.accounts.quoter.key(),
        max_oracle_deviation_bps
    );
    Ok(())
}
