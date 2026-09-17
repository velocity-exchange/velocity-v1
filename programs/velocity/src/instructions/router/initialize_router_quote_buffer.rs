//! Stamp a router's quote buffer. [`super::quote_router`] writes its
//! per-source books into that account.
//!
//! The caller creates the account, not this instruction. At about 42 KB the
//! account exceeds the 10,240-byte limit on account allocation from inside a
//! CPI, which is the same reason the CLOB's market account is pre-created. The
//! caller allocates the account and assigns it to velocity. This instruction
//! takes it `zero`, which means program-owned, zeroed and unstamped, and
//! records who may quote into it.
//!
//! The instruction is permissionless. The buffer holds no protocol state. It
//! holds the caller's own view of available liquidity, and it is written under
//! simulation.

use {crate::state::router_quote::RouterQuoteBufferV0, anchor_lang::prelude::*};

#[derive(Accounts)]
pub struct InitializeRouterQuoteBuffer<'info> {
    /// Pre-created, zeroed, velocity-owned, and sized `RouterQuoteBufferV0::SIZE`.
    #[account(zero)]
    pub quote_buffer: AccountLoader<'info, RouterQuoteBufferV0>,
    /// The only signer that may later quote into this buffer.
    pub authority: Signer<'info>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct InitializeRouterQuoteBufferArgs {
    pub market_index: u16,
}

pub fn handle_initialize_router_quote_buffer(
    ctx: Context<InitializeRouterQuoteBuffer>,
    args: InitializeRouterQuoteBufferArgs,
) -> Result<()> {
    let mut buffer = ctx.accounts.quote_buffer.load_init()?;
    buffer.authority = ctx.accounts.authority.key();
    buffer.market = args.market_index;
    Ok(())
}
