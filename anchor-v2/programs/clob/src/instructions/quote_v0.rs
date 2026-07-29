use anchor_lang_v2::prelude::*;

use crate::error::ClobError;
use crate::state::{ClobBook, ClobMarketV0, Direction, QuoteResponseV0, ResponsePointerV0};

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
    /// `None` = unrestricted (off-chain discovery). Orders for absent users
    /// are skipped within the market's grace window, fail the call past it.
    pub users: Option<Vec<Address>>,
    /// The taker's `User`: their own resting orders are skipped
    /// unconditionally (self-trade prevention).
    pub taker: Option<Address>,
}

/// Quoter interface: price levels for a taker of `direction`/`size`, written
/// to the market's response tail; the returned pointer locates them.
pub fn handle_quote_v0(ctx: &mut Context<QuoteV0>, args: QuoteArgsV0) -> Result<ResponsePointerV0> {
    let clock = Clock::get()?;
    let market = &mut ctx.accounts.market;
    let levels = market.quote(
        args.direction,
        args.size,
        args.users.as_deref(),
        args.taker.as_ref(),
        clock.slot,
        clock.unix_timestamp,
    )?;

    let mut data = Vec::with_capacity(1024);
    anchor_lang_v2::wincode::config::serialize_into(
        &mut data,
        &QuoteResponseV0 { levels },
        anchor_lang_v2::BORSH_CONFIG,
    )
    .map_err(|_| ClobError::ResponseTooLarge)?;
    market.write_response(&data)
}
