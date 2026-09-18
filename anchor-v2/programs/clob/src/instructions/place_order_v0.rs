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

/// Floor on a slot's wall-clock length, in milliseconds.
///
/// The cluster targets 400ms per slot. This bound must stay at or below that
/// rate, or the refusal below could reject an order that could still activate.
const MIN_SLOT_MILLIS: u64 = 400;

/// Place a resting order. Returns the new order's [`OrderRefV0`] as return
/// data, so the CPI caller can store the hint.
pub fn handle_place_order_v0(
    ctx: &mut Context<PlaceOrderV0>,
    args: PlaceOrderArgsV0,
) -> Result<OrderRefV0> {
    let clock = Clock::get()?;
    let user = args.user;
    let market = &mut ctx.accounts.market;

    // Argument checks live here. Account checks live on the accounts struct.
    require!(
        args.max_ts == 0 || args.max_ts > clock.unix_timestamp,
        ClobError::MaxTsInPast
    );

    let delay = match args.activation_delay_slots {
        None => market.default_activation_delay_slots,
        Some(d) => {
            // Only the upper bound is checked here. A delay below the default
            // is legal from the place authority. Velocity gates that on the
            // flow attestation, and only velocity can place.
            require!(
                d <= market.max_activation_delay_slots,
                ClobError::InvalidActivationDelay
            );

            d
        }
    };

    // An order whose `max_ts` falls inside its own activation delay expires
    // before anything can match it. It still takes an arena slot, and it
    // still sits at the head of its side until the expiry crank reclaims it.
    if args.max_ts != 0 && delay != 0 {
        let earliest_activation = (delay as u64 * MIN_SLOT_MILLIS / 1_000) as i64;
        require!(
            args.max_ts.saturating_sub(clock.unix_timestamp) > earliest_activation,
            ClobError::MaxTsBeforeActivation
        );
    }

    let activation_slot = clock.slot + delay as u64;
    let order_ref = market.place(PlaceOrderParams {
        side: args.side,
        price: args.price,
        base_asset_amount: args.base_asset_amount,
        user,
        activation_slot,
        placed_slot: clock.slot,
        max_ts: args.max_ts,
        now: clock.unix_timestamp,
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
