//! `crank_taker_origin_cross`: route one resting taker remainder.
//!
//! A migrated taker remainder rests on the book flagged taker-origin, at the
//! worst price its signer agreed to tolerate. The book refuses to let anyone
//! *take* it while a live counterparty crosses it, so the improvement between
//! the two prices cannot be won by landing a transaction at the activation
//! slot. Somebody still has to hand that improvement to the taker, and this is
//! that somebody: permissionless, and paid out of the improvement it delivers.
//!
//! The resolution is an ordinary fill. The remainder comes off the book, goes
//! back into `User.orders` as a limit at the price it rested at, and the router
//! fills it against everything the transaction carries — the book, the vAMM and
//! the quoters its signer chose. Because a router leg fills at its source's own
//! price and the order's limit is the resting price, the taker can only do
//! better than it was doing, and the improvement is whatever the difference
//! comes to.
//!
//! Two rules fall out of that rather than being coded:
//!
//! - **The counterparty keeps its own price.** It is an ordinary maker to an
//!   ordinary fill, and that is all "settling at the counterparty's price" ever
//!   meant.
//! - **Price-time decides between two remainders.** Lifting the aggressor
//!   uncrosses the book, so the other remainder stops being held back by the
//!   taker-origin gate and becomes depth this fill reaches at its own price.
//!   The handler only has to refuse the inverse — a subject that rested
//!   *before* the remainder it crosses — because that one is the maker of the
//!   pair and the improvement is not its to take.
//!
//! The subject is an argument, not something this crank discovers. What crosses
//! a resting remainder is usually a quote ladder rather than another book
//! order, and a book cannot report that, so there is nothing to discover from
//! `next_cross` in the general case. Finding work to stage is the resolver's
//! job: see [`stage_taker_origin_cross`].
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
    super::helpers::crank_common::ResolveClobCrank,
    crate::{
        controller::{
            self,
            position::{get_position_index, PositionDirection},
        },
        error::ErrorCode,
        instructions::{
            constraints::*,
            optional_accounts::{load_maps, AccountMaps},
            StagedCall,
        },
        load, load_mut,
        math::{
            casting::Cast,
            crosses::{resolve_crosses, Cross, CrossKind, RestingOrder},
            safe_math::SafeMath,
        },
        msg,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            events::TakerOriginCrossRecordV1,
            fill_mode::FillMode,
            oracle_map::OracleMap,
            order_params::NO_ROUTE_DIGEST,
            pdas,
            perp_market_map::{get_writable_perp_market_set, MarketSet, PerpMarketMap},
            prop_amm::{
                quoter_slab_clob, ClobFillArgsV0, ClobFillRequestV0, ClobMarket, ClobSide,
                ClobUserRefV0, Direction, WireDirectionExt,
            },
            revenue_share::RevenueShareEscrowZeroCopyMut,
            signed_msg_user::{SignedMsgUserOrdersLoader, SIGNED_MSG_PDA_SEED},
            spot_market_map::SpotMarketMap,
            state::State,
            user::{User, UserStats},
            user_map::{load_user_maps, UserMap, UserStatsMap},
        },
        validate,
    },
    anchor_lang::prelude::*,
    solana_program::sysvar::instructions::ID as IX_ID,
    std::ops::DerefMut,
};

#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct CrankTakerOriginCrossArgs {
    pub market_index: u16,
    /// How deep to read each side of the book. A short read truncates worse
    /// prices, never a better counterparty.
    pub cross_rows: u16,
    /// The taker's signed route, when the crank claims one. Empty claims the
    /// market baseline.
    pub signed_route: Vec<Pubkey>,
}

#[derive(Accounts)]
#[instruction(args: CrankTakerOriginCrossArgs)]
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
    /// The market's quoter slab; the book's config is its `Clob` slot.
    #[account(
        has_one = clob_market,
        constraint = quoter_slab.load()?.market == args.market_index,
    )]
    pub quoter_slab: AccountLoader<'info, crate::state::prop_amm::QuoterSlabV0>,
    /// CHECK: validated against the book slot's registered response account
    /// (`ClobMarket::from_slab`), so a valid slot cannot be pointed at an
    /// arbitrary account.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration; the handler re-checks through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    /// The market's relay conditions account: the wake-hint host and the
    /// lamport reservoir. Optional so a signed keeper can crank a market whose
    /// conditions were never initialized; required in program-keeper mode.
    #[account(
        mut,
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            args.market_index.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub crank_conditions: Option<AccountLoader<'info, ClobCrankConditionsV0>>,
    /// The taker's signed-message record, which carries the route its signer
    /// chose. The fill below is held to it.
    ///
    /// Required, and pinned to the taker's own authority by its seeds, even
    /// though a remainder off a directly-placed order has no such record. The
    /// two are not the same thing: an address the program derives cannot be
    /// omitted or substituted, so a caller cannot hide a route by leaving it
    /// out. A record that was never created still arrives — owned by the system
    /// program, with no data — and reads as genuinely unrouted.
    /// CHECK: read through `SignedMsgUserOrdersLoader`, which checks the owner
    /// and the discriminator before anything is read out of it.
    #[account(
        seeds = [SIGNED_MSG_PDA_SEED.as_bytes(), taker.load()?.authority.as_ref()],
        bump
    )]
    pub signed_msg_user_orders: UncheckedAccount<'info>,
    /// CHECK: address-locked. The filler obligation is measured against how
    /// many account locks the transaction holds, and this is what counts them.
    #[account(address = IX_ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,
}

/// Crosses one pass resolves. The transaction settles the one whose accounts
/// it carries; the rest are the next crank's, which is what keeps an account
/// list bounded.
const MAX_CROSSES_PER_CRANK: usize = 8;

/// The side of a cross that demanded liquidity.
fn aggressor_of(cross: &Cross, side: ClobSide) -> RestingOrder {
    match side {
        ClobSide::Bid => cross.bid,
        ClobSide::Ask => cross.ask,
    }
}

/// The other side: the one whose price the match settles at.
fn counterparty_of(cross: &Cross, aggressor_side: ClobSide) -> RestingOrder {
    match aggressor_side {
        ClobSide::Bid => cross.ask,
        ClobSide::Ask => cross.bid,
    }
}

/// Most rows one crank will read per side, whatever it was asked for.
///
/// A ceiling rather than the working depth: the caller says how deep to go,
/// because the resolver that stages this runs under simulation and can walk
/// the whole book to find out. This only stops an argument from spending the
/// crank's compute budget on a book that does not need it.
const MAX_CROSS_ROWS: u16 = 64;

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_crank_taker_origin_cross<'c: 'info, 'info>(
    ctx: Context<'info, CrankTakerOriginCross<'info>>,
    args: CrankTakerOriginCrossArgs,
) -> Result<()> {
    let CrankTakerOriginCrossArgs {
        market_index,
        cross_rows,
        signed_route,
    } = args;
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
        state.slot_clock(),
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

    let book_slot = quoter_slab_clob(&ctx.accounts.quoter_slab, market_index)?;
    validate!(
        book_slot.quotes(),
        ErrorCode::DefaultError,
        "CLOB quoter is not active and approved"
    )?;

    // ---- Resolution: read both sides, and work out what should happen.
    //
    // Nothing here is named by the caller. A book declines to resolve its own
    // crosses, and what crosses a remainder is not always another book order —
    // so velocity reads every resting row on both sides and computes the
    // matches itself. `next_cross_v0` cannot answer this: it reports the two
    // heads, which is enough only when the answer is one pair, and several
    // remainders can cross at once.
    //
    // Because the outcome is computed rather than chosen, a cranker has nothing
    // to pick and there is nothing to guard: which order aggresses, and at whose
    // price, falls out of the flags and the rest order on the rows themselves.
    let taker_ref = {
        let taker = load!(ctx.accounts.taker)?;
        taker.clob_user_ref()
    };
    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    let (bids, asks) = super::helpers::crank_common::book_l3_sides(
        &book_slot.config,
        &ctx.accounts.quoter_slab,
        market_index,
        cross_rows.min(MAX_CROSS_ROWS),
        &[
            ctx.accounts.clob_market.to_account_info(),
            ctx.accounts.clob_program.to_account_info(),
        ],
        &mut cpi_scratch,
    )?
    .ok_or(ErrorCode::NoTakerOriginCross)?;
    // Price priority decides which crank owns the front of a book, and this
    // instruction is permissionless, so the rule is enforced here and not only
    // in the resolver that stages it. When neither head demands liquidity the
    // front is a maker-against-maker cross, and taking a remainder behind it
    // would fill that remainder out of the depth the better-priced resting
    // order had priority on. Clearing the front with `crank_cross_match` is
    // what brings the remainder forward.
    let (Some(best_bid), Some(best_ask)) = (bids.first(), asks.first()) else {
        return Err(ErrorCode::NoTakerOriginCross.into());
    };
    validate!(
        best_bid.taker_origin || best_ask.taker_origin,
        ErrorCode::NoTakerOriginCross,
        "the front of the book is a maker cross; crank_cross_match resolves it first"
    )?;
    let crosses = resolve_crosses(&bids, &asks, MAX_CROSSES_PER_CRANK);

    // This crank settles the remainder the transaction carries the accounts
    // for. The rest of what the pass found is another crank's work, which is
    // what bounds one transaction's account list.
    let subject = crosses
        .iter()
        .find(|cross| {
            cross
                .kind
                .aggressor_side()
                .is_some_and(|side| aggressor_of(cross, side).user == taker_ref)
        })
        .copied()
        .ok_or(ErrorCode::NoTakerOriginCross)?;
    let aggressor_side = subject
        .kind
        .aggressor_side()
        .ok_or(ErrorCode::NoTakerOriginCross)?;
    let subject_order = aggressor_of(&subject, aggressor_side);
    drop(book_slot);

    let taker_direction = aggressor_side.to_position_direction();
    let resting_base = subject_order.base_asset_amount;
    let counterparty = counterparty_of(&subject, aggressor_side);

    // Two remainders crossing are the one case the router cannot reach.
    //
    // The book holds both back: each is taker-origin, and each is crossed by
    // the other, so the gate passes over whichever one a fill tries to take.
    // That is the gate working — the improvement between their prices belongs
    // to one of them, not to whoever lands a transaction first — and it means
    // the fill has to come from the side that knows whose it is. Velocity
    // computed that above, so it settles the pair itself and tells the book
    // after, rather than asking the book to match orders it is right to refuse.
    if counterparty.taker_origin {
        return settle_taker_origin_pair(
            &ctx,
            market_index,
            aggressor_side,
            &subject,
            &subject_order,
            &counterparty,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            &makers_and_referrer,
            &makers_and_referrer_stats,
            &state,
            program_keeper_mode,
            &clock,
        );
    }

    // ---- Fill it the way anything else fills. The order is a limit at the
    // price it rested at, so the router can only fill it at or better, and it
    // fills at each source's own price. That is how the improvement this crank
    // exists to deliver reaches the taker.
    //
    // The order is a local. It came off a book and belongs to no `orders`
    // slot, so the fill takes it directly and the taker never needs a spare
    // one — its reservation is still on it from when the remainder rested.
    let mut order =
        controller::orders::taker_origin_order(market_index, taker_direction, &subject_order);

    // The route its signer chose, if it had one.
    //
    // Absent reads as unrouted, and absent covers three real cases: a remainder
    // off a directly-placed order, whose taker has no such record at all; a
    // caller that passed the omitted-account sentinel; and the staged path,
    // which names the PDA from the taker's authority without knowing whether
    // the account was ever created. Only a record that exists, belongs to this
    // taker, and names this order carries a route.
    // Seeds pin the address, so what is left to ask is whether the record
    // exists. A taker who never sent a signed message has no account here, and
    // the runtime hands over a system-owned empty one; that reads as unrouted,
    // which for such a taker is the truth.
    let record = &ctx.accounts.signed_msg_user_orders;
    let route_digest = if record.owner == &crate::ID {
        record
            .load()?
            .route_for_clob_order(subject_order.order_ref.order_id)
            .unwrap_or(NO_ROUTE_DIGEST)
    } else {
        NO_ROUTE_DIGEST
    };

    let tail_from = ctx.remaining_accounts.len() - remaining_accounts_iter.len();
    let tail = &ctx.remaining_accounts[tail_from..];
    let route_reference_price = {
        let oracle_id = perp_market_map.get_ref(&market_index)?.oracle_id();
        oracle_map.get_price_data(&oracle_id)?.price
    };
    let direction = match taker_direction {
        PositionDirection::Long => Direction::Long,
        PositionDirection::Short => Direction::Short,
    };
    let inputs = crate::instructions::QuoteInputs {
        caps: crate::state::prop_amm::QuoterUserCapsV0::EMPTY,
        market_index,
        direction,
        size: resting_base,
        users: &crate::state::prop_amm::quoter_wire_users(
            makers_and_referrer
                .user_ref_index()?
                .into_keys()
                .map(|(authority, sub_account_id)| ClobUserRefV0 {
                    authority,
                    sub_account_id,
                }),
        )?,
        reference_price: route_reference_price,
        taker: taker_ref,
        limit_price: subject_order.price,
        // The crank vouches for measured rest, not for its own nature: on a
        // zero-delay book "rested through placement" is a zero-length
        // window, and a caller could place and crank back-to-back. The
        // subject is the flow this fill transmits, so its age is the claim.
        taker_served_window: crate::math::crosses::served_window(
            subject_order.placed_slot,
            clock.slot,
        ),
    };
    let inputs = crate::instructions::QuoteInputs {
        caps: crate::instructions::build_user_caps(
            tail,
            &inputs,
            &mut crate::instructions::CapInputs {
                makers_and_referrer: &makers_and_referrer,
                makers_and_referrer_stats: &makers_and_referrer_stats,
                perp_market_map: &perp_market_map,
                spot_market_map: &spot_market_map,
                oracle_map: &mut oracle_map,
                slot: clock.slot,
                now: clock.unix_timestamp,
            },
        )?,
        ..inputs
    };

    // Reuse the CPI scratch the book read filled: its buffers clear and refill
    // per leg, so one fill pays for one set of buffers.
    let route = crate::instructions::QuotedRoute::assemble(tail, &inputs, &mut cpi_scratch)?;
    route.require_baseline(perp_market_map.get_ref(&market_index)?.clob_market)?;
    route.require_signed_route(&signed_route, route_digest)?;
    let mut book_storage =
        [crate::math::router::QuoterBook::default(); crate::instructions::MAX_ROUTE_QUOTERS];
    let books = route.books(&mut book_storage);
    let mut executor = route.executor(&inputs, clock.slot, clock.unix_timestamp, &mut cpi_scratch);
    let mut router_inputs = crate::math::router::RouterFillInputs {
        books,
        executor: &mut executor,
        protocol_authority: state.signer,
        obligation: crate::math::router::FillerObligation {
            // The taker is not here to choose the account list, so the cranker
            // answers for what it left out, as a keeper fill does.
            taker_signed: false,
            tx_accounts: Some(
                crate::instructions::optional_accounts::tx_writable_lock_count(
                    &ctx.accounts.instructions_sysvar.to_account_info(),
                )?,
            ),
            unrouted_quoters: route.unrouted_quoters(&signed_route, route_digest),
        },
    };

    // The taker stands as its own filler, so no reward is carved out of the
    // taker fee. The cranker is paid below, out of the improvement it actually
    // delivered — a crank that improves nothing is worth nothing.
    let (base_filled, quote_filled) = controller::orders::fill_perp_order_with_router(
        // The remainder rested on the book first, so it holds an
        // `open_bids`/`open_asks` reservation this fill unwinds.
        controller::orders::FillTarget::Detached {
            order: &mut order,
            reserved: true,
        },
        &state,
        &ctx.accounts.taker,
        &ctx.accounts.taker_stats,
        &spot_market_map,
        &perp_market_map,
        &mut oracle_map,
        &ctx.accounts.taker,
        &ctx.accounts.taker_stats,
        &makers_and_referrer,
        &makers_and_referrer_stats,
        &clock,
        FillMode::Fill,
        &mut router_inputs,
        &mut None,
        false,
    )?;
    // Nothing beat the resting price. Reverting puts the remainder back where
    // it was — the cancel above is undone with it — so an unprofitable crank
    // costs the taker nothing and pays the cranker nothing.
    validate!(
        base_filled > 0,
        ErrorCode::NoTakerOriginCross,
        "no source beat the remainder's resting price"
    )?;

    // ---- What the taker gained, and the cranker's cut of it.
    //
    // The same rule as before, applied to the price the fill reached instead of
    // to one counterparty's quote: the reward is paid in full or not at all,
    // and only out of the improvement.
    let fill_price = quote_filled
        .cast::<u128>()?
        .safe_mul(crate::math::constants::BASE_PRECISION_U64.cast()?)?
        .safe_div(base_filled.cast()?)?
        .cast::<u64>()?;
    let (fee, _, _, _) = {
        let taker_stats = load!(ctx.accounts.taker_stats)?;
        controller::orders::price_taker_origin_cross(
            &state,
            market_index,
            taker_direction,
            subject_order.price,
            fill_price,
            base_filled,
            subject_order.placed_slot,
            &taker_stats,
            &perp_market_map,
            &mut oracle_map,
            &clock,
        )?
    };
    let crank_reward = controller::orders::pay_taker_origin_crank_reward(
        market_index,
        &fee,
        quote_filled,
        &ctx.accounts.taker,
        &ctx.accounts.filler,
        &ctx.accounts.filler_stats,
        &perp_market_map,
        &spot_market_map,
        &mut oracle_map,
        &clock,
    )?;

    // ---- Tell the book what the fill took. The order shrinks in place, so it
    // keeps its queue position and its id: a remainder that was partly filled
    // has not changed its mind about price, and re-placing it would send it to
    // the back of its own level.
    //
    // The reservation and the open-order slot never moved off the owner, so a
    // remainder that stays on the book costs no `User` bookkeeping. One the
    // book culls for falling under its minimum has to give them back.
    let filled = {
        let clob = ClobMarket::from_slab(
            &ctx.accounts.quoter_slab,
            market_index,
            &ctx.accounts.clob_market,
            &ctx.accounts.clob_program,
        )?;
        clob.fill(ClobFillArgsV0 {
            fills: vec![ClobFillRequestV0 {
                order_ref: subject_order.order_ref,
                base_asset_amount: base_filled,
            }],
        })?
    };
    let filled = filled
        .filled
        .first()
        .copied()
        .ok_or(ErrorCode::NoTakerOriginCross)?;
    if filled.removed {
        // The router released the slot itself if it exhausted the order, so
        // only a culled remainder still owes one. Measured off what the fill
        // returned rather than off `order`, which a detached fill does not
        // write back.
        let release_slot = base_filled < subject_order.base_asset_amount;
        let mut taker = load_mut!(ctx.accounts.taker)?;
        taker.unwind_removed_clob_order(
            market_index,
            &taker_direction,
            filled.culled_base_asset_amount,
            subject_order.order_ref.order_id,
            release_slot,
            subject_order.reduce_only,
        )?;
    }

    pay_crank_lamports(&ctx, program_keeper_mode)?;

    emit!(TakerOriginCrossRecordV1 {
        ts: clock.unix_timestamp,
        slot: clock.slot,
        market_index,
        taker: ctx.accounts.taker.key(),
        filler: ctx.accounts.filler.key(),
        base_asset_amount: base_filled,
        quote_asset_amount: quote_filled,
        rest_price: subject_order.price,
        fill_price,
        improvement: fee.improvement,
        crank_reward,
        remainder_base_asset_amount: if filled.removed {
            0
        } else {
            resting_base.saturating_sub(base_filled)
        },
        clob_order_id: subject_order.order_ref.order_id,
    });
    msg!(
        "taker-origin remainder routed: {} base at {} instead of {}, improvement {} quote, cranker paid {}",
        base_filled,
        fill_price,
        subject_order.price,
        fee.improvement,
        crank_reward
    );
    Ok(())
}

/// The keeper's lamports, as every CLOB crank pays them.
fn pay_crank_lamports<'info>(
    ctx: &Context<'info, CrankTakerOriginCross<'info>>,
    program_keeper_mode: bool,
) -> Result<()> {
    let Some(conditions_loader) = &ctx.accounts.crank_conditions else {
        return Ok(());
    };
    let payment = {
        let conditions = load_mut!(conditions_loader)?;
        u64::from(conditions.crank_payments.taker_origin_cross)
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
    Ok(())
}

/// What the taker gained and what the cranker took out of it. The per-source
/// detail rides the fill's own `OrderActionRecord`s; this is what those cannot
/// say.
#[allow(clippy::too_many_arguments)]
fn emit_taker_origin_record<'info>(
    ctx: &Context<'info, CrankTakerOriginCross<'info>>,
    market_index: u16,
    aggressor: &RestingOrder,
    base_filled: u64,
    quote_filled: u64,
    fill_price: u64,
    crank_reward: u64,
    remainder_base_asset_amount: u64,
    clock: &Clock,
) {
    let improvement = quote_filled
        .max(controller::orders::clob_notional(aggressor.price, base_filled).unwrap_or(0))
        .saturating_sub(
            quote_filled
                .min(controller::orders::clob_notional(aggressor.price, base_filled).unwrap_or(0)),
        );
    emit!(TakerOriginCrossRecordV1 {
        ts: clock.unix_timestamp,
        slot: clock.slot,
        market_index,
        taker: ctx.accounts.taker.key(),
        filler: ctx.accounts.filler.key(),
        base_asset_amount: base_filled,
        quote_asset_amount: quote_filled,
        rest_price: aggressor.price,
        fill_price,
        improvement,
        crank_reward,
        remainder_base_asset_amount,
        clob_order_id: aggressor.order_ref.order_id,
    });
    msg!(
        "taker-origin remainder routed: {} base at {} instead of {}, improvement {} quote, cranker paid {}",
        base_filled,
        fill_price,
        aggressor.price,
        improvement,
        crank_reward
    );
}

/// Settle two crossed remainders against each other, and tell the book.
///
/// No intermediary and no router. Both orders are on the book, both are frozen
/// there, and the price between them is decided already: the later of the two
/// to rest aggresses, and the earlier one's price stands. What is left is an
/// ordinary two-user match at that price, and two reductions the book applies
/// in place — so neither order loses its queue position for a fill that never
/// changed its mind about price.
///
/// The cranker is paid out of the improvement, as on every other path here, so
/// no filler is passed to the settlement and no reward comes out of the taker
/// fee.
#[allow(clippy::too_many_arguments)]
fn settle_taker_origin_pair<'c: 'info, 'info>(
    ctx: &Context<'info, CrankTakerOriginCross<'info>>,
    market_index: u16,
    aggressor_side: ClobSide,
    cross: &Cross,
    aggressor: &RestingOrder,
    counterparty: &RestingOrder,
    perp_market_map: &PerpMarketMap<'info>,
    spot_market_map: &SpotMarketMap<'info>,
    oracle_map: &mut OracleMap<'info>,
    makers_and_referrer: &UserMap<'info>,
    makers_and_referrer_stats: &UserStatsMap<'info>,
    state: &State,
    program_keeper_mode: bool,
    clock: &Clock,
) -> Result<()> {
    let taker_direction = aggressor_side.to_position_direction();
    let base_filled = cross.base_asset_amount;
    // The earlier order's price, which is what the resolution settled on.
    let price = counterparty.price;
    let quote_filled = controller::orders::clob_notional(price, base_filled)?;
    let maker_key = *makers_and_referrer
        .user_ref_index()?
        .get(&(
            counterparty.user.authority,
            counterparty.user.sub_account_id,
        ))
        .ok_or_else(|| {
            msg!(
                "counterparty {}/{} is not loaded",
                counterparty.user.authority,
                counterparty.user.sub_account_id
            );
            ErrorCode::UserNotFound
        })?;

    let mut order =
        controller::orders::taker_origin_order(market_index, taker_direction, aggressor);
    let (oracle_price, margin_ratio_initial) = {
        let market = perp_market_map.get_ref(&market_index)?;
        let oracle_id = market.oracle_id();
        let margin_ratio_initial = market.margin_ratio_initial;
        drop(market);
        (
            oracle_map.get_price_data(&oracle_id)?.price,
            margin_ratio_initial,
        )
    };

    // Both the price and the size come off the book's own rows, and this path
    // has no quote leg to hold them against — velocity priced the match itself.
    // The oracle is the outside anchor, applied the way the router fill and the
    // cross crank apply it to an external leg: the counterparty is a resting
    // maker at `price`, so a price that far from oracle would move value onto
    // it that no quote ever offered.
    validate!(
        !crate::math::orders::limit_price_breaches_maker_oracle_price_bands(
            price,
            taker_direction.opposite(),
            oracle_price,
            margin_ratio_initial,
        )?,
        ErrorCode::QuoterFillOffQuote,
        "the book rested the counterparty at {}, outside the oracle band around {}",
        price,
        oracle_price
    )?;
    // The aggressor's own side. `settle_external_match_fill` holds the
    // counterparty's leg to its reservation, but the aggressor's leg is
    // velocity's own order row and would clamp instead, so the size the book
    // reported is bound here.
    {
        let taker = load!(ctx.accounts.taker)?;
        let position_index = get_position_index(&taker.perp_positions, market_index)?;
        let reserved = taker.perp_positions[position_index].reserved_open_base(taker_direction);
        // A reduce-only aggressor fills only up to the position it reduces. The
        // book must clamp it to the same cover, but bind it here too, so a
        // misbehaving book cannot grow a position a reduce-only order shrinks.
        // A long aggressor reduces a short, and a short aggressor reduces a
        // long, so the cover is the position held the opposite way.
        let cover = if order.reduce_only {
            let base = taker.perp_positions[position_index].base_asset_amount;
            match taker_direction {
                PositionDirection::Long => base.min(0).unsigned_abs(),
                PositionDirection::Short => base.max(0).unsigned_abs(),
            }
        } else {
            u64::MAX
        };
        let bound = reserved.min(cover);
        validate!(
            base_filled <= bound,
            ErrorCode::QuoterReportExceedsReservation,
            "the book reported a {} base cross for a taker bound to {}",
            base_filled,
            bound
        )?;
    }
    // The counterparty's leg. `settle_external_match_fill` binds it to its
    // reservation, but a reduce-only counterparty must also stay within the
    // position it reduces. This cross settles both legs itself, so no book
    // clamp stands behind it: the bind here is the whole guard. A long
    // counterparty rests bids and reduces a short; a short counterparty rests
    // asks and reduces a long, so the cover is the position held the other way.
    if counterparty.reduce_only {
        let counterparty_user = makers_and_referrer.get_ref(&maker_key)?;
        let cp_index = get_position_index(&counterparty_user.perp_positions, market_index)?;
        let base = counterparty_user.perp_positions[cp_index].base_asset_amount;
        let cp_cover = match taker_direction.opposite() {
            PositionDirection::Long => base.min(0).unsigned_abs(),
            PositionDirection::Short => base.max(0).unsigned_abs(),
        };
        validate!(
            base_filled <= cp_cover,
            ErrorCode::QuoterReportExceedsReservation,
            "the cross filled {} base against a reduce-only counterparty covering {}",
            base_filled,
            cp_cover
        )?;
    }

    // Settle funding for both parties before any position update.
    // `update_position_and_market` requires each position's
    // `last_cumulative_funding_rate` to match the market's rate. A party that
    // last traded before a funding update fails that invariant. The router and
    // `cross_match` paths pre-settle the same way. This two-remainder path must
    // match them, or it reverts whenever either party holds a position.
    {
        let now = clock.unix_timestamp;
        let taker_key = ctx.accounts.taker.key();
        let mut market = perp_market_map.get_ref_mut(&market_index)?;
        let mut taker = load_mut!(ctx.accounts.taker)?;
        crate::controller::funding::settle_funding_payment(
            &mut taker,
            &taker_key,
            &mut market,
            now,
        )?;
        let mut counterparty_user = makers_and_referrer.get_ref_mut(&maker_key)?;
        crate::controller::funding::settle_funding_payment(
            &mut counterparty_user,
            &maker_key,
            &mut market,
            now,
        )?;
    }

    {
        let mut market = perp_market_map.get_ref_mut(&market_index)?;
        let mut taker = load_mut!(ctx.accounts.taker)?;
        let mut taker_stats = load_mut!(ctx.accounts.taker_stats)?;
        let taker_position_index = get_position_index(&taker.perp_positions, market_index)?;
        let taker_existing_position_params = taker.perp_positions[taker_position_index]
            .get_existing_position_params_for_order_action(taker_direction);
        let mut maker = makers_and_referrer.get_ref_mut(&maker_key)?;
        let mut maker_stats = Some(makers_and_referrer_stats.get_ref_mut(&maker.authority)?);
        let taker_key = ctx.accounts.taker.key();
        let mut none_filler: Option<&mut User> = None;
        let mut none_filler_stats: Option<&mut UserStats> = None;
        let mut no_escrow: Option<&mut RevenueShareEscrowZeroCopyMut> = None;
        let mut filler_reward_paid = 0u64;
        controller::orders::settle_external_match_fill(
            base_filled,
            quote_filled,
            market.deref_mut(),
            &mut taker,
            &mut taker_stats,
            taker_position_index,
            &mut order,
            &taker_key,
            taker_direction,
            taker_existing_position_params,
            &mut maker,
            maker_stats.as_deref_mut(),
            &maker_key,
            // A CLOB order's worst case is reserved through velocity at
            // placement, so its fill unwinds that reservation.
            true,
            Some(counterparty.order_ref.order_id as u32),
            Some(aggressor.price),
            oracle_price,
            &mut none_filler,
            &mut none_filler_stats,
            &taker_key,
            &mut no_escrow,
            false,
            &state.perp_fee_structure,
            oracle_map,
            false,
            clock.unix_timestamp,
            clock.slot,
            state.promo_fee_tier,
            false,
            // The aggressor rested on the book first, so its reservation is
            // still on it and this fill unwinds it.
            true,
            &mut filler_reward_paid,
        )?;
    }

    // One call for both sides. Each order shrinks in place, and the book culls
    // whichever leftover falls under its minimum.
    let filled = {
        let clob = ClobMarket::from_slab(
            &ctx.accounts.quoter_slab,
            market_index,
            &ctx.accounts.clob_market,
            &ctx.accounts.clob_program,
        )?;
        clob.fill(ClobFillArgsV0 {
            fills: vec![
                ClobFillRequestV0 {
                    order_ref: aggressor.order_ref,
                    base_asset_amount: base_filled,
                },
                ClobFillRequestV0 {
                    order_ref: counterparty.order_ref,
                    base_asset_amount: base_filled,
                },
            ],
        })?
    };

    // A leg the book no longer holds gives its owner back the open-order slot
    // and whatever the cull dropped.
    for (leg, owner_is_taker) in filled.filled.iter().zip([true, false]) {
        if !leg.removed {
            continue;
        }
        let direction = if owner_is_taker {
            taker_direction
        } else {
            taker_direction.opposite()
        };
        if owner_is_taker {
            let mut taker = load_mut!(ctx.accounts.taker)?;
            taker.unwind_removed_clob_order(
                market_index,
                &direction,
                leg.culled_base_asset_amount,
                leg.order_id,
                true,
                aggressor.reduce_only,
            )?;
        } else {
            let mut maker = makers_and_referrer.get_ref_mut(&maker_key)?;
            maker.unwind_removed_clob_order(
                market_index,
                &direction,
                leg.culled_base_asset_amount,
                leg.order_id,
                true,
                counterparty.reduce_only,
            )?;
        }
    }

    // ---- What the taker gained, and the cranker's cut of it. The same rule
    // the routed path uses: paid out of the improvement, in full or not at all,
    // so a crank that improves nothing is worth nothing.
    let (fee, _, _, _) = {
        let taker_stats = load!(ctx.accounts.taker_stats)?;
        controller::orders::price_taker_origin_cross(
            state,
            market_index,
            taker_direction,
            aggressor.price,
            price,
            base_filled,
            aggressor.placed_slot,
            &taker_stats,
            perp_market_map,
            oracle_map,
            clock,
        )?
    };
    let crank_reward = controller::orders::pay_taker_origin_crank_reward(
        market_index,
        &fee,
        quote_filled,
        &ctx.accounts.taker,
        &ctx.accounts.filler,
        &ctx.accounts.filler_stats,
        perp_market_map,
        spot_market_map,
        oracle_map,
        clock,
    )?;

    pay_crank_lamports(ctx, program_keeper_mode)?;
    emit_taker_origin_record(
        ctx,
        market_index,
        aggressor,
        base_filled,
        quote_filled,
        price,
        crank_reward,
        aggressor.base_asset_amount.saturating_sub(base_filled),
        clock,
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
) -> Result<Option<StagedCall>> {
    let (market_index, oracle, quote_spot_market_index) = {
        let conditions = ctx.accounts.crank_conditions.load()?;
        (
            conditions.market_index,
            conditions.oracle,
            conditions.quote_spot_market_index,
        )
    };
    let book_slot = quoter_slab_clob(&ctx.accounts.quoter_slab, market_index)?;
    if !book_slot.quotes() {
        // The crank refuses a killed or unvetted book, so there is no work to
        // stage against one; reclaiming orders left on a dead book is the
        // eviction and force-cancel paths'.
        return Ok(None);
    }

    // The resolver runs under simulation, so it reads the whole window the
    // crank could ever be asked for and hands back the depth that matters.
    let book_accounts = [
        ctx.accounts.clob_market.to_account_info(),
        ctx.accounts.clob_program.to_account_info(),
    ];
    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    let (bids, asks) = super::helpers::crank_common::book_l3_sides(
        &book_slot.config,
        &ctx.accounts.quoter_slab,
        market_index,
        MAX_CROSS_ROWS,
        &book_accounts,
        &mut cpi_scratch,
    )?
    .ok_or(ErrorCode::NoTakerOriginCross)?;

    // Price priority decides which crank owns the front of a book. A
    // maker-against-maker cross ahead of a remainder is `crank_cross_match`'s
    // work, and clearing it is what brings the remainder to the front — so the
    // two cranks compose instead of racing for the same book.
    let (Some(best_bid), Some(best_ask)) = (bids.first(), asks.first()) else {
        return Ok(None);
    };
    if !best_bid.taker_origin && !best_ask.taker_origin {
        return Ok(None);
    }
    let crosses = resolve_crosses(&bids, &asks, MAX_CROSSES_PER_CRANK);
    let Some(cross) = crosses
        .iter()
        .find(|cross| cross.kind != CrossKind::ProtocolMiddles)
    else {
        return Ok(None);
    };
    let Some(side) = cross.kind.aggressor_side() else {
        return Ok(None);
    };

    let (protocol_user, protocol_user_stats) = pdas::protocol_user_pair();
    let taker_ref = aggressor_of(cross, side).user;
    let counterparty_ref = counterparty_of(cross, side).user;
    let (taker, taker_stats) = pdas::user_pair(&taker_ref.authority, taker_ref.sub_account_id);
    // The protocol `User` is the filler on this path, and the crank loads each
    // margin account exactly once, so it cannot also be a side of the cross it
    // resolves.
    if taker == protocol_user
        || pdas::user(&counterparty_ref.authority, counterparty_ref.sub_account_id) == protocol_user
    {
        return Ok(None);
    }

    // How deep the crank must read to see the cross this resolver picked.
    // Rows come back best-first, so the deeper of the pair's two positions is
    // the whole window the crank needs.
    let depth = |rows: &[RestingOrder], target: &crate::math::crosses::RestingOrder| {
        rows.iter()
            .position(|row| row.order_ref == target.order_ref)
            .unwrap_or(0) as u16
    };
    let cross_rows = depth(&bids, &cross.bid)
        .max(depth(&asks, &cross.ask))
        .saturating_add(1)
        .min(MAX_CROSS_ROWS);

    Ok(Some(
        crate::staged_call!(CrankTakerOriginCross {
            state: ctx.accounts.state.key(),
            authority: pdas::keeper_placeholder(),
            filler: protocol_user,
            filler_stats: protocol_user_stats,
            taker,
            taker_stats,
            quoter_slab: ctx.accounts.quoter_slab.key(),
            clob_market: ctx.accounts.clob_market.key(),
            clob_program: crate::ids::clob_program::id(),
            crank_conditions: Some(ctx.accounts.crank_conditions.key()),
            // The route the taker signed rides its own signed-message record,
            // derived from the authority the book stores on the node.
            signed_msg_user_orders: pdas::signed_msg_user_orders(&taker_ref.authority),
            instructions_sysvar: IX_ID,
        })
        // Both `(User, UserStats)` pairs derive from the nodes' own
        // `(authority, sub_account_id)`, which is what the book stores them for.
        .map_section(oracle, quote_spot_market_index, market_index)
        .maker_refs([counterparty_ref])
        // The quoter tail. The crank routes the remainder like any other fill,
        // and every router fill must carry the market's slab and consult its
        // book. A taker-origin order rests on the CLOB and nowhere else, so
        // the book this resolver reads is that baseline.
        .account(ctx.accounts.quoter_slab.key(), false)
        .account(ctx.accounts.clob_market.key(), true)
        .account(crate::ids::clob_program::id(), false)
        // The resolver stages no quoters of its own, so it claims no route
        // (an empty signed route). A staged crank routes through the market's
        // baseline — the CLOB and the vAMM — which every fill carries anyway.
        // A keeper that wants a taker's custom quoters consulted builds the
        // call itself and claims the route the taker signed.
        .arg(CrankTakerOriginCrossArgs {
            market_index,
            cross_rows,
            signed_route: Vec::new(),
        })?,
    ))
}
