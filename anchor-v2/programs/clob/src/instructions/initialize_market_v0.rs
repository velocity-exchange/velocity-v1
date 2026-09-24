use {
    crate::{
        book::ClobBook,
        state::{ClobMarketV0, MarketConfigV0},
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct InitializeMarketV0 {
    /// Registered as the market's config authority. It need not sign, because
    /// the market's own signature below already binds initialization to the
    /// account's creator. A program PDA can therefore hold the role from the
    /// start.
    pub authority: UncheckedAccount,
    /// Registered as the market's place authority. It is an account rather
    /// than an argument, because a duplicated account costs one index byte in
    /// the transaction.
    pub place_authority: UncheckedAccount,
    /// Pre-created zeroed account, sized by [`ClobMarketV0::space_for`] for the
    /// wanted capacity. That is about 98KB, which is above the CPI allocation
    /// limit, so the client creates the account.
    /// [`crate::instructions::resize_market_v0`] grows it later.
    ///
    /// It signs, which binds initialization to the account's creator. The
    /// client creates the account with a keypair, and the System program
    /// already requires that keypair to sign the allocation, so the signature
    /// costs an honest caller nothing. Without it, creation and initialization
    /// may land in different transactions. Anyone may then initialize the
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
