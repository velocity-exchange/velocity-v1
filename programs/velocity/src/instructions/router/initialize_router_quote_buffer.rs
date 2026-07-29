//! Stamp a router's quote buffer — the account [`super::quote_router`] writes
//! its per-source books into.
//!
//! The account is **created by the caller**, not by this instruction: at
//! ~33KB it exceeds the 10,240-byte limit on account allocation from inside a
//! CPI, which is the same reason the CLOB's market account is pre-created.
//! So the caller allocates and assigns it to velocity, and this instruction
//! takes it `zero` (program-owned, zeroed, unstamped) and records who may
//! quote into it.
//!
//! Permissionless: the buffer holds no protocol state, only the caller's own
//! view of available liquidity, and it is written under simulation.

use {crate::state::router_quote::RouterQuoteBufferV0, anchor_lang::prelude::*};

#[derive(Accounts)]
pub struct InitializeRouterQuoteBuffer<'info> {
    /// Pre-created, zeroed, velocity-owned, and sized `RouterQuoteBufferV0::SIZE`.
    #[account(zero)]
    pub quote_buffer: AccountLoader<'info, RouterQuoteBufferV0>,
    /// The only signer that may later quote into this buffer.
    pub authority: Signer<'info>,
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
