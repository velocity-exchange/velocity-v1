//! Declare or clear a quoter's reprice-watch region. The region is the account
//! bytes whose change means the quoter may quote differently now. A midpoint
//! declares its mid region, and a custom AMM declares its parameter block.
//! Relay cross-discovery conditions wake on a write to the region.
//!
//! The maker declares the region, because only the maker knows the layout of
//! their program. The region is registry metadata, so velocity holds no
//! per-program code. The write is staging only. It goes live once the admin
//! copies it into the market's slab.

use {
    crate::{
        error::ErrorCode,
        instructions::quoter_registry::check_quoter_config_authority,
        state::{
            prop_amm::{QuoterType, QuoterV0},
            state::State,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateQuoterWatch<'info> {
    /// The entry's own authority. For a `Custom` entry that is the quoted
    /// user's wallet.
    pub authority: Signer<'info>,
    #[account(mut)]
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// CHECK: the account whose bytes the watch covers. It is usually the
    /// quoter's own state account. Nothing else constrains it. The admin
    /// reviews it, and a wrong watch only costs the maker latency.
    pub watch_account: UncheckedAccount<'info>,
    /// Read for the admin check that a non-`Custom` entry needs. A `Custom`
    /// entry answers to its own stored authority and omits this account.
    pub state: Option<AccountLoader<'info, State>>,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct UpdateQuoterWatchArgs {
    pub watch_offset: u32,
    /// Zero clears the declaration. Discovery then polls.
    pub watch_len: u32,
}

pub fn handle_update_quoter_watch(
    ctx: Context<UpdateQuoterWatch>,
    args: UpdateQuoterWatchArgs,
) -> Result<()> {
    check_quoter_config_authority(
        &ctx.accounts.quoter.load()?.config,
        &ctx.accounts.authority.key(),
        ctx.accounts.state.as_ref(),
    )?;
    let mut quoter = ctx.accounts.quoter.load_mut()?;
    let config = &mut quoter.config;
    validate!(
        config.quoter_type == QuoterType::Custom,
        ErrorCode::InvalidQuoterConfig,
        "watch declarations are for Custom quoters (the CLOB's cross watch is built in)"
    )?;
    config.watch_account = if args.watch_len == 0 {
        Pubkey::default()
    } else {
        ctx.accounts.watch_account.key()
    };
    config.watch_offset = args.watch_offset;
    config.watch_len = args.watch_len;
    Ok(())
}
