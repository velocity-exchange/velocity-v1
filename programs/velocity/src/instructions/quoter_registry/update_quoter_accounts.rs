//! Write a slice of a quoter's registered CPI account list. Chunked by
//! `index` so a full 32-entry list never has to fit in one transaction.
//! Clears `is_approved` — the CPI surface changed, the admin re-vets.

use {
    crate::{
        error::ErrorCode,
        state::prop_amm::{validate_quoter_accounts, QuoterCpiLeg, QuoterV0, MAX_QUOTER_ACCOUNTS},
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateQuoterAccounts<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = quoter.load()?.authority == authority.key() @ ErrorCode::InvalidQuoterAuthority
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
    pub leg: QuoterCpiLeg,
    /// Slot in the registered list this slice starts at.
    pub index: u8,
    pub metas: Vec<QuoterAccountMetaArg>,
}

pub fn handle_update_quoter_accounts(
    ctx: Context<UpdateQuoterAccounts>,
    args: UpdateQuoterAccountsArgs,
) -> Result<()> {
    let mut quoter = ctx.accounts.quoter.load_mut()?;
    let quoter = &mut *quoter;

    let end = (args.index as usize)
        .checked_add(args.metas.len())
        .ok_or(ErrorCode::InvalidQuoterConfig)?;
    validate!(
        end <= MAX_QUOTER_ACCOUNTS,
        ErrorCode::InvalidQuoterConfig,
        "quoter account list slice [{}, {}) exceeds capacity {}",
        args.index,
        end,
        MAX_QUOTER_ACCOUNTS
    )?;
    validate_quoter_accounts(args.metas.iter().map(|meta| &meta.pubkey))?;

    let (list, count) = match args.leg {
        QuoterCpiLeg::Quote => (&mut quoter.quote_accounts, &mut quoter.quote_accounts_count),
        QuoterCpiLeg::Execute => (
            &mut quoter.execute_accounts,
            &mut quoter.execute_accounts_count,
        ),
    };
    for (slot, meta) in list[args.index as usize..end].iter_mut().zip(&args.metas) {
        slot.pubkey = meta.pubkey;
        slot.is_writable = meta.is_writable;
    }
    // The list is exactly [0, end): a slice write is also a truncation, so a
    // shrinking update can't leave stale live entries past its end.
    *count = end as u8;

    quoter.is_approved = false;
    Ok(())
}
