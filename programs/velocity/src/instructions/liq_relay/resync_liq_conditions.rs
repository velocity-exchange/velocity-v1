//! Relay's self-maintenance path for a user's liquidation conditions, and
//! its resolver.
//!
//! Separate from the opt-in [`super::sync_liq_conditions`] for one hard
//! reason: **a staged executor may not name a signer.** Relay's turner
//! builds every executor meta `is_signer: false` and refuses outright to
//! sign a transaction whose executor account list contains a signer —
//! executors are permissionless by construction, and a signing account
//! handed to one is a drain vector. So the instruction relay stages takes
//! no payer, allocates nothing (the account exists by then — the opt-in
//! sync created it), and pays its keeper from the protocol crank treasury.
//!
//! The treasury pays rather than the user's own conditions account, because a
//! stale threshold is a protocol problem before it is a user's: a resync that
//! nobody is paid to run leaves the user's liquidation thresholds behind their
//! real exposure, and the liquidation that should fire does not. Charging that
//! to the account being watched makes an underfunded user into protocol bad
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
    /// CHECK: the keeper payout target — relay's `KEEPER_PLACEHOLDER`
    /// slot. Never a signer (see the module doc); it only receives
    /// lamports.
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

pub fn handle_resync_liq_conditions<'c: 'info, 'info>(
    ctx: Context<'info, ResyncLiqConditions<'info>>,
) -> Result<()> {
    // The block carries its own terms: re-deriving them from the account
    // means a staged resync can never re-price itself.
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

    // Paid at most once per fallback interval. The instruction succeeds
    // whether or not anything moved, opting in is permissionless, and the
    // payer is the protocol treasury rather than the account being watched —
    // so without this bound anyone could crank the same account in a loop and
    // draw the fee every time. The interval is the cadence the fallback poll
    // already runs at, so honest cranking is unaffected.
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
    // Best-effort, as it was when the user's own account paid: an empty
    // treasury must not fail a resync that has already rewritten the block.
    // The keeper is protected by relay's own payment guard, which skips work
    // that would not pay.
    let available = treasury.lamports().saturating_sub(rent_minimum);
    let paid = CrankTreasuryV0::pay_out(
        &treasury,
        &ctx.accounts.keeper.to_account_info(),
        args.sync_payment_lamports.min(available),
        rent_minimum,
    )?;
    let mut treasury_state = ctx.accounts.treasury.load_mut()?;
    treasury_state.total_paid = treasury_state.total_paid.saturating_add(paid);
    Ok(())
}

/// Resolver for the self-sync conditions: report work when the user's
/// positions no longer match the thresholds the block was built from.
///
/// "No longer match" is deliberately cheap and conservative — the resolver
/// cannot re-derive thresholds without the market/oracle accounts (a
/// four-account list can't carry them), so it compares the *shape* of the
/// user's exposures against the recorded slots: a new market, a closed
/// position, or a first opt-in with no slots yet. The staged executor
/// recomputes everything from the account list the last sync stored.
#[derive(Accounts)]
pub struct ResolveResyncLiqConditions<'info> {
    /// The shared staging account, index 0 by convention — a resolver's
    /// response pointer is interpreted against it.
    #[account(mut, seeds = [crate::state::relay_scratch::RELAY_SCRATCH_PDA_SEED], bump)]
    pub scratch: AccountLoader<'info, crate::state::relay_scratch::RelayScratchV0>,
    /// Read-only: resolvers stage into the shared scratch account, not
    /// into the block they read.
    #[account(constraint = liq_conditions.load()?.user == user.key())]
    pub liq_conditions: AccountLoader<'info, UserConditionsV0>,
    pub user: AccountLoader<'info, User>,
}

pub fn handle_resolve_resync_liq_conditions(
    ctx: Context<ResolveResyncLiqConditions>,
) -> Result<()> {
    crate::instructions::resolve_into(&ctx.accounts.scratch, || {
        let stale = {
            let conditions = ctx.accounts.liq_conditions.load()?;
            let user = crate::load!(ctx.accounts.user)?;
            // Digest mismatch = the thresholds were derived from different
            // exposures. Converges by construction: the sync stamps the digest
            // it ran against, so a rewrite that produces no watchable
            // threshold still stops the wake.
            UserConditionsV0::digest_positions(&user) != conditions.positions_digest
        };
        if !stale {
            return Ok(None);
        }
        // The no-signer rule this whole instruction exists to satisfy is
        // enforced by the builder for every resolver.
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
