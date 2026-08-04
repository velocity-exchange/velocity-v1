use {
    crate::{
        constants::admin,
        constraints::{
            is_admin, is_authority_key_for_vault_depositor, is_user_for_vault,
            is_user_stats_for_vault,
        },
        declare_vault_seeds, implement_update_user_delegate_cpi,
        implement_update_user_reduce_only_cpi, refresh_velocity_spot_market,
        state::{Vault, VaultDepositor},
        velocity_cpi::{UpdateUserDelegateCPI, UpdateUserReduceOnlyCPI},
        AccountMapProvider, VaultProtocolProvider,
    },
    anchor_lang::prelude::*,
    velocity::{
        cpi::accounts::UpdateUser,
        instructions::optional_accounts::AccountMaps,
        program::Velocity,
        state::{spot_market::SpotMarket, user::User},
    },
};

pub fn liquidate<'info>(ctx: Context<'info, Liquidate<'info>>) -> Result<()> {
    // Advance the denomination market's `cumulative_deposit_interest` BEFORE any
    // account is borrowed and before NAV is snapshotted (OtterSec #136/#137).
    // Must precede `load_mut`/`load_maps`: `invoke` rejects a CPI whose writable
    // accounts still have live borrows, and the maps must read post-refresh data.
    refresh_velocity_spot_market!(ctx);

    let clock = &Clock::get()?;
    let now = Clock::get()?.unix_timestamp;

    let mut user = ctx.accounts.velocity_user.load_mut()?;
    let mut vault = ctx.accounts.vault.load_mut()?;
    let vault_depositor = ctx.accounts.vault_depositor.load()?;

    // backwards compatible: if last rem acct does not deserialize into [`VaultProtocol`] then it's a legacy vault.
    let mut vp = ctx.vault_protocol();
    vault.validate_vault_protocol(&vp)?;
    let vp = vp.as_mut().map(|vp| vp.load_mut()).transpose()?;

    let AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = ctx.load_maps(
        clock.slot,
        Some(vault.spot_market_index),
        vp.is_some(),
        false,
    )?;

    // 1. Check the vault depositor has waited the redeem period
    vault_depositor
        .last_withdraw_request
        .check_redeem_period_finished(&vault, now)?;
    // 2. Check that the depositor is unable to withdraw
    let vault_equity =
        vault.calculate_equity(&user, &perp_market_map, &spot_market_map, &mut oracle_map)?;
    vault_depositor.check_cant_withdraw(
        &vault,
        vault_equity,
        &mut user,
        &perp_market_map,
        &spot_market_map,
        &mut oracle_map,
    )?;
    // 3. Check that the vault is not already in liquidation
    vault.check_available_for_liquidation(now)?;

    vault.set_liquidation_delegate(admin::ID, now);

    drop(user);
    drop(vault);
    drop(vp);

    ctx.velocity_update_user_delegate(admin::ID)?;
    ctx.velocity_update_user_reduce_only(true)?;

    Ok(())
}

#[derive(Accounts)]
pub struct Liquidate<'info> {
    #[account(mut)]
    pub vault: AccountLoader<'info, Vault>,
    #[account(
        mut,
        seeds = [b"vault_depositor", vault.key().as_ref(), authority.key().as_ref()],
        bump,
    )]
    pub vault_depositor: AccountLoader<'info, VaultDepositor>,
    #[account(
        constraint = is_authority_key_for_vault_depositor(&vault_depositor, &authority.key())?,
    )]
    /// CHECK: checked in constraints
    pub authority: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = is_admin(&admin)?,
    )]
    pub admin: Signer<'info>,
    #[account(
        mut,
        constraint = is_user_stats_for_vault(&vault, &velocity_user_stats.key())?
    )]
    /// CHECK: checked in velocity cpi
    pub velocity_user_stats: AccountInfo<'info>,
    #[account(
        mut,
        constraint = is_user_for_vault(&vault, &velocity_user.key())?
    )]
    /// CHECK: checked in velocity cpi
    pub velocity_user: AccountLoader<'info, User>,
    pub velocity_program: Program<'info, Velocity>,
    /// CHECK: checked in velocity cpi
    pub velocity_state: AccountInfo<'info>,
    /// The vault's denomination spot market, refreshed by CPI before NAV is
    /// snapshotted (OtterSec #136/#137). Writable because velocity advances its
    /// `cumulative_deposit_interest`.
    #[account(
        mut,
        seeds = [b"spot_market".as_ref(), vault.load()?.spot_market_index.to_le_bytes().as_ref()],
        bump,
        seeds::program = velocity_program.key(),
    )]
    pub velocity_spot_market: AccountLoader<'info, SpotMarket>,
    /// CHECK: must be `velocity_spot_market.oracle`; enforced by velocity's
    /// `valid_oracle_for_spot_market` access control on the refresh CPI.
    pub velocity_oracle: AccountInfo<'info>,
    /// CHECK: PDA-pinned to the denomination market's velocity vault;
    /// deserialized and validated inside the refresh CPI.
    #[account(
        seeds = [b"spot_market_vault".as_ref(), vault.load()?.spot_market_index.to_le_bytes().as_ref()],
        bump,
        seeds::program = velocity_program.key(),
    )]
    pub velocity_spot_market_vault: AccountInfo<'info>,
}

impl<'info> UpdateUserDelegateCPI for Context<'info, Liquidate<'info>> {
    fn velocity_update_user_delegate(&self, delegate: Pubkey) -> Result<()> {
        implement_update_user_delegate_cpi!(self, delegate);
        Ok(())
    }
}

impl<'info> UpdateUserReduceOnlyCPI for Context<'info, Liquidate<'info>> {
    fn velocity_update_user_reduce_only(&self, reduce_only: bool) -> Result<()> {
        implement_update_user_reduce_only_cpi!(self, reduce_only);
        Ok(())
    }
}
