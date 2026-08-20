use {
    crate::{
        constraints::{
            is_authority_for_vault_depositor, is_user_for_vault, is_user_stats_for_vault,
        },
        refresh_velocity_spot_market,
        state::{
            account_maps::AccountMapProvider, FeeUpdateProvider, FeeUpdateStatus, Vault,
            VaultProtocolProvider,
        },
        VaultDepositor, WithdrawUnit,
    },
    anchor_lang::prelude::*,
    velocity::{
        instructions::optional_accounts::AccountMaps,
        math::casting::Cast,
        program::Velocity,
        state::user::{User, UserStats},
    },
};

pub fn request_withdraw<'info>(
    ctx: Context<'info, RequestWithdraw<'info>>,
    withdraw_amount: u64,
    withdraw_unit: WithdrawUnit,
) -> Result<()> {
    // Book the lending interest of every market that prices NAV BEFORE any account
    // is borrowed and before NAV is snapshotted (OtterSec #136/#137). Must precede
    // `load_mut`/`load_maps`: `invoke` rejects a CPI whose writable accounts still
    // have live borrows, and the maps must read post-refresh data.
    refresh_velocity_spot_market!(ctx);

    let clock = &Clock::get()?;
    let vault = &mut ctx.accounts.vault.load_mut()?;
    let mut vault_depositor = ctx.accounts.vault_depositor.load_mut()?;

    let user = ctx.accounts.velocity_user.load()?;

    let mut vp = ctx.vault_protocol();
    vault.validate_vault_protocol(&vp)?;
    let mut vp = vp.as_mut().map(|vp| vp.load_mut()).transpose()?;

    let has_fee_update = FeeUpdateStatus::has_pending_fee_update(vault.fee_update_status);
    let mut fee_update = ctx.fee_update(vp.is_some(), has_fee_update);
    vault.validate_fee_update(&fee_update)?;

    let AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = ctx.load_maps(
        clock.slot,
        None,
        vp.is_some(),
        has_fee_update,
        Some(&ctx.accounts.velocity_state),
    )?;

    let vault_equity =
        vault.calculate_equity(&user, &perp_market_map, &spot_market_map, &mut oracle_map)?;

    let spot_market = spot_market_map.get_ref(&vault.spot_market_index)?;
    let oracle = oracle_map.get_price_data(&spot_market.oracle_id())?;

    vault_depositor.request_withdraw(
        withdraw_amount.cast()?,
        withdraw_unit,
        vault_equity,
        vault,
        &mut vp,
        &mut fee_update,
        clock.unix_timestamp,
        oracle.price,
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct RequestWithdraw<'info> {
    #[account(mut)]
    pub vault: AccountLoader<'info, Vault>,
    #[account(
        mut,
        seeds = [b"vault_depositor", vault.key().as_ref(), authority.key().as_ref()],
        bump,
        constraint = is_authority_for_vault_depositor(&vault_depositor, &authority)?,
    )]
    pub vault_depositor: AccountLoader<'info, VaultDepositor>,
    pub authority: Signer<'info>,
    #[account(
        constraint = is_user_stats_for_vault(&vault, &velocity_user_stats.key())?
    )]
    pub velocity_user_stats: AccountLoader<'info, UserStats>,
    #[account(
        constraint = is_user_for_vault(&vault, &velocity_user.key())?
    )]
    pub velocity_user: AccountLoader<'info, User>,
    /// CHECK: checked in velocity cpi
    pub velocity_state: AccountInfo<'info>,
    pub velocity_program: Program<'info, Velocity>,
}
