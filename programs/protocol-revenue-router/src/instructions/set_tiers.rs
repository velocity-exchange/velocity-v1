use {
    crate::{
        errors::RouterError,
        instructions::set_admin::AdminUpdate,
        state::{Tier, SECONDS_PER_DAY},
    },
    anchor_lang::prelude::*,
};

pub fn set_tiers(ctx: Context<AdminUpdate>, tiers: Vec<Tier>) -> Result<()> {
    let config = &mut ctx.accounts.config;
    let now_day = Clock::get()?.unix_timestamp / SECONDS_PER_DAY;
    // A ladder must never change part-way through a period it has already priced.
    require!(
        !(now_day == config.period_day && config.period_fees > 0),
        RouterError::TiersLockedForPeriod
    );
    config.set_tiers(&tiers)?;
    ctx.accounts.emit_updated()
}
