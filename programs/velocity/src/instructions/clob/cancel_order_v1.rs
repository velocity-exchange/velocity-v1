//! Cancel a resting CLOB order. The CLOB verifies the order belongs to the
//! passed `User` (velocity has already verified the signer controls that
//! `User`) and returns the removed order, whose remaining size velocity
//! unwinds from the open-order aggregates. Deliberately NOT gated on the
//! quoter entry's active/approved flags — a maker must always be able to
//! pull their orders off a killed or de-listed book.

use {
    crate::{
        error::ErrorCode,
        instructions::constraints::*,
        load_mut, msg,
        state::{
            perp_market::PerpMarket,
            prop_amm::{
                ClobCancelOrderArgsV0, ClobMarket, ClobOrderRefV0, QuoterSlabV0, WireDirectionExt,
            },
            user::User,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
#[instruction(params: CancelOrderV1Params)]
pub struct CancelOrderV1<'info> {
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    pub authority: Signer<'info>,
    /// Read-only, and read for one thing: the cached oracle price the cancel
    /// record is stamped with. Deliberately not an oracle account — a maker
    /// pulling orders off a book must not be able to fail on a stale feed.
    #[account(
        constraint = perp_market.load()?.market_index == params.market_index,
        has_one = quoter_slab,
        has_one = clob_market,
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// The market's quoter slab
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: the perp market's `has_one` binds it to the book the market
    /// designated.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration; the handler re-checks through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct CancelOrderV1Params {
    pub market_index: u16,
    /// The hint returned at placement; the CLOB fails closed on a stale one.
    pub order_ref: ClobOrderRefV0,
}

pub fn handle_cancel_order_v1(
    ctx: Context<CancelOrderV1>,
    params: CancelOrderV1Params,
) -> Result<()> {
    let clock = Clock::get()?;

    let clob = ClobMarket::from_slab(
        &ctx.accounts.quoter_slab,
        params.market_index,
        &ctx.accounts.clob_market,
        &ctx.accounts.clob_program,
    )?;

    // CPI cancel; ownership travels in the args in derivable form and the
    // CLOB verifies it against the node.
    let user_ref = {
        let user = crate::load!(ctx.accounts.user)?;
        user.clob_user_ref()
    };
    let removed = clob.cancel(ClobCancelOrderArgsV0 {
        order_ref: params.order_ref,
        user: user_ref,
        force: false,
    })?;
    validate!(
        removed.user == user_ref,
        ErrorCode::DefaultError,
        "clob cancelled an order for {}/{} instead of the passed user",
        removed.user.authority,
        removed.user.sub_account_id
    )?;

    // Unwind the removed order's remaining size from the aggregates the
    // placement reserved. This also frees a placed trigger's shadow slot —
    // cancelling here is how a user cancels a placed trigger.
    let mut user = load_mut!(ctx.accounts.user)?;
    user.cleanup_removed_clob_order(
        params.market_index,
        &removed.side.to_position_direction(),
        removed.base_asset_amount,
        removed.reduce_only,
        removed.order_id,
    )?;
    user.update_last_active_slot(clock.slot);
    // An order that never filled leaves its owner with no position in the
    // market, which is the ordinary case for a cancel. A missing position is
    // not isolated, so the record says so rather than the cancel failing.
    let is_isolated_position = user
        .get_perp_position(params.market_index)
        .map(|position| position.is_isolated())
        .unwrap_or(false);
    drop(user);

    super::emit_clob_cancel_record(
        clock.unix_timestamp,
        ctx.accounts
            .perp_market
            .load()?
            .market_stats
            .historical_oracle_data
            .last_oracle_price,
        &ctx.accounts.user.key(),
        super::ClobOrderFacts::from_removed(&removed, params.market_index, clock.slot),
        crate::state::events::OrderActionExplanation::None,
        None,
        None,
        is_isolated_position,
    )?;

    msg!(
        "cancelled clob order {} for user {}",
        removed.order_id,
        ctx.accounts.user.key()
    );
    Ok(())
}
