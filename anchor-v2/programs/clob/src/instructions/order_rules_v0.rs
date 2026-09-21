//! What the book requires of an order before it will hold one.
//!
//! A caller that builds orders has to satisfy these rules, and a rejection
//! costs it the transaction, so it asks first. It used to read the rules out of
//! the market account's header, which meant knowing where each one sits. The
//! book could then not move a field without breaking a program that never
//! called into it.
//!
//! Read-only, and cheap enough to sit inside a landed transaction. These are
//! header scalars, and reading them is not a walk.
//!
//! The response also carries the live side counts and the arena capacity. A
//! side at its cap refuses a placement, and a caller resting a taker's
//! remainder loses the whole fill to that refusal. The counts are the only
//! way it can see the refusal coming.

/// Declared by `clob-wire`.
pub use clob_wire::OrderRulesV0;
use {crate::instructions::MarketViewV0, anchor_lang::prelude::*};

pub fn handle_order_rules_v0(ctx: &mut Context<MarketViewV0>) -> Result<OrderRulesV0> {
    let market = &ctx.accounts.market;
    Ok(OrderRulesV0 {
        min_order_size: market.min_order_size,
        blocking_min_size: market.blocking_min_size,
        default_activation_delay_slots: market.default_activation_delay_slots,
        max_activation_delay_slots: market.max_activation_delay_slots,
        place_authority: market.place_authority.to_bytes(),
        tick_size: market.order_tick_size,
        step_size: market.order_step_size,
        side_order_counts: [market.bid_count, market.ask_count],
        arena_capacity: market.capacity() as u32,
        evict_threshold_per_side: market.evict_threshold_per_side,
    })
}
