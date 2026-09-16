use {
    crate::{error::ClobError, state::ClobMarketV0},
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
/// A market account holds the whole order arena, so its rent is large. The
/// account is created by a client rather than by this program, and a market
/// that was created with the wrong capacity or the wrong config has no other
/// way back. Without this instruction the rent is lost.
///
/// Only an empty book closes. Every resting order is also an open order in
/// velocity's margin aggregates, and velocity unwinds one by loading the
/// maker and applying the removal this program reports. A close would take
/// those orders away with no removal reported, so each maker would carry an
/// open-order reservation against an order that no longer exists.
pub fn handle_close_market_v0(ctx: &mut Context<CloseMarketV0>) -> Result<()> {
    let market = &ctx.accounts.market;
    require!(
        market.bid_count == 0 && market.ask_count == 0,
        ClobError::MarketNotEmpty
    );
    Ok(())
}
