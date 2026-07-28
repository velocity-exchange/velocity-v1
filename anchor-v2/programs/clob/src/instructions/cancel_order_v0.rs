use anchor_lang_v2::prelude::*;

use crate::error::ClobError;
use crate::events::OrderCancelRecord;
use crate::state::{ClobBook, ClobMarketV0, OrderRefV0};

#[derive(Accounts)]
pub struct CancelOrderV0 {
    #[account(mut)]
    pub market: ClobMarketV0,
    #[account(address = market.place_authority @ ClobError::InvalidAuthority)]
    pub place_authority: Signer,
    /// Owner of the order being cancelled (verified against the node).
    pub user: UncheckedAccount,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CancelOrderArgsV0 {
    pub order_ref: OrderRefV0,
}

pub fn handle_cancel_order_v0(
    ctx: &mut Context<CancelOrderV0>,
    args: CancelOrderArgsV0,
) -> Result<()> {
    let clock = Clock::get()?;
    let user = *ctx.accounts.user.address();
    let market = &mut ctx.accounts.market;
    let removed = market.cancel(user, args.order_ref)?;
    emit!(OrderCancelRecord {
        user: removed.user,
        ts: clock.unix_timestamp,
        order_id: removed.order_id,
        price: removed.price,
        base_asset_amount: removed.base_asset_amount,
        market_index: market.market_index,
        _pad: [0; 6],
    });
    Ok(())
}
