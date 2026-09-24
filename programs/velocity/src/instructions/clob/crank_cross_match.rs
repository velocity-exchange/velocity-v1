//! Cross-match crank: match two crossed resting sources against each other.
//!
//! Nothing else matches two resting sources. Router matching happens only when
//! a taker fills through, so a CLOB bid at or above a CLOB ask rests crossed
//! until a taker clears it. A maker place that is not post-only rests crossed
//! instead of refusing, which makes this case routine, and the orders it
//! strands are real user orders. That is the work this crank exists for. A
//! PropAMM quoting through the CLOB's best has the same shape but not the same
//! duty. A PropAMM keeps no resting order, so it strands nothing of its own,
//! and the rule below governs when the crank may settle it.
//!
//! The crank is two ordinary router fills. The protocol `User` buys `size` as a
//! taker, then sells exactly what the buy filled. Each leg routes across every
//! source the transaction carries. Running the cross this way gives it
//! everything a fill has: the vAMM's last look, a PropAMM that can cross the
//! book, the pre-execute margin clamp on unreserved depth, reduce-only trigger
//! cancellation, and the shared post-fill margin, equity-floor and
//! open-interest checks.
//!
//! Three requirements make the pair a cross rather than two sweeps. The legs
//! must match the same base, so the protocol ends flat. Every unit must have
//! crossed, which the worst price of each leg states exactly. The highest price
//! the buy leg paid must be at or under the lowest price the sell leg received.
//! The quote the protocol keeps must clear the market's floor, so a cross the
//! reservoir pays for never nets less than it costs to land. A cross inside the
//! fee gulf rests instead.
//!
//! The surplus lands in the protocol `User`, which the crank incentive loop
//! drains. The caller's `authority` is paid reservoir lamports. No signature is
//! required anywhere, because relay turners submit executors unsigned.
//!
//! A taker-origin order needs no guard here. Such an order reserves the depth
//! it crosses, and reserved depth leaves the book's matchable set for every
//! caller that does not pass `include_taker_origin_reservations`. This crank
//! passes `false` at every site, so it cannot reach that cover at any size the
//! caller asks for. `crank_taker_origin_cross` owes the taker that improvement
//! and is the only caller that can reach it.
//!
//! The crank reports protected flow (`taker_served_window`) only when it
//! measured how long every source that can fill has rested. A book records the
//! slot each order was placed in, so the crank can measure it. A `Custom` quoter
//! prices during the call and keeps no resting order, so the crank can measure
//! nothing and reports nothing. `consults_custom_quoter` states the rule. A book
//! with a speed bump then quotes the cross no depth, so a PropAMM cross settles
//! only on a book that runs no speed bump.

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
            optional_accounts::AccountMaps,
            relay_harness::{resolve_into, StagedCall},
            route_direction, FillerTerms, RouteFill, RouteMark, RouteRequest, RoutedOrder,
        },
        load, load_mut,
        math::{casting::Cast, constants::MARGIN_PRECISION_U128, safe_math::SafeMath},
        msg,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            fill_mode::FillMode,
            pdas,
            perp_market_map::MarketSet,
            prop_amm::{
                DirectionV0, PriceLevelV0, QuoterCpiScratch, QuoterSlabExt, QuoterSlabV0,
                QuoterType,
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
    /// filled, and the cross is refused unless every unit of it crossed. A size
    /// past the crossing depth therefore fails rather than sweeping through it.
    pub size: u64,
}

#[derive(Accounts)]
#[instruction(args: CrankCrossMatchArgs)]
pub struct CrankCrossMatch<'info> {
    pub state: AccountLoader<'info, State>,
    /// CHECK: the lamport payout target, relay's keeper-placeholder slot. No
    /// signature is required. The cross's own profitability rules are the gate.
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
    /// The crossed market. It is named rather than read out of the maps
    /// section, because the cross serves one market.
    #[account(
        mut,
        seeds = [b"perp_market", args.market_index.to_le_bytes().as_ref()],
        bump,
        has_one = quoter_slab
    )]
    pub perp_market: AccountLoader<'info, crate::state::perp_market::PerpMarket>,
    /// The market's approved quoters. The market's `has_one` binds it, which
    /// costs a memcmp where a seeds constraint pays a PDA derivation. Each leg
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

    // The perp market is a named account rather than one of the maps section's usual entries,
    // so the oracle and spot maps are loaded directly above and the bundle is built here
    // instead of by `load_maps`.
    let mut maps = AccountMaps {
        perp_market_map,
        spot_market_map,
        oracle_map,
    };
    let (makers_and_referrer, makers_and_referrer_stats) =
        load_user_maps(remaining_accounts_iter, true)?;
    // The protocol taker is loaded by name and mutated for the length of each
    // leg. Repeating it in the maker section fails the leg on a borrow, which
    // reports nothing about the cause.
    validate!(
        !makers_and_referrer
            .0
            .contains_key(&ctx.accounts.taker.key()),
        ErrorCode::InvalidMaker,
        "the maker section must not repeat the protocol taker"
    )?;

    let tail_from = ctx.remaining_accounts.len() - remaining_accounts_iter.len();
    let tail = &ctx.remaining_accounts[tail_from..];

    // Both legs are held to the oracle rules an ordinary fill applies, before
    // either of them moves a position. The pre-flight refuses a market in
    // settlement, paused fills, an oracle that may not price a match, and a
    // mark outside the market's band.
    let (band_oracle_price, margin_ratio_initial) = {
        let market = &mut maps.perp_market_map.get_ref_mut(&market_index)?;
        crank_oracle_preflight(market, &state, &mut maps.oracle_map, &clock, "cross match")?;
        let oracle_id = market.oracle_id();
        let margin_ratio_initial = market.margin_ratio_initial;
        (
            maps.oracle_map.get_price_data(&oracle_id)?.price,
            margin_ratio_initial,
        )
    };

    let base_before = taker_base(&ctx.accounts.taker, market_index)?;

    // One set of CPI buffers for the whole crank, as a router fill uses. Both
    // legs refill them in turn. A set per leg would be an allocation per leg,
    // and the runtime's allocator never gives one back.
    let mut cpi_scratch = QuoterCpiScratch::new();
    let cx = CrossMatchContext {
        accounts: &ctx.accounts,
        tail,
        state: &state,
        market_index,
        band_oracle_price,
        margin_ratio_initial,
        makers_and_referrer: &makers_and_referrer,
        makers_and_referrer_stats: &makers_and_referrer_stats,
        clock: &clock,
    };

    let buy = run_cross_leg(
        &cx,
        PositionDirection::Long,
        size,
        &mut maps,
        &mut cpi_scratch,
    )?;
    let sell = run_cross_leg(
        &cx,
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
struct CrossMatchContext<'a, 'info> {
    accounts: &'a CrankCrossMatch<'info>,
    /// The market's slab plus the union of the consulted quoters' registered
    /// accounts, the same shape a router fill carries.
    tail: &'info [AccountInfo<'info>],
    state: &'a State,
    market_index: u16,
    /// The oracle price the fill's own maker band measures against. Each leg
    /// bounds itself at the edge of that band.
    band_oracle_price: i64,
    /// The market's own band, in MARGIN_PRECISION units.
    margin_ratio_initial: u32,
    makers_and_referrer: &'a UserMap<'info>,
    makers_and_referrer_stats: &'a UserStatsMap<'info>,
    clock: &'a Clock,
}

/// What one leg of a cross filled.
struct CrossLegFill {
    base_filled: u64,
    /// What the leg did to the protocol `User`'s quote, net of the taker fee
    /// it paid. Read on both sides of the fill with funding already settled,
    /// so a funding payment cannot read as cross surplus.
    quote_delta: i64,
    /// The worst price any single source of this leg executed at. Zero when
    /// the leg filled nothing.
    worst_price: u64,
}

/// The protocol taker's base in this market, or zero when it holds no position
/// there yet.
fn taker_base(taker: &AccountLoader<User>, market_index: u16) -> Result<i64> {
    Ok(load!(taker)?
        .get_perp_position(market_index)
        .map(|position| position.base_asset_amount)
        .unwrap_or(0))
}

/// allow-verbose: states why a `Custom` quoter must gate protected flow, a fact the boolean
/// return does not carry on its own.
///
/// Whether this cross routes to a quoter that prices on demand.
///
/// A `Custom` quoter computes its price during the call, so it keeps no resting order to measure
/// rest against and can move its price in the slot the crank runs in. This crank must not report
/// protected flow while such a quoter is in the route: a quoter that repriced this slot gave the
/// makers no time to answer, so a book with a speed bump quotes the cross nothing, the same
/// protection an unattested taker gets. A `Vamm` slot does not count, since it is velocity's own
/// liquidity applying its own last look.
fn consults_custom_quoter<'info>(
    quoter_slab: &AccountLoader<'info, QuoterSlabV0>,
    tail: &'info [AccountInfo<'info>],
) -> Result<bool> {
    let consulted = quoter_slab.consulted_slots(tail)?;
    let slots = quoter_slab.slots()?;
    Ok(consulted.iter().any(|&index| {
        slots[index].quotes() && slots[index].config.quoter_type == QuoterType::Custom
    }))
}

/// Run one leg of a cross as an ordinary router fill.
///
/// The protocol `User` is its own filler, so no reward comes out of the taker
/// fee it pays, and there is no builder escrow to accrue against.
fn run_cross_leg<'info>(
    cx: &CrossMatchContext<'_, 'info>,
    taker_direction: PositionDirection,
    size: u64,
    maps: &mut AccountMaps<'info>,
    cpi_scratch: &mut QuoterCpiScratch<'info>,
) -> Result<CrossLegFill> {
    if size == 0 {
        return Ok(CrossLegFill {
            base_filled: 0,
            quote_delta: 0,
            worst_price: 0,
        });
    }

    let limit_price = leg_limit_price(
        taker_direction,
        cx.band_oracle_price,
        cx.margin_ratio_initial,
    )?;

    // Funding is settled here, before the quote is read. The fill settles it
    // too, so by the time the fill runs there is nothing left to charge, and the
    // quote the leg is measured on moves for the fill alone. Without this call,
    // a funding payment reads as cross surplus. That includes the payment the
    // first leg's own funding update creates for the second leg.
    let (order_id, taker_ref, quote_before) = {
        let mut market = maps.perp_market_map.get_ref_mut(&cx.market_index)?;
        let mut taker = load_mut!(cx.accounts.taker)?;
        settle_funding_payment(
            &mut taker,
            &cx.accounts.taker.key(),
            &mut market,
            cx.clock.unix_timestamp,
        )?;

        let order_id = crate::get_then_update_id!(taker, next_order_id);
        let quote_before = taker
            .get_perp_position(cx.market_index)
            .map(|position| position.quote_asset_amount)
            .unwrap_or(0);
        (order_id, taker.clob_user_ref(), quote_before)
    };

    let mut order = Order {
        slot: cx.clock.slot,
        order_id,
        market_index: cx.market_index,
        status: OrderStatus::Open,
        order_type: OrderType::Limit,
        market_type: MarketType::Perp,
        direction: taker_direction,
        base_asset_amount: size,
        price: limit_price,
        existing_position_direction: taker_direction,
        ..Order::default()
    };

    // A cheap check first. When the route consults a `Custom` quoter, no window was served,
    // because a prop-AMM has no rest to wait out. This skips the expensive CPI that checking
    // rested depth on the other quoters would otherwise require.
    let taker_served_window = if consults_custom_quoter(&cx.accounts.quoter_slab, cx.tail)? {
        false
    } else {
        // Since this is not servicing taker-origin trades, both sides must have served the window
        // to get the `taker_served_window` set.
        [DirectionV0::Long, DirectionV0::Short]
            .iter()
            .copied()
            .try_fold(true, |served, side| -> Result<bool> {
                Ok(served
                    && super::helpers::crank_common::book_side_rested(
                        &cx.accounts.quoter_slab,
                        cx.tail,
                        cx.market_index,
                        side,
                        size,
                        cx.clock.slot,
                        cpi_scratch,
                    )?)
            })?
    };
    let filled = RouteFill {
        state: cx.state,
        clock: cx.clock,
        tail: cx.tail,
        scratch: cpi_scratch,
    }
    .run(
        RouteRequest {
            order: RoutedOrder {
                direction: route_direction(taker_direction),
                unfilled: size,
                taker: taker_ref,
                limit_price,
                mark: RouteMark {
                    reference_price: cx.band_oracle_price,
                    margin_ratio_initial: cx.margin_ratio_initial,
                },
            },
            taker_served_window,
            // The depth a taker-origin order reserves is that taker's
            // improvement, not arbitrage for the protocol to middle. Reading
            // the book without it stops this crank reaching that cover.
            include_taker_origin_reservations: false,
            claim: None,
            // The taker is the protocol, so nobody is owed a maker. A book that
            // stops at an owner the transaction does not carry only makes the
            // cross smaller, and the surplus floor decides if it is worth landing.
            filler: FillerTerms {
                taker_exposure_closed_by_caller: true,
                ..FillerTerms::keeper(Some(&cx.accounts.instructions_sysvar.to_account_info()))?
            },
        },
        controller::orders::FillRequest {
            order: &mut order,
            reserved: false,
            mode: FillMode::Fill,
            referrer_is_accelerated: false,
        },
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
    )?;

    // Read straight after the fill, with no second settle. The fill's own
    // funding update belongs to whoever holds the position next, and the next
    // leg settles it before it starts measuring.
    let quote_after = load!(cx.accounts.taker)?
        .get_perp_position(cx.market_index)
        .map(|position| position.quote_asset_amount)
        .unwrap_or(0);
    Ok(CrossLegFill {
        base_filled: filled.amounts.base,
        quote_delta: quote_after.safe_sub(quote_before)?,
        worst_price: filled.worst_fill_price.unwrap_or(0),
    })
}

/// The edge of the market's maker oracle band, on the side the leg buys or
/// sells at.
///
/// A cross leg brings no price of its own. The crossed prices are what it
/// exists to reach, and they are not known until both sides are read. The band
/// is the widest price the fill itself settles a maker at, so it is the widest
/// bound that discards nothing. It still bounds the leg, which keeps a quoter's
/// walk and the vAMM ladder off levels the fill would refuse.
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

/// The base both legs matched, and the quote the protocol kept for it.
#[derive(Debug)]
struct CrossSurplus {
    base_matched: u64,
    surplus: i64,
}

/// The three rules that make a pair of fills a cross.
///
/// The legs must match the same base and the taker's base must return to where
/// it started, so the protocol ends flat and carries no position out of the
/// crank.
///
/// Every unit must have crossed. The worst price of each leg states that
/// exactly. The highest price the buy leg paid must be at or under the lowest
/// price the sell leg received. A size past the crossing depth fails on this
/// rule. Without it, a caller sizes past that depth and takes the loss-making
/// tail through its own resting orders at intermediate prices, and the totals
/// still clear the floor. The rule is therefore stronger than the floor. Both
/// prices are floored the same way, so a cross whose edge is inside one unit of
/// quote is refused. The floor already asks for more than that cross pays.
///
/// The two legs' quote deltas add up to the crossed spread net of both legs'
/// taker fees. The sum must be positive and at or above the market's floor, so
/// the protocol never runs a losing cross and never pays reservoir lamports for
/// one worth less than the payment.
fn validate_cross_legs(
    buy: &CrossLegFill,
    sell: &CrossLegFill,
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
/// The floor covers the keeper's lamport payment valued in quote, so a cross the
/// reservoir pays for never nets the protocol less than it costs to land. The
/// two figures are in different units, and the SOL oracle converts between them.
/// When no SOL market rides the crank, or its oracle is unusable, the admin's
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
    let payment_quote =
        crate::state::clob_crank::sol_oracle_price(state, spot_market_map, oracle_map)
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

/// The cross and activation conditions' answer: a crossed taker remainder if
/// the book has one, otherwise a maker-against-maker cross worth taking.
///
/// For the maker-against-maker case, this finds the book's crossing prefix,
/// estimates profitability with the most conservative taker-fee tier on both
/// legs, and stages the `crank_cross_match` executor. The staged accounts are
/// full `(User, UserStats)` pairs, both derived from the node's
/// `(authority, sub_account_id)` identity. Only a CLOB-against-CLOB cross is
/// discoverable here. A PropAMM crossing the CLOB belongs to the generic
/// quoter-cross resolver, which CPIs `quote_v0` through the entry's registered
/// surface, with the book publisher as the fast path. The executor verifies
/// profitability exactly either way.
///
/// A taker-origin cross is looked for first and staged as
/// `crank_taker_origin_cross` instead. A `ResolvedCrankV0` names its own
/// executor, so serving both from one condition costs no extra slot and no
/// second wake. The wakes that find a maker-against-maker cross are the same
/// ones that find a taker-origin cross. See [`stage_taker_origin_cross`]. The
/// order is the economics. The improvement between the two prices belongs to
/// the order that came to trade, so it is handed over before the protocol
/// middles the same crossed book as arbitrage.
pub(super) fn stage_cross(ctx: &Context<ResolveClobCrank>) -> Result<Option<StagedCall>> {
    if let Some(call) = stage_taker_origin_cross(ctx)? {
        return Ok(Some(call));
    }

    let cross = find_clob_cross(ctx)?;
    if cross.size == 0 {
        return Ok(None);
    }

    // A conservative estimate at the tier-0 taker fee on both legs. The
    // executor measures the real figure. This only avoids staging obvious
    // losers.
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

    // The named accounts go through the executor's own client struct, which
    // checks their shape at compile time. The remaining sections follow: maps,
    // maker `(User, UserStats)` pairs, and the quoter section.
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
        // The slab rides the tail as well as being named. Each leg assembles
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

/// The crossing prefix of the book: total matchable size, the gross quote of
/// each leg, and the makers it touches, deduplicated and capped.
struct ClobCross {
    size: u64,
    buy_quote: u128,
    sell_quote: u128,
    makers: Vec<crate::state::prop_amm::UserRefV0>,
}

/// How deep either side of the crossing prefix is read.
///
/// The walk ends on [`MAX_CROSS_MAKERS`] makers regardless, so this only reaches past a single
/// maker's ladder. A longer prefix stages a smaller cross, continued by the next wake.
const CROSS_ROWS_PER_SIDE: u16 = 32;

/// The crossing prefix of a book against itself: total matchable size, the gross
/// quote of each leg, and the makers it touches, deduplicated and capped.
///
/// The walk uses two pointers over the two sides, best first. Those are the
/// orders the executor's two legs consume. Both sides come from the book's own
/// `quote_l3_v0`, which already applied its rules about which orders are
/// matchable now, so nothing here reads the market account.
///
/// Taker-origin rows are dropped rather than stopping the walk. A remainder is
/// withheld from the crank's own legs, so a leg sized to include its base comes
/// back short and the two legs imbalance. The ordinary cross resting in front of
/// the remainder could then not clear for as long as the remainder is there.
///
/// [`stage_taker_origin_cross`] resolves a crossed remainder at the
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

    // The sum of price times base per leg. It converts to quote units once, at
    // the end.
    let (mut scaled_buy, mut scaled_sell) = (0u128, 0u128);
    let (mut bid_index, mut ask_index) = (0usize, 0usize);
    let mut bid_remaining = bids.first().map(|row| row.size).unwrap_or(0);
    let mut ask_remaining = asks.first().map(|row| row.size).unwrap_or(0);
    while let (Some(bid_row), Some(ask_row)) = (bids.get(bid_index), asks.get(ask_index)) {
        if bid_row.price < ask_row.price {
            break;
        }

        // Admit both makers before taking. The walk stops at the cap rather
        // than take size whose maker is not staged.
        let admit = |user: crate::state::prop_amm::UserRefV0,
                     makers: &mut Vec<crate::state::prop_amm::UserRefV0>| {
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
        // walk. The two forms differ only by rounding, and this one is cheaper.
        // A u128 division is a helper call on this target, and the loop runs
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
        // A killed or unvetted book has no cross to stage. The conditions go
        // quiet rather than fail on every wake.
        return Ok(cross_prefix(&[], &[]));
    }

    let quoter = &*book_slot;
    let accounts = [
        ctx.accounts.clob_market.to_account_info(),
        ctx.accounts.clob_program.to_account_info(),
    ];
    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    // One side at a time. Both responses land in the same region of the book's
    // response tail, so the first is copied out before the second CPI overwrites
    // it. A buyer consumes the asks.
    let asks = super::helpers::crank_common::book_l3_side(
        quoter,
        &ctx.accounts.quoter_slab,
        market_index,
        crate::state::prop_amm::DirectionV0::Long,
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
        crate::state::prop_amm::DirectionV0::Short,
        CROSS_ROWS_PER_SIDE,
        &accounts,
        &mut cpi_scratch,
        false,
        |row| *row,
    )?
    .unwrap_or_default();
    Ok(cross_prefix(&bids, &asks))
}

/// The generic quoter-cross resolver's accounts.
///
/// The entry's registered quote surface and its program ride
/// `remaining_accounts`. They are registered per condition at attach time, so
/// they are whatever `quote_v0` needs for any Custom quoter. Nothing here is
/// specific to one program.
#[derive(Accounts)]
pub struct ResolveCrankCrossMatchQuoter<'info> {
    /// The shared staging account, at index 0 by convention. A resolver's
    /// response pointer is read against it.
    #[account(mut, seeds = [crate::state::relay_scratch::RELAY_SCRATCH_PDA_SEED], bump)]
    pub scratch: AccountLoader<'info, crate::state::relay_scratch::RelayScratchV0>,
    /// Read-only, because resolvers stage into the shared scratch account.
    pub cross_conditions: AccountLoader<'info, crate::state::quoter_cross::QuoterCrossConditionsV0>,
    /// CHECK: locked to the CLOB book captured at attach. Writable for the
    /// book's response tail, which is where `quote_l3_v0` streams the resting
    /// orders this resolver crosses the entry against.
    #[account(mut, address = cross_conditions.load()?.clob_market)]
    pub clob_market: UncheckedAccount<'info>,
    pub state: AccountLoader<'info, State>,
    /// The market's slab, which holds both legs' approved configs: the entry
    /// the conditions name, and the book at slot 0.
    #[account(
        seeds = [
            crate::state::prop_amm::QUOTER_SLAB_PDA_SEED,
            cross_conditions.load()?.market_index.to_le_bytes().as_ref(),
        ],

        bump
    )]
    pub quoter_slab: AccountLoader<'info, crate::state::prop_amm::QuoterSlabV0>,
    /// The entry's quoted user, and the maker every staged balance change lands
    /// on. Its identity derives the staged `(User, UserStats)` pair. The handler
    /// checks it against the entry's approved config.
    pub user: AccountLoader<'info, User>,
    /// CHECK: locked to the program the CLOB entry was registered with.
    #[account(address = cross_conditions.load()?.clob_program)]
    pub clob_program: UncheckedAccount<'info>,
}

/// Discover a cross between a Custom quoter and the CLOB.
///
/// This CPIs the entry's registered `quote_v0`, the same interface every fill
/// uses. Resolvers run only under simulation, so the CPI costs nothing. It then
/// walks the CLOB's bytes against the returned levels in both directions and
/// stages `crank_cross_match` for the profitable side. It works for any quoter
/// program with a registry entry, and velocity carries no per-program code.
pub fn handle_resolve_crank_cross_match_quoter<'info>(
    ctx: Context<'info, ResolveCrankCrossMatchQuoter<'info>>,
) -> Result<()> {
    // The book's own account rides writable, because `quote_l3_v0` streams the
    // answer into its response tail. It belongs to the CLOB program rather than
    // to velocity, so the check passes over it and the staging region stays
    // just the scratch account.
    crate::instructions::constraints::require_view_accounts(
        &ctx.accounts.to_account_infos(),
        &[ctx.accounts.scratch.key()],
    )?;

    resolve_into(&ctx.accounts.scratch, || {
        let quoter_entry_key = ctx.accounts.cross_conditions.load()?.quoter;
        let slots = ctx.accounts.quoter_slab.slots()?;
        let Some(quoter_slot_index) =
            cross_entry_slot(&slots, &quoter_entry_key, ctx.accounts.user.key())?
        else {
            return Ok(None);
        };

        let quoter = &slots[quoter_slot_index];
        let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
        // The resolver's own tail is searched rather than indexed. It holds a
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

        // Both cross directions run against the book, quoted the way the entry was, and the better
        // one is kept. The buy leg is the entry whose ask is consumed. One side runs at a time:
        // both responses land in the same region of the book's response tail, so the first is
        // copied out before the second CPI overwrites it.
        let clob_accounts = [
            ctx.accounts.clob_market.to_account_info(),
            ctx.accounts.clob_program.to_account_info(),
        ];
        let book_slot = crate::state::prop_amm::clob_slot_index(&slots).ok_or_else(|| {
            msg!("quoter slab holds no book slot");
            error!(ErrorCode::QuoterNotOnSlab)
        })?;

        // A `Custom` entry in the route stops the crank reporting protected
        // flow, so a book with a speed bump quotes that cross no depth and the
        // legs never cross. Report no work rather than stage a call that always
        // fails.
        if slots[book_slot].config.book_default_activation_delay_slots > 0 {
            return Ok(None);
        }

        let mut clob_book = |direction: crate::state::prop_amm::DirectionV0| -> Result<Vec<_>> {
            Ok(super::helpers::crank_common::book_l3_side(
                &slots[book_slot],
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

        let a = find_quoter_clob_cross(
            &clob_book(crate::state::prop_amm::DirectionV0::Short)?,
            &quoter_asks,
            true,
        )?;
        let b = find_quoter_clob_cross(
            &clob_book(crate::state::prop_amm::DirectionV0::Long)?,
            &quoter_bids,
            false,
        )?;

        // Keep whichever direction pays better. The crank names no legs,
        // because each of its two fills routes across every source the tail
        // carries. The direction only decides how big a cross the resolver
        // claims.
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
            &quoter.config,
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
        // A revoked quoter has no discoverable work. The conditions go quiet
        // rather than fail on every wake.
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

/// The entry's asks and bids, sanitized and ordered best first.
///
/// An empty user set means discovery mode, which restricts nothing. There is no
/// taker. The executor's taker is the protocol `User`, which quotes nothing
/// anywhere.
fn quote_entry_sides<'info>(
    quoter: &crate::state::prop_amm::QuoterSlotV0,
    quoter_slab: &AccountLoader<'info, QuoterSlabV0>,
    market_index: u16,
    accounts: &[AccountInfo<'info>],
    cpi_scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
) -> Result<(Vec<PriceLevelV0>, Vec<PriceLevelV0>)> {
    let mut quote = |direction: crate::state::prop_amm::DirectionV0| -> Result<Vec<PriceLevelV0>> {
        let located = quoter.quote_in_place(
            market_index,
            crate::state::prop_amm::QuoteArgsV0 {
                // The crank's taker is the protocol `User` and the legs it
                // matches are the book's own, so it constrains nobody.
                caps: crate::state::prop_amm::UserCapsV0::EMPTY,
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
                // A crank's discovery read. What it stages settles only orders
                // that rested through placement.
                taker_served_window: true,
                // The depth a taker-origin order reserves is that taker's
                // improvement, not arbitrage for the protocol to middle, so
                // this crank reads the book without it.
                include_taker_origin_reservations: false,
            },
            quoter_slab,
            accounts,
            cpi_scratch,
        )?;

        // The response is read where the quoter wrote it and copied once, into this side's own
        // list. Only one ladder is alive at a time here, so this read needs none of the pooling a
        // route's quote does. The crank routes the book against itself, so the response can never
        // fall short of a caller-supplied user set.
        let data = located.borrow()?;
        let response = located.checked_quote_response(&data, direction)?;
        Ok(crate::state::prop_amm::usable_levels(response.levels).to_vec())
    };
    let quoter_asks = quote(crate::state::prop_amm::DirectionV0::Long)?;
    let quoter_bids = quote(crate::state::prop_amm::DirectionV0::Short)?;
    Ok((quoter_asks, quoter_bids))
}

/// Stage the `crank_cross_match` executor for a discovered quoter-against-CLOB
/// cross.
fn stage_quoter_cross<'info>(
    ctx: &Context<'info, ResolveCrankCrossMatchQuoter<'info>>,
    quoter: &crate::state::prop_amm::QuoterConfigV0,
    cross: &QuoterCross,
    maker_ref: crate::state::prop_amm::UserRefV0,
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

    // The union of every source's surfaces: the market's slab, the CLOB's book,
    // and everything the quoter registered, programs included. Each leg
    // assembles its route from this tail, so the slab rides it as well as being
    // named. A route without the slab consults nothing external.
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

/// A crossing prefix between a quoter and the CLOB. `quoter_is_ask_side`
/// selects which legs cross: the quoter's asks against the CLOB's bids, or the
/// CLOB's asks against the quoter's bids.
struct QuoterCross {
    size: u64,
    buy_quote: u128,
    sell_quote: u128,
    makers: Vec<crate::state::prop_amm::UserRefV0>,
}

impl QuoterCross {
    /// The after-fee surplus at the most conservative taker-fee tier on both
    /// legs. It is zero when the cross is inside the fee gulf.
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

/// Walk the quoter's levels against the CLOB's resting rows.
///
/// `quoter_is_ask_side` crosses quoter asks with CLOB bids, where the CLOB bid
/// price is at or above the quoter ask price. Otherwise it crosses CLOB asks
/// with quoter bids.
fn find_quoter_clob_cross(
    clob_rows: &[crate::state::prop_amm::L3RowV0],
    quoter_levels: &[PriceLevelV0],
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

    // The sum of price times base per leg, converted to quote units once after
    // the walk. Dividing per level understates each leg by up to one quote unit
    // per level, which would price the surplus off what the executor measures.
    let (mut scaled_buy, mut scaled_sell) = (0u128, 0u128);

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
        scaled_buy = scaled_buy.saturating_add(ask_price as u128 * take as u128);
        scaled_sell = scaled_sell.saturating_add(bid_price as u128 * take as u128);
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

    cross.buy_quote = scaled_buy / base_precision;
    cross.sell_quote = scaled_sell / base_precision;

    Ok(cross)
}
