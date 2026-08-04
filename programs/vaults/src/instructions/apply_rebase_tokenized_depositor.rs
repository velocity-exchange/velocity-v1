use {
    crate::{
        constraints::{is_tokenized_depositor_for_vault, is_user_for_vault},
        refresh_velocity_spot_market,
        state::traits::VaultDepositorBase,
        AccountMapProvider, TokenizedVaultDepositor, Vault, VaultProtocolProvider,
    },
    anchor_lang::prelude::*,
    velocity::{
        instructions::optional_accounts::AccountMaps,
        program::Velocity,
        state::{spot_market::SpotMarket, user::User},
    },
};

pub fn apply_rebase_tokenized_depositor<'info>(
    ctx: Context<'info, ApplyRebaseTokenizedDepositor<'info>>,
) -> Result<()> {
    // Advance the denomination market's `cumulative_deposit_interest` BEFORE any
    // account is borrowed and before NAV is snapshotted (OtterSec #136/#137).
    // Must precede `load_mut`/`load_maps`: `invoke` rejects a CPI whose writable
    // accounts still have live borrows, and the maps must read post-refresh data.
    refresh_velocity_spot_market!(ctx);

    let clock = &Clock::get()?;

    let mut vault = ctx.accounts.vault.load_mut()?;

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

    ctx.accounts
        .tokenized_vault_depositor
        .load_mut()?
        .apply_rebase(&mut vault, &mut vp, vault_equity)?;

    Ok(())
}

#[derive(Accounts)]
pub struct ApplyRebaseTokenizedDepositor<'info> {
    #[account(mut)]
    pub vault: AccountLoader<'info, Vault>,
    #[account(
        mut,
        constraint = is_tokenized_depositor_for_vault(&tokenized_vault_depositor, &vault)?
    )]
    pub tokenized_vault_depositor: AccountLoader<'info, TokenizedVaultDepositor>,
    #[account(
        mut,
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
