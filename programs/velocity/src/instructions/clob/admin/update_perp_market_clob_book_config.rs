//! Change an attached book's config through velocity.
//!
//! The market's quoter slab is the book's config authority, so the CLOB's
//! `update_market_v0` runs only by this CPI. The same instruction then
//! re-reads the book's rules and rewrites the mirror the hot paths read. The
//! mirror therefore cannot fall behind the book.

use {
    super::update_perp_market_clob_quoter::{bind_book_slot, mirror_book_placement_rules},
    crate::{
        auth::check_warm,
        instructions::constraints::perp_market_valid,
        msg,
        state::{
            perp_market::PerpMarket,
            prop_amm::{ClobUpdateMarketArgsV0, QuoterSlabV0, QuoterV0},
            state::State,
        },
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct AdminUpdatePerpMarketClobBookConfig<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(has_one = quoter_slab, has_one = clob_market)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// Writable, because the staging entry carries the mirror forward to a
    /// later re-approval.
    #[account(mut)]
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// Writable, because the book's slot holds the mirror the hot paths read.
    /// The slab also signs the CPI as the book's config authority.
    #[account(mut)]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: the perp market's `has_one` binds it to the book the market
    /// designated.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: the handler checks it against the book's slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_clob_book_config(
    ctx: Context<AdminUpdatePerpMarketClobBookConfig>,
    args: ClobUpdateMarketArgsV0,
) -> Result<()> {
    let perp_market = ctx.accounts.perp_market.load()?;
    msg!("perp market {}", perp_market.market_index);

    let (clob, _) = bind_book_slot(
        &ctx.accounts.quoter_slab,
        &ctx.accounts.quoter,
        &ctx.accounts.clob_market,
        &ctx.accounts.clob_program,
        perp_market.market_index,
    )?;

    clob.update_market(args)?;
    mirror_book_placement_rules(
        &clob,
        &perp_market,
        &ctx.accounts.quoter_slab,
        &ctx.accounts.quoter,
    )
}
