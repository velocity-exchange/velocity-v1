//! Relay's self-maintenance path for a user's liquidation conditions, and
//! its resolver.
//!
//! This is separate from the opt-in [`super::sync_liq_conditions`] for one
//! reason. A staged executor may not name a signer. Relay's turner builds
//! every executor meta with `is_signer: false`, and it refuses to sign a
//! transaction whose executor account list holds a signer. An executor is
//! permissionless, so a signing account handed to one can be drained.
//!
//! The instruction relay stages therefore takes no payer and allocates
//! nothing. The opt-in sync created the account before relay ever stages this.
//! A staged resync replays the stored account list and adds the accounts of
//! every market the user entered since, so coverage follows new exposure.
//! The keeper is paid from the protocol crank treasury, and only for a resync
//! that found the user's positions changed. The stored payment prices a whole
//! transaction, so resyncs batched into one transaction share it. A resync
//! invoked through CPI is not paid, because the transaction cannot count it.
//!
//! The treasury pays rather than the user's own conditions account, because a
//! stale threshold is a protocol problem before it is a user's. A resync that
//! nobody is paid to run leaves the user's liquidation thresholds behind their
//! real exposure, and the liquidation that should fire does not. Charging that
//! to the account being watched turns an underfunded user into protocol bad
//! debt.

use {
    super::sync_liq_conditions::{rewrite_liq_conditions, SyncLiqConditionsTerms},
    crate::{
        error::ErrorCode,
        state::{
            crank_treasury::{CrankTreasuryV0, CRANK_TREASURY_PDA_SEED},
            user::User,
            user_conditions::{UserConditionsV0, USER_CONDITIONS_PDA_SEED},
        },
    },
    anchor_lang::{prelude::*, Discriminator},
};

#[derive(Accounts)]
pub struct ResyncLiqConditions<'info> {
    /// CHECK: the keeper payout target, which fills relay's
    /// `KEEPER_PLACEHOLDER` slot. It is never a signer, as the module doc
    /// explains. It only receives lamports.
    #[account(mut)]
    pub keeper: UncheckedAccount<'info>,
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        seeds = [USER_CONDITIONS_PDA_SEED, user.key().as_ref()],
        bump,
        constraint = liq_conditions.load()?.user == user.key()
    )]
    pub liq_conditions: AccountLoader<'info, UserConditionsV0>,
    /// The protocol pool this resync is paid from.
    #[account(mut, seeds = [CRANK_TREASURY_PDA_SEED], bump)]
    pub treasury: AccountLoader<'info, CrankTreasuryV0>,
    /// CHECK: the instructions sysvar, pinned by address. The payment is
    /// divided by the resyncs this transaction carries.
    #[account(address = solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,
}

/// Resyncs a user's liquidation conditions along relay's permissionless path.
///
/// The block carries its own terms. Re-deriving them from the account would
/// let a staged resync re-price itself.
pub fn handle_resync_liq_conditions<'c: 'info, 'info>(
    ctx: Context<'info, ResyncLiqConditions<'info>>,
) -> Result<()> {
    let (args, digest_before) = {
        let conditions = ctx.accounts.liq_conditions.load()?;
        let terms = SyncLiqConditionsTerms {
            sync_payment_lamports: conditions.sync_payment_lamports,
            sync_fallback_slots: conditions.sync_fallback_slots,
        };

        (terms, conditions.positions_digest)
    };

    rewrite_liq_conditions(
        &ctx.accounts.liq_conditions,
        &ctx.accounts.user,
        ctx.remaining_accounts,
        args,
        false,
    )?;

    // Terms below the interval floor pay nothing, since paying them would let
    // any permissionless account fund a crank once per slot. The rewrite
    // above already zeroes and silences a block armed before the floor
    // existed, so this also covers that case.
    let payment = args.payable_lamports();
    let digest_after = ctx.accounts.liq_conditions.load()?.positions_digest;
    if !resync_earns_payment(payment, digest_before, digest_after) {
        return Ok(());
    }

    // Paid at most once per fallback interval, the fallback poll's own
    // cadence. A user who changes positions every slot still draws the fee
    // once per interval.
    let slot = Clock::get()?.slot;
    let due_slot = {
        let conditions = ctx.accounts.liq_conditions.load()?;
        conditions
            .last_paid_sync_slot
            .saturating_add(conditions.sync_fallback_slots)
    };

    if slot < due_slot {
        msg!(
            "resync of {} was already paid this interval",
            ctx.accounts.user.key()
        );

        return Ok(());
    }

    let share = batch_share(payment, &ctx.accounts.instructions_sysvar);
    if share == 0 {
        return Ok(());
    }

    ctx.accounts.liq_conditions.load_mut()?.last_paid_sync_slot = slot;

    let treasury = ctx.accounts.treasury.to_account_info();
    let rent_minimum = Rent::get()?.minimum_balance(treasury.data_len());
    // The payment is best effort, as it was when the user's own account paid.
    // An empty treasury must not fail a resync that already rewrote the block.
    // Relay's own payment guard protects the keeper, because it skips work
    // that would not pay.
    let available = treasury.lamports().saturating_sub(rent_minimum);
    let paid = CrankTreasuryV0::pay_out(
        &treasury,
        &ctx.accounts.keeper.to_account_info(),
        share.min(available),
        rent_minimum,
    )?;
    let mut treasury_state = ctx.accounts.treasury.load_mut()?;
    treasury_state.total_paid = treasury_state.total_paid.saturating_add(paid);
    Ok(())
}

/// Whether a resync may draw its payment at all. The treasury pays for a
/// position change, not for a rewrite of the same exposures. Without the
/// digest test a keeper could crank an unchanged account every interval.
fn resync_earns_payment(payment: u64, digest_before: u64, digest_after: u64) -> bool {
    payment > 0 && digest_before != digest_after
}

/// This resync's share of a payment that prices a whole transaction. A resync
/// that another program invokes through CPI gets nothing, because the sysvar
/// cannot count those calls. Relay submits each executor at the top level.
fn batch_share(payment: u64, instructions_sysvar: &AccountInfo) -> u64 {
    if !crate::instructions::optional_accounts::is_top_level_call(
        instructions_sysvar,
        crate::instruction::ResyncLiqConditions::DISCRIMINATOR,
    ) {
        return 0;
    }

    let claimants = crate::instructions::optional_accounts::tx_reimbursement_claimants(
        instructions_sysvar,
        crate::instruction::ResyncLiqConditions::DISCRIMINATOR,
    )
    .unwrap_or(1);

    payment / u64::from(claimants.max(1))
}

/// Resolver for the self-sync conditions. It reports work when the user's
/// positions no longer match the thresholds the block was built from, and the
/// resync would be paid what the block advertises.
///
/// The match test is cheap and conservative. It compares a digest of the
/// user's exposures against the one the last sync stamped, so it catches a
/// new market, a closed position, or a first opt-in. The staged executor
/// recomputes everything from the stored account list, plus the accounts of
/// any market the user entered since. A resync inside the paid interval pays
/// nothing, and relay asserts the advertised payment, so the fallback poll
/// takes that change once the interval ends.
#[derive(Accounts)]
pub struct ResolveResyncLiqConditions<'info> {
    /// The shared staging account, at index 0 by convention. A resolver's
    /// response pointer is read against it.
    #[account(mut, seeds = [crate::state::relay_scratch::RELAY_SCRATCH_PDA_SEED], bump)]
    pub scratch: AccountLoader<'info, crate::state::relay_scratch::RelayScratchV0>,
    /// Read-only. A resolver stages into the shared scratch account rather
    /// than into the block it reads.
    #[account(constraint = liq_conditions.load()?.user == user.key())]
    pub liq_conditions: AccountLoader<'info, UserConditionsV0>,
    pub user: AccountLoader<'info, User>,
}

pub fn handle_resolve_resync_liq_conditions(
    ctx: Context<ResolveResyncLiqConditions>,
) -> Result<()> {
    crate::instructions::constraints::require_view_accounts(
        &ctx.accounts.to_account_infos(),
        &[ctx.accounts.scratch.key()],
    )?;
    crate::instructions::resolve_into(&ctx.accounts.scratch, || {
        let slot = Clock::get()?.slot;
        let (stale, paid) = {
            let conditions = ctx.accounts.liq_conditions.load()?;
            let user = crate::load!(ctx.accounts.user)?;
            let terms = SyncLiqConditionsTerms {
                sync_payment_lamports: conditions.sync_payment_lamports,
                sync_fallback_slots: conditions.sync_fallback_slots,
            };
            let due_slot = conditions
                .last_paid_sync_slot
                .saturating_add(conditions.sync_fallback_slots);

            (
                UserConditionsV0::digest_positions(&user) != conditions.positions_digest,
                terms.payable_lamports() > 0 && slot >= due_slot,
            )
        };

        if !stale || !paid {
            return Ok(None);
        }

        let stored = ctx.accounts.liq_conditions.load()?.read_sync_accounts();
        let entered = entered_market_accounts(
            &*crate::load!(ctx.accounts.user)?,
            ctx.remaining_accounts,
            &stored,
        );

        // The builder enforces the no-signer rule for every resolver. That
        // rule is why this instruction exists.
        Ok(Some(
            crate::staged_call!(ResyncLiqConditions {
                keeper: crate::state::pdas::keeper_placeholder(),
                user: ctx.accounts.user.key(),
                liq_conditions: ctx.accounts.liq_conditions.key(),
                treasury: crate::state::pdas::crank_treasury(),
                instructions_sysvar: solana_program::sysvar::instructions::ID,
            })
            .refs(stored)
            .refs(entered),
        ))
    })
}

/// The accounts of every market the user holds a position in that the stored
/// list does not carry. `stored_accounts` is that list as relay passed it.
///
/// A perp market brings its crank account and slab, which exist only for a
/// market with a book. The executor skips an account that does not exist. Every
/// perp market quotes in the quote spot market, so a first perp position also
/// brings that market. The executor stores each market's oracle from the market
/// account, so no oracle is named here.
fn entered_market_accounts(
    user: &User,
    stored_accounts: &[AccountInfo],
    stored: &[relay_spec::AccountRefV0],
) -> Vec<relay_spec::AccountRefV0> {
    use crate::state::{pdas, perp_market::PerpMarket, spot_market::SpotMarket};

    let covered_perps: Vec<u16> = stored_accounts
        .iter()
        .filter_map(|info| {
            stored_market_index(
                info,
                PerpMarket::DISCRIMINATOR,
                core::mem::offset_of!(PerpMarket, market_index),
            )
        })
        .collect();
    let covered_spots: Vec<u16> = stored_accounts
        .iter()
        .filter_map(|info| {
            stored_market_index(
                info,
                SpotMarket::DISCRIMINATOR,
                core::mem::offset_of!(SpotMarket, market_index),
            )
        })
        .collect();

    let entered_perps = user
        .perp_positions
        .iter()
        .filter(|position| !position.is_available())
        .map(|position| position.market_index)
        .filter(|index| !covered_perps.contains(index));
    let perp_keys = entered_perps.flat_map(|index| {
        [
            pdas::perp_market(index),
            pdas::clob_crank_conditions(index),
            pdas::quoter_slab(index),
        ]
    });

    let holds_perp = user.perp_positions.iter().any(|p| !p.is_available());
    let quote_market = holds_perp.then_some(crate::math::constants::QUOTE_SPOT_MARKET_INDEX);
    let entered_spots = user
        .spot_positions
        .iter()
        .filter(|position| !position.is_available())
        .map(|position| position.market_index)
        .chain(quote_market)
        .filter(|index| !covered_spots.contains(index));
    let spot_keys = entered_spots.map(pdas::spot_market);

    let stored_keys: Vec<Pubkey> = stored
        .iter()
        .map(|account| Pubkey::new_from_array(account.address))
        .collect();
    perp_keys
        .chain(spot_keys)
        .fold(Vec::new(), |mut entered, key| {
            if !stored_keys.contains(&key) && !entered.contains(&key) {
                entered.push(key);
            }

            entered
        })
        .into_iter()
        .map(|key| relay_spec::AccountRefV0::readonly(key.to_bytes()))
        .collect()
}

/// The index of the market `info` holds, read at `index_offset` past the
/// discriminator. The resolver's accounts and their lifetimes differ, so it
/// cannot build an `AccountLoader` over them.
fn stored_market_index(
    info: &AccountInfo,
    discriminator: &[u8],
    index_offset: usize,
) -> Option<u16> {
    if info.owner != &crate::ID {
        return None;
    }

    let data = info.try_borrow_data().ok()?;
    if !data.starts_with(discriminator) {
        return None;
    }

    let at = discriminator.len() + index_offset;
    let bytes = data.get(at..at + 2)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

#[cfg(test)]
mod tests {
    use {
        super::{batch_share, resync_earns_payment},
        anchor_lang::{
            prelude::{AccountInfo, Pubkey},
            Discriminator,
        },
    };

    /// A user synced over spot market 0 and perp market 0 deposits in spot
    /// market 1 and opens perp market 2. The staged resync names those
    /// markets, the perp market's crank account and slab, and nothing the
    /// stored list already holds.
    #[test]
    fn a_resync_names_the_markets_the_user_entered() {
        use {
            super::entered_market_accounts,
            crate::{
                create_anchor_account_info,
                state::{
                    pdas,
                    perp_market::PerpMarket,
                    spot_market::SpotMarket,
                    user::{PerpPosition, SpotPosition, User},
                },
            },
            relay_spec::AccountRefV0,
        };

        let (perp_key, spot_key) = (pdas::perp_market(0), pdas::spot_market(0));
        let mut perp = PerpMarket::default();
        create_anchor_account_info!(perp, &perp_key, PerpMarket, perp_info);
        let mut spot = SpotMarket::default();
        create_anchor_account_info!(spot, &spot_key, SpotMarket, spot_info);
        let stored_accounts = [spot_info, perp_info];
        let stored: Vec<AccountRefV0> = stored_accounts
            .iter()
            .map(|info| AccountRefV0::writable(info.key.to_bytes()))
            .collect();

        let mut user = User::default();
        user.perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 1,
            ..PerpPosition::default()
        };
        user.spot_positions[0] = SpotPosition {
            market_index: 0,
            scaled_balance: 1,
            ..SpotPosition::default()
        };
        assert!(entered_market_accounts(&user, &stored_accounts, &stored).is_empty());

        user.spot_positions[1] = SpotPosition {
            market_index: 1,
            scaled_balance: 1,
            ..SpotPosition::default()
        };
        user.perp_positions[1] = PerpPosition {
            market_index: 2,
            base_asset_amount: 1,
            ..PerpPosition::default()
        };
        let entered: Vec<Pubkey> = entered_market_accounts(&user, &stored_accounts, &stored)
            .iter()
            .map(|account| Pubkey::new_from_array(account.address))
            .collect();
        assert_eq!(
            entered,
            vec![
                pdas::perp_market(2),
                pdas::clob_crank_conditions(2),
                pdas::quoter_slab(2),
                pdas::spot_market(1),
            ]
        );
    }

    #[test]
    fn a_resync_of_unchanged_positions_is_not_paid() {
        assert!(!resync_earns_payment(5_000, 42, 42));
        assert!(resync_earns_payment(5_000, 42, 43));
        assert!(!resync_earns_payment(0, 42, 43));
    }

    /// A payment divides by the resyncs in the transaction. The instructions
    /// sysvar is serialized as a u16 count, one u16 offset per instruction,
    /// then each instruction and a trailing current index.
    #[test]
    fn resyncs_in_one_transaction_share_its_payment() {
        use solana_program::{
            instruction::Instruction,
            sysvar::instructions::{construct_instructions_data, BorrowedInstruction},
        };

        let data = crate::instruction::ResyncLiqConditions::DISCRIMINATOR.to_vec();
        let resync = Instruction::new_with_bytes(crate::ID, &data, vec![]);
        let other = Instruction::new_with_bytes(Pubkey::new_unique(), &data, vec![]);
        fn borrowed(ix: &Instruction) -> BorrowedInstruction<'_> {
            BorrowedInstruction {
                program_id: &ix.program_id,
                accounts: vec![],
                data: &ix.data,
            }
        }

        let share = |instructions: &[&Instruction]| {
            let mut bytes = construct_instructions_data(
                &instructions
                    .iter()
                    .map(|ix| borrowed(ix))
                    .collect::<Vec<_>>(),
            );
            let key = solana_program::sysvar::instructions::ID;
            let owner = Pubkey::default();
            let mut lamports = 0;
            let info =
                AccountInfo::new(&key, false, false, &mut lamports, &mut bytes, &owner, false);
            batch_share(20_000, &info)
        };

        assert_eq!(share(&[&resync]), 20_000);
        assert_eq!(share(&[&resync, &resync, &other, &resync, &resync]), 5_000);
        // The current index is 0, so `other` runs at the top level and the
        // resync is its CPI.
        assert_eq!(share(&[&other, &resync]), 0);
    }
}
