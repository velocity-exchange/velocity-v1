//! Update a quoter's scalar CPI config, which is the response account and the
//! leg discriminators. The write is staging only. The approved copy in the
//! market's slab keeps serving until the admin copies again. A `Custom` entry
//! answers to the quoted user's authority and offers no handoff, which is what
//! guarantees the maker's kill switch. A book's entry answers to the State
//! admin roles.

use {
    crate::{
        instructions::quoter_registry::check_quoter_config_authority,
        state::{prop_amm::QuoterV0, state::State},
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateQuoterConfig<'info> {
    pub authority: Signer<'info>,
    #[account(mut)]
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// Read for the admin check that a non-`Custom` entry needs. A `Custom`
    /// entry answers to its own stored authority and omits this account.
    pub state: Option<AccountLoader<'info, State>>,
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
    check_quoter_config_authority(
        &ctx.accounts.quoter.load()?.config,
        &ctx.accounts.authority.key(),
        ctx.accounts.state.as_ref(),
    )?;
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
