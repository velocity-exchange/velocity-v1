//! The maker's own on/off switch. Always available to the entry authority —
//! for Custom quoters that is the quoted user's authority (enforced at
//! creation, no handoff), so a maker can always shut their quoter down.

use anchor_lang::prelude::*;

use crate::error::ErrorCode;
use crate::state::prop_amm::QuoterV0;

#[derive(Accounts)]
pub struct UpdateQuoterActive<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = quoter.load()?.authority == authority.key() @ ErrorCode::InvalidQuoterAuthority
    )]
    pub quoter: AccountLoader<'info, QuoterV0>,
}

pub fn handle_update_quoter_active(ctx: Context<UpdateQuoterActive>, active: bool) -> Result<()> {
    ctx.accounts.quoter.load_mut()?.is_active = active;
    Ok(())
}
