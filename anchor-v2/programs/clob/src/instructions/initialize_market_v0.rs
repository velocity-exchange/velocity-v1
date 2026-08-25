use {
    crate::{
        book::ClobBook,
        state::{ClobMarketV0, MarketConfigV0},
    },
    anchor_lang_v2::prelude::*,
};

#[derive(Accounts)]
pub struct InitializeMarketV0 {
    pub authority: Signer,
    /// Registered as the market's place authority (accounts, not args:
    /// duplicated accounts cost one index byte in the tx).
    pub place_authority: UncheckedAccount,
    /// Pre-created zeroed account of [`ClobMarketV0::space_for`] the wanted
    /// capacity (~98KB — larger than CPI alloc limits, so the client creates
    /// it). [`crate::instructions::resize_market_v0`] grows it later.
    #[account(zeroed)]
    pub market: ClobMarketV0,
}

pub fn handle_initialize_market_v0(
    ctx: &mut Context<InitializeMarketV0>,
    config: MarketConfigV0,
) -> Result<()> {
    let authority = *ctx.accounts.authority.address();
    let place_authority = *ctx.accounts.place_authority.address();
    ctx.accounts
        .market
        .initialize(authority, place_authority, config)
}
