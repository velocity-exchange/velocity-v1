//! Write a quoter's registered CPI account list. One call replaces the unified
//! list and both index lists. Each index list says which of the quoter's
//! accounts a leg forwards, in CPI order. Replacing all three together stops a
//! leg from pointing past the list it was written with.
//!
//! The write is staging only. The approved copy in the market's slab keeps
//! serving until the admin copies again. A `Custom` entry answers to its own
//! stored authority. A book's entry answers to the State admin roles.

use {
    crate::{
        error::ErrorCode,
        instructions::quoter_registry::check_quoter_config_authority,
        state::{
            prop_amm::{validate_quoter_accounts, QuoterV0, MAX_QUOTER_ACCOUNTS},
            state::State,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateQuoterAccounts<'info> {
    pub authority: Signer<'info>,
    #[account(mut)]
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// Read for the admin check that a non-`Custom` entry needs. A `Custom`
    /// entry answers to its own stored authority and omits this account.
    pub state: Option<AccountLoader<'info, State>>,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct QuoterAccountMetaArg {
    pub pubkey: Pubkey,
    pub is_writable: bool,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct UpdateQuoterAccountsArgs {
    /// The unified registered list. It replaces the stored list whole.
    pub metas: Vec<QuoterAccountMetaArg>,
    /// Indexes into `metas` forwarded to `quote_v0` / `quote_l3_v0`, in CPI
    /// order.
    pub quote_indexes: Vec<u8>,
    /// Indexes into `metas` forwarded to `execute_v0`, in CPI order.
    pub execute_indexes: Vec<u8>,
}

pub fn handle_update_quoter_accounts(
    ctx: Context<UpdateQuoterAccounts>,
    args: UpdateQuoterAccountsArgs,
) -> Result<()> {
    check_quoter_config_authority(
        &ctx.accounts.quoter.load()?.config,
        &ctx.accounts.authority.key(),
        ctx.accounts.state.as_ref(),
    )?;

    let mut quoter = ctx.accounts.quoter.load_mut()?;
    let config = &mut quoter.config;

    validate!(
        args.metas.len() <= MAX_QUOTER_ACCOUNTS,
        ErrorCode::InvalidQuoterConfig,
        "{} quoter accounts exceeds the capacity of {}",
        args.metas.len(),
        MAX_QUOTER_ACCOUNTS
    )?;
    validate_quoter_accounts(
        args.metas
            .iter()
            .map(|meta| (&meta.pubkey, meta.is_writable)),
        config.market,
    )?;

    for (name, indexes) in [
        ("quote", &args.quote_indexes),
        ("execute", &args.execute_indexes),
    ] {
        validate!(
            indexes.len() <= MAX_QUOTER_ACCOUNTS,
            ErrorCode::InvalidQuoterConfig,
            "{} {} leg indexes exceeds the capacity of {}",
            indexes.len(),
            name,
            MAX_QUOTER_ACCOUNTS
        )?;
        validate!(
            indexes.iter().all(|&i| (i as usize) < args.metas.len()),
            ErrorCode::InvalidQuoterConfig,
            "a {} leg index points past the registered list",
            name
        )?;
    }

    config.accounts = Default::default();
    for (slot, meta) in config.accounts.iter_mut().zip(&args.metas) {
        slot.pubkey = meta.pubkey;
        slot.is_writable = meta.is_writable;
    }

    config.accounts_count = args.metas.len() as u8;

    config.quote_account_indexes = Default::default();
    config.quote_account_indexes[..args.quote_indexes.len()].copy_from_slice(&args.quote_indexes);
    config.quote_accounts_count = args.quote_indexes.len() as u8;

    config.execute_account_indexes = Default::default();
    config.execute_account_indexes[..args.execute_indexes.len()]
        .copy_from_slice(&args.execute_indexes);
    config.execute_accounts_count = args.execute_indexes.len() as u8;

    Ok(())
}
