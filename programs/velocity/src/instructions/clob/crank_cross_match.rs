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
//! Four requirements make the pair a cross rather than two sweeps. The legs
//! must match the same base, so the protocol ends flat. Every unit must have
//! crossed, which the worst price of each leg states exactly. The highest price
//! the buy leg paid must be at or under the lowest price the sell leg received.
//! The quote the protocol keeps must clear the market's floor, so a cross the
//! reservoir pays for never nets less than it costs to land. A cross inside the
//! fee gulf rests instead. No authority may fill both legs, because it would pay
//! itself the spread to collect maker volume and rebates.
//!
//! The surplus lands in the protocol `User`, which the crank incentive loop
//! drains. The caller's `authority` is paid reservoir lamports when a SOL price
//! values the payment. No signature is required anywhere, because relay turners
//! submit executors unsigned.
//!
//! The cross resolver stages a taker-origin cross first. A `ResolvedCrankV0`
//! names its own executor, so one condition serves both cranks. The
//! improvement between two crossed prices belongs to the order that came to
//! trade, so it is handed over before the protocol middles the same book. A
//! taker-origin cross that stays on the book past
//! [`STALLED_TAKER_ORIGIN_CROSS_SLOTS`] gives way to a maker cross. The resolver
//! cannot read a taker's position or the oracle, so it cannot tell a remainder
//! that cannot fill from one that nobody cranked. A maker cross reads only
//! depth that no remainder reserves, so it takes nothing a taker is owed.
//!
//! Neither leg may take a taker-origin order. Such an order reserves the depth
//! it crosses, and this crank reads the book without those reservations. The
//! book withholds the remainder itself whole from every ordinary fill, before
//! and after its claim lapses. A leg that took one would give the protocol the
//! gap to the other leg's source, so each leg also names the owners of the
//! taker-origin rows it can take, and the crank refuses a leg that filled one
//! of them. The read cannot measure how far
//! a leg reaches, because the book passes over rows the read reports as depth.
//! `crank_taker_origin_cross` owes the taker its improvement and is the only
//! caller that fills such a row.
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
        crank_common::{book_l3_sides, BookSides, ResolveClobCrank, MAX_CROSS_MAKERS},
        crank_taker_origin_cross::{stage_taker_origin_cross, TakerOriginStage},
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
        math::{
            casting::Cast,
            constants::MARGIN_PRECISION_U128,
            crosses::{crossing_prefix, same_authority, CrossLevel, CrossPrefix},
            safe_math::SafeMath,
        },
        msg,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            fill_mode::FillMode,
            pdas,
            perp_market_map::MarketSet,
            prop_amm::{
                DirectionV0, L3RowV0, PriceLevelV0, QuoterCpiScratch, QuoterSlabExt, QuoterSlabV0,
                QuoterType, UserRefV0, L3_ROW_FLAG_TAKER_ORIGIN,
            },
            state::State,
            user::{MarketType, Order, OrderStatus, OrderType, User, UserStats},
            user_map::{load_user_maps, UserMap, UserStatsMap},
        },
        validate,
    },
    anchor_lang::prelude::*,
    quoter_spec::L3_ROW_FLAG_RESERVED,
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

    // Funding is settled once, before the first leg. The second leg reads its
    // baseline without a settle, so a payment that the first leg's own funding
    // update charges the protocol `User` counts against the surplus.
    settle_funding_payment(
        &mut *load_mut!(ctx.accounts.taker)?,
        &ctx.accounts.taker.key(),
        &mut *maps.perp_market_map.get_ref_mut(&market_index)?,
        clock.unix_timestamp,
    )?;

    let base_before = taker_base(&ctx.accounts.taker, market_index)?;

    // One set of CPI buffers for the whole crank, as a router fill uses. Both
    // legs refill them in turn. A set per leg would be an allocation per leg,
    // and the runtime's allocator never gives one back.
    let mut cpi_scratch = QuoterCpiScratch::new();
    let cx = CrossMatchContext {
        accounts: ctx.accounts,
        tail,
        state: &state,
        market_index,
        band_oracle_price,
        margin_ratio_initial,
        leg_oracle_band: consulted_oracle_band(
            &ctx.accounts.quoter_slab,
            tail,
            margin_ratio_initial,
        )?,
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

    let payment = cross_payment(
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
        payment.min_surplus,
    )?;

    // The keeper's fee, so relay's `assert_paid_v0` has a balance to measure.
    if payment.lamports > 0 {
        ClobCrankConditionsV0::pay_keeper(
            &ctx.accounts.crank_conditions,
            &ctx.accounts.authority.to_account_info(),
            payment.lamports,
        )?;
    }

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
    /// The band each leg bounds its limit price at, in MARGIN_PRECISION
    /// units. It is never wider than `margin_ratio_initial`.
    leg_oracle_band: u32,
    makers_and_referrer: &'a UserMap<'info>,
    makers_and_referrer_stats: &'a UserStatsMap<'info>,
    clock: &'a Clock,
}

/// What one leg of a cross filled.
#[derive(Default)]
struct CrossLegFill {
    base_filled: u64,
    /// What the leg did to the protocol `User`'s quote, net of the taker fee
    /// it paid. It includes any funding the fill settled.
    quote_delta: i64,
    /// The worst price any single source of this leg executed at. Zero when
    /// the leg filled nothing.
    worst_price: u64,
    /// The carried makers this leg filled.
    makers: Vec<UserRefV0>,
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
        return Ok(CrossLegFill::default());
    }

    let limit_price = leg_limit_price(taker_direction, cx.band_oracle_price, cx.leg_oracle_band)?;
    let reach = TakerOriginReach::read(cx, taker_direction, limit_price, cpi_scratch)?;
    let makers_before = MakerBases::read(cx.makers_and_referrer, cx.market_index)?;

    let LegOpening {
        order_id,
        taker_ref,
        quote_before,
    } = open_leg(&mut *load_mut!(cx.accounts.taker)?, cx.market_index);

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

    let taker_served_window = leg_served_window(cx, size, cpi_scratch)?;
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

    let makers = makers_before.moved(cx.makers_and_referrer, cx.market_index)?;
    validate!(
        !reach.taken(&makers, filled.worst_fill_price, taker_direction),
        ErrorCode::CrossedTakerRemainderPending,
        "a cross leg of {} took a taker-origin order",
        size
    )?;

    let quote_after = load!(cx.accounts.taker)?
        .get_perp_position(cx.market_index)
        .map(|position| position.quote_asset_amount)
        .unwrap_or(0);
    Ok(CrossLegFill {
        base_filled: filled.amounts.base,
        quote_delta: quote_after.safe_sub(quote_before)?,
        worst_price: filled.worst_fill_price.unwrap_or(0),
        makers,
    })
}

/// Whether a leg of `size` may report protected flow.
///
/// The crank serves no taker-origin trade, so both sides must have served the
/// window. A `Custom` quoter in the route serves none, and checking it first
/// skips the CPIs that measure the rested depth of the other quoters.
fn leg_served_window<'info>(
    cx: &CrossMatchContext<'_, 'info>,
    size: u64,
    cpi_scratch: &mut QuoterCpiScratch<'info>,
) -> Result<bool> {
    if consults_custom_quoter(&cx.accounts.quoter_slab, cx.tail)? {
        return Ok(false);
    }

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
        })
}

/// One carried maker's base in the crossed market.
struct MakerBase {
    user: UserRefV0,
    base: i64,
}

/// The base every carried maker holds in the crossed market at one point.
struct MakerBases(Vec<MakerBase>);

impl MakerBases {
    fn read(makers: &UserMap, market_index: u16) -> Result<Self> {
        makers
            .0
            .values()
            .map(|loader| {
                let user = load!(loader)?;
                Ok(MakerBase {
                    user: UserRefV0 {
                        authority: user.authority,
                        sub_account_id: user.sub_account_id,
                    },
                    base: user
                        .get_perp_position(market_index)
                        .map(|position| position.base_asset_amount)
                        .unwrap_or(0),
                })
            })
            .collect::<Result<Vec<_>>>()
            .map(Self)
    }

    /// The makers whose base moved since this read, which are the makers a
    /// leg filled. A fill always moves its maker's base.
    fn moved(&self, makers: &UserMap, market_index: u16) -> Result<Vec<UserRefV0>> {
        let now = Self::read(makers, market_index)?;
        Ok(self
            .0
            .iter()
            .zip(now.0)
            .filter(|(before, after)| before.base != after.base)
            .map(|(_, after)| after.user)
            .collect())
    }
}

/// The taker-origin orders a leg can take, read just before it fills.
///
/// The read cannot say how far the leg reaches. The book passes over a row a
/// claim covers in part, an owner the margin clamp excludes, and an owner the
/// transaction does not carry, and the read still reports each of them as
/// depth. So the leg names who it may not fill, and [`Self::taken`] checks the
/// fill after it lands.
#[derive(Default)]
struct TakerOriginReach {
    /// The owners of the taker-origin rows inside the leg's limit that the book
    /// does not withhold.
    owners: Vec<UserRefV0>,
    /// For each book whose read filled [`CROSS_ROWS_PER_SIDE`] inside the
    /// limit, the price of its last row. The rows past it are not read.
    read_edges: Vec<u64>,
}

impl TakerOriginReach {
    fn read<'info>(
        cx: &CrossMatchContext<'_, 'info>,
        taker_direction: PositionDirection,
        limit_price: u64,
        cpi_scratch: &mut QuoterCpiScratch<'info>,
    ) -> Result<Self> {
        let quoter_slab = &cx.accounts.quoter_slab;
        let mut reach = Self::default();
        for slot_index in quoter_slab.consulted_slots(cx.tail)? {
            // Copied out so that no slab borrow lives across the book CPI.
            let quoter_slot = quoter_slab.slots()?[slot_index];
            if quoter_slot.config.quoter_type != QuoterType::Clob || !quoter_slot.quotes() {
                continue;
            }

            let rows = super::helpers::crank_common::book_l3_side(
                &quoter_slot,
                quoter_slab,
                cx.market_index,
                route_direction(taker_direction),
                CROSS_ROWS_PER_SIDE,
                cx.tail,
                cpi_scratch,
                false,
                ReachRow::from_row,
            )?
            .unwrap_or_default();
            reach.add_side(&rows, taker_direction, limit_price);
        }

        Ok(reach)
    }

    /// Add one side of a book, best price first.
    fn add_side(
        &mut self,
        rows: &[ReachRow],
        taker_direction: PositionDirection,
        limit_price: u64,
    ) {
        let within_limit = |price: u64| match taker_direction {
            PositionDirection::Long => price <= limit_price,
            PositionDirection::Short => price >= limit_price,
        };

        let reachable = rows.iter().take_while(|row| within_limit(row.price));
        self.owners.extend(
            reachable
                .clone()
                .filter(|row| row.takeable_taker_origin)
                .map(|row| row.user),
        );

        if rows.len() >= CROSS_ROWS_PER_SIDE as usize && reachable.count() == rows.len() {
            self.read_edges.extend(rows.last().map(|row| row.price));
        }
    }

    /// Whether a leg that filled `makers`, down to `worst_price`, took a
    /// taker-origin order. A leg that reached a read edge can have taken a row
    /// the read did not see.
    fn taken(
        &self,
        makers: &[UserRefV0],
        worst_price: Option<u64>,
        taker_direction: PositionDirection,
    ) -> bool {
        let reached_edge = worst_price.is_some_and(|worst| {
            self.read_edges.iter().any(|&edge| match taker_direction {
                PositionDirection::Long => worst >= edge,
                PositionDirection::Short => worst <= edge,
            })
        });

        reached_edge || makers.iter().any(|maker| self.owners.contains(maker))
    }
}

/// The part of an L3 row the reach check reads. The heap never gives memory
/// back, so the executor copies out less than the whole row.
#[derive(Clone, Copy)]
struct ReachRow {
    price: u64,
    user: UserRefV0,
    takeable_taker_origin: bool,
}

impl ReachRow {
    fn from_row(row: &L3RowV0) -> Self {
        Self {
            price: row.price,
            user: row.user,
            takeable_taker_origin: is_takeable_taker_origin(row.flags),
        }
    }
}

/// Whether a row is a taker-origin order that a leg can take. A claim that
/// covers any part of an order makes the book pass over the whole order.
fn is_takeable_taker_origin(flags: u8) -> bool {
    flags & L3_ROW_FLAG_TAKER_ORIGIN != 0 && flags & L3_ROW_FLAG_RESERVED == 0
}

/// What a leg reads off the protocol `User` before it fills.
struct LegOpening {
    order_id: u32,
    taker_ref: crate::state::prop_amm::UserRefV0,
    quote_before: i64,
}

/// Take the leg's order id and read the quote it is measured from.
///
/// This settles no funding. A settle here before the second leg would pay the
/// funding that the first leg's own update rolls, on the whole crossed size,
/// outside the surplus. The leg-1 maker would then collect it from the protocol.
fn open_leg(taker: &mut User, market_index: u16) -> LegOpening {
    let order_id = crate::get_then_update_id!(taker, next_order_id);
    LegOpening {
        order_id,
        taker_ref: taker.clob_user_ref(),
        quote_before: taker
            .get_perp_position(market_index)
            .map(|position| position.quote_asset_amount)
            .unwrap_or(0),
    }
}

/// The last price inside the maker oracle band, on the side the leg buys or
/// sells at.
///
/// A cross leg brings no price of its own, and the crossed prices are not
/// known until both sides are read. The band is the widest price the fill
/// settles a maker at, so the bound discards nothing the fill would take.
/// `limit_price_breaches_maker_oracle_price_bands` refuses a distance that
/// reaches the band, so the bound is one unit inside it.
fn leg_limit_price(
    taker_direction: PositionDirection,
    oracle_price: i64,
    oracle_band: u32,
) -> Result<u64> {
    let oracle_price = oracle_price.unsigned_abs();
    let refused_distance = oracle_price
        .cast::<u128>()?
        .safe_mul(oracle_band.cast()?)?
        .div_ceil(MARGIN_PRECISION_U128)
        .cast::<u64>()?;
    let accepted_distance = refused_distance.saturating_sub(1);
    Ok(match taker_direction {
        PositionDirection::Long => oracle_price.saturating_add(accepted_distance),
        PositionDirection::Short => oracle_price.saturating_sub(accepted_distance),
    })
}

/// The narrowest oracle band among the quoters this cross consults.
///
/// A quoter entry can declare a band inside the market's. The router drops a
/// book whose quote reaches past its entry's band, so a leg bounded at the
/// market's band alone can lose the book it exists to cross.
fn consulted_oracle_band<'info>(
    quoter_slab: &AccountLoader<'info, QuoterSlabV0>,
    tail: &'info [AccountInfo<'info>],
    margin_ratio_initial: u32,
) -> Result<u32> {
    let consulted = quoter_slab.consulted_slots(tail)?;
    let slots = quoter_slab.slots()?;
    Ok(consulted
        .iter()
        .filter(|&&index| slots[index].quotes())
        .map(|&index| slots[index].config.oracle_band(margin_ratio_initial))
        .fold(margin_ratio_initial, u32::min))
}

/// The base both legs matched, and the quote the protocol kept for it.
#[derive(Debug)]
struct CrossSurplus {
    base_matched: u64,
    surplus: i64,
}

/// The rules that make a pair of fills a cross.
///
/// No authority may sell to the buy leg and buy from the sell leg. One authority
/// on both sides pays itself the spread and collects maker volume for it.
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
        !buy.makers.iter().any(|bought_from| sell
            .makers
            .iter()
            .any(|sold_to| same_authority(sold_to, bought_from))),
        ErrorCode::InvalidMaker,
        "a cross must not match one authority against itself"
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

/// The lamports a cross pays its keeper, and the quote surplus it must clear.
#[derive(Debug, PartialEq, Eq)]
struct CrossPayment {
    min_surplus: u64,
    lamports: u64,
}

/// What a cross pays its keeper, and the surplus it must clear to pay it.
///
/// The floor is the keeper's lamport payment valued in quote, so a cross the
/// reservoir pays for never nets the protocol less than it costs to land. A
/// crank without a usable SOL price cannot value the payment. The reservoir
/// then pays nothing, and the floor is the admin's `min_cross_surplus`. Market
/// index zero is the quote market, so it never prices the payment.
fn cross_payment(
    crank_conditions: &AccountLoader<ClobCrankConditionsV0>,
    state: &State,
    spot_market_map: &crate::state::spot_market_map::SpotMarketMap,
    oracle_map: &mut crate::state::oracle_map::OracleMap,
) -> Result<CrossPayment> {
    let (min_surplus, payment_lamports) = {
        let conditions = crank_conditions.load()?;
        (
            conditions.min_cross_surplus,
            u64::from(conditions.crank_payments.cross),
        )
    };

    let payment_quote = (state.sol_spot_market_index != 0)
        .then(|| {
            crate::state::clob_crank::sol_price_for_payment_floor(
                state,
                spot_market_map,
                oracle_map,
            )
        })
        .flatten()
        .and_then(|sol_price| {
            crate::state::clob_crank::CrankPaymentsV0::lamports_to_quote(
                payment_lamports,
                sol_price,
            )
        });

    Ok(match payment_quote {
        Some(payment_quote) => CrossPayment {
            min_surplus: min_surplus.max(payment_quote),
            lamports: payment_lamports,
        },
        None => CrossPayment {
            min_surplus,
            lamports: 0,
        },
    })
}

/// The cross and activation conditions' answer: a crossed taker remainder if
/// the book has one, otherwise a maker-against-maker cross worth taking. A
/// taker-origin cross that stalled gives way to a maker cross. The module doc
/// states the order and the reason for it.
pub(super) fn stage_cross(ctx: &Context<ResolveClobCrank>) -> Result<Option<StagedCall>> {
    let taker_origin = match stage_taker_origin_cross(ctx)? {
        Some(TakerOriginStage {
            call,
            yields_to_maker_cross: false,
        }) => return Ok(Some(call)),
        stage => stage,
    };

    if let Some(call) = stage_maker_cross(ctx)? {
        return Ok(Some(call));
    }

    Ok(taker_origin.map(|stage| stage.call))
}

/// Stage `crank_cross_match` for the book's own crossing prefix, if it clears
/// the fee gulf at the tier-0 taker fee. The executor measures the real
/// figure. A PropAMM that crosses the book is the quoter-cross resolver's work.
fn stage_maker_cross(ctx: &Context<ResolveClobCrank>) -> Result<Option<StagedCall>> {
    let cross = find_clob_cross(ctx)?;
    let state = ctx.accounts.state.load()?;
    if cross.size == 0 || cross.estimated_surplus(&state.perp_fee_structure.fee_tiers[0]) == 0 {
        return Ok(None);
    }

    let (market_index, oracle, quote_oracle, quote_spot_market_index) = {
        let conditions = ctx.accounts.crank_conditions.load()?;
        (
            conditions.market_index,
            conditions.oracle,
            conditions.quote_oracle,
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
    .map_section_named_perp(oracle, quote_oracle, quote_spot_market_index);
    let call = with_sol_spot_market(call, &state, quote_spot_market_index)
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

/// Add the SOL spot market to the maps section, after the quote spot market.
/// The executor prices the keeper payment off it, and a resolver can derive
/// it where it cannot name the SOL oracle.
pub(super) fn with_sol_spot_market(
    call: StagedCall,
    state: &State,
    quote_spot_market_index: u16,
) -> StagedCall {
    match state.sol_spot_market_index {
        0 => call,
        index if index == quote_spot_market_index => call,
        index => call.account(pdas::spot_market(index), false),
    }
}

/// How deep either side of the crossing prefix is read.
///
/// The walk ends on [`MAX_CROSS_MAKERS`] makers regardless, so this only reaches past a single
/// maker's ladder. A longer prefix stages a smaller cross, continued by the next wake.
const CROSS_ROWS_PER_SIDE: u16 = 32;

/// How long a taker-origin cross may stay on the book before a maker cross
/// behind it is staged first. About a minute at 400 ms slots.
pub(super) const STALLED_TAKER_ORIGIN_CROSS_SLOTS: u64 = 150;

/// One side of a book as the depth a cross leg may take, best price first.
///
/// The executor refuses a leg that fills the owner of a taker-origin row it can
/// take. So the side ends at the first row of such an owner. A row that a claim
/// covers in part is no depth, because the book passes over all of it.
fn crossable_levels(rows: &[L3RowV0]) -> Vec<CrossLevel> {
    let refused_owners: Vec<UserRefV0> = rows
        .iter()
        .filter(|row| is_takeable_taker_origin(row.flags))
        .map(|row| row.user)
        .collect();

    rows.iter()
        .take_while(|row| !refused_owners.contains(&row.user))
        .filter(|row| row.flags & (L3_ROW_FLAG_TAKER_ORIGIN | L3_ROW_FLAG_RESERVED) == 0)
        .map(CrossLevel::from_row)
        .collect()
}

/// The crossing prefix of the book against itself, over the rows that
/// `quote_l3_v0` reports matchable now.
fn clob_cross_prefix(sides: &BookSides<L3RowV0>) -> CrossPrefix {
    crossing_prefix(
        &crossable_levels(&sides.bids),
        &crossable_levels(&sides.asks),
        MAX_CROSS_MAKERS,
    )
}

/// Quote both sides of the market's book and cross them against each other.
fn find_clob_cross(ctx: &Context<ResolveClobCrank>) -> Result<CrossPrefix> {
    let market_index = ctx.accounts.crank_conditions.load()?.market_index;
    let book_slot = ctx.accounts.quoter_slab.clob_slot(market_index)?;
    if !book_slot.quotes() {
        // A killed or unvetted book has no cross to stage. The conditions go
        // quiet rather than fail on every wake.
        return Ok(CrossPrefix::default());
    }

    let accounts = [
        ctx.accounts.clob_market.to_account_info(),
        ctx.accounts.clob_program.to_account_info(),
    ];
    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    let sides = book_l3_sides(
        &book_slot,
        &ctx.accounts.quoter_slab,
        market_index,
        CROSS_ROWS_PER_SIDE,
        &accounts,
        &mut cpi_scratch,
        false,
        |row| *row,
    )?;

    Ok(sides.map_or_else(CrossPrefix::default, |sides| clob_cross_prefix(&sides)))
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
/// stages `crank_cross_match` for the profitable side. A taker's remainder that
/// the quote crosses goes first, through `crank_taker_origin_cross`. It works
/// for any quoter program with a registry entry, and velocity carries no
/// per-program code.
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

        // A taker's remainder that this quote crosses goes first. The
        // improvement belongs to the taker, so it fills through the taker's
        // own crank rather than a cross the protocol middles.
        let remainder = {
            let conditions = ctx.accounts.cross_conditions.load()?;
            super::crank_taker_origin_cross::stage_quoter_crossed_remainder(
                super::crank_taker_origin_cross::QuoterCrossedRead {
                    keys: super::crank_taker_origin_cross::TakerOriginKeys {
                        market_index,
                        state: ctx.accounts.state.key(),
                        quoter_slab: ctx.accounts.quoter_slab.key(),
                        clob_market: ctx.accounts.clob_market.key(),
                        oracle: conditions.oracle,
                        quote_oracle: conditions.quote_oracle,
                        quote_spot_market_index: conditions.quote_spot_market_index,
                    },
                    state: &*ctx.accounts.state.load()?,
                    quoter_slab: &ctx.accounts.quoter_slab,
                    clob_accounts: &clob_accounts,
                    quoter: super::crank_taker_origin_cross::QuoterTops {
                        bid: quoter_bids.first().map(|level| level.price),
                        ask: quoter_asks.first().map(|level| level.price),
                    },
                    quoter_user: maker_ref,
                },
                &mut cpi_scratch,
            )?
        };
        if remainder.is_some() {
            return Ok(remainder);
        }

        // A `Custom` entry in the route stops the crank reporting protected
        // flow, so a book with a speed bump quotes that cross no depth and the
        // legs never cross. Report no work rather than stage a call that always
        // fails.
        if slots[book_slot].config.book_default_activation_delay_slots > 0 {
            return Ok(None);
        }

        let Some(book) = book_l3_sides(
            &slots[book_slot],
            &ctx.accounts.quoter_slab,
            market_index,
            CROSS_ROWS_PER_SIDE,
            &clob_accounts,
            &mut cpi_scratch,
            false,
            |row| *row,
        )?
        else {
            return Ok(None);
        };
        let book = BookSides {
            bids: crossable_levels(&book.bids),
            asks: crossable_levels(&book.asks),
        };

        let quoted = |levels: &[PriceLevelV0]| -> Vec<CrossLevel> {
            levels
                .iter()
                .map(|level| CrossLevel {
                    price: level.price,
                    size: level.size,
                    owner: maker_ref,
                })
                .collect()
        };

        // Keep whichever direction pays better. The crank names no legs,
        // because each of its two fills routes across every source the tail
        // carries. The direction only decides how big a cross the resolver
        // claims.
        let fee_tier = ctx.accounts.state.load()?.perp_fee_structure.fee_tiers[0];
        let quoter_sells = crossing_prefix(&book.bids, &quoted(&quoter_asks), MAX_CROSS_MAKERS);
        let quoter_buys = crossing_prefix(&quoted(&quoter_bids), &book.asks, MAX_CROSS_MAKERS);
        let cross = if quoter_sells.estimated_surplus(&fee_tier)
            >= quoter_buys.estimated_surplus(&fee_tier)
        {
            quoter_sells
        } else {
            quoter_buys
        };

        if cross.size == 0 || cross.estimated_surplus(&fee_tier) == 0 {
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
                // A discovery read settles nothing, so it carries no mark.
                reference_price: None,
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
    cross: &CrossPrefix,
    maker_ref: crate::state::prop_amm::UserRefV0,
    market_index: u16,
) -> Result<StagedCall> {
    let (oracle, quote_oracle, quote_spot_market_index, clob_program) = {
        let conditions = ctx.accounts.cross_conditions.load()?;
        (
            conditions.oracle,
            conditions.quote_oracle,
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
    .map_section_named_perp(oracle, quote_oracle, quote_spot_market_index);
    let call = with_sol_spot_market(call, &*ctx.accounts.state.load()?, quote_spot_market_index);
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
