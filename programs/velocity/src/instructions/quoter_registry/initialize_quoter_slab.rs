//! Create a market's [`QuoterSlabV0`]: the one account that holds every
//! approved quoter config for that market. Permissionless — the payer buys
//! rent on an all-vacant slab, and only the approval flow writes slots — so
//! any client can ensure it exists before asking the admin to approve.
//!
//! The slab is born at one slot and stays right-sized from then on: approval
//! grows the account by exactly the slot it needs, and revocation gives
//! trailing vacancy back (`update_quoter_approved`). A fixed creation
//! capacity would either overpay rent or under-provision, and every reader
//! pays compute per declared slot.

use {
    crate::state::{
        perp_market::PerpMarket,
        prop_amm::{QuoterSlabV0, QUOTER_SLAB_PDA_SEED},
    },
    anchor_lang::prelude::*,
};

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct InitializeQuoterSlabArgs {
    pub market_index: u16,
}

#[derive(Accounts)]
#[instruction(args: InitializeQuoterSlabArgs)]
pub struct InitializeQuoterSlab<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// Existence check: a slab serves a market that exists. The market
    /// stored the slab PDA at its own initialization, so the `has_one` holds
    /// before the slab account exists.
    #[account(
        seeds = [b"perp_market", args.market_index.to_le_bytes().as_ref()],
        bump,
        has_one = quoter_slab
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    #[account(
        init,
        seeds = [QUOTER_SLAB_PDA_SEED, args.market_index.to_le_bytes().as_ref()],
        space = QuoterSlabV0::space(1),
        bump,
        payer = payer
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

pub fn handle_initialize_quoter_slab(
    ctx: Context<InitializeQuoterSlab>,
    args: InitializeQuoterSlabArgs,
) -> Result<()> {
    let mut slab = ctx.accounts.quoter_slab.load_init()?;
    slab.market = args.market_index;
    slab.capacity = 1;
    // The slab signs every external quoter CPI for this market; store the
    // bump once so the hot paths never derive it.
    slab.bump = ctx.bumps.quoter_slab;
    Ok(())
}
