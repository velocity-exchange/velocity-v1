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
//!
//! ## Discovery
//!
//! The crank needs no condition slot or watch of its own. It only ever resolves
//! the tops of the matchable book, so a taker-origin cross can appear in two
//! ways, and both are already wired. A side's best moved, which the cross
//! condition's 8-byte change-watch over `best_bid` and `best_ask` covers,
//! because a crossing order is by definition a new best. Or a front-of-book
//! order reached its `activation_slot`, which the cross-activation `AtSlot` hint
//! names. `note_activation` min-folds every placement's activation slot,
//! including a migrating remainder's.
//!
//! `min_payment` stays the market's `keeper_payment_lamports`, because relay
//! measures lamports and `assert_paid_v0` watches the payout account's
//! balance. The quote-denominated crank reward can be zero, because a dust or
//! equal-price improvement resolves for free by design. Pricing the condition
//! above the lamport payout would leave those crosses undiscoverable, and a
//! unit of dust in front of a gated remainder would strand it for its life.
//!
//! Two remainders can face each other only while something crosses the earlier
//! one, so their pair usually becomes resolvable when the blocker is removed
//! rather than when a new order arrives. A removal moves a head u32 and fires
//! the change-watch whenever the blocker is its side's head. It misses two
//! shapes. A blocker behind a better-priced order that nothing can match yet
//! rewrites an arena link, not the head. A blocker that leaves the matchable
//! set by passing its own `max_ts` writes nothing at all. The expire
//! condition's `AtTimestamp` hint covers the second shape, because removing
//! the expired order then moves the head. The every-slots cross fallback is
//! the floor under both, so a missed hint costs latency rather than liveness.

use {
    super::helpers::crank_common::{
        book_l3_sides, program_keeper_mode, BookSides, ResolveClobCrank,
    },
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
        math::crosses::{resolve_crosses, Cross, CrossKind, RestingOrder},
        msg,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            events::TakerOriginCrossRecordV1,
            fill_mode::FillMode,
            order_params::NO_ROUTE_DIGEST,
            pdas,
            perp_market_map::{get_writable_perp_market_set, MarketSet, PerpMarketMap},
            prop_amm::{ClobMarket, FillArgsV0, FillRequestV0, QuoterSlabExt, SideV0, UserRefV0},
            revenue_share::RevenueShareEscrowZeroCopyMut,
            signed_msg_user::{SignedMsgUserOrdersLoader, SIGNED_MSG_PDA_SEED},
            state::State,
            user::{
                OrderReservation, OrderStatus, PerpPosition, ReferrerStatus, ReleaseCheck, User,
                UserStats,
            },
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
    /// chose. Derived seeds mean a caller cannot omit or substitute it. An
    /// absent record arrives system-owned and reads as unrouted. It is
    /// writable so that a fill that takes the whole remainder releases its
    /// entry.
    /// CHECK: `SignedMsgUserOrdersLoader` checks the owner and discriminator.
    #[account(
        mut,
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
fn aggressor_of(cross: &Cross, side: SideV0) -> RestingOrder {
    match side {
        SideV0::Bid => cross.bid,
        SideV0::Ask => cross.ask,
    }
}

/// The other side: the one whose price the match settles at.
fn counterparty_of(cross: &Cross, aggressor_side: SideV0) -> RestingOrder {
    match aggressor_side {
        SideV0::Bid => cross.ask,
        SideV0::Ask => cross.bid,
    }
}

/// Ceiling on rows one crank reads per side, not the working depth. The
/// caller sets how deep to go, since the resolver that stages this runs
/// under simulation and can walk the whole book. This stops an argument
/// from spending the crank's compute budget on a book that does not need it.
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
    let program_keeper_mode = program_keeper_mode(
        &ctx.accounts.filler,
        &ctx.accounts.state,
        ctx.accounts.crank_conditions.is_some(),
    )?;

    // The settlement loads three margin accounts mutably at once. An overlap
    // fails on the borrow, which reports nothing about the cause.
    validate!(
        ctx.accounts.filler.key() != ctx.accounts.taker.key(),
        ErrorCode::CrossParticipantOverlap,
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
    // The taker's escrow, when the caller carries it. The referee discount and
    // the referrer reward are keyed by market, so they bind here. A builder
    // fee does not bind, because `L3RowV0` names the book's handle and not the
    // order id.
    let mut rev_share_escrow = if state.builder_codes_enabled() {
        let taker_authority = load!(ctx.accounts.taker)?.authority;
        let escrow = crate::instructions::optional_accounts::get_revenue_share_escrow_account(
            remaining_accounts_iter,
            &taker_authority,
        )?;
        require_referral_escrow(escrow.is_some(), &*load!(ctx.accounts.taker_stats)?)?;
        escrow
    } else {
        None
    };

    // A referred taker earns the same accelerated rate here as on any other
    // fill path. The status rides the account tail after the escrow.
    let referrer_is_accelerated =
        crate::instructions::optional_accounts::get_referrer_accelerated_status(
            remaining_accounts_iter,
            rev_share_escrow.as_ref(),
        )?;

    validate!(
        !makers_and_referrer
            .0
            .contains_key(&ctx.accounts.filler.key())
            && !makers_and_referrer
                .0
                .contains_key(&ctx.accounts.taker.key()),
        ErrorCode::CrossParticipantOverlap,
        "the counterparty section must not repeat the taker or the cranker"
    )?;

    let book_slot = ctx.accounts.quoter_slab.clob_slot(market_index)?;
    validate!(
        book_slot.quotes(),
        ErrorCode::ClobQuoterNotActive,
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
        &book_slot,
        market_index,
        cross_rows,
        taker_ref,
        &mut cpi_scratch,
    )?;

    drop(book_slot);

    let taker_direction = PositionDirection::from(aggressor_side);
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
        referrer_is_accelerated,
    };

    // Two crossed remainders are the one case the router cannot reach. The
    // book holds both back, because each is taker-origin and each is crossed
    // by the other, so the gate passes over whichever one a fill tries to
    // take. The improvement between their prices belongs to one of them, and
    // the resolution above worked out which.
    if counterparty.taker_origin {
        return settle_taker_origin_pair(
            &cx,
            &subject,
            &subject_order,
            &counterparty,
            &mut maps,
            &mut rev_share_escrow,
        );
    }

    let route_claim = SignedRouteClaim {
        quoters: &signed_route,
        digest: signed_route_digest(
            &ctx.accounts.signed_msg_user_orders,
            market_index,
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
        &mut rev_share_escrow,
    )?;

    let fill_price = crate::math::orders::calculate_fill_price(
        quote_filled,
        base_filled,
        crate::math::constants::BASE_PRECISION_U64,
    )?;
    // The router fill applies the oracle gates and the shared post-fill checks
    // itself, so this branch only prices the cross. It is measured against the
    // price the fill reached, which is known only after the fill.
    let fee = price_cross(&cx, &subject_order, fill_price, base_filled, &mut maps)?.fee;
    let crank_reward = pay_crank_reward(&cx, &fee, quote_filled, &mut maps)?;

    let remainder_base_asset_amount = report_fills_to_book(
        &cx,
        &[BookFill {
            order: &subject_order,
            owner: None,
            base_asset_amount: base_filled,
        }],
    )?;

    pay_crank_lamports(&cx)?;
    emit_taker_origin_record(
        &cx,
        &TakerOriginOutcome {
            rested: &subject_order,
            base_filled,
            quote_filled,
            fill_price,
            improvement: fee.improvement,
            crank_reward,
            remainder_base_asset_amount,
        },
    );

    Ok(())
}

/// What every step of one crank shares.
///
/// The handler builds this once, after the book read decides which row
/// aggresses. Every field is fixed for the whole crank. The three market maps
/// stay separate parameters, the way every other instruction here passes them.
struct TakerOriginContext<'a, 'info> {
    accounts: &'a CrankTakerOriginCross<'info>,
    market_index: u16,
    /// The taker's identity as the book reports it on its own rows.
    taker_ref: UserRefV0,
    /// The side the taker's remainder demanded liquidity on.
    taker_direction: PositionDirection,
    state: &'a State,
    makers_and_referrer: &'a UserMap<'info>,
    makers_and_referrer_stats: &'a UserStatsMap<'info>,
    clock: &'a Clock,
    /// True when the protocol `User` cranks. The reservoir then pays the
    /// keeper its lamports.
    program_keeper_mode: bool,
    /// True when the taker's referrer earns the accelerated rate.
    referrer_is_accelerated: bool,
}

impl TakerOriginContext<'_, '_> {
    /// Release the taker's signed-message entry for a remainder that left the
    /// book. A taker with no record has no entry to release.
    fn release_taker_route(&self, clob_order_id: u64) {
        if let Some(mut record) = crate::state::signed_msg_user::carried_signed_msg_record(
            Some(&*self.accounts.signed_msg_user_orders),
            &self.taker_ref.authority,
        ) {
            record.clear_resting_route(self.market_index, clob_order_id);
        }
    }
}

/// The cross this crank settles, and its two rows.
struct SubjectCross {
    /// The side that demanded liquidity.
    aggressor_side: SideV0,
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
    book_slot: &crate::state::prop_amm::QuoterSlotV0,
    market_index: u16,
    cross_rows: u16,
    taker_ref: UserRefV0,
    cpi_scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
) -> Result<SubjectCross> {
    let BookSides { bids, asks } = book_l3_sides(
        book_slot,
        &ctx.accounts.quoter_slab,
        market_index,
        cross_rows.min(MAX_CROSS_ROWS),
        &[
            ctx.accounts.clob_market.to_account_info(),
            ctx.accounts.clob_program.to_account_info(),
        ],
        cpi_scratch,
        true,
        RestingOrder::from_row,
    )?
    .ok_or(ErrorCode::NoTakerOriginCross)?;
    // Price priority decides which crank owns the front of a book. This
    // instruction is permissionless, so the rule is enforced here as well as
    // in the resolver that stages it. Taking a remainder behind a maker cross
    // would fill out of depth the better-priced resting order had priority on.
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
/// the omitted-account sentinel. The staged path names the PDA without knowing
/// whether the account was ever created. Only a record that exists, belongs to
/// this taker, and names this order carries a route.
fn signed_route_digest(
    record: &UncheckedAccount<'_>,
    market_index: u16,
    clob_order_id: u64,
) -> Result<crate::state::order_params::RouteDigest> {
    let digest = if record.owner == &crate::ID {
        record
            .load()?
            .route_for_clob_order(market_index, clob_order_id)
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
    rev_share_escrow: &mut Option<RevenueShareEscrowZeroCopyMut<'info>>,
) -> Result<controller::orders::FillAmounts> {
    // The order is a local. It came off a book and belongs to no `orders` slot,
    // so the fill takes it directly and the taker needs no spare slot. The
    // reservation from when the remainder rested is still on the position.
    let mut order =
        controller::orders::taker_origin_order(cx.market_index, cx.taker_direction, subject_order);

    let mark = crate::instructions::RouteMark::read(maps, cx.market_index)?;

    // The taker is its own filler, so no reward comes out of the taker fee. The
    // cranker is paid below, out of the improvement it delivered. A crank that
    // improves nothing earns nothing.
    let filled = crate::instructions::RouteFill {
        state: cx.state,
        clock: cx.clock,
        tail,
        // Reuse the CPI scratch the book read filled. Its buffers clear and
        // refill per leg, so one fill pays for one set of buffers.
        scratch: cpi_scratch,
    }
    .run(
        crate::instructions::RouteRequest {
            order: crate::instructions::RoutedOrder {
                direction: crate::instructions::route_direction(cx.taker_direction),
                unfilled: subject_order.base_asset_amount,
                taker: cx.taker_ref,
                limit_price: subject_order.price,
                mark,
            },
            // The window is measured from the subject's own rest, not assumed
            // from the crank. On a zero-delay book, rest through placement is
            // a zero-length window, and a caller can place and crank in the
            // same slot. The subject is the flow this fill transmits.
            taker_served_window: crate::math::crosses::served_window(
                subject_order.placed_slot,
                cx.clock.slot,
            ),
            // This crank owes the taker the improvement, so it is the one
            // caller that may fill the depth its order reserves.
            include_taker_origin_reservations: true,
            claim: Some(crate::instructions::RouteClaim {
                quoters: route_claim.quoters,
                digest: route_claim.digest,
            }),
            // The taker is not here to choose the account list, so the cranker
            // answers for what it left out, as a keeper fill does.
            filler: crate::instructions::FillerTerms::keeper(Some(
                &cx.accounts.instructions_sysvar.to_account_info(),
            ))?,
        },
        controller::orders::FillRequest {
            order: &mut order,
            // The remainder rested on the book first, so it holds a
            // reservation the fill must unwind as it fills.
            reserved: true,
            mode: FillMode::Fill,
            referrer_is_accelerated: cx.referrer_is_accelerated,
        },
        controller::orders::PerpFillAccounts {
            user: &cx.accounts.taker,
            user_stats: &cx.accounts.taker_stats,
            filler: &cx.accounts.taker,
            filler_stats: &cx.accounts.taker_stats,
            rev_share_escrow: &mut rev_share_escrow.as_mut(),
        },
        &mut controller::orders::FillParties {
            maps,
            makers_and_referrer: cx.makers_and_referrer,
            makers_and_referrer_stats: cx.makers_and_referrer_stats,
        },
    )?
    .amounts;

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

/// Whether the cranker is paid a reward out of the improvement.
///
/// A cranker that shares the taker's authority earns no reward and no filler
/// volume, as on a keeper fill. A cranker outside pool 0 cannot hold the perp
/// quote the reward pays in, so it fails.
fn cranker_earns_reward(filler: &User, taker_authority: &Pubkey) -> Result<bool> {
    if filler.authority == *taker_authority {
        return Ok(false);
    }

    validate!(
        filler.pool_id == 0,
        ErrorCode::InvalidPoolId,
        "filler pool id ({}) != 0",
        filler.pool_id
    )?;

    Ok(true)
}

/// Fail when a referred taker's escrow is missing. The referee discount and
/// the referrer reward bind to it, and both branches settle through it.
fn require_referral_escrow(has_escrow: bool, taker_stats: &UserStats) -> Result<()> {
    validate!(
        has_escrow || !ReferrerStatus::has_builder_referral(taker_stats.referrer_status),
        ErrorCode::UnableToLoadRevenueShareAccount,
        "the taker is referred with an escrow but no RevenueShareEscrow account was included"
    )?;

    Ok(())
}

/// The oracle pre-flight, and what the taker gained.
///
/// A caller that settles the match itself must run this before it touches the
/// book. The pre-flight refuses a market in settlement, paused fills, an invalid
/// oracle, and a price outside the band. A refusal must leave the book as it
/// was.
///
/// The pre-flight's own mm-oracle price is dropped. The match and the maker
/// band use the plain oracle price, as the router pass does.
fn price_cross<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    rested: &RestingOrder,
    fill_price: u64,
    base_filled: u64,
    maps: &mut AccountMaps,
) -> Result<controller::orders::TakerOriginCrossPricing> {
    let taker_stats = load!(cx.accounts.taker_stats)?;
    Ok(controller::orders::price_taker_origin_cross(
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
    )?)
}

/// The cranker's cut of what the taker gained.
///
/// The reward is paid in full or not at all, and only out of the improvement,
/// so a crank that improves nothing earns nothing. The improvement is measured
/// against the price the fill reached, not against one counterparty's quote.
///
fn pay_crank_reward<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    fee: &crate::math::fees::TakerOriginCrossFee,
    quote_filled: u64,
    maps: &mut AccountMaps,
) -> Result<u64> {
    if fee.crank_reward == 0
        || !cranker_earns_reward(&*load!(cx.accounts.filler)?, &cx.taker_ref.authority)?
    {
        return Ok(0);
    }

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

/// One order the book is told a fill took.
struct BookFill<'a> {
    order: &'a RestingOrder,
    /// The counterparty's margin account, as the user map keys it. `None` is
    /// the taker.
    owner: Option<Pubkey>,
    base_asset_amount: u64,
}

/// Tell the book what each fill took, and unwind whatever it removed.
///
/// Each order shrinks in place and keeps its queue position and its id. A
/// partial fill does not change the price the remainder wants, and re-placing
/// it would send it to the back of its own level. The reservation and the
/// open-order slot stay on the owner while the order rests. An order the book
/// culls for falling under its minimum gives them back.
///
/// Returns the base the taker's order still rests at.
fn report_fills_to_book<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    fills: &[BookFill<'_>],
) -> Result<u64> {
    let reported = {
        let clob = ClobMarket::from_slab(
            &cx.accounts.quoter_slab,
            cx.market_index,
            &cx.accounts.clob_market,
            &cx.accounts.clob_program,
        )?;

        clob.fill(FillArgsV0 {
            fills: fills
                .iter()
                .map(|fill| FillRequestV0 {
                    order_ref: fill.order.order_ref,
                    base_asset_amount: fill.base_asset_amount,
                })
                .collect(),
        })?
    };

    validate!(
        reported.filled.len() == fills.len(),
        ErrorCode::NoTakerOriginCross,
        "the book reported {} fills for {} requests",
        reported.filled.len(),
        fills.len()
    )?;

    let mut taker_remainder = 0;
    for (fill, leg) in fills.iter().zip(reported.filled.iter()) {
        if !leg.removed {
            if fill.owner.is_none() {
                taker_remainder = fill
                    .order
                    .base_asset_amount
                    .saturating_sub(fill.base_asset_amount);
            }

            continue;
        }

        let (mut owner, direction) = match fill.owner {
            None => (load_mut!(cx.accounts.taker)?, cx.taker_direction),
            Some(key) => (
                cx.makers_and_referrer.get_ref_mut(&key)?,
                cx.taker_direction.opposite(),
            ),
        };
        owner.close_book_order(
            &OrderReservation::book_order(
                cx.market_index,
                direction,
                leg.culled_base_asset_amount,
                fill.order.reduce_only,
            ),
            ReleaseCheck::HeldToReservation,
            leg.order_id,
            OrderStatus::Canceled,
        )?;

        drop(owner);
        // The counterparty's record is not in this transaction, so only the
        // taker's entry is released here.
        if fill.owner.is_none() {
            cx.release_taker_route(leg.order_id);
        }
    }

    Ok(taker_remainder)
}

/// The keeper's lamports, as every CLOB crank pays them.
fn pay_crank_lamports<'info>(cx: &TakerOriginContext<'_, 'info>) -> Result<()> {
    if let (true, Some(conditions)) = (cx.program_keeper_mode, &cx.accounts.crank_conditions) {
        ClobCrankConditionsV0::pay_crank(
            conditions,
            &cx.accounts.authority.to_account_info(),
            |payments| u64::from(payments.taker_origin_cross),
        )?;
    }

    Ok(())
}

/// What one crank did for the taker, as its record reports it.
struct TakerOriginOutcome<'a> {
    /// The taker's remainder as it rested before the crank.
    rested: &'a RestingOrder,
    base_filled: u64,
    quote_filled: u64,
    fill_price: u64,
    improvement: u64,
    crank_reward: u64,
    remainder_base_asset_amount: u64,
}

/// What the taker gained and what the cranker took out of it. The fill's own
/// `OrderActionRecord`s carry the per-source detail.
fn emit_taker_origin_record<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    outcome: &TakerOriginOutcome<'_>,
) {
    emit!(TakerOriginCrossRecordV1 {
        ts: cx.clock.unix_timestamp,
        slot: cx.clock.slot,
        market_index: cx.market_index,
        taker: cx.accounts.taker.key(),
        filler: cx.accounts.filler.key(),
        base_asset_amount: outcome.base_filled,
        quote_asset_amount: outcome.quote_filled,
        rest_price: outcome.rested.price,
        fill_price: outcome.fill_price,
        improvement: outcome.improvement,
        crank_reward: outcome.crank_reward,
        remainder_base_asset_amount: outcome.remainder_base_asset_amount,
        clob_order_id: outcome.rested.order_ref.order_id,
    });

    msg!(
        "taker-origin remainder routed: {} base at {} instead of {}, improvement {} quote, cranker paid {}",
        outcome.base_filled,
        outcome.fill_price,
        outcome.rested.price,
        outcome.improvement,
        outcome.crank_reward
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

impl RemainderPair<'_> {
    /// Both orders, each reduced by the size the pair settled.
    fn book_fills(&self) -> [BookFill<'_>; 2] {
        [
            BookFill {
                order: self.aggressor,
                owner: None,
                base_asset_amount: self.base_filled,
            },
            BookFill {
                order: self.counterparty,
                owner: Some(self.maker_key),
                base_asset_amount: self.base_filled,
            },
        ]
    }
}

/// Settle two crossed remainders against each other, and tell the book.
///
/// There is no intermediary and no router. Both orders rest on the book and the
/// price between them is already decided. The later of the two to rest
/// aggresses, and the earlier one's price stands. What remains is an ordinary
/// two-user match at that price. [`controller::orders::SettledMatch`] holds it
/// to the order-layer steps of every fill. The book applies both reductions in
/// place, so neither order loses its queue position.
///
/// The cranker is paid out of the improvement, as on every other path here, so
/// the settlement takes no filler and no reward comes out of the taker fee.
fn settle_taker_origin_pair<'c: 'info, 'info>(
    cx: &TakerOriginContext<'_, 'info>,
    cross: &Cross,
    aggressor: &RestingOrder,
    counterparty: &RestingOrder,
    maps: &mut AccountMaps<'info>,
    rev_share_escrow: &mut Option<RevenueShareEscrowZeroCopyMut<'info>>,
) -> Result<()> {
    let mut order =
        controller::orders::taker_origin_order(cx.market_index, cx.taker_direction, aggressor);
    let pair = bind_pair(cx, cross, aggressor, counterparty, &mut order, maps)?;

    // The pre-flight comes first, because a refusal must leave the book as it
    // was. The facts it reports are inputs to the post-fill checks below.
    let pricing = price_cross(cx, aggressor, counterparty.price, pair.base_filled, maps)?;
    let settled = controller::orders::SettledMatch::read(
        cx.state,
        maps,
        &cx.accounts.taker,
        &cx.accounts.taker_stats,
        &mut order,
        cx.clock,
    )?;
    admit_pair_match(cx, &pair, &settled, maps)?;

    let facts = pair_fill_facts(cx, &order, &pricing)?;
    settle_pair_funding(cx, &pair, &maps.perp_market_map)?;

    // The settlement returns evidence that the shared post-fill checks ran on
    // both legs. Nothing outside `post_checks` can build that evidence.
    let _checked = settle_pair_match(
        cx,
        &pair,
        &mut order,
        settled.oracle_price(),
        &facts,
        maps,
        rev_share_escrow,
    )?;

    settled.apply_bookkeeping(
        cx.state,
        &mut order,
        &cx.accounts.taker,
        &cx.accounts.taker_stats,
        &mut controller::orders::FillParties {
            maps,
            makers_and_referrer: cx.makers_and_referrer,
            makers_and_referrer_stats: cx.makers_and_referrer_stats,
        },
        controller::orders::FillAmounts {
            base: pair.base_filled,
            quote: pair.quote_filled,
        },
    )?;

    let remainder_base_asset_amount = report_fills_to_book(cx, &pair.book_fills())?;
    let crank_reward = pay_crank_reward(cx, &pricing.fee, pair.quote_filled, maps)?;

    pay_crank_lamports(cx)?;
    emit_taker_origin_record(
        cx,
        &TakerOriginOutcome {
            rested: aggressor,
            base_filled: pair.base_filled,
            quote_filled: pair.quote_filled,
            fill_price: counterparty.price,
            improvement: pricing.fee.improvement,
            crank_reward,
            remainder_base_asset_amount,
        },
    );

    Ok(())
}

/// Find the counterparty, stamp the live market's reduce-only rule on the
/// aggressor's order, and size the match.
///
/// A row that rested while the market was `Active` carries `reduce_only`
/// false, and the market may have flipped to `ReduceOnly` since. The flag is
/// re-derived the way `admit_perp_market` does for a routed fill.
fn bind_pair<'a>(
    cx: &TakerOriginContext<'_, '_>,
    cross: &Cross,
    aggressor: &'a RestingOrder,
    counterparty: &'a RestingOrder,
    order: &mut crate::state::user::Order,
    maps: &AccountMaps,
) -> Result<RemainderPair<'a>> {
    let maker_key = counterparty_key(cx, counterparty)?;
    let market_is_reduce_only = maps
        .perp_market_map
        .get_ref(&cx.market_index)?
        .is_reduce_only()?;
    order.reduce_only |= market_is_reduce_only;

    let base_filled = {
        let taker = load!(cx.accounts.taker)?;
        let counterparty_user = cx.makers_and_referrer.get_ref(&maker_key)?;
        pair_fill_size(
            cross.base_asset_amount,
            cx.taker_direction,
            taker.get_perp_position(cx.market_index)?,
            counterparty_user.get_perp_position(cx.market_index)?,
            PairReduceOnly {
                aggressor: order.reduce_only,
                counterparty: counterparty.reduce_only || market_is_reduce_only,
            },
        )?
    };

    Ok(RemainderPair {
        aggressor,
        counterparty,
        maker_key,
        base_filled,
        quote_filled: controller::orders::clob_notional(counterparty.price, base_filled)?,
    })
}

/// The counterparty's margin account, as the user map keys it.
fn counterparty_key(
    cx: &TakerOriginContext<'_, '_>,
    counterparty: &RestingOrder,
) -> Result<Pubkey> {
    let user = counterparty.user;
    Ok(*cx
        .makers_and_referrer
        .user_ref_index()?
        .get(&(user.authority, user.sub_account_id))
        .ok_or_else(|| {
            msg!(
                "counterparty {}/{} is not loaded",
                user.authority,
                user.sub_account_id
            );

            ErrorCode::UserNotFound
        })?)
}

/// Which side of a pair fills only up to the position it reduces.
struct PairReduceOnly {
    aggressor: bool,
    counterparty: bool,
}

/// The base a pair of remainders settles.
///
/// The aggressor's reservation bounds the book's report, and a report above it
/// fails. A reduce-only side fills only up to the position it reduces, so the
/// size shrinks to that cover instead of failing, the way a routed fill clamps
/// a reduce-only order. A long rests bids and reduces a short, so each cover is
/// the position held the other way to that side's order. A pair with nothing
/// left to settle fails.
fn pair_fill_size(
    cross_base: u64,
    taker_direction: PositionDirection,
    aggressor: &PerpPosition,
    counterparty: &PerpPosition,
    reduce_only: PairReduceOnly,
) -> Result<u64> {
    let reserved = aggressor.reserved_open_base(taker_direction);
    validate!(
        cross_base <= reserved,
        ErrorCode::QuoterReportExceedsReservation,
        "the book reported a {} base cross for a taker that reserved {}",
        cross_base,
        reserved
    )?;

    let cover = |reduces: bool, position: &PerpPosition, direction| {
        if reduces {
            crate::math::orders::reduce_only_cover(position.base_asset_amount, direction)
        } else {
            u64::MAX
        }
    };
    let base = cross_base
        .min(cover(reduce_only.aggressor, aggressor, taker_direction))
        .min(cover(
            reduce_only.counterparty,
            counterparty,
            taker_direction.opposite(),
        ));
    validate!(
        base > 0,
        ErrorCode::NoTakerOriginCross,
        "a reduce-only side of the pair has no position left to reduce"
    )?;

    Ok(base)
}

/// The oracle gates a routed fill applies before it matches.
///
/// A fill skips while the oracle has run too far from its 5-minute TWAP. A
/// party with an equity floor matches only while the raw exchange oracle
/// admits a match, because that oracle values the floor. The price comes off
/// the book's own rows, so the counterparty's price is held to the maker band
/// the router holds an external leg to.
fn admit_pair_match(
    cx: &TakerOriginContext<'_, '_>,
    pair: &RemainderPair<'_>,
    settled: &controller::orders::SettledMatch,
    maps: &AccountMaps,
) -> Result<()> {
    validate!(
        !settled.oracle_too_divergent_with_twap(cx.state)?,
        ErrorCode::PriceBandsBreached,
        "the oracle is too far from its 5-minute TWAP to match"
    )?;

    let floored = load!(cx.accounts.taker)?.equity_floor != 0
        || cx
            .makers_and_referrer
            .get_ref(&pair.maker_key)?
            .equity_floor
            != 0;
    validate!(
        !floored || settled.exchange_admits_match(),
        ErrorCode::InvalidOracle,
        "the exchange oracle does not admit a match for a floored party"
    )?;

    let margin_ratio_initial = maps
        .perp_market_map
        .get_ref(&cx.market_index)?
        .margin_ratio_initial;
    validate!(
        !crate::math::orders::limit_price_breaches_maker_oracle_price_bands(
            pair.counterparty.price,
            cx.taker_direction.opposite(),
            settled.oracle_price(),
            margin_ratio_initial,
        )?,
        ErrorCode::QuoterFillOffQuote,
        "the book rested the counterparty at {}, outside the oracle band around {}",
        pair.counterparty.price,
        settled.oracle_price()
    )?;

    Ok(())
}

/// What the post-fill checks need to know about the aggressor before the
/// match moves its position.
fn pair_fill_facts(
    cx: &TakerOriginContext<'_, '_>,
    order: &crate::state::user::Order,
    pricing: &controller::orders::TakerOriginCrossPricing,
) -> Result<PairFillFacts> {
    let taker = load!(cx.accounts.taker)?;
    Ok(PairFillFacts {
        aggressor_order_decreasing:
            controller::orders::determine_if_user_order_is_position_decreasing(
                &taker,
                cx.market_index,
                order,
            )?,
        aggressor_is_isolated: taker
            .get_perp_position(cx.market_index)
            .map(|position| position.is_isolated())
            .unwrap_or(false),
        perp_market_oi_before: pricing.perp_market_oi_before,
        oracle_stale_for_margin: pricing.oracle_stale_for_margin,
    })
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
    rev_share_escrow: &mut Option<RevenueShareEscrowZeroCopyMut<'info>>,
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
    let mut escrow_ref = rev_share_escrow.as_mut();
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
            effective_taker_limit: pair.aggressor.price,
            oracle_price,
        },
        &mut controller::orders::FillerSide {
            user: &mut none_filler,
            stats: &mut none_filler_stats,
            key: taker_key,
            rev_share_escrow: &mut escrow_ref,
        },
        &mut controller::orders::SettleContext {
            market: market.deref_mut(),
            rules: &controller::orders::PricingRules::for_settlement(
                cx.state,
                cx.referrer_is_accelerated,
            ),
            // This crank settles a cross, never a liquidation.
            mode: crate::state::fill_mode::FillMode::Fill,
            oracle_map,
            now: cx.clock.unix_timestamp,
            slot: cx.clock.slot,
            filler_reward_paid: &mut filler_reward_paid,
        },
    )?;

    // The settlement binds no filler, so a reward here would be quote nobody
    // was debited for.
    validate!(
        filler_reward_paid == 0,
        ErrorCode::DefaultError,
        "a pair settlement paid a filler reward of {}",
        filler_reward_paid
    )?;

    market.market_stats.update_volume_24h(
        pair.quote_filled,
        taker_direction,
        cx.clock.unix_timestamp,
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

/// Stage this crank for the book's taker-origin cross, if it has one.
///
/// This is the discovery half of [`handle_crank_taker_origin_cross`]. The cross
/// conditions' resolver calls it ahead of the maker-against-maker cross. The
/// module doc covers the wakes that find this work and its payment floor.
pub(super) fn stage_taker_origin_cross(
    ctx: &Context<ResolveClobCrank>,
) -> Result<Option<TakerOriginStage>> {
    let market_index = ctx.accounts.crank_conditions.load()?.market_index;
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
    let BookSides { bids, asks } = book_l3_sides(
        &book_slot,
        &ctx.accounts.quoter_slab,
        market_index,
        MAX_CROSS_ROWS,
        &book_accounts,
        &mut cpi_scratch,
        true,
        RestingOrder::from_row,
    )?
    .ok_or(ErrorCode::NoTakerOriginCross)?;

    let Some((cross, side)) = stageable_cross(&bids, &asks) else {
        return Ok(None);
    };

    // The protocol `User` is the filler on this path, and the crank loads each
    // margin account once, so it cannot also be a side of the cross it
    // resolves.
    let (protocol_user, _) = pdas::protocol_user_pair();
    let taker_ref = aggressor_of(&cross, side).user;
    let counterparty_ref = counterparty_of(&cross, side).user;
    if pdas::user(&taker_ref.authority, taker_ref.sub_account_id) == protocol_user
        || pdas::user(&counterparty_ref.authority, counterparty_ref.sub_account_id) == protocol_user
    {
        return Ok(None);
    }

    Ok(Some(TakerOriginStage {
        call: taker_origin_call(
            ctx,
            taker_ref,
            counterparty_ref,
            cross_read_depth(&bids, &asks, &cross),
        )?,
        stalled: cross_stalled(&cross, Clock::get()?.slot),
    }))
}

/// Whether a cross stayed on the book past `STALLED_TAKER_ORIGIN_CROSS_SLOTS`.
/// It formed no earlier than the later of its two rows rested.
fn cross_stalled(cross: &Cross, slot: u64) -> bool {
    let formed_no_earlier_than = cross.bid.placed_slot.max(cross.ask.placed_slot);
    slot.saturating_sub(formed_no_earlier_than)
        > super::crank_cross_match::STALLED_TAKER_ORIGIN_CROSS_SLOTS
}

/// A staged taker-origin crank, and whether its cross has stalled on the book.
pub(super) struct TakerOriginStage {
    pub call: StagedCall,
    /// The later of the two rows rested more than
    /// `STALLED_TAKER_ORIGIN_CROSS_SLOTS` ago, so a maker cross may go first.
    pub stalled: bool,
}

/// The executor call for one taker-origin cross.
///
/// Both `(User, UserStats)` pairs derive from the nodes' own
/// `(authority, sub_account_id)`, which the book stores for this use. The
/// taker's escrow derives from its authority. A crank that needs it and lacks
/// it fails, and a taker with no escrow leaves the account unread.
fn taker_origin_call(
    ctx: &Context<ResolveClobCrank>,
    taker_ref: UserRefV0,
    counterparty_ref: UserRefV0,
    cross_rows: u16,
) -> Result<StagedCall> {
    let (market_index, oracle, quote_spot_market_index) = {
        let conditions = ctx.accounts.crank_conditions.load()?;
        (
            conditions.market_index,
            conditions.oracle,
            conditions.quote_spot_market_index,
        )
    };
    let (protocol_user, protocol_user_stats) = pdas::protocol_user_pair();
    let (taker, taker_stats) = pdas::user_pair(&taker_ref.authority, taker_ref.sub_account_id);
    let call = crate::staged_call!(CrankTakerOriginCross {
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
    .map_section(oracle, quote_spot_market_index, market_index)
    .maker_refs([counterparty_ref]);
    let call = if ctx.accounts.state.load()?.builder_codes_enabled() {
        call.account(revenue_share_escrow(&taker_ref.authority), true)
    } else {
        call
    };

    // The quoter tail. The crank routes the remainder like any other fill, and
    // every router fill carries the market's slab and consults its book. The
    // resolver stages no quoters of its own, so it claims no route. A keeper
    // that wants a taker's custom quoters consulted builds the call itself.
    call.account(ctx.accounts.quoter_slab.key(), false)
        .account(ctx.accounts.clob_market.key(), true)
        .account(crate::ids::clob_program::id(), false)
        .arg(CrankTakerOriginCrossArgs {
            market_index,
            cross_rows,
            signed_route: Vec::new(),
        })
}

/// The taker's `RevenueShareEscrow` PDA.
fn revenue_share_escrow(authority: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            crate::state::revenue_share::REVENUE_SHARE_ESCROW_PDA_SEED.as_bytes(),
            authority.as_ref(),
        ],
        &crate::ID,
    )
    .0
}

/// The cross a resolver may stage, if the book has one.
///
/// Price priority decides which crank owns the front of a book. A
/// maker-against-maker cross ahead of a remainder is `crank_cross_match`'s work,
/// and clearing it brings the remainder to the front. The two cranks therefore
/// run in turn instead of competing for the same book.
fn stageable_cross(bids: &[RestingOrder], asks: &[RestingOrder]) -> Option<(Cross, SideV0)> {
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
