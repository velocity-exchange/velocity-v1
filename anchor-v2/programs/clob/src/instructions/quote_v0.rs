use {
    crate::{
        book::ClobBook,
        state::{ClobMarketV0, ResponsePointerV0},
    },
    anchor_lang_v2::prelude::*,
};

#[derive(Accounts)]
pub struct QuoteV0 {
    /// mut only for the response tail — the book itself is not touched.
    #[account(mut)]
    pub market: ClobMarketV0,
}

/// Declared in `quoter-spec`: velocity writes these bytes and this program
/// reads them, so the shape lives in the crate both compile against.
pub use quoter_spec::QuoteArgsV0;

/// Quoter interface: price levels for a taker of `direction`/`size`, streamed
/// into the market's response tail as they are aggregated; the returned
/// pointer locates them.
pub fn handle_quote_v0(ctx: &mut Context<QuoteV0>, args: QuoteArgsV0) -> Result<ResponsePointerV0> {
    let clock = Clock::get()?;
    ctx.accounts.market.quote(
        args.direction,
        args.size,
        args.users,
        &args.caps,
        args.reference_price,
        args.taker.as_ref(),
        clock.slot,
        clock.unix_timestamp,
    )
}
