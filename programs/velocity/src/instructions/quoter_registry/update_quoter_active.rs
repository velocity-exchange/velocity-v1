//! The maker's own on/off switch. Always available to the entry authority —
//! for Custom quoters that is the quoted user's authority (enforced at
//! creation, no handoff), so a maker can always shut their quoter down. A
//! book's entry answers to the State admin roles instead, because its stored
//! authority is only the admin key that registered it. Writes through to the
//! approved copy in the market's slab, so a kill takes effect at once rather
//! than at the next approval.

use {
    crate::{
        instructions::quoter_registry::check_quoter_config_authority,
        state::{
            prop_amm::{
                slot_for_entry, QuoterSlabExt, QuoterSlabV0, QuoterV0, QUOTER_SLAB_PDA_SEED,
            },
            state::State,
        },
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateQuoterActive<'info> {
    pub authority: Signer<'info>,
    #[account(mut)]
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
    /// Read for the admin check a non-Custom entry needs. Absent for a
    /// Custom entry, which answers to its own stored authority.
    pub state: Option<AccountLoader<'info, State>>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct UpdateQuoterActiveArgs {
    pub active: bool,
}

pub fn handle_update_quoter_active(
    ctx: Context<UpdateQuoterActive>,
    args: UpdateQuoterActiveArgs,
) -> Result<()> {
    let UpdateQuoterActiveArgs { active } = args;
    check_quoter_config_authority(
        &ctx.accounts.quoter.load()?.config,
        &ctx.accounts.authority.key(),
        ctx.accounts.state.as_ref(),
    )?;
    ctx.accounts.quoter.load_mut()?.config.is_active = active;
    if let Some(slab) = &ctx.accounts.quoter_slab {
        let mut slots = slab.slots_mut()?;
        if let Some(index) = slot_for_entry(&slots, &ctx.accounts.quoter.key()) {
            slots[index].config.is_active = active;
        }
    }
    Ok(())
}
