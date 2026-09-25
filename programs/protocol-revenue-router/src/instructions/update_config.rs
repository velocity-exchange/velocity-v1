use {
    crate::{
        dfx_redemption,
        errors::RouterError,
        events::RouterConfigUpdated,
        state::{RouterConfig, Tier, ROUTER_CONFIG_SEED, SECONDS_PER_DAY},
    },
    anchor_lang::prelude::*,
};

/// Every field is optional: `None` keeps the stored value, `Some` replaces it.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct UpdateConfigArgs {
    pub admin: Option<Pubkey>,
    pub cranker: Option<Pubkey>,
    pub treasury: Option<Pubkey>,
    pub tiers: Option<Vec<Tier>>,
}

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
}

/// The treasury's ATA must not alias the router ATA or the redemption vault.
pub fn validate_treasury(treasury: &Pubkey, config: &Pubkey) -> Result<()> {
    require_keys_neq!(*treasury, Pubkey::default(), RouterError::InvalidAuthority);
    require_keys_neq!(*treasury, *config, RouterError::InvalidTreasury);
    let (redemption_config, _) = Pubkey::find_program_address(&[b"config"], &dfx_redemption::ID);
    require_keys_neq!(*treasury, redemption_config, RouterError::InvalidTreasury);
    Ok(())
}

pub fn update_config(ctx: Context<UpdateConfig>, args: UpdateConfigArgs) -> Result<()> {
    let config_key = ctx.accounts.config.key();
    let config = &mut ctx.accounts.config;

    if let Some(admin) = args.admin {
        require_keys_neq!(admin, Pubkey::default(), RouterError::InvalidAuthority);
        config.admin = admin;
    }
    if let Some(cranker) = args.cranker {
        require_keys_neq!(cranker, Pubkey::default(), RouterError::InvalidAuthority);
        config.cranker = cranker;
    }
    if let Some(treasury) = args.treasury {
        validate_treasury(&treasury, &config_key)?;
        config.treasury = treasury;
    }
    if let Some(tiers) = args.tiers {
        let now_day = Clock::get()?.unix_timestamp / SECONDS_PER_DAY;
        // A ladder must never change part-way through a period it has already priced.
        // `<=` mirrors roll_period: a clock reading earlier than period_day never unlocks.
        require!(
            !(now_day <= config.period_day && config.period_fees > 0),
            RouterError::TiersLockedForPeriod
        );
        config.set_tiers(&tiers)?;
    }

    emit!(RouterConfigUpdated {
        ts: Clock::get()?.unix_timestamp,
        admin: config.admin,
        cranker: config.cranker,
        treasury: config.treasury,
        tier_count: config.tier_count,
    });
    Ok(())
}
