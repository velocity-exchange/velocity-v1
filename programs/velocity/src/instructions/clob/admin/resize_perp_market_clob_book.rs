//! Grow an attached book's order arena through velocity.
//!
//! The market's quoter slab is the book's config authority, so the CLOB's
//! `resize_market_v0` runs only by this CPI. The CLOB caps one realloc at
//! 10KB, so a large target takes repeated calls.

use {
    crate::{
        auth::check_warm,
        instructions::constraints::perp_market_valid,
        msg,
        state::{
            perp_market::PerpMarket,
            prop_amm::{ClobMarket, ClobResizeMarketArgsV0, QuoterSlabV0},
            state::State,
        },
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct AdminResizePerpMarketClobBook<'info> {
    /// Pays the rent the larger arena needs.
    #[account(mut, constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(has_one = quoter_slab, has_one = clob_market)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// Signs the CPI as the book's config authority.
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: the perp market's `has_one` binds it to the book the market
    /// designated.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: the address constraint pins it to velocity's CLOB.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_resize_perp_market_clob_book(
    ctx: Context<AdminResizePerpMarketClobBook>,
    new_capacity: u32,
) -> Result<()> {
    let market_index = ctx.accounts.perp_market.load()?.market_index;
    msg!(
        "perp market {} book to {} orders",
        market_index,
        new_capacity
    );

    ClobMarket::from_slab(
        &ctx.accounts.quoter_slab,
        market_index,
        &ctx.accounts.clob_market,
        &ctx.accounts.clob_program,
    )?
    .resize(
        ctx.accounts.admin.as_ref(),
        ctx.accounts.system_program.as_ref(),
        ClobResizeMarketArgsV0 { new_capacity },
    )
}
