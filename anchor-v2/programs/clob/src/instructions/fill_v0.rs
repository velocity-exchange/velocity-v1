use {
    crate::{
        book::ClobBook,
        emit::emit_execute_record,
        error::ClobError,
        events::FillSlimV0,
        state::{ClobMarketV0, FillOutcomeV0, FilledOrder},
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct FillV0 {
    #[account(mut)]
    pub market: ClobMarketV0,
    #[account(address = market.place_authority @ ClobError::InvalidAuthority)]
    pub place_authority: Signer,
}

/// Declared by `clob-wire`.
pub use clob_wire::FillArgsV0;

/// Report fills the caller made against taker remainders resting here.
///
/// `execute_v0` is the book filling its own orders for a taker it can see. This
/// is the mirror: a taker remainder resting here is itself the aggressor, and
/// the prices it aggresses against are not all on this book — a quoter or the
/// vAMM may be the better one, and this program can see neither. Velocity does
/// that matching and reports the result, and the order shrinks in place.
///
/// In place is the point. A cancel and a fresh placement would cost the order
/// its queue position and its id for a fill that never changed its price, so a
/// partly-filled remainder would drift to the back of its own level every time
/// somebody improved it.
///
/// Same gate as `execute_v0`, and for the same reason: only velocity can settle
/// a fill, so only velocity may tell the book one happened.
pub fn handle_fill_v0(ctx: &mut Context<FillV0>, args: FillArgsV0) -> Result<FillOutcomeV0> {
    let clock = Clock::get()?;
    let market = &mut ctx.accounts.market;
    let market_index = market.market_index;

    require!(
        !args.fills.is_empty() && args.fills.len() <= crate::state::FILL_BATCH_CEILING,
        ClobError::InvalidOrderParams
    );
    let filled = args
        .fills
        .iter()
        .map(|request| market.fill(request.order_ref, request.base_asset_amount))
        .collect::<Result<Vec<FilledOrder>>>()?;

    // The same record `execute_v0` writes, because this is the same event: an
    // order on this book filled. A reader that already follows fills needs no
    // second shape to follow these.
    let fills: Vec<FillSlimV0> = filled
        .iter()
        .map(|order| FillSlimV0 {
            order_id: order.order_id,
            base_size: order.base_asset_amount,
            client_order_id: order.client_order_id,
        })
        .collect();
    let culled: Vec<u32> = filled
        .iter()
        .filter(|order| order.culled_base_asset_amount > 0)
        .map(|order| order.client_order_id)
        .collect();
    emit_execute_record(
        clock.unix_timestamp,
        clock.slot,
        market_index,
        // A batch has no single direction — each order carries its own side,
        // and a reader joins to it by order id, as it already does for the
        // fills an execute reports.
        0,
        &fills,
        &culled,
    )?;

    Ok(FillOutcomeV0 { filled })
}
