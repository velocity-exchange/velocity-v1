/// Declared by `clob-wire`. The owner is verified against the node.
pub use clob_wire::CancelOrderArgsV0;
use {
    crate::{
        book::{BookHeader, ClobBook},
        emit::emit_removal,
        events::OrderCancelRecordV0,
        instructions::GatedMarketV0,
        state::RemovedOrderV0,
    },
    anchor_lang::prelude::*,
};

/// Cancel a resting order. Returns the removed order as return data, so
/// velocity can decrement the maker's open-order aggregates by the remaining
/// size on the correct side.
pub fn handle_cancel_order_v0(
    ctx: &mut Context<GatedMarketV0>,
    args: CancelOrderArgsV0,
) -> Result<RemovedOrderV0> {
    let clock = Clock::get()?;
    let market = &mut ctx.accounts.market;
    let removed = market.cancel(args.user, args.order_ref, clock.slot, args.force)?;
    market.expire_activation_hint(clock.slot)?;

    emit_removal!(OrderCancelRecordV0, removed, clock, market.market_index);

    Ok(removed.into())
}
