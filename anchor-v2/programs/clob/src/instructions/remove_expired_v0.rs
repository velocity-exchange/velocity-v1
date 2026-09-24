/// Declared by `clob-wire`.
pub use clob_wire::RemoveExpiredArgsV0;
use {
    crate::{
        book::{BookHeader, ClobBook},
        emit::emit_removal,
        events::OrderExpireRecordV0,
        instructions::GatedMarketV0,
        state::RemovedOrderV0,
    },
    anchor_lang::prelude::*,
};

/// Crank reclamation of an expired order. `quote_v0` and `execute_v0` only skip
/// an expired order, because a removal without the maker's `User` loaded would
/// leak the open-order aggregates. Velocity is the caller and applies the
/// returned removal.
pub fn handle_remove_expired_v0(
    ctx: &mut Context<GatedMarketV0>,
    args: RemoveExpiredArgsV0,
) -> Result<RemovedOrderV0> {
    let clock = Clock::get()?;
    let market = &mut ctx.accounts.market;
    let removed = market.remove_expired(args.order_ref, clock.unix_timestamp)?;
    market.expire_activation_hint(clock.slot)?;

    emit_removal!(OrderExpireRecordV0, removed, clock, market.market_index);

    Ok(removed)
}
