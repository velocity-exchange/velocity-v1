//! Derive a user's whole relay condition block in one instruction.
//!
//! The liquidation conditions and the trigger-order watches live in one
//! account, and the same event invalidates both. That event is a change to the
//! user's positions or orders. Syncing them separately cost two transactions,
//! two classifications of the same `remaining_accounts`, and two writes of the
//! same margin-map list, for one account's contents.
//!
//! The two passes stay separate functions because they compute unrelated
//! things. They share the account list. The liquidation pass stores it and the
//! trigger pass reuses it instead of rewriting it.

use {
    crate::{
        instructions::{
            price_sync_terms, rewrite_liq_conditions, rewrite_trigger_conditions,
            SyncLiqConditionsArgs,
        },
        state::{
            state::State,
            user::User,
            user_conditions::{UserConditionsV0, USER_CONDITIONS_PDA_SEED},
        },
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct SyncUserConditions<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// Read for the fee rails the sync's own keeper payment is priced from.
    pub state: AccountLoader<'info, State>,
    pub user: AccountLoader<'info, User>,
    #[account(
        init_if_needed,
        seeds = [USER_CONDITIONS_PDA_SEED, user.key().as_ref()],
        space = UserConditionsV0::SIZE,
        bump,
        payer = payer
    )]
    pub user_conditions: AccountLoader<'info, UserConditionsV0>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

pub fn handle_sync_user_conditions<'c: 'info, 'info>(
    ctx: Context<'info, SyncUserConditions<'info>>,
    args: SyncLiqConditionsArgs,
) -> Result<()> {
    // One shared helper prices the sync, so this entry point and the
    // liquidation-only one cannot disagree about what a sync costs.
    let state_rails = ctx.accounts.state.load()?.transaction_fee_rails;
    let terms = price_sync_terms(&state_rails, &args)?;
    // The liquidation pass runs first because it stores the shared resolver
    // account list. The trigger pass can then skip writing it.
    rewrite_liq_conditions(
        &ctx.accounts.user_conditions,
        &ctx.accounts.user,
        ctx.remaining_accounts,
        terms,
        // An opt-in sync, so the next resync the treasury pays for is an
        // interval away.
        true,
    )?;
    rewrite_trigger_conditions(
        &ctx.accounts.user_conditions,
        &ctx.accounts.user,
        ctx.remaining_accounts,
        false,
    )
}
