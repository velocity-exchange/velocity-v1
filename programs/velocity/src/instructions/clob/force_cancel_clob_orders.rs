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
        controller::{
            orders::pay_keeper_flat_reward_for_spot,
            position::{decrease_open_bids_and_asks, get_position_index},
        },
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
        signer::CLOB_AUTHORITY_SEED,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            events::OrderActionExplanation,
            margin_calculation::MarginContext,
            perp_market_map::MarketSet,
            prop_amm::{
                ClobCancelAllArgsV0, ClobCancelOrderArgsV0, ClobCancelSides, ClobCancelSidesExt,
                ClobMarket, ClobOrderRefV0, ClobRemovedOrderV0, ClobSide, ClobUserRefV0, QuoterV0,
                WireDirectionExt,
            },
            spot_market_map::get_writable_spot_market_set,
            state::State,
            user::{OrderStatus, User, UserStats},
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

#[derive(Accounts)]
#[instruction(market_index: u16)]
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
    /// failing makers' orders reclaimed.
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// CHECK: validated against the quoter entry's registered execute
    /// accounts in the handler.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: locked to the registered quoter program.
    #[account(address = quoter.load()?.program_id)]
    pub clob_program: UncheckedAccount<'info>,
    /// CHECK: the CLOB place authority PDA — what a book's `place_authority`
    /// is set to. Its own key, distinct from the per-entry signer a
    /// third-party quoter is handed: signer privilege is inherited by a
    /// callee, and this one may place and cancel on any book, for any user.
    #[account(seeds = [CLOB_AUTHORITY_SEED], bump)]
    pub clob_authority: UncheckedAccount<'info>,
    /// Wake-hint host; optional like every other CLOB path.
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

pub fn handle_force_cancel_clob_orders<'c: 'info, 'info>(
    ctx: Context<'info, ForceCancelClobOrders<'info>>,
    market_index: u16,
    order_refs: Vec<ForceCancelClobRefV0>,
) -> Result<()> {
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
        None,
    )?;

    let clob = ClobMarket::from_quoter(
        &*ctx.accounts.quoter.load()?,
        market_index,
        &ctx.accounts.clob_market,
        &ctx.accounts.clob_program,
        &ctx.accounts.clob_authority,
        ctx.bumps.clob_authority,
    )?;

    // ---- Gate: the account must actually be failing, same as the DLOB
    // force-cancel, and the refs must be this user's risk-increasing
    // orders. ----
    let (user_ref, cancellable, sweep): (
        ClobUserRefV0,
        Vec<ForceCancelClobRefV0>,
        Option<ClobCancelSides>,
    ) = {
        let user = &mut load_mut!(ctx.accounts.user)?;
        validate!(
            !user.is_being_liquidated(),
            ErrorCode::UserIsBeingLiquidated
        )?;
        validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

        let margin_calc = calculate_margin_requirement_and_total_collateral_and_liability_info(
            user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )?;
        // "Below floor" authorizes a keeper against the user here, so it fails
        // closed the other way from the gates that restrict the user: the floor
        // counts as grounds only when every oracle is valid and the trusted
        // value sits below it, so a bad price cannot manufacture authorization.
        let below_equity_floor = calculate_net_equity_for_floor(
            user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
        )?
        .is_some_and(|net_equity| net_equity.proves_below_floor(user));
        // A tripped breaker is grounds on its own. It is the authority-wide
        // latch that says one of this authority's subaccounts was proven
        // below its floor, it is already permissionless to set, and while it
        // is set every subaccount is barred from risk-increasing activity —
        // so the risk-increasing orders this one is resting cannot legally
        // fill, and holding them on the book only blocks other people's.
        let breaker_tripped = ctx.accounts.user_stats.load()?.is_equity_breaker_tripped();
        if margin_calc.meets_margin_requirement() && !below_equity_floor && !breaker_tripped {
            // Not a "no": a "nothing to do". A prefixed force-cancel races
            // relay for the same work, and the account may also have simply
            // recovered since the caller looked.
            msg!("account meets its requirements; nothing to force-cancel");
            return Ok(());
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
            return Ok(());
        }

        let user_ref = ClobUserRefV0 {
            authority: user.authority,
            sub_account_id: user.sub_account_id.into(),
        };
        let position = user.get_perp_position(market_index).ok();
        let position_base = position.map(|p| p.base_asset_amount).unwrap_or(0);

        // One whole side is always beyond saving, and often both, so it goes
        // in a single sweep instead of one CPI per order. `is_order_position_reducing`
        // only ever answers yes to an order facing an open position, so every
        // order on the side that *adds* to the position is risk-increasing
        // whatever its size — and a flat account has no reducing side at all.
        //
        // This is what stops a maker outrunning its own cleanup. Resting
        // orders cost `OPEN_ORDER_MARGIN_REQUIREMENT` each, so a few dollars
        // buys the per-position ceiling of 255, and clearing those eight at a
        // time is 32 transactions the keeper pays for and an insolvent
        // account may never repay. The sweep takes them in one CPI, and the
        // per-order refs are left to the tail of the reducing side, which is
        // bounded by how far past flat that side's orders reach.
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
        let sweep = match (bids_swept, asks_swept) {
            (true, true) => Some(ClobCancelSides::Both),
            (true, false) => Some(ClobCancelSides::Bids),
            (false, true) => Some(ClobCancelSides::Asks),
            (false, false) => None,
        };

        // Ask the book what each hinted ref still holds. A ref that no longer
        // names a live order comes back empty — relay or a fill got there
        // first, which is the expected outcome of the race, not an error. A
        // ref naming someone else's order is the caller being wrong about
        // what it passed, and fails loudly.
        //
        // Risk-reducing orders are dropped here rather than after the cancel:
        // cancelling one would only make the account worse, and a caller
        // whose refs went stale against a position that moved must not take
        // the transaction down for it. That decision needs the order's size,
        // which is why this asks before removing rather than reading the
        // answer out of what came back.
        let views = clob
            .reader()
            .orders(order_refs.iter().map(|r| r.order_ref).collect())?;
        let cancellable = order_refs
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
                if sweep.is_some_and(|sides| sides.includes(order_ref.side.to_position_direction()))
                {
                    return Ok(None);
                }
                let reducing = is_order_position_reducing(
                    &order_ref.side.to_position_direction(),
                    view.base_asset_amount,
                    position_base,
                )?;
                Ok((!reducing).then_some(*order_ref))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        (user_ref, cancellable, sweep)
    };

    if cancellable.is_empty() && sweep.is_none() {
        msg!("nothing of this user's is reclaimable on this book");
        return Ok(());
    }

    // ---- Cancel CPIs while no user borrows are held. ----
    // Per-order first, while the node indices read above are still current:
    // the sweep below moves the book and would invalidate them. The sweep
    // itself names no index, so it is safe to run second.
    let removed_orders: Vec<ClobRemovedOrderV0> = cancellable
        .iter()
        .map(|order_ref| {
            clob.cancel(ClobCancelOrderArgsV0 {
                order_ref: order_ref.order_ref,
                user: user_ref,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let swept = sweep
        .map(|sides| {
            clob.cancel_all(ClobCancelAllArgsV0 {
                user: user_ref,
                sides,
            })
        })
        .transpose()?;

    // Stamped on every cancel record below. Read once, before the user
    // borrow, because the record is the only thing that wants it.
    let oracle_price = {
        let oracle_id = perp_market_map.get_ref(&market_index)?.oracle_id();
        oracle_map.get_price_data(&oracle_id)?.price
    };

    // Orders this crank actually reclaimed. The reservoir pays for work, and
    // reaching this point does not prove any was done: `cancellable` may be
    // empty, and the sweep is decided from `open_bids`/`open_asks`, which count
    // DLOB orders too. A user holding only DLOB orders therefore sweeps a book
    // that holds nothing of theirs, and `cancel_all_v0` removes zero without
    // erroring. Paying for that would let anyone with a failing account drain
    // the market's reservoir in a loop, which stops every other crank on the
    // market — liquidations included.
    let reclaimed_orders = removed_orders.len() as u64
        + swept.map_or(0, |outcome| {
            u64::from(outcome.bid_orders) + u64::from(outcome.ask_orders)
        });

    // ---- Unwind, skip-filter risk-reducing, fee. ----
    let mut total_fee = 0u64;
    {
        let user = &mut load_mut!(ctx.accounts.user)?;
        let mut filler = load_mut!(ctx.accounts.filler)?;
        let position_index = get_position_index(&user.perp_positions, market_index)?;
        for (order_ref, removed) in cancellable.iter().zip(removed_orders.iter()) {
            validate!(
                removed.user == user_ref,
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
            decrease_open_bids_and_asks(
                &mut user.perp_positions[position_index],
                &direction,
                removed.base_asset_amount,
                true,
            )?;
            user.perp_positions[position_index].open_orders = user.perp_positions[position_index]
                .open_orders
                .saturating_sub(1);
            user.decrement_open_orders(false);
            // A placed trigger's shadow frees for good — a failing account
            // must not re-arm.
            user.release_placed_trigger_slot(market_index, removed.order_id, OrderStatus::Canceled);
            total_fee = total_fee.safe_add(state.perp_fee_structure.flat_filler_fee)?;
            super::emit_clob_cancel_record(
                clock.unix_timestamp,
                oracle_price,
                &ctx.accounts.user.key(),
                super::ClobOrderFacts {
                    order_id: removed.client_order_id,
                    market_index,
                    direction,
                    price: removed.price,
                    base_asset_amount: removed.base_asset_amount,
                    base_asset_amount_filled: 0,
                    max_ts: removed.max_ts,
                    slot: clock.slot,
                    taker_origin: removed.taker_origin,
                },
                OrderActionExplanation::InsufficientFreeCollateral,
                Some(ctx.accounts.filler.key()),
                Some(state.perp_fee_structure.flat_filler_fee),
                user.perp_positions[position_index].is_isolated(),
            )?;
        }

        // The sweep unwinds by its per-side totals: identical arithmetic to
        // one unwind per order (each placement reserved its own amount, so
        // the sum cannot exceed what is reserved) at a fixed cost. Both
        // directions regardless of which sides were asked for, so the reserve
        // moves by exactly what left the book.
        if let (Some(sides), Some(swept)) = (sweep, swept) {
            validate!(
                swept.user == user_ref,
                ErrorCode::DefaultError,
                "clob swept orders for a different user"
            )?;
            let orders = crate::state::prop_amm::unwind_swept_orders(
                user,
                &clob.reader(),
                market_index,
                sides,
                &swept,
            )?;
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
    }

    // Pay the relay turner out of the reservoir when it is the one that
    // cranked, and only for a crank that reclaimed something. The same rule
    // `liquidate_perp_with_fill` applies to its own reward: a crank that
    // removed nothing is a correct outcome rather than an error, but it is not
    // work, and paying for it empties the reservoir.
    if let Some(conditions_loader) = &ctx.accounts.crank_conditions {
        let payment = {
            let conditions = load_mut!(conditions_loader)?;
            u64::from(conditions.crank_payments.force_cancel)
        };
        if program_keeper_mode && reclaimed_orders > 0 {
            let conditions_info = conditions_loader.to_account_info();
            let rent_minimum = Rent::get()?.minimum_balance(conditions_info.data_len());
            ClobCrankConditionsV0::pay_keeper_lamports(
                &conditions_info,
                &ctx.accounts.authority.to_account_info(),
                payment,
                rent_minimum,
            )?;
        }
    }

    msg!(
        "force-cancelled {} clob orders for user {}",
        removed_orders.len(),
        ctx.accounts.user.key()
    );
    Ok(())
}
