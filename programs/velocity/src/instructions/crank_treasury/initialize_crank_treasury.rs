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
    // Born unpriced: a zero target refills nothing and a zero payment offers
    // no keeper anything, so the treasury is inert until it is priced. That
    // keeps creating it separate from deciding what it spends.
    ctx.accounts.treasury.load_init()?;
    Ok(())
}
