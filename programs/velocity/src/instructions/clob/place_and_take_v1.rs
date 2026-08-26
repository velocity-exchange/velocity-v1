//! `place_and_take_perp_order_v1` — the CLOB-aware taker route.
//!
//! Same semantics as `place_and_take_perp_order`, except the market's CLOB
//! accounts are **required**, and an unfilled restable limit remainder rests
//! on the book instead of on the DLOB ("if it can rest and be matched, it
//! lives on the CLOB", applied to the taker flow's leftover).
//!
//! Why a new instruction rather than optional accounts on v0: appending
//! optional accounts to a shipped `#[derive(Accounts)]` changes the account
//! list every existing client builds, so v0 keeps its exact `master` shape
//! forever and callers opt into the book by naming this endpoint. The two
//! share one body ([`crate::instructions::place_and_take_perp_order`]) — the
//! only difference is whether the CLOB accounts are passed.
//!
//! Not yet routed *through* the book: the fill leg here is still the vAMM
//! plus the DLOB makers the caller passed. Filling a place-and-take across
//! external quoter books needs the fill entrypoint's quote/execute account
//! section, which today only `fill_perp_order`'s keeper entrypoint builds.

use {
    crate::{
        instructions::{constraints::*, place_and_take_perp_order, ClobRemainderRoute},
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
pub struct PlaceAndTakeV1<'info> {
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
    pub authority: Signer<'info>,
    /// The market's CLOB registry entry — the remainder only ever rests on a
    /// vetted book.
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// CHECK: validated against the quoter entry's registered execute
    /// accounts (`ClobMarket::from_quoter`), so a valid entry can't be
    /// pointed at an arbitrary account.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: locked to the registered quoter program.
    #[account(address = quoter.load()?.program_id)]
    pub clob_program: UncheckedAccount<'info>,
    /// CHECK: the CLOB place authority PDA — what a book's `place_authority`
    /// is set to. Its own key, distinct from the per-entry signer a
    /// third-party quoter is handed: signer privilege is inherited by a
    /// callee, and this one may place and cancel on any book, for any user.
    #[account(seeds = [CLOB_AUTHORITY_SEED], bump)]
    pub clob_authority: UncheckedAccount<'info>,
    /// Wake-hint host for the rested remainder. Optional like every other
    /// CLOB placement path: a market whose conditions were never initialized
    /// must still be tradeable, and a missed hint costs crank latency, not
    /// liveness (the fallback poll is the floor).
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
pub fn handle_place_and_take_perp_order_v1<'c: 'info, 'info>(
    ctx: Context<'info, PlaceAndTakeV1<'info>>,
    params: OrderParams,
    optional_params: Option<u32>, // u32 for backwards compatibility with v0
) -> Result<()> {
    place_and_take_perp_order(
        &ctx.accounts.state,
        &ctx.accounts.user,
        &ctx.accounts.user_stats,
        ctx.remaining_accounts,
        params,
        optional_params,
        Some(ClobRemainderRoute {
            quoter: &ctx.accounts.quoter,
            clob_market: &ctx.accounts.clob_market,
            clob_program: &ctx.accounts.clob_program,
            clob_authority: &ctx.accounts.clob_authority,
            clob_authority_nonce: ctx.bumps.clob_authority,
            crank_conditions: ctx.accounts.crank_conditions.as_ref(),
        }),
    )
}
