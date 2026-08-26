//! `place_and_make_perp_order_v1` — the CLOB-aware maker route.
//!
//! Same semantics as `place_and_make_perp_order`: post an IOC post-only limit
//! order and immediately match a named taker order against it. The difference
//! is where the unmatched remainder goes. v0 cancels it; v1 rests it on the
//! market's CLOB.
//!
//! That is not a contradiction of the IOC requirement — IOC here means the
//! order must not occupy a `User.orders` slot, and on this route it does not:
//! the remainder leaves `User.orders` and lives on the book, which is where a
//! restable maker order belongs. A maker who quoted a price to fill a taker
//! generally still wants that price working afterwards, and throwing the
//! remainder away is the thing v0 could not avoid.
//!
//! Why a new instruction rather than optional accounts on v0: appending
//! optional accounts to a shipped `#[derive(Accounts)]` changes the account
//! list every existing client builds, so v0 keeps its exact shape forever and
//! callers opt into the book by naming this endpoint. Both share one body
//! ([`crate::instructions::place_and_make_perp_order`]).
//!
//! Unlike the taker route, this one does **not** quote external books: a maker
//! is providing liquidity, not consuming it, and the fill here is the named
//! taker order against this one order. Nothing to route.

use {
    crate::{
        instructions::{constraints::*, place_and_make_perp_order, ClobRemainderRoute},
        signer::CLOB_AUTHORITY_SEED,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            order_params::OrderParams,
            prop_amm::QuoterV0,
            state::State,
            user::{User, UserStats},
        },
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
#[instruction(params: OrderParams)]
pub struct PlaceAndMakeV1<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    #[account(mut)]
    pub taker: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&taker, &taker_stats)?
    )]
    pub taker_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    /// The market's CLOB registry entry — the remainder only ever rests on a
    /// vetted book.
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// CHECK: validated against the quoter entry's registered accounts
    /// (`ClobMarket::from_quoter`), so a valid entry can't be pointed at an
    /// arbitrary account.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: locked to the registered quoter program.
    #[account(address = quoter.load()?.program_id)]
    pub clob_program: UncheckedAccount<'info>,
    /// CHECK: the CLOB place authority PDA — what a book's `place_authority`
    /// is set to, and nothing a third-party quoter is ever handed.
    #[account(seeds = [CLOB_AUTHORITY_SEED], bump)]
    pub quoter_signer: UncheckedAccount<'info>,
    /// Wake-hint host for the rested remainder. Optional like every other CLOB
    /// placement path: a market whose conditions were never initialized must
    /// still be tradeable, and a missed hint costs crank latency, not liveness.
    #[account(
        mut,
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            params.market_index.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub crank_conditions: Option<AccountLoader<'info, ClobCrankConditionsV0>>,
}

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_place_and_make_perp_order_v1<'c: 'info, 'info>(
    ctx: Context<'info, PlaceAndMakeV1<'info>>,
    params: OrderParams,
    taker_order_id: u32,
) -> Result<()> {
    place_and_make_perp_order(
        &ctx.accounts.state,
        &ctx.accounts.user,
        &ctx.accounts.user_stats,
        &ctx.accounts.taker,
        &ctx.accounts.taker_stats,
        ctx.remaining_accounts,
        params,
        taker_order_id,
        Some(ClobRemainderRoute {
            quoter: &ctx.accounts.quoter,
            clob_market: &ctx.accounts.clob_market,
            clob_program: &ctx.accounts.clob_program,
            quoter_signer: &ctx.accounts.quoter_signer,
            quoter_signer_nonce: ctx.bumps.quoter_signer,
            crank_conditions: ctx.accounts.crank_conditions.as_ref(),
        }),
    )
}
