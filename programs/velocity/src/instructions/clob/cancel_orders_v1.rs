//! Pull every order a maker holds on a CLOB — one side or both — in a single
//! instruction.
//!
//! The per-order [`super::cancel_order_v1`] is what a maker had before, and it
//! costs a velocity instruction plus a CPI round trip *each*: repricing a
//! twenty-quote ladder meant twenty of them, which at some point stops fitting
//! in one transaction at all. Here the sweep is one CPI, and the aggregates come
//! back as per-side totals — so unwinding twenty orders costs exactly what
//! unwinding one does.
//!
//! Deliberately NOT gated on the quoter entry's active/approved flags, and not
//! on `exchange_not_paused`, for the same reason the per-order cancel isn't: a
//! maker must always be able to get their quotes off a killed, de-listed or
//! halted book.
//!
//! The CLOB caps how many orders one call removes and reports whether it
//! finished. This handler unwinds by what the call actually removed, never by
//! what was asked for, so a capped sweep is not a partial failure — it is a
//! smaller correct one, and repeating the instruction converges.

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
    /// The market's quoter slab; the book's config is its `Clob` slot.
    #[account(
        has_one = clob_market,
        constraint = quoter_slab.load()?.market == params.market_index,
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: the slab's `has_one` binds it to the book the admin approved.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration; the handler re-checks through the slot.
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

    // CPI the sweep with no user borrow held; ownership travels in the args in
    // derivable form and the CLOB verifies it against every node it takes.
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

    // A side that wasn't asked for must come back empty. The unwind below does
    // not depend on this — it applies whatever was reported, so velocity stays
    // consistent with the book either way — but a book reporting removals on a
    // side it was never asked to walk is a bug worth failing on rather than
    // absorbing.
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
        // Nothing was resting. Not an error: a maker firing their kill switch
        // twice must not get a failed transaction that reads as a real problem.
        // Returning early also skips the wake-hint repair below, which is right
        // — the book is unchanged, so the hints already describe it.
        msg!(
            "no clob orders to cancel for user {}",
            ctx.accounts.user.key()
        );
        return Ok(());
    }

    {
        let mut user = load_mut!(ctx.accounts.user)?;
        // The owner's own exit, so the unwind clamps at the reservation: a
        // book that reports garbage must not be able to keep its maker on it.
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
