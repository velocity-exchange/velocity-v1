use {
    crate::{book::ClobBook, error::ClobError, state::ClobMarketV0},
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct ResizeMarketV0 {
    #[account(mut)]
    pub payer: Signer,
    #[account(mut)]
    pub market: ClobMarketV0,
    #[account(address = market.authority @ ClobError::InvalidAuthority)]
    pub authority: Signer,
    pub system_program: Program<System>,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build", derive(anchor_lang::IdlType))]
pub struct ResizeMarketArgsV0 {
    pub new_capacity: u32,
}

/// Grow the order arena. Realloc is capped at 10KB per instruction, which is
/// about 116 nodes, so a large target takes repeated calls. Each call tops up
/// rent from `payer` and threads the new slots into the free list. The arena
/// does not shrink, because live orders and free-list links may sit above any
/// lower bound.
pub fn handle_resize_market_v0(
    ctx: &mut Context<ResizeMarketV0>,
    args: ResizeMarketArgsV0,
) -> Result<()> {
    let market = &mut ctx.accounts.market;
    require!(
        args.new_capacity as usize > market.capacity(),
        ClobError::InvalidCapacity
    );

    market.resize_to_capacity(args.new_capacity)?;
    market.top_up(ctx.accounts.payer.as_ref())?;
    market.grow_free_list()?;
    // The eviction threshold is bounded by the per-side capacity, and this is
    // the other instruction that moves that bound.
    crate::book::validate_evict_threshold(market.evict_threshold_per_side, market.capacity() as u32)
}
