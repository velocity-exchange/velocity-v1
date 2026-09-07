use {
    crate::{
        constraints::{
            is_protocol_for_vault, is_user_for_vault, is_user_stats_for_vault,
            is_vault_protocol_for_vault,
        },
        refresh_velocity_spot_market, AccountMapProvider, Vault, VaultProtocol,
    },
    anchor_lang::prelude::*,
    velocity::{math::casting::Cast, program::Velocity, state::user::User},
};

pub fn protocol_cancel_withdraw_request<'info>(
    ctx: Context<'info, ProtocolCancelWithdrawRequest<'info>>,
) -> Result<()> {
    // Book the lending interest of every market that prices NAV BEFORE any account
    // is borrowed and before NAV is snapshotted (OtterSec #136/#137). Must precede
    // `load_mut`/`load_maps`: `invoke` rejects a CPI whose writable accounts still
    // have live borrows, and the maps must read post-refresh data.
    refresh_velocity_spot_market!(ctx);

    let clock = &Clock::get()?;
    let vault = &mut ctx.accounts.vault.load_mut()?;

    let mut vp = Some(ctx.accounts.vault_protocol.load_mut()?);

    let user = ctx.accounts.velocity_user.load()?;

    let mut maps = ctx.load_maps(
        clock.slot,
        None,
        vp.is_some(),
        false,
        &ctx.accounts.velocity_state,
    )?;

    let vault_equity = vault.calculate_equity(&user, &mut maps)?;

    let spot_market = maps.spot_market_map.get_ref(&vault.spot_market_index)?;
    let oracle = maps.oracle_map.get_price_data(&spot_market.oracle_id())?;

    vault.protocol_cancel_withdraw_request(
        &mut vp,
        &mut None,
        vault_equity.cast()?,
        clock.unix_timestamp,
        oracle.price,
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct ProtocolCancelWithdrawRequest<'info> {
    #[account(
        mut,
        constraint = is_protocol_for_vault(&vault, &vault_protocol, &protocol)?
    )]
    pub vault: AccountLoader<'info, Vault>,
    #[account(
        mut,
        constraint = is_vault_protocol_for_vault(&vault_protocol, &vault)?
    )]
    pub vault_protocol: AccountLoader<'info, VaultProtocol>,
    pub protocol: Signer<'info>,
    #[account(
        constraint = is_user_stats_for_vault(&vault, &velocity_user_stats.key())?
    )]
    /// CHECK: unused, for future proofing
    pub velocity_user_stats: AccountInfo<'info>,
    #[account(
        constraint = is_user_for_vault(&vault, &velocity_user.key())?
    )]
    pub velocity_user: AccountLoader<'info, User>,
    /// CHECK: checked in velocity cpi
    pub velocity_state: AccountInfo<'info>,
    pub velocity_program: Program<'info, Velocity>,
}
