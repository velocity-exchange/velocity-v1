//! Create the treasury account.

use {
    crate::{
        auth::check_warm,
        state::{
            crank_treasury::{CrankTreasuryV0, CRANK_TREASURY_PDA_SEED},
            state::State,
        },
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct InitializeCrankTreasury<'info> {
    #[account(
        init,
        seeds = [CRANK_TREASURY_PDA_SEED],
        space = CrankTreasuryV0::SIZE,
        bump,
        payer = admin
    )]
    pub treasury: AccountLoader<'info, CrankTreasuryV0>,
    #[account(mut, constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

pub fn handle_initialize_crank_treasury(ctx: Context<InitializeCrankTreasury>) -> Result<()> {
    // The treasury is born unpriced. A zero target stages no refill, and a
    // zero watermark never wakes one, so the treasury does nothing until an
    // admin prices it. Creating the account stays separate from deciding what
    // it spends.
    ctx.accounts.treasury.load_init()?;
    Ok(())
}
