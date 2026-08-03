use {
    crate::{
        constraints::{is_if_stake_for_vault, is_manager_for_vault, is_user_stats_for_vault},
        declare_vault_seeds,
        velocity_cpi::CancelRequestRemoveInsuranceFundStakeCPI,
        Vault,
    },
    anchor_lang::prelude::*,
    anchor_spl::token_interface::{TokenAccount, TokenInterface},
    velocity::{
        cpi::accounts::CancelRequestRemoveInsuranceFundStake as VelocityCancelRequestRemoveInsuranceFundStake,
        program::Velocity,
        state::{insurance_fund_stake::InsuranceFundStake, spot_market::SpotMarket},
    },
};

pub fn cancel_request_remove_insurance_fund_stake<'info>(
    ctx: Context<'info, CancelRequestRemoveInsuranceFundStake<'info>>,
    market_index: u16,
) -> Result<()> {
    ctx.velocity_cancel_request_remove_insurance_fund_stake(market_index)?;
    Ok(())
}

// Own accounts struct (mirrors velocity's split of cancel off request-remove). Cancel now DOES
// settle already-due revenue before pricing the forfeiture (OtterSec #141), so it carries the same
// `velocity_state` / `velocity_spot_market_vault` / `velocity_signer` / `token_program` set as the
// request-remove path.
#[derive(Accounts)]
#[instruction(market_index: u16)]
pub struct CancelRequestRemoveInsuranceFundStake<'info> {
    #[account(
        mut,
        constraint = is_manager_for_vault(&vault, &manager)?,
    )]
    pub vault: AccountLoader<'info, Vault>,
    pub manager: Signer<'info>,
    #[account(
        mut,
        seeds = [b"spot_market", market_index.to_le_bytes().as_ref()],
        bump,
        seeds::program = velocity_program.key(),
    )]
    pub velocity_spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        mut,
        seeds = [b"insurance_fund_stake", vault.key().as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
        seeds::program = velocity_program.key(),
        constraint = is_if_stake_for_vault(&insurance_fund_stake, &vault)?,
    )]
    pub insurance_fund_stake: AccountLoader<'info, InsuranceFundStake>,
    #[account(
        mut,
        seeds = [b"insurance_fund_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
        seeds::program = velocity_program.key(),
    )]
    pub insurance_fund_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = is_user_stats_for_vault(&vault, &velocity_user_stats.key())?
    )]
    /// CHECK: checked in velocity cpi
    pub velocity_user_stats: AccountInfo<'info>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
        seeds::program = velocity_program.key(),
    )]
    pub velocity_spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: checked in velocity cpi
    pub velocity_state: AccountInfo<'info>,
    /// CHECK: forced velocity_signer
    pub velocity_signer: AccountInfo<'info>,
    pub velocity_program: Program<'info, Velocity>,
    pub token_program: Interface<'info, TokenInterface>,
}

impl<'info> CancelRequestRemoveInsuranceFundStakeCPI
    for Context<'info, CancelRequestRemoveInsuranceFundStake<'info>>
{
    fn velocity_cancel_request_remove_insurance_fund_stake(&self, market_index: u16) -> Result<()> {
        declare_vault_seeds!(self.accounts.vault, seeds);

        let cpi_accounts = VelocityCancelRequestRemoveInsuranceFundStake {
            state: self.accounts.velocity_state.clone(),
            spot_market: self.accounts.velocity_spot_market.to_account_info().clone(),
            insurance_fund_stake: self.accounts.insurance_fund_stake.to_account_info().clone(),
            user_stats: self.accounts.velocity_user_stats.clone(),
            authority: self.accounts.vault.to_account_info().clone(),
            spot_market_vault: self
                .accounts
                .velocity_spot_market_vault
                .to_account_info()
                .clone(),
            insurance_fund_vault: self.accounts.insurance_fund_vault.to_account_info().clone(),
            velocity_signer: self.accounts.velocity_signer.clone(),
            token_program: self.accounts.token_program.to_account_info().clone(),
        };

        let velocity_program = self.accounts.velocity_program.key();
        let cpi_context = CpiContext::new_with_signer(velocity_program, cpi_accounts, seeds)
            .with_remaining_accounts(self.remaining_accounts.into());
        velocity::cpi::cancel_request_remove_insurance_fund_stake(cpi_context, market_index)?;

        Ok(())
    }
}
