//! Price the treasury: what a refill fills to, and what it pays its keeper.

use {
    crate::{
        auth::check_warm,
        error::ErrorCode,
        state::{
            crank_treasury::{CrankTreasuryV0, CRANK_TREASURY_PDA_SEED},
            state::State,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateCrankTreasury<'info> {
    #[account(mut, seeds = [CRANK_TREASURY_PDA_SEED], bump)]
    pub treasury: AccountLoader<'info, CrankTreasuryV0>,
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
}

/// Both levels are counted in cranks rather than lamports, so one setting
/// serves every market: a market whose cranks cost more carries a
/// proportionally larger float.
///
/// The target is read at refill time and so reaches every market at once. The
/// watermark is resolved to lamports and written onto a market at attach,
/// because it is the threshold that market's wake condition carries, so a new
/// watermark reaches a market on its next attach.
///
/// What a refill *pays* is not set here. It is priced from the network rails
/// like every other crank and stored on the market whose reservoir it fills,
/// because that is where the condition advertising it lives.
pub fn handle_update_crank_treasury(
    ctx: Context<UpdateCrankTreasury>,
    refill_target_cranks: u16,
    refill_watermark_cranks: u16,
) -> Result<()> {
    validate!(
        refill_watermark_cranks > 0,
        ErrorCode::DefaultError,
        "a zero watermark never wakes a refill"
    )?;
    // Strictly above the level that wakes the refill. A target at the
    // watermark leaves the reservoir still due the moment it is filled, and
    // the condition would stay lit against an executor that can only revert.
    validate!(
        refill_target_cranks > refill_watermark_cranks,
        ErrorCode::DefaultError,
        "refill target of {} cranks must exceed the {} that wakes it",
        refill_target_cranks,
        refill_watermark_cranks
    )?;
    let mut treasury = ctx.accounts.treasury.load_mut()?;
    treasury.refill_target_cranks = refill_target_cranks;
    treasury.refill_watermark_cranks = refill_watermark_cranks;
    msg!(
        "crank treasury wakes a refill under {} cranks and fills to {}; markets take a new \
         watermark on their next attach",
        refill_watermark_cranks,
        refill_target_cranks
    );
    Ok(())
}
