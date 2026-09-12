//! Feeding the insurance fund, and resyncing a stake.

use super::*;

#[access_control(
    withdraw_not_paused(&ctx.accounts.state)
)]
pub fn handle_settle_revenue_to_insurance_fund<'c: 'info, 'info>(
    ctx: Context<'info, SettleRevenueToInsuranceFund<'info>>,
    spot_market_index: u16,
) -> Result<()> {
    let state = ctx.accounts.state.load()?;
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mint = get_token_mint(remaining_accounts_iter)?;

    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    require_revenue_settle_due(spot_market, spot_market_index, now)?;

    let spot_vault_amount = ctx.accounts.spot_market_vault.amount;
    let insurance_vault_amount = ctx.accounts.insurance_fund_vault.amount;

    // uses proportion of revenue pool allocated to insurance fund
    let token_amount = controller::insurance::settle_revenue_to_insurance_fund(
        spot_vault_amount,
        insurance_vault_amount,
        spot_market,
        now,
        true,
        state.funding_paused()?,
    )?;

    spot_market.insurance_fund.last_revenue_settle_ts = now;

    controller::token::send_from_program_vault(
        &ctx.accounts.token_program,
        &ctx.accounts.spot_market_vault,
        &ctx.accounts.insurance_fund_vault,
        &ctx.accounts.velocity_signer,
        state.signer_nonce,
        token_amount,
        &mint,
        if spot_market.has_transfer_hook() {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;

    // reload the spot market vault balance so it's up-to-date
    ctx.accounts.spot_market_vault.reload()?;
    math::spot_withdraw::validate_spot_market_vault_amount(
        spot_market,
        ctx.accounts.spot_market_vault.amount,
    )?;

    Ok(())
}

/// Prove this market may settle revenue into the insurance fund right now.
fn require_revenue_settle_due(
    spot_market: &SpotMarket,
    spot_market_index: u16,
    now: i64,
) -> Result<()> {
    validate!(
        spot_market_index == spot_market.market_index,
        ErrorCode::InvalidSpotMarketAccount,
        "invalid spot_market passed"
    )?;

    // Moving revenue out of the spot vault into the IF vault is an egress from
    // the market: gate it on the market-scoped Withdraw pause, not just the
    // global `withdraw_not_paused` access control.
    validate!(
        !spot_market.is_operation_paused(SpotOperation::Withdraw),
        ErrorCode::MarketWithdrawPaused,
        "spot market {} withdraws paused",
        spot_market.market_index
    )?;

    validate!(
        spot_market.insurance_fund.revenue_settle_period > 0,
        ErrorCode::RevenueSettingsCannotSettleToIF,
        "invalid revenue_settle_period settings on spot market"
    )?;

    let time_until_next_update = math::helpers::on_the_hour_update(
        now,
        spot_market.insurance_fund.last_revenue_settle_ts,
        spot_market.insurance_fund.revenue_settle_period,
    )?;

    validate!(
        time_until_next_update == 0,
        ErrorCode::RevenueSettingsCannotSettleToIF,
        "Must wait {} seconds until next available settlement time",
        time_until_next_update
    )?;

    Ok(())
}

pub fn handle_update_user_quote_asset_insurance_stake(
    ctx: Context<UpdateUserQuoteAssetInsuranceStake>,
) -> Result<()> {
    let insurance_fund_stake = &mut load_mut!(ctx.accounts.insurance_fund_stake)?;
    let user_stats = &mut load_mut!(ctx.accounts.user_stats)?;
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;

    validate!(
        insurance_fund_stake.market_index == 0,
        ErrorCode::IncorrectSpotMarketAccountPassed,
        "insurance_fund_stake is not for quote market"
    )?;

    if insurance_fund_stake.market_index == 0 && spot_market.market_index == 0 {
        update_user_stats_if_stake_amount(
            0,
            ctx.accounts.insurance_fund_vault.amount,
            insurance_fund_stake,
            user_stats,
            spot_market,
        )?;
    }

    Ok(())
}

#[derive(Accounts)]
#[instruction(market_index: u16,)]
pub struct SettleRevenueToInsuranceFund<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        seeds = [b"spot_market", market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        constraint = state.load()?.signer.eq(&velocity_signer.key())
    )]
    /// CHECK: forced velocity_signer
    pub velocity_signer: UncheckedAccount<'info>,
    #[account(
        mut,
        seeds = [b"insurance_fund_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub insurance_fund_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct UpdateUserQuoteAssetInsuranceStake<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        seeds = [b"spot_market", 0_u16.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        mut,
        constraint = is_stats_for_if_stake(&insurance_fund_stake, &user_stats)?
    )]
    pub insurance_fund_stake: AccountLoader<'info, InsuranceFundStake>,
    #[account(mut)]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub signer: Signer<'info>,
    #[account(
        mut,
        seeds = [b"insurance_fund_vault".as_ref(), 0_u16.to_le_bytes().as_ref()],
        bump,
    )]
    pub insurance_fund_vault: Box<InterfaceAccount<'info, TokenAccount>>,
}
