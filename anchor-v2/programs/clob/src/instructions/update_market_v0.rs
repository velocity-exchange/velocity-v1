use {
    crate::{
        error::ClobError,
        state::{ClobMarketV0, EXECUTE_FILLS_CEILING, EXECUTE_USERS_CEILING, QUOTE_LEVELS_CEILING},
    },
    anchor_lang_v2::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateMarketV0 {
    #[account(mut)]
    pub market: ClobMarketV0,
    #[account(address = market.authority @ ClobError::InvalidAuthority)]
    pub authority: Signer,
    /// Present = becomes the new place authority.
    pub new_place_authority: Option<UncheckedAccount>,
}

#[derive(Clone, Default, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct UpdateMarketArgsV0 {
    pub order_tick_size: Option<u64>,
    pub order_step_size: Option<u64>,
    pub min_order_size: Option<u64>,
    pub blocking_min_size: Option<u64>,
    pub default_activation_delay_slots: Option<u32>,
    pub max_activation_delay_slots: Option<u32>,
    pub unknown_user_grace_slots: Option<u32>,
    pub evict_threshold_per_side: Option<u32>,
    pub max_quote_levels: Option<u16>,
    pub max_execute_fills: Option<u16>,
    pub max_execute_users: Option<u16>,
}

pub fn handle_update_market_v0(
    ctx: &mut Context<UpdateMarketV0>,
    args: UpdateMarketArgsV0,
) -> Result<()> {
    let new_place = ctx
        .accounts
        .new_place_authority
        .as_ref()
        .map(|a| *a.address());
    let market = &mut ctx.accounts.market;
    if let Some(v) = args.order_tick_size {
        market.order_tick_size = v;
    }
    if let Some(v) = args.order_step_size {
        market.order_step_size = v;
    }
    if let Some(v) = args.min_order_size {
        market.min_order_size = v;
    }
    if let Some(v) = args.blocking_min_size {
        market.blocking_min_size = v;
    }
    if let Some(v) = args.default_activation_delay_slots {
        market.default_activation_delay_slots = v;
    }
    if let Some(v) = args.max_activation_delay_slots {
        market.max_activation_delay_slots = v;
    }
    if let Some(v) = args.unknown_user_grace_slots {
        market.unknown_user_grace_slots = v;
    }
    if let Some(v) = args.evict_threshold_per_side {
        market.evict_threshold_per_side = v;
    }
    if let Some(v) = args.max_quote_levels {
        require!(
            v != 0 && v <= QUOTE_LEVELS_CEILING,
            crate::error::ClobError::InvalidConfig
        );
        market.max_quote_levels = v;
    }
    if let Some(v) = args.max_execute_fills {
        require!(
            v != 0 && v <= EXECUTE_FILLS_CEILING,
            crate::error::ClobError::InvalidConfig
        );
        market.max_execute_fills = v;
    }
    if let Some(v) = args.max_execute_users {
        require!(
            v != 0 && v <= EXECUTE_USERS_CEILING,
            crate::error::ClobError::InvalidConfig
        );
        market.max_execute_users = v;
    }
    require!(
        market.default_activation_delay_slots <= market.max_activation_delay_slots,
        crate::error::ClobError::InvalidConfig
    );
    if let Some(v) = new_place {
        market.place_authority = v;
    }
    Ok(())
}
