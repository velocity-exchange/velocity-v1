//! Admin-set routing priority (lower fills first, pro rata within a tier).
//! Admin-only — a maker choosing their own priority could jump the vAMM and
//! CLOB in the fill waterfall. Writes through to the approved copy in the
//! market's slab, so the new tier applies without a re-approval.

use {
    crate::{
        auth::check_warm,
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
pub struct UpdateQuoterPriority<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// The market's slab. Optional for an entry that was never approved.
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

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct UpdateQuoterPriorityArgs {
    /// Routing tier at a shared price: lower fills first, pro rata within.
    pub priority: u8,
}

pub fn handle_update_quoter_priority(
    ctx: Context<UpdateQuoterPriority>,
    args: UpdateQuoterPriorityArgs,
) -> Result<()> {
    let UpdateQuoterPriorityArgs { priority } = args;
    ctx.accounts.quoter.load_mut()?.config.priority = priority;
    if let Some(slab) = &ctx.accounts.quoter_slab {
        let mut slots = slab.slots_mut()?;
        if let Some(index) = slot_for_entry(&slots, &ctx.accounts.quoter.key()) {
            slots[index].config.priority = priority;
        }
    }
    Ok(())
}
