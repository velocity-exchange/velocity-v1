//! Create a market's [`QuoterSlabV0`]: the one account that holds every
//! approved quoter config for that market. Permissionless — the payer buys
//! rent on an all-vacant slab, and only the approval flow writes slots — so
//! any client can ensure it exists before asking the admin to approve.
//!
//! `capacity` sizes the slot region at the account's tail. It is a rent
//! decision, not a protocol bound: a market that outgrows its slab grows the
//! account (`extend_quoter_slab`), and the zeroed new bytes are more vacant
//! slots.

use {
    crate::{
        error::ErrorCode,
        state::{
            perp_market::PerpMarket,
            prop_amm::{QuoterSlabV0, QUOTER_SLAB_PDA_SEED},
        },
        validate,
    },
    anchor_lang::prelude::*,
};

/// Ceiling on the creation size: the runtime lets one call allocate at most
/// 10,240 bytes, which holds 13 slots after the header. A slab grows past it
/// with `extend_quoter_slab`.
const MAX_INITIAL_CAPACITY: u16 = 13;

#[derive(Accounts)]
#[instruction(market_index: u16, capacity: u16)]
pub struct InitializeQuoterSlab<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// Existence check: a slab serves a market that exists.
    #[account(
        seeds = [b"perp_market", market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    #[account(
        init,
        seeds = [QUOTER_SLAB_PDA_SEED, market_index.to_le_bytes().as_ref()],
        space = QuoterSlabV0::space(capacity as usize),
        bump,
        payer = payer
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

pub fn handle_initialize_quoter_slab(
    ctx: Context<InitializeQuoterSlab>,
    market_index: u16,
    capacity: u16,
) -> Result<()> {
    validate!(
        (1..=MAX_INITIAL_CAPACITY).contains(&capacity),
        ErrorCode::InvalidQuoterConfig,
        "slab capacity {} is outside 1..={}",
        capacity,
        MAX_INITIAL_CAPACITY
    )?;
    let mut slab = ctx.accounts.quoter_slab.load_init()?;
    slab.market = market_index;
    slab.capacity = capacity;
    Ok(())
}
