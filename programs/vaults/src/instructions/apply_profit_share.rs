use {
    super::constraints::is_admin,
    crate::{
        constraints::{
            is_delegate_for_vault, is_manager_for_vault, is_user_for_vault,
            is_user_stats_for_vault, is_vault_for_vault_depositor,
        },
        error::ErrorCode,
        refresh_velocity_spot_market,
        state::{FeeUpdateProvider, FeeUpdateStatus, Vault, VaultProtocolProvider},
        validate, AccountMapProvider, VaultDepositor,
    },
    anchor_lang::prelude::*,
    velocity::{
        instructions::optional_accounts::AccountMaps,
        program::Velocity,
        state::user::{User, UserStats},
    },
};

pub fn apply_profit_share<'info>(ctx: Context<'info, ApplyProfitShare<'info>>) -> Result<()> {
    // Book the lending interest of every market that prices NAV BEFORE any account
    // is borrowed and before NAV is snapshotted (OtterSec #136/#137). Must precede
    // `load_mut`/`load_maps`: `invoke` rejects a CPI whose writable accounts still
    // have live borrows, and the maps must read post-refresh data.
    refresh_velocity_spot_market!(ctx);

    let clock = &Clock::get()?;

    let mut vault = ctx.accounts.vault.load_mut()?;
    let mut vault_depositor = ctx.accounts.vault_depositor.load_mut()?;

    // backwards compatible: if last rem acct does not deserialize into [`VaultProtocol`] then it's a legacy vault.
    let mut vp = ctx.vault_protocol();
    vault.validate_vault_protocol(&vp)?;
    let mut vp = vp.as_mut().map(|vp| vp.load_mut()).transpose()?;

    let user = ctx.accounts.velocity_user.load()?;
    let spot_market_index = vault.spot_market_index;

    let has_fee_update = FeeUpdateStatus::has_pending_fee_update(vault.fee_update_status);
    let mut fee_update = ctx.fee_update(vp.is_some(), has_fee_update);
    vault.validate_fee_update(&fee_update)?;

    if is_admin(&ctx.accounts.manager)? {
        validate!(
            has_fee_update,
            ErrorCode::InvalidFeeUpdateStatus,
            "Admin can only force apply fees if a fee update is pending"
        )?;
    }

    let AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = ctx.load_maps(
        clock.slot,
        Some(spot_market_index),
        vp.is_some(),
        has_fee_update,
        Some(&ctx.accounts.velocity_state),
    )?;

    let vault_equity =
        vault.calculate_equity(&user, &perp_market_map, &spot_market_map, &mut oracle_map)?;

    let spot_market = spot_market_map.get_ref(&spot_market_index)?;
    let oracle = oracle_map.get_price_data(&spot_market.oracle_id())?;

    vault_depositor.realize_profits(
        vault_equity,
        &mut vault,
        &mut vp,
        &mut fee_update,
        clock.unix_timestamp,
        oracle.price,
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct ApplyProfitShare<'info> {
    #[account(
        mut,
        constraint = is_manager_for_vault(&vault, &manager)? || is_delegate_for_vault(&vault, &manager)? || is_admin(&manager)?
    )]
    pub vault: AccountLoader<'info, Vault>,
    #[account(
        mut,
        constraint = is_vault_for_vault_depositor(&vault_depositor, &vault)?
    )]
    pub vault_depositor: AccountLoader<'info, VaultDepositor>,
    pub manager: Signer<'info>,
    #[account(
        mut,
        constraint = is_user_stats_for_vault(&vault, &velocity_user_stats.key())?
    )]
    /// CHECK: checked in velocity cpi
    pub velocity_user_stats: AccountLoader<'info, UserStats>,
    #[account(
        mut,
        constraint = is_user_for_vault(&vault, &velocity_user.key())?
    )]
    /// CHECK: checked in velocity cpi
    pub velocity_user: AccountLoader<'info, User>,
    /// CHECK: checked in velocity cpi
    pub velocity_state: AccountInfo<'info>,
    /// CHECK: checked in velocity cpi
    pub velocity_signer: AccountInfo<'info>,
    pub velocity_program: Program<'info, Velocity>,
}
