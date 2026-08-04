use {
    crate::{
        constraints::{is_manager_for_vault, is_user_for_vault, is_user_stats_for_vault},
        refresh_velocity_spot_market,
        state::VaultProtocolProvider,
        AccountMapProvider, Vault, WithdrawUnit,
    },
    anchor_lang::prelude::*,
    velocity::{
        instructions::optional_accounts::AccountMaps,
        program::Velocity,
        state::{spot_market::SpotMarket, user::User},
    },
};

pub fn manager_request_withdraw<'info>(
    ctx: Context<'info, ManagerRequestWithdraw<'info>>,
    withdraw_amount: u64,
    withdraw_unit: WithdrawUnit,
) -> Result<()> {
    // Advance the denomination market's `cumulative_deposit_interest` BEFORE any
    // account is borrowed and before NAV is snapshotted (OtterSec #136/#137).
    // Must precede `load_mut`/`load_maps`: `invoke` rejects a CPI whose writable
    // accounts still have live borrows, and the maps must read post-refresh data.
    refresh_velocity_spot_market!(ctx);

    let clock = &Clock::get()?;
    let mut vault = ctx.accounts.vault.load_mut()?;
    let now = clock.unix_timestamp;

    // backwards compatible: if last rem acct does not deserialize into [`VaultProtocol`] then it's a legacy vault.
    let mut vp = ctx.vault_protocol();
    vault.validate_vault_protocol(&vp)?;
    let mut vp = vp.as_mut().map(|vp| vp.load_mut()).transpose()?;

    let user = ctx.accounts.velocity_user.load()?;
    let spot_market_index = vault.spot_market_index;

    let AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = ctx.load_maps(clock.slot, Some(spot_market_index), vp.is_some(), false)?;

    let vault_equity =
        vault.calculate_equity(&user, &perp_market_map, &spot_market_map, &mut oracle_map)?;

    let spot_market = spot_market_map.get_ref(&spot_market_index)?;
    let oracle = oracle_map.get_price_data(&spot_market.oracle_id())?;

    vault.manager_request_withdraw(
        &mut vp,
        &mut None,
        withdraw_amount,
        withdraw_unit,
        vault_equity,
        now,
        oracle.price,
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct ManagerRequestWithdraw<'info> {
    #[account(
        mut,
        constraint = is_manager_for_vault(&vault, &manager)?
    )]
    pub vault: AccountLoader<'info, Vault>,
    pub manager: Signer<'info>,
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
    pub velocity_program: Program<'info, Velocity>,
}
