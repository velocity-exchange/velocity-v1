//! Admin vetting gate. Approval validates the entry is coherent enough to
//! CPI: non-empty account lists on both legs, each containing the response
//! account (the router reads responses from it, so it must be forwarded).

use anchor_lang::prelude::*;

use crate::auth::check_warm;
use crate::error::ErrorCode;
use crate::state::prop_amm::QuoterV0;
use crate::state::state::State;
use crate::validate;

#[derive(Accounts)]
pub struct UpdateQuoterApproved<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub quoter: AccountLoader<'info, QuoterV0>,
}

pub fn handle_update_quoter_approved(
    ctx: Context<UpdateQuoterApproved>,
    approved: bool,
) -> Result<()> {
    let mut quoter = ctx.accounts.quoter.load_mut()?;
    if approved {
        for (list, count) in [
            (&quoter.quote_accounts, quoter.quote_accounts_count),
            (&quoter.execute_accounts, quoter.execute_accounts_count),
        ] {
            validate!(
                count > 0,
                ErrorCode::InvalidQuoterConfig,
                "cannot approve a quoter with an empty account list"
            )?;
            validate!(
                list[..count as usize]
                    .iter()
                    .any(|meta| meta.pubkey == quoter.response_account),
                ErrorCode::InvalidQuoterConfig,
                "response account must be registered in both CPI account lists"
            )?;
        }
    }
    quoter.is_approved = approved;
    Ok(())
}
