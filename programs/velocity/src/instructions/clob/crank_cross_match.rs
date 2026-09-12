//! Cross-match crank: match two crossed resting sources against each other.
//!
//! Nothing else matches two *resting* books — router matching only happens
//! when a taker fills through — so a CLOB bid at/above a CLOB ask, or a
//! PropAMM quoting through the CLOB's best, would rest crossed forever.
//! Trigger placements make the first routine and PropAMM reprices the
//! second; both strand user orders, which is the UX this crank exists for.
//!
//! The crank is two ordinary router fills. The protocol `User` buys `size`
//! as a taker, then sells exactly what the buy filled, and each leg routes
//! across every source the transaction carries. Running the cross this way
//! is what gives it everything a fill has: the vAMM's last look, a PropAMM
//! that can cross the book, the pre-execute margin clamp on unreserved
//! depth, reduce-only trigger cancellation, and the shared post-fill margin,
//! equity-floor and open-interest checks.
//!
//! Three requirements make the pair a cross rather than two sweeps. The legs
//! must match the same base, so the protocol ends flat. Every unit must have
//! crossed, which the worst price of each leg states exactly: the highest
//! price the buy leg paid must be at or under the lowest price the sell leg
//! received. And the quote the protocol keeps must clear the market's floor,
//! so a cross the reservoir pays for never nets less than it costs to land.
//! A cross inside the fee gulf simply rests.
//!
//! The surplus lands in the protocol `User` — the sink the crank incentive
//! loop drains — and the caller's `authority` is paid reservoir lamports; no
//! signature is required anywhere (relay turners submit executors unsigned).
//!
//! A crossing taker remainder needs no guard here. A remainder claims the
//! depth it crosses, and claimed depth leaves the book's matchable set for
//! every caller that does not pass `consume_reservation`. This crank passes
//! `false` at every site, so it cannot reach a remainder's cover at any size
//! the caller asks for, and `crank_taker_origin_cross` — which owes the taker
//! that improvement — is the only caller that can.

use {
    super::{
        crank_common::{ResolveClobCrank, MAX_CROSS_MAKERS},
        crank_taker_origin_cross::stage_taker_origin_cross,
    },
    crate::{
        controller::{
            self, funding::settle_funding_payment, orders::crank_oracle_preflight,
            position::PositionDirection,
        },
        error::ErrorCode,
        instructions::{
            constraints::*,
            optional_accounts::{tx_writable_lock_count, AccountMaps},
            relay_harness::{resolve_into, StagedCall},
            with_counterparty_room, CapInputs, QuoteInputs, QuotedRoute,
        },
        load, load_mut,
        math::{
            casting::Cast, constants::MARGIN_PRECISION_U128, router::FillerObligation,
            safe_math::SafeMath,
        },
        msg,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            fill_mode::FillMode,
            pdas,
            perp_market_map::MarketSet,
            prop_amm::{
                Direction, PriceLevel, QuoterCpiScratch, QuoterSlabExt, QuoterSlabV0, QuoterType,
            },
            state::State,
            user::{MarketType, Order, OrderStatus, OrderType, User, UserStats},
            user_map::{load_user_maps, UserMap, UserStatsMap},
        },
        validate,
    },
    anchor_lang::prelude::*,
    solana_program::sysvar::instructions::ID as IX_ID,
};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct CrankCrossMatchArgs {
    pub market_index: u16,
    /// Base the buy leg takes. The sell leg returns exactly what the buy leg
    /// filled, and the cross is refused unless every unit of it crossed — so
    /// a size past the crossing depth fails rather than sweeping through it.
    pub size: u64,
}

#[derive(Accounts)]
#[instruction(args: CrankCrossMatchArgs)]
pub struct CrankCrossMatch<'info> {
    pub state: AccountLoader<'info, State>,
    /// CHECK: the lamport payout target — relay's keeper-placeholder slot.
    /// No signature: the cross's own profitability rules are the gate.
    #[account(mut)]
    pub authority: UncheckedAccount<'info>,
    /// The protocol-owned pass-through taker. Locked to the protocol `User`
    /// so the reservoir never pays for someone else's private arb.
    #[account(
        mut,
        constraint = is_protocol_user(&taker, &state)?
    )]
    pub taker: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&taker, &taker_stats)?
    )]
    pub taker_stats: AccountLoader<'info, UserStats>,
    /// The market's conditions account: the reservoir that pays the keeper.
    #[account(
        mut,
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            args.market_index.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub crank_conditions: AccountLoader<'info, ClobCrankConditionsV0>,
    /// The crossed market. Named rather than read out of the maps section:
    /// the cross serves exactly one market, so the account is structural.
    #[account(
        mut,
        seeds = [b"perp_market", args.market_index.to_le_bytes().as_ref()],
        bump,
        has_one = quoter_slab
    )]
    pub perp_market: AccountLoader<'info, crate::state::perp_market::PerpMarket>,
    /// The market's approved quoters. Bound by the market's `has_one`, which
    /// is a memcmp where a seeds constraint pays a PDA derivation. Each leg
    /// assembles its route off the copy that rides the account tail, as every
    /// router fill does.
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: address-locked. The filler obligation is measured against how
    /// many account locks the transaction holds, and this is what counts them.
    #[account(address = IX_ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,
}

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_crank_cross_match<'c: 'info, 'info>(
    ctx: Context<'info, CrankCrossMatch<'info>>,
    args: CrankCrossMatchArgs,
) -> Result<()> {
    let CrankCrossMatchArgs { market_index, size } = args;
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    // The perp market is a named account, so the maps section carries only
    // the oracle and the quote spot market.
    let oracle_map = crate::state::oracle_map::OracleMap::load(
        remaining_accounts_iter,
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;
    let spot_market_map = crate::state::spot_market_map::SpotMarketMap::load(
        &MarketSet::new(),
        remaining_accounts_iter,
    )?;
    let perp_market_map = crate::state::perp_market_map::PerpMarketMap::load_one(
        ctx.accounts.perp_market.as_ref(),
        true,
    )?;
    crate::instructions::optional_accounts::update_prelaunch_oracle(
        std::ops::Deref::deref(&perp_market_map.get_ref(&market_index)?),
        &oracle_map,
        clock.slot,
    )?;
    // The perp market arrives named rather than through the maps section, so
    // the bundle is built here instead of by `load_maps`.
    let mut maps = AccountMaps {
        perp_market_map,
        spot_market_map,
        oracle_map,
    };
    let (makers_and_referrer, makers_and_referrer_stats) =
        load_user_maps(remaining_accounts_iter, true)?;
    // The protocol taker is loaded by name and mutated for the length of each
    // leg. Repeating it in the maker section would fail the leg on a borrow
    // rather than say why.
    validate!(
        !makers_and_referrer
            .0
            .contains_key(&ctx.accounts.taker.key()),
        ErrorCode::InvalidMaker,
        "the maker section must not repeat the protocol taker"
    )?;

    // The account tail: the market's slab plus the union of the consulted
    // quoters' registered accounts, the same shape a router fill carries.
    let tail_from = ctx.remaining_accounts.len() - remaining_accounts_iter.len();
    let tail = &ctx.remaining_accounts[tail_from..];

    // Both legs are held to the oracle rules an ordinary fill applies, before
    // either of them moves a position. The pre-flight refuses a market in
    // settlement, paused fills, an oracle that may not price a match, and a
    // mark outside the market's band.
    let (band_oracle_price, margin_ratio_initial, clob_market) = {
        let market = &mut maps.perp_market_map.get_ref_mut(&market_index)?;
        crank_oracle_preflight(market, &state, &mut maps.oracle_map, &clock, "cross match")?;
        let oracle_id = market.oracle_id();
        let (margin_ratio_initial, clob_market) = (market.margin_ratio_initial, market.clob_market);
        (
            maps.oracle_map.get_price_data(&oracle_id)?.price,
            margin_ratio_initial,
            clob_market,
        )
    };

    let base_before = taker_base(&ctx.accounts.taker, market_index)?;

    // One set of CPI buffers for the whole crank, as a router fill uses. Both
    // legs refill them in turn; a set per leg would be an allocation per leg,
    // and the runtime's allocator never gives one back.
    let mut cpi_scratch = QuoterCpiScratch::new();
    let legs = CrossLegs {
        accounts: &ctx.accounts,
        tail,
        state: &state,
        market_index,
        band_oracle_price,
        margin_ratio_initial,
        clob_market,
        makers_and_referrer: &makers_and_referrer,
        makers_and_referrer_stats: &makers_and_referrer_stats,
        clock: &clock,
    };

    let buy = run_cross_leg(
        &legs,
        PositionDirection::Long,
        size,
        &mut maps,
        &mut cpi_scratch,
    )?;
    let sell = run_cross_leg(
        &legs,
        PositionDirection::Short,
        buy.base_filled,
        &mut maps,
        &mut cpi_scratch,
    )?;

    let cross_floor = cross_surplus_floor(
        &ctx.accounts.crank_conditions,
        &state,
        &maps.spot_market_map,
        &mut maps.oracle_map,
    )?;
    let CrossSurplus {
        base_matched,
        surplus,
    } = validate_cross_legs(
        &buy,
        &sell,
        (base_before, taker_base(&ctx.accounts.taker, market_index)?),
        cross_floor,
    )?;

    pay_cross_keeper(&ctx.accounts.crank_conditions, &ctx.accounts.authority)?;

    msg!(
        "cross matched {} base for {} quote surplus on market {}",
        base_matched,
        surplus,
        market_index
    );
    Ok(())
}

/// What both legs of a cross read and neither of them changes.
struct CrossLegs<'a, 'info> {
    accounts: &'a CrankCrossMatch<'info>,
    tail: &'info [AccountInfo<'info>],
    state: &'a State,
    market_index: u16,
    /// The oracle price the fill's own maker band measures against. Each leg
    /// bounds itself at the edge of that band.
    band_oracle_price: i64,
    /// The market's own band, in MARGIN_PRECISION units.
    margin_ratio_initial: u32,
    /// The book the market names. Every router fill must consult it.
    clob_market: Pubkey,
    makers_and_referrer: &'a UserMap<'info>,
    makers_and_referrer_stats: &'a UserStatsMap<'info>,
    clock: &'a Clock,
}

/// What one leg of a cross filled.
struct CrossLegFilled {
    base_filled: u64,
    /// What the leg did to the protocol `User`'s quote, net of the taker fee
    /// it paid. Read on both sides of the fill with funding already settled,
    /// so a funding payment cannot read as cross surplus.
    quote_delta: i64,
    /// The worst price any single source of this leg executed at. Zero when
    /// the leg filled nothing.
    worst_price: u64,
}

/// The protocol taker's base in this market, or zero when it holds no
/// position there yet.
fn taker_base(taker: &AccountLoader<User>, market_index: u16) -> Result<i64> {
    Ok(load!(taker)?
        .get_perp_position(market_index)
        .map(|position| position.base_asset_amount)
        .unwrap_or(0))
}

/// Run one leg of a cross as an ordinary router fill.
///
/// The leg's order is a local. It belongs to no `orders` slot and never
/// rested, so the fill takes it directly and unwinds no reservation for it.
/// Whatever the fill leaves unfilled is simply dropped, and the two legs then
/// fail to balance, which is how a leg that came up short refuses the cross.
///
/// The protocol `User` stands as its own filler, so no reward is carved out
/// of the taker fee it pays, and there is no builder escrow to accrue
/// against.
fn run_cross_leg<'info>(
    legs: &CrossLegs<'_, 'info>,
    taker_direction: PositionDirection,
    size: u64,
    maps: &mut AccountMaps<'info>,
    cpi_scratch: &mut QuoterCpiScratch<'info>,
) -> Result<CrossLegFilled> {
    if size == 0 {
        return Ok(CrossLegFilled {
            base_filled: 0,
            quote_delta: 0,
            worst_price: 0,
        });
    }
    let direction = match taker_direction {
        PositionDirection::Long => Direction::Long,
        PositionDirection::Short => Direction::Short,
    };
    let limit_price = leg_limit_price(
        taker_direction,
        legs.band_oracle_price,
        legs.margin_ratio_initial,
    )?;

    // Funding is settled here, before the quote is read. The fill settles it
    // too, so by the time the fill runs there is nothing left to charge, and
    // the quote the leg is measured on moves for the fill alone. Without
    // this, a funding payment would read as cross surplus — including the
    // one the first leg's own funding update creates for the second.
    let (order_id, taker_ref, quote_before) = {
        let mut market = maps.perp_market_map.get_ref_mut(&legs.market_index)?;
        let mut taker = load_mut!(legs.accounts.taker)?;
        settle_funding_payment(
            &mut taker,
            &legs.accounts.taker.key(),
            &mut market,
            legs.clock.unix_timestamp,
        )?;
        let order_id = taker.next_order_id;
        taker.next_order_id = taker.next_order_id.wrapping_add(1).max(1);
        let quote_before = taker
            .get_perp_position(legs.market_index)
            .map(|position| position.quote_asset_amount)
            .unwrap_or(0);
        (order_id, taker.clob_user_ref(), quote_before)
    };
    // A limit order at the edge of the market's maker band. The band is what
    // the fill refuses a maker price past anyway, so bounding the leg there
    // discards no depth the fill could have taken, and it keeps every
    // quoter's walk off levels the fill would drop.
    let mut order = Order {
        slot: legs.clock.slot,
        order_id,
        market_index: legs.market_index,
        status: OrderStatus::Open,
        order_type: OrderType::Limit,
        market_type: MarketType::Perp,
        direction: taker_direction,
        base_asset_amount: size,
        price: limit_price,
        existing_position_direction: taker_direction,
        ..Order::default()
    };

    let users = crate::state::prop_amm::quoter_wire_users(
        legs.makers_and_referrer.user_ref_index()?.into_keys().map(
            |(authority, sub_account_id)| crate::state::prop_amm::ClobUserRefV0 {
                authority,
                sub_account_id,
            },
        ),
    )?;
    // A cross is one event on two sides, so one verdict covers both legs. A
    // leg that read only the side it sweeps would call the flow protected
    // whenever the fresh order sat on the other side, and the cross would
    // then reach liquidity that serves protected flow only — laundering an
    // order that never rested through the leg that does not settle it. Both
    // directions are read, bounded by the size this cross takes, so depth the
    // cross never touches still does not count against it.
    let taker_served_window = [Direction::Long, Direction::Short]
        .iter()
        .copied()
        .try_fold(true, |served, side| -> Result<bool> {
            Ok(served
                && leg_served_window(
                    &legs.accounts.quoter_slab,
                    legs.tail,
                    legs.market_index,
                    side,
                    size,
                    legs.clock.slot,
                    cpi_scratch,
                )?)
        })?;
    let inputs = QuoteInputs {
        caps: crate::state::prop_amm::QuoterUserCapsV0::EMPTY,
        market_index: legs.market_index,
        direction,
        size,
        users: &users,
        reference_price: legs.band_oracle_price,
        taker: taker_ref,
        limit_price,
        taker_served_window,
        // The depth a crossing remainder claims is that taker's improvement,
        // not arbitrage for the protocol to middle. Reading the book without
        // it is what makes this crank unable to reach a remainder's cover.
        consume_reservation: false,
        // Both filled in below, once every counterparty is sized.
        rooms: crate::instructions::router::user_caps::QuoterRooms::NONE,
        margin_ratio_initial: legs.margin_ratio_initial,
    };
    let inputs = with_counterparty_room(
        legs.tail,
        &legs.accounts.taker.key(),
        inputs,
        &mut CapInputs {
            makers_and_referrer: legs.makers_and_referrer,
            makers_and_referrer_stats: legs.makers_and_referrer_stats,
            maps,
            slot: legs.clock.slot,
            now: legs.clock.unix_timestamp,
        },
    )?;

    let route = QuotedRoute::assemble(legs.tail, &inputs, cpi_scratch)?;
    route.require_baseline(legs.clob_market)?;
    let mut book_storage =
        [crate::math::router::QuoterBook::default(); crate::state::prop_amm::MAX_ROUTE_QUOTERS];
    let books = route.books(&mut book_storage)?;
    let mut executor = route.executor(
        &inputs,
        legs.clock.slot,
        legs.clock.unix_timestamp,
        cpi_scratch,
    );
    let mut router_inputs = crate::math::router::RouterFillInputs {
        books,
        executor: &mut executor,
        protocol_authority: legs.state.signer,
        taker_exposure_closed_by_caller: true,
        obligation: FillerObligation {
            // The taker is the protocol, not a user trusting a cranker with
            // its order, so nobody is owed a maker here. A book that stops at
            // an owner the transaction does not carry only makes the cross
            // smaller, and the surplus floor decides whether the smaller
            // cross is worth landing.
            taker_signed: false,
            tx_accounts: Some(tx_writable_lock_count(
                &legs.accounts.instructions_sysvar.to_account_info(),
            )?),
            unrouted_quoters: 0,
        },
        worst_fill_price: None,
    };

    let (base_filled, _) = controller::orders::fill_perp_order(
        controller::orders::FillRequest {
            target: controller::orders::FillTarget::Detached {
                order: &mut order,
                reserved: false,
            },
            mode: FillMode::Fill,
            referrer_is_accelerated: false,
        },
        legs.state,
        legs.clock,
        controller::orders::PerpFillAccounts {
            user: &legs.accounts.taker,
            user_stats: &legs.accounts.taker_stats,
            filler: &legs.accounts.taker,
            filler_stats: &legs.accounts.taker_stats,
            rev_share_escrow: &mut None,
        },
        &mut controller::orders::FillParties {
            maps,
            makers_and_referrer: legs.makers_and_referrer,
            makers_and_referrer_stats: legs.makers_and_referrer_stats,
        },
        &mut router_inputs,
    )?;
    // Read straight after the fill, with no second settle: the fill's own
    // funding update belongs to whoever holds the position next, and the next
    // leg settles it before it starts measuring.
    let quote_after = load!(legs.accounts.taker)?
        .get_perp_position(legs.market_index)
        .map(|position| position.quote_asset_amount)
        .unwrap_or(0);
    Ok(CrossLegFilled {
        base_filled,
        quote_delta: quote_after.safe_sub(quote_before)?,
        worst_price: router_inputs.worst_fill_price.unwrap_or(0),
    })
}

/// The edge of the market's maker oracle band, on the side the leg buys or
/// sells at.
///
/// A cross leg has no price of its own to bring: the crossed prices are what
/// it exists to reach, and they are not known until both sides are read. The
/// band is the widest price the fill itself will settle a maker at, so it is
/// the widest bound that discards nothing. It still bounds the leg, which is
/// what keeps a quoter's walk and the vAMM ladder off levels the fill would
/// refuse.
fn leg_limit_price(
    taker_direction: PositionDirection,
    oracle_price: i64,
    margin_ratio_initial: u32,
) -> Result<u64> {
    let oracle_price = oracle_price.unsigned_abs();
    let band = oracle_price
        .cast::<u128>()?
        .safe_mul(margin_ratio_initial.cast()?)?
        .safe_div(MARGIN_PRECISION_U128)?
        .cast::<u64>()?;
    Ok(match taker_direction {
        PositionDirection::Long => oracle_price.saturating_add(band),
        PositionDirection::Short => oracle_price.saturating_sub(band),
    })
}

/// Whether every book order this leg could consume has measurably rested.
///
/// A crank cannot vouch for that by construction. On a zero-delay book,
/// place-then-crank is two back-to-back transactions, so fresh informed flow
/// would wear the protected flag into a quoter that only serves protected
/// flow. So the crank measures it, over the depth the leg can reach. That
/// depth is the first `size` base of the side the leg sweeps, so a fresh
/// order deeper than the leg goes defers nothing.
///
/// One side per leg. Each leg is its own fill and transmits only the flow it
/// sweeps, so a fresh order on the other side of the book has no bearing on
/// it.
fn leg_served_window<'info>(
    quoter_slab: &AccountLoader<'info, QuoterSlabV0>,
    tail: &'info [AccountInfo<'info>],
    market_index: u16,
    direction: Direction,
    size: u64,
    slot: u64,
    cpi_scratch: &mut QuoterCpiScratch<'info>,
) -> Result<bool> {
    let consulted = quoter_slab.consulted_slots(tail)?;
    consulted
        .iter()
        .try_fold(true, |served, &slot_index| -> Result<bool> {
            // Copy the config out so no slab borrow lives across the book CPI.
            let config = quoter_slab.slots()?[slot_index].config;
            if config.quoter_type != QuoterType::Clob {
                return Ok(served);
            }
            let rows = super::helpers::crank_common::book_l3_side(
                &config,
                quoter_slab,
                market_index,
                direction,
                CROSS_ROWS_PER_SIDE,
                tail,
                cpi_scratch,
                false,
                |row| (row.size, row.placed_slot),
            )?
            .unwrap_or_default();
            let (_, rested) =
                rows.iter()
                    .fold((0u64, true), |(depth, rested), (row_size, placed_slot)| {
                        if depth >= size {
                            return (depth, rested);
                        }
                        (
                            depth.saturating_add(*row_size),
                            rested && crate::math::crosses::served_window(*placed_slot, slot),
                        )
                    });
            Ok(served && rested)
        })
}

/// The base both legs matched, and the quote the protocol kept for it.
#[derive(Debug)]
struct CrossSurplus {
    base_matched: u64,
    surplus: i64,
}

/// The three rules that make a pair of fills a cross.
///
/// The legs must match the same base and the taker's base must return to
/// where it started, so the protocol ends flat and carries no position out of
/// the crank.
///
/// Every unit must have crossed. The worst price of each leg states that
/// exactly. The highest price the buy leg paid must be at or under the lowest
/// price the sell leg received. This is what a size past the crossing depth
/// fails on. Without the rule, a caller sizes past that depth and takes the
/// loss-making tail through its own resting orders at intermediate prices,
/// and the totals still clear the floor. The rule is therefore stronger than
/// the floor rather than a restatement of it. Both prices are floored the
/// same way, so a cross whose edge is inside one unit of quote is refused.
/// That cross is worth refusing, because the floor already asks for more.
///
/// The two legs' quote deltas add up to the crossed spread net of both legs'
/// taker fees. It must be positive and at or above the market's floor, so the
/// protocol never runs a losing cross and never pays reservoir lamports for
/// one worth less than the payment.
fn validate_cross_legs(
    buy: &CrossLegFilled,
    sell: &CrossLegFilled,
    base: (i64, i64),
    min_surplus: u64,
) -> Result<CrossSurplus> {
    let (base_before, base_after) = base;
    let base_matched = buy.base_filled;
    validate!(
        sell.base_filled == base_matched,
        ErrorCode::CrossMatchImbalanced,
        "cross legs imbalanced: bought {} sold {}",
        base_matched,
        sell.base_filled
    )?;
    validate!(
        base_matched > 0,
        ErrorCode::CrossMatchUnprofitable,
        "nothing crossed"
    )?;
    validate!(
        base_after == base_before,
        ErrorCode::CrossMatchImbalanced,
        "protocol user base changed: {} -> {}",
        base_before,
        base_after
    )?;
    validate!(
        buy.worst_price <= sell.worst_price,
        ErrorCode::CrossMatchLegsDoNotCross,
        "cross paid up to {} and sold down to {}, so part of it did not cross",
        buy.worst_price,
        sell.worst_price
    )?;
    let surplus = buy.quote_delta.safe_add(sell.quote_delta)?;
    validate!(
        surplus > 0 && surplus.unsigned_abs() >= min_surplus,
        ErrorCode::CrossMatchUnprofitable,
        "cross surplus {} below the market's floor of {} (fees are the gulf)",
        surplus,
        min_surplus
    )?;
    Ok(CrossSurplus {
        base_matched,
        surplus,
    })
}

/// The quote surplus a cross has to clear.
///
/// The floor covers the keeper's lamport payment valued in quote, so a cross
/// the reservoir pays for never nets the protocol less than it costs to land.
/// The two figures are in different units; the SOL oracle bridges them. When
/// no SOL market rides the crank or its oracle is unusable, the admin's
/// `min_cross_surplus` stands alone.
fn cross_surplus_floor(
    crank_conditions: &AccountLoader<ClobCrankConditionsV0>,
    state: &State,
    spot_market_map: &crate::state::spot_market_map::SpotMarketMap,
    oracle_map: &mut crate::state::oracle_map::OracleMap,
) -> Result<u64> {
    let (min_surplus, payment_lamports) = {
        let conditions = crank_conditions.load()?;
        (
            conditions.min_cross_surplus,
            u64::from(conditions.crank_payments.cross),
        )
    };
    let payment_quote = sol_oracle_price(state, spot_market_map, oracle_map)
        .and_then(|sol_price| {
            crate::state::clob_crank::CrankPaymentsV0::lamports_to_quote(
                payment_lamports,
                sol_price,
            )
        })
        .unwrap_or(0);
    Ok(min_surplus.max(payment_quote))
}

/// Pay the keeper's fee, so relay's `assert_paid_v0` has a balance to measure.
fn pay_cross_keeper<'info>(
    crank_conditions: &AccountLoader<'info, ClobCrankConditionsV0>,
    authority: &UncheckedAccount<'info>,
) -> Result<()> {
    let payment = u64::from(crate::load_mut!(crank_conditions)?.crank_payments.cross);
    ClobCrankConditionsV0::pay_keeper(crank_conditions, &authority.to_account_info(), payment)?;
    Ok(())
}

/// The validity-gated SOL oracle price, for pricing a lamport crank payment in
/// quote. `None` when no SOL market is configured, it is not loaded on this
/// crank, or its oracle is not valid — the caller then falls back to the
/// admin-set floor rather than block the cross.
fn sol_oracle_price(
    state: &State,
    spot_market_map: &crate::state::spot_market_map::SpotMarketMap,
    oracle_map: &mut crate::state::oracle_map::OracleMap,
) -> Option<i64> {
    if state.sol_spot_market_index == 0 {
        return None;
    }
    let sol_market = spot_market_map.get_ref(&state.sol_spot_market_index).ok()?;
    let (oracle_data, validity) = oracle_map
        .get_price_data_and_validity(
            crate::state::user::MarketType::Spot,
            sol_market.market_index,
            &sol_market.oracle_id(),
            sol_market.historical_oracle_data.last_oracle_price_twap,
            sol_market.get_max_confidence_interval_multiplier().ok()?,
            -1,
            0,
            None,
        )
        .ok()?;
    matches!(validity, crate::math::oracle::OracleValidity::Valid).then_some(oracle_data.price)
}

/// Resolver for the cross conditions: find the book's crossing prefix,
/// estimate profitability with the top (most conservative) taker-fee tier
/// on both legs, and stage the `crank_cross_match` executor — full
/// `(User, UserStats)` pairs, both derived from the node's `(authority,
/// sub_account_id)` identity. Only CLOB×CLOB is discoverable here — a
/// PropAMM crossing the CLOB is the generic quoter-cross resolver's job
/// (it CPIs `quote_v0` through the entry's registered surface), with the
/// book publisher as the fast path. The executor re-verifies profitability
/// exactly either way.
///
/// A taker-origin cross is looked for first and staged as
/// `crank_taker_origin_cross` instead. A `ResolvedCrankV0` names its own
/// executor, so serving both from one condition costs no extra slot and no
/// second wake — the wakes that find a maker×maker cross are the same ones that
/// find a taker-origin cross (see [`stage_taker_origin_cross`]). The order is
/// the economics: the improvement between the two prices belongs to the order
/// that came to trade, so it is handed over before the protocol middles the
/// same crossed book as arbitrage.
/// The cross and activation slots' answer: a crossed taker remainder if
/// there is one, otherwise a maker-against-maker cross worth taking.
pub(super) fn stage_cross(ctx: &Context<ResolveClobCrank>) -> Result<Option<StagedCall>> {
    if let Some(call) = stage_taker_origin_cross(ctx)? {
        return Ok(Some(call));
    }
    let cross = find_clob_cross(ctx)?;
    if cross.size == 0 {
        return Ok(None);
    }

    // Conservative estimate: tier-0 taker fee on both legs. The executor
    // measures the real thing; this only avoids staging obvious losers.
    let (fee_numerator, fee_denominator) = {
        let state = ctx.accounts.state.load()?;
        let tier = state.perp_fee_structure.fee_tiers[0];
        (
            tier.fee_numerator as u128,
            (tier.fee_denominator as u128).max(1),
        )
    };
    let fees = (cross.buy_quote * fee_numerator).div_ceil(fee_denominator)
        + (cross.sell_quote * fee_numerator).div_ceil(fee_denominator);
    if cross.sell_quote <= cross.buy_quote.saturating_add(fees) {
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
    let (protocol_user, protocol_user_stats) = pdas::protocol_user_pair();

    // Named accounts through the executor's own client struct (compile-time
    // shape check), then the remaining sections: maps, maker
    // `(User, UserStats)` pairs, the quoter section.
    let call = crate::staged_call!(CrankCrossMatch {
        state: ctx.accounts.state.key(),
        authority: pdas::keeper_placeholder(),
        taker: protocol_user,
        taker_stats: protocol_user_stats,
        crank_conditions: ctx.accounts.crank_conditions.key(),
        perp_market: pdas::perp_market(market_index),
        quoter_slab: ctx.accounts.quoter_slab.key(),
        instructions_sysvar: IX_ID,
    })
    .map_section_named_perp(oracle, quote_spot_market_index)
    .maker_refs(cross.makers.iter().copied());
    Ok(Some(
        // The slab rides the tail as well as being named: each leg assembles
        // its route from the tail, as every router fill does, and a route
        // without the slab consults nothing external.
        call.account(ctx.accounts.quoter_slab.key(), false)
            .account(ctx.accounts.clob_market.key(), true)
            .account(crate::ids::clob_program::id(), false)
            .arg(CrankCrossMatchArgs {
                market_index,
                size: cross.size,
            })?,
    ))
}

/// The crossing prefix of the book: total matchable size, the gross quote
/// of each leg, and the (deduped, capped) makers it touches.
struct ClobCross {
    size: u64,
    buy_quote: u128,
    sell_quote: u128,
    makers: Vec<crate::state::prop_amm::ClobUserRefV0>,
}

/// How deep either side of the crossing prefix is read.
///
/// The walk ends on [`MAX_CROSS_MAKERS`] distinct makers anyway, so this only
/// has to be past the point where a crossing prefix could still be one
/// maker's ladder. A prefix longer than this stages a smaller cross, which the
/// next wake continues.
const CROSS_ROWS_PER_SIDE: u16 = 32;

/// The crossing prefix of a book against itself: total matchable size, the
/// gross quote of each leg, and the (deduped, capped) makers it touches.
///
/// Two-pointer walk over the two sides, best-first — exactly the orders the
/// executor's two legs will consume. Both sides come from the book's own
/// `quote_l3_v0`, which has already applied its rules about which of its
/// orders are matchable right now, so nothing here reads the market account.
///
/// Taker-origin rows are dropped rather than stopping the walk. A remainder
/// is withheld from the crank's own legs, so a leg sized to include its base
/// comes back short and the two legs imbalance. That would leave the ordinary
/// cross resting in *front* of the remainder unable to clear for as long as
/// the remainder is there.
///
/// A crossed remainder is [`stage_taker_origin_cross`]'s to resolve, at the
/// counterparty's price. Whatever rests behind it is an ordinary cross and
/// stays in.
fn cross_prefix(
    bid_rows: &[crate::state::prop_amm::L3RowV0],
    ask_rows: &[crate::state::prop_amm::L3RowV0],
) -> ClobCross {
    let base_precision = crate::math::constants::BASE_PRECISION_U64 as u128;
    let mut cross = ClobCross {
        size: 0,
        buy_quote: 0,
        sell_quote: 0,
        makers: Vec::new(),
    };
    let crossable = |rows: &[crate::state::prop_amm::L3RowV0]| -> Vec<_> {
        rows.iter()
            .copied()
            .filter(|row| row.flags & crate::state::prop_amm::L3_ROW_FLAG_TAKER_ORIGIN == 0)
            .collect()
    };
    let (bids, asks) = (crossable(bid_rows), crossable(ask_rows));

    // `Σ price·base` per leg, divided into quote units once at the end.
    let (mut scaled_buy, mut scaled_sell) = (0u128, 0u128);
    let (mut bid_index, mut ask_index) = (0usize, 0usize);
    let mut bid_remaining = bids.first().map(|row| row.size).unwrap_or(0);
    let mut ask_remaining = asks.first().map(|row| row.size).unwrap_or(0);
    while let (Some(bid_row), Some(ask_row)) = (bids.get(bid_index), asks.get(ask_index)) {
        if bid_row.price < ask_row.price {
            break;
        }
        // Admit both makers before taking; stop at the cap instead of
        // taking size whose maker is not staged.
        let admit = |user: crate::state::prop_amm::ClobUserRefV0,
                     makers: &mut Vec<crate::state::prop_amm::ClobUserRefV0>| {
            if makers.contains(&user) {
                true
            } else if makers.len() < MAX_CROSS_MAKERS {
                makers.push(user);
                true
            } else {
                false
            }
        };
        if !admit(bid_row.user, &mut cross.makers) || !admit(ask_row.user, &mut cross.makers) {
            break;
        }

        let take = bid_remaining.min(ask_remaining);
        cross.size = cross.size.saturating_add(take);
        // The products accumulate and the division happens once, after the
        // walk. `Σ(price·take)/precision` is the same number as the sum of the
        // per-row quotients only up to rounding, and it is the cheaper one:
        // u128 division is a helper call on this target, and this loop runs
        // once per row on both sides.
        scaled_buy = scaled_buy.saturating_add(ask_row.price as u128 * take as u128);
        scaled_sell = scaled_sell.saturating_add(bid_row.price as u128 * take as u128);

        bid_remaining -= take;
        ask_remaining -= take;
        if bid_remaining == 0 {
            bid_index += 1;
            bid_remaining = bids.get(bid_index).map(|row| row.size).unwrap_or(0);
        }
        if ask_remaining == 0 {
            ask_index += 1;
            ask_remaining = asks.get(ask_index).map(|row| row.size).unwrap_or(0);
        }
    }
    cross.buy_quote = scaled_buy / base_precision;
    cross.sell_quote = scaled_sell / base_precision;
    cross
}

/// Quote both sides of the market's book and cross them against each other.
fn find_clob_cross(ctx: &Context<ResolveClobCrank>) -> Result<ClobCross> {
    let market_index = ctx.accounts.crank_conditions.load()?.market_index;
    let book_slot = ctx.accounts.quoter_slab.clob_slot(market_index)?;
    if !book_slot.quotes() {
        // A killed or unvetted book has no cross to stage; the conditions go
        // quiet rather than erroring forever.
        return Ok(cross_prefix(&[], &[]));
    }
    let quoter = &book_slot.config;
    let accounts = [
        ctx.accounts.clob_market.to_account_info(),
        ctx.accounts.clob_program.to_account_info(),
    ];
    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    // One side at a time: both responses land in the same region of the book's
    // response tail, so the first is copied out before the second CPI
    // overwrites it. A buyer consumes the asks.
    let asks = super::helpers::crank_common::book_l3_side(
        quoter,
        &ctx.accounts.quoter_slab,
        market_index,
        crate::state::prop_amm::Direction::Long,
        CROSS_ROWS_PER_SIDE,
        &accounts,
        &mut cpi_scratch,
        false,
        |row| *row,
    )?
    .unwrap_or_default();
    let bids = super::helpers::crank_common::book_l3_side(
        quoter,
        &ctx.accounts.quoter_slab,
        market_index,
        crate::state::prop_amm::Direction::Short,
        CROSS_ROWS_PER_SIDE,
        &accounts,
        &mut cpi_scratch,
        false,
        |row| *row,
    )?
    .unwrap_or_default();
    Ok(cross_prefix(&bids, &asks))
}

/// The generic quoter-cross resolver's accounts. The entry's registered
/// quote surface (plus its program) rides `remaining_accounts` — registered
/// per condition at attach time, so it is whatever `quote_v0` needs for
/// *any* Custom quoter. Nothing here is program-specific.
#[derive(Accounts)]
pub struct ResolveCrankCrossMatchQuoter<'info> {
    /// The shared staging account, index 0 by convention — a resolver's
    /// response pointer is interpreted against it.
    #[account(mut, seeds = [crate::state::relay_scratch::RELAY_SCRATCH_PDA_SEED], bump)]
    pub scratch: AccountLoader<'info, crate::state::relay_scratch::RelayScratchV0>,
    /// Read-only: resolvers stage into the shared scratch account.
    pub cross_conditions: AccountLoader<'info, crate::state::quoter_cross::QuoterCrossConditionsV0>,
    /// CHECK: locked to the CLOB book captured at attach. Writable for the
    /// book's response tail, which is where `quote_l3_v0` streams the resting
    /// orders this resolver crosses the entry against.
    #[account(mut, address = cross_conditions.load()?.clob_market)]
    pub clob_market: UncheckedAccount<'info>,
    pub state: AccountLoader<'info, State>,
    /// The market's slab: both legs' approved configs — the entry the
    /// conditions name, and the book at slot 0.
    #[account(
        seeds = [
            crate::state::prop_amm::QUOTER_SLAB_PDA_SEED,
            cross_conditions.load()?.market_index.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub quoter_slab: AccountLoader<'info, crate::state::prop_amm::QuoterSlabV0>,
    /// The entry's quoted user — the maker every staged balance change
    /// lands on; its identity derives the staged `(User, UserStats)` pair.
    /// Checked against the entry's approved config in the handler.
    pub user: AccountLoader<'info, User>,
    /// CHECK: locked to the program the CLOB entry was registered with.
    #[account(address = cross_conditions.load()?.clob_program)]
    pub clob_program: UncheckedAccount<'info>,
}

/// Discover a cross between a Custom quoter and the CLOB *generically*: CPI
/// the entry's registered `quote_v0` (the same interface every fill uses —
/// resolvers only run under simulation, so the CPI is free), walk the
/// CLOB's bytes against the returned levels in both directions, and stage
/// `crank_cross_match` for the profitable side. Works for any quoter
/// program with a registry entry; velocity carries no per-program code.
pub fn handle_resolve_crank_cross_match_quoter<'info>(
    ctx: Context<'info, ResolveCrankCrossMatchQuoter<'info>>,
) -> Result<()> {
    resolve_into(&ctx.accounts.scratch, || {
        let quoter_entry_key = ctx.accounts.cross_conditions.load()?.quoter;
        let slots = ctx.accounts.quoter_slab.slots()?;
        let Some(quoter_slot_index) =
            cross_entry_slot(&slots, &quoter_entry_key, ctx.accounts.user.key())?
        else {
            return Ok(None);
        };
        let quoter = &slots[quoter_slot_index].config;
        let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
        // The resolver's own tail, searched rather than indexed: it is a
        // handful of accounts and this reads a few of them.
        let accounts: Vec<AccountInfo<'info>> = ctx.remaining_accounts.to_vec();

        let market_index = ctx.accounts.cross_conditions.load()?.market_index;
        let (quoter_asks, quoter_bids) = quote_entry_sides(
            quoter,
            &ctx.accounts.quoter_slab,
            market_index,
            &accounts,
            &mut cpi_scratch,
        )?;

        let maker_ref = {
            let user = crate::load!(ctx.accounts.user)?;
            user.clob_user_ref()
        };

        // Both cross directions against the book, quoted the same way the
        // entry was; keep the better one. buy leg = the entry whose ask is
        // consumed.
        //
        // One side at a time: both responses land in the same region of the
        // book's response tail, so the first is copied out before the second
        // CPI overwrites it.
        let clob_accounts = [
            ctx.accounts.clob_market.to_account_info(),
            ctx.accounts.clob_program.to_account_info(),
        ];
        let book_slot = crate::state::prop_amm::clob_slot_index(&slots).ok_or_else(|| {
            msg!("quoter slab holds no book slot");
            error!(ErrorCode::QuoterNotOnSlab)
        })?;
        let mut clob_book = |direction: crate::state::prop_amm::Direction| -> Result<Vec<_>> {
            Ok(super::helpers::crank_common::book_l3_side(
                &slots[book_slot].config,
                &ctx.accounts.quoter_slab,
                market_index,
                direction,
                CROSS_ROWS_PER_SIDE,
                &clob_accounts,
                &mut cpi_scratch,
                false,
                |row| *row,
            )?
            .unwrap_or_default())
        };
        // The entry's asks cross the book's bids, which is what a seller
        // consumes.
        let a = find_quoter_clob_cross(
            &clob_book(crate::state::prop_amm::Direction::Short)?,
            &quoter_asks,
            true,
        )?;
        let b = find_quoter_clob_cross(
            &clob_book(crate::state::prop_amm::Direction::Long)?,
            &quoter_bids,
            false,
        )?;
        // Whichever direction pays better. The crank names no legs: each of
        // its two fills routes across every source the tail carries, so the
        // direction only decides how big a cross the resolver claims.
        let cross = if a.surplus(&ctx.accounts.state)? >= b.surplus(&ctx.accounts.state)? {
            a
        } else {
            b
        };
        if cross.size == 0 || cross.surplus(&ctx.accounts.state)? == 0 {
            return Ok(None);
        }

        Ok(Some(stage_quoter_cross(
            &ctx,
            quoter,
            &cross,
            maker_ref,
            market_index,
        )?))
    })
}

/// The slab slot of the entry the cross conditions name.
///
/// `None` when the entry has no discoverable work. The staged user must be the
/// entry's own quoted user, because every staged balance change lands on it.
fn cross_entry_slot(
    slots: &[crate::state::prop_amm::QuoterSlotV0],
    entry: &Pubkey,
    user: Pubkey,
) -> Result<Option<usize>> {
    let Some(quoter_slot_index) = crate::state::prop_amm::slot_for_entry(slots, entry) else {
        // A revoked quoter has no discoverable work; the conditions go
        // quiet rather than erroring forever.
        return Ok(None);
    };
    let quoter_slot = &slots[quoter_slot_index];
    if !quoter_slot.quotes() {
        // A killed or suspended quoter has no discoverable work either.
        return Ok(None);
    }
    validate!(
        user == quoter_slot.config.user,
        ErrorCode::InvalidQuoterConfig,
        "user {} is not the entry's quoted user",
        user
    )?;
    Ok(Some(quoter_slot_index))
}

/// The entry's asks and bids, sanitized best-first.
///
/// An empty user set is discovery mode (unrestricted). There is no taker: the
/// executor's taker is the protocol User, which quotes nothing anywhere.
fn quote_entry_sides<'info>(
    quoter: &crate::state::prop_amm::QuoterConfigV0,
    quoter_slab: &AccountLoader<'info, QuoterSlabV0>,
    market_index: u16,
    accounts: &[AccountInfo<'info>],
    cpi_scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
) -> Result<(Vec<PriceLevel>, Vec<PriceLevel>)> {
    let mut quote = |direction: crate::state::prop_amm::Direction| -> Result<Vec<PriceLevel>> {
        let located = quoter.quote_in_place(
            market_index,
            crate::state::prop_amm::QuoteArgsV0 {
                // The crank's taker is the protocol User and the legs it
                // matches are the book's own; it constrains no one.
                caps: crate::state::prop_amm::QuoterUserCapsV0::EMPTY,
                // No budgets to price, so nothing reads this.
                reference_price: 0,
                direction,
                size: u64::MAX / 2,
                users: &[],
                taker: None,
                // A cross is found by comparing the two sides, so
                // neither side has a price to stop at until the other
                // has been read.
                limit_price: 0,
                // A crank's discovery read: what it stages settles only
                // orders that rested through placement.
                taker_served_window: true,
                // The depth a taker remainder claims is that taker's
                // improvement, not arbitrage for the protocol to middle,
                // so this crank reads the book without it.
                consume_reservation: false,
                self_base_room: u64::MAX,
            },
            quoter_slab,
            accounts,
            cpi_scratch,
        )?;
        // Read where the quoter wrote it and copied once, into this side's
        // own list. There is only one ladder alive at a time here, so this
        // read needs none of the pooling a route's quote does. The crank
        // routes the book against itself, so the response can never fall
        // short of a caller-supplied user set.
        let data = located.borrow()?;
        let response = located.checked_quote_response(&data, direction)?;
        Ok(crate::state::prop_amm::usable_levels(response.levels).to_vec())
    };
    let quoter_asks = sanitize_levels(quote(crate::state::prop_amm::Direction::Long)?, true);
    let quoter_bids = sanitize_levels(quote(crate::state::prop_amm::Direction::Short)?, false);
    Ok((quoter_asks, quoter_bids))
}

/// Stage the `crank_cross_match` executor for a discovered quoter-against-CLOB
/// cross.
fn stage_quoter_cross<'info>(
    ctx: &Context<'info, ResolveCrankCrossMatchQuoter<'info>>,
    quoter: &crate::state::prop_amm::QuoterConfigV0,
    cross: &QuoterCross,
    maker_ref: crate::state::prop_amm::ClobUserRefV0,
    market_index: u16,
) -> Result<StagedCall> {
    let (oracle, quote_spot_market_index, clob_program) = {
        let conditions = ctx.accounts.cross_conditions.load()?;
        (
            conditions.oracle,
            conditions.quote_spot_market_index,
            conditions.clob_program,
        )
    };
    let (protocol_user, protocol_user_stats) = pdas::protocol_user_pair();

    let call = crate::staged_call!(CrankCrossMatch {
        state: ctx.accounts.state.key(),
        authority: pdas::keeper_placeholder(),
        taker: protocol_user,
        taker_stats: protocol_user_stats,
        crank_conditions: pdas::clob_crank_conditions(market_index),
        perp_market: pdas::perp_market(market_index),
        quoter_slab: ctx.accounts.quoter_slab.key(),
        instructions_sysvar: IX_ID,
    })
    .map_section_named_perp(oracle, quote_spot_market_index);
    // Maker pairs: the quoter's user first, then the CLOB-side makers.
    let mut staged = vec![maker_ref];
    for maker in &cross.makers {
        if !staged.contains(maker) {
            staged.push(*maker);
        }
    }
    // The union of every source's surfaces: the market's slab, the CLOB's
    // book, and everything the quoter registered, programs included. Each
    // leg assembles its route from this tail, so the slab rides it as well
    // as being named — a route without the slab consults nothing external.
    let mut call = call.maker_refs(staged.iter().copied());
    let mut union: std::collections::BTreeMap<Pubkey, bool> = Default::default();
    union.entry(ctx.accounts.quoter_slab.key()).or_default();
    *union.entry(ctx.accounts.clob_market.key()).or_default() |= true;
    union.entry(clob_program).or_default();
    for meta in quoter.leg_metas(quoter.execute_leg_indexes())? {
        *union.entry(meta.pubkey).or_default() |= meta.is_writable;
    }
    *union.entry(quoter.response_account).or_default() |= true;
    union.entry(quoter.program_id).or_default();
    for (key, writable) in union.iter() {
        call = call.account(*key, *writable);
    }
    call.arg(CrankCrossMatchArgs {
        market_index,
        size: cross.size,
    })
}

/// A quoter-vs-CLOB crossing prefix. `quoter_is_ask_side` selects which
/// legs cross: the quoter's asks against the CLOB's bids, or the CLOB's
/// asks against the quoter's bids.
struct QuoterCross {
    size: u64,
    buy_quote: u128,
    sell_quote: u128,
    makers: Vec<crate::state::prop_amm::ClobUserRefV0>,
}

impl QuoterCross {
    /// After-fee surplus at the top (most conservative) taker-fee tier on
    /// both legs; zero when the cross is inside the fee gulf.
    fn surplus(&self, state: &AccountLoader<State>) -> Result<u128> {
        if self.size == 0 {
            return Ok(0);
        }
        let (fee_numerator, fee_denominator) = {
            let state = state.load()?;
            let tier = state.perp_fee_structure.fee_tiers[0];
            (
                tier.fee_numerator as u128,
                (tier.fee_denominator as u128).max(1),
            )
        };
        let fees = (self.buy_quote * fee_numerator).div_ceil(fee_denominator)
            + (self.sell_quote * fee_numerator).div_ceil(fee_denominator);
        Ok(self
            .sell_quote
            .saturating_sub(self.buy_quote.saturating_add(fees)))
    }
}

/// The longest usable best-first prefix of an untrusted `quote_v0` book:
/// positive prices/sizes, monotone (ascending asks / descending bids),
/// truncated at the first violation — the router's sanitization rule.
fn sanitize_levels(levels: Vec<PriceLevel>, ascending: bool) -> Vec<PriceLevel> {
    let mut out: Vec<PriceLevel> = Vec::with_capacity(levels.len());
    for level in levels {
        if level.price == 0 || level.size == 0 {
            break;
        }
        if let Some(previous) = out.last() {
            let monotone = if ascending {
                level.price >= previous.price
            } else {
                level.price <= previous.price
            };
            if !monotone {
                break;
            }
        }
        out.push(level);
    }
    out
}

/// Walk the quoter's (sanitized) levels against the CLOB's resting rows:
/// `quoter_is_ask_side` crosses quoter asks with CLOB bids (CLOB bid price
/// >= quoter ask price), else CLOB asks with quoter bids.
fn find_quoter_clob_cross(
    clob_rows: &[crate::state::prop_amm::L3RowV0],
    quoter_levels: &[PriceLevel],
    quoter_is_ask_side: bool,
) -> Result<QuoterCross> {
    let base_precision = crate::math::constants::BASE_PRECISION_U64 as u128;
    let mut cross = QuoterCross {
        size: 0,
        buy_quote: 0,
        sell_quote: 0,
        makers: Vec::new(),
    };
    let mut rows = clob_rows.iter();
    let mut row = rows.next();
    let mut clob_remaining = row.map(|row| row.size).unwrap_or(0);
    let mut levels = quoter_levels.iter();
    let mut level = levels.next();
    let mut level_remaining = level.map(|l| l.size).unwrap_or(0);

    while let (Some(r), Some(l)) = (row, level) {
        let crossed = if quoter_is_ask_side {
            r.price >= l.price
        } else {
            l.price >= r.price
        };
        if !crossed {
            break;
        }
        // Reserve one maker slot for the quoter's user (staged first).
        if !cross.makers.contains(&r.user) {
            if cross.makers.len() + 1 >= MAX_CROSS_MAKERS {
                break;
            }
            cross.makers.push(r.user);
        }
        let take = clob_remaining.min(level_remaining);
        let (ask_price, bid_price) = if quoter_is_ask_side {
            (l.price, r.price)
        } else {
            (r.price, l.price)
        };
        cross.size = cross.size.saturating_add(take);
        cross.buy_quote = cross
            .buy_quote
            .saturating_add(ask_price as u128 * take as u128 / base_precision);
        cross.sell_quote = cross
            .sell_quote
            .saturating_add(bid_price as u128 * take as u128 / base_precision);
        clob_remaining -= take;
        level_remaining -= take;
        if clob_remaining == 0 {
            row = rows.next();
            clob_remaining = row.map(|row| row.size).unwrap_or(0);
        }
        if level_remaining == 0 {
            level = levels.next();
            level_remaining = level.map(|l| l.size).unwrap_or(0);
        }
    }
    Ok(cross)
}
