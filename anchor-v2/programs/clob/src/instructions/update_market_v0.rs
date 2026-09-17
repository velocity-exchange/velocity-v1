use {
    crate::{
        error::ClobError,
        state::{
            ClobMarketV0, EXECUTE_FILLS_CEILING, EXECUTE_USERS_CEILING, QUOTE_LEVELS_CEILING,
            RESERVATION_GRACE_SLOTS_CEILING,
        },
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateMarketV0 {
    #[account(mut)]
    pub market: ClobMarketV0,
    #[account(address = market.authority @ ClobError::InvalidAuthority)]
    pub authority: Signer,
}

#[derive(Clone, Default, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build", derive(anchor_lang::IdlType))]
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
    pub reservation_grace_slots: Option<u16>,
}

pub fn handle_update_market_v0(
    ctx: &mut Context<UpdateMarketV0>,
    args: UpdateMarketArgsV0,
) -> Result<()> {
    // place_authority is immutable after initialize_market_v0. A book settles
    // for whoever it names as a maker, and velocity pins this field to its own
    // signing PDA. Any rotation, even a short one, would let this market's
    // authority place orders for any user. The book offers no rotation.
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
        crate::book::validate_evict_threshold(v, market.capacity() as u32)?;
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
    if let Some(v) = args.reservation_grace_slots {
        // A claim holds top-of-book depth out of the matchable set, and only
        // the cross crank can consume it. The ceiling keeps a crank that
        // never lands from holding that depth for longer than one transaction
        // stays valid. Zero is a valid setting. A claim then ends the slot its
        // remainder activates.
        require!(
            v <= RESERVATION_GRACE_SLOTS_CEILING,
            crate::error::ClobError::InvalidConfig
        );
        market.reservation_grace_slots = v;
    }
    require!(
        market.default_activation_delay_slots <= market.max_activation_delay_slots,
        crate::error::ClobError::InvalidConfig
    );
    Ok(())
}
