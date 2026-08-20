//! Pull every order a maker holds on a CLOB — one side or both — in a single
//! instruction.
//!
//! The per-order [`super::cancel_clob_order`] is what a maker had before, and it
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
        controller::position::{
            decrease_open_bids_and_asks, get_position_index, PositionDirection,
        },
        error::ErrorCode,
        instructions::constraints::*,
        load_mut, msg,
        signer::QUOTER_SIGNER_SEED,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            prop_amm::{
                clob_hint_scan, read_clob_node, ClobCancelAllArgsV0, ClobCancelSides, ClobMarket,
                ClobUserRefV0, QuoterV0,
            },
            state::State,
            user::{MarketType, OrderStatus, User},
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
#[instruction(params: CancelAllClobOrdersParams)]
pub struct CancelAllClobOrders<'info> {
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
    /// Wake-hint host; optional like every other CLOB path. Pulling orders can
    /// only *relax* the expiry and activation hints, so a caller that omits it
    /// leaves the cranks waking earlier than they need to — latency, not
    /// liveness.
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

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct CancelAllClobOrdersParams {
    pub market_index: u16,
    pub sides: ClobCancelSides,
}

pub fn handle_cancel_all_clob_orders(
    ctx: Context<CancelAllClobOrders>,
    params: CancelAllClobOrdersParams,
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

    // CPI the sweep with no user borrow held; ownership travels in the args in
    // derivable form and the CLOB verifies it against every node it takes.
    let user_ref = {
        let user = crate::load!(ctx.accounts.user)?;
        ClobUserRefV0 {
            authority: user.authority,
            sub_account_id: user.sub_account_id.into(),
        }
    };
    let removed = clob.cancel_all(ClobCancelAllArgsV0 {
        user: user_ref,
        sides: params.sides,
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
        let position_index = get_position_index(&user.perp_positions, params.market_index)?;

        // One unwind per side, by the summed base the sweep reported. This is
        // the whole point of the aggregate wire: the arithmetic is identical to
        // N per-order unwinds (each placement reserved its own amount, so the
        // sum can never exceed what is reserved) at a fixed cost.
        //
        // Both sides regardless of what was requested, so the reserve always
        // moves by exactly what left the book — the reserved aggregates track
        // the book's contents, not the caller's intent.
        [PositionDirection::Long, PositionDirection::Short]
            .iter()
            .try_for_each(|direction| -> Result<()> {
                decrease_open_bids_and_asks(
                    &mut user.perp_positions[position_index],
                    direction,
                    removed.base_for(*direction),
                    true,
                )?;
                Ok(())
            })?;

        let orders = removed.orders();
        user.perp_positions[position_index].open_orders = user.perp_positions[position_index]
            .open_orders
            .saturating_sub(orders.min(u8::MAX as u32) as u8);
        (0..orders).for_each(|_| user.decrement_open_orders(false));

        // Free the placed-trigger shadows whose live orders the sweep took.
        let book = ctx.accounts.clob_market.try_borrow_data()?;
        let shadows = crate::state::prop_amm::release_swept_trigger_shadows(
            &mut user,
            &book,
            params.market_index,
            params.sides,
        );
        drop(book);
        if shadows > 0 {
            msg!("released {} placed-trigger shadows", shadows);
        }

        user.update_last_active_slot(clock.slot);
    }

    // Repair the wake hints from the post-sweep book: orders left, so the
    // earliest expiry and the earliest pending activation can only have moved
    // later.
    if let Some(conditions_loader) = &ctx.accounts.crank_conditions {
        let (min_expiry, min_activation) =
            clob_hint_scan(&ctx.accounts.clob_market.try_borrow_data()?, clock.slot);
        let mut conditions = load_mut!(conditions_loader)?;
        conditions.repair_expiry(min_expiry)?;
        conditions.repair_activation(min_activation)?;
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
