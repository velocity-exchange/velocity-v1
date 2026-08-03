use {
    crate::{
        book::ClobBook,
        emit::emit_pod,
        error::ClobError,
        events::OrderEvictRecordV0,
        state::{ClobMarketV0, RemovedOrderV0, Side},
    },
    anchor_lang_v2::prelude::*,
};

#[derive(Accounts)]
pub struct EvictWorstV0 {
    #[account(mut)]
    pub market: ClobMarketV0,
    #[account(address = market.place_authority @ ClobError::InvalidAuthority)]
    pub place_authority: Signer,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct EvictWorstArgsV0 {
    pub side: Side,
}

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

    emit_pod!(OrderEvictRecordV0 {
        authority: removed.user.authority,
        ts: clock.unix_timestamp,
        order_id: removed.order_id,
        price: removed.price,
        base_asset_amount: removed.base_asset_amount,
        market_index: market.market_index,
        sub_account_id: removed.user.sub_account_id,
        _pad: [0; 4],
    });

    Ok(RemovedOrderV0 {
        user: removed.user,
        order_id: removed.order_id,
        price: removed.price,
        base_asset_amount: removed.base_asset_amount,
        side: removed.side,
    })
}
