use {
    crate::{
        errors::RouterError,
        events::RouterInitialized,
        instructions::set_treasury::validate_treasury,
        state::{RouterConfig, Tier, ROUTER_CONFIG_SEED},
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
    pub usdt_mint: Account<'info, Mint>,
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

pub fn initialize(
    ctx: Context<Initialize>,
    admin: Pubkey,
    cranker: Pubkey,
    treasury: Pubkey,
    tiers: Vec<Tier>,
) -> Result<()> {
    for key in [admin, cranker] {
        require_keys_neq!(key, Pubkey::default(), RouterError::InvalidAuthority);
    }
    validate_treasury(&treasury, &ctx.accounts.config.key())?;

    let config = &mut ctx.accounts.config;
    config.bump = ctx.bumps.config;
    config.admin = admin;
    config.cranker = cranker;
    config.treasury = treasury;
    config.usdt_mint = ctx.accounts.usdt_mint.key();
    config.period_day = 0;
    config.period_fees = 0;
    config.lifetime_fees = 0;
    config.lifetime_to_pool = 0;
    config.lifetime_to_treasury = 0;
    config.set_tiers(&tiers)?;

    emit!(RouterInitialized {
        ts: Clock::get()?.unix_timestamp,
        admin,
        cranker,
        treasury,
        usdt_mint: config.usdt_mint,
        tier_count: config.tier_count,
    });
    Ok(())
}
