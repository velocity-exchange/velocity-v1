use {
    crate::{
        constraints::{
            is_authority_for_vault_depositor, is_user_for_vault, is_vault_for_vault_depositor,
        },
        error::ErrorCode,
        refresh_velocity_spot_market,
        state::{traits::VaultDepositorBase, FeeUpdateProvider, FeeUpdateStatus},
        validate, AccountMapProvider, Vault, VaultDepositor, VaultProtocolProvider, WithdrawUnit,
    },
    anchor_lang::prelude::*,
    velocity::{
        instructions::optional_accounts::AccountMaps,
        math::safe_math::SafeMath,
        program::Velocity,
        state::{spot_market::SpotMarket, user::User},
    },
};

pub fn transfer_vault_depositor_shares<'info>(
    ctx: Context<'info, TransferVaultDepositorShares<'info>>,
    amount: u64,
    withdraw_unit: WithdrawUnit,
) -> Result<()> {
    // Advance the denomination market's `cumulative_deposit_interest` BEFORE any
    // account is borrowed and before NAV is snapshotted (OtterSec #136/#137).
    // Must precede `load_mut`/`load_maps`: `invoke` rejects a CPI whose writable
    // accounts still have live borrows, and the maps must read post-refresh data.
    refresh_velocity_spot_market!(ctx);

    let clock = &Clock::get()?;

    let mut vault = ctx.accounts.vault.load_mut()?;

    validate!(!vault.in_liquidation(), ErrorCode::OngoingLiquidation)?;

    validate!(
        ctx.accounts.vault_depositor.key() != ctx.accounts.to_vault_depositor.key(),
        ErrorCode::InvalidVaultDeposit,
        "Cannot transfer shares to the same depositor"
    )?;

    validate!(
        amount > 0,
        ErrorCode::InvalidVaultWithdrawSize,
        "Transfer amount must be greater than 0"
    )?;

    let mut vault_depositor = ctx.accounts.vault_depositor.load_mut()?;
    let mut to_vault_depositor = ctx.accounts.to_vault_depositor.load_mut()?;

    // backwards compatible: if last rem acct does not deserialize into [`VaultProtocol`] then it's a legacy vault.
    let mut vp = ctx.vault_protocol();
    vault.validate_vault_protocol(&vp)?;
    let mut vp = vp.as_mut().map(|vp| vp.load_mut()).transpose()?;

    // #101: apply a matured fee update on this share-movement path (mirrors deposit/withdraw), so
    // a queued profit-share/management-fee increase can't be escaped by moving shares and resetting
    // the recipient's cost basis under stale fee terms.
    let has_fee_update = FeeUpdateStatus::has_pending_fee_update(vault.fee_update_status);
    let mut fee_update = ctx.fee_update(vp.is_some(), has_fee_update);
    vault.validate_fee_update(&fee_update)?;

    let user = ctx.accounts.velocity_user.load()?;
    let spot_market_index = vault.spot_market_index;

    let AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = ctx.load_maps(
        clock.slot,
        Some(spot_market_index),
        vp.is_some(),
        has_fee_update,
    )?;

    let vault_equity =
        vault.calculate_equity(&user, &perp_market_map, &spot_market_map, &mut oracle_map)?;

    validate!(
        !vault_depositor.last_withdraw_request.pending(),
        ErrorCode::InvalidVaultDeposit,
        "Cannot transfer shares with a pending withdraw request"
    )?;

    validate!(
        !to_vault_depositor.last_withdraw_request.pending(),
        ErrorCode::InvalidVaultDeposit,
        "Cannot transfer shares to a depositor with a pending withdraw request"
    )?;

    let spot_market = spot_market_map.get_ref(&spot_market_index)?;
    let oracle = oracle_map.get_price_data(&spot_market.oracle_id())?;

    let total_shares_before = vault_depositor
        .get_vault_shares()
        .safe_add(to_vault_depositor.get_vault_shares())?;

    vault_depositor.transfer_shares(
        &mut *to_vault_depositor,
        &mut vault,
        &mut vp,
        &mut fee_update,
        amount,
        withdraw_unit,
        vault_equity,
        clock.unix_timestamp,
        oracle.price,
    )?;

    let total_shares_after = vault_depositor
        .get_vault_shares()
        .safe_add(to_vault_depositor.get_vault_shares())?;

    validate!(
        total_shares_after.eq(&total_shares_before),
        ErrorCode::InvalidVaultSharesDetected,
        "Total vault depositor shares before != after"
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct TransferVaultDepositorShares<'info> {
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
        mut,
        constraint = is_vault_for_vault_depositor(&to_vault_depositor, &vault)?,
    )]
    pub to_vault_depositor: AccountLoader<'info, VaultDepositor>,
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
