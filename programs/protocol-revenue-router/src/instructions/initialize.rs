use {
    crate::{
        dfx_redemption,
        errors::RouterError,
        events::RouterInitialized,
        state::{RouterConfig, Tier, MAX_TIERS, ROUTER_CONFIG_SEED},
    },
    anchor_lang::prelude::*,
    anchor_spl::token::Mint,
};

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = payer,
        seeds = [ROUTER_CONFIG_SEED],
        bump,
        space = 8 + RouterConfig::INIT_SPACE
    )]
    pub config: Account<'info, RouterConfig>,
    // There is no mint setter, so a wrong mint here would strand every future fee.
    #[account(address = redemption_config.usdt_mint @ RouterError::UsdtMintMismatch)]
    pub usdt_mint: Account<'info, Mint>,
    #[account(seeds = [b"config"], bump, seeds::program = dfx_redemption::ID)]
    pub redemption_config: Box<Account<'info, dfx_redemption::accounts::Config>>,
    /// CHECK: only its key is stored
    #[account(constraint = admin.key() != Pubkey::default() @ RouterError::InvalidAuthority)]
    pub admin: UncheckedAccount<'info>,
    /// CHECK: only its key is stored
    #[account(constraint = cranker.key() != Pubkey::default() @ RouterError::InvalidAuthority)]
    pub cranker: UncheckedAccount<'info>,
    /// CHECK: only its key is stored; its ATA must not alias either distribute leg
    #[account(
        constraint = treasury.key() != Pubkey::default() @ RouterError::InvalidAuthority,
        constraint = treasury.key() != config.key()
            && treasury.key() != redemption_config.key()
            @ RouterError::InvalidTreasury
    )]
    pub treasury: UncheckedAccount<'info>,
    // The singleton init is locked to a fixed key only on a real mainnet build,
    // so devnet/localnet and the test build can still initialize freely.
    #[cfg_attr(
        any(not(feature = "mainnet-beta"), feature = "anchor-test"),
        account(mut)
    )]
    #[cfg_attr(
        all(feature = "mainnet-beta", not(feature = "anchor-test")),
        account(mut, address = crate::ids::init_authority::id())
    )]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
}

pub fn initialize(ctx: Context<Initialize>, tiers: Vec<Tier>) -> Result<()> {
    let config = &mut ctx.accounts.config;
    config.set_inner(RouterConfig {
        bump: ctx.bumps.config,
        admin: ctx.accounts.admin.key(),
        cranker: ctx.accounts.cranker.key(),
        usdt_mint: ctx.accounts.usdt_mint.key(),
        treasury: ctx.accounts.treasury.key(),
        tiers: [Tier::default(); MAX_TIERS],
        tier_count: 0,
        period_day: 0,
        period_fees: 0,
        lifetime_fees: 0,
        lifetime_to_pool: 0,
        lifetime_to_treasury: 0,
        _reserved: [0u8; 128],
    });
    config.set_tiers(&tiers)?;

    emit!(RouterInitialized {
        ts: Clock::get()?.unix_timestamp,
        admin: config.admin,
        cranker: config.cranker,
        treasury: config.treasury,
        usdt_mint: config.usdt_mint,
        tier_count: config.tier_count,
    });
    Ok(())
}
