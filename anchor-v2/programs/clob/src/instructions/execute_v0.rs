use {
    crate::{
        book::ClobBook,
        emit::emit_execute_record,
        error::ClobError,
        state::{ClobDirectionExt, ClobMarketV0, ResponsePointerV0},
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct ExecuteV0 {
    #[account(mut)]
    pub market: ClobMarketV0,
    #[account(address = market.place_authority @ ClobError::InvalidAuthority)]
    pub place_authority: Signer,
}

/// Declared in `quoter-spec`: velocity writes these bytes and this program
/// reads them, so the shape lives in the crate both compile against.
pub use quoter_spec::ExecuteArgsV0;

/// Quoter interface: commit a fill; balance changes (merged by user) are
/// streamed into the market's response tail as the book is consumed, located
/// by the returned pointer. Velocity clamps `size` to margin before calling
/// and validates the changes on its side.
pub fn handle_execute_v0(
    ctx: &mut Context<ExecuteV0>,
    args: ExecuteArgsV0<'_>,
) -> Result<ResponsePointerV0> {
    let clock = Clock::get()?;
    let market = &mut ctx.accounts.market;
    let market_index = market.market_index;
    let outcome = market.execute(
        args.direction,
        args.size,
        args.users,
        &args.caps,
        args.reference_price,
        args.taker.as_ref(),
        clock.slot,
        clock.unix_timestamp,
    )?;

    emit_execute_record(
        clock.unix_timestamp,
        clock.slot,
        market_index,
        args.direction.to_u8(),
        &outcome.fills,
        outcome.cancelled_client_order_id.as_slice(),
    )?;

    Ok(outcome.response)
}
