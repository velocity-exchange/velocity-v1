use {
    crate::{
        dfx_redemption,
        errors::RouterError,
        events::RouterConfigUpdated,
        state::{treasury_is_valid, RouterConfig, Tier, ROUTER_CONFIG_SEED, SECONDS_PER_DAY},
    },
    anchor_lang::prelude::*,
};

/// Every new value is optional: leave an account out (or pass `None` for
/// tiers) to keep the stored value.
#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    #[account(
        mut,
        seeds = [ROUTER_CONFIG_SEED],
        bump = config.bump,
        has_one = admin @ RouterError::Unauthorized
    )]
    pub config: Account<'info, RouterConfig>,
    pub admin: Signer<'info>,
    /// CHECK: seeds-checked; only its key is compared against the new treasury
    #[account(seeds = [b"config"], bump, seeds::program = dfx_redemption::ID)]
    pub redemption_config: UncheckedAccount<'info>,
    /// CHECK: only its key is stored
    #[account(constraint = new_admin.key() != Pubkey::default() @ RouterError::InvalidAuthority)]
    pub new_admin: Option<UncheckedAccount<'info>>,
    /// CHECK: only its key is stored
    #[account(constraint = new_cranker.key() != Pubkey::default() @ RouterError::InvalidAuthority)]
    pub new_cranker: Option<UncheckedAccount<'info>>,
    /// CHECK: only its key is stored; its ATA must not alias either distribute leg
    #[account(
        constraint = treasury_is_valid(
            &new_treasury.key(),
            &config.key(),
            &redemption_config.key()
        ) @ RouterError::InvalidTreasury
    )]
    pub new_treasury: Option<UncheckedAccount<'info>>,
}

pub fn update_config(ctx: Context<UpdateConfig>, tiers: Option<Vec<Tier>>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let accounts = &mut *ctx.accounts;
    let config = &mut accounts.config;

    if let Some(admin) = &accounts.new_admin {
        config.admin = admin.key();
    }
    if let Some(cranker) = &accounts.new_cranker {
        config.cranker = cranker.key();
    }
    if let Some(treasury) = &accounts.new_treasury {
        config.treasury = treasury.key();
    }
    if let Some(tiers) = tiers {
        let now_day = now / SECONDS_PER_DAY;
        // A ladder must never change part-way through a period it has already priced.
        // `<=` mirrors roll_period: a clock reading earlier than period_day never unlocks.
        require!(
            !(now_day <= config.period_day && config.period_fees > 0),
            RouterError::TiersLockedForPeriod
        );
        config.set_tiers(&tiers)?;
    }

    emit!(RouterConfigUpdated {
        ts: now,
        admin: config.admin,
        cranker: config.cranker,
        treasury: config.treasury,
        tier_count: config.tier_count,
    });
    Ok(())
}
