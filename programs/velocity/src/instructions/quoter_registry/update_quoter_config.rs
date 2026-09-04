//! Update a quoter's scalar CPI config (response account, discriminators).
//! Staging only: the approved copy in the market's slab keeps serving its
//! vetted config until the admin copies again. No authority handoff: for
//! Custom quoters the authority is the quoted user's authority by
//! construction, which is what guarantees the maker's kill switch.

use {
    crate::{error::ErrorCode, state::prop_amm::QuoterV0},
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateQuoterConfig<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = quoter.load()?.config.authority == authority.key() @ ErrorCode::InvalidQuoterAuthority
    )]
    pub quoter: AccountLoader<'info, QuoterV0>,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct UpdateQuoterConfigArgs {
    pub response_account: Option<Pubkey>,
    pub quote_v0_discriminator: Option<[u8; 8]>,
    /// Set to all-zero to withdraw the leg.
    pub quote_l3_v0_discriminator: Option<[u8; 8]>,
    pub execute_v0_discriminator: Option<[u8; 8]>,
}

pub fn handle_update_quoter_config(
    ctx: Context<UpdateQuoterConfig>,
    args: UpdateQuoterConfigArgs,
) -> Result<()> {
    let mut quoter = ctx.accounts.quoter.load_mut()?;
    let config = &mut quoter.config;
    if let Some(response_account) = args.response_account {
        config.response_account = response_account;
    }
    if let Some(discriminator) = args.quote_l3_v0_discriminator {
        config.quote_l3_v0_discriminator = discriminator;
    }
    if let Some(discriminator) = args.quote_v0_discriminator {
        config.quote_v0_discriminator = discriminator;
    }
    if let Some(discriminator) = args.execute_v0_discriminator {
        config.execute_v0_discriminator = discriminator;
    }
    Ok(())
}
