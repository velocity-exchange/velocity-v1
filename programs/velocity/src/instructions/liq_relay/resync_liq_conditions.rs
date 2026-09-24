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
//! The keeper is paid from the protocol crank treasury.
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
    anchor_lang::prelude::*,
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
}

/// Resyncs a user's liquidation conditions along relay's permissionless path.
///
/// The block carries its own terms. Re-deriving them from the account would
/// let a staged resync re-price itself.
pub fn handle_resync_liq_conditions<'c: 'info, 'info>(
    ctx: Context<'info, ResyncLiqConditions<'info>>,
) -> Result<()> {
    let args = {
        let conditions = ctx.accounts.liq_conditions.load()?;
        SyncLiqConditionsTerms {
            sync_payment_lamports: conditions.sync_payment_lamports,
            sync_fallback_slots: conditions.sync_fallback_slots,
        }
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
    if payment == 0 {
        return Ok(());
    }

    // Paid at most once per fallback interval. Opting in is permissionless
    // and the treasury pays even when nothing moved, so without this bound
    // anyone could loop a crank on the same account and draw the fee every
    // time. The interval matches the fallback poll's own cadence.
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
        payment.min(available),
        rent_minimum,
    )?;
    let mut treasury_state = ctx.accounts.treasury.load_mut()?;
    treasury_state.total_paid = treasury_state.total_paid.saturating_add(paid);
    Ok(())
}

/// Resolver for the self-sync conditions. It reports work when the user's
/// positions no longer match the thresholds the block was built from.
///
/// The match test is cheap and conservative. The resolver cannot re-derive a
/// threshold without the market and oracle accounts, which its short account
/// list cannot carry. It compares the shape of the user's exposures against
/// the recorded slots instead, so it catches a new market, a closed position,
/// or a first opt-in with no slots yet. The staged executor recomputes
/// everything from the account list the last sync stored.
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
        let stale = {
            let conditions = ctx.accounts.liq_conditions.load()?;
            let user = crate::load!(ctx.accounts.user)?;
            // A digest mismatch means the thresholds came from different
            // exposures. The sync stamps the digest it ran against, so a
            // rewrite that produces no watchable threshold still stops the
            // wake.
            UserConditionsV0::digest_positions(&user) != conditions.positions_digest
        };

        if !stale {
            return Ok(None);
        }

        // The builder enforces the no-signer rule for every resolver. That
        // rule is why this instruction exists.
        Ok(Some(
            crate::staged_call!(ResyncLiqConditions {
                keeper: crate::state::pdas::keeper_placeholder(),
                user: ctx.accounts.user.key(),
                liq_conditions: ctx.accounts.liq_conditions.key(),
                treasury: crate::state::pdas::crank_treasury(),
            })
            // The margin-map + reservoir accounts the last sync stored.
            .refs(ctx.accounts.liq_conditions.load()?.read_sync_accounts()),
        ))
    })
}
