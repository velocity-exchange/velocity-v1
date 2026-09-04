//! Declare (or clear) a quoter's reprice-watch region: the account bytes
//! whose change means "this quoter may quote differently now" — a
//! midpoint's mid region, a custom AMM's parameter block. Relay
//! cross-discovery conditions wake on it. Maker-declared because only the
//! maker knows their program's layout; generic because it is registry
//! metadata, not per-program velocity code. A config change like any other:
//! staging only, live once the admin copies it into the market's slab.

use {
    crate::{
        error::ErrorCode,
        state::prop_amm::{QuoterType, QuoterV0},
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateQuoterWatch<'info> {
    /// The entry's own authority — the quoted user's wallet for Custom
    /// entries.
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = quoter.load()?.config.authority == authority.key() @ ErrorCode::InvalidQuoterAuthority
    )]
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// CHECK: the account whose bytes the watch covers — typically the
    /// quoter's own state account; not otherwise constrained (the admin
    /// vets it, and a wrong watch only costs the maker latency).
    pub watch_account: UncheckedAccount<'info>,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct UpdateQuoterWatchArgs {
    pub watch_offset: u32,
    /// 0 clears the declaration (poll-only discovery).
    pub watch_len: u32,
}

pub fn handle_update_quoter_watch(
    ctx: Context<UpdateQuoterWatch>,
    args: UpdateQuoterWatchArgs,
) -> Result<()> {
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
