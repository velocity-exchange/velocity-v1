//! Flag an existing vault's velocity User as vault-owned.
//!
//! Vault initialization sets the flag, but a vault created before that change
//! has a User without it. The revenue-share sweep can then credit that User,
//! which dilutes the depositors against a timed sweep. Only the vault PDA can
//! sign `update_user_vault_owned`, so this instruction signs for it.

use {
    crate::{
        constraints::{is_admin, is_manager_for_vault, is_user_for_vault},
        declare_vault_seeds, Vault,
    },
    anchor_lang::prelude::*,
    velocity::{cpi::accounts::UpdateUser, program::Velocity, state::user::User},
};

/// The flag is set only and never cleared, so a repeated call does nothing.
pub fn mark_user_vault_owned<'info>(ctx: Context<'info, MarkUserVaultOwned<'info>>) -> Result<()> {
    declare_vault_seeds!(ctx.accounts.vault, seeds);

    let cpi_accounts = UpdateUser {
        user: ctx.accounts.velocity_user.to_account_info(),
        authority: ctx.accounts.vault.to_account_info(),
    };
    let cpi_context =
        CpiContext::new_with_signer(ctx.accounts.velocity_program.key(), cpi_accounts, seeds);
    velocity::cpi::update_user_vault_owned(cpi_context, 0)?;

    Ok(())
}

#[derive(Accounts)]
pub struct MarkUserVaultOwned<'info> {
    #[account(
        constraint = is_manager_for_vault(&vault, &authority)? || is_admin(&authority)?,
    )]
    pub vault: AccountLoader<'info, Vault>,
    /// The vault manager or the vaults admin.
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = is_user_for_vault(&vault, &velocity_user.key())?
    )]
    pub velocity_user: AccountLoader<'info, User>,
    pub velocity_program: Program<'info, Velocity>,
}
