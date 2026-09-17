//! `crank_taker_origin_cross`: route one resting taker remainder.
//!
//! A migrated taker remainder rests on the book with the taker-origin flag. It
//! rests at the worst price its signer agreed to tolerate. The book refuses to
//! let anyone take it while a live counterparty crosses it, so the improvement
//! between the two prices cannot be won by landing a transaction at the
//! activation slot. This crank hands that improvement to the taker. It is
//! permissionless and is paid out of the improvement it delivers.
//!
//! The resolution is an ordinary fill. The remainder becomes a detached limit
//! order at the price it rested at. The router fills it against everything the
//! transaction carries: the book, the vAMM and the quoters its signer chose. A
//! router leg fills at its source's own price, so the taker can only do better
//! than the resting price. The improvement is the difference. The book learns
//! the filled size afterwards and the row shrinks in place.
//!
//! Two rules follow from that shape instead of being coded:
//!
//! - The counterparty keeps its own price. It is an ordinary maker to an
//!   ordinary fill.
//! - Price and time decide between two remainders. Lifting the aggressor
//!   uncrosses the book, so the other remainder stops being held back by the
//!   taker-origin gate and becomes depth this fill reaches at its own price.
//!   The handler refuses only the inverse case. A subject that rested before
//!   the remainder it crosses is the maker of the pair, and the improvement is
//!   not its to take.
//!
//! The subject is an argument, not something this crank discovers. What crosses
//! a resting remainder is usually a quote ladder rather than another book
//! order, and a book cannot report that. Finding work to stage is the
//! resolver's job. See [`stage_taker_origin_cross`], which the cross
//! conditions' resolver reaches. This crank has no condition of its own.
//!
//! `crank_cross_match` middles two crossed makers for the protocol. This crank
//! does not. One side is the aggressor by construction, the improvement belongs
//! to it, and the only cut anyone takes is the cranker's reward.

use {
    super::helpers::crank_common::ResolveClobCrank,
    crate::{
        controller::{
            self,
            position::{get_position_index, PositionDirection},
        },
        error::{ErrorCode, VelocityResult},
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
            order_params::NO_ROUTE_DIGEST,
            pdas,
            perp_market_map::{get_writable_perp_market_set, MarketSet, PerpMarketMap},
            prop_amm::{
                ClobFillArgsV0, ClobFillRequestV0, ClobMarket, ClobSide, ClobUserRefV0, Direction,
                QuoterSlabExt, WireDirectionExt,
            },
            revenue_share::RevenueShareEscrowZeroCopyMut,
            signed_msg_user::{SignedMsgUserOrdersLoader, SIGNED_MSG_PDA_SEED},
            state::State,
            user::{User, UserStats},
            user_map::{load_user_maps, UserMap, UserStatsMap},
        },
        validate,
    },
    anchor_lang::prelude::*,
    solana_program::sysvar::instructions::ID as IX_ID,
    std::{collections::BTreeMap, ops::DerefMut},
};

#[cfg(test)]
mod tests;

#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct CrankTakerOriginCrossArgs {
    pub market_index: u16,
    /// How deep to read each side of the book. A short read truncates worse
    /// prices, never a better counterparty.
    pub cross_rows: u16,
    /// The taker's signed route, when the crank claims one. An empty vector
    /// claims the market baseline.
    pub signed_route: Vec<Pubkey>,
}

#[derive(Accounts)]
#[instruction(args: CrankTakerOriginCrossArgs)]
pub struct CrankTakerOriginCross<'info> {
    pub state: AccountLoader<'info, State>,
    /// CHECK: in signed-keeper mode this must sign for `filler`, which the
    /// constraint below enforces. In program-keeper mode it is only the lamport
    /// payout target, relay's keeper-placeholder slot, and no signature is
    /// required.
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
    /// Owner of the taker-origin order, and the taker of this match. Verified
    /// against the identity the CLOB reports on removal, so a wrong account
    /// fails the crank rather than settling against someone else.
    #[account(mut)]
    pub taker: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&taker, &taker_stats)?
    )]
    pub taker_stats: AccountLoader<'info, UserStats>,
    /// The market's quoter slab. The book's config is its `Clob` slot.
    #[account(
        has_one = clob_market,
        constraint = quoter_slab.load()?.market == args.market_index,
    )]
    pub quoter_slab: AccountLoader<'info, crate::state::prop_amm::QuoterSlabV0>,
    /// CHECK: `ClobMarket::from_slab` validates this against the book slot's
    /// registered response account, so a valid slot cannot be pointed at an
    /// arbitrary account.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration. The handler checks it again through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    /// The market's relay conditions account: the wake-hint host and the
    /// lamport reservoir. It is optional so that a signed keeper can crank a
    /// market whose conditions were never initialized. Program-keeper mode
    /// requires it.
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
    /// The account is required and its seeds pin it to the taker's own
    /// authority, even though a remainder off a directly-placed order has no
    /// such record. An address the program derives cannot be omitted or
    /// substituted, so a caller cannot hide a route by leaving it out. A record
    /// that was never created still arrives. It is owned by the system program
    /// and holds no data, and it reads as unrouted.
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

/// Crosses one pass resolves. The transaction settles the one whose accounts it
/// carries. The rest are the next crank's work, which bounds the account list.
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

/// Most rows one crank reads per side, whatever it was asked for.
///
/// This is a ceiling, not the working depth. The caller says how deep to go,
/// because the resolver that stages this runs under simulation and can walk the
/// whole book. The ceiling stops an argument from spending the crank's compute
/// budget on a book that does not need it.
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
    // The settlement loads three margin accounts mutably at once. An overlap
    // fails on the borrow, which reports nothing about the cause.
    validate!(
        ctx.accounts.filler.key() != ctx.accounts.taker.key(),
        ErrorCode::DefaultError,
        "the cranker cannot be the taker of the cross it resolves"
    )?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
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

    let book_slot = ctx.accounts.quoter_slab.clob_slot(market_index)?;
    validate!(
        book_slot.quotes(),
        ErrorCode::DefaultError,
        "CLOB quoter is not active and approved"
    )?;

    let taker_ref = {
        let taker = load!(ctx.accounts.taker)?;
        taker.clob_user_ref()
    };
    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    let SubjectCross {
        aggressor_side,
        cross: subject,
        order: subject_order,
        counterparty,
    } = resolve_subject_cross(
        &ctx,
        &book_slot.config,
        market_index,
        cross_rows,
        taker_ref,
        &mut cpi_scratch,
    )?;
    drop(book_slot);

    let taker_direction = aggressor_side.to_position_direction();
    let cx = TakerOriginContext {
        accounts: &*ctx.accounts,
        market_index,
        taker_ref,
        taker_direction,
        state: &state,
        makers_and_referrer: &makers_and_referrer,
        makers_and_referrer_stats: &makers_and_referrer_stats,
        clock: &clock,
        program_keeper_mode,
    };

    // Two crossed remainders are the one case the router cannot reach. The
    // book holds both back, because each is taker-origin and each is crossed by
    // the other, so the gate passes over whichever one a fill tries to take.
    // The improvement between their prices belongs to one of them, and the
    // resolution above worked out which. Velocity settles the pair itself and
    // tells the book after.
    if counterparty.taker_origin {
        return settle_taker_origin_pair(&cx, &subject, &subject_order, &counterparty, &mut maps);
    }

    let route_claim = SignedRouteClaim {
        quoters: &signed_route,
        digest: signed_route_digest(
            &ctx.accounts.signed_msg_user_orders,
            subject_order.order_ref.order_id,
        )?,
    };

    let tail_from = ctx.remaining_accounts.len() - remaining_accounts_iter.len();
    let tail = &ctx.remaining_accounts[tail_from..];
    let controller::orders::FillAmounts {
        base: base_filled,
        quote: quote_filled,
    } = route_and_fill_remainder(
        &cx,
        tail,
        &subject_order,
        &route_claim,
        &mut maps,
        &mut cpi_scratch,
    )?;

    let fill_price = quote_filled
        .cast::<u128>()?
        .safe_mul(crate::math::constants::BASE_PRECISION_U64.cast()?)?
        .safe_div(base_filled.cast()?)?
        .cast::<u64>()?;
    // The router fill applies the oracle gates and the shared post-fill checks
    // itself, so this branch only prices the cross. It is measured against the
    // price the fill reached, which is known only after the fill.
    let (fee, _, _) = price_cross(&cx, &subject_order, fill_price, base_filled, &mut maps)?;
    let crank_reward = pay_crank_reward(&cx, &fee, quote_filled, &mut maps)?;

    let remainder_base_asset_amount = report_fill_to_book(&cx, &subject_order, base_filled)?;

    pay_crank_lamports(&cx)?;

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
        remainder_base_asset_amount,
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

/// What every step of one crank shares.
///
/// The handler builds this once, after the book read decides which row
/// aggresses. Every field is fixed for the whole crank. The three market maps
/// stay separate parameters, the way every other instruction in this program
/// passes them.
struct TakerOriginContext<'a, 'info> {
    accounts: &'a CrankTakerOriginCross<'info>,
    market_index: u16,
    /// The taker's identity as the book reports it on its own rows.
    taker_ref: ClobUserRefV0,
    /// The side the taker's remainder demanded liquidity on.
    taker_direction: PositionDirection,
    state: &'a State,
    makers_and_referrer: &'a UserMap<'info>,
    makers_and_referrer_stats: &'a UserStatsMap<'info>,
    clock: &'a Clock,
    /// True when the protocol `User` cranks. The reservoir then pays the
    /// keeper its lamports.
    program_keeper_mode: bool,
}

/// The cross this crank settles, and its two rows.
struct SubjectCross {
    /// The side that demanded liquidity.
    aggressor_side: ClobSide,
    /// The matched pair, which carries the size the two rows share.
    cross: Cross,
    /// The taker's own resting remainder.
    order: RestingOrder,
    /// The row that crosses the remainder.
    counterparty: RestingOrder,
}

/// Read both sides of the book and work out which cross to settle.
///
/// The caller names none of this. A book declines to resolve its own crosses,
/// and what crosses a remainder is not always another book order. Velocity
/// reads every resting row on both sides and computes the matches itself.
/// `next_cross_v0` cannot answer this, because it reports the two heads, and
/// several remainders can cross at once.
///
/// The outcome is computed, not chosen, so a cranker has nothing to pick. Which
/// order aggresses, and at whose price, follows from the flags and the rest
/// order on the rows themselves.
fn resolve_subject_cross<'info>(
    ctx: &Context<'info, CrankTakerOriginCross<'info>>,
    book_config: &crate::state::prop_amm::QuoterConfigV0,
    market_index: u16,
    cross_rows: u16,
    taker_ref: ClobUserRefV0,
    cpi_scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
) -> Result<SubjectCross> {
    let (bids, asks) = super::helpers::crank_common::book_l3_sides(
        book_config,
        &ctx.accounts.quoter_slab,
        market_index,
        cross_rows.min(MAX_CROSS_ROWS),
        &[
            ctx.accounts.clob_market.to_account_info(),
            ctx.accounts.clob_program.to_account_info(),
        ],
        cpi_scratch,
        true,
    )?
    .ok_or(ErrorCode::NoTakerOriginCross)?;
    // Price priority decides which crank owns the front of a book. This
    // instruction is permissionless, so the rule is enforced here as well as in
    // the resolver that stages it. When neither head demands liquidity, the
    // front is a maker-against-maker cross. Taking a remainder behind it would
    // fill that remainder out of depth the better-priced resting order had
    // priority on. `crank_cross_match` clears the front and brings the
    // remainder forward.
    let (Some(best_bid), Some(best_ask)) = (bids.first(), asks.first()) else {
        return Err(ErrorCode::NoTakerOriginCross.into());
    };
    validate!(
        best_bid.taker_origin || best_ask.taker_origin,
        ErrorCode::NoTakerOriginCross,
        "the front of the book is a maker cross; crank_cross_match resolves it first"
    )?;
    let crosses = resolve_crosses(&bids, &asks, MAX_CROSSES_PER_CRANK);

    // This crank settles the remainder whose accounts the transaction carries.
    // The rest of the pass is another crank's work, which bounds one
    // transaction's account list.
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

    Ok(SubjectCross {
        aggressor_side,
        cross: subject,
        order: subject_order,
        counterparty: counterparty_of(&subject, aggressor_side),
    })
}

/// The route claim the crank makes on the taker's behalf.
///
/// The two halves are always read together. A claim is good only when the
/// quoters the crank names agree with the digest the taker's record holds.
struct SignedRouteClaim<'a> {
    /// The quoters the crank claims the taker's signer chose. An empty slice
    /// claims the market baseline.
    quoters: &'a [Pubkey],
    /// The digest the taker's signed-message record holds for this order.
    digest: crate::state::order_params::RouteDigest,
}

/// The route the taker's signer chose, if it had one.
///
/// An absent record reads as unrouted, and three real cases produce one. A
/// remainder off a directly-placed order has no such record. A caller can pass
/// the omitted-account sentinel. The staged path names the PDA from the taker's
/// authority without knowing whether the account was ever created. Only a
/// record that exists, belongs to this taker, and names this order carries a
/// route. Seeds pin the address, so the one open question is whether the record
/// exists. A taker who never sent a signed message has no account here, and the
/// runtime hands over a system-owned empty one.
fn signed_route_digest(
    record: &UncheckedAccount<'_>,
    clob_order_id: u64,
) -> Result<crate::state::order_params::RouteDigest> {
    let digest = if record.owner == &crate::ID {
        record
            .load()?
            .route_for_clob_order(clob_order_id)
            .unwrap_or(NO_ROUTE_DIGEST)
    } else {
        NO_ROUTE_DIGEST
    };
    Ok(digest)
}

/// Fill the remainder the way anything else fills.
///
/// The order is a limit at the price it rested at, so the router can only fill
/// it at that price or better. Each leg fills at its own source's price, which
/// is how the improvement reaches the taker.
///
/// Returns the base and the quote the fill took.
#[allow(clippy::too_many_arguments)]
fn route_and_fill_remainder<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    tail: &'info [AccountInfo<'info>],
    subject_order: &RestingOrder,
    route_claim: &SignedRouteClaim<'_>,
    maps: &mut AccountMaps<'info>,
    cpi_scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
) -> Result<controller::orders::FillAmounts> {
    // The order is a local. It came off a book and belongs to no `orders` slot,
    // so the fill takes it directly and the taker needs no spare slot. The
    // reservation from when the remainder rested is still on the position.
    let mut order =
        controller::orders::taker_origin_order(cx.market_index, cx.taker_direction, subject_order);

    let (route_reference_price, route_margin_ratio_initial) = {
        let market = maps.perp_market_map.get_ref(&cx.market_index)?;
        let oracle_id = market.oracle_id();
        let margin_ratio_initial = market.margin_ratio_initial;
        drop(market);
        (
            maps.oracle_map.get_price_data(&oracle_id)?.price,
            margin_ratio_initial,
        )
    };
    let direction = match cx.taker_direction {
        PositionDirection::Long => Direction::Long,
        PositionDirection::Short => Direction::Short,
    };
    // Reuse the CPI scratch the book read filled. Its buffers clear and refill
    // per leg, so one fill pays for one set of buffers.
    let users = crate::state::prop_amm::quoter_wire_users(
        cx.makers_and_referrer
            .user_ref_index()?
            .into_keys()
            .map(|(authority, sub_account_id)| ClobUserRefV0 {
                authority,
                sub_account_id,
            }),
    )?;
    let quoted = crate::instructions::quote_route(
        tail,
        crate::instructions::QuoteInputs {
            market_index: cx.market_index,
            direction,
            size: subject_order.base_asset_amount,
            users: &users,
            reference_price: route_reference_price,
            taker: cx.taker_ref,
            limit_price: subject_order.price,
            // The window is measured from the subject's own rest, not assumed
            // from the crank. On a zero-delay book, rest through placement is a
            // zero-length window, and a caller can place and crank in the same
            // slot. The subject is the flow this fill transmits, so its age is
            // the claim.
            taker_served_window: crate::math::crosses::served_window(
                subject_order.placed_slot,
                cx.clock.slot,
            ),
            consume_reservation: true,
            margin_ratio_initial: route_margin_ratio_initial,
        },
        Some(crate::instructions::RouteClaim {
            quoters: route_claim.quoters,
            digest: route_claim.digest,
        }),
        &mut crate::instructions::CapInputs {
            taker_key: &cx.accounts.taker.key(),
            makers_and_referrer: cx.makers_and_referrer,
            makers_and_referrer_stats: cx.makers_and_referrer_stats,
            maps,
            slot: cx.clock.slot,
            now: cx.clock.unix_timestamp,
        },
        cpi_scratch,
    )?;
    let obligation = crate::math::router::FillerObligation {
        // The taker is not here to choose the account list, so the cranker
        // answers for what it left out, as a keeper fill does.
        taker_signed: false,
        tx_accounts: Some(
            crate::instructions::optional_accounts::tx_writable_lock_count(
                &cx.accounts.instructions_sysvar.to_account_info(),
            )?,
        ),
        unrouted_quoters: quoted.unrouted_quoters,
    };

    // The taker is its own filler, so no reward comes out of the taker fee. The
    // cranker is paid below, out of the improvement it delivered. A crank that
    // improves nothing earns nothing.
    let mut books = quoted.books(cx.clock, cpi_scratch)?;
    let mut router = books.for_fill(crate::instructions::FillerStanding {
        protocol_authority: cx.state.signer,
        taker_exposure_closed_by_caller: false,
        obligation,
    });
    let filled = controller::orders::fill_perp_order(
        controller::orders::FillRequest {
            // The remainder rested on the book first, so it holds an
            // `open_bids`/`open_asks` reservation this fill unwinds.
            order: &mut order,
            // The remainder rested on the book first, so it holds a
            // reservation the fill must unwind as it fills.
            reserved: true,
            mode: FillMode::Fill,
            referrer_is_accelerated: false,
        },
        cx.state,
        cx.clock,
        controller::orders::PerpFillAccounts {
            user: &cx.accounts.taker,
            user_stats: &cx.accounts.taker_stats,
            filler: &cx.accounts.taker,
            filler_stats: &cx.accounts.taker_stats,
            rev_share_escrow: &mut None,
        },
        &mut controller::orders::FillParties {
            maps,
            makers_and_referrer: cx.makers_and_referrer,
            makers_and_referrer_stats: cx.makers_and_referrer_stats,
        },
        &mut router,
    )?;
    // Nothing beat the resting price. The revert leaves the remainder resting
    // as it was, so an unprofitable crank costs the taker nothing and pays the
    // cranker nothing.
    validate!(
        filled.base > 0,
        ErrorCode::NoTakerOriginCross,
        "no source beat the remainder's resting price"
    )?;
    Ok(filled)
}

/// The oracle pre-flight, and what the taker gained.
///
/// A caller that settles the match itself must run this before it touches the
/// book. The pre-flight refuses a market in settlement, paused fills, an invalid
/// oracle, and a price outside the band. A refusal must leave the book as it
/// was.
///
/// Returns the fee split, whether the oracle is stale for margin, and the
/// market's open interest before the fill. The last two are inputs to the
/// post-fill checks. The pre-flight's own mm-oracle price is dropped. The match
/// and the maker band use the plain oracle price, as the router pass does.
fn price_cross<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    rested: &RestingOrder,
    fill_price: u64,
    base_filled: u64,
    maps: &mut AccountMaps,
) -> Result<(crate::math::fees::TakerOriginCrossFee, bool, u128)> {
    let taker_stats = load!(cx.accounts.taker_stats)?;
    let (fee, _, oracle_stale_for_margin, perp_market_oi_before) =
        controller::orders::price_taker_origin_cross(
            cx.state,
            cx.market_index,
            cx.taker_direction,
            rested.price,
            fill_price,
            base_filled,
            rested.placed_slot,
            &taker_stats,
            &maps.perp_market_map,
            &mut maps.oracle_map,
            cx.clock,
        )?;
    Ok((fee, oracle_stale_for_margin, perp_market_oi_before))
}

/// The cranker's cut of what the taker gained.
///
/// The reward is paid in full or not at all, and only out of the improvement,
/// so a crank that improves nothing earns nothing. The improvement is measured
/// against the price the fill reached, not against one counterparty's quote.
fn pay_crank_reward<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    fee: &crate::math::fees::TakerOriginCrossFee,
    quote_filled: u64,
    maps: &mut AccountMaps,
) -> Result<u64> {
    Ok(controller::orders::pay_taker_origin_crank_reward(
        cx.market_index,
        fee,
        quote_filled,
        &cx.accounts.taker,
        &cx.accounts.filler,
        &cx.accounts.filler_stats,
        maps,
        cx.clock,
    )?)
}

/// Tell the book what the fill took.
///
/// The order shrinks in place and keeps its queue position and its id. A
/// partial fill does not change the price the remainder wants, and re-placing
/// it would send it to the back of its own level.
///
/// The reservation and the open-order slot never move off the owner, so a
/// remainder that stays on the book costs no `User` bookkeeping. A remainder
/// the book culls for falling under its minimum gives them back.
///
/// Returns the base the remainder still rests at.
fn report_fill_to_book<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    subject_order: &RestingOrder,
    base_filled: u64,
) -> Result<u64> {
    let filled = {
        let clob = ClobMarket::from_slab(
            &cx.accounts.quoter_slab,
            cx.market_index,
            &cx.accounts.clob_market,
            &cx.accounts.clob_program,
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
        // only a culled remainder still owes one. This reads what the fill
        // returned, because a detached fill does not write back to `order`.
        let release_slot = base_filled < subject_order.base_asset_amount;
        let mut taker = load_mut!(cx.accounts.taker)?;
        taker.unwind_removed_clob_order(
            cx.market_index,
            &cx.taker_direction,
            filled.culled_base_asset_amount,
            subject_order.order_ref.order_id,
            release_slot,
            subject_order.reduce_only,
        )?;
        return Ok(0);
    }

    Ok(subject_order.base_asset_amount.saturating_sub(base_filled))
}

/// The keeper's lamports, as every CLOB crank pays them.
fn pay_crank_lamports<'info>(cx: &TakerOriginContext<'_, 'info>) -> Result<()> {
    let Some(conditions_loader) = &cx.accounts.crank_conditions else {
        return Ok(());
    };
    let payment = {
        let conditions = load_mut!(conditions_loader)?;
        u64::from(conditions.crank_payments.taker_origin_cross)
    };
    if cx.program_keeper_mode {
        ClobCrankConditionsV0::pay_keeper(
            conditions_loader,
            &cx.accounts.authority.to_account_info(),
            payment,
        )?;
    }
    Ok(())
}

/// What the taker gained and what the cranker took out of it. The fill's own
/// `OrderActionRecord`s carry the per-source detail.
fn emit_taker_origin_record<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    pair: &RemainderPair<'_>,
    fill_price: u64,
    crank_reward: u64,
    remainder_base_asset_amount: u64,
) {
    let (aggressor, base_filled, quote_filled) =
        (pair.aggressor, pair.base_filled, pair.quote_filled);
    let improvement = quote_filled
        .max(controller::orders::clob_notional(aggressor.price, base_filled).unwrap_or(0))
        .saturating_sub(
            quote_filled
                .min(controller::orders::clob_notional(aggressor.price, base_filled).unwrap_or(0)),
        );
    emit!(TakerOriginCrossRecordV1 {
        ts: cx.clock.unix_timestamp,
        slot: cx.clock.slot,
        market_index: cx.market_index,
        taker: cx.accounts.taker.key(),
        filler: cx.accounts.filler.key(),
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

/// Two crossed remainders, and the size and price their match settles at.
struct RemainderPair<'a> {
    /// The later of the two to rest, which aggresses.
    aggressor: &'a RestingOrder,
    /// The earlier one, whose price the match settles at.
    counterparty: &'a RestingOrder,
    /// The counterparty's margin account, as the user map keys it.
    maker_key: Pubkey,
    base_filled: u64,
    quote_filled: u64,
}

/// Settle two crossed remainders against each other, and tell the book.
///
/// There is no intermediary and no router. Both orders rest on the book and the
/// price between them is already decided. The later of the two to rest
/// aggresses, and the earlier one's price stands. What remains is an ordinary
/// two-user match at that price. The book applies both reductions in place, so
/// neither order loses its queue position.
///
/// The cranker is paid out of the improvement, as on every other path here, so
/// the settlement takes no filler and no reward comes out of the taker fee.
fn settle_taker_origin_pair<'c: 'info, 'info>(
    cx: &TakerOriginContext<'_, 'info>,
    cross: &Cross,
    aggressor: &RestingOrder,
    counterparty: &RestingOrder,
    maps: &mut AccountMaps<'info>,
) -> Result<()> {
    let taker_direction = cx.taker_direction;
    let base_filled = cross.base_asset_amount;
    // The earlier order's price, which the resolution settled on.
    let price = counterparty.price;
    let quote_filled = controller::orders::clob_notional(price, base_filled)?;
    let maker_key = *cx
        .makers_and_referrer
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
    let pair = RemainderPair {
        aggressor,
        counterparty,
        maker_key,
        base_filled,
        quote_filled,
    };

    let mut order =
        controller::orders::taker_origin_order(cx.market_index, taker_direction, aggressor);
    let (oracle_price, margin_ratio_initial) = {
        let market = maps.perp_market_map.get_ref(&cx.market_index)?;
        let oracle_id = market.oracle_id();
        let margin_ratio_initial = market.margin_ratio_initial;
        drop(market);
        (
            maps.oracle_map.get_price_data(&oracle_id)?.price,
            margin_ratio_initial,
        )
    };

    // Both the price and the size come off the book's own rows, and this path
    // has no quote leg to hold them against. The oracle is the outside anchor,
    // applied the way the router fill and the cross crank apply it to an
    // external leg. The counterparty is a resting maker at `price`, so a price
    // outside the band would move value onto it that no quote ever offered.
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
    // The pre-flight comes first, because a refusal must leave the book as it
    // was. The two flags it reports are inputs to the post-fill checks below.
    let (fee, oracle_stale_for_margin, perp_market_oi_before) =
        price_cross(cx, aggressor, price, base_filled, maps)?;

    bind_aggressor_size(
        &cx.accounts.taker,
        cx.market_index,
        taker_direction,
        base_filled,
        order.reduce_only,
    )?;
    bind_counterparty_size(cx, &pair)?;

    // The margin type the post-fill checks apply depends on the position the
    // aggressor held before the match, so both facts are read first.
    let facts = {
        let taker = load!(cx.accounts.taker)?;
        PairFillFacts {
            aggressor_order_decreasing:
                controller::orders::determine_if_user_order_is_position_decreasing(
                    &taker,
                    cx.market_index,
                    &order,
                )?,
            aggressor_is_isolated: taker
                .get_perp_position(cx.market_index)
                .map(|position| position.is_isolated())
                .unwrap_or(false),
            perp_market_oi_before,
            oracle_stale_for_margin,
        }
    };

    settle_pair_funding(cx, &pair, &maps.perp_market_map)?;

    // The settlement returns evidence that the shared post-fill checks ran on
    // both legs. Nothing outside `post_checks` can build that evidence.
    let _checked = settle_pair_match(cx, &pair, &mut order, oracle_price, &facts, maps)?;

    report_pair_fill_to_book(cx, &pair)?;

    let crank_reward = pay_crank_reward(cx, &fee, quote_filled, maps)?;

    pay_crank_lamports(cx)?;
    emit_taker_origin_record(
        cx,
        &pair,
        price,
        crank_reward,
        aggressor.base_asset_amount.saturating_sub(base_filled),
    );
    Ok(())
}

/// Bind the aggressor's leg to its reservation.
///
/// `settle_external_match_fill` holds the counterparty's leg to its
/// reservation, but the aggressor's leg is velocity's own order row and clamps
/// instead. The size the book reported is bound here.
fn bind_aggressor_size(
    taker_loader: &AccountLoader<'_, User>,
    market_index: u16,
    taker_direction: PositionDirection,
    base_filled: u64,
    reduce_only: bool,
) -> Result<()> {
    let taker = load!(taker_loader)?;
    let position_index = get_position_index(&taker.perp_positions, market_index)?;
    let reserved = taker.perp_positions[position_index].reserved_open_base(taker_direction);
    // A reduce-only aggressor fills only up to the position it reduces. The
    // book clamps it to the same cover, and the bind here repeats that, so a
    // misbehaving book cannot grow a position a reduce-only order shrinks. A
    // long aggressor reduces a short and a short aggressor reduces a long, so
    // the cover is the position held the opposite way.
    let cover = if reduce_only {
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
    Ok(())
}

/// Bind a reduce-only counterparty to the position it reduces.
///
/// `settle_external_match_fill` binds the leg to its reservation, but a
/// reduce-only counterparty must also stay within the position it reduces. This
/// cross settles both legs itself, so no book clamp stands behind it and the
/// bind here is the whole guard. A long counterparty rests bids and reduces a
/// short. A short counterparty rests asks and reduces a long. The cover is
/// therefore the position held the other way.
fn bind_counterparty_size<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    pair: &RemainderPair<'_>,
) -> Result<()> {
    if !pair.counterparty.reduce_only {
        return Ok(());
    }
    let counterparty_user = cx.makers_and_referrer.get_ref(&pair.maker_key)?;
    let cp_index = get_position_index(&counterparty_user.perp_positions, cx.market_index)?;
    let base = counterparty_user.perp_positions[cp_index].base_asset_amount;
    let cp_cover = match cx.taker_direction.opposite() {
        PositionDirection::Long => base.min(0).unsigned_abs(),
        PositionDirection::Short => base.max(0).unsigned_abs(),
    };
    validate!(
        pair.base_filled <= cp_cover,
        ErrorCode::QuoterReportExceedsReservation,
        "the cross filled {} base against a reduce-only counterparty covering {}",
        pair.base_filled,
        cp_cover
    )?;
    Ok(())
}

/// Settle funding for both parties before any position update.
///
/// `update_position_and_market` requires each position's
/// `last_cumulative_funding_rate` to match the market's rate. A party that last
/// traded before a funding update fails that invariant. The router and
/// `cross_match` paths settle funding first for the same reason. Without this
/// call, the two-remainder path reverts whenever either party holds a position.
fn settle_pair_funding<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    pair: &RemainderPair<'_>,
    perp_market_map: &PerpMarketMap<'info>,
) -> Result<()> {
    let now = cx.clock.unix_timestamp;
    let taker_key = cx.accounts.taker.key();
    let mut market = perp_market_map.get_ref_mut(&cx.market_index)?;
    let mut taker = load_mut!(cx.accounts.taker)?;
    crate::controller::funding::settle_funding_payment(&mut taker, &taker_key, &mut market, now)?;
    let mut counterparty_user = cx.makers_and_referrer.get_ref_mut(&pair.maker_key)?;
    crate::controller::funding::settle_funding_payment(
        &mut counterparty_user,
        &pair.maker_key,
        &mut market,
        now,
    )?;
    Ok(())
}

/// Move both positions with one match at the counterparty's price.
///
/// The settlement takes no filler, so no reward comes out of the taker fee. The
/// cranker is paid out of the improvement instead.
fn settle_pair_match<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    pair: &RemainderPair<'_>,
    order: &mut crate::state::user::Order,
    oracle_price: i64,
    facts: &PairFillFacts,
    maps: &mut AccountMaps<'info>,
) -> Result<post_checks::PairChecked> {
    let taker_direction = cx.taker_direction;
    // The settlement writes the market and the oracle map, and the check below
    // needs the whole bundle back, so the fields are borrowed apart.
    let AccountMaps {
        perp_market_map,
        oracle_map,
        ..
    } = &mut *maps;
    let mut market = perp_market_map.get_ref_mut(&cx.market_index)?;
    let mut taker = load_mut!(cx.accounts.taker)?;
    let mut taker_stats = load_mut!(cx.accounts.taker_stats)?;
    let taker_position_index = get_position_index(&taker.perp_positions, cx.market_index)?;
    let taker_existing_position_params = taker.perp_positions[taker_position_index]
        .get_existing_position_params_for_order_action(taker_direction);
    let mut maker = cx.makers_and_referrer.get_ref_mut(&pair.maker_key)?;
    let mut maker_stats = Some(cx.makers_and_referrer_stats.get_ref_mut(&maker.authority)?);
    let taker_key = cx.accounts.taker.key();
    let mut none_filler: Option<&mut User> = None;
    let mut none_filler_stats: Option<&mut UserStats> = None;
    let mut no_escrow: Option<&mut RevenueShareEscrowZeroCopyMut> = None;
    let mut filler_reward_paid = 0u64;
    let mut maker_side = controller::orders::MakerSide::bind(
        &mut maker,
        maker_stats.as_deref_mut(),
        pair.maker_key,
        taker_direction,
        cx.market_index,
        // Velocity reserves a CLOB order's worst case at placement, so the
        // fill unwinds that reservation.
        true,
        Some(pair.counterparty.order_ref.order_id as u32),
    )?;
    controller::orders::settle_external_match_fill(
        controller::orders::FillAmounts {
            base: pair.base_filled,
            quote: pair.quote_filled,
        },
        &mut controller::orders::TakerSide {
            user: &mut taker,
            stats: &mut taker_stats,
            key: taker_key,
            position_index: taker_position_index,
            order,
            direction: taker_direction,
            existing_position_params_before: taker_existing_position_params,
            // The aggressor rested on the book first, so its reservation is
            // still live and this fill unwinds it.
            reserved: true,
        },
        &mut maker_side,
        &controller::orders::ExternalMatch {
            effective_taker_limit: Some(pair.aggressor.price),
            oracle_price,
        },
        &mut controller::orders::FillerSide {
            user: &mut none_filler,
            stats: &mut none_filler_stats,
            key: taker_key,
            rev_share_escrow: &mut no_escrow,
        },
        &mut controller::orders::SettleContext {
            market: market.deref_mut(),
            rules: &controller::orders::PricingRules::for_settlement(cx.state),
            // This crank settles a cross, never a liquidation.
            mode: crate::state::fill_mode::FillMode::Fill,
            oracle_map,
            now: cx.clock.unix_timestamp,
            slot: cx.clock.slot,
            filler_reward_paid: &mut filler_reward_paid,
        },
    )?;
    drop(maker_stats);
    drop(maker);
    drop(taker_stats);
    drop(taker);
    drop(market);

    // The check belongs to the settlement, not to the caller. This is the one
    // fill path with no router pass behind it, so nothing else applies the
    // shared post-fill rules to either leg. Keeping the two together stops the
    // call from being dropped.
    Ok(post_checks::check_pair_fill(
        &cx.accounts.taker,
        &cx.accounts.taker_stats,
        cx.makers_and_referrer,
        cx.makers_and_referrer_stats,
        maps,
        cx.market_index,
        &PairFill {
            counterparty_key: pair.maker_key,
            counterparty_direction: taker_direction.opposite(),
            base_filled: pair.base_filled,
            quote_filled: pair.quote_filled,
        },
        facts,
        cx.clock.unix_timestamp,
    )?)
}

/// What one settled pair moved, as the post-fill checks read it.
struct PairFill {
    /// The counterparty's margin account, as the user map keys it.
    counterparty_key: Pubkey,
    /// The side the counterparty took, which is the aggressor's opposite.
    counterparty_direction: PositionDirection,
    base_filled: u64,
    quote_filled: u64,
}

/// What the post-fill checks need and the settled fill cannot report.
struct PairFillFacts {
    /// Whether the aggressor's order reduces the position it held before the
    /// match. A reducing order is held to maintenance margin, not to fill
    /// margin, and is exempt from the buffered floor.
    aggressor_order_decreasing: bool,
    /// Whether the aggressor's position in this market is isolated, which
    /// decides the margin scope the check runs under.
    aggressor_is_isolated: bool,
    /// The market's open interest before the match.
    perp_market_oi_before: u128,
    /// Whether the oracle is too old to price margin.
    oracle_stale_for_margin: bool,
}

/// The shared post-fill checks, and the evidence that they ran.
///
/// The checks hold both sides of the settled pair to fill or maintenance margin
/// under each side's own margin scope, the equity breaker, the buffered floor,
/// the spot-borrow oracle and interest rules, and the stale-oracle
/// open-interest rule.
///
/// Every other fill path reaches these through the router pass. This branch
/// settles the pair itself, so it is the one fill path that applies them
/// directly. The reservation each order holds bounds the size of the fill and
/// nothing more. These checks refuse collateral state the reservation cannot
/// see: a breaker that tripped, a floor the account no longer clears, or a spot
/// borrow whose oracle went stale while the order rested.
///
/// The counterparty's margin scope is read off its live position, because
/// neither book row records whether the position it fills is isolated.
///
/// The only failure mode of a check like this is absence, and absence is hard
/// to see. The fill still balances, the records still emit, and every test of
/// the check itself still passes. The check therefore lives alone in this
/// module and hands back a [`PairChecked`] that nothing outside can build. A
/// settlement that skips it has nothing to return, and the crate stops
/// compiling.
#[allow(clippy::too_many_arguments)]
mod post_checks {
    use super::*;

    /// Evidence that [`check_pair_fill`] ran. The unit field is private to this
    /// module, so no other module can build one.
    pub(super) struct PairChecked(());

    pub(super) fn check_pair_fill<'info>(
        taker_loader: &AccountLoader<'info, User>,
        taker_stats_loader: &AccountLoader<'info, UserStats>,
        makers_and_referrer: &UserMap<'info>,
        makers_and_referrer_stats: &UserStatsMap<'info>,
        maps: &mut AccountMaps<'info>,
        market_index: u16,
        fill: &PairFill,
        facts: &PairFillFacts,
        now: i64,
    ) -> VelocityResult<PairChecked> {
        let counterparty_is_isolated = {
            let counterparty = makers_and_referrer.get_ref(&fill.counterparty_key)?;
            get_position_index(&counterparty.perp_positions, market_index)
                .map(|position_index| counterparty.perp_positions[position_index].is_isolated())
                .unwrap_or(false)
        };
        let mut maker_fills = BTreeMap::new();
        controller::orders::update_maker_fills_map(
            &mut maker_fills,
            &fill.counterparty_key,
            fill.counterparty_direction,
            fill.base_filled,
            counterparty_is_isolated,
        )?;

        let limits = controller::orders::TakerRiskLimits {
            market_index,
            order_decreasing: facts.aggressor_order_decreasing,
            is_isolated: facts.aggressor_is_isolated,
            oracle_stale_for_margin: facts.oracle_stale_for_margin,
            // A cross of two resting remainders is never a liquidation.
            mode: crate::state::fill_mode::FillMode::Fill,
            // Both legs are ordinary users who keep the positions this
            // settles, so both carry their own risk and both are checked.
            taker_exposure_closed_by_caller: false,
            perp_market_oi_before: facts.perp_market_oi_before,
        };
        let taker = load!(taker_loader)?;
        let mut taker_stats = load_mut!(taker_stats_loader)?;
        limits.check_after_fill(
            &mut controller::orders::TakerRefs {
                user: &taker,
                stats: &mut taker_stats,
            },
            &mut controller::orders::FillParties {
                maps,
                makers_and_referrer,
                makers_and_referrer_stats,
            },
            controller::orders::FillAmounts {
                base: fill.base_filled,
                quote: fill.quote_filled,
            },
            &maker_fills,
            now,
        )?;
        Ok(PairChecked(()))
    }
}

/// Tell the book what the cross took, and unwind whatever it removed.
///
/// One call covers both sides. Each order shrinks in place, and the book culls
/// a leftover that falls under its minimum.
fn report_pair_fill_to_book<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    pair: &RemainderPair<'_>,
) -> Result<()> {
    let filled = {
        let clob = ClobMarket::from_slab(
            &cx.accounts.quoter_slab,
            cx.market_index,
            &cx.accounts.clob_market,
            &cx.accounts.clob_program,
        )?;
        clob.fill(ClobFillArgsV0 {
            fills: vec![
                ClobFillRequestV0 {
                    order_ref: pair.aggressor.order_ref,
                    base_asset_amount: pair.base_filled,
                },
                ClobFillRequestV0 {
                    order_ref: pair.counterparty.order_ref,
                    base_asset_amount: pair.base_filled,
                },
            ],
        })?
    };

    // A leg the book no longer holds returns the open-order slot and whatever
    // the cull dropped to its owner.
    for (leg, owner_is_taker) in filled.filled.iter().zip([true, false]) {
        if !leg.removed {
            continue;
        }
        let direction = if owner_is_taker {
            cx.taker_direction
        } else {
            cx.taker_direction.opposite()
        };
        if owner_is_taker {
            let mut taker = load_mut!(cx.accounts.taker)?;
            taker.unwind_removed_clob_order(
                cx.market_index,
                &direction,
                leg.culled_base_asset_amount,
                leg.order_id,
                true,
                pair.aggressor.reduce_only,
            )?;
        } else {
            let mut maker = cx.makers_and_referrer.get_ref_mut(&pair.maker_key)?;
            maker.unwind_removed_clob_order(
                cx.market_index,
                &direction,
                leg.culled_base_asset_amount,
                leg.order_id,
                true,
                pair.counterparty.reduce_only,
            )?;
        }
    }
    Ok(())
}

/// Stage this crank for the book's taker-origin cross, if it has one.
///
/// This is the discovery half of [`handle_crank_taker_origin_cross`]. The cross
/// conditions' resolver (`handle_resolve_crank_cross_match`) calls it ahead of
/// the maker-against-maker cross it stages otherwise. A `ResolvedCrankV0` names
/// its own executor, so one resolver serves both. The order is the economics. A
/// taker-origin cross resolves in the taker's favour before the protocol
/// middles the same crossed book as arbitrage.
///
/// The crank needs no condition slot or watch of its own. It only ever resolves
/// the tops of the matchable book, so a taker-origin cross can appear in two
/// ways, and both are already wired. A side's best moved, which the cross
/// condition's 8-byte change-watch over `best_bid` and `best_ask` covers,
/// because a crossing order is by definition a new best. Or a front-of-book
/// order reached its `activation_slot`, which the cross-activation `AtSlot` hint
/// names. `note_activation` min-folds every placement's activation slot,
/// including a migrating remainder's.
///
/// `min_payment` stays the market's `keeper_payment_lamports`, the same price
/// the maker-against-maker cross on that slot carries, because relay measures
/// lamports. `assert_paid_v0` watches the payout account's lamport balance, and
/// this crank pays the same reservoir lamports as every other CLOB crank. The
/// quote-denominated crank reward can be zero, because a dust or equal-price
/// improvement resolves for free by design. Pricing the condition above the
/// lamport payout would make those crosses undiscoverable, and a unit of dust in
/// front of a gated remainder would strand it for its whole life.
///
/// Two remainders can face each other only while something crosses the earlier
/// one, so their pair usually becomes resolvable when the blocker is removed
/// rather than when a new order arrives. A removal moves a head u32 and fires
/// the change-watch whenever the blocker is its side's head, which is the
/// ordinary case. It misses two shapes. A blocker behind a better-priced order
/// that nothing can match yet rewrites an arena link, not the head. A blocker
/// that leaves the matchable set by passing its own `max_ts` writes nothing at
/// all. The expire condition's `AtTimestamp` hint covers the second shape one
/// hop earlier, because it fires at that `max_ts` and removing the expired order
/// then moves the head. The every-slots cross fallback is the floor under both,
/// so a missed hint costs latency rather than liveness.
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
    let book_slot = ctx.accounts.quoter_slab.clob_slot(market_index)?;
    if !book_slot.quotes() {
        // The crank refuses a killed or unvetted book, so there is no work to
        // stage against one. The eviction and force-cancel paths reclaim orders
        // left on a dead book.
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
        true,
    )?
    .ok_or(ErrorCode::NoTakerOriginCross)?;

    let Some((cross, side)) = stageable_cross(&bids, &asks) else {
        return Ok(None);
    };

    let (protocol_user, protocol_user_stats) = pdas::protocol_user_pair();
    let taker_ref = aggressor_of(&cross, side).user;
    let counterparty_ref = counterparty_of(&cross, side).user;
    let (taker, taker_stats) = pdas::user_pair(&taker_ref.authority, taker_ref.sub_account_id);
    // The protocol `User` is the filler on this path, and the crank loads each
    // margin account once, so it cannot also be a side of the cross it
    // resolves.
    if taker == protocol_user
        || pdas::user(&counterparty_ref.authority, counterparty_ref.sub_account_id) == protocol_user
    {
        return Ok(None);
    }

    let cross_rows = cross_read_depth(&bids, &asks, &cross);

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
        // `(authority, sub_account_id)`, which the book stores for this use.
        .map_section(oracle, quote_spot_market_index, market_index)
        .maker_refs([counterparty_ref])
        // The quoter tail. The crank routes the remainder like any other fill,
        // and every router fill carries the market's slab and consults its book.
        // A taker-origin order rests on the CLOB and nowhere else, so the book
        // this resolver reads is that baseline.
        .account(ctx.accounts.quoter_slab.key(), false)
        .account(ctx.accounts.clob_market.key(), true)
        .account(crate::ids::clob_program::id(), false)
        // The resolver stages no quoters of its own, so it claims no route. A
        // staged crank routes through the market's baseline, the CLOB and the
        // vAMM, which every fill carries anyway. A keeper that wants a taker's
        // custom quoters consulted builds the call itself and claims the route
        // the taker signed.
        .arg(CrankTakerOriginCrossArgs {
            market_index,
            cross_rows,
            signed_route: Vec::new(),
        })?,
    ))
}

/// The cross a resolver may stage, if the book has one.
///
/// Price priority decides which crank owns the front of a book. A
/// maker-against-maker cross ahead of a remainder is `crank_cross_match`'s work,
/// and clearing it brings the remainder to the front. The two cranks therefore
/// run in turn instead of competing for the same book.
fn stageable_cross(bids: &[RestingOrder], asks: &[RestingOrder]) -> Option<(Cross, ClobSide)> {
    let (Some(best_bid), Some(best_ask)) = (bids.first(), asks.first()) else {
        return None;
    };
    if !best_bid.taker_origin && !best_ask.taker_origin {
        return None;
    }
    let crosses = resolve_crosses(bids, asks, MAX_CROSSES_PER_CRANK);
    let cross = crosses
        .iter()
        .find(|cross| cross.kind != CrossKind::ProtocolMiddles)?;
    Some((*cross, cross.kind.aggressor_side()?))
}

/// How deep the crank must read to see the cross this resolver picked.
///
/// Rows come back best first, so the deeper of the pair's two positions is the
/// whole window the crank needs.
fn cross_read_depth(bids: &[RestingOrder], asks: &[RestingOrder], cross: &Cross) -> u16 {
    let depth = |rows: &[RestingOrder], target: &RestingOrder| {
        rows.iter()
            .position(|row| row.order_ref == target.order_ref)
            .unwrap_or(0) as u16
    };
    depth(bids, &cross.bid)
        .max(depth(asks, &cross.ask))
        .saturating_add(1)
        .min(MAX_CROSS_ROWS)
}
