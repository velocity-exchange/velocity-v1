//! Admin vetting gate: copy a staging entry's config into the market's slab
//! (or pull it back out). The slab copy is the only config fills read, so a
//! maker edit to the staging entry never reaches flow until the admin copies
//! it in again — and until then the previously vetted copy keeps serving.
//!
//! Approval validates the config is coherent enough to CPI: non-empty index
//! lists on both legs, each naming the response account (the router reads
//! responses from it, so it must be forwarded), and no reserved key on the
//! registered list.
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
//! 3. The admin can pull the copy at any time, and the maker holds
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
//!
//! Revocation splits by type. A `Custom` slot is cleared — it has no resting
//! state to unwind. A `Clob` slot is suspended instead: it quotes nothing,
//! but its config stays so the removal paths keep working, because a maker
//! must always be able to pull orders off a killed book.

use {
    crate::{
        auth::check_warm,
        error::ErrorCode,
        state::{
            prop_amm::{
                occupied_slots, quoter_slab_slots_mut, slot_for_entry, vacant_slot_index,
                validate_quoter_accounts, QuoterSlabV0, QuoterType, QuoterV0, QUOTER_SLAB_PDA_SEED,
            },
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
    /// The staging entry whose config is copied in (or whose copy is pulled).
    pub quoter: AccountLoader<'info, QuoterV0>,
    #[account(
        mut,
        seeds = [
            QUOTER_SLAB_PDA_SEED,
            quoter.load()?.config.market.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: locked to the entry's registered program. Read for its loader,
    /// which says whether a deploy slot exists to record.
    #[account(address = quoter.load()?.config.program_id)]
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
    let entry_key = ctx.accounts.quoter.key();
    let quoter = ctx.accounts.quoter.load()?;
    let mut slots = quoter_slab_slots_mut(&ctx.accounts.quoter_slab)?;

    if !approved {
        let Some(index) = slot_for_entry(&slots, &entry_key) else {
            msg!("quoter {} holds no slab slot; nothing to revoke", entry_key);
            return Ok(());
        };
        if slots[index].config.quoter_type == QuoterType::Clob {
            // The config stays so the removal paths keep working on the dead
            // book; the slot just quotes nothing.
            slots[index].suspended = true;
        } else {
            slots[index].clear();
        }
        return Ok(());
    }

    let config = &quoter.config;
    validate!(
        config.quoter_type != QuoterType::Vamm,
        ErrorCode::InvalidQuoterConfig,
        "the vAMM quotes in-program, not through the registry"
    )?;
    let registered = config.registered_accounts();
    for (name, indexes) in [
        ("quote", config.quote_leg_indexes()),
        ("execute", config.execute_leg_indexes()),
    ] {
        validate!(
            !indexes.is_empty(),
            ErrorCode::InvalidQuoterConfig,
            "cannot approve a quoter with an empty {} leg",
            name
        )?;
        validate!(
            indexes.iter().all(|&i| (i as usize) < registered.len()),
            ErrorCode::InvalidQuoterConfig,
            "a {} leg index points past the registered list",
            name
        )?;
        // The router reads responses from the response account, so every leg
        // must forward it.
        validate!(
            indexes
                .iter()
                .any(|&i| registered[i as usize].pubkey == config.response_account),
            ErrorCode::InvalidQuoterConfig,
            "response account must be forwarded on both CPI legs"
        )?;
    }
    // Re-checked here, not only at write time: a list stored before the
    // reserved-key check existed is still on chain, and approval is the gate
    // that lets a config take flow.
    validate_quoter_accounts(registered.iter().map(|meta| &meta.pubkey))?;

    // A route names the slots it consults by carrying their response
    // accounts, so two slots sharing one could not be carried apart.
    validate!(
        occupied_slots(&slots).all(|(_, slot)| slot.entry == entry_key
            || slot.config.response_account != config.response_account),
        ErrorCode::InvalidQuoterConfig,
        "another approved quoter already uses response account {}",
        config.response_account
    )?;
    // Nor may the registered lists overlap the response accounts: a list that
    // names another slot's response account would force that slot into every
    // fill this one rides in, and a consulted slot with an incomplete account
    // list fails the fill. Checked in both directions, so approval order does
    // not decide which pair is refused.
    validate!(
        occupied_slots(&slots).all(|(_, slot)| slot.entry == entry_key
            || (registered
                .iter()
                .all(|meta| meta.pubkey != slot.config.response_account)
                && slot
                    .config
                    .registered_accounts()
                    .iter()
                    .all(|meta| meta.pubkey != config.response_account))),
        ErrorCode::InvalidQuoterConfig,
        "a registered account list may not name another approved quoter's response account"
    )?;
    // Slot 0 is the book's, by convention, so every book-touching
    // instruction reads it without a scan. One book per market: a second
    // Clob approval must be the same entry re-approved.
    let index = if config.quoter_type == QuoterType::Clob {
        validate!(
            slots[0].is_vacant() || slots[0].entry == entry_key,
            ErrorCode::InvalidQuoterConfig,
            "the slab already holds a book slot"
        )?;
        0
    } else {
        match slot_for_entry(&slots, &entry_key) {
            Some(index) => index,
            None => vacant_slot_index(&slots).ok_or_else(|| {
                msg!("quoter slab for market {} is full", config.market);
                error!(ErrorCode::QuoterSlabFull)
            })?,
        }
    };
    let approved_program_slot = deployed_slot(
        &ctx.accounts.quoter_program,
        ctx.accounts.quoter_program_data.as_ref(),
    )?;
    slots[index].entry = entry_key;
    slots[index].suspended = false;
    slots[index].config = *config;
    slots[index].config.approved_program_slot = approved_program_slot;
    Ok(())
}
