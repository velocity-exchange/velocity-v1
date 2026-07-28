//! Admin-set routing priority (lower fills first, pro rata within a tier).
//! Admin-only — a maker choosing their own priority could jump the vAMM and
//! CLOB in the fill waterfall.

use anchor_lang::prelude::*;

use crate::auth::check_warm;
use crate::state::prop_amm::QuoterV0;
use crate::state::state::State;

#[derive(Accounts)]
pub struct UpdateQuoterPriority<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub quoter: AccountLoader<'info, QuoterV0>,
}

pub fn handle_update_quoter_priority(
    ctx: Context<UpdateQuoterPriority>,
    priority: u8,
) -> Result<()> {
    ctx.accounts.quoter.load_mut()?.priority = priority;
    Ok(())
}
