/// Declared by `clob-wire`.
pub use clob_wire::FillArgsV0;
use {
    crate::{
        book::{ClobBook, NodeArena},
        emit::emit_fill_record,
        error::ClobError,
        events::FillEntryV0,
        instructions::GatedMarketV0,
        state::{FillOutcomeV0, FilledOrderV0},
    },
    anchor_lang::prelude::*,
};

/// Report fills the caller made against taker remainders resting here.
///
/// `execute_v0` is the book filling its own orders for a taker it can see. This
/// runs the opposite direction. A taker remainder resting here is itself the
/// aggressor, and the prices it aggresses against are not all on this book. A
/// quoter or the vAMM may hold the better price, and this program can see
/// neither. Velocity does that matching and reports the result, and the order
/// shrinks in place.
///
/// The order has to shrink in place. A cancel and a fresh placement would cost
/// the order its queue position and its id for a fill that never changed its
/// price. A partly-filled remainder would then drift to the back of its own
/// level every time somebody improved it.
///
/// The gate matches `execute_v0` for the same reason. Only velocity can settle
/// a fill, so only velocity may tell the book that one happened.
pub fn handle_fill_v0(ctx: &mut Context<GatedMarketV0>, args: FillArgsV0) -> Result<FillOutcomeV0> {
    let clock = Clock::get()?;
    let market = &mut ctx.accounts.market;
    let market_index = market.market_index;

    require!(
        !args.fills.is_empty() && args.fills.len() <= crate::state::FILL_BATCH_CEILING,
        ClobError::InvalidOrderParams
    );

    // The node is read before `fill` shrinks or removes it, since the record
    // needs the remainder's own price and owner and this is the only place
    // either is stored.
    let (fills, filled): (Vec<FillEntryV0>, Vec<FilledOrderV0>) = args
        .fills
        .iter()
        .map(|request| {
            let node = market.read_node(request.order_ref.node_index)?;
            let outcome = market.fill(
                request.order_ref,
                request.base_asset_amount,
                clock.slot,
                clock.unix_timestamp,
            )?;
            Ok((
                FillEntryV0 {
                    order_id: outcome.order_id,
                    owner: node.authority,
                    price: node.price,
                    base_size: outcome.base_asset_amount,
                    client_order_id: outcome.client_order_id,
                },
                outcome,
            ))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .unzip();

    let culled: Vec<u32> = filled
        .iter()
        .filter(|order| order.culled_base_asset_amount > 0)
        .map(|order| order.client_order_id)
        .collect();
    emit_fill_record(
        clock.unix_timestamp,
        clock.slot,
        market_index,
        &fills,
        &culled,
    )?;

    Ok(FillOutcomeV0 { filled })
}
