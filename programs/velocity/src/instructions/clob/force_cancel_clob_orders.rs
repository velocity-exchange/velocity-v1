//! Force-cancel a deteriorated account's CLOB orders. This is the CLOB arm of
//! the `force_cancel_orders` keeper flow. It is also how a failing account's
//! placed-trigger shadows are reclaimed. The slot-side sweep skips those
//! shadows, because their live orders rest on the book.
//!
//! The gates are the gates of `force_cancel_orders`. The account must fail
//! initial margin or sit below its equity floor, which is the cleanup before a
//! liquidation. Risk-reducing orders are skipped, because cancelling one would
//! only make the account worse. The keeper reads the user's orders off the book
//! and passes their `OrderRef`s. The CLOB rejects a hint that no longer belongs
//! to this user. The keeper earns the same flat fee per cancelled order, and
//! one transfer at the end charges the user's quote deposit.
//!
//! The handler is not gated on the quoter entry's active and approved flags. A
//! dead book still needs a failing maker's orders reclaimed.
//!
//! The crank has two modes, like the evict and expiry cranks. A signed keeper
//! cranks for its own filler. Otherwise the protocol `User` is passed as the
//! filler and no signature is required, which is how a relay turner drives it.
//!
//! Every gate answers a case of no work with success rather than an error. A
//! fill that would touch a doomed maker puts this instruction in front of
//! itself to clear the way, and relay races to do the same thing. Whichever
//! lands second must not fail the transaction. A caller that is wrong about
//! something it declared still fails loudly. That covers a ref belonging to
//! another user, and a side that does not match the order.

use {
    crate::{
        controller::{orders::pay_keeper_flat_reward_for_spot, position::get_position_index},
        error::ErrorCode,
        instructions::{
            constraints::*,
            optional_accounts::{load_maps, AccountMaps},
        },
        load_mut,
        math::{
            constants::QUOTE_SPOT_MARKET_INDEX, orders::is_order_position_reducing,
            safe_math::SafeMath,
        },
        msg,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            events::OrderActionExplanation,
            perp_market_map::MarketSet,
            prop_amm::{
                ClobCancelAllArgsV0, ClobCancelAllOutcomeV0, ClobCancelOrderArgsV0,
                ClobCancelSides, ClobCancelSidesExt, ClobMarket, ClobOrderRefV0,
                ClobRemovedOrderV0, ClobSide, ClobUserRefV0, QuoterSlabV0, WireDirectionExt,
            },
            spot_market_map::{get_writable_spot_market_set, SpotMarketMap},
            state::State,
            user::{User, UserStats},
        },
        validate,
    },
    anchor_lang::prelude::*,
    std::ops::DerefMut,
};

/// Refs per call, bounding CPI count and compute. The book answers about at
/// most `CLOB_ORDER_VIEW_CEILING` refs in one call, so this cannot exceed it.
pub const MAX_FORCE_CANCEL_CLOB_ORDERS: usize = 8;
const _: () =
    assert!(MAX_FORCE_CANCEL_CLOB_ORDERS <= crate::state::prop_amm::CLOB_ORDER_VIEW_CEILING,);

/// One order the caller wants reclaimed.
///
/// The side is declared, not read, since a node carries no side of its own.
/// It lets the risk-reducing test run before the CPI, but is not trusted: the handler checks it against the real side the CLOB removal returns.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct ForceCancelClobRefV0 {
    pub order_ref: ClobOrderRefV0,
    pub side: ClobSide,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct ForceCancelClobOrdersArgs {
    pub market_index: u16,
    /// The orders to cancel one by one, each judged not risk-reducing by the
    /// declared side. Empty asks for the whole-side sweep instead.
    pub order_refs: Vec<ForceCancelClobRefV0>,
}

#[derive(Accounts)]
#[instruction(args: ForceCancelClobOrdersArgs)]
pub struct ForceCancelClobOrders<'info> {
    pub state: AccountLoader<'info, State>,
    /// CHECK: in signed-keeper mode this must sign for `filler`. In
    /// program-keeper mode, where the protocol `User` is the filler and relay
    /// turners call, it is only the reservoir payout target and needs no
    /// signature.
    #[account(mut)]
    pub authority: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = can_crank_for_filler(&filler, &authority, &state)?
    )]
    pub filler: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&filler, &filler_stats)?
    )]
    pub filler_stats: AccountLoader<'info, UserStats>,
    /// The deteriorated account whose CLOB orders are being reclaimed.
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    /// Not gated on the active and approved flags, because a dead book still
    /// needs a failing maker's orders reclaimed. The header's book pointer
    /// survives a suspension, so the `has_one` still passes on a killed
    /// book.
    #[account(
        has_one = clob_market,
        constraint = quoter_slab.load()?.market == args.market_index,
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: the slab's `has_one` binds it to the book the admin approved.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration. The handler checks it again through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    /// Wake-hint host. It is optional, as on every other CLOB path.
    #[account(
        mut,
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            args.market_index.to_le_bytes().as_ref(),
        ],

        bump
    )]
    pub crank_conditions: Option<AccountLoader<'info, ClobCrankConditionsV0>>,
}

pub fn handle_force_cancel_clob_orders<'c: 'info, 'info>(
    ctx: Context<'info, ForceCancelClobOrders<'info>>,
    args: ForceCancelClobOrdersArgs,
) -> Result<()> {
    let ForceCancelClobOrdersArgs {
        market_index,
        order_refs,
    } = args;
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;
    let program_keeper_mode = is_protocol_user(&ctx.accounts.filler, &ctx.accounts.state)?;
    validate!(
        !program_keeper_mode || ctx.accounts.crank_conditions.is_some(),
        ErrorCode::CrankConditionsAccountRequired,
        "program-keeper force-cancel requires the market's conditions account"
    )?;

    validate!(
        order_refs.len() <= MAX_FORCE_CANCEL_CLOB_ORDERS,
        ErrorCode::TooManyForceCancelRefs,
        "pass at most {} order refs, got {}",
        MAX_FORCE_CANCEL_CLOB_ORDERS,
        order_refs.len()
    )?;

    let mut maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &get_writable_spot_market_set(QUOTE_SPOT_MARKET_INDEX),
        clock.slot,
        state.slot_clock(),
        // The State's rails, as in `force_cancel_orders`. The oracle decides
        // whether this account is failing, so it answers to the configured
        // staleness and confidence bounds rather than to the defaults.
        Some(state.oracle_guard_rails),
    )?;

    let clob = ClobMarket::from_slab(
        &ctx.accounts.quoter_slab,
        market_index,
        &ctx.accounts.clob_market,
        &ctx.accounts.clob_program,
    )?;

    // The gate. The account must be failing, as in `force_cancel_orders`, and
    // the refs must name this user's risk-increasing orders.
    let plan = {
        let user = &mut load_mut!(ctx.accounts.user)?;
        if !has_force_cancel_grounds(user, &mut maps, market_index)? {
            return Ok(());
        }

        let user_ref = user.clob_user_ref();
        let sweep = decide_sweep(user, market_index);
        let refs = select_cancellable_refs(&clob, &order_refs, user_ref, &sweep)?;
        ForceCancelPlan {
            user_ref,
            refs,
            sweep: sweep.sides,
        }
    };

    if plan.refs.is_empty() && plan.sweep.is_none() {
        msg!("nothing of this user's is reclaimable on this book");
        return Ok(());
    }

    let removals = cancel_orders_on_book(&clob, &plan)?;

    // Every cancel record below is stamped with this price. Read it once,
    // before the user borrow, because the records are its only readers.
    let oracle_price = {
        let oracle_id = maps.perp_market_map.get_ref(&market_index)?.oracle_id();
        maps.oracle_map.get_price_data(&oracle_id)?.price
    };

    // The orders this crank reclaimed. Reaching this point does not prove any
    // work was done: `open_bids`/`open_asks` count a user's slot orders too, so
    // a sweep can remove zero without erroring. Paying anyway would let a
    // failing account drain the market's reservoir in a loop and stall every crank, liquidations included.
    let reclaimed_orders = removals.orders.len() as u64
        + removals.swept.map_or(0, |outcome| {
            u64::from(outcome.bid_orders) + u64::from(outcome.ask_orders)
        });

    unwind_cancelled_orders(
        ctx.accounts,
        &state,
        &maps.spot_market_map,
        &clob,
        &clock,
        market_index,
        oracle_price,
        &plan,
        &removals,
    )?;

    pay_crank_reward(
        &ctx.accounts.crank_conditions,
        &ctx.accounts.authority,
        program_keeper_mode,
        reclaimed_orders,
    )?;

    msg!(
        "force-cancelled {} clob orders for user {}",
        removals.orders.len(),
        ctx.accounts.user.key()
    );

    Ok(())
}

/// What the gate authorized this crank to reclaim.
struct ForceCancelPlan {
    /// The account the orders belong to, as the book names it.
    user_ref: ClobUserRefV0,
    /// The refs to cancel one at a time. Each ref is this user's, and the
    /// declared side makes the order risk-increasing.
    refs: Vec<ForceCancelClobRefV0>,
    /// The sides the whole-side sweep takes. `None` when neither side
    /// qualifies.
    sweep: Option<ClobCancelSides>,
}

/// What the book gave back.
struct ClobRemovals {
    /// One removal per ref in [`ForceCancelPlan::refs`], in the same order.
    orders: Vec<ClobRemovedOrderV0>,
    /// The sweep's per-side totals. `None` when no sweep ran.
    swept: Option<ClobCancelAllOutcomeV0>,
}

/// The sides the sweep takes, and the position the decision was made against.
struct SweepDecision {
    /// The user's base position in this market. Zero when it holds none.
    position_base: i64,
    /// The sides to sweep. `None` when neither side qualifies.
    sides: Option<ClobCancelSides>,
}

/// True when this market's risk-increasing orders may be reclaimed.
///
/// The account must fail its initial margin requirement or sit below its
/// equity floor, which are the grounds `force_cancel_orders` answers to. A
/// market that still meets its own requirement is left alone. Both no-work
/// answers return `false` rather than
/// an error, because a force-cancel put in front of a fill races relay for the
/// same work.
fn has_force_cancel_grounds(
    user: &User,
    maps: &mut AccountMaps,
    market_index: u16,
) -> Result<bool> {
    let grounds = crate::controller::orders::ForceCancelGrounds::measure(user, maps)?;

    if !grounds.any() {
        // There is no work here, which is not a refusal. A force-cancel put in
        // front of a fill races relay for the same work, and the account may
        // have recovered since the caller looked.
        msg!("account meets its requirements; nothing to force-cancel");
        return Ok(false);
    }

    if grounds.market_recoverable(user, market_index)? {
        msg!("market {} meets its margin requirement", market_index);
        return Ok(false);
    }

    Ok(true)
}

/// Decide which whole sides go in one sweep instead of one cancel per order.
///
/// At least one whole side is always beyond saving, often both, and goes in a
/// single sweep. `is_order_position_reducing` marks an order risk-increasing
/// unless it faces an open position, so a flat account has no reducing side.
///
/// The sweep stops a maker from outrunning its own cleanup. Each resting order
/// costs `OPEN_ORDER_MARGIN_REQUIREMENT`, so a few dollars buys the 255-order
/// per-position ceiling. Clearing eight at a time is 32 transactions the
/// keeper pays for and an insolvent account may never repay. The sweep takes them in one CPI, and per-order refs stay for the reducing side's tail, bounded by how far past flat it reaches.
fn decide_sweep(user: &User, market_index: u16) -> SweepDecision {
    let position = user.get_perp_position(market_index).ok();
    let position_base = position.map(|p| p.base_asset_amount).unwrap_or(0);

    let (bids_swept, asks_swept) = match position_base.cmp(&0) {
        core::cmp::Ordering::Greater => (true, false),
        core::cmp::Ordering::Less => (false, true),
        core::cmp::Ordering::Equal => (true, true),
    };

    // A zero aggregate proves this side rests nothing on the book. The
    // aggregate counts a user's slot orders too, so only a zero is conclusive. Skipping
    // the call keeps a one-sided account from paying for a CPI that can remove
    // nothing.
    let bids_swept = bids_swept && position.is_some_and(|p| p.open_bids != 0);
    let asks_swept = asks_swept && position.is_some_and(|p| p.open_asks != 0);
    let sides = match (bids_swept, asks_swept) {
        (true, true) => Some(ClobCancelSides::Both),
        (true, false) => Some(ClobCancelSides::Bids),
        (false, true) => Some(ClobCancelSides::Asks),
        (false, false) => None,
    };

    SweepDecision {
        position_base,
        sides,
    }
}

/// Ask the book what each hinted ref still holds. A ref that no longer names a
/// live order comes back empty, because relay or a fill reached it first. That
/// is the expected outcome of the race rather than an error. A ref that names
/// another user's order means the caller was wrong about what it passed, and it
/// fails loudly.
///
/// Risk-reducing orders are dropped here rather than after the cancel.
/// Cancelling one would only make the account worse, and a caller whose refs
/// went stale against a position that moved must not fail the transaction for
/// it. That decision needs the order's size, so this function asks before
/// removing rather than reading the answer out of the removal.
fn select_cancellable_refs(
    clob: &ClobMarket<'_, '_>,
    order_refs: &[ForceCancelClobRefV0],
    user_ref: ClobUserRefV0,
    sweep: &SweepDecision,
) -> Result<Vec<ForceCancelClobRefV0>> {
    let views = clob
        .reader()
        .orders(order_refs.iter().map(|r| r.order_ref).collect())?;
    Ok(order_refs
        .iter()
        .zip(views.iter())
        .filter(|(_, view)| view.found())
        .map(|(order_ref, view)| {
            validate!(
                view.user == user_ref,
                ErrorCode::InvalidUserAccount,
                "order {} belongs to {}/{}, not the passed user",
                order_ref.order_ref.order_id,
                view.user.authority,
                view.user.sub_account_id
            )?;

            // The sweep takes this whole side, so a per-order CPI would be a
            // second call for work already done.
            if sweep
                .sides
                .is_some_and(|sides| sides.includes(order_ref.side.to_position_direction()))
            {
                return Ok(None);
            }

            let reducing = is_order_position_reducing(
                &order_ref.side.to_position_direction(),
                view.base_asset_amount,
                sweep.position_base,
            )?;

            Ok((!reducing).then_some(*order_ref))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>())
}

/// Take the planned orders off the book. The caller holds no user borrow,
/// because every removal is a CPI.
///
/// The per-order removals run first, while the node indices the plan carries
/// are still current. The sweep below moves the book and would invalidate them.
/// The sweep itself names no index, so it is safe to run second.
///
/// `force` is set on this path alone. A taker-origin remainder is bound to its
/// activation window against its own owner, but it is still an open order that
/// holds margin, so a liquidation must be able to reclaim it.
fn cancel_orders_on_book(
    clob: &ClobMarket<'_, '_>,
    plan: &ForceCancelPlan,
) -> Result<ClobRemovals> {
    let orders: Vec<ClobRemovedOrderV0> = plan
        .refs
        .iter()
        .map(|order_ref| {
            clob.cancel(ClobCancelOrderArgsV0 {
                order_ref: order_ref.order_ref,
                user: plan.user_ref,
                force: true,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let swept = plan
        .sweep
        .map(|sides| {
            clob.cancel_all(ClobCancelAllArgsV0 {
                user: plan.user_ref,
                sides,
                force: true,
            })
        })
        .transpose()?;

    Ok(ClobRemovals { orders, swept })
}

/// Unwind the user's aggregates for everything that left the book, then pay
/// the keeper its flat fee per reclaimed order.
///
/// One borrow of the user account covers the whole stage. Every CPI already
/// ran, so no call here re-enters the account.
fn unwind_cancelled_orders(
    accounts: &ForceCancelClobOrders<'_>,
    state: &State,
    spot_market_map: &SpotMarketMap<'_>,
    clob: &ClobMarket<'_, '_>,
    clock: &Clock,
    market_index: u16,
    oracle_price: i64,
    plan: &ForceCancelPlan,
    removals: &ClobRemovals,
) -> Result<()> {
    let mut total_fee = 0u64;
    let user = &mut load_mut!(accounts.user)?;
    let mut filler = load_mut!(accounts.filler)?;
    let position_index = get_position_index(&user.perp_positions, market_index)?;
    for (order_ref, removed) in plan.refs.iter().zip(removals.orders.iter()) {
        validate!(
            removed.user == plan.user_ref,
            ErrorCode::InvalidUserAccount,
            "clob cancelled an order for a different user"
        )?;

        // The declared side decided, before the CPI, that this order was not
        // risk-reducing. A caller that declared the side wrong had a different
        // order cancelled from the one the test judged, so the test did not
        // apply. The caller was wrong about what it passed, so fail loudly.
        validate!(
            removed.side == order_ref.side,
            ErrorCode::ForceCancelSideMismatch,
            "order {} rested on the other side than declared",
            removed.order_id
        )?;

        let direction = removed.side.to_position_direction();
        // The cleanup also frees a placed trigger's shadow permanently. A
        // failing account must not re-arm.
        user.cleanup_removed_clob_order(
            market_index,
            &direction,
            removed.base_asset_amount,
            removed.reduce_only,
            removed.order_id,
        )?;

        total_fee = total_fee.safe_add(state.perp_fee_structure.flat_filler_fee)?;
        super::emit_clob_cancel_record(
            clock.unix_timestamp,
            oracle_price,
            &accounts.user.key(),
            super::ClobOrderFacts::from_removed(removed, market_index, clock.slot),
            OrderActionExplanation::InsufficientFreeCollateral,
            Some(accounts.filler.key()),
            Some(state.perp_fee_structure.flat_filler_fee),
            user.perp_positions[position_index].is_isolated(),
        )?;
    }

    // The sweep unwinds by its per-side totals. That is the same arithmetic as
    // one unwind per order, at a fixed cost. Each placement reserved its own
    // amount, so the sum cannot exceed what is reserved. The unwind covers both
    // directions whatever sides were asked for, so the reserve moves by exactly
    // what left the book.
    if let (Some(sides), Some(swept)) = (plan.sweep, removals.swept) {
        validate!(
            swept.user == plan.user_ref,
            ErrorCode::InvalidUserAccount,
            "clob swept orders for a different user"
        )?;

        let orders = user.unwind_swept_orders(&clob.reader(), market_index, sides, &swept)?;
        total_fee = total_fee.safe_add(
            state
                .perp_fee_structure
                .flat_filler_fee
                .safe_mul(orders.into())?,
        )?;

        if !swept.exhaustive {
            // The CLOB stopped at its per-call cap. Everything unwound here
            // is real, and the caller repeats the call to take the rest.
            msg!("sweep hit the clob's per-call cap; orders remain");
        }
    }

    // A full exchange halt stops the fee, not the cancel, so this instruction
    // carries no `exchange_not_paused` gate. A failing account must stay
    // reachable while halted. Its `User.orders` twin `force_cancel_orders`
    // refuses outright under the same halt, because the fee moves value.
    let exchange_halted = state.get_exchange_status()?.is_all();
    if exchange_halted && total_fee > 0 {
        msg!("exchange halted; cancelling without the keeper fee");
    }

    pay_keeper_flat_reward_for_spot(
        user,
        Some(&mut filler),
        spot_market_map.get_quote_spot_market_mut()?.deref_mut(),
        if exchange_halted { 0 } else { total_fee },
        clock.slot,
    )?;

    user.update_last_active_slot(clock.slot);

    Ok(())
}

/// Pay the relay turner out of the reservoir when the turner cranked, and only
/// for a crank that reclaimed something. `liquidate_perp_with_fill` applies the
/// same rule to its own reward. A crank that removed nothing is a correct
/// outcome rather than an error, but it is not work, and paying for it empties
/// the reservoir.
fn pay_crank_reward<'info>(
    crank_conditions: &Option<AccountLoader<'info, ClobCrankConditionsV0>>,
    authority: &UncheckedAccount<'info>,
    program_keeper_mode: bool,
    reclaimed_orders: u64,
) -> Result<()> {
    if let Some(conditions_loader) = crank_conditions {
        let payment = {
            let conditions = load_mut!(conditions_loader)?;
            u64::from(conditions.crank_payments.force_cancel)
        };

        if program_keeper_mode && reclaimed_orders > 0 {
            ClobCrankConditionsV0::pay_keeper(
                conditions_loader,
                &authority.to_account_info(),
                payment,
            )?;
        }
    }

    Ok(())
}
