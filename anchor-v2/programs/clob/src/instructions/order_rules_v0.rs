//! What the book requires of an order before it will hold one.
//!
//! A caller that builds orders has to satisfy these rules, and finding out by
//! rejection costs it the transaction — so it asks first. It used to read
//! them out of the market account's header instead, which meant knowing where
//! each one sits, and left the book unable to move a field without breaking a
//! program that never called into it.
//!
//! Read-only, and cheap enough to sit inside a landed transaction: these are
//! header scalars, not a walk.

/// Declared by `clob-wire`.
pub use clob_wire::OrderRulesV0;
use {crate::state::ClobMarketV0, anchor_lang_v2::prelude::*};

#[derive(Accounts)]
pub struct OrderRulesV0Accounts {
    pub market: ClobMarketV0,
}

pub fn handle_order_rules_v0(ctx: &mut Context<OrderRulesV0Accounts>) -> Result<OrderRulesV0> {
    let market = &ctx.accounts.market;
    Ok(OrderRulesV0 {
        min_order_size: market.min_order_size,
        blocking_min_size: market.blocking_min_size,
        default_activation_delay_slots: market.default_activation_delay_slots,
        max_activation_delay_slots: market.max_activation_delay_slots,
        place_authority: market.place_authority.to_bytes(),
        tick_size: market.order_tick_size,
        step_size: market.order_step_size,
    })
}
