//! Second half of a config authority rotation. The key that
//! `propose_market_authority_v0` named signs, and becomes the authority.

use {
    crate::{
        emit::emit_pod,
        error::ClobError,
        events::MarketAuthorityAcceptedRecordV0,
        state::{ClobMarketV0, ZERO_ADDRESS},
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct AcceptMarketAuthorityV0 {
    #[account(mut)]
    pub market: ClobMarketV0,
    #[account(address = market.pending_authority @ ClobError::InvalidAuthority)]
    pub pending_authority: Signer,
}

pub fn handle_accept_market_authority_v0(ctx: &mut Context<AcceptMarketAuthorityV0>) -> Result<()> {
    let market_address = *ctx.accounts.market.address();
    let market = &mut ctx.accounts.market;
    require!(
        market.pending_authority != ZERO_ADDRESS,
        ClobError::InvalidAuthority
    );

    let previous_authority = market.authority;
    market.authority = market.pending_authority;
    market.pending_authority = ZERO_ADDRESS;

    emit_pod!(MarketAuthorityAcceptedRecordV0 {
        market: market_address,
        previous_authority,
        authority: market.authority,
        ts: Clock::get()?.unix_timestamp,
    });

    Ok(())
}
