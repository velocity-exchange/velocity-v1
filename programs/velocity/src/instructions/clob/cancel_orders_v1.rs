//! Remove every order a maker holds on a CLOB, on one side or on both, in a
//! single instruction.
//!
//! The per-order [`super::cancel_order_v1`] costs a velocity instruction and a
//! CPI round trip for each order. Repricing a twenty-quote ladder needs twenty
//! of them, and a long enough ladder stops fitting in one transaction. This
//! sweep is one CPI, and the book returns the aggregates as per-side totals.
//! Unwinding twenty orders therefore costs what unwinding one costs.
//!
//! The handler is not gated on the quoter entry's active or approved flags, and
//! not on `exchange_not_paused`. The per-order cancel is ungated for the same
//! reason. A maker must always be able to remove quotes from a killed,
//! de-listed or halted book.
//!
//! The CLOB caps how many orders one call removes, and reports whether it
//! finished. This handler unwinds by what the call removed, never by what the
//! caller asked for. A capped sweep is a correct unwind over a smaller set, and
//! a repeat call converges.

use {
    crate::{
        controller::position::PositionDirection,
        error::ErrorCode,
        instructions::constraints::*,
        load_mut, msg,
        state::{
            prop_amm::{
                ClobCancelAllArgsV0, ClobCancelAllOutcomeExt, ClobCancelSides, ClobCancelSidesExt,
                ClobMarket, QuoterSlabV0,
            },
            user::User,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
#[instruction(params: CancelOrdersV1Params)]
pub struct CancelOrdersV1<'info> {
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    pub authority: Signer<'info>,
    /// The market's quoter slab. The book's configuration is its `Clob` slot.
    #[account(
        has_one = clob_market,
        constraint = quoter_slab.load()?.market == params.market_index,
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: the slab's `has_one` binds it to the book the admin approved.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration. The handler checks it again through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct CancelOrdersV1Params {
    pub market_index: u16,
    pub sides: ClobCancelSides,
}

pub fn handle_cancel_orders_v1(
    ctx: Context<CancelOrdersV1>,
    params: CancelOrdersV1Params,
) -> Result<()> {
    let clock = Clock::get()?;

    let clob = ClobMarket::from_slab(
        &ctx.accounts.quoter_slab,
        params.market_index,
        &ctx.accounts.clob_market,
        &ctx.accounts.clob_program,
    )?;

    // The sweep runs with no borrow of `user` held. Ownership travels in the
    // args in derivable form, and the CLOB checks it against every node it
    // takes.
    let user_ref = {
        let user = crate::load!(ctx.accounts.user)?;
        user.clob_user_ref()
    };
    let removed = clob.cancel_all(ClobCancelAllArgsV0 {
        user: user_ref,
        sides: params.sides,
        force: false,
    })?;
    validate!(
        removed.user == user_ref,
        ErrorCode::DefaultError,
        "clob swept orders for {}/{} instead of the passed user",
        removed.user.authority,
        removed.user.sub_account_id
    )?;

    // A side the caller did not ask for must come back empty. The unwind below
    // does not depend on this. It applies whatever the book reported, so
    // velocity stays consistent with the book either way. A book that reports
    // removals on a side it was not asked to walk is a bug, so fail here rather
    // than absorb it.
    validate!(
        [PositionDirection::Long, PositionDirection::Short]
            .iter()
            .copied()
            .filter(|direction| !params.sides.includes(*direction))
            .all(|direction| {
                removed.base_for(direction) == 0 && removed.orders_for(direction) == 0
            }),
        ErrorCode::DefaultError,
        "clob reported removals on a side that was not swept"
    )?;

    if removed.orders() == 0 {
        // Nothing was resting. This is not an error. A maker who fires the
        // kill switch twice must not get a failed transaction that reads as a
        // real problem.
        msg!(
            "no clob orders to cancel for user {}",
            ctx.accounts.user.key()
        );
        return Ok(());
    }

    {
        let mut user = load_mut!(ctx.accounts.user)?;
        // This is the owner's own exit, so the release clamps at the
        // reservation instead of failing. A book that reports wrong totals must
        // not be able to keep its maker's margin reserved.
        user.exit_swept_orders(&clob.reader(), params.market_index, params.sides, &removed)?;
        user.update_last_active_slot(clock.slot);
    }

    if removed.exhaustive {
        msg!(
            "cancelled all {} clob orders ({} bid / {} ask) for user {}",
            removed.orders(),
            removed.bid_orders,
            removed.ask_orders,
            ctx.accounts.user.key()
        );
    } else {
        msg!(
            "cancelled {} clob orders for user {}; the book's per-call cap stopped \
             the sweep early — repeat to clear the rest",
            removed.orders(),
            ctx.accounts.user.key()
        );
    }
    Ok(())
}
