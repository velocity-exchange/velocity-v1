use {
    crate::{
        errors::RouterError,
        events::RouterConfigUpdated,
        state::{RouterConfig, ROUTER_CONFIG_SEED},
    },
    anchor_lang::prelude::*,
};

/// Shared context for every admin-only setter on the singleton config.
#[derive(Accounts)]
pub struct AdminUpdate<'info> {
    #[account(
        mut,
        seeds = [ROUTER_CONFIG_SEED],
        bump = config.bump,
        has_one = admin @ RouterError::Unauthorized
    )]
    pub config: Account<'info, RouterConfig>,
    pub admin: Signer<'info>,
}

impl AdminUpdate<'_> {
    pub fn emit_updated(&self) -> Result<()> {
        let config = &self.config;
        emit!(RouterConfigUpdated {
            ts: Clock::get()?.unix_timestamp,
            admin: config.admin,
            cranker: config.cranker,
            treasury: config.treasury,
            tier_count: config.tier_count,
        });
        Ok(())
    }
}

pub fn set_admin(ctx: Context<AdminUpdate>, new_admin: Pubkey) -> Result<()> {
    require_keys_neq!(new_admin, Pubkey::default(), RouterError::InvalidAuthority);
    ctx.accounts.config.admin = new_admin;
    ctx.accounts.emit_updated()
}
