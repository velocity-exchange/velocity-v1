use anchor_lang_v2::prelude::*;

use crate::error::ClobError;
use crate::events::OrderExpireRecord;
use crate::state::{ClobBook, ClobMarketV0, OrderRefV0, RemovedOrderV0};

#[derive(Accounts)]
pub struct RemoveExpiredV0 {
    #[account(mut)]
    pub market: ClobMarketV0,
    #[account(address = market.place_authority @ ClobError::InvalidAuthority)]
    pub place_authority: Signer,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct RemoveExpiredArgsV0 {
    pub order_ref: OrderRefV0,
}

/// Crank reclamation of an expired order (quote/execute only skip expired —
/// removal without the maker's `User` loaded would leak the open-order
/// aggregates). Velocity is the caller and applies the returned removal.
pub fn handle_remove_expired_v0(
    ctx: &mut Context<RemoveExpiredV0>,
    args: RemoveExpiredArgsV0,
) -> Result<RemovedOrderV0> {
    let clock = Clock::get()?;
    let market = &mut ctx.accounts.market;
    let removed = market.remove_expired(args.order_ref, clock.unix_timestamp)?;

    emit!(OrderExpireRecord {
        user: removed.user,
        ts: clock.unix_timestamp,
        order_id: removed.order_id,
        price: removed.price,
        base_asset_amount: removed.base_asset_amount,
        market_index: market.market_index,
        _pad: [0; 6],
    });

    Ok(RemovedOrderV0 {
        user: removed.user,
        order_id: removed.order_id,
        price: removed.price,
        base_asset_amount: removed.base_asset_amount,
        side: removed.side,
    })
}
