use {
    crate::{
        book::ClobBook,
        emit::emit_pod,
        error::ClobError,
        events::OrderPlaceRecordV0,
        state::{ClobMarketV0, ClobSideExt, OrderRefV0, PlaceOrderParams},
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct PlaceOrderV0 {
    #[account(mut)]
    pub market: ClobMarketV0,
    #[account(address = market.place_authority @ ClobError::InvalidAuthority)]
    pub place_authority: Signer,
}

/// Declared by `clob-wire`, which owns every shape on this program's
/// instruction surface. `taker_origin` marks the order
/// [`crate::state::OrderBitFlag::TakerOrigin`].
pub use clob_wire::PlaceOrderArgsV0;

/// Place a resting order. Returns the new order's [`OrderRefV0`] (as return
/// data) so the CPI caller can persist the hint.
pub fn handle_place_order_v0(
    ctx: &mut Context<PlaceOrderV0>,
    args: PlaceOrderArgsV0,
) -> Result<OrderRefV0> {
    let clock = Clock::get()?;
    let user = args.user;
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
        taker_origin: args.taker_origin,
        client_order_id: args.client_order_id,
        reject_if_crossed: args.reject_if_crossed,
        reduce_only: args.reduce_only,
    })?;

    emit_pod!(OrderPlaceRecordV0 {
        authority: user.authority,
        ts: clock.unix_timestamp,
        slot: clock.slot,
        order_id: order_ref.order_id,
        activation_slot,
        max_ts: args.max_ts,
        price: args.price,
        base_asset_amount: args.base_asset_amount,
        node_index: order_ref.node_index,
        client_order_id: args.client_order_id,
        market_index: market.market_index,
        sub_account_id: user.sub_account_id,
        side: args.side.to_u8(),
        _pad: [0; 3],
    });

    Ok(order_ref)
}
