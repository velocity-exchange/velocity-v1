use {
    crate::{
        book::ClobBook,
        state::{ClobMarketV0, MarketConfigV0},
    },
    anchor_lang::prelude::*,
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
    ///
    /// It signs, which is what binds initialization to the account's creator.
    /// The client creates the account with a keypair, and the System program
    /// already requires that keypair to sign the allocation, so the signature
    /// costs an honest caller nothing. Without it, creation and initialization
    /// may land in different transactions and anyone may initialize the
    /// account first and name themselves `authority`. The operator's own call
    /// then fails and the rent is stranded.
    #[account(zeroed, signer)]
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
