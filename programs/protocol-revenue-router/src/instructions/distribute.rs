use {
    crate::{
        dfx_redemption,
        errors::RouterError,
        events::FeesDistributed,
        math::pool_share,
        state::{RouterConfig, ROUTER_CONFIG_SEED, SECONDS_PER_DAY},
    },
    anchor_lang::prelude::*,
    anchor_spl::{
        associated_token::AssociatedToken,
        token::{self, Mint, Token, TokenAccount, Transfer},
    },
};

#[derive(Accounts)]
pub struct Distribute<'info> {
    #[account(
        mut,
        seeds = [ROUTER_CONFIG_SEED],
        bump = config.bump,
        constraint = cranker.key() == config.cranker || cranker.key() == config.admin
            @ RouterError::Unauthorized
    )]
    pub config: Box<Account<'info, RouterConfig>>,
    pub cranker: Signer<'info>,
    #[account(address = config.usdt_mint)]
    pub usdt_mint: Box<Account<'info, Mint>>,
    #[account(
        mut,
        associated_token::mint = usdt_mint,
        associated_token::authority = config
    )]
    pub router_ata: Box<Account<'info, TokenAccount>>,
    /// CHECK: only used as the ATA wallet; locked to the admin-set treasury
    #[account(address = config.treasury)]
    pub treasury: UncheckedAccount<'info>,
    // Aliasing either leg's account would misroute funds: the SPL self-transfer
    // is a no-op, and the redemption vault would swallow the treasury share.
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = usdt_mint,
        associated_token::authority = treasury,
        constraint = treasury_ata.key() != router_ata.key()
            && treasury_ata.key() != redemption_vault.key()
            @ RouterError::InvalidTreasury
    )]
    pub treasury_ata: Box<Account<'info, TokenAccount>>,
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(
        mut,
        seeds = [b"config"],
        bump,
        seeds::program = dfx_redemption::ID
    )]
    pub redemption_config: Box<Account<'info, dfx_redemption::accounts::Config>>,
    #[account(
        mut,
        seeds = [b"contribution_ledger"],
        bump,
        seeds::program = dfx_redemption::ID
    )]
    pub redemption_ledger: Box<Account<'info, dfx_redemption::accounts::ContributionLedger>>,
    #[account(mut, address = redemption_config.usdt_vault)]
    pub redemption_vault: Box<Account<'info, TokenAccount>>,
    pub dfx_redemption_program: Program<'info, dfx_redemption::program::DfxRedemption>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

pub fn distribute(ctx: Context<Distribute>) -> Result<()> {
    let clock = Clock::get()?;
    ctx.accounts
        .config
        .roll_period(clock.unix_timestamp / SECONDS_PER_DAY);

    let total = ctx.accounts.router_ata.amount;
    if total == 0 {
        return Ok(());
    }

    let config = &ctx.accounts.config;
    let mut to_pool = pool_share(config.active_tiers(), config.period_fees, total as u128);

    // Count USDT already sitting unrecognised in the vault: `contribute` recognises
    // it first, so it eats cap room this contribution would otherwise use.
    let rc = &ctx.accounts.redemption_config;
    let unrecognized = ctx
        .accounts
        .redemption_vault
        .amount
        .saturating_sub(rc.recognized_backing_remaining);
    let cap_room = rc
        .total_exploited_amount
        .saturating_sub(rc.lifetime_recognized_backing)
        .saturating_sub(unrecognized);
    to_pool = to_pool.min(cap_room as u128);

    let to_pool = to_pool as u64;
    let to_treasury = total - to_pool;

    let bump = ctx.accounts.config.bump;
    let seeds: &[&[u8]] = &[ROUTER_CONFIG_SEED, &[bump]];
    let signer_seeds = &[seeds];

    // Treasury leg first, so the external program only ever gets signer authority
    // over an ATA holding exactly the pool share.
    if to_treasury > 0 {
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.router_ata.to_account_info(),
                    to: ctx.accounts.treasury_ata.to_account_info(),
                    authority: ctx.accounts.config.to_account_info(),
                },
                signer_seeds,
            ),
            to_treasury,
        )?;
    }

    if to_pool > 0 {
        dfx_redemption::cpi::contribute(
            CpiContext::new_with_signer(
                ctx.accounts.dfx_redemption_program.key(),
                dfx_redemption::cpi::accounts::Contribute {
                    config: ctx.accounts.redemption_config.to_account_info(),
                    contribution_ledger: ctx.accounts.redemption_ledger.to_account_info(),
                    usdt_mint: ctx.accounts.usdt_mint.to_account_info(),
                    usdt_vault: ctx.accounts.redemption_vault.to_account_info(),
                    depositor: ctx.accounts.config.to_account_info(),
                    depositor_usdt: ctx.accounts.router_ata.to_account_info(),
                    token_program: ctx.accounts.token_program.to_account_info(),
                },
                signer_seeds,
            ),
            to_pool,
            dfx_redemption::types::ContributionSource::ProtocolFees,
        )?;
    }

    let config = &mut ctx.accounts.config;
    config.period_fees = add(config.period_fees, total)?;
    config.lifetime_fees = add(config.lifetime_fees, total)?;
    config.lifetime_to_pool = add(config.lifetime_to_pool, to_pool)?;
    config.lifetime_to_treasury = add(config.lifetime_to_treasury, to_treasury)?;

    emit!(FeesDistributed {
        ts: clock.unix_timestamp,
        total,
        to_pool,
        to_treasury,
        cap_room_after: cap_room - to_pool,
        period_day: config.period_day,
        period_fees_after: config.period_fees,
        lifetime_fees_after: config.lifetime_fees,
    });
    Ok(())
}

fn add(acc: u128, delta: u64) -> Result<u128> {
    acc.checked_add(delta as u128)
        .ok_or_else(|| error!(RouterError::ArithmeticOverflow))
}
