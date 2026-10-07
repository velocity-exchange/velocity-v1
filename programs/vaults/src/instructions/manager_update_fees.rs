use {
    crate::{
        constants::ONE_WEEK,
        constraints::{is_admin, is_manager_for_vault, is_user_for_vault},
        error::ErrorCode,
        refresh_velocity_spot_market,
        state::{
            events::{FeeUpdateAction, FeeUpdateRecord},
            vault::validate_fee_policy,
            FeeUpdate, FeeUpdateStatus,
        },
        validate, AccountMapProvider, Vault, VaultProtocolProvider,
    },
    anchor_lang::prelude::*,
    velocity::{math::safe_math::SafeMath, program::Velocity, state::user::User},
};

pub fn manager_update_fees<'info>(
    ctx: Context<'info, ManagerUpdateFees<'info>>,
    params: ManagerUpdateFeesParams,
) -> Result<()> {
    // Book the lending interest of every market that prices NAV before any
    // account is borrowed and before NAV is snapshotted (OtterSec #136/#137). The refresh must run
    // before `load_mut` and `load_maps`. `invoke` rejects a CPI whose writable
    // accounts still have live borrows, and the maps must read refreshed data.
    refresh_velocity_spot_market!(ctx);

    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    let mut vault = ctx.accounts.vault.load_mut()?;
    let has_pending_fee_update = FeeUpdateStatus::has_pending_fee_update(vault.fee_update_status);

    validate!(!vault.in_liquidation(), ErrorCode::OngoingLiquidation)?;

    if is_admin(&ctx.accounts.manager)? {
        validate!(
            has_pending_fee_update,
            ErrorCode::InvalidFeeUpdateStatus,
            "Admin can only force update fees if a fee update is pending"
        )?;
    }

    let vp = ctx.vault_protocol();
    vault.validate_vault_protocol(&vp)?;
    let mut vp = vp.as_ref().map(|vp| vp.load_mut()).transpose()?;

    if has_pending_fee_update {
        validate!(
            ctx.accounts.fee_update.load()?.is_pending(),
            ErrorCode::InvalidFeeUpdateStatus,
            "Vault has pending fee status but FeeUpdate is not in a pending state"
        )?;

        // The install needs the vault settled at this instant, so this path calls
        // apply_fee instead of writing the new policy directly (OtterSec #98).
        // apply_fee is the only installer. It accrues the closing interval at the
        // old policy, then validates the queued policy against live protocol
        // state, then installs.
        let mut maps = ctx.load_maps(
            clock.slot,
            Some(vault.spot_market_index),
            vp.is_some(),
            false,
            &ctx.accounts.velocity_state,
        )?;

        let vault_equity = {
            let user = ctx.accounts.velocity_user.load()?;
            vault.calculate_equity(&user, &mut maps)?
        };

        vault.apply_fee(
            &mut vp,
            &mut Some(ctx.accounts.fee_update.clone()),
            vault_equity,
            now,
        )?;
    } else {
        let mut fee_update = ctx.accounts.fee_update.load_mut()?;
        validate!(
            params.timelock_duration > 0,
            ErrorCode::InvalidVaultUpdate,
            "Timelock duration must be greater than 0"
        )?;

        let timelock_end_ts = now.safe_add(params.timelock_duration)?;

        let min_fee_queue_period = vault.redeem_period.safe_mul(2)?.max(ONE_WEEK);
        validate!(
            params.timelock_duration >= min_fee_queue_period,
            ErrorCode::InvalidVaultUpdate,
            "Fee updates must be queued for at least max(1 week, 2 redeem periods)"
        )?;

        let old_management_fee = vault.management_fee;
        let old_profit_share = vault.profit_share;
        let old_hurdle_rate = vault.hurdle_rate;

        let new_management_fee = params.new_management_fee.unwrap_or(old_management_fee);
        let new_profit_share = params.new_profit_share.unwrap_or(old_profit_share);
        let new_hurdle_rate = params.new_hurdle_rate.unwrap_or(old_hurdle_rate);

        // The queued values take the same bounds as vault initialization, so the
        // timelocked path cannot install a policy that no init instruction could
        // create (OtterSec #97). The combined protocol fee and protocol profit
        // share sums need the VaultProtocol account, which apply_fee loads at
        // maturity. This call checks the manager bounds, and a zero hurdle rate
        // for a protocol vault.
        validate_fee_policy(
            new_management_fee,
            new_profit_share,
            new_hurdle_rate,
            vault.vault_protocol,
            0,
            0,
        )?;

        fee_update.incoming_update_ts = timelock_end_ts;
        fee_update.incoming_management_fee = new_management_fee;
        fee_update.incoming_profit_share = new_profit_share;
        fee_update.incoming_hurdle_rate = new_hurdle_rate;

        vault.fee_update_status = FeeUpdateStatus::PendingFeeUpdate as u8;

        emit!(FeeUpdateRecord {
            ts: Clock::get()?.unix_timestamp,
            action: FeeUpdateAction::Pending,
            timelock_end_ts,
            vault: vault.pubkey,
            old_management_fee,
            old_profit_share,
            old_hurdle_rate,
            new_management_fee: fee_update.incoming_management_fee,
            new_profit_share: fee_update.incoming_profit_share,
            new_hurdle_rate: fee_update.incoming_hurdle_rate,
        });
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq)]
pub struct ManagerUpdateFeesParams {
    pub timelock_duration: i64,
    pub new_management_fee: Option<i64>,
    pub new_profit_share: Option<u32>,
    pub new_hurdle_rate: Option<u32>,
}

#[derive(Accounts)]
pub struct ManagerUpdateFees<'info> {
    #[account(
        mut,
        constraint = is_manager_for_vault(&vault, &manager)? || is_admin(&manager)?,
    )]
    pub vault: AccountLoader<'info, Vault>,
    pub manager: Signer<'info>,
    #[account(
        mut,
        seeds = [b"fee_update".as_ref(), vault.key().as_ref()],
        bump,
    )]
    pub fee_update: AccountLoader<'info, FeeUpdate>,
    /// A matured update settles the vault fee before it installs, and that needs vault equity.
    #[account(
        constraint = is_user_for_vault(&vault, &velocity_user.key())?
    )]
    /// CHECK: checked in constraint
    pub velocity_user: AccountLoader<'info, User>,
    /// CHECK: checked in velocity cpi
    pub velocity_state: AccountInfo<'info>,
    pub velocity_program: Program<'info, Velocity>,
}
