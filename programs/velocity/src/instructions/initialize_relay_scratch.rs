//! Create the program's shared resolver staging account.
//!
//! One account for the whole program, so it is permissionless and
//! idempotent: there is nothing to configure, nothing an attacker gains by
//! creating it first, and every resolver needs it to exist before it can
//! stage anything. Whoever runs it pays the rent once.
//!
//! Its contents are never read on chain. Resolvers write into it under
//! simulation and turners read the result out of the simulated
//! post-state, so nothing here is ever committed and two turners
//! simulating at once cannot interfere.

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
    // `init` zeroes it, which is all a scratch region needs to be valid.
    ctx.accounts.scratch.load_init()?;
    Ok(())
}
