//! Declare (or clear) how far from oracle a quoter's fills may price.
//!
//! Velocity bounds every external leg by the market's own band. That band is
//! sized for the market, not for one maker's risk appetite, so a maker that
//! wants a tighter one asks for it here. It caps what the maker's own program
//! can lose if that program is compromised.
//!
//! Unlike the rest of the entry's config this writes through to the approved
//! copy in the market's slab without re-vetting. The band applies as the
//! smaller of the declaration and the market's, so no value it can hold is
//! wider than the one the admin vetted, and a maker tightening it during an
//! incident must not wait.

use {
    crate::{
        error::ErrorCode,
        math::constants::MARGIN_PRECISION,
        msg,
        state::prop_amm::{
            quoter_slab_slots_mut, slot_for_entry, QuoterSlabV0, QuoterType, QuoterV0,
            QUOTER_SLAB_PDA_SEED,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateQuoterMaxOracleDeviation<'info> {
    /// The entry's own authority — the quoted user's wallet for Custom
    /// entries.
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = quoter.load()?.config.authority == authority.key() @ ErrorCode::InvalidQuoterAuthority
    )]
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// The market's slab. Optional for an entry that was never approved;
    /// omitting it on an approved entry leaves the live band as it was.
    #[account(
        mut,
        seeds = [
            QUOTER_SLAB_PDA_SEED,
            quoter.load()?.config.market.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub quoter_slab: Option<AccountLoader<'info, QuoterSlabV0>>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct UpdateQuoterMaxOracleDeviationArgs {
    /// In MARGIN_PRECISION units, so one unit is one basis point. Zero clears
    /// the declaration and the market's own band stands.
    pub max_oracle_deviation_bps: u32,
}

pub fn handle_update_quoter_max_oracle_deviation(
    ctx: Context<UpdateQuoterMaxOracleDeviation>,
    args: UpdateQuoterMaxOracleDeviationArgs,
) -> Result<()> {
    let UpdateQuoterMaxOracleDeviationArgs {
        max_oracle_deviation_bps,
    } = args;
    let mut quoter = ctx.accounts.quoter.load_mut()?;
    // Custom entries only. A book fills third parties, and its entry authority
    // is whoever registered it, so a band on a book would let that authority
    // revert other people's fills.
    validate!(
        quoter.config.quoter_type == QuoterType::Custom,
        ErrorCode::InvalidQuoterConfig,
        "an oracle band is for Custom quoters; a book's makers are bounded by the market's"
    )?;
    validate!(
        max_oracle_deviation_bps < MARGIN_PRECISION,
        ErrorCode::InvalidQuoterConfig,
        "an oracle band of {} is at or past 100%; the market's band already bounds that",
        max_oracle_deviation_bps
    )?;
    quoter.config.max_oracle_deviation_bps = max_oracle_deviation_bps;
    drop(quoter);
    if let Some(slab) = &ctx.accounts.quoter_slab {
        let mut slots = quoter_slab_slots_mut(slab)?;
        if let Some(index) = slot_for_entry(&slots, &ctx.accounts.quoter.key()) {
            slots[index].config.max_oracle_deviation_bps = max_oracle_deviation_bps;
        }
    }
    msg!(
        "quoter {} fills within {} bps of oracle, or the market's band if that is tighter",
        ctx.accounts.quoter.key(),
        max_oracle_deviation_bps
    );
    Ok(())
}
