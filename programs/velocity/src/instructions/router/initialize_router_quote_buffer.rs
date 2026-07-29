//! Create a router's quote buffer — the account
//! [`super::quote_router`] writes its per-source books into.
//!
//! One per (authority, market), so a router quoting every market never
//! contends with itself, and two routers on the same market can't overwrite
//! each other's reads. Permissionless: the buffer holds no protocol state,
//! only the caller's own view of what liquidity is available, and it is
//! written under simulation.

use anchor_lang::prelude::*;

use crate::state::router_quote::{RouterQuoteBufferV0, ROUTER_QUOTE_PDA_SEED};
use crate::state::traits::Size;

#[derive(Accounts)]
#[instruction(market_index: u16)]
pub struct InitializeRouterQuoteBuffer<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub authority: Signer<'info>,
    #[account(
        init,
        seeds = [ROUTER_QUOTE_PDA_SEED, authority.key().as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
        payer = payer,
        space = RouterQuoteBufferV0::SIZE,
    )]
    pub quote_buffer: AccountLoader<'info, RouterQuoteBufferV0>,
    pub system_program: Program<'info, System>,
}

pub fn handle_initialize_router_quote_buffer(
    ctx: Context<InitializeRouterQuoteBuffer>,
    market_index: u16,
) -> Result<()> {
    let mut buffer = ctx.accounts.quote_buffer.load_init()?;
    buffer.authority = ctx.accounts.authority.key();
    buffer.market = market_index;
    Ok(())
}
