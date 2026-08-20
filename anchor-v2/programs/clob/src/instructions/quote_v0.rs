use {
    crate::{
        book::ClobBook,
        state::{ClobMarketV0, Direction, ResponsePointerV0, UserCapsV0, UserRefV0, UserSetV0},
    },
    anchor_lang_v2::prelude::*,
};

#[derive(Accounts)]
pub struct QuoteV0 {
    /// mut only for the response tail — the book itself is not touched.
    #[account(mut)]
    pub market: ClobMarketV0,
}

#[derive(Clone, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct QuoteArgsV0 {
    pub direction: Direction,
    pub size: u64,
    /// `User`s the caller can settle balance changes for (loaded in its tx).
    /// Empty = unrestricted (off-chain discovery). Orders for absent users
    /// are skipped within the market's grace window, fail the call past it.
    pub users: UserSetV0,
    /// How much of `users` each named one may still take, per side. Anyone
    /// absent is unconstrained; a zero cap passes their orders over. Quote
    /// and execute must be given the same caps — the ladder is a promise
    /// about what the fill will deliver.
    pub caps: UserCapsV0,
    /// The taker's `User`: their own resting orders are skipped
    /// unconditionally (self-trade prevention).
    pub taker: Option<UserRefV0>,
}

/// Quoter interface: price levels for a taker of `direction`/`size`, streamed
/// into the market's response tail as they are aggregated; the returned
/// pointer locates them.
pub fn handle_quote_v0(ctx: &mut Context<QuoteV0>, args: QuoteArgsV0) -> Result<ResponsePointerV0> {
    let clock = Clock::get()?;
    ctx.accounts.market.quote(
        args.direction,
        args.size,
        args.users.as_slice(),
        &args.caps,
        args.taker.as_ref(),
        clock.slot,
        clock.unix_timestamp,
    )
}
