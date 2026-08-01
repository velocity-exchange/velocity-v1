//! Derive a user's whole relay condition block in one instruction.
//!
//! Liquidation thresholds and trigger-order watches live in one account
//! and are invalidated by the same event — the user's positions or orders
//! changing — so syncing them separately meant two transactions, two
//! classifications of the same `remaining_accounts`, and two writes of the
//! same margin-map list, to end up at one account's contents.
//!
//! The two passes stay separate functions because they compute unrelated
//! things; what they share is the account list, which the liquidation pass
//! stores and the trigger pass then reuses instead of rewriting.

use {
    crate::{
        instructions::{rewrite_liq_conditions, rewrite_trigger_conditions, SyncLiqConditionsArgs},
        state::{
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
    // Liquidation first: it is the pass that stores the shared resolver
    // account list, so the trigger pass can skip writing it.
    rewrite_liq_conditions(
        &ctx.accounts.user_conditions,
        &ctx.accounts.user,
        ctx.remaining_accounts,
        args,
    )?;
    rewrite_trigger_conditions(
        &ctx.accounts.user_conditions,
        &ctx.accounts.user,
        ctx.remaining_accounts,
        false,
    )
}
