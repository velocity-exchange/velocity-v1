//! `crank_taker_origin_cross`: resolve one taker-origin cross on a CLOB book.
//!
//! A migrated taker remainder rests on the book flagged taker-origin, and the
//! book refuses to let anyone *take* it while a live counterparty crosses it —
//! so the improvement between the two prices cannot be won by landing a
//! transaction at the activation slot. Somebody still has to hand that
//! improvement to the taker, and this is that somebody: permissionless, and
//! paid out of the improvement it delivers.
//!
//! The resolution runs in the one direction the book leaves open: consume the
//! **counterparty** with `execute_v0` (an ordinary fill at its own stored
//! price), lift the taker-origin order off with `cancel_order_v0`, and settle
//! the two as an ordinary two-user match at the counterparty's price. The
//! cancel goes first, so a refusal costs nothing and the book is never left
//! holding one side of a half-settled pair.
//!
//! Nothing here resembles `crank_cross_match`'s protocol pass-through. That
//! crank exists for two *makers* crossing, where neither side is demanding
//! liquidity and the spread is unclaimed arbitrage the protocol middles for a
//! floored surplus. Here one side is the aggressor by construction, the
//! improvement belongs to it, and the only cut anyone takes is the cranker's
//! reward.

use {
    super::crank_common::next_matchable,
    crate::{
        controller::{
            self,
            position::{decrease_open_bids_and_asks, get_position_index, PositionDirection},
        },
        error::ErrorCode,
        instructions::{
            constraints::*,
            optional_accounts::{load_maps, AccountMaps},
        },
        load, load_mut,
        math::{casting::Cast, safe_math::SafeMath},
        msg,
        signer::QUOTER_SIGNER_SEED,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            events::TakerOriginCrossRecordV0,
            perp_market_map::{get_writable_perp_market_set, MarketSet},
            prop_amm::{
                clob_hint_scan, clob_resting_prefix, read_clob_u32, ClobCancelOrderArgsV0,
                ClobMarket, ClobNodeView, ClobOrderRefV0, ClobPlaceOrderArgsV0, ClobSide,
                ClobUserRefV0, Direction, ExecuteArgsV0, QuoterSubjects, QuoterUserSetRef,
                QuoterV0, CLOB_BEST_ASK_OFFSET, CLOB_BEST_BID_OFFSET,
            },
            state::State,
            user::{OrderStatus, User, UserStats},
            user_map::load_user_maps,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

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

/// One resolvable cross: the taker-origin order and the counterparty whose
/// price the match settles at.
struct TakerOriginCross {
    taker_origin: ClobNodeView,
    taker_origin_node: u32,
    counterparty: ClobNodeView,
    /// Side the taker-origin order rests on (the counterparty is on the other).
    side: ClobSide,
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

/// The taker-origin cross at the top of the book, if there is one.
///
/// Only the best matchable order on each side is considered. A taker-origin
/// order sitting *behind* a better one on its own side is crossed by the same
/// counterparty as that better order, which makes the pair an ordinary
/// maker×maker cross — `crank_cross_match`'s job — and once that clears, the
/// taker-origin order is the best and this crank sees it. So the two cranks
/// compose instead of duplicating each other's search.
///
/// Refused pairs, both leaving the book untouched: a counterparty that is
/// itself taker-origin (two aggressors, so neither side's price is "the
/// counterparty's price" — nothing here gets to pick a winner between them),
/// and a counterparty owned by the taker (self-trade).
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
    // The bid first when both are marked: it is the side that would be filled
    // by a taker selling, and nothing distinguishes them otherwise.
    let (side, taker_origin_node, taker_origin, counterparty) = if bid.is_taker_origin {
        (ClobSide::Bid, bid_node, bid, ask)
    } else if ask.is_taker_origin {
        (ClobSide::Ask, ask_node, ask, bid)
    } else {
        return Ok(None);
    };
    if counterparty.is_taker_origin || counterparty.user_ref() == taker_origin.user_ref() {
        return Ok(None);
    }
    Ok(Some(TakerOriginCross {
        taker_origin,
        taker_origin_node,
        counterparty,
        side,
    }))
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

    // ---- Lift the taker-origin order off the book. First, because the CLOB
    // is the authority on the flag: velocity's byte read found the node, and
    // the removal is what proves it was taker-origin, whose it was, and at
    // what price — before anything settles against those facts. It also leaves
    // the book uncrossed for the counterparty's fill, so the book's own
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

    // ---- Consume the counterparty: an ordinary fill at its own price, which
    // is what makes the response's quote *be* the price the match settles at.
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
    // The execute leg's account list is the registry's, resolved against the
    // accounts this instruction already names — the book, the CPI signer, and
    // the program.
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
    drop(quoter);

    let fill = controller::orders::settle_taker_origin_cross(
        &state,
        market_index,
        &removed,
        cross.taker_origin.placed_slot,
        &response,
        &subjects,
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

    // ---- Whatever the counterparty was too small to consume goes back on the
    // book, still taker-origin. Cancelling it outright instead would let a
    // cranker delete a taker's whole resting order by crossing one unit of it,
    // and the remainder has already served its auction window, so it goes back
    // matchable immediately rather than behind a fresh speed bump. Its
    // reservation and open-order slot never moved, so only a dropped remainder
    // unwinds anything. The book's new order ref is left as the transaction's
    // return data (the CLOB writes it), and rides the record below.
    let remainder = removed.base_asset_amount.saturating_sub(fill.base_filled);
    let remainder_order_id = if remainder > 0 && remainder >= clob.min_order_size()? {
        let order_ref = clob.place(ClobPlaceOrderArgsV0 {
            side: removed.side,
            price: removed.price,
            base_asset_amount: remainder,
            activation_delay_slots: Some(0),
            max_ts: cross.taker_origin.max_ts,
            user: taker_ref,
            taker_origin: true,
        })?;
        order_ref.order_id
    } else {
        // Gone: sub-min remainders cannot rest (the book culls its own), so the
        // taker's leftover reservation and open-order slot come off here.
        let mut taker = load_mut!(ctx.accounts.taker)?;
        let position_index = get_position_index(&taker.perp_positions, market_index)?;
        if remainder > 0 {
            decrease_open_bids_and_asks(
                &mut taker.perp_positions[position_index],
                &taker_direction,
                remainder,
                true,
            )?;
        }
        taker.perp_positions[position_index].open_orders = taker.perp_positions[position_index]
            .open_orders
            .saturating_sub(1);
        taker.decrement_open_orders(false);
        taker.release_placed_trigger_slot(market_index, removed.order_id, OrderStatus::Canceled);
        0
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
        remainder_base_asset_amount: if remainder_order_id == 0 {
            0
        } else {
            remainder
        },
        remainder_order_id,
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
