use {
    crate::{
        book::{BookHeader, ClobBook},
        emit::emit_pod,
        error::ClobError,
        events::OrderEvictRecordV0,
        state::{ClobMarketV0, RemovedOrderV0},
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct EvictWorstV0 {
    #[account(mut)]
    pub market: ClobMarketV0,
    #[account(address = market.place_authority @ ClobError::InvalidAuthority)]
    pub place_authority: Signer,
}

/// Declared by `clob-wire`.
pub use clob_wire::EvictWorstArgsV0;

/// Crank eviction of the side's tail, allowed once the side is at or above
/// `evict_threshold_per_side`. Velocity is the caller: it loads the evicted
/// maker's `User`, applies the returned removal to the open-order
/// aggregates, and re-arms a trigger slot holding this `OrderRef`.
pub fn handle_evict_worst_v0(
    ctx: &mut Context<EvictWorstV0>,
    args: EvictWorstArgsV0,
) -> Result<RemovedOrderV0> {
    let clock = Clock::get()?;
    let market = &mut ctx.accounts.market;
    let removed = market.evict_worst(args.side)?;
    // The removal path takes no clock, so an activation hint the chain
    // has already reached is dropped here instead.
    market.expire_activation_hint(clock.slot)?;

    emit_pod!(OrderEvictRecordV0 {
        authority: removed.user.authority,
        ts: clock.unix_timestamp,
        order_id: removed.order_id,
        price: removed.price,
        base_asset_amount: removed.base_asset_amount,
        market_index: market.market_index,
        sub_account_id: removed.user.sub_account_id,
        client_order_id: removed.client_order_id,
    });

    Ok(RemovedOrderV0 {
        user: removed.user,
        order_id: removed.order_id,
        client_order_id: removed.client_order_id,
        price: removed.price,
        base_asset_amount: removed.base_asset_amount,
        side: removed.side,
        taker_origin: removed.taker_origin,
        reduce_only: removed.reduce_only,
        max_ts: removed.max_ts,
    })
}
