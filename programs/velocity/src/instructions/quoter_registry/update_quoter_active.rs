//! The maker's own on/off switch. Always available to the entry authority —
//! for Custom quoters that is the quoted user's authority (enforced at
//! creation, no handoff), so a maker can always shut their quoter down.
//! Writes through to the approved copy in the market's slab, so a kill takes
//! effect at once rather than at the next approval.

use {
    crate::{
        error::ErrorCode,
        state::prop_amm::{
            quoter_slab_slots_mut, slot_for_entry, QuoterSlabV0, QuoterV0, QUOTER_SLAB_PDA_SEED,
        },
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateQuoterActive<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = quoter.load()?.config.authority == authority.key() @ ErrorCode::InvalidQuoterAuthority
    )]
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// The market's slab, so the switch reaches the approved copy. Optional:
    /// an entry that was never approved has no copy to write. Omitting it on
    /// an approved entry leaves the live copy as it was — the staging value
    /// still lands at the next approval — so a maker flipping the live
    /// switch passes it.
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

pub fn handle_update_quoter_active(ctx: Context<UpdateQuoterActive>, active: bool) -> Result<()> {
    ctx.accounts.quoter.load_mut()?.config.is_active = active;
    if let Some(slab) = &ctx.accounts.quoter_slab {
        let mut slots = quoter_slab_slots_mut(slab)?;
        if let Some(index) = slot_for_entry(&slots, &ctx.accounts.quoter.key()) {
            slots[index].config.is_active = active;
        }
    }
    Ok(())
}
