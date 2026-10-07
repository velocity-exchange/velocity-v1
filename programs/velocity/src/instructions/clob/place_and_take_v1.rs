//! `place_and_take_perp_order_v1`, the CLOB-aware taker route.
//!
//! The taker's order is detached. The handler builds it on the stack, checks
//! margin, and fills it through the router across the vAMM and the quoter
//! books. It never writes the order into `User.orders`. The restable remainder
//! rests on the market's CLOB taker-origin, because an order that can rest and
//! be matched belongs on the CLOB.
//!
//! The transaction carries the `(User, UserStats)` pair of every maker the
//! books name, because a fill settles only for users it can reach. The caller
//! does not choose those counterparties: the books do.

use {
    crate::{
        instructions::{
            constraints::*, place_and_take_perp_order_v1, ClobRemainderRoute, PlaceAndTakeAccounts,
            PlaceAndTakeRequest,
        },
        state::{
            order_params::{OrderParams, PlaceAndTakeOrderSuccessCondition},
            prop_amm::QuoterSlabV0,
            state::State,
            user::{User, UserStats},
        },
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
#[instruction(args: PlaceAndTakePerpOrderV1Args)]
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
    /// The market's quoter slab. The remainder only ever rests on the vetted
    /// book that its `Clob` slot names.
    #[account(
        has_one = clob_market,
        constraint = quoter_slab.load()?.market == args.params.market_index,
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: `ClobMarket::from_slab` checks this against the book slot's
    /// registered response account. A valid slot cannot be pointed at an
    /// arbitrary account.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration. The handler checks it again through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct PlaceAndTakePerpOrderV1Args {
    pub params: OrderParams,
    pub success_condition: Option<PlaceAndTakeOrderSuccessCondition>,
}

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_place_and_take_perp_order_v1<'c: 'info, 'info>(
    ctx: Context<'info, PlaceAndTakeV1<'info>>,
    args: PlaceAndTakePerpOrderV1Args,
) -> Result<()> {
    let PlaceAndTakePerpOrderV1Args {
        params,
        success_condition,
    } = args;

    // The transaction carries no flow attestation. Only the signed-message
    // route can attest, so a book with a speed bump rests this order whole.
    let taker_served_window = false;
    let synchronous_take = crate::instructions::synchronous_take_allowed(
        taker_served_window,
        &ctx.accounts.quoter_slab,
        params.market_index,
    )?;

    place_and_take_perp_order_v1(
        PlaceAndTakeAccounts {
            state: &ctx.accounts.state,
            user: &ctx.accounts.user,
            user_stats: &ctx.accounts.user_stats,
            remaining_accounts: ctx.remaining_accounts,
        },
        PlaceAndTakeRequest {
            params,
            success_condition,
            taker_served_window,
            synchronous_take,
        },
        ClobRemainderRoute {
            quoter_slab: &ctx.accounts.quoter_slab,
            clob_market: &ctx.accounts.clob_market,
            clob_program: &ctx.accounts.clob_program,
        },
    )
}
