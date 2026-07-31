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
            liq_conditions::{LiqConditionsV0, LIQ_CONDITIONS_PDA_SEED},
            user::User,
        },
    },
    anchor_lang::prelude::*,
    relay_spec::{ResolvedCrankV0, KEEPER_PLACEHOLDER},
    solana_program::{instruction::AccountMeta, program::set_return_data},
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
        seeds = [LIQ_CONDITIONS_PDA_SEED, user.key().as_ref()],
        bump,
        constraint = liq_conditions.load()?.user == user.key()
    )]
    pub liq_conditions: AccountLoader<'info, LiqConditionsV0>,
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
    LiqConditionsV0::pay_sync_keeper(
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
    /// Writable only for the staging region; simulation-only.
    #[account(mut, constraint = liq_conditions.load()?.user == user.key())]
    pub liq_conditions: AccountLoader<'info, LiqConditionsV0>,
    pub user: AccountLoader<'info, User>,
}

pub fn handle_resolve_resync_liq_conditions(
    ctx: Context<ResolveResyncLiqConditions>,
) -> Result<()> {
    let stale = {
        let conditions = ctx.accounts.liq_conditions.load()?;
        let user = crate::load!(ctx.accounts.user)?;
        let watched: Vec<u16> = conditions
            .slots
            .iter()
            .filter(|slot| slot.active != 0)
            .map(|slot| slot.target_market_index)
            .collect();
        let live: Vec<u16> = user
            .perp_positions
            .iter()
            .filter(|p| p.base_asset_amount != 0)
            .map(|p| p.market_index)
            .collect();
        // Stale iff the watched set and the live set disagree. Closing
        // every position counts (orphaned slots would otherwise keep
        // level-triggered wakes armed against exposures that are gone),
        // and the rewrite converges: once both sets are empty, no work.
        let missing = live.iter().any(|market| !watched.contains(market));
        let orphaned = watched.iter().any(|market| !live.contains(market));
        missing || orphaned
    };
    if !stale {
        return crate::instructions::no_work();
    }

    let mut metas = crate::accounts::ResyncLiqConditions {
        keeper: Pubkey::new_from_array(KEEPER_PLACEHOLDER),
        user: ctx.accounts.user.key(),
        liq_conditions: ctx.accounts.liq_conditions.key(),
    }
    .to_account_metas(None);
    // The margin-map + reservoir accounts the last sync stored.
    for r in ctx.accounts.liq_conditions.load()?.read_sync_accounts() {
        let pubkey = Pubkey::new_from_array(r.address);
        metas.push(if r.writable != 0 {
            AccountMeta::new(pubkey, false)
        } else {
            AccountMeta::new_readonly(pubkey, false)
        });
    }
    // Belt for the rule this whole instruction exists to satisfy.
    require!(
        metas.iter().all(|meta| !meta.is_signer),
        ErrorCode::DefaultError
    );

    let resolved = ResolvedCrankV0 {
        accounts: crate::instructions::to_account_refs(metas),
        data: Vec::new(),
    };
    let pointer = ctx.accounts.liq_conditions.load_mut()?.stage(&resolved)?;
    set_return_data(&pointer);
    Ok(())
}
