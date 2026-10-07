//! Create the program's shared resolver staging account.
//!
//! One account serves the whole program, and creating it is permissionless.
//! There is nothing to configure, and an attacker gains nothing by creating it
//! first. Every resolver needs it to exist before it can stage anything.
//! Whoever runs this pays the rent once.
//!
//! The chain never reads its contents. A resolver writes into it under
//! simulation and a turner reads the result out of the simulated post-state.
//! Nothing here is ever committed, so two turners that simulate at once cannot
//! interfere.

use {
    crate::state::relay_scratch::{RelayScratchV0, RELAY_SCRATCH_PDA_SEED},
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct InitializeRelayScratch<'info> {
    #[account(
        init,
        seeds = [RELAY_SCRATCH_PDA_SEED],
        space = RelayScratchV0::SIZE,
        bump,
        payer = payer
    )]
    pub scratch: AccountLoader<'info, RelayScratchV0>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

pub fn handle_initialize_relay_scratch(ctx: Context<InitializeRelayScratch>) -> Result<()> {
    // `init` zeroes the data and `load_init` writes the discriminator. A
    // zeroed scratch region is already valid.
    ctx.accounts.scratch.load_init()?;
    Ok(())
}
