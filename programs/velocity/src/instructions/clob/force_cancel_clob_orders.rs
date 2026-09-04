//! Force-cancel a deteriorated account's CLOB orders — the CLOB arm of the
//! `force_cancel_orders` keeper flow, and how placed-trigger shadows on a
//! failing account get reclaimed (the DLOB-side sweep deliberately skips
//! them: their live orders rest on the book).
//!
//! Same gates as the DLOB force-cancel: the account must fail initial
//! margin or sit below its equity floor (pre-liquidation cleanup), and
//! risk-*reducing* orders are skipped — cancelling those would only make
//! the account worse. The keeper reads the user's orders off the book and
//! passes their `OrderRef`s; each hint fails closed on the CLOB side if it
//! no longer belongs to this user. The keeper earns the same flat fee per
//! cancelled order, charged to the user's quote deposit in one transfer at
//! the end.
//!
//! Deliberately not gated on the quoter entry's active/approved flags —
//! dead books still need failing makers' orders reclaimed.
//!
//! Dual-mode, like the evict and expiry cranks: a signed keeper cranks for
//! its own filler, or the protocol `User` is passed as filler and no
//! signature is required, which is how a relay turner drives it.
//!
//! Every gate answers "nothing to do" with success rather than an error. A
//! fill that would touch a doomed maker prefixes this instruction to clear
//! the way, and relay is racing to do the same thing; whichever lands second
//! must not take the transaction down with it. Only a caller that is wrong
//! about something it declared — a ref belonging to another user, a side that
//! does not match the order — still fails loudly.

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
            constants::QUOTE_SPOT_MARKET_INDEX,
            margin::{
                calculate_margin_requirement_and_total_collateral_and_liability_info,
                calculate_net_equity_for_floor, MarginRequirementType,
            },
            orders::is_order_position_reducing,
            safe_math::SafeMath,
        },
        msg,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            events::OrderActionExplanation,
            margin_calculation::MarginContext,
            oracle_map::OracleMap,
            perp_market_map::{MarketSet, PerpMarketMap},
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
/// The side is declared rather than read, because a node carries no side of
/// its own — the book stores it by which list the node is linked into, and
/// finding that out costs a walk from the head. Declaring it lets the
/// risk-reducing test run *before* the CPI, so a reducing order is passed
/// over instead of being cancelled and then reverting the call. The
/// declaration is not trusted: the removal the CLOB returns carries the real
/// side and is checked against it.
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
    /// CHECK: in signed-keeper mode this must sign for `filler`; in
    /// program-keeper mode (protocol `User` as filler, relay turners) it is
    /// only the reservoir payout target and no signature is required.
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
    /// Carries the authority-wide equity breaker, which is grounds on its own.
    #[account(constraint = is_stats_for_user(&user, &user_stats)?)]
    pub user_stats: AccountLoader<'info, UserStats>,
    /// Deliberately not gated on active/approved: dead books still need
    /// failing makers' orders reclaimed — the header's book pointer survives
    /// a suspension, so the `has_one` still passes on a killed book.
    #[account(
        has_one = clob_market,
        constraint = quoter_slab.load()?.market == args.market_index,
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: the slab's `has_one` binds it to the book the admin approved.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration; the handler re-checks through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    /// Wake-hint host; optional like every other CLOB path.
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
        ErrorCode::DefaultError,
        "program-keeper force-cancel requires the market's conditions account"
    )?;

    validate!(
        order_refs.len() <= MAX_FORCE_CANCEL_CLOB_ORDERS,
        ErrorCode::DefaultError,
        "pass at most {} order refs, got {}",
        MAX_FORCE_CANCEL_CLOB_ORDERS,
        order_refs.len()
    )?;

    let AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &get_writable_spot_market_set(QUOTE_SPOT_MARKET_INDEX),
        clock.slot,
        state.slot_clock(),
        None,
    )?;

    let clob = ClobMarket::from_slab(
        &ctx.accounts.quoter_slab,
        market_index,
        &ctx.accounts.clob_market,
        &ctx.accounts.clob_program,
    )?;

    // ---- Gate: the account must actually be failing, same as the DLOB
    // force-cancel, and the refs must be this user's risk-increasing
    // orders. ----
    let plan = {
        let user = &mut load_mut!(ctx.accounts.user)?;
        if !has_force_cancel_grounds(
            user,
            &ctx.accounts.user_stats,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            market_index,
        )? {
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

    // Stamped on every cancel record below. Read once, before the user
    // borrow, because the record is the only thing that wants it.
    let oracle_price = {
        let oracle_id = perp_market_map.get_ref(&market_index)?.oracle_id();
        oracle_map.get_price_data(&oracle_id)?.price
    };

    // Orders this crank actually reclaimed. The reservoir pays for work, and
    // reaching this point does not prove any was done: `plan.refs` may be
    // empty, and the sweep is decided from `open_bids`/`open_asks`, which count
    // DLOB orders too. A user holding only DLOB orders therefore sweeps a book
    // that holds nothing of theirs, and `cancel_all_v0` removes zero without
    // erroring. Paying for that would let anyone with a failing account drain
    // the market's reservoir in a loop, which stops every other crank on the
    // market — liquidations included.
    let reclaimed_orders = removals.orders.len() as u64
        + removals.swept.map_or(0, |outcome| {
            u64::from(outcome.bid_orders) + u64::from(outcome.ask_orders)
        });

    unwind_cancelled_orders(
        ctx.accounts,
        &state,
        &spot_market_map,
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
    /// The refs to cancel one at a time. Each one is this user's, and the
    /// declared side makes it risk-increasing.
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
/// The account must fail its initial margin requirement, sit below its equity
/// floor, or carry a tripped equity breaker. A market that still meets its own
/// requirement is left alone. Both "nothing to do" answers are `false` rather
/// than an error, because a prefixed force-cancel races relay for the same
/// work.
fn has_force_cancel_grounds(
    user: &User,
    user_stats: &AccountLoader<'_, UserStats>,
    perp_market_map: &PerpMarketMap<'_>,
    spot_market_map: &SpotMarketMap<'_>,
    oracle_map: &mut OracleMap<'_>,
    market_index: u16,
) -> Result<bool> {
    validate!(
        !user.is_being_liquidated(),
        ErrorCode::UserIsBeingLiquidated
    )?;
    validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

    let margin_calc = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        MarginContext::standard(MarginRequirementType::Initial),
    )?;
    // "Below floor" authorizes a keeper against the user here, so it fails
    // closed the other way from the gates that restrict the user: the floor
    // counts as grounds only when every oracle is valid and the trusted
    // value sits below it, so a bad price cannot manufacture authorization.
    let below_equity_floor =
        calculate_net_equity_for_floor(user, perp_market_map, spot_market_map, oracle_map)?
            .is_some_and(|net_equity| net_equity.proves_below_floor(user));
    // A tripped breaker is grounds on its own. It is the authority-wide
    // latch that says one of this authority's subaccounts was proven
    // below its floor, it is already permissionless to set, and while it
    // is set every subaccount is barred from risk-increasing activity —
    // so the risk-increasing orders this one is resting cannot legally
    // fill, and holding them on the book only blocks other people's.
    let breaker_tripped = user_stats.load()?.is_equity_breaker_tripped();
    if margin_calc.meets_margin_requirement() && !below_equity_floor && !breaker_tripped {
        // Not a "no": a "nothing to do". A prefixed force-cancel races
        // relay for the same work, and the account may also have simply
        // recovered since the caller looked.
        msg!("account meets its requirements; nothing to force-cancel");
        return Ok(false);
    }
    // Per-market arm of the DLOB sweep's skip logic: an isolated
    // position answers to its own requirement, cross positions to the
    // cross requirement. The breaker outranks both — it freezes every
    // subaccount, whatever this one market looks like.
    let market_isolated = user
        .get_perp_position(market_index)
        .map(|position| position.is_isolated())
        .unwrap_or(false);
    let market_recoverable = !breaker_tripped
        && if market_isolated {
            margin_calc.meets_isolated_margin_requirement(market_index)?
        } else {
            margin_calc.meets_cross_margin_requirement() && !below_equity_floor
        };
    if market_recoverable {
        msg!("market {} meets its margin requirement", market_index);
        return Ok(false);
    }

    Ok(true)
}

/// Decide which whole sides go in one sweep instead of one cancel per order.
///
/// One whole side is always beyond saving, and often both, so it goes
/// in a single sweep instead of one CPI per order. `is_order_position_reducing`
/// only ever answers yes to an order facing an open position, so every
/// order on the side that *adds* to the position is risk-increasing
/// whatever its size — and a flat account has no reducing side at all.
///
/// This is what stops a maker outrunning its own cleanup. Resting
/// orders cost `OPEN_ORDER_MARGIN_REQUIREMENT` each, so a few dollars
/// buys the per-position ceiling of 255, and clearing those eight at a
/// time is 32 transactions the keeper pays for and an insolvent
/// account may never repay. The sweep takes them in one CPI, and the
/// per-order refs are left to the tail of the reducing side, which is
/// bounded by how far past flat that side's orders reach.
fn decide_sweep(user: &User, market_index: u16) -> SweepDecision {
    let position = user.get_perp_position(market_index).ok();
    let position_base = position.map(|p| p.base_asset_amount).unwrap_or(0);

    let (bids_swept, asks_swept) = match position_base.cmp(&0) {
        core::cmp::Ordering::Greater => (true, false),
        core::cmp::Ordering::Less => (false, true),
        core::cmp::Ordering::Equal => (true, true),
    };
    // A zero aggregate proves this side rests nothing on the book (it
    // counts the DLOB too, so only the zero direction is conclusive), and
    // skipping the call keeps a one-sided account from paying for a CPI
    // that can remove nothing.
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

/// Ask the book what each hinted ref still holds. A ref that no longer
/// names a live order comes back empty — relay or a fill got there
/// first, which is the expected outcome of the race, not an error. A
/// ref naming someone else's order is the caller being wrong about
/// what it passed, and fails loudly.
///
/// Risk-reducing orders are dropped here rather than after the cancel:
/// cancelling one would only make the account worse, and a caller
/// whose refs went stale against a position that moved must not take
/// the transaction down for it. That decision needs the order's size,
/// which is why this asks before removing rather than reading the
/// answer out of what came back.
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
                ErrorCode::DefaultError,
                "order {} belongs to {}/{}, not the passed user",
                order_ref.order_ref.order_id,
                view.user.authority,
                view.user.sub_account_id
            )?;
            // The sweep is taking this whole side; a per-order CPI for it
            // would be a second call for work already done.
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
/// Per-order first, while the node indices the plan carries are still
/// current: the sweep below moves the book and would invalidate them. The
/// sweep itself names no index, so it is safe to run second.
///
/// `force` is set on this path alone. A taker-origin remainder is bound to
/// its activation window against its own owner, but it is still an open
/// order holding margin, so liquidation has to be able to reclaim it.
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
            ErrorCode::DefaultError,
            "clob cancelled an order for a different user"
        )?;
        // The declared side decided, before the CPI, that this order was
        // not risk-reducing. A caller that declared it wrong got a
        // different order cancelled than the one it was judged on, so the
        // judgement did not apply — that is the caller being wrong about
        // what it passed, and it fails loudly.
        validate!(
            removed.side == order_ref.side,
            ErrorCode::DefaultError,
            "order {} rested on the other side than declared",
            removed.order_id
        )?;
        let direction = removed.side.to_position_direction();
        // The cleanup also frees a placed trigger's shadow for good — a
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

    // The sweep unwinds by its per-side totals: identical arithmetic to
    // one unwind per order (each placement reserved its own amount, so
    // the sum cannot exceed what is reserved) at a fixed cost. Both
    // directions regardless of which sides were asked for, so the reserve
    // moves by exactly what left the book.
    if let (Some(sides), Some(swept)) = (plan.sweep, removals.swept) {
        validate!(
            swept.user == plan.user_ref,
            ErrorCode::DefaultError,
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
            // The CLOB stopped at its per-call cap. Everything unwound
            // here is real; the caller repeats to take the rest.
            msg!("sweep hit the clob's per-call cap; orders remain");
        }
    }

    pay_keeper_flat_reward_for_spot(
        user,
        Some(&mut filler),
        spot_market_map.get_quote_spot_market_mut()?.deref_mut(),
        total_fee,
        clock.slot,
    )?;
    user.update_last_active_slot(clock.slot);

    Ok(())
}

/// Pay the relay turner out of the reservoir when it is the one that
/// cranked, and only for a crank that reclaimed something. The same rule
/// `liquidate_perp_with_fill` applies to its own reward: a crank that
/// removed nothing is a correct outcome rather than an error, but it is not
/// work, and paying for it empties the reservoir.
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
