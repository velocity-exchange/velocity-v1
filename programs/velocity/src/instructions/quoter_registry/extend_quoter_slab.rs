//! Grow a market's [`QuoterSlabV0`] slot region. Permissionless like
//! creation: the payer buys the extra rent, the runtime zeroes the new bytes
//! (which is what a vacant slot is), and the header's `capacity` moves up so
//! readers see the new slots. Occupied slots never move, so every stored slot
//! index stays valid.
//!
//! One call can grow an account by at most 10,240 bytes — 13 slots — which is
//! the same runtime bound that caps the creation size. A larger target takes
//! several calls.

use {
    crate::{
        error::ErrorCode,
        state::prop_amm::{QuoterSlabV0, QUOTER_SLAB_PDA_SEED},
        validate,
    },
    anchor_lang::prelude::*,
};

/// Ceiling on a slab's total capacity. Far above any plausible roster; it
/// exists so the account cannot be grown without bound.
const MAX_TOTAL_CAPACITY: u16 = 128;

#[derive(Accounts)]
#[instruction(market_index: u16)]
pub struct ExtendQuoterSlab<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(
        mut,
        seeds = [QUOTER_SLAB_PDA_SEED, market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    pub system_program: Program<'info, System>,
}

pub fn handle_extend_quoter_slab(
    ctx: Context<ExtendQuoterSlab>,
    _market_index: u16,
    capacity: u16,
) -> Result<()> {
    let current = ctx.accounts.quoter_slab.load()?.capacity;
    validate!(
        capacity > current,
        ErrorCode::InvalidQuoterConfig,
        "slab already holds {} slots; {} does not grow it",
        current,
        capacity
    )?;
    validate!(
        capacity <= MAX_TOTAL_CAPACITY,
        ErrorCode::InvalidQuoterConfig,
        "slab capacity {} is above the ceiling of {}",
        capacity,
        MAX_TOTAL_CAPACITY
    )?;

    let info = ctx.accounts.quoter_slab.to_account_info();
    let new_space = QuoterSlabV0::space(capacity as usize);
    validate!(
        new_space.saturating_sub(info.data_len()) <= 10_240,
        ErrorCode::InvalidQuoterConfig,
        "one call can grow the slab by at most 13 slots; ask again for the rest"
    )?;

    // Rent first: a resize that leaves the account under the new minimum
    // fails the transaction at its end.
    let required = Rent::get()?.minimum_balance(new_space);
    let shortfall = required.saturating_sub(info.lamports());
    if shortfall > 0 {
        anchor_lang::system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                anchor_lang::system_program::Transfer {
                    from: ctx.accounts.payer.to_account_info(),
                    to: info.clone(),
                },
            ),
            shortfall,
        )?;
    }
    // `resize` zero-fills the added tail, and zeroed new bytes are vacant
    // slots.
    info.resize(new_space).map_err(Into::<Error>::into)?;
    ctx.accounts.quoter_slab.load_mut()?.capacity = capacity;
    Ok(())
}
