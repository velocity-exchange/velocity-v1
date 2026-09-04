//! `place_and_take_perp_order_v1` — the CLOB-aware taker route.
//!
//! The taker's order is ephemeral: built on the stack, margin-checked, filled
//! through the router across the vAMM, the quoter books, and the passed DLOB
//! makers, and never written into `User.orders`. The restable remainder rests
//! on the market's CLOB taker-origin ("if it can rest and be matched, it
//! lives on the CLOB", applied to the taker flow's leftover).
//!
//! Why a new instruction rather than optional accounts on v0: appending
//! optional accounts to a shipped `#[derive(Accounts)]` changes the account
//! list every existing client builds, so v0 keeps its exact `master` shape
//! forever and callers opt into the book by naming this endpoint. The two
//! paths are separate bodies: v0 runs the legacy slot-order fill
//! ([`crate::instructions::place_and_take_perp_order_legacy`]); this endpoint
//! runs the ephemeral routed fill
//! ([`crate::instructions::place_and_take_perp_order_v1`]).

use {
    crate::{
        instructions::{constraints::*, place_and_take_perp_order_v1, ClobRemainderRoute},
        state::{
            order_params::OrderParams,
            prop_amm::QuoterSlabV0,
            state::State,
            user::{User, UserStats},
        },
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
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
    /// The market's quoter slab — the remainder only ever rests on the
    /// vetted book its `Clob` slot names.
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: validated against the book slot's registered response account
    /// (`ClobMarket::from_slab`), so a valid slot can't be pointed at an
    /// arbitrary account.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration; the handler re-checks through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    /// The flow authority, signing this transaction as a named account —
    /// swift builds and signs its own user transactions, so presence marks
    /// the flow attested. Absent reads as unattested, which on a book with
    /// a speed bump rests the order whole instead of filling. The zero key
    /// cannot sign, so an unset flow authority admits nobody.
    #[account(
        constraint = flow_authority.key()
            == state.load()?.hot_key(crate::state::state::HotRole::FlowAuthority)
            @ crate::error::ErrorCode::UnattestedSynchronousTake
    )]
    pub flow_authority: Option<Signer<'info>>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct PlaceAndTakePerpOrderV1Args {
    pub params: OrderParams,
    /// Bit 0 selects a success condition (`PlaceAndTakeOrderSuccessCondition`);
    /// a u32 for wire compatibility with the v0 `optional_params`.
    pub success_condition: Option<u32>,
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
        success_condition: optional_params,
    } = args;
    let (taker_served_window, synchronous_take) = {
        let attested = ctx.accounts.flow_authority.is_some();
        let synchronous = crate::instructions::synchronous_take_allowed(
            attested,
            &ctx.accounts.quoter_slab,
            params.market_index,
        )?;
        (attested, synchronous)
    };
    place_and_take_perp_order_v1(
        &ctx.accounts.state,
        &ctx.accounts.user,
        &ctx.accounts.user_stats,
        ctx.remaining_accounts,
        params,
        optional_params,
        ClobRemainderRoute {
            quoter_slab: &ctx.accounts.quoter_slab,
            clob_market: &ctx.accounts.clob_market,
            clob_program: &ctx.accounts.clob_program,
        },
        taker_served_window,
        synchronous_take,
    )
}
