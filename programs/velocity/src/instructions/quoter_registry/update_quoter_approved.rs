//! Admin vetting gate. Approval validates the entry is coherent enough to
//! CPI: non-empty account lists on both legs, each containing the response
//! account (the router reads responses from it, so it must be forwarded), and
//! no reserved key on either list.
//!
//! Approval does not require a frozen program, and does not freeze one. A maker
//! may upgrade the program behind an approved entry. Three things make that
//! acceptable, and the first is the one that matters:
//!
//! 1. A `Custom` entry can move only its own registered user, at a price held
//!    to its own quote and to the taker's limit price, sized inside its own
//!    margin, with every touched account margin-checked after the fill. An
//!    upgrade can therefore lose the maker's money and cannot take anyone
//!    else's.
//! 2. An entry that quotes and does not deliver stops being routed to: fillers
//!    choose which entries to carry, and a taker's signed route names its own.
//! 3. The admin can set `is_approved` false at any time, and the maker holds
//!    `is_active` as well.
//!
//! Requiring a frozen program would buy little against that. It closes only
//! "honest at approval, hostile later", while the same behaviour can ship in
//! the binary that gets approved — and no practical review of a compiled
//! program catches a quoter that sometimes returns nothing. It would cost a
//! maker every bug fix, because a redeploy is a new program id and therefore a
//! new registry entry.
//!
//! What approval does instead is record the slot the program was deployed at.
//! An upgrade then shows up as a changed slot, so a reader knows the code moved
//! without having to infer it from behaviour.

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
    /// CHECK: locked to the entry's registered program. Read for its loader,
    /// which says whether a deploy slot exists to record.
    #[account(address = quoter.load()?.program_id)]
    pub quoter_program: UncheckedAccount<'info>,
    /// CHECK: validated as `quoter_program`'s program-data account in the
    /// handler. Read for the slot the program was last deployed at. Optional
    /// because revoking approval needs none of this, and a program on a loader
    /// that cannot redeploy has no such account.
    pub quoter_program_data: Option<UncheckedAccount<'info>>,
}

/// Offsets into a `ProgramData` account: a four-byte enum tag, then the slot
/// the program was last deployed at.
const PROGRAM_DATA_TAG: [u8; 4] = [3, 0, 0, 0];
const DEPLOY_SLOT_OFFSET: usize = 4;

/// The slot `program` was last deployed at, or zero when its loader keeps no
/// such record.
///
/// Recorded rather than enforced. A later upgrade moves this slot, so an
/// off-chain reader that holds the approved figure can see that the code
/// changed and act on it.
fn deployed_slot(
    program: &UncheckedAccount,
    program_data: Option<&UncheckedAccount>,
) -> Result<u64> {
    if program.owner != &bpf_loader_upgradeable::ID {
        // Any other loader writes the program once. There is no program-data
        // account, and no slot to move.
        return Ok(0);
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
        data.len() >= DEPLOY_SLOT_OFFSET + 8 && data[..4] == PROGRAM_DATA_TAG,
        ErrorCode::InvalidQuoterConfig,
        "program data account does not hold program data"
    )?;
    let mut slot = [0u8; 8];
    slot.copy_from_slice(&data[DEPLOY_SLOT_OFFSET..DEPLOY_SLOT_OFFSET + 8]);
    Ok(u64::from_le_bytes(slot))
}

pub fn handle_update_quoter_approved(
    ctx: Context<UpdateQuoterApproved>,
    approved: bool,
) -> Result<()> {
    let mut quoter = ctx.accounts.quoter.load_mut()?;
    if approved {
        quoter.approved_program_slot = deployed_slot(
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
    } else {
        // Nothing is approved, so no slot is either.
        quoter.approved_program_slot = 0;
    }
    quoter.is_approved = approved;
    Ok(())
}
