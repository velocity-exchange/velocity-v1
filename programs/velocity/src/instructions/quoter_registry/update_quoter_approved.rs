//! The admin approval gate. It copies a staging entry's config into the
//! market's slab, or it pulls that copy back out. The slab copy is the only
//! config fills read. A maker edit to the staging entry reaches no flow until
//! the admin copies it in again. The approved copy keeps serving until then.
//!
//! The slab account stays right-sized here. Approval grows it by exactly the
//! slot it needs, and the admin pays the added rent. Revocation returns trailing
//! vacancy to the admin. Every reader pays compute per declared slot, so
//! capacity tracks the roster and not a guess made at creation.
//!
//! Approval checks that the config is coherent enough to call. Both legs need a
//! non-empty index list, and each list must forward the response account. The
//! router reads responses from that account. No reserved key may sit on the
//! registered list. Only the book's own entry may name the market's book, which
//! closes the window between the book's designation and its own approval. A
//! `Clob` approval also asks the book for its placement rules, so approval
//! refuses a slot that would fail every fill.
//!
//! Approval does not require a frozen program, and it does not freeze one. A
//! maker may upgrade the program behind an approved entry. Three things make
//! that acceptable, and the first one matters most.
//!
//! 1. A `Custom` entry can move only its own registered user. The price is held
//!    to the entry's own quote and to the taker's limit price. The size stays
//!    inside the entry's own margin. Every touched account is margin-checked
//!    after the fill. An upgrade can therefore lose the maker's money, and it
//!    cannot take anyone else's.
//! 2. An entry that quotes and does not deliver stops receiving flow. Fillers
//!    choose which entries to carry, and a taker's signed route names its own.
//! 3. The admin can pull the copy at any time, and the maker holds
//!    `is_active` as well.
//!
//! A frozen program would add little to that. It closes only the case of a
//! program that is honest at approval and hostile later. The same behaviour can
//! ship in the binary that gets approved, and no practical review of a compiled
//! program catches a quoter that sometimes returns nothing. A frozen program
//! would also cost a maker every bug fix, because a redeploy is a new program id
//! and therefore a new registry entry.
//!
//! Approval records the slot the program was deployed at. An upgrade then shows
//! as a changed slot, so a reader sees that the code moved and does not have to
//! infer it from behaviour.
//!
//! Revocation depends on the type. Revocation clears a `Custom` slot, which has
//! no resting state to unwind. Revocation suspends a `Clob` slot instead. A
//! suspended slot quotes nothing, and its config stays so the removal paths keep
//! working. A maker must always be able to pull orders off a killed book.

use {
    crate::{
        auth::check_warm,
        error::ErrorCode,
        state::{
            prop_amm::{
                list_stays_off_the_book, occupied_slots, slot_for_entry, vacant_slot_index,
                validate_quoter_accounts, ClobReader, QuoterConfigV0, QuoterSlabExt, QuoterSlabV0,
                QuoterType, QuoterV0,
            },
            state::State,
        },
        validate,
    },
    anchor_lang::{prelude::*, solana_program::bpf_loader_upgradeable},
};

/// Ceiling on a slab's capacity. It is far above any plausible roster. It
/// exists so the account cannot grow without bound.
const MAX_TOTAL_CAPACITY: u16 = 128;

#[derive(Accounts)]
pub struct UpdateQuoterApproved<'info> {
    /// Mutable, because approval growth takes the added rent from the admin
    /// and revocation refunds it there.
    #[account(mut, constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    /// The staging entry whose config is copied in, or whose copy is pulled.
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// The market the entry serves. A `Clob` approval is held to the book this
    /// market designated at registration. The staging entry's response account
    /// is maker-editable. Without that pin an edited entry could put a different
    /// book into slot 0 than the one the market names.
    #[account(
        seeds = [b"perp_market", quoter.load()?.config.market.to_le_bytes().as_ref()],
        bump,
        has_one = quoter_slab
    )]
    pub perp_market: AccountLoader<'info, crate::state::perp_market::PerpMarket>,
    #[account(mut)]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: locked to the entry's registered program. Read for its loader,
    /// which says whether a deploy slot exists to record.
    #[account(address = quoter.load()?.config.program_id)]
    pub quoter_program: UncheckedAccount<'info>,
    /// CHECK: the handler validates it as `quoter_program`'s program-data
    /// account. It is read for the slot the program was last deployed at. It is
    /// optional, because revoking approval needs none of this, and a program on
    /// a loader that cannot redeploy has no such account.
    pub quoter_program_data: Option<UncheckedAccount<'info>>,
    /// CHECK: the book the market designated at registration, bound by that
    /// designation. A `Clob` approval needs it, because the handler asks the
    /// book for its own placement rules. Every other entry omits it.
    #[account(address = perp_market.load()?.clob_market)]
    pub clob_market: Option<UncheckedAccount<'info>>,
    pub system_program: Program<'info, System>,
}

/// Layout of a `ProgramData` account. A four-byte enum tag comes first, then
/// the slot the program was last deployed at.
const PROGRAM_DATA_TAG: [u8; 4] = [3, 0, 0, 0];
const DEPLOY_SLOT_OFFSET: usize = 4;

/// The slot `program` was last deployed at, or zero when this code cannot
/// read one.
///
/// The slot is recorded and not enforced. A later upgrade moves the slot, so
/// an off-chain reader that holds the approved figure sees that the code
/// changed, and can act on it.
///
/// Zero means unknown. It does not mean immutable. Only the upgradeable loader
/// is read here, and a program on a later loader can redeploy while it reports
/// zero. A reader must not take zero as proof that the code is fixed. A reader
/// that needs that proof compares the program's own bytes.
fn deployed_slot(
    program: &UncheckedAccount,
    program_data: Option<&UncheckedAccount>,
) -> Result<u64> {
    if program.owner != &bpf_loader_upgradeable::ID {
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

/// Resize the slab to hold exactly `capacity` slots.
///
/// Growth takes the rent shortfall from the admin and zero-fills the new tail,
/// which is what a vacant slot is. Shrink writes the header first, so the
/// declared capacity never exceeds what the account holds. Shrink then refunds
/// the freed rent to the admin. Only velocity can debit a velocity-owned
/// account, so the refund is a direct lamport move.
fn resize_slab<'info>(
    slab: &AccountLoader<'info, QuoterSlabV0>,
    admin: &AccountInfo<'info>,
    system_program: &Program<'info, System>,
    capacity: u16,
) -> Result<()> {
    let current = slab.load()?.capacity;
    if capacity == current {
        return Ok(());
    }

    let info = slab.to_account_info();
    let new_space = QuoterSlabV0::space(capacity as usize);
    let required = Rent::get()?.minimum_balance(new_space);
    if capacity > current {
        // Transfer the rent first. A resize that leaves the account under the
        // new minimum fails the transaction at its end.
        let shortfall = required.saturating_sub(info.lamports());
        if shortfall > 0 {
            anchor_lang::system_program::transfer(
                CpiContext::new(
                    system_program.key(),
                    anchor_lang::system_program::Transfer {
                        from: admin.clone(),
                        to: info.clone(),
                    },
                ),
                shortfall,
            )?;
        }

        info.resize(new_space).map_err(Into::<Error>::into)?;
        slab.load_mut()?.capacity = capacity;
    } else {
        slab.load_mut()?.capacity = capacity;
        info.resize(new_space).map_err(Into::<Error>::into)?;
        let refund = info.lamports().saturating_sub(required);
        if refund > 0 {
            **info.try_borrow_mut_lamports()? -= refund;
            **admin.try_borrow_mut_lamports()? += refund;
        }
    }

    Ok(())
}

/// The smallest capacity that still holds every occupied slot. Never below
/// one, because slot 0 stays allocated for the market's book.
fn fitted_capacity(slab: &AccountLoader<QuoterSlabV0>) -> Result<u16> {
    let slots = slab.slots()?;
    Ok(slots
        .iter()
        .rposition(|slot| !slot.is_vacant())
        .map(|index| index as u16 + 1)
        .unwrap_or(1))
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct UpdateQuoterApprovedArgs {
    /// True copies the staged config into the slab. False pulls the copy.
    pub approved: bool,
}

pub fn handle_update_quoter_approved(
    ctx: Context<UpdateQuoterApproved>,
    args: UpdateQuoterApprovedArgs,
) -> Result<()> {
    let UpdateQuoterApprovedArgs { approved } = args;
    let entry_key = ctx.accounts.quoter.key();
    let quoter = ctx.accounts.quoter.load()?;

    if !approved {
        return revoke_slab_slot(
            &ctx.accounts.quoter_slab,
            &ctx.accounts.admin,
            &ctx.accounts.system_program,
            &entry_key,
        );
    }

    let config = &quoter.config;
    validate_approvable_config(config)?;
    let index = approved_slot_index(
        &ctx.accounts.quoter_slab,
        &ctx.accounts.perp_market,
        config,
        &entry_key,
    )?;

    validate!(
        (index as u16) < MAX_TOTAL_CAPACITY,
        ErrorCode::QuoterSlabFull,
        "quoter slab for market {} is at its {}-slot ceiling",
        config.market,
        MAX_TOTAL_CAPACITY
    )?;

    if config.quoter_type == QuoterType::Clob {
        validate_book_identity(
            ctx.accounts.clob_market.as_ref(),
            &ctx.accounts.quoter_program,
            &ctx.accounts.quoter_slab.key(),
        )?;
    }

    let approved_program_slot = deployed_slot(
        &ctx.accounts.quoter_program,
        ctx.accounts.quoter_program_data.as_ref(),
    )?;

    // Grow to fit the chosen slot, and never shrink here. A `Clob` approval
    // into slot 0 must not take allocated slots away.
    let current = ctx.accounts.quoter_slab.load()?.capacity;
    if index as u16 >= current {
        resize_slab(
            &ctx.accounts.quoter_slab,
            &ctx.accounts.admin,
            &ctx.accounts.system_program,
            index as u16 + 1,
        )?;
    }

    write_approved_slot(
        &ctx.accounts.quoter_slab,
        index,
        &entry_key,
        config,
        approved_program_slot,
    )
}

/// Ask the book the market designated whether it is a book, and whether
/// velocity may place on it.
///
/// Every router fill needs the `Clob` slot. A slot whose account is not a CLOB
/// market fails its `execute_v0` on every fill, and so does a slot whose
/// `place_authority` is not the market's slab. The market then stops filling
/// until an admin revokes the slot. `update_perp_market_clob_quoter` runs the
/// same two checks, but only after approval, so approval must ask for itself.
///
/// The question is `order_rules_v0`, a CPI that signs nothing. An account
/// that is not this program's market answers nothing, and the approval fails.
fn validate_book_identity<'info>(
    clob_market: Option<&UncheckedAccount<'info>>,
    clob_program: &UncheckedAccount<'info>,
    quoter_slab: &Pubkey,
) -> Result<()> {
    let market = clob_market.ok_or_else(|| {
        msg!("approving a book requires the book account");
        error!(ErrorCode::InvalidQuoterConfig)
    })?;
    let rules = ClobReader {
        market: market.as_ref(),
        program: clob_program.as_ref(),
    }
    .order_rules()?;
    validate!(
        rules.place_authority == quoter_slab.to_bytes(),
        ErrorCode::InvalidQuoterConfig,
        "book place authority is not the market's quoter slab {}",
        quoter_slab
    )?;

    Ok(())
}

/// Pull the entry's copy out of the slab, then give the freed tail back. An
/// entry that holds no slot is not an error, so a repeated revocation does
/// nothing.
fn revoke_slab_slot<'info>(
    slab: &AccountLoader<'info, QuoterSlabV0>,
    admin: &AccountInfo<'info>,
    system_program: &Program<'info, System>,
    entry_key: &Pubkey,
) -> Result<()> {
    {
        let mut slots = slab.slots_mut()?;
        let Some(index) = slot_for_entry(&slots, entry_key) else {
            msg!("quoter {} holds no slab slot; nothing to revoke", entry_key);
            return Ok(());
        };

        if slots[index].config.quoter_type == QuoterType::Clob {
            // The config stays so the removal paths keep working on the dead
            // book. The slot quotes nothing.
            slots[index].suspended = true;
        } else {
            slots[index].clear();
        }
    }

    // Give trailing vacancy back. Occupied slots never move, so only the
    // tail past the last occupied slot can shrink away.
    let fitted = fitted_capacity(slab)?;
    resize_slab(slab, admin, system_program, fitted)
}

/// Check that the config is coherent enough to call. Both legs name accounts.
/// Every index points into the registered list. Both legs forward the response
/// account. No reserved key sits on the registered list.
fn validate_approvable_config(config: &QuoterConfigV0) -> Result<()> {
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

    // The check runs here as well as at write time. A list stored before the
    // reserved-key check existed is still on chain, and approval is the gate
    // that lets a config take flow.
    validate_quoter_accounts(
        registered
            .iter()
            .map(|meta| (&meta.pubkey, meta.is_writable)),
        config.market,
    )
}

/// Whether two approved quoters keep out of each other's response account.
/// The caller checks both directions, so approval order does not decide
/// which of a pair is refused. This is velocity's only enforcement of the
/// signing model's first fact, checked once at approval. See [`crate::signer`].
fn response_accounts_stay_apart<'a>(
    mut left_list: impl Iterator<Item = &'a Pubkey>,
    left_response: &Pubkey,
    mut right_list: impl Iterator<Item = &'a Pubkey>,
    right_response: &Pubkey,
) -> bool {
    left_list.all(|key| key != right_response) && right_list.all(|key| key != left_response)
}

/// The slab slot the entry takes, checked against the slots already approved.
/// A `Clob` entry always takes slot 0. Any other entry keeps the slot it holds,
/// or takes the first vacant one, or takes the slot past the tail.
fn approved_slot_index(
    slab: &AccountLoader<QuoterSlabV0>,
    perp_market: &AccountLoader<crate::state::perp_market::PerpMarket>,
    config: &QuoterConfigV0,
    entry_key: &Pubkey,
) -> Result<usize> {
    let registered = config.registered_accounts();
    let slots = slab.slots()?;
    // A route names the slots it consults by carrying their response
    // accounts, so two slots sharing one could not be carried apart.
    validate!(
        occupied_slots(&slots).all(|(_, slot)| slot.entry == *entry_key
            || slot.config.response_account != config.response_account),
        ErrorCode::InvalidQuoterConfig,
        "another approved quoter already uses response account {}",
        config.response_account
    )?;

    // A registered list may not name another slot's response account either.
    // Doing so forces that slot into every fill this one rides in, and a
    // slot with an incomplete account list fails the fill. The check runs
    // both directions, so approval order does not decide which of a pair is
    // refused. This also carries the signing model (see `crate::signer`):
    // before approving a third-party quoter program, check that every gated
    // instruction also requires that program's response account.
    validate!(
        occupied_slots(&slots).all(|(_, slot)| slot.entry == *entry_key
            || response_accounts_stay_apart(
                registered.iter().map(|meta| &meta.pubkey),
                &config.response_account,
                slot.config
                    .registered_accounts()
                    .iter()
                    .map(|meta| &meta.pubkey),
                &slot.config.response_account,
            )),
        ErrorCode::InvalidQuoterConfig,
        "a registered account list may not name another approved quoter's response account"
    )?;

    // The market's own book is excluded by name, not only by slot. It is named at registration
    // (`initialize_quoter`) but reaches slot 0 only at its own approval, so the sweep above
    // misses it while vacant. The book gates its authority on the slab signer alone, so a
    // quoter holding the book account could place, cancel, evict and fill with its own signature.
    if config.quoter_type != QuoterType::Clob {
        let book = perp_market.load()?.clob_market;
        validate!(
            list_stays_off_the_book(registered.iter().map(|meta| &meta.pubkey), &book),
            ErrorCode::InvalidQuoterConfig,
            "a registered account list may not name the market's book {}",
            book
        )?;
    }

    // Slot 0 is the book's by convention, so every book-touching instruction
    // reads it without a scan. A market holds one book, so a second `Clob`
    // approval must be the same entry approved again.
    if config.quoter_type == QuoterType::Clob {
        validate!(
            slots[0].is_vacant() || slots[0].entry == *entry_key,
            ErrorCode::InvalidQuoterConfig,
            "the slab already holds a book slot"
        )?;

        // The book the market designated at registration. The staging response
        // account is maker-editable, so this check refuses an edit that points
        // elsewhere.
        validate!(
            perp_market.load()?.clob_market == config.response_account,
            ErrorCode::InvalidQuoterConfig,
            "the entry's book {} is not the market's designated book",
            config.response_account
        )?;

        Ok(0)
    } else {
        Ok(match slot_for_entry(&slots, entry_key) {
            Some(index) => index,
            // With no vacancy, take the slot past the current tail. The
            // resize below allocates it.
            None => vacant_slot_index(&slots).unwrap_or(slots.len()),
        })
    }
}

/// Copy the vetted config into its slab slot, which is the only copy fills
/// read.
fn write_approved_slot(
    slab: &AccountLoader<QuoterSlabV0>,
    index: usize,
    entry_key: &Pubkey,
    config: &QuoterConfigV0,
    approved_program_slot: u64,
) -> Result<()> {
    // The header mirrors the book's account so accounts structs can bind a
    // slab to its book with `has_one = clob_market`. The header keeps that key
    // through a book suspension, because the removal paths must keep reaching
    // a killed book.
    if config.quoter_type == QuoterType::Clob {
        slab.load_mut()?.clob_market = config.response_account;
    }

    let mut slots = slab.slots_mut()?;
    slots[index].entry = *entry_key;
    slots[index].suspended = false;
    slots[index].config = *config;
    slots[index].config.approved_program_slot = approved_program_slot;
    Ok(())
}

/// The rule the quoter signing model rests on, per direction.
#[cfg(test)]
mod response_exclusion_tests {
    use {super::response_accounts_stay_apart, anchor_lang::prelude::Pubkey};

    fn apart(
        left: &[Pubkey],
        left_response: &Pubkey,
        right: &[Pubkey],
        right_response: &Pubkey,
    ) -> bool {
        response_accounts_stay_apart(left.iter(), left_response, right.iter(), right_response)
    }

    #[test]
    fn two_quoters_that_share_nothing_are_apart() {
        let (a, b, x, y) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );

        assert!(apart(&[a, x], &a, &[b, y], &b));
    }

    #[test]
    fn a_list_naming_the_other_response_account_is_refused() {
        // The quoter being approved reaches the approved one. Without this an
        // execute leg could forward the market's slab signature into that
        // quoter, which requires its response account to complete the call.
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        assert!(!apart(&[a, b], &a, &[b], &b));
    }

    #[test]
    fn an_approved_list_naming_the_new_response_account_is_refused() {
        // The other direction. The quoter already on the slab reaches the one
        // being approved. The check runs here too, so approval order does not
        // decide which of a pair is refused.
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        assert!(!apart(&[a], &a, &[b, a], &b));
    }

    #[test]
    fn naming_ones_own_response_account_is_not_a_breach() {
        // Approval requires each list to forward its own response account, so
        // the rule must not read that as a reach into another quoter.
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        assert!(apart(&[a], &a, &[b], &b));
    }
}
