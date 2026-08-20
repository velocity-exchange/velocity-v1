//! Cancel a resting CLOB order. The CLOB verifies the order belongs to the
//! passed `User` (velocity has already verified the signer controls that
//! `User`) and returns the removed order, whose remaining size velocity
//! unwinds from the open-order aggregates. Deliberately NOT gated on the
//! quoter entry's active/approved flags — a maker must always be able to
//! pull their orders off a killed or de-listed book.

use {
    crate::{
        controller::position::{decrease_open_bids_and_asks, get_position_index},
        error::ErrorCode,
        instructions::constraints::*,
        load_mut, msg,
        signer::QUOTER_SIGNER_SEED,
        state::{
            prop_amm::{
                ClobCancelOrderArgsV0, ClobMarket, ClobOrderRefV0, QuoterV0, WireDirectionExt,
            },
            state::State,
            user::User,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct CancelClobOrder<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    pub authority: Signer<'info>,
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// CHECK: validated against the quoter entry's registered execute
    /// accounts in the handler.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: locked to the registered quoter program.
    #[account(address = quoter.load()?.program_id)]
    pub clob_program: UncheckedAccount<'info>,
    /// CHECK: the quoter CPI signer PDA — what a book's `place_authority` is
    /// set to. Deliberately not the vault authority: signer privilege is
    /// inherited by a callee, so the key velocity hands an external program
    /// must be the authority on nothing.
    #[account(seeds = [QUOTER_SIGNER_SEED], bump)]
    pub quoter_signer: UncheckedAccount<'info>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct CancelClobOrderParams {
    pub market_index: u16,
    /// The hint returned at placement; the CLOB fails closed on a stale one.
    pub order_ref: ClobOrderRefV0,
}

pub fn handle_cancel_clob_order(
    ctx: Context<CancelClobOrder>,
    params: CancelClobOrderParams,
) -> Result<()> {
    let clock = Clock::get()?;

    let clob = ClobMarket::from_quoter(
        &*ctx.accounts.quoter.load()?,
        params.market_index,
        &ctx.accounts.clob_market,
        &ctx.accounts.clob_program,
        &ctx.accounts.quoter_signer,
        ctx.bumps.quoter_signer,
    )?;

    // CPI cancel; ownership travels in the args in derivable form and the
    // CLOB verifies it against the node.
    let user_ref = {
        let user = crate::load!(ctx.accounts.user)?;
        crate::state::prop_amm::ClobUserRefV0 {
            authority: user.authority,
            sub_account_id: user.sub_account_id.into(),
        }
    };
    let removed = clob.cancel(ClobCancelOrderArgsV0 {
        order_ref: params.order_ref,
        user: user_ref,
    })?;
    validate!(
        removed.user == user_ref,
        ErrorCode::DefaultError,
        "clob cancelled an order for {}/{} instead of the passed user",
        removed.user.authority,
        removed.user.sub_account_id
    )?;

    // Unwind the removed order's remaining size from the aggregates the
    // placement reserved.
    let mut user = load_mut!(ctx.accounts.user)?;
    let position_index = get_position_index(&user.perp_positions, params.market_index)?;
    decrease_open_bids_and_asks(
        &mut user.perp_positions[position_index],
        &removed.side.to_position_direction(),
        removed.base_asset_amount,
        true,
    )?;
    user.perp_positions[position_index].open_orders = user.perp_positions[position_index]
        .open_orders
        .saturating_sub(1);
    user.decrement_open_orders(false);
    // If this was a placed trigger's live order, its shadow slot frees too —
    // cancelling here is how a user cancels a placed trigger.
    user.release_placed_trigger_slot(
        params.market_index,
        removed.order_id,
        crate::state::user::OrderStatus::Canceled,
    );
    user.update_last_active_slot(clock.slot);

    msg!(
        "cancelled clob order {} for user {}",
        removed.order_id,
        ctx.accounts.user.key()
    );
    Ok(())
}
