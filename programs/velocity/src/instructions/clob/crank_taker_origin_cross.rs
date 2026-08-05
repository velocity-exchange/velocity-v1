//! `crank_taker_origin_cross`: resolve one taker-origin cross on a CLOB book.
//!
//! A migrated taker remainder rests on the book flagged taker-origin, and the
//! book refuses to let anyone *take* it while a live counterparty crosses it —
//! so the improvement between the two prices cannot be won by landing a
//! transaction at the activation slot. Somebody still has to hand that
//! improvement to the taker, and this is that somebody: permissionless, and
//! paid out of the improvement it delivers.
//!
//! Against an ordinary maker the resolution runs in the one direction the book
//! leaves open: consume the **counterparty** with `execute_v0` (an ordinary
//! fill at its own stored price), lift the taker-origin order off with
//! `cancel_order_v0`, and settle the two as an ordinary two-user match at the
//! counterparty's price. The cancel goes first, so a refusal costs nothing and
//! the book is never left holding one side of a half-settled pair.
//!
//! When the counterparty is a **second taker remainder**, neither side can be
//! consumed — the book withholds both from `execute_v0` — so the crank cancels
//! them both and velocity prices the match itself. Price-time priority decides
//! whose price: the order that rested first is the maker, the later one is the
//! aggressor, and the improvement goes to the aggressor exactly as it would
//! against a maker who chose to quote there.
//!
//! Nothing here resembles `crank_cross_match`'s protocol pass-through. That
//! crank exists for two *makers* crossing, where neither side is demanding
//! liquidity and the spread is unclaimed arbitrage the protocol middles for a
//! floored surplus. Here one side is the aggressor by construction, the
//! improvement belongs to it, and the only cut anyone takes is the cranker's
//! reward.
//!
//! Relay discovery is [`stage_taker_origin_cross`], reached from the cross
//! conditions' resolver rather than from a condition of its own — see it for why
//! the existing wakes already cover this crank.

use {
    super::crank_common::{next_matchable, ResolveClobCrank},
    crate::{
        controller::{
            self,
            orders::TakerOriginCounterparty,
            position::{decrease_open_bids_and_asks, get_position_index, PositionDirection},
        },
        error::ErrorCode,
        instructions::{
            constraints::*,
            optional_accounts::{load_maps, AccountMaps},
            StagedCall,
        },
        load, load_mut,
        math::{casting::Cast, safe_math::SafeMath},
        msg,
        signer::QUOTER_SIGNER_SEED,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            events::TakerOriginCrossRecordV0,
            pdas,
            perp_market_map::{get_writable_perp_market_set, MarketSet},
            prop_amm::{
                clob_hint_scan, clob_resting_prefix, read_clob_u32, ClobCancelOrderArgsV0,
                ClobMarket, ClobNodeView, ClobOrderRefV0, ClobPlaceOrderArgsV0, ClobRemovedOrderV0,
                ClobSide, ClobUserRefV0, Direction, ExecuteArgsV0, QuoterSubjects,
                QuoterUserSetRef, QuoterV0, CLOB_BEST_ASK_OFFSET, CLOB_BEST_BID_OFFSET,
            },
            state::State,
            user::{OrderStatus, User, UserStats},
            user_map::load_user_maps,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[cfg(test)]
mod tests;

#[derive(Accounts)]
#[instruction(market_index: u16)]
pub struct CrankTakerOriginCross<'info> {
    pub state: AccountLoader<'info, State>,
    /// CHECK: in signed-keeper mode this must sign for `filler` (the
    /// constraint below enforces it); in program-keeper mode it is only the
    /// lamport payout target — relay's keeper-placeholder slot — and no
    /// signature is required.
    #[account(mut)]
    pub authority: UncheckedAccount<'info>,
    /// The cranker's margin account: the crank reward lands here as quote.
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
    /// Owner of the taker-origin order — the taker of this match. Verified
    /// against the identity the CLOB reports on removal, so a wrong account
    /// fails the crank rather than settling against someone else.
    #[account(mut)]
    pub taker: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&taker, &taker_stats)?
    )]
    pub taker_stats: AccountLoader<'info, UserStats>,
    /// The market's CLOB registry entry.
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// CHECK: validated against the quoter entry's registered execute accounts
    /// (`ClobMarket::from_quoter`), so a valid entry cannot be pointed at an
    /// arbitrary account.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: locked to the registered quoter program.
    #[account(address = quoter.load()?.program_id)]
    pub clob_program: UncheckedAccount<'info>,
    /// CHECK: the quoter CPI signer PDA — what a book's `place_authority` is
    /// set to, and the authority on nothing else.
    #[account(seeds = [QUOTER_SIGNER_SEED], bump)]
    pub quoter_signer: UncheckedAccount<'info>,
    /// The market's relay conditions account: the wake-hint host and the
    /// lamport reservoir. Optional so a signed keeper can crank a market whose
    /// conditions were never initialized; required in program-keeper mode.
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

/// Which shape of cross this is, and therefore how the counterparty leaves the
/// book.
enum TakerOriginCrossKind {
    /// The counterparty is an ordinary maker: it is consumed with `execute_v0`,
    /// an ordinary fill at its own stored price.
    Maker,
    /// The counterparty is a second taker remainder, and is cancelled off the
    /// book rather than consumed. Its node index is carried because that cancel
    /// needs the handle.
    ///
    /// **Not a variation on the maker path.** The book skips a taker-origin
    /// order that a live counterparty crosses, so an `execute_v0` aimed at this
    /// one would pass over it and fill whatever is behind it instead — settling
    /// against a maker the cross was never priced for, or failing the response
    /// validation. Cancelling both sides takes them out of the book's reach
    /// entirely, and two removals is everything the settlement needs.
    Pair { counterparty_node: u32 },
}

/// One resolvable cross: the aggressor (a taker-origin order) and the
/// counterparty whose price the match settles at.
struct TakerOriginCross {
    taker_origin: ClobNodeView,
    taker_origin_node: u32,
    counterparty: ClobNodeView,
    /// Side the aggressor rests on (the counterparty is on the other).
    side: ClobSide,
    kind: TakerOriginCrossKind,
}

impl TakerOriginCross {
    /// Direction the taker is trading: a resting bid wants to buy.
    fn taker_direction(&self) -> PositionDirection {
        self.side.to_position_direction()
    }

    fn size(&self) -> u64 {
        self.taker_origin
            .base_asset_amount
            .min(self.counterparty.base_asset_amount)
    }
}

/// Did `a` rest before `b`?
///
/// Price-time priority between two crossing taker remainders: the earlier one
/// is the maker, and its price is the one the match settles at. Ties on the
/// slot break on the CLOB order id, which is sound rather than arbitrary — a
/// book's `next_order_id` only ever increases, so within one slot the lower id
/// was placed first.
fn rested_first(a: &ClobNodeView, b: &ClobNodeView) -> bool {
    (a.placed_slot, a.order_id) < (b.placed_slot, b.order_id)
}

/// The taker-origin cross at the top of the book, if there is one.
///
/// Only the best matchable order on each side is considered. A taker-origin
/// order sitting *behind* a better one on its own side is crossed by the same
/// counterparty as that better order, which makes the pair an ordinary
/// maker×maker cross — `crank_cross_match`'s job — and once that clears, the
/// taker-origin order is the best and this crank sees it. So the two cranks
/// compose instead of duplicating each other's search.
///
/// When **both** sides are taker-origin, both are demanding liquidity and
/// neither price is "the counterparty's price" by construction, so the tie is
/// broken the way a book breaks every other one: whoever rested first is the
/// maker at its own price, and the later arrival is the aggressor that crosses
/// into it. It earns the improvement for the same reason a taker always does —
/// it was the one that came to trade — and the earlier order gets the price it
/// was already offering, which is all a maker is ever promised.
///
/// The one refused pair, leaving the book untouched: a counterparty owned by
/// the taker (self-trade).
fn find_taker_origin_cross(data: &[u8], slot: u64, now: i64) -> Result<Option<TakerOriginCross>> {
    let head = |offset: usize| -> Result<u32> {
        read_clob_u32(data, offset).ok_or_else(|| error!(ErrorCode::DefaultError))
    };
    let bid = next_matchable(data, head(CLOB_BEST_BID_OFFSET)?, slot, now);
    let ask = next_matchable(data, head(CLOB_BEST_ASK_OFFSET)?, slot, now);
    let (Some((bid_node, bid)), Some((ask_node, ask))) = (bid, ask) else {
        return Ok(None);
    };
    if bid.price < ask.price {
        return Ok(None);
    }
    let bid_aggresses = (ClobSide::Bid, bid_node, bid, ask, ask_node);
    let ask_aggresses = (ClobSide::Ask, ask_node, ask, bid, bid_node);
    let (side, taker_origin_node, taker_origin, counterparty, counterparty_node) =
        match (bid.is_taker_origin, ask.is_taker_origin) {
            (false, false) => return Ok(None),
            (true, false) => bid_aggresses,
            (false, true) => ask_aggresses,
            (true, true) if rested_first(&bid, &ask) => ask_aggresses,
            (true, true) => bid_aggresses,
        };
    if counterparty.user_ref() == taker_origin.user_ref() {
        return Ok(None);
    }
    Ok(Some(TakerOriginCross {
        taker_origin,
        taker_origin_node,
        counterparty,
        side,
        kind: if counterparty.is_taker_origin {
            TakerOriginCrossKind::Pair { counterparty_node }
        } else {
            TakerOriginCrossKind::Maker
        },
    }))
}

/// Put a leftover back on the book, if the book can hold it.
///
/// Cancelling the whole of a taker's resting order because one unit of it
/// crossed would take its queue position for nothing, so whatever the match did
/// not consume goes back — still taker-origin, and immediately matchable rather
/// than behind a fresh speed bump, since it has already served its auction
/// window. Its reservation and open-order slot never moved, so a leftover that
/// rests costs no `User` bookkeeping at all.
///
/// `None` when nothing rested: either there was no leftover, or it was below the
/// book's minimum, which the book culls rather than rests — and then
/// [`unwind_leftover`] is the other half of this.
fn rest_leftover(
    clob: &ClobMarket,
    removed: &ClobRemovedOrderV0,
    leftover: u64,
    max_ts: i64,
) -> Result<Option<ClobOrderRefV0>> {
    if leftover == 0 || leftover < clob.min_order_size()? {
        return Ok(None);
    }
    clob.place(ClobPlaceOrderArgsV0 {
        side: removed.side,
        price: removed.price,
        base_asset_amount: leftover,
        activation_delay_slots: Some(0),
        max_ts,
        user: removed.user,
        taker_origin: true,
    })
    .map(Some)
}

/// Take an order that did not go back on the book off its owner's aggregates:
/// the open-order slot comes off, and so does whatever the leftover still
/// reserved. Runs for a consumed order (nothing left to unwind but the slot) and
/// for a sub-min leftover the book cannot hold.
fn unwind_leftover(
    user: &mut User,
    market_index: u16,
    direction: &PositionDirection,
    leftover: u64,
    order_id: u64,
) -> Result<()> {
    let position_index = get_position_index(&user.perp_positions, market_index)?;
    if leftover > 0 {
        decrease_open_bids_and_asks(
            &mut user.perp_positions[position_index],
            direction,
            leftover,
            true,
        )?;
    }
    user.perp_positions[position_index].open_orders = user.perp_positions[position_index]
        .open_orders
        .saturating_sub(1);
    user.decrement_open_orders(false);
    user.release_placed_trigger_slot(market_index, order_id, OrderStatus::Canceled);
    Ok(())
}

pub fn handle_crank_taker_origin_cross<'c: 'info, 'info>(
    ctx: Context<'info, CrankTakerOriginCross<'info>>,
    market_index: u16,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;
    let program_keeper_mode = load!(ctx.accounts.filler)?.authority == state.signer;
    validate!(
        !program_keeper_mode || ctx.accounts.crank_conditions.is_some(),
        ErrorCode::DefaultError,
        "program-keeper crank requires the market's conditions account"
    )?;
    // Three distinct margin accounts: each is loaded mutably in the same
    // settlement, so an overlap would fail on the borrow rather than say why.
    validate!(
        ctx.accounts.filler.key() != ctx.accounts.taker.key(),
        ErrorCode::DefaultError,
        "the cranker cannot be the taker of the cross it resolves"
    )?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = load_maps(
        remaining_accounts_iter,
        &get_writable_perp_market_set(market_index),
        &MarketSet::new(),
        clock.slot,
        Some(state.oracle_guard_rails),
    )?;
    let (makers_and_referrer, makers_and_referrer_stats) =
        load_user_maps(remaining_accounts_iter, true)?;
    validate!(
        !makers_and_referrer
            .0
            .contains_key(&ctx.accounts.filler.key())
            && !makers_and_referrer
                .0
                .contains_key(&ctx.accounts.taker.key()),
        ErrorCode::DefaultError,
        "the counterparty section must not repeat the taker or the cranker"
    )?;

    let quoter = ctx.accounts.quoter.load()?;
    validate!(
        quoter.is_active && quoter.is_approved,
        ErrorCode::DefaultError,
        "CLOB quoter is not active and approved"
    )?;
    let clob = ClobMarket::from_quoter(
        &quoter,
        market_index,
        &ctx.accounts.clob_market,
        &ctx.accounts.clob_program,
        &ctx.accounts.quoter_signer,
        ctx.bumps.quoter_signer,
    )?;

    // ---- Discovery and pricing: everything that can refuse the cross runs
    // before the first CPI, so a refusal leaves the book exactly as it was.
    let cross = {
        let data = ctx.accounts.clob_market.try_borrow_data()?;
        find_taker_origin_cross(&data, clock.slot, clock.unix_timestamp)?
    }
    .ok_or(ErrorCode::NoTakerOriginCross)?;
    let taker_ref = {
        let taker = load!(ctx.accounts.taker)?;
        ClobUserRefV0 {
            authority: taker.authority,
            sub_account_id: taker.sub_account_id,
        }
    };
    validate!(
        cross.taker_origin.user_ref() == taker_ref,
        ErrorCode::DefaultError,
        "the taker-origin order belongs to {}/{}, not the passed taker",
        cross.taker_origin.user_ref().authority,
        cross.taker_origin.user_ref().sub_account_id
    )?;

    let size = cross.size();
    let taker_direction = cross.taker_direction();
    let (fee, oracle_price, oracle_stale_for_margin, perp_market_oi_before) = {
        let taker_stats = load!(ctx.accounts.taker_stats)?;
        controller::orders::price_taker_origin_cross(
            &state,
            market_index,
            taker_direction,
            cross.taker_origin.price,
            cross.counterparty.price,
            size,
            cross.taker_origin.placed_slot,
            &taker_stats,
            &perp_market_map,
            &mut oracle_map,
            &clock,
        )?
    };

    // ---- Lift the aggressor off the book. First, because the CLOB is the
    // authority on the flag: velocity's byte read found the node, and the
    // removal is what proves it was taker-origin, whose it was, and at what
    // price — before anything settles against those facts. It also leaves the
    // book uncrossed for the counterparty's fill, so the book's own
    // taker-origin protection cannot fire on the way through.
    let removed = clob.cancel(ClobCancelOrderArgsV0 {
        order_ref: ClobOrderRefV0 {
            node_index: cross.taker_origin_node,
            order_id: cross.taker_origin.order_id,
        },
        user: taker_ref,
    })?;
    validate!(
        removed.taker_origin,
        ErrorCode::NoTakerOriginCross,
        "clob order {} is not taker-origin; nothing to reprice",
        removed.order_id
    )?;
    validate!(
        removed.user == taker_ref
            && removed.side == cross.side
            && removed.price == cross.taker_origin.price
            && removed.base_asset_amount >= size,
        ErrorCode::DefaultError,
        "clob removed a different order than the cross was priced against"
    )?;

    // ---- Get the counterparty out of the book, the way its own flag allows.
    let counterparty = match cross.kind {
        // An ordinary maker: consume it. An ordinary fill at its own price,
        // which is what makes the response's quote *be* the price the match
        // settles at.
        TakerOriginCrossKind::Maker => {
            let direction = match taker_direction {
                PositionDirection::Long => Direction::Long,
                PositionDirection::Short => Direction::Short,
            };
            let users = [taker_ref, cross.counterparty.user_ref()];
            let subjects = QuoterSubjects::Book(clob_resting_prefix(
                &ctx.accounts.clob_market.try_borrow_data()?,
                direction.clob_side(),
                size,
                &users,
                &taker_ref,
                clock.slot,
                clock.unix_timestamp,
            ));
            // The execute leg's account list is the registry's, resolved
            // against the accounts this instruction already names — the book,
            // the CPI signer, and the program.
            let mut account_map = std::collections::BTreeMap::new();
            for info in [
                ctx.accounts.clob_market.to_account_info(),
                ctx.accounts.quoter_signer.to_account_info(),
                ctx.accounts.clob_program.to_account_info(),
            ] {
                account_map.insert(info.key(), info);
            }
            let response = quoter.execute(
                market_index,
                ExecuteArgsV0 {
                    direction,
                    size,
                    users: QuoterUserSetRef(&users),
                    taker: Some(taker_ref),
                },
                &ctx.accounts.quoter_signer.key(),
                ctx.bumps.quoter_signer,
                &account_map,
            )?;
            TakerOriginCounterparty::Executed { response, subjects }
        }
        // A second taker remainder: cancel it too. It cannot be consumed —
        // `execute_v0` skips a crossed taker-origin order and would fill past
        // it — and once both sides are off the book there is no gate left to
        // interfere and nothing for a fill to reach the wrong maker through.
        // The removal carries the price, the size and the owner, which is the
        // whole of what settling the pair needs.
        TakerOriginCrossKind::Pair { counterparty_node } => {
            let counterparty_ref = cross.counterparty.user_ref();
            let removed_counterparty = clob.cancel(ClobCancelOrderArgsV0 {
                order_ref: ClobOrderRefV0 {
                    node_index: counterparty_node,
                    order_id: cross.counterparty.order_id,
                },
                user: counterparty_ref,
            })?;
            validate!(
                removed_counterparty.taker_origin,
                ErrorCode::NoTakerOriginCross,
                "clob order {} is not taker-origin; it cannot be the maker of a pair",
                removed_counterparty.order_id
            )?;
            validate!(
                removed_counterparty.user == counterparty_ref
                    && removed_counterparty.side != cross.side
                    && removed_counterparty.price == cross.counterparty.price
                    && removed_counterparty.base_asset_amount >= size,
                ErrorCode::DefaultError,
                "clob removed a different counterparty than the cross was priced against"
            )?;
            TakerOriginCounterparty::Cancelled {
                removed: removed_counterparty,
                base_asset_amount: size,
            }
        }
    };
    drop(quoter);

    let fill = controller::orders::settle_taker_origin_cross(
        &state,
        market_index,
        &removed,
        cross.taker_origin.placed_slot,
        &counterparty,
        &fee,
        oracle_price,
        oracle_stale_for_margin,
        perp_market_oi_before,
        &ctx.accounts.taker,
        &ctx.accounts.taker_stats,
        &ctx.accounts.filler,
        &ctx.accounts.filler_stats,
        &makers_and_referrer,
        &makers_and_referrer_stats,
        &perp_market_map,
        &spot_market_map,
        &mut oracle_map,
        &clock,
    )?;
    validate!(
        fill.base_filled == size,
        ErrorCode::DefaultError,
        "cross settled {} of the {} it was priced for",
        fill.base_filled,
        size
    )?;

    // ---- Whatever the match was too small to consume goes back on the book,
    // still taker-origin. Against an ordinary maker only the aggressor can have
    // a leftover, since the cross is sized to what the counterparty had; two
    // remainders are sized to each other, so the survivor may be either of
    // them. Both are handled the same way, and the book's new order ref is left
    // as the transaction's return data (the CLOB writes it) besides riding the
    // record below.
    let taker_leftover = removed.base_asset_amount.saturating_sub(fill.base_filled);
    let taker_rested = rest_leftover(&clob, &removed, taker_leftover, cross.taker_origin.max_ts)?;
    if taker_rested.is_none() {
        let mut taker = load_mut!(ctx.accounts.taker)?;
        unwind_leftover(
            &mut taker,
            market_index,
            &taker_direction,
            taker_leftover,
            removed.order_id,
        )?;
    }
    let (counterparty_rested, counterparty_leftover) = match &counterparty {
        TakerOriginCounterparty::Cancelled {
            removed: counterparty_removed,
            ..
        } => {
            let leftover = counterparty_removed
                .base_asset_amount
                .saturating_sub(fill.base_filled);
            let rested = rest_leftover(
                &clob,
                counterparty_removed,
                leftover,
                cross.counterparty.max_ts,
            )?;
            if rested.is_none() {
                let mut maker = makers_and_referrer.get_ref_mut(&fill.maker)?;
                unwind_leftover(
                    &mut maker,
                    market_index,
                    &taker_direction.opposite(),
                    leftover,
                    counterparty_removed.order_id,
                )?;
            }
            (rested, leftover)
        }
        // The book kept this counterparty's own accounting: it reported the
        // orders the fill retired and the sub-min remainder it culled, and the
        // settlement unwound both.
        TakerOriginCounterparty::Executed { .. } => (None, 0),
    };
    // Only one leftover can exist — the match is sized to the smaller of the
    // two orders, so the other side is consumed outright.
    let (remainder_owner, remainder, remainder_order_id) = match (taker_rested, counterparty_rested)
    {
        (Some(order_ref), _) => (ctx.accounts.taker.key(), taker_leftover, order_ref.order_id),
        (_, Some(order_ref)) => (fill.maker, counterparty_leftover, order_ref.order_id),
        (None, None) => (Pubkey::default(), 0, 0),
    };

    // ---- Wake hints and the keeper's lamports, as every CLOB crank does.
    if let Some(conditions_loader) = &ctx.accounts.crank_conditions {
        let (min_expiry, min_activation) =
            clob_hint_scan(&ctx.accounts.clob_market.try_borrow_data()?, clock.slot);
        let payment = {
            let mut conditions = load_mut!(conditions_loader)?;
            conditions.repair_expiry(min_expiry)?;
            conditions.repair_activation(min_activation)?;
            conditions.keeper_payment_lamports
        };
        if program_keeper_mode {
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

    let fill_price = fill
        .quote_filled
        .cast::<u128>()?
        .safe_mul(crate::math::constants::BASE_PRECISION_U64.cast()?)?
        .safe_div(fill.base_filled.cast()?)?
        .cast::<u64>()?;
    emit!(TakerOriginCrossRecordV0 {
        ts: clock.unix_timestamp,
        slot: clock.slot,
        market_index,
        taker: ctx.accounts.taker.key(),
        maker: fill.maker,
        filler: ctx.accounts.filler.key(),
        base_asset_amount: fill.base_filled,
        quote_asset_amount: fill.quote_filled,
        rest_price: removed.price,
        fill_price,
        improvement: fee.improvement,
        crank_reward: fill.crank_reward,
        maker_taker_origin: matches!(counterparty, TakerOriginCounterparty::Cancelled { .. }),
        remainder_base_asset_amount: remainder,
        remainder_order_id,
        remainder_owner,
    });
    msg!(
        "taker-origin cross: {} base at {} instead of {}, improvement {} quote, cranker paid {}",
        fill.base_filled,
        fill_price,
        removed.price,
        fee.improvement,
        fill.crank_reward
    );
    Ok(())
}

/// Stage this crank for the book's taker-origin cross, if it has one — the
/// discovery half of [`handle_crank_taker_origin_cross`], called by the cross
/// conditions' resolver (`handle_resolve_crank_cross_match`) ahead of the
/// maker×maker cross it stages otherwise. A `ResolvedCrankV0` names its own
/// executor, so one resolver serves both, and the order is the economics: a
/// taker-origin cross is resolved in the taker's favour before the protocol
/// middles the same crossed book as arbitrage.
///
/// **No condition slot or watch of its own, and none needed.** This crank only
/// ever resolves the tops of the matchable book, so a taker-origin cross can
/// newly appear in exactly two ways, both already wired: a side's best moved —
/// covered by the cross condition's 8-byte change-watch over `best_bid` and
/// `best_ask`, since a crossing order is by definition a new best — or a
/// front-of-book order reached its `activation_slot`, which the cross-activation
/// `AtSlot` hint names precisely (`note_activation` min-folds every placement's,
/// a migrating remainder's included).
///
/// **`min_payment` stays the market's `keeper_payment_lamports`**, the same
/// price the maker×maker cross on that slot carries, because lamports are what
/// relay measures: `assert_paid_v0` watches the payout account's lamport
/// balance, and this crank pays the same reservoir lamports as every other CLOB
/// crank. Its quote-denominated crank reward can legitimately be zero — a dust
/// or equal-price improvement resolves for free by design — so pricing the
/// condition above the lamport payout would make exactly those crosses
/// undiscoverable, and a unit of dust in front of a gated remainder is enough to
/// strand it for its whole life.
///
/// **The blocker-removal wake.** Two remainders can only face each other while
/// something crosses the earlier one, so the moment their pair becomes
/// resolvable is usually the *blocker's* removal rather than a new order's
/// arrival. That moves a head u32 and fires the change-watch whenever the
/// blocker is its side's head, which is the ordinary case. Two shapes it misses:
/// a blocker sitting behind a better-priced order nothing can match yet (its
/// removal rewrites an arena link, not the head), and a blocker that leaves the
/// matchable set by passing its own `max_ts`, which is no write at all. The
/// expire condition's `AtTimestamp` hint covers the second one hop earlier — it
/// fires at that `max_ts`, and removing the expired order then moves the head —
/// and the every-slots cross fallback is the floor under both, so a missed hint
/// costs latency rather than liveness.
pub(super) fn stage_taker_origin_cross(
    ctx: &Context<ResolveClobCrank>,
    clock: &Clock,
) -> Result<Option<StagedCall>> {
    let quoter = ctx.accounts.quoter.load()?;
    if !quoter.is_active || !quoter.is_approved {
        // The crank refuses a killed or unvetted entry, so there is no work to
        // stage against one; reclaiming orders left on a dead book is the
        // eviction and force-cancel paths'.
        return Ok(None);
    }
    let cross = {
        let data = ctx.accounts.clob_market.try_borrow_data()?;
        find_taker_origin_cross(&data, clock.slot, clock.unix_timestamp)?
    };
    let Some(cross) = cross else {
        return Ok(None);
    };

    let (protocol_user, protocol_user_stats) = pdas::protocol_user_pair();
    let taker_ref = cross.taker_origin.user_ref();
    let counterparty_ref = cross.counterparty.user_ref();
    let (taker, taker_stats) = pdas::user_pair(&taker_ref.authority, taker_ref.sub_account_id);
    // The protocol `User` is the filler on this path, and the crank loads each
    // margin account exactly once, so it cannot also be a side of the cross it
    // resolves.
    if taker == protocol_user
        || pdas::user(&counterparty_ref.authority, counterparty_ref.sub_account_id) == protocol_user
    {
        return Ok(None);
    }
    let (market_index, oracle, quote_spot_market_index) = {
        let conditions = ctx.accounts.crank_conditions.load()?;
        (
            conditions.market_index,
            conditions.oracle,
            conditions.quote_spot_market_index,
        )
    };
    Ok(Some(
        crate::staged_call!(CrankTakerOriginCross {
            state: ctx.accounts.state.key(),
            authority: pdas::keeper_placeholder(),
            filler: protocol_user,
            filler_stats: protocol_user_stats,
            taker,
            taker_stats,
            quoter: ctx.accounts.quoter.key(),
            clob_market: ctx.accounts.clob_market.key(),
            clob_program: quoter.program_id,
            quoter_signer: pdas::quoter_signer(),
            crank_conditions: Some(ctx.accounts.crank_conditions.key()),
        })
        // Both `(User, UserStats)` pairs derive from the nodes' own
        // `(authority, sub_account_id)`, which is what the book stores them for.
        .map_section(oracle, quote_spot_market_index, market_index)
        .maker_refs([counterparty_ref])
        .arg(market_index)?,
    ))
}
