/// Declared by `clob-wire`.
pub use clob_wire::EvictWorstArgsV0;
use {
    crate::{
        book::{BookHeader, ClobBook},
        emit::emit_removal,
        events::OrderEvictRecordV0,
        instructions::GatedMarketV0,
        state::RemovedOrderV0,
    },
    anchor_lang::prelude::*,
};

/// Crank eviction of the side's worst order that is not a bound taker
/// remainder. It is allowed once the side is at or above
/// `evict_threshold_per_side`. Velocity is the caller. It loads the
/// evicted maker's `User`, applies the returned removal to the open-order
/// aggregates, and re-arms a trigger slot that holds this `OrderRef`.
pub fn handle_evict_worst_v0(
    ctx: &mut Context<GatedMarketV0>,
    args: EvictWorstArgsV0,
) -> Result<RemovedOrderV0> {
    let clock = Clock::get()?;
    let market = &mut ctx.accounts.market;
    let removed = market.evict_worst(args.side, clock.slot)?;
    market.expire_activation_hint(clock.slot)?;

    emit_removal!(OrderEvictRecordV0, removed, clock, market.market_index);

    Ok(removed)
}
