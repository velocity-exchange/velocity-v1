//! Create a market's [`QuoterSlabV0`]. That one account holds every approved
//! quoter config for the market. The call is permissionless, because the payer
//! buys rent on an all-vacant slab and only the approval flow writes slots. Any
//! client can make sure the slab exists before it asks the admin to approve.
//!
//! The slab is born with one slot and stays right-sized. Approval grows the
//! account by exactly the slot it needs, and revocation returns trailing
//! vacancy. See `update_quoter_approved`. A fixed creation capacity would
//! either overpay rent or run out of slots, and every reader pays compute per
//! declared slot.

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
    /// Existence check, so a slab serves a market that exists. The market
    /// stored the slab PDA at its own initialization, so `has_one` holds
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
    // The slab signs every external quoter CPI for this market. Store the bump
    // once so the hot paths never derive it.
    slab.bump = ctx.bumps.quoter_slab;
    Ok(())
}
