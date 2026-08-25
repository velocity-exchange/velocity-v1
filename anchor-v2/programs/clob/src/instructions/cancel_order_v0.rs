use {
    crate::{
        book::{BookHeader, ClobBook},
        emit::emit_pod,
        error::ClobError,
        events::OrderCancelRecordV0,
        state::{ClobMarketV0, RemovedOrderV0},
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

/// Declared by `clob-wire`. The owner is verified against the node.
pub use clob_wire::CancelOrderArgsV0;

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
    // The removal path takes no clock, so an activation hint the chain
    // has already reached is dropped here instead.
    market.expire_activation_hint(clock.slot)?;
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
        max_ts: removed.max_ts,
    })
}
