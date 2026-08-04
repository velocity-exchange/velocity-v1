use {
    crate::{
        book::ClobBook,
        emit::emit_pod,
        error::ClobError,
        events::OrderCancelRecordV0,
        state::{ClobMarketV0, OrderRefV0, RemovedOrderV0, UserRefV0},
    },
    anchor_lang_v2::prelude::*,
};

#[derive(Accounts)]
pub struct CancelOrderV0 {
    #[account(mut)]
    pub market: ClobMarketV0,
    #[account(address = market.place_authority @ ClobError::InvalidAuthority)]
    pub place_authority: Signer,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CancelOrderArgsV0 {
    pub order_ref: OrderRefV0,
    /// Owner of the order being cancelled (verified against the node;
    /// velocity verified control before the CPI).
    pub user: UserRefV0,
}

/// Cancel a resting order. Returns the removed order (as return data) so the
/// CPI caller (velocity) can decrement the maker's open-order aggregates by
/// the remaining size on the right side.
pub fn handle_cancel_order_v0(
    ctx: &mut Context<CancelOrderV0>,
    args: CancelOrderArgsV0,
) -> Result<RemovedOrderV0> {
    let clock = Clock::get()?;
    let market = &mut ctx.accounts.market;
    let removed = market.cancel(args.user, args.order_ref)?;
    emit_pod!(OrderCancelRecordV0 {
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
        taker_origin: removed.taker_origin,
    })
}
