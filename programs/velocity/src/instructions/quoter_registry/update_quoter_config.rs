//! Update a quoter's scalar CPI config (response account, discriminators).
//! Clears `is_approved` — the admin re-vets. No authority handoff: for
//! Custom quoters the authority is the quoted user's authority by
//! construction, which is what guarantees the maker's kill switch.

use anchor_lang::prelude::*;

use crate::error::ErrorCode;
use crate::state::prop_amm::QuoterV0;

#[derive(Accounts)]
pub struct UpdateQuoterConfig<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = quoter.load()?.authority == authority.key() @ ErrorCode::InvalidQuoterAuthority
    )]
    pub quoter: AccountLoader<'info, QuoterV0>,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct UpdateQuoterConfigArgs {
    pub response_account: Option<Pubkey>,
    pub quote_v0_discriminator: Option<[u8; 8]>,
    pub execute_v0_discriminator: Option<[u8; 8]>,
}

pub fn handle_update_quoter_config(
    ctx: Context<UpdateQuoterConfig>,
    args: UpdateQuoterConfigArgs,
) -> Result<()> {
    let mut quoter = ctx.accounts.quoter.load_mut()?;
    if let Some(response_account) = args.response_account {
        quoter.response_account = response_account;
    }
    if let Some(discriminator) = args.quote_v0_discriminator {
        quoter.quote_v0_discriminator = discriminator;
    }
    if let Some(discriminator) = args.execute_v0_discriminator {
        quoter.execute_v0_discriminator = discriminator;
    }
    quoter.is_approved = false;
    Ok(())
}
