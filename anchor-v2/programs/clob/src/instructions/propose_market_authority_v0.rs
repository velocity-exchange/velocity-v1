//! First half of a config authority rotation. The current authority names its
//! successor, and nothing changes until the successor signs
//! `accept_market_authority_v0`. A mistyped key therefore cannot take the
//! market out of every signer's reach.

use {
    crate::{
        emit::emit_pod, error::ClobError, events::MarketAuthorityProposedRecordV0,
        state::ClobMarketV0,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct ProposeMarketAuthorityV0 {
    #[account(mut)]
    pub market: ClobMarketV0,
    #[account(address = market.authority @ ClobError::InvalidAuthority)]
    pub authority: Signer,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build", derive(anchor_lang::IdlType))]
pub struct ProposeMarketAuthorityArgsV0 {
    /// The zero address withdraws an open proposal.
    pub proposed_authority: Address,
}

pub fn handle_propose_market_authority_v0(
    ctx: &mut Context<ProposeMarketAuthorityV0>,
    args: ProposeMarketAuthorityArgsV0,
) -> Result<()> {
    let market_address = *ctx.accounts.market.address();
    let market = &mut ctx.accounts.market;
    market.pending_authority = args.proposed_authority;

    emit_pod!(MarketAuthorityProposedRecordV0 {
        market: market_address,
        authority: market.authority,
        proposed_authority: args.proposed_authority,
        ts: Clock::get()?.unix_timestamp,
    });

    Ok(())
}
