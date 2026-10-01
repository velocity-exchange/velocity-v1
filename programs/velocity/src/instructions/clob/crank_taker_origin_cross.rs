//! `crank_taker_origin_cross`: route one resting taker remainder.
//!
//! A migrated taker remainder rests on the book with the taker-origin flag. It
//! rests at the worst price its signer agreed to tolerate. The book withholds
//! it from every ordinary fill, before and after its claim lapses, so the
//! improvement between that price and whatever crosses it cannot be won by
//! landing a transaction at the activation slot. This crank hands that
//! improvement to the taker. It is permissionless and is paid out of the
//! improvement it delivers.
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
//! - Price and time decide between two remainders. The book withholds both
//!   from every ordinary fill, so this crank settles the pair itself at the
//!   earlier one's price. A subject that rested before the remainder it
//!   crosses is the maker of the pair, and the improvement is not its to take.
//!
//! The subject is an argument, not something this crank discovers. What crosses
//! a resting remainder is usually a quote ladder rather than another book
//! order, and a book cannot report that. Finding work to stage is the
//! resolver's job. See [`stage_taker_origin_cross`], which the cross
//! conditions' resolver reaches. This crank has no condition of its own.
//!
//! A remainder that no book row crosses still routes. So does one that waits
//! behind an older claim on its side. That fill honours every live claim, so it
//! reaches the vAMM, the quoters and the depth nobody claims. Without it, a
//! remainder that only the vAMM crosses rests at its worst price, and anyone
//! who rests a crossing order takes it there. For the same reason, two
//! remainders settle as a pair only when the vAMM can fill the earlier one and
//! beats the counterparty's price for neither side. A remainder whose claim
//! lapsed claims nothing, so the cross walk reads it as depth. Two lapsed
//! remainders at the front still settle as a pair, because the book withholds
//! each one from a claim-honouring route of the other.
//!
//! `crank_cross_match` middles two crossed makers for the protocol. This crank
//! does not. One side is the aggressor by construction, the improvement belongs
//! to it, and the only cut anyone takes is the cranker's reward.
//!
//! A row may only reduce when it is reduce-only or its market is `ReduceOnly`.
//! Such a row can never fill while its owner holds nothing to reduce, but the
//! book still reports it at full size. As the oldest claimant on its side,
//! it holds back every newer remainder there. The crank cancels such a row on
//! either side of the cross instead of routing it. The owner pays the flat
//! removal fee. In program-keeper mode the fee rises to the keeper payment's
//! value, so the reservoir pays no more than the crank collects.
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
//! The cross conditions' `min_payment` is in lamports, because relay measures
//! lamports and `assert_paid_v0` watches the payout account's balance. It is
//! the cheaper of the market's `cross` and `taker_origin_cross` payments,
//! because one condition stages both crosses. The quote-denominated crank
//! reward can be zero, because a dust or equal-price improvement resolves for
//! free by design. The reservoir pays the
//! lamports only when what the crank collected covers their value in quote, so
//! the protocol never pays more for a crank than it collects. A crank that
//! collected less charges the taker the shortfall, because an unpaid cross is
//! one relay never lands, and it holds back every newer remainder on its side.
//! The charge is capped by what the fill gained the taker against its rest
//! price, so the taker never ends worse than resting.
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
            events::{OrderActionExplanation, TakerOriginCrossRecordV1},
            fill_mode::FillMode,
            order_params::NO_ROUTE_DIGEST,
            pdas,
            perp_market_map::{get_writable_perp_market_set, MarketSet, PerpMarketMap},
            prop_amm::{
                CancelOrderArgsV0, ClobMarket, ClobReader, FillArgsV0, FillRequestV0,
                QuoterSlabExt, SideV0, UserRefV0,
            },
            revenue_share::RevenueShareEscrowZeroCopyMut,
            signed_msg_user::{SignedMsgUserOrdersLoader, SIGNED_MSG_PDA_SEED},
            state::State,
            user::{
                MarketType, OrderBitFlag, OrderReservation, OrderStatus, PerpPosition,
                ReferrerStatus, ReleaseCheck, User, UserStats,
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
    /// How deep to read each side of the book, up to `MAX_CROSS_ROWS`. The
    /// crank refuses a read that ends on a row crossing the other side,
    /// because the rows behind it can hold an older claimant.
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
    // The taker's escrow, at the PDA the tail must carry. The referee discount and
    // the referrer reward are keyed by market, so they bind here. The builder
    // row is keyed by order, so it binds to the remainder the crank fills.
    let mut rev_share_escrow = if state.builder_codes_enabled() {
        let taker_authority = load!(ctx.accounts.taker)?.authority;
        let escrow = load_taker_escrow(remaining_accounts_iter, &taker_authority)?;

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
    let plan = resolve_subject(
        &ctx,
        &book_slot,
        market_index,
        cross_rows,
        taker_ref,
        &mut cpi_scratch,
    )?;

    drop(book_slot);

    let fees_booked_before = booked_fee_remainder(&maps.perp_market_map, market_index)?;
    let cx = TakerOriginContext {
        accounts: &*ctx.accounts,
        market_index,
        taker_ref,
        taker_direction: PositionDirection::from(plan.side()),
        state: &state,
        makers_and_referrer: &makers_and_referrer,
        makers_and_referrer_stats: &makers_and_referrer_stats,
        clock: &clock,
        program_keeper_mode,
        referrer_is_accelerated,
    };

    let subject_order = plan.order();
    if let Some(row) = unfillable_row(&cx, &subject_order, plan.counterparty(), &maps)? {
        return cancel_unfillable_row(&cx, &row, &mut maps);
    }

    // Two crossed remainders are the one case the router cannot reach. The
    // book holds both back, because each is taker-origin and each is crossed
    // by the other, so the gate passes over whichever one a fill tries to
    // take. The improvement between their prices belongs to one of them, and
    // the resolution above worked out which.
    if let SubjectPlan::Cross(subject) = &plan {
        let resolution = if subject.counterparty.taker_origin {
            pair_resolution(
                pair_vamm_tops(&cx, &mut maps)?,
                subject.aggressor_side,
                subject.counterparty.price,
            )
        } else {
            PairResolution::RouteAggressor
        };

        validate!(
            resolution != PairResolution::CounterpartyRoutesFirst,
            ErrorCode::NoTakerOriginCross,
            "the vAMM fills the earlier remainder better than the pair price; it routes first"
        )?;

        validate!(
            resolution != PairResolution::Unpriced,
            ErrorCode::NoTakerOriginCross,
            "the vAMM cannot fill the earlier remainder, so the pair waits for it"
        )?;

        if resolution == PairResolution::Settle {
            return settle_taker_origin_pair(
                &cx,
                &subject.cross,
                &subject.order,
                &subject.counterparty,
                &mut maps,
                &mut rev_share_escrow,
                fees_booked_before,
            );
        }
    }

    let route_claim = SignedRouteClaim {
        quoters: &signed_route,
        digest: claimed_route_digest(
            signed_route_digest(
                &ctx.accounts.signed_msg_user_orders,
                market_index,
                subject_order.order_ref.order_id,
            )?,
            !plan.owns_its_claim(),
            signed_route.is_empty(),
        ),
    };

    let tail_from = ctx.remaining_accounts.len() - remaining_accounts_iter.len();
    let tail = &ctx.remaining_accounts[tail_from..];
    let controller::orders::FillAmounts {
        base: base_filled,
        quote: quote_filled,
    } = route_and_fill_remainder(
        &cx,
        tail,
        &RoutedRemainder {
            order: &subject_order,
            base: plan.route_base(),
            claim: &route_claim,
            include_taker_origin_reservations: plan.owns_its_claim(),
        },
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

    pay_crank_lamports(&cx, &mut maps, fees_booked_before, &fee, crank_reward)?;
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
    /// The base the fill may route. See [`base_ahead_of_opposite_remainder`].
    route_base: u64,
    /// The subject holds the first live claim on the counterparty. A pair of
    /// remainders whose claims both lapsed claims nothing.
    owns_claim: bool,
}

/// One book row, and whether the book still honours its claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BookRow {
    order: RestingOrder,
    claim_lapsed: bool,
}

impl BookRow {
    fn from_row(row: &crate::state::prop_amm::L3RowV0) -> Self {
        Self {
            order: RestingOrder::from_row(row),
            claim_lapsed: row.flags & quoter_spec::L3_ROW_FLAG_CLAIM_LAPSED != 0,
        }
    }
}

/// The rows as the book ranks claims. A remainder whose claim lapsed is still
/// depth, but it claims nothing, so the cross walk reads it as a maker.
fn claim_view(rows: &[BookRow]) -> Vec<RestingOrder> {
    rows.iter()
        .map(|row| RestingOrder {
            taker_origin: row.order.taker_origin && !row.claim_lapsed,
            ..row.order
        })
        .collect()
}

/// What one crank does with the taker's remainder.
enum SubjectPlan {
    /// The remainder holds the first live claim on its side, on a book row it
    /// crosses. The fill reads the book with every claim ignored.
    Cross(Box<SubjectCross>),
    /// No book cross is the remainder's to settle. The fill honours every live
    /// claim, so it reaches the vAMM, the quoters and unclaimed depth only.
    Route { order: RestingOrder, side: SideV0 },
}

impl SubjectPlan {
    fn side(&self) -> SideV0 {
        match self {
            Self::Cross(subject) => subject.aggressor_side,
            Self::Route { side, .. } => *side,
        }
    }

    fn order(&self) -> RestingOrder {
        match self {
            Self::Cross(subject) => subject.order,
            Self::Route { order, .. } => *order,
        }
    }

    /// The base the fill may route.
    fn route_base(&self) -> u64 {
        match self {
            Self::Cross(subject) => subject.route_base,
            Self::Route { order, .. } => order.base_asset_amount,
        }
    }

    fn counterparty(&self) -> Option<&RestingOrder> {
        match self {
            Self::Cross(subject) => Some(&subject.counterparty),
            Self::Route { .. } => None,
        }
    }

    /// Whether the fill may take the depth the book reserves for the claim.
    fn owns_its_claim(&self) -> bool {
        matches!(self, Self::Cross(subject) if subject.owns_claim)
    }
}

/// Read both sides of the book and work out what to settle.
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
fn resolve_subject<'info>(
    ctx: &Context<'info, CrankTakerOriginCross<'info>>,
    book_slot: &crate::state::prop_amm::QuoterSlotV0,
    market_index: u16,
    cross_rows: u16,
    taker_ref: UserRefV0,
    cpi_scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
) -> Result<SubjectPlan> {
    let rows_read = cross_rows.min(MAX_CROSS_ROWS);
    let BookSides { bids, asks } = book_l3_sides(
        book_slot,
        &ctx.accounts.quoter_slab,
        market_index,
        rows_read,
        &[
            ctx.accounts.clob_market.to_account_info(),
            ctx.accounts.clob_program.to_account_info(),
        ],
        cpi_scratch,
        true,
        BookRow::from_row,
    )?
    .ok_or(ErrorCode::NoTakerOriginCross)?;

    validate!(
        read_shows_every_crossing_row(&bids, &asks, rows_read),
        ErrorCode::NoTakerOriginCross,
        "a read of {} rows per side ends on a row that crosses the other side",
        rows_read
    )?;

    Ok(plan_subject(&bids, &asks, taker_ref)?)
}

/// Whether a read of `rows_read` rows per side shows every row that crosses
/// the other side. Rows behind a full side rest at worse prices, so none of
/// them crosses when its last row does not. A read at `MAX_CROSS_ROWS` is
/// accepted, because the resolver reads no deeper.
fn read_shows_every_crossing_row(bids: &[BookRow], asks: &[BookRow], rows_read: u16) -> bool {
    if rows_read >= MAX_CROSS_ROWS {
        return true;
    }

    let side_shown = |rows: &[BookRow], side: SideV0, opposite: &[BookRow]| {
        rows.len() < usize::from(rows_read)
            || rows
                .last()
                .zip(opposite.first())
                .is_none_or(|(last, best)| !price_crosses(side, last.order.price, best.order.price))
    };

    side_shown(bids, SideV0::Bid, asks) && side_shown(asks, SideV0::Ask, bids)
}

/// Whether an order at `price` on `side` crosses one at `opposite_price`.
fn price_crosses(side: SideV0, price: u64, opposite_price: u64) -> bool {
    match side {
        SideV0::Bid => price >= opposite_price,
        SideV0::Ask => price <= opposite_price,
    }
}

/// Rows the crank must read per side to see every row that crosses the other
/// side, and one more to show where the crossing rows end.
fn crossing_read_depth(bids: &[BookRow], asks: &[BookRow]) -> u16 {
    let crossing_rows = |rows: &[BookRow], side: SideV0, opposite: &[BookRow]| {
        rows.iter()
            .take_while(|row| {
                opposite
                    .first()
                    .is_some_and(|best| price_crosses(side, row.order.price, best.order.price))
            })
            .count()
    };
    let deepest =
        crossing_rows(bids, SideV0::Bid, asks).max(crossing_rows(asks, SideV0::Ask, bids));

    (deepest.min(usize::from(MAX_CROSS_ROWS)) as u16)
        .saturating_add(1)
        .min(MAX_CROSS_ROWS)
}

/// The plan for the taker's remainder on this book.
///
/// A remainder that holds the first live claim on a book row it crosses
/// settles that cross. Any other remainder routes with every live claim
/// honoured. That is how a remainder that no book row crosses reaches the
/// vAMM, and how a newer remainder behind an older claim still fills against
/// depth nobody claims.
fn plan_subject(
    bids: &[BookRow],
    asks: &[BookRow],
    taker_ref: UserRefV0,
) -> VelocityResult<SubjectPlan> {
    if let Ok(subject) = claimed_subject_cross(bids, asks, taker_ref) {
        return Ok(SubjectPlan::Cross(Box::new(subject)));
    }

    if let Some(subject) = lapsed_pair_subject(bids, asks, taker_ref) {
        return Ok(SubjectPlan::Cross(Box::new(subject)));
    }

    let owned = |rows: &[BookRow], side: SideV0| {
        rows.iter()
            .find(|row| row.order.taker_origin && row.order.user == taker_ref)
            .map(|row| SubjectPlan::Route {
                order: row.order,
                side,
            })
    };

    owned(bids, SideV0::Bid)
        .or_else(|| owned(asks, SideV0::Ask))
        .ok_or(ErrorCode::NoTakerOriginCross)
}

/// The cross of the two heads, when both are lapsed remainders, and its
/// aggressor's side. The book withholds each one from a claim-honouring route
/// of the other.
fn lapsed_pair_at_front(bids: &[BookRow], asks: &[BookRow]) -> Option<(Cross, SideV0)> {
    let lapsed_remainder = |row: &BookRow| row.order.taker_origin && row.claim_lapsed;
    let (bid, ask) = (bids.first()?, asks.first()?);
    if !lapsed_remainder(bid) || !lapsed_remainder(ask) {
        return None;
    }

    let cross = *resolve_crosses(&[bid.order], &[ask.order], 1).first()?;
    Some((cross, cross.kind.aggressor_side()?))
}

/// The lapsed pair at the front, when the taker's remainder aggresses it.
fn lapsed_pair_subject(
    bids: &[BookRow],
    asks: &[BookRow],
    taker_ref: UserRefV0,
) -> Option<SubjectCross> {
    let (cross, aggressor_side) = lapsed_pair_at_front(bids, asks)?;
    let order = aggressor_of(&cross, aggressor_side);
    (order.user == taker_ref).then(|| SubjectCross {
        aggressor_side,
        cross,
        order,
        counterparty: counterparty_of(&cross, aggressor_side),
        route_base: order.base_asset_amount,
        owns_claim: false,
    })
}

/// The cross the taker's remainder settles with its claim, when it has one.
///
/// Price priority decides which crank owns the front of a book. This
/// instruction is permissionless, so it enforces the rule as the resolver does.
fn claimed_subject_cross(
    bids: &[BookRow],
    asks: &[BookRow],
    taker_ref: UserRefV0,
) -> VelocityResult<SubjectCross> {
    validate!(
        front_is_taker_origin(bids, asks),
        ErrorCode::NoTakerOriginCross,
        "the front of the book is a maker cross; crank_cross_match resolves it first"
    )?;

    let (bid_view, ask_view) = (claim_view(bids), claim_view(asks));
    let crosses = resolve_crosses(&bid_view, &ask_view, MAX_CROSSES_PER_CRANK);
    let mut subject = subject_cross(&crosses, taker_ref)?;
    if !subject.counterparty.taker_origin {
        let opposite = match subject.aggressor_side {
            SideV0::Bid => &ask_view,
            SideV0::Ask => &bid_view,
        };
        subject.route_base = subject.route_base.min(base_ahead_of_opposite_remainder(
            &subject.order,
            subject.aggressor_side,
            opposite,
        ));
    }

    Ok(subject)
}

/// Whether either best row is a taker remainder. A lapsed one counts, because
/// `crank_cross_match` never middles a remainder.
fn front_is_taker_origin(bids: &[BookRow], asks: &[BookRow]) -> bool {
    match (bids.first(), asks.first()) {
        (Some(best_bid), Some(best_ask)) => {
            best_bid.order.taker_origin || best_ask.order.taker_origin
        }
        _ => false,
    }
}

/// The first cross in `crosses` that `taker_ref` aggresses.
///
/// This crank settles the remainder whose accounts the transaction carries.
/// The rest of the pass is another crank's work, which bounds one transaction's
/// account list.
///
/// The fill reads the book with every claim ignored and takes the whole
/// remainder. The book serves claims oldest first, so only the oldest crossing
/// remainder on a side may do that. A newer one waits until the older one
/// leaves, or else it takes depth the book reserves for the older one.
fn subject_cross(crosses: &[Cross], taker_ref: UserRefV0) -> VelocityResult<SubjectCross> {
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

    let first_claimant = crosses
        .iter()
        .find(|cross| cross.kind.aggressor_side() == Some(aggressor_side))
        .map(|cross| aggressor_of(cross, aggressor_side).order_ref);
    validate!(
        first_claimant == Some(subject_order.order_ref),
        ErrorCode::NoTakerOriginCross,
        "an older remainder on this side holds the first claim on the depth it crosses"
    )?;

    Ok(SubjectCross {
        aggressor_side,
        cross: subject,
        order: subject_order,
        counterparty: counterparty_of(&subject, aggressor_side),
        route_base: subject_order.base_asset_amount,
        owns_claim: true,
    })
}

/// The crossing base on the other side that rests in front of the first live
/// remainder there. `u64::MAX` when no such remainder crosses the subject.
///
/// A Cross plan fills with every claim ignored, so it would take that
/// remainder at the remainder's own worst price. The two remainders are a pair
/// instead, and a pair settles at the earlier one's price. The fill stops in
/// front of it, and the pair is the next crank's work.
fn base_ahead_of_opposite_remainder(
    subject: &RestingOrder,
    side: SideV0,
    opposite: &[RestingOrder],
) -> u64 {
    let mut base_ahead = 0u64;
    for row in opposite {
        if !price_crosses(side, subject.price, row.price) {
            break;
        }

        if crate::math::crosses::same_authority(&subject.user, &row.user) {
            continue;
        }

        if row.taker_origin {
            return base_ahead;
        }

        base_ahead = base_ahead.saturating_add(row.base_asset_amount);
    }

    u64::MAX
}

/// A row of the subject cross that can never fill.
struct UnfillableRow<'a> {
    order: &'a RestingOrder,
    /// The owner's margin account, as the user map keys it. `None` is the
    /// taker.
    owner: Option<Pubkey>,
}

/// Whether a row that fills only in the reduce direction has nothing left to
/// reduce. The book reports such a row at its full size, but every fill of it
/// clamps to zero.
fn has_nothing_to_reduce(
    reduces_only: bool,
    position_base: i64,
    direction: PositionDirection,
) -> bool {
    reduces_only && crate::math::orders::reduce_only_cover(position_base, direction) == 0
}

/// The row of the subject cross that can never fill, if there is one.
///
/// A `ReduceOnly` market holds every row to the reduce direction, as a routed
/// fill does. A counterparty whose owner the transaction does not carry is
/// left to the fill.
fn unfillable_row<'a>(
    cx: &TakerOriginContext<'_, '_>,
    subject: &'a RestingOrder,
    counterparty: Option<&'a RestingOrder>,
    maps: &AccountMaps,
) -> Result<Option<UnfillableRow<'a>>> {
    let market_is_reduce_only = maps
        .perp_market_map
        .get_ref(&cx.market_index)?
        .is_reduce_only()?;
    let position_base = |user: &User| {
        user.get_perp_position(cx.market_index)
            .map(|position| position.base_asset_amount)
            .unwrap_or(0)
    };

    let taker_base = position_base(&*load!(cx.accounts.taker)?);
    if has_nothing_to_reduce(
        subject.reduce_only || market_is_reduce_only,
        taker_base,
        cx.taker_direction,
    ) {
        return Ok(Some(UnfillableRow {
            order: subject,
            owner: None,
        }));
    }

    let Some(counterparty) = counterparty else {
        return Ok(None);
    };

    if !counterparty.reduce_only && !market_is_reduce_only {
        return Ok(None);
    }

    let user = counterparty.user;
    let Some(key) = cx
        .makers_and_referrer
        .user_ref_index()?
        .get(&(user.authority, user.sub_account_id))
        .copied()
    else {
        return Ok(None);
    };

    let counterparty_base = position_base(&*cx.makers_and_referrer.get_ref(&key)?);
    Ok(
        has_nothing_to_reduce(true, counterparty_base, cx.taker_direction.opposite()).then_some(
            UnfillableRow {
                order: counterparty,
                owner: Some(key),
            },
        ),
    )
}

/// What removing an unfillable row charges its owner: the flat fee, raised to
/// the keeper payment's value in quote when the reservoir pays one.
fn unfillable_row_fee(flat_filler_fee: u64, payment_quote: Option<u64>) -> u64 {
    payment_quote.map_or(flat_filler_fee, |payment_quote| {
        payment_quote.max(flat_filler_fee)
    })
}

/// Remove a row that can never fill, and charge its owner for the crank.
///
/// Such a row can be the oldest claimant on its side, and every crank of its
/// cross reverts. Nothing else removes it while it stays near the market, so
/// this crank does.
fn cancel_unfillable_row<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    row: &UnfillableRow<'_>,
    maps: &mut AccountMaps<'info>,
) -> Result<()> {
    let payment_lamports = match (cx.program_keeper_mode, &cx.accounts.crank_conditions) {
        (true, Some(conditions)) => Some(u64::from(
            conditions.load()?.crank_payments.taker_origin_cross,
        )),
        _ => None,
    };
    let payment_quote =
        payment_lamports.and_then(|lamports| taker_origin_payment_quote(cx.state, maps, lamports));

    // A bound remainder refuses a plain cancel while its claim holds.
    let removed = ClobMarket::from_slab(
        &cx.accounts.quoter_slab,
        cx.market_index,
        &cx.accounts.clob_market,
        &cx.accounts.clob_program,
    )?
    .cancel(CancelOrderArgsV0 {
        order_ref: row.order.order_ref,
        user: row.order.user,
        force: true,
    })?;
    validate!(
        removed.user == row.order.user,
        ErrorCode::InvalidUserAccount,
        "the book cancelled an order of another user"
    )?;

    let charged = close_unfillable_row(
        cx,
        row,
        &removed,
        unfillable_row_fee(cx.state.perp_fee_structure.flat_filler_fee, payment_quote),
        maps,
    )?;

    if row.owner.is_none() {
        cx.release_taker_route(removed.order_id);
    }

    if let (Some(lamports), Some(conditions)) = (payment_lamports, &cx.accounts.crank_conditions) {
        if super::helpers::earns_crank_lamports(
            charged.fee,
            &cx.accounts.authority.key(),
            &charged.owner_authority,
        ) && collected_covers_payment(charged.fee, payment_quote)
        {
            ClobCrankConditionsV0::pay_keeper(
                conditions,
                &cx.accounts.authority.to_account_info(),
                lamports,
            )?;
        }
    }

    Ok(())
}

/// What the owner of a removed row paid.
struct UnfillableRowCharge {
    fee: u64,
    owner_authority: Pubkey,
}

/// Charge the owner the fee, then unwind the removed row and record it. The
/// fee comes first, because unwinding an otherwise-empty position frees the
/// slot the fee resolves in.
fn close_unfillable_row(
    cx: &TakerOriginContext<'_, '_>,
    row: &UnfillableRow<'_>,
    removed: &crate::state::prop_amm::RemovedOrderV0,
    fee: u64,
    maps: &AccountMaps,
) -> Result<UnfillableRowCharge> {
    let (mut owner, owner_key) = match row.owner {
        None => (load_mut!(cx.accounts.taker)?, cx.accounts.taker.key()),
        Some(key) => (cx.makers_and_referrer.get_ref_mut(&key)?, key),
    };
    let mut market = maps.perp_market_map.get_ref_mut(&cx.market_index)?;

    let fee = {
        let mut filler = load_mut!(cx.accounts.filler)?;
        if cranker_earns_reward(&filler, &owner.authority)? {
            controller::orders::pay_keeper_flat_reward_for_perps(
                &mut owner,
                Some(&mut filler),
                &mut market,
                fee,
                cx.clock.slot,
            )?
        } else {
            0
        }
    };

    let position_index = owner.close_book_order(
        &OrderReservation::book_order(
            cx.market_index,
            PositionDirection::from(removed.side),
            removed.base_asset_amount,
            removed.reduce_only,
        ),
        ReleaseCheck::ClampedForExit,
        removed.order_id,
        OrderStatus::Canceled,
    )?;

    super::helpers::emit_clob_cancel_record(
        cx.clock.unix_timestamp,
        market.market_stats.historical_oracle_data.last_oracle_price,
        &owner_key,
        super::helpers::ClobOrderFacts::from_removed(removed, cx.market_index, cx.clock.slot),
        OrderActionExplanation::ReduceOnlyOrderIncreasedPosition,
        Some(cx.accounts.filler.key()),
        Some(fee),
        owner.perp_positions[position_index].is_isolated(),
    )?;

    Ok(UnfillableRowCharge {
        fee,
        owner_authority: owner.authority,
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

/// The route claim the crank is held to. A fill that honours every claim
/// takes no claimed depth, so a crank that names no route may fill it across
/// the baseline. Relay cannot read the taker's record, so the digest would
/// fail every staged crank for the claim window.
fn claimed_route_digest(
    signed_digest: crate::state::order_params::RouteDigest,
    honours_every_claim: bool,
    claims_baseline: bool,
) -> crate::state::order_params::RouteDigest {
    if honours_every_claim && claims_baseline {
        return NO_ROUTE_DIGEST;
    }

    signed_digest
}

/// The remainder one routed fill takes, and how much of the book it reaches.
struct RoutedRemainder<'a> {
    order: &'a RestingOrder,
    /// The base this fill may take, at most the order's size.
    base: u64,
    claim: &'a SignedRouteClaim<'a>,
    /// True only for the first live claim on a book row, which may take the
    /// depth the book reserves for it.
    include_taker_origin_reservations: bool,
}

/// Fill the remainder the way anything else fills.
///
/// The order is a limit at the price it rested at, so the router can only fill
/// it at that price or better. Each leg fills at its own source's price, which
/// is how the improvement reaches the taker.
///
/// Returns the base and the quote the fill took.
fn route_and_fill_remainder<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    tail: &'info [AccountInfo<'info>],
    remainder: &RoutedRemainder<'_>,
    maps: &mut AccountMaps<'info>,
    cpi_scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
    rev_share_escrow: &mut Option<RevenueShareEscrowZeroCopyMut<'info>>,
) -> Result<controller::orders::FillAmounts> {
    // The order is a local. It came off a book and belongs to no `orders` slot,
    // so the fill takes it directly and the taker needs no spare slot. The
    // reservation from when the remainder rested is still on the position.
    let subject_order = remainder.order;
    let mut order =
        controller::orders::taker_origin_order(cx.market_index, cx.taker_direction, subject_order);
    // The fill targets the order's own size, so the bound goes on the order.
    order.base_asset_amount = remainder.base;
    bind_builder_order(cx, rev_share_escrow, subject_order, &mut order)?;

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
                unfilled: remainder.base,
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
            include_taker_origin_reservations: remainder.include_taker_origin_reservations,
            claim: Some(crate::instructions::RouteClaim {
                quoters: remainder.claim.quoters,
                digest: remainder.claim.digest,
            }),
            // The taker is not here to choose the account list, so a signed
            // keeper answers for what it left out. Relay stages a fixed maker
            // count, so a program-keeper crank stops short at the makers it
            // carries instead.
            filler: crate::instructions::FillerTerms {
                stops_at_carried_makers: cx.program_keeper_mode,
                ..crate::instructions::FillerTerms::keeper(Some(
                    &cx.accounts.instructions_sysvar.to_account_info(),
                ))?
            },
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

/// Carry the taker's builder terms onto the order the crank fills.
///
/// The escrow keys a builder row by velocity order id, and a book row names
/// the book's handle instead. The book keeps the velocity id as the order's
/// `client_order_id`, so the crank reads it back. A live row for that id,
/// sub-account and market is this order's own. Velocity order ids only
/// increase, and a placement that stops early clears the row it wrote.
fn bind_builder_order<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    rev_share_escrow: &Option<RevenueShareEscrowZeroCopyMut<'info>>,
    resting: &RestingOrder,
    order: &mut crate::state::user::Order,
) -> Result<()> {
    let Some(escrow) = rev_share_escrow.as_ref() else {
        return Ok(());
    };

    let Some(view) = ClobReader {
        market: &cx.accounts.clob_market,
        program: &cx.accounts.clob_program,
    }
    .orders(vec![resting.order_ref])?
    .first()
    .copied()
    .filter(|view| view.found()) else {
        return Ok(());
    };

    let builder_row = escrow.find_builder_order_index(
        cx.taker_ref.sub_account_id,
        view.client_order_id,
        cx.market_index,
        MarketType::Perp,
    );

    apply_builder_row(order, view.client_order_id, builder_row.is_some());
    Ok(())
}

/// Name the order by its velocity id and flag its builder, so the fill finds
/// the escrow row and charges the builder fee. An order with no row is left
/// as the book named it.
fn apply_builder_row(
    order: &mut crate::state::user::Order,
    velocity_order_id: u32,
    has_builder_row: bool,
) {
    if !has_builder_row {
        return;
    }

    order.order_id = velocity_order_id;
    order.add_bit_flag(OrderBitFlag::HasBuilder);
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

/// The taker's escrow, from the account the tail must carry at its PDA.
///
/// The PDA is required whether or not it exists, so a caller cannot drop the
/// builder fee by leaving the escrow out. A PDA nobody created arrives empty
/// and reads as no escrow.
fn load_taker_escrow<'a>(
    remaining_accounts_iter: &mut std::iter::Peekable<std::slice::Iter<'a, AccountInfo<'a>>>,
    taker_authority: &Pubkey,
) -> Result<Option<RevenueShareEscrowZeroCopyMut<'a>>> {
    let expected = revenue_share_escrow(taker_authority);
    validate!(
        remaining_accounts_iter
            .peek()
            .is_some_and(|account| account.key() == expected),
        ErrorCode::UnableToLoadRevenueShareAccount,
        "the tail must carry the taker's RevenueShareEscrow PDA {}",
        expected
    )?;

    let escrow = crate::instructions::optional_accounts::get_revenue_share_escrow_account(
        remaining_accounts_iter,
        taker_authority,
    )?;
    if escrow.is_none() {
        remaining_accounts_iter.next();
    }

    Ok(escrow)
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
/// The pre-flight's own mm-oracle price is dropped. The match uses the plain
/// oracle price, as the router pass does.
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
            // A leg the fill consumed whole filled. A leg the book culled
            // under its minimum was cancelled, as the router fill records it.
            if leg.culled_base_asset_amount == 0 {
                OrderStatus::Filled
            } else {
                OrderStatus::Canceled
            },
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

/// The fee remainder the market's ledger has booked: the protocol's, the
/// insurance fund's and the AMM's cuts of every taker fee, net of the maker
/// rebate, the referral and the filler reward.
fn booked_fee_remainder(perp_market_map: &PerpMarketMap, market_index: u16) -> Result<u128> {
    let market = perp_market_map.get_ref(&market_index)?;
    let ledger = &market.fee_ledger;
    Ok(ledger
        .pending_protocol_fee
        .saturating_add(ledger.pending_if_fee)
        .saturating_add(ledger.amm_protocol_fees_received))
}

/// The keeper's lamports in program-keeper mode, for a crank that collected
/// at least their value.
///
/// What the crank collected is the fee remainder its fill booked plus the
/// crank reward, which lands in the protocol `User`. Two wallets can cross
/// each other at one price for no reward, so without the floor each such
/// crank draws lamports that nothing paid for. A crank that collected less
/// charges the taker the shortfall, as a removal charges its owner. Relay
/// asserts the payment, so an unpaid cross would stall every cross behind it.
/// The charge never exceeds what the fill left the taker under its rest
/// price, so many small cranks cannot charge it past that price.
fn pay_crank_lamports<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    maps: &mut AccountMaps,
    fees_booked_before: u128,
    fee: &crate::math::fees::TakerOriginCrossFee,
    crank_reward: u64,
) -> Result<()> {
    let (true, Some(conditions)) = (cx.program_keeper_mode, &cx.accounts.crank_conditions) else {
        return Ok(());
    };

    let payment_lamports = u64::from(conditions.load()?.crank_payments.taker_origin_cross);
    let payment_quote = taker_origin_payment_quote(cx.state, maps, payment_lamports);
    let booked = booked_fee_remainder(&maps.perp_market_map, cx.market_index)?
        .saturating_sub(fees_booked_before)
        .saturating_add(u128::from(crank_reward))
        .min(u128::from(u64::MAX)) as u64;
    let shortfall = chargeable_shortfall(booked, payment_quote, fee.budget, crank_reward);
    let charged = if shortfall > 0 && cx.accounts.authority.key() != cx.taker_ref.authority {
        charge_payment_shortfall(cx, maps, shortfall)?
    } else {
        0
    };

    let collected = booked.saturating_add(charged);
    if !super::helpers::crank_common::earns_crank_lamports(
        collected,
        &cx.accounts.authority.key(),
        &cx.taker_ref.authority,
    ) {
        return Ok(());
    }

    if !collected_covers_payment(collected, payment_quote) {
        msg!(
            "crank collected {} quote against a keeper payment worth {:?}; the reservoir pays nothing",
            collected,
            payment_quote
        );

        return Ok(());
    }

    ClobCrankConditionsV0::pay_keeper(
        conditions,
        &cx.accounts.authority.to_account_info(),
        payment_lamports,
    )?;

    Ok(())
}

/// The quote a crank must still collect to cover the keeper payment. A
/// payment with no price is never paid, so it has no shortfall.
fn payment_shortfall(collected: u64, payment_quote: Option<u64>) -> u64 {
    payment_quote.map_or(0, |payment_quote| payment_quote.saturating_sub(collected))
}

/// The shortfall the taker is charged: at most what it gained against its
/// rest price, net of the crank reward. `budget` is that gain.
fn chargeable_shortfall(
    collected: u64,
    payment_quote: Option<u64>,
    budget: u64,
    crank_reward: u64,
) -> u64 {
    payment_shortfall(collected, payment_quote).min(budget.saturating_sub(crank_reward))
}

/// Charge the taker `shortfall` in quote, paid to the protocol `User` that
/// cranks. Returns what the charge moved. A protocol `User` with no free perp
/// position takes nothing.
fn charge_payment_shortfall(
    cx: &TakerOriginContext<'_, '_>,
    maps: &AccountMaps,
    shortfall: u64,
) -> Result<u64> {
    let mut taker = load_mut!(cx.accounts.taker)?;
    let mut filler = load_mut!(cx.accounts.filler)?;
    let mut market = maps.perp_market_map.get_ref_mut(&cx.market_index)?;
    Ok(controller::orders::pay_keeper_flat_reward_for_perps(
        &mut taker,
        Some(&mut filler),
        &mut market,
        shortfall,
        cx.clock.slot,
    )?)
}

/// Whether what a crank collected covers the keeper payment's value in quote.
/// A payment with no price to value it at is not covered.
fn collected_covers_payment(collected: u64, payment_quote: Option<u64>) -> bool {
    payment_quote.is_some_and(|payment_quote| collected >= payment_quote)
}

/// The keeper payment's value in quote, as the cross-match floor prices it.
/// A state with no SOL spot market has no price to convert at, so the
/// reservoir pays nothing.
fn taker_origin_payment_quote(
    state: &State,
    maps: &mut AccountMaps,
    payment_lamports: u64,
) -> Option<u64> {
    if state.sol_spot_market_index == 0 {
        return None;
    }

    crate::state::clob_crank::sol_price_for_payment_floor(
        state,
        &maps.spot_market_map,
        &mut maps.oracle_map,
    )
    .and_then(|sol_price| {
        crate::state::clob_crank::CrankPaymentsV0::lamports_to_quote(payment_lamports, sol_price)
    })
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

/// How a cross of two remainders resolves against the vAMM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PairResolution {
    /// The pair settles at the earlier remainder's price.
    Settle,
    /// The vAMM beats that price for the aggressor, which routes instead.
    RouteAggressor,
    /// The vAMM beats that price for the earlier remainder, whose own price
    /// it is. The earlier remainder routes first.
    CounterpartyRoutesFirst,
    /// The vAMM cannot fill the earlier remainder, so nothing shows that its
    /// worst price is fair. The pair waits until the vAMM can fill it.
    Unpriced,
}

/// Settle a pair only at a price the vAMM beats for neither side.
///
/// The pair price is the earlier remainder's worst price. Without this rule,
/// anyone who rests an unattested order through a remainder takes it there,
/// while the vAMM quotes it better. A vAMM paused or stopped by its fill gates
/// quotes nothing, which is no evidence that the price is fair.
fn pair_resolution(
    vamm: VammTops,
    aggressor_side: SideV0,
    counterparty_price: u64,
) -> PairResolution {
    let aggressor_direction = PositionDirection::from(aggressor_side);
    let counterparty_side = match aggressor_side {
        SideV0::Bid => SideV0::Ask,
        SideV0::Ask => SideV0::Bid,
    };

    if vamm.facing(counterparty_side).is_none() {
        return PairResolution::Unpriced;
    }

    if controller::orders::vamm_improves_on(
        vamm.facing(counterparty_side),
        aggressor_direction.opposite(),
        counterparty_price,
    ) {
        return PairResolution::CounterpartyRoutesFirst;
    }

    if controller::orders::vamm_improves_on(
        vamm.facing(aggressor_side),
        aggressor_direction,
        counterparty_price,
    ) {
        return PairResolution::RouteAggressor;
    }

    PairResolution::Settle
}

/// The vAMM's tops at the live oracle, for the pair decision.
fn pair_vamm_tops(cx: &TakerOriginContext<'_, '_>, maps: &mut AccountMaps) -> Result<VammTops> {
    let market = maps.perp_market_map.get_ref(&cx.market_index)?;
    let oracle_price_data = *maps.oracle_map.get_price_data(&market.oracle_id())?;
    let top = |direction| {
        controller::orders::vamm_top_price(
            &market,
            oracle_price_data,
            cx.state,
            cx.clock.slot,
            direction,
        )
    };

    Ok(VammTops {
        bid: top(PositionDirection::Short),
        ask: top(PositionDirection::Long),
    })
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
    fees_booked_before: u128,
) -> Result<()> {
    let mut order =
        controller::orders::taker_origin_order(cx.market_index, cx.taker_direction, aggressor);
    bind_builder_order(cx, rev_share_escrow, aggressor, &mut order)?;
    let pair = bind_pair(cx, cross, aggressor, counterparty, &mut order, maps)?;
    admit_pair_parties(
        &cx.accounts.taker,
        cx.makers_and_referrer,
        &pair.maker_key,
        cx.state.liquidation_margin_buffer_ratio,
        maps,
    )?;

    // The pre-flight comes first, because a refusal must leave the book as it
    // was. The facts it reports are inputs to the post-fill checks below.
    let pricing = price_cross(cx, aggressor, counterparty.price, pair.base_filled, maps)?;
    // Read before `SettledMatch::read` refreshes it, as a routed fill reads it.
    let entry_twap_5min = maps
        .perp_market_map
        .get_ref(&cx.market_index)?
        .market_stats
        .historical_oracle_data
        .last_oracle_price_twap_5min;
    let settled = controller::orders::SettledMatch::read(
        cx.state,
        maps,
        &cx.accounts.taker,
        &cx.accounts.taker_stats,
        &mut order,
        cx.clock,
    )?;

    let fill_price = admit_pair_match(cx, &pair, &settled, entry_twap_5min, maps)?;

    let facts = pair_fill_facts(cx, &order, &pricing)?;
    let builder_fee_allowed =
        pair_builder_fee_allowed(cx, &order, &facts, rev_share_escrow.is_some(), maps)?;
    settle_pair_funding(cx, &pair, &maps.perp_market_map)?;

    // The settlement returns evidence that the shared post-fill checks ran on
    // both legs. Nothing outside `post_checks` can build that evidence.
    let _checked = settle_pair_match(
        cx,
        &pair,
        &mut order,
        &PairPricing {
            oracle_price: settled.oracle_price(),
            fill_price,
            builder_fee_allowed,
        },
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

    pay_crank_lamports(cx, maps, fees_booked_before, &pricing.fee, crank_reward)?;
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

/// Refuse a pair whose parties a routed fill would not match.
///
/// A routed fill skips a bankrupt taker, and a taker still under liquidation
/// after a fresh margin check. It refuses a maker under liquidation. Without
/// these checks, an account under liquidation can trade back above maintenance
/// and avoid the liquidation penalty.
fn admit_pair_parties<'info>(
    taker_loader: &AccountLoader<'info, User>,
    makers_and_referrer: &UserMap<'info>,
    counterparty_key: &Pubkey,
    liquidation_margin_buffer_ratio: u32,
    maps: &mut AccountMaps<'info>,
) -> VelocityResult {
    {
        let mut taker = load_mut!(taker_loader)?;
        validate!(!taker.is_bankrupt(), ErrorCode::UserBankrupt)?;
        crate::math::liquidation::validate_user_not_being_liquidated(
            &mut taker,
            maps,
            liquidation_margin_buffer_ratio,
        )?;
    }

    let counterparty = makers_and_referrer.get_ref(counterparty_key)?;
    validate!(
        !counterparty.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "counterparty {} is bankrupt",
        counterparty_key
    )?;
    validate!(
        !counterparty.is_being_liquidated(),
        ErrorCode::UserIsBeingLiquidated,
        "counterparty {} is being liquidated",
        counterparty_key
    )?;

    Ok(())
}

/// The oracle gates a routed fill applies before it matches.
///
/// A fill skips while the oracle has run too far from its 5-minute TWAP. A
/// party with an equity floor matches only while the raw exchange oracle
/// admits a match, because that oracle values the floor. The price comes off
/// the book's own rows, so the counterparty's price is held to the book
/// entry's band at the MM oracle, as the router holds a book leg. The fill
/// price is held to the symmetric fill band every routed fill passes.
///
/// Returns the fill price.
fn admit_pair_match(
    cx: &TakerOriginContext<'_, '_>,
    pair: &RemainderPair<'_>,
    settled: &controller::orders::SettledMatch,
    entry_twap_5min: i64,
    maps: &mut AccountMaps,
) -> Result<u64> {
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

    let band = super::crank_clob_cancel_outside_band::MakerBand::at_placement(
        cx.state,
        maps,
        &cx.accounts.quoter_slab,
        cx.market_index,
        cx.clock.slot,
    )?;
    validate!(
        !band.refuses(pair.counterparty.price, cx.taker_direction.opposite())?,
        ErrorCode::QuoterFillOffQuote,
        "the book rested the counterparty at {}, outside the maker oracle band",
        pair.counterparty.price
    )?;

    let fill_price = crate::math::orders::calculate_fill_price(
        pair.quote_filled,
        pair.base_filled,
        crate::math::constants::BASE_PRECISION_U64,
    )?;
    crate::math::orders::validate_fill_price_within_price_bands(
        fill_price,
        settled.oracle_price(),
        entry_twap_5min,
        maps.perp_market_map
            .get_ref(&cx.market_index)?
            .margin_ratio_initial,
        cx.state
            .oracle_guard_rails
            .max_oracle_twap_5min_percent_divergence(),
        None,
    )?;

    Ok(fill_price)
}

/// Whether the aggressor of a pair pays its builder fee.
///
/// A routed fill charges it only to a taker that meets initial margin before
/// the fill, under strict oracles. The pair applies the same gate.
fn pair_builder_fee_allowed(
    cx: &TakerOriginContext<'_, '_>,
    order: &crate::state::user::Order,
    facts: &PairFillFacts,
    has_escrow: bool,
    maps: &mut AccountMaps,
) -> Result<bool> {
    if !has_escrow || !order.is_bit_flag_set(OrderBitFlag::HasBuilder) {
        return Ok(false);
    }

    let limits = controller::orders::TakerRiskLimits {
        market_index: cx.market_index,
        order_decreasing: facts.aggressor_order_decreasing,
        is_isolated: facts.aggressor_is_isolated,
        oracle_stale_for_margin: facts.oracle_stale_for_margin,
        mode: FillMode::Fill,
        taker_exposure_closed_by_caller: false,
        perp_market_oi_before: facts.perp_market_oi_before,
    };
    let context = crate::state::margin_calculation::MarginContext::standard_with_config(
        limits.margin_config(
            crate::math::margin::MarginRequirementType::Initial,
            limits.is_isolated,
        ),
    )
    .strict(true)
    .ignore_invalid_deposit_oracles(true);
    let calculation =
        crate::math::margin::calculate_margin_requirement_and_total_collateral_and_liability_info(
            &*load!(cx.accounts.taker)?,
            maps,
            context,
        )?;

    Ok(calculation.meets_margin_requirement() && calculation.all_liability_oracles_valid)
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

/// What the gates before a pair settlement decided about its price.
struct PairPricing {
    oracle_price: i64,
    fill_price: u64,
    builder_fee_allowed: bool,
}

/// Move both positions with one match at the counterparty's price.
///
/// The settlement takes no filler, so no reward comes out of the taker fee. The
/// cranker is paid out of the improvement instead.
fn settle_pair_match<'info>(
    cx: &TakerOriginContext<'_, 'info>,
    pair: &RemainderPair<'_>,
    order: &mut crate::state::user::Order,
    pricing: &PairPricing,
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
            oracle_price: pricing.oracle_price,
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
            )
            .allow_builder_fee(pricing.builder_fee_allowed),
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

    // A pair takes nothing from the vAMM, so the sample reads the stored curve.
    let amm_mark_quote = controller::orders::AmmMarkQuote::of_amm(&market.amm)?;
    controller::orders::record_fill_in_mark_twap_and_volume(
        market.deref_mut(),
        &amm_mark_quote,
        controller::orders::FillAmounts {
            base: pair.base_filled,
            quote: pair.quote_filled,
        },
        taker_direction,
        cx.clock.unix_timestamp,
    )?;
    market.last_fill_price = pricing.fill_price;

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

/// Stage this crank for the book's taker-origin work, if it has any.
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
        BookRow::from_row,
    )?
    .ok_or(ErrorCode::NoTakerOriginCross)?;

    let slot = Clock::get()?.slot;
    let vamm = resolver_vamm_tops(ctx, market_index, slot)?;
    let Some(stage) = choose_stage(&bids, &asks, vamm, slot) else {
        return Ok(None);
    };

    // The protocol `User` is the filler on this path, and the crank loads each
    // margin account once, so it cannot also be a side of the cross it
    // resolves.
    let (protocol_user, _) = pdas::protocol_user_pair();
    let is_protocol_user =
        |user: &UserRefV0| pdas::user(&user.authority, user.sub_account_id) == protocol_user;
    if is_protocol_user(&stage.taker) || stage.makers.iter().any(is_protocol_user) {
        return Ok(None);
    }

    Ok(Some(TakerOriginStage {
        call: taker_origin_call(ctx, stage.taker, &stage.makers, stage.read_depth)?,
        yields_to_maker_cross: stage.yields_to_maker_cross,
    }))
}

/// Whether a cross stayed on the book past `STALLED_TAKER_ORIGIN_CROSS_SLOTS`.
/// It formed no earlier than the later of its two rows rested.
fn cross_stalled(cross: &Cross, slot: u64) -> bool {
    let formed_no_earlier_than = cross.bid.placed_slot.max(cross.ask.placed_slot);
    slot.saturating_sub(formed_no_earlier_than)
        > super::crank_cross_match::STALLED_TAKER_ORIGIN_CROSS_SLOTS
}

/// A staged taker-origin crank, and whether a maker cross may go first.
pub(super) struct TakerOriginStage {
    pub call: StagedCall,
    /// The claimed cross stalled past `STALLED_TAKER_ORIGIN_CROSS_SLOTS`, or
    /// the stage settles no book cross the taker claims.
    pub yields_to_maker_cross: bool,
}

/// The vAMM's best price on each side, as the resolver estimates it. A side
/// the vAMM cannot fill, or a resolver that carries no perp market, is `None`.
#[derive(Clone, Copy, Default)]
struct VammTops {
    bid: Option<u64>,
    ask: Option<u64>,
}

impl VammTops {
    /// The price the vAMM fills a taker on `side` at.
    fn facing(&self, side: SideV0) -> Option<u64> {
        match side {
            SideV0::Bid => self.ask,
            SideV0::Ask => self.bid,
        }
    }
}

/// The vAMM's tops, off the perp market the resolver's tail carries.
///
/// The resolver has no oracle account, so the curve is projected to the
/// oracle price the market last stored, and the fill gates read that price
/// too. The executor routes against the live oracle, so a stale estimate can
/// fail a crank until the stored price moves, but it never fills wrong.
fn resolver_vamm_tops(
    ctx: &Context<ResolveClobCrank>,
    market_index: u16,
    slot: u64,
) -> Result<VammTops> {
    let Some(info) = ctx
        .remaining_accounts
        .iter()
        .find(|account| account.key() == pdas::perp_market(market_index))
    else {
        return Ok(VammTops::default());
    };

    let perp_market_map = PerpMarketMap::load_one(info, false)?;
    let market = perp_market_map.get_ref(&market_index)?;
    let state = ctx.accounts.state.load()?;
    let stored = &market.market_stats.historical_oracle_data;
    let oracle_price_data = crate::state::oracle::OraclePriceData {
        price: stored.last_oracle_price,
        confidence: stored.last_oracle_conf,
        delay: stored.last_oracle_delay,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };
    let top = |direction| {
        controller::orders::vamm_top_price(&market, oracle_price_data, &state, slot, direction)
    };

    Ok(VammTops {
        bid: top(PositionDirection::Short),
        ask: top(PositionDirection::Long),
    })
}

/// Makers one staged crank carries, the counterparty included. Each costs two
/// accounts. Past the counterparty, they let the fill pass over a row whose
/// owner cannot settle and reach the depth behind it. The fill stops short at
/// the first owner past these, so more owners than this cannot refuse it.
const STAGED_COUNTERPARTIES: usize = 3;

/// What the resolver stages for one book.
#[derive(Debug, PartialEq, Eq)]
struct ChosenStage {
    taker: UserRefV0,
    /// The makers whose accounts the crank carries, best first.
    makers: Vec<UserRefV0>,
    read_depth: u16,
    yields_to_maker_cross: bool,
}

/// The taker-origin work the resolver stages, if the book has any, with a
/// read deep enough for the executor's check on it.
fn choose_stage(
    bids: &[BookRow],
    asks: &[BookRow],
    vamm: VammTops,
    slot: u64,
) -> Option<ChosenStage> {
    let mut stage = choose_subject_stage(bids, asks, vamm, slot)?;
    stage.read_depth = stage.read_depth.max(crossing_read_depth(bids, asks));
    Some(stage)
}

/// The remainder the resolver stages, and the makers its crank carries. A
/// cross the taker claims or a lapsed pair goes first. Otherwise the resolver
/// stages a remainder that routes with every live claim honoured. The executor
/// applies the same plan, so a stage it would refuse is never chosen.
fn choose_subject_stage(
    bids: &[BookRow],
    asks: &[BookRow],
    vamm: VammTops,
    slot: u64,
) -> Option<ChosenStage> {
    match stageable_cross(bids, asks).or_else(|| lapsed_pair_at_front(bids, asks)) {
        Some((cross, side)) => cross_stage(bids, asks, &cross, side, vamm, slot),
        None => routed_stage(bids, asks, vamm),
    }
}

/// The stage for one cross with an aggressor, as the executor's plan treats it.
fn cross_stage(
    bids: &[BookRow],
    asks: &[BookRow],
    cross: &Cross,
    side: SideV0,
    vamm: VammTops,
    slot: u64,
) -> Option<ChosenStage> {
    let aggressor = aggressor_of(cross, side);
    let counterparty = counterparty_of(cross, side);
    let resolution = counterparty
        .taker_origin
        .then(|| pair_resolution(vamm, side, counterparty.price));
    match resolution {
        Some(PairResolution::CounterpartyRoutesFirst) => {
            return counterparty_route_stage(bids, asks, &counterparty);
        }

        // The executor refuses the pair, and the vAMM cannot fill either
        // remainder's route. Staging it would fail every attempt.
        Some(PairResolution::Unpriced) => return routed_stage(bids, asks, vamm),
        _ => {}
    }

    Some(ChosenStage {
        taker: aggressor.user,
        makers: staged_makers(
            &aggressor,
            Some(&counterparty),
            side,
            opposite_rows(bids, asks, side),
        ),
        read_depth: crossing_read_depth(bids, asks),
        yields_to_maker_cross: cross_stalled(cross, slot),
    })
}

/// The earlier remainder of a pair, routed to the vAMM before the pair
/// settles it at its own worst price.
fn counterparty_route_stage(
    bids: &[BookRow],
    asks: &[BookRow],
    counterparty: &RestingOrder,
) -> Option<ChosenStage> {
    let (side, position) = [(SideV0::Bid, bids), (SideV0::Ask, asks)]
        .iter()
        .copied()
        .find_map(|(side, rows)| {
            rows.iter()
                .position(|row| row.order.order_ref == counterparty.order_ref)
                .map(|position| (side, position))
        })?;
    let row = match side {
        SideV0::Bid => &bids[position],
        SideV0::Ask => &asks[position],
    };
    if !is_routed_subject(bids, asks, side, row) {
        return None;
    }

    Some(ChosenStage {
        taker: counterparty.user,
        makers: Vec::new(),
        read_depth: (position as u16).saturating_add(1).min(MAX_CROSS_ROWS),
        yields_to_maker_cross: true,
    })
}

/// The oldest remainder the executor would route, and that the vAMM or an
/// unclaimed row fills.
fn routed_stage(bids: &[BookRow], asks: &[BookRow], vamm: VammTops) -> Option<ChosenStage> {
    let side_rows = |side| match side {
        SideV0::Bid => bids,
        SideV0::Ask => asks,
    };

    [SideV0::Bid, SideV0::Ask]
        .iter()
        .copied()
        .flat_map(|side| {
            side_rows(side)
                .iter()
                .enumerate()
                .map(move |(position, row)| (side, position, row))
        })
        .filter(|(side, _, row)| {
            row.order.taker_origin
                && is_routed_subject(bids, asks, *side, row)
                && routed_subject_fills(
                    row,
                    *side,
                    side_rows(*side),
                    opposite_rows(bids, asks, *side),
                    vamm,
                )
        })
        .min_by_key(|(_, _, row)| row.order.order_ref.order_id)
        .map(|(side, position, row)| {
            let makers = staged_makers(&row.order, None, side, opposite_rows(bids, asks, side));
            let opposite_depth = makers_depth(opposite_rows(bids, asks, side), &makers);
            ChosenStage {
                taker: row.order.user,
                makers,
                read_depth: (position.max(opposite_depth) as u16)
                    .saturating_add(1)
                    .min(MAX_CROSS_ROWS),
                yields_to_maker_cross: true,
            }
        })
}

fn opposite_rows<'a>(bids: &'a [BookRow], asks: &'a [BookRow], side: SideV0) -> &'a [BookRow] {
    match side {
        SideV0::Bid => asks,
        SideV0::Ask => bids,
    }
}

/// Whether the executor's plan for this row's owner routes this row. It
/// routes the owner's first remainder, bids before asks.
fn is_routed_subject(bids: &[BookRow], asks: &[BookRow], side: SideV0, row: &BookRow) -> bool {
    let user = row.order.user;
    let first = bids
        .iter()
        .map(|row| (SideV0::Bid, row))
        .chain(asks.iter().map(|row| (SideV0::Ask, row)))
        .find(|(_, candidate)| candidate.order.taker_origin && candidate.order.user == user);
    first.is_some_and(|(first_side, first_row)| {
        first_side == side && first_row.order.order_ref == row.order.order_ref
    })
}

/// Whether a remainder that routes with every live claim honoured can fill.
///
/// The vAMM fills it when its top beats the rest price. A maker row fills it
/// only when no live claimant on its side could hold that row, which the
/// resolver's read cannot tell apart row by row. A remainder on the other side
/// is a pair, never depth for this route.
fn routed_subject_fills(
    row: &BookRow,
    side: SideV0,
    own_side: &[BookRow],
    opposite: &[BookRow],
    vamm: VammTops,
) -> bool {
    let direction = PositionDirection::from(side);
    if controller::orders::vamm_improves_on(vamm.facing(side), direction, row.order.price) {
        return true;
    }

    let no_live_claimant = own_side
        .iter()
        .all(|other| !other.order.taker_origin || other.claim_lapsed);
    no_live_claimant
        && opposite.iter().any(|other| {
            !other.order.taker_origin && crossable_counterparty(&row.order, side, other)
        })
}

/// Whether `other` is a row a fill of `subject` on `side` can take.
fn crossable_counterparty(subject: &RestingOrder, side: SideV0, other: &BookRow) -> bool {
    price_crosses(side, subject.price, other.order.price)
        && !crate::math::crosses::same_authority(&subject.user, &other.order.user)
}

/// The makers a crank for `subject` carries: the counterparty first, then the
/// next distinct owners of the rows that cross the subject, best first.
fn staged_makers(
    subject: &RestingOrder,
    counterparty: Option<&RestingOrder>,
    side: SideV0,
    opposite: &[BookRow],
) -> Vec<UserRefV0> {
    let mut makers: Vec<UserRefV0> = counterparty.map(|row| row.user).into_iter().collect();
    for row in opposite {
        if makers.len() == STAGED_COUNTERPARTIES {
            break;
        }

        if crossable_counterparty(subject, side, row) && !makers.contains(&row.order.user) {
            makers.push(row.order.user);
        }
    }

    makers
}

/// The deepest position of a staged maker's row on the opposite side.
fn makers_depth(opposite: &[BookRow], makers: &[UserRefV0]) -> usize {
    opposite
        .iter()
        .rposition(|row| makers.contains(&row.order.user))
        .unwrap_or(0)
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
    makers: &[UserRefV0],
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
    .map_section_named_perp(oracle, quote_spot_market_index);
    // The SOL spot market prices the keeper payment's floor. It sits after the
    // quote spot market, so the perp market still closes the maps section.
    let call = super::crank_cross_match::with_sol_spot_market(
        call,
        &*ctx.accounts.state.load()?,
        quote_spot_market_index,
    )
    .account(pdas::perp_market(market_index), true)
    .maker_refs(makers.iter().copied());
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
fn stageable_cross(bids: &[BookRow], asks: &[BookRow]) -> Option<(Cross, SideV0)> {
    if !front_is_taker_origin(bids, asks) {
        return None;
    }

    let crosses = resolve_crosses(&claim_view(bids), &claim_view(asks), MAX_CROSSES_PER_CRANK);
    let cross = crosses
        .iter()
        .find(|cross| cross.kind != CrossKind::ProtocolMiddles)?;
    Some((*cross, cross.kind.aggressor_side()?))
}
