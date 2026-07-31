//! Resolver for the self-sync conditions: report work when the user's
//! positions no longer match the thresholds the block was built from.
//!
//! "No longer match" is deliberately cheap and conservative — the resolver
//! cannot re-derive thresholds without the market/oracle accounts (a
//! four-account list can't carry them), so it compares the *shape* of the
//! user's exposures against the recorded slots: a new market, a closed
//! position, or a flipped direction means the hints are stale. The staged
//! executor is the real sync, which recomputes everything from the account
//! list it stored last time.

use {
    crate::{
        error::ErrorCode,
        state::{liq_conditions::LiqConditionsV0, user::User},
    },
    anchor_lang::prelude::*,
    relay_spec::{ResolvedCrankV0, KEEPER_PLACEHOLDER},
    solana_program::{instruction::AccountMeta, program::set_return_data},
};

#[derive(Accounts)]
pub struct ResolveSyncLiqConditions<'info> {
    /// Writable only for the staging region; simulation-only.
    #[account(mut, constraint = liq_conditions.load()?.user == user.key())]
    pub liq_conditions: AccountLoader<'info, LiqConditionsV0>,
    pub user: AccountLoader<'info, User>,
}

pub fn handle_resolve_sync_liq_conditions(ctx: Context<ResolveSyncLiqConditions>) -> Result<()> {
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
        // A live perp market with no slot, or a slot whose market the user
        // no longer trades: the hints were built for different exposures.
        let missing = live.iter().any(|market| !watched.contains(market));
        let orphaned = watched
            .iter()
            .any(|market| !live.contains(market) && !live.is_empty());
        // A user with no thresholds at all but real exposure needs a sync
        // (e.g. first opt-in, or a block reset by a full close).
        let never_synced = watched.is_empty() && !live.is_empty();
        missing || orphaned || never_synced
    };
    if !stale {
        return crate::instructions::no_work();
    }

    let conditions_key = ctx.accounts.liq_conditions.key();
    let mut metas = crate::accounts::SyncLiqConditions {
        payer: Pubkey::new_from_array(KEEPER_PLACEHOLDER),
        user: ctx.accounts.user.key(),
        liq_conditions: conditions_key,
        rent: <Rent as anchor_lang::solana_program::sysvar::SysvarId>::id(),
        system_program: System::id(),
    }
    .to_account_metas(None);
    // The account list the last sync stored — markets, oracles, and the
    // markets' crank conditions.
    for r in ctx.accounts.liq_conditions.load()?.read_sync_accounts() {
        let pubkey = Pubkey::new_from_array(r.address);
        metas.push(if r.writable != 0 {
            AccountMeta::new(pubkey, false)
        } else {
            AccountMeta::new_readonly(pubkey, false)
        });
    }

    let (payment, fallback) = {
        let conditions = ctx.accounts.liq_conditions.load()?;
        (
            conditions.sync_payment_lamports,
            conditions.sync_fallback_slots,
        )
    };
    let mut args = Vec::with_capacity(16);
    crate::instructions::SyncLiqConditionsArgs {
        sync_payment_lamports: payment,
        sync_fallback_slots: fallback,
    }
    .serialize(&mut args)?;
    let resolved = ResolvedCrankV0 {
        accounts: crate::instructions::to_account_refs(metas),
        data: args,
    };
    let pointer = ctx.accounts.liq_conditions.load_mut()?.stage(&resolved)?;
    set_return_data(&pointer);
    Ok(())
}
