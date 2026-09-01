use {
    crate::{
        book::ClobBook,
        state::{ClobMarketV0, ResponsePointerV0},
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct QuoteL3V0 {
    /// mut only for the response tail — the book itself is not touched.
    #[account(mut)]
    pub market: ClobMarketV0,
}

/// Declared in `quoter-spec`: velocity writes these bytes and this program
/// reads them, so the shape lives in the crate both compile against.
pub use quoter_spec::L3ArgsV0;

/// Quoter interface, optional leg: the resting orders behind the ladder
/// `quote_v0` would return, one row per order, streamed into the market's
/// response tail.
///
/// A quoter implements this when its ladder stands on orders that belong to
/// somebody other than itself, which of the quoter types is only a book. A
/// caller that must carry those users' accounts — or draw the book — reads
/// them here instead of decoding this account from outside.
pub fn handle_quote_l3_v0(
    ctx: &mut Context<QuoteL3V0>,
    args: L3ArgsV0,
) -> Result<ResponsePointerV0> {
    let clock = Clock::get()?;
    ctx.accounts.market.quote_l3(
        args.direction,
        args.size,
        args.max_rows,
        clock.slot,
        clock.unix_timestamp,
    )
}
