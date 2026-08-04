//! `fill_perp_order_v1` — the CLOB-aware keeper fill.
//!
//! Same fill as `fill_perp_order`, plus the market's CLOB accounts, and a
//! restable remainder migrates to the book instead of resting in
//! `User.orders`.
//!
//! This closes the last hole in "if it can rest and be matched, it lives on
//! the CLOB". A signed-message taker order cannot be IOC — the program
//! rejects that — so whatever it does not fill rests. `place_and_take_v1` and
//! `place_and_make_v1` migrate their own remainders because they hold CLOB
//! accounts; a keeper-driven fill held none, so every partially-filled swift
//! order stayed on the DLOB. This is the highest-volume case of the
//! exception, not an edge one.
//!
//! Cheap in accounts, which is what makes it viable on the most
//! account-pressured instruction in the program: a router fill already
//! carries the CLOB entry, its book, the clob program and the quoter signer,
//! because the market's canonical CLOB is a mandatory baseline. Only
//! `crank_conditions` is new, and it is optional here as everywhere else.
//!
//! Market-order remainders are deliberately *not* migrated. Their only price
//! is `auction_end_price` — a slippage bound, not a price the taker wants to
//! trade at — and resting one on the book is safe only once a taker-origin
//! cross pays the taker the improvement rather than whoever lands a
//! transaction at the activation slot. See `docs/taker-remainder-auction.md`.

use {
    crate::{
        error::ErrorCode,
        instructions::{constraints::*, keeper::FillAccounts, ClobRemainderRoute},
        load,
        signer::QUOTER_SIGNER_SEED,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            prop_amm::QuoterV0,
            state::State,
            user::{User, UserStats},
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
#[instruction(order_id: Option<u32>, _maker_order_id: Option<u32>, signed_route: Vec<Pubkey>, market_index: u16)]
pub struct FillOrderV1<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = can_sign_for_user(&filler, &authority)?
    )]
    pub filler: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&filler, &filler_stats)?
    )]
    pub filler_stats: AccountLoader<'info, UserStats>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    /// The market's CLOB registry entry — a remainder only ever rests on a
    /// vetted book.
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// CHECK: validated against the quoter entry's registered accounts
    /// (`ClobMarket::from_quoter`), so a valid entry cannot be pointed at an
    /// arbitrary account.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: locked to the registered quoter program.
    #[account(address = quoter.load()?.program_id)]
    pub clob_program: UncheckedAccount<'info>,
    /// CHECK: the quoter CPI signer PDA — what a book's `place_authority` is,
    /// and the authority on nothing else.
    #[account(seeds = [QUOTER_SIGNER_SEED], bump)]
    pub quoter_signer: UncheckedAccount<'info>,
    /// Wake-hint host for the rested remainder. Optional as on every CLOB
    /// placement path: a market whose conditions were never initialized must
    /// still be fillable, and a missed hint costs crank latency, not liveness.
    #[account(
        mut,
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            market_index.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub crank_conditions: Option<AccountLoader<'info, ClobCrankConditionsV0>>,
}

/// `market_index` is an argument rather than being read off the order because
/// the `crank_conditions` PDA seed needs it at account-resolution time, before
/// any account is loaded. It is checked against the order's own market below,
/// so a mismatch is a malformed transaction rather than a wrong book.
#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_fill_perp_order_v1<'c: 'info, 'info>(
    ctx: Context<'info, FillOrderV1<'info>>,
    order_id: Option<u32>,
    signed_route: Vec<Pubkey>,
    market_index: u16,
) -> Result<()> {
    let (order_id, order_market_index) = {
        let user = &load!(ctx.accounts.user)?;
        let order_id = order_id.unwrap_or_else(|| user.get_last_order_id());
        match user.get_order(order_id) {
            Some(order) => (order_id, order.market_index),
            None => {
                msg!("Order does not exist {}", order_id);
                return Ok(());
            }
        }
    };
    validate!(
        order_market_index == market_index,
        ErrorCode::DefaultError,
        "fill is for market {} but the order is on market {}",
        market_index,
        order_market_index
    )?;

    let user_key = &ctx.accounts.user.key();
    crate::instructions::keeper::fill_order_v1_entry(
        FillAccounts {
            state: &ctx.accounts.state,
            filler: &ctx.accounts.filler,
            filler_stats: &ctx.accounts.filler_stats,
            user: &ctx.accounts.user,
            user_stats: &ctx.accounts.user_stats,
        },
        ctx.remaining_accounts,
        order_id,
        market_index,
        signed_route,
        Some(ClobRemainderRoute {
            quoter: &ctx.accounts.quoter,
            clob_market: &ctx.accounts.clob_market,
            clob_program: &ctx.accounts.clob_program,
            quoter_signer: &ctx.accounts.quoter_signer,
            quoter_signer_nonce: ctx.bumps.quoter_signer,
            crank_conditions: ctx.accounts.crank_conditions.as_ref(),
        }),
    )
    .inspect_err(|_e| {
        msg!(
            "Err filling order id {} for user {} for market index {}",
            order_id,
            user_key,
            market_index
        );
    })?;

    Ok(())
}
