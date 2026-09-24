/// Declared by `clob-wire`, because velocity sends it by CPI when velocity is
/// the market's authority.
pub use clob_wire::ClobUpdateMarketArgsV0 as UpdateMarketArgsV0;
use {
    crate::{
        config::validate_market_config,
        error::ClobError,
        events::{MarketSettingsV0, MarketUpdateRecordV0},
        state::ClobMarketV0,
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

/// Apply every field the caller set, then check the whole config.
///
/// `place_authority` is not offered. A book settles for whoever it names as a
/// maker, and velocity pins this field to its own signing PDA. Any rotation,
/// even a short one, would let this market's authority place orders for any
/// user.
pub fn handle_update_market_v0(
    ctx: &mut Context<UpdateMarketV0>,
    args: UpdateMarketArgsV0,
) -> Result<()> {
    let market_address = *ctx.accounts.market.address();
    let authority = *ctx.accounts.authority.address();
    let market = &mut ctx.accounts.market;
    let before = MarketSettingsV0::of(market);
    let UpdateMarketArgsV0 {
        order_tick_size,
        order_step_size,
        min_order_size,
        blocking_min_size,
        default_activation_delay_slots,
        max_activation_delay_slots,
        unknown_user_grace_slots,
        evict_threshold_per_side,
        max_quote_levels,
        max_execute_fills,
        max_execute_users,
        reservation_grace_slots,
    } = args;

    let header = &mut **market;
    set(&mut header.order_tick_size, order_tick_size);
    set(&mut header.order_step_size, order_step_size);
    set(&mut header.min_order_size, min_order_size);
    set(&mut header.blocking_min_size, blocking_min_size);
    set(
        &mut header.default_activation_delay_slots,
        default_activation_delay_slots,
    );
    set(
        &mut header.max_activation_delay_slots,
        max_activation_delay_slots,
    );
    set(
        &mut header.unknown_user_grace_slots,
        unknown_user_grace_slots,
    );
    set(
        &mut header.evict_threshold_per_side,
        evict_threshold_per_side,
    );
    set(&mut header.max_quote_levels, max_quote_levels);
    set(&mut header.max_execute_fills, max_execute_fills);
    set(&mut header.max_execute_users, max_execute_users);
    set(&mut header.reservation_grace_slots, reservation_grace_slots);

    validate_market_config(market)?;

    emit!(MarketUpdateRecordV0 {
        market: market_address,
        authority,
        ts: Clock::get()?.unix_timestamp,
        before,
        after: MarketSettingsV0::of(market),
    });

    Ok(())
}

fn set<T>(field: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *field = value;
    }
}
