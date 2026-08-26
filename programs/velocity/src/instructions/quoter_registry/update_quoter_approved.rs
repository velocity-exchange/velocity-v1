//! Admin vetting gate. Approval validates the entry is coherent enough to
//! CPI: non-empty account lists on both legs, each containing the response
//! account (the router reads responses from it, so it must be forwarded), and
//! no reserved key on either list.
//!
//! Approval also requires the program behind the entry to be frozen. Approving
//! an upgradeable program approves its *author*, not its code: the holder of
//! the upgrade authority can replace the binary the moment approval lands, and
//! everything else velocity does to bound a quoter — the quoted-price binding,
//! the subject rule, the margin check after the fill — is bounded in turn by
//! what that binary does with the accounts it is handed. A frozen program is
//! the only version of "the admin vetted this" that survives the next slot.
//!
//! The cost is that a maker who wants to change their quoter deploys a new
//! program and comes back for approval. That is the intended shape: a new
//! binary is a new thing to vet.

use {
    crate::{
        auth::check_warm,
        error::ErrorCode,
        state::{
            prop_amm::{validate_quoter_accounts, QuoterV0},
            state::State,
        },
        validate,
    },
    anchor_lang::{prelude::*, solana_program::bpf_loader_upgradeable},
};

#[derive(Accounts)]
pub struct UpdateQuoterApproved<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// CHECK: locked to the entry's registered program. Read for its loader
    /// and, through it, whether the code can still change.
    #[account(address = quoter.load()?.program_id)]
    pub quoter_program: UncheckedAccount<'info>,
    /// CHECK: validated as `quoter_program`'s program-data account in the
    /// handler. Optional because revoking approval needs none of this, and a
    /// program on a loader that cannot upgrade in place has no such account.
    pub quoter_program_data: Option<UncheckedAccount<'info>>,
}

/// Offsets into a `ProgramData` account: a four-byte enum tag, the slot it was
/// last deployed at, then the upgrade authority behind an option tag.
const PROGRAM_DATA_TAG: [u8; 4] = [3, 0, 0, 0];
const UPGRADE_AUTHORITY_OPTION_TAG: usize = 4 + 8;

/// The code behind `program` can no longer change.
fn require_frozen_program(
    program: &UncheckedAccount,
    program_data: Option<&UncheckedAccount>,
) -> Result<()> {
    if program.owner != &bpf_loader_upgradeable::ID {
        // Any other loader writes the program once. There is nothing to
        // freeze, and no program-data account to read.
        return Ok(());
    }
    let program_data = program_data.ok_or_else(|| {
        msg!("approving an upgradeable program requires its program-data account");
        error!(ErrorCode::InvalidQuoterConfig)
    })?;
    let (expected, _) =
        Pubkey::find_program_address(&[program.key.as_ref()], &bpf_loader_upgradeable::ID);
    validate!(
        program_data.key() == expected,
        ErrorCode::InvalidQuoterConfig,
        "program data {} is not {}'s",
        program_data.key(),
        program.key()
    )?;
    validate!(
        program_data.owner == &bpf_loader_upgradeable::ID,
        ErrorCode::InvalidQuoterConfig,
        "program data is not owned by the upgradeable loader"
    )?;
    let data = program_data
        .try_borrow_data()
        .map_err(|_| error!(ErrorCode::InvalidQuoterConfig))?;
    validate!(
        data.len() > UPGRADE_AUTHORITY_OPTION_TAG && data[..4] == PROGRAM_DATA_TAG,
        ErrorCode::InvalidQuoterConfig,
        "program data account does not hold program data"
    )?;
    // A `None` authority is a program nobody can redeploy.
    validate!(
        data[UPGRADE_AUTHORITY_OPTION_TAG] == 0,
        ErrorCode::InvalidQuoterConfig,
        "quoter program {} still has an upgrade authority",
        program.key()
    )?;
    Ok(())
}

pub fn handle_update_quoter_approved(
    ctx: Context<UpdateQuoterApproved>,
    approved: bool,
) -> Result<()> {
    let mut quoter = ctx.accounts.quoter.load_mut()?;
    if approved {
        require_frozen_program(
            &ctx.accounts.quoter_program,
            ctx.accounts.quoter_program_data.as_ref(),
        )?;
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
            // Re-checked here, not only at write time: a list stored before
            // the reserved-key check existed is still on chain, and approval
            // is the gate that lets an entry take flow.
            validate_quoter_accounts(list[..count as usize].iter().map(|meta| &meta.pubkey))?;
        }
    }
    quoter.is_approved = approved;
    Ok(())
}
