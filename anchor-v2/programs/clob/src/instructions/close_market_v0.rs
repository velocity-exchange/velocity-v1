use {
    crate::{emit::emit_pod, error::ClobError, events::MarketCloseRecordV0, state::ClobMarketV0},
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct CloseMarketV0 {
    #[account(mut, close = rent_recipient)]
    pub market: ClobMarketV0,
    #[account(address = market.authority @ ClobError::InvalidAuthority)]
    pub authority: Signer,
    /// Receives the market's rent.
    #[account(mut)]
    pub rent_recipient: UncheckedAccount,
}

/// Close an empty market and return its rent.
///
/// Only an empty book closes. A resting order also backs an open-order
/// reservation in velocity's margin aggregates, which a close leaves dangling.
pub fn handle_close_market_v0(ctx: &mut Context<CloseMarketV0>) -> Result<()> {
    let market = &ctx.accounts.market;
    require!(
        market.bid_count == 0 && market.ask_count == 0,
        ClobError::MarketNotEmpty
    );

    emit_pod!(MarketCloseRecordV0 {
        market: *market.address(),
        authority: *ctx.accounts.authority.address(),
        rent_recipient: *ctx.accounts.rent_recipient.address(),
        ts: Clock::get()?.unix_timestamp,
    });

    Ok(())
}
