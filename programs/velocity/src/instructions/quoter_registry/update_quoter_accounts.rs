//! Write a quoter's registered CPI account list: one unified list, plus the
//! index lists that say which of its accounts each leg forwards, in CPI
//! order. One call replaces all three, so a leg can never point past the
//! list it was written with. Staging only: the approved copy in the market's
//! slab keeps serving its vetted config until the admin copies again.

use {
    crate::{
        error::ErrorCode,
        state::prop_amm::{validate_quoter_accounts, QuoterV0, MAX_QUOTER_ACCOUNTS},
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateQuoterAccounts<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = quoter.load()?.config.authority == authority.key() @ ErrorCode::InvalidQuoterAuthority
    )]
    pub quoter: AccountLoader<'info, QuoterV0>,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct QuoterAccountMetaArg {
    pub pubkey: Pubkey,
    pub is_writable: bool,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct UpdateQuoterAccountsArgs {
    /// The unified registered list, replacing the stored one whole.
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
    let mut quoter = ctx.accounts.quoter.load_mut()?;
    let config = &mut quoter.config;

    validate!(
        args.metas.len() <= MAX_QUOTER_ACCOUNTS,
        ErrorCode::InvalidQuoterConfig,
        "{} quoter accounts exceeds the capacity of {}",
        args.metas.len(),
        MAX_QUOTER_ACCOUNTS
    )?;
    validate_quoter_accounts(args.metas.iter().map(|meta| &meta.pubkey))?;
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
