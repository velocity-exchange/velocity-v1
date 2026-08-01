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
//! sync created it), and pays its keeper from the conditions account's own
//! lamports.

use {
    super::sync_liq_conditions::{rewrite_liq_conditions, SyncLiqConditionsArgs},
    crate::{
        error::ErrorCode,
        state::{
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
}

pub fn handle_resync_liq_conditions<'c: 'info, 'info>(
    ctx: Context<'info, ResyncLiqConditions<'info>>,
) -> Result<()> {
    // The block carries its own terms: re-deriving them from the account
    // means a staged resync can never re-price itself.
    let args = {
        let conditions = ctx.accounts.liq_conditions.load()?;
        SyncLiqConditionsArgs {
            sync_payment_lamports: conditions.sync_payment_lamports,
            sync_fallback_slots: conditions.sync_fallback_slots,
        }
    };
    rewrite_liq_conditions(
        &ctx.accounts.liq_conditions,
        &ctx.accounts.user,
        ctx.remaining_accounts,
        args,
    )?;

    let info = ctx.accounts.liq_conditions.to_account_info();
    let rent_minimum = Rent::get()?.minimum_balance(info.data_len());
    UserConditionsV0::pay_sync_keeper(
        &info,
        &ctx.accounts.keeper.to_account_info(),
        args.sync_payment_lamports,
        rent_minimum,
    )?;
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
    /// Writable only for the staging region; simulation-only.
    #[account(mut, constraint = liq_conditions.load()?.user == user.key())]
    pub liq_conditions: AccountLoader<'info, UserConditionsV0>,
    pub user: AccountLoader<'info, User>,
}

pub fn handle_resolve_resync_liq_conditions(
    ctx: Context<ResolveResyncLiqConditions>,
) -> Result<()> {
    let conditions = ctx.accounts.liq_conditions.clone();
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
            crate::instructions::StagedCall::new(crate::accounts::ResyncLiqConditions {
                keeper: crate::state::pdas::keeper_placeholder(),
                user: ctx.accounts.user.key(),
                liq_conditions: ctx.accounts.liq_conditions.key(),
            })
            // The margin-map + reservoir accounts the last sync stored.
            .refs(ctx.accounts.liq_conditions.load()?.read_sync_accounts()),
        ))
    })
}
