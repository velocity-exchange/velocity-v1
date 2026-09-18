use {
    crate::{
        book::ClobBook,
        state::{ClobMarketV0, ResponsePointerV0},
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct QuoteL3V0 {
    /// Mutable only for the response tail. The book itself is not changed.
    #[account(mut)]
    pub market: ClobMarketV0,
}

/// Declared in `quoter-spec`: velocity writes these bytes and this program
/// reads them, so the shape lives in the crate both compile against.
pub use quoter_spec::L3ArgsV0;

/// Quoter interface, optional leg. Reports the resting orders behind the
/// ladder `quote_v0` would return, one row per order, streamed into the
/// market's response tail.
///
/// A book is the only quoter type whose ladder stands on orders belonging to
/// somebody else. A caller that must carry those users' accounts, or draw
/// the book, reads them here instead of decoding this account from outside.
pub fn handle_quote_l3_v0(
    ctx: &mut Context<QuoteL3V0>,
    args: L3ArgsV0,
) -> Result<ResponsePointerV0> {
    let clock = Clock::get()?;
    ctx.accounts.market.quote_l3(
        args.direction,
        args.size,
        args.max_rows,
        args.include_taker_origin_reservations,
        clock.slot,
        clock.unix_timestamp,
    )
}
