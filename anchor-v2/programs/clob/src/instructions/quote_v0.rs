/// Declared in `quoter-spec`: velocity writes these bytes and this program
/// reads them, so the shape lives in the crate both compile against.
pub use quoter_spec::QuoteArgsV0;
use {
    crate::{book::ClobBook, instructions::ResponseMarketV0, state::ResponsePointerV0},
    anchor_lang::prelude::*,
};

/// Quoter interface. Reports price levels for a taker of `direction` and
/// `size`. The book streams the levels into the market's response tail as it
/// aggregates them. The returned pointer locates them.
pub fn handle_quote_v0(
    ctx: &mut Context<ResponseMarketV0>,
    args: QuoteArgsV0,
) -> Result<ResponsePointerV0> {
    let clock = Clock::get()?;
    ctx.accounts.market.quote(
        args.direction,
        args.size,
        args.users,
        &args.caps,
        args.reference_price,
        args.taker.as_ref(),
        args.limit_price,
        args.include_taker_origin_reservations,
        clock.slot,
        clock.unix_timestamp,
    )
}
