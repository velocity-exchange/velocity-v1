use anchor_lang_v2::prelude::*;

use crate::error::ClobError;
use crate::events::OrderPlaceRecord;
use crate::state::{ClobBook, ClobMarketV0, OrderRefV0, PlaceOrderParams, Side};

#[derive(Accounts)]
pub struct PlaceOrderV0 {
    #[account(mut)]
    pub market: ClobMarketV0,
    #[account(address = market.place_authority @ ClobError::InvalidAuthority)]
    pub place_authority: Signer,
    /// Velocity `User` account the order settles against (authority verified
    /// by velocity before the CPI). An account, not an arg: it's already in
    /// the enclosing velocity transaction, so it costs one index byte.
    pub user: UncheckedAccount,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct PlaceOrderArgsV0 {
    pub side: Side,
    pub price: u64,
    pub base_asset_amount: u64,
    /// None = market default. Some(d) must be <= max (auction flow). Zero is
    /// allowed — the caller (velocity) owns attestation policy.
    pub activation_delay_slots: Option<u32>,
    pub max_ts: i64,
}

/// Place a resting order. Returns the new order's [`OrderRefV0`] (as return
/// data) so the CPI caller can persist the hint.
pub fn handle_place_order_v0(
    ctx: &mut Context<PlaceOrderV0>,
    args: PlaceOrderArgsV0,
) -> Result<OrderRefV0> {
    let clock = Clock::get()?;
    let user = *ctx.accounts.user.address();
    let market = &mut ctx.accounts.market;

    // Arg validation up front; account validation lives on the struct.
    require!(
        args.max_ts == 0 || args.max_ts > clock.unix_timestamp,
        ClobError::MaxTsInPast
    );
    let delay = match args.activation_delay_slots {
        None => market.default_activation_delay_slots,
        Some(d) => {
            require!(
                d <= market.max_activation_delay_slots,
                ClobError::InvalidActivationDelay
            );
            d
        }
    };

    let activation_slot = clock.slot + delay as u64;
    let order_ref = market.place(PlaceOrderParams {
        side: args.side,
        price: args.price,
        base_asset_amount: args.base_asset_amount,
        user,
        activation_slot,
        placed_slot: clock.slot,
        max_ts: args.max_ts,
    })?;

    emit!(OrderPlaceRecord {
        user,
        ts: clock.unix_timestamp,
        slot: clock.slot,
        order_id: order_ref.order_id,
        activation_slot,
        max_ts: args.max_ts,
        price: args.price,
        base_asset_amount: args.base_asset_amount,
        node_index: order_ref.node_index,
        market_index: market.market_index,
        side: args.side.to_u8(),
        _pad: [0; 1],
    });

    Ok(order_ref)
}
