//! The liquidity layer of a perp fill.
//!
//! This layer governs liquidity. It quotes every source, splits the taker's
//! unfilled size across them, executes each allocation and settles what comes
//! back. It measures no risk of its own. [`super::taker_risk`] sets the limits
//! it runs inside.

use {
    super::{super::*, context::*},
    crate::{
        controller::{
            funding::settle_funding_payment,
            position::{self, get_position_index, PositionDirection},
        },
        error::{ErrorCode, VelocityResult},
        instructions::optional_accounts::AccountMaps,
        math::{
            casting::Cast,
            constants::BASE_PRECISION_U64,
            router::{split_across_quoters, QuoterAllocation, QuoterBook, RouterLeg},
            safe_math::SafeMath,
        },
        state::{
            events::OrderActionExplanation,
            oracle_map::OracleMap,
            perp_market::PerpMarket,
            prop_amm::{ClobUserRefV0, Direction, PriceLevel, QuoterType},
            quoter::{MarketQuoteInputs as QuoteInputs, QuoterFill, RouterQuoter},
            user::{OrderStatus, User, UserStats},
            user_map::{UserMap, UserStatsMap},
        },
        validate,
        vlp::amm::{
            math::amm::calculate_amm_available_liquidity, router_adapter::vamm_quote_levels,
            AmmQuoter,
        },
    },
    anchor_lang::prelude::{msg, Pubkey},
    std::{cell::RefMut, collections::BTreeMap},
};

/// What the fill prices against, read once before any allocation executes:
/// the market snapshot every quoter quotes from, the vAMM state the deferred
/// mark TWAP needs, and the two limits the ladders are cut at.
struct FillMarketSetup {
    /// The market fields quoting reads, owned so the caller can hold the AMM
    /// mutably while it quotes.
    quote_inputs: QuoteInputs,
    /// The vAMM bid and ask after the refresh, and the spreads that produced
    /// them. The deferred mark TWAP reads all five.
    amm_bid_price: u64,
    amm_ask_price: u64,
    amm_base_spread: u32,
    amm_long_spread: u32,
    amm_short_spread: u32,
    /// The ceiling the vAMM ladder is cut at, tighter than the maker books get. A
    /// post-only taker's limit is buffered by the rebate it earns, and a ladder bounded
    /// by the raw limit would let it sweep past that buffer and price every unit at the
    /// raw limit. That difference is LP value.
    amm_taker_limit: Option<u64>,
    /// The one ceiling every maker book is cut at. A market order falls back
    /// to the AMM fallback price, so a router sweep stays price-bounded.
    effective_taker_limit: Option<u64>,
    /// The price the taker's own order holds the fill to, as the fill mode
    /// resolves it. `None` for a market order, which has no limit of its own.
    /// The settle legs charge against it.
    taker_limit_price: Option<u64>,
}

impl FillMarketSetup {
    /// Refresh the vAMM and read everything the fill quotes against.
    fn load(
        amm_quoter: &mut AmmQuoter,
        quote_inputs: QuoteInputs,
        taker: &TakerSide,
        rules: &PricingRules,
        conditions: &FillConditions,
        market_fee_adjustment: i16,
    ) -> VelocityResult<Self> {
        let now = conditions.now;
        let slot = conditions.slot;
        let taker_limit_price = conditions.mode.get_limit_price(
            taker.order,
            conditions.valid_oracle_price,
            slot,
            quote_inputs.tick_size,
            quote_inputs.slot_clock,
        )?;

        amm_quoter.refresh(&quote_inputs.ctx(slot))?;
        let reserve_after_setup = amm_quoter.amm.reserve_price()?;
        let (amm_bid_price, amm_ask_price) = amm_quoter.amm_bid_ask(reserve_after_setup)?;
        let amm_base_spread = amm_quoter.amm_base_spread();
        let amm_long_spread = amm_quoter.amm.long_spread;
        let amm_short_spread = amm_quoter.amm.short_spread;

        let amm_taker_limit = crate::math::orders::calculate_effective_amm_taker_limit(
            taker.order,
            taker_limit_price,
            None,
            &rules.user_fee_tier(taker.stats, now)?,
            market_fee_adjustment,
            quote_inputs.tick_size,
        )?;

        let effective_taker_limit = match taker_limit_price {
            Some(price) => Some(price),
            None => Some(market_order_limit(amm_quoter, &quote_inputs, taker, now)?),
        };

        Ok(Self {
            quote_inputs,
            amm_bid_price,
            amm_ask_price,
            amm_base_spread,
            amm_long_spread,
            amm_short_spread,
            amm_taker_limit,
            effective_taker_limit,
            taker_limit_price,
        })
    }
}

/// The ceiling a market order is cut at.
///
/// A market order carries no limit of its own, so the vAMM's fallback price
/// stands in for one. That keeps a router sweep price-bounded.
fn market_order_limit(
    amm_quoter: &AmmQuoter,
    quote_inputs: &QuoteInputs,
    taker: &TakerSide,
    now: i64,
) -> VelocityResult<u64> {
    let amm: &crate::vlp::amm::AMM = amm_quoter.amm;
    let amm_available =
        calculate_amm_available_liquidity(amm, &taker.direction, quote_inputs.step_size)?;
    amm.get_fallback_price(
        &quote_inputs.stats,
        &taker.direction,
        amm_available,
        quote_inputs.oracle_price,
        taker.order.seconds_til_expiry(now),
        quote_inputs.stats.min_order_size,
    )
}

/// What one liquidity pass has moved so far. Every source settles into this. Only the
/// steps that close the pass out read a running total, so the accumulators are held apart
/// from the snapshot the pass quotes against.
#[derive(Default)]
struct FillTally {
    /// Base and quote settled so far, over every source.
    base_filled: u64,
    quote_filled: u64,
    /// The worst price any one source of this fill executed at.
    worst_fill_price: Option<u64>,
    /// The size-independent part of the filler reward this fill already paid.
    /// Each leg draws down what earlier legs paid, so a taker crossing several
    /// sources pays that component once.
    filler_reward_paid: u64,
    /// Signed base each maker filled, and whether that maker is isolated.
    /// The post-fill checks read it, so the pass hands it back.
    maker_fills: MakerFills,
    /// One bit per loaded user, in the order the map holds them. Set as each
    /// balance change settles, so the obligation check can name a loaded user
    /// that did nothing.
    settled_users: u64,
}

impl FillTally {
    /// Add what one source settled.
    fn note_fill(
        &mut self,
        direction: PositionDirection,
        base_filled: u64,
        quote_filled: u64,
    ) -> VelocityResult {
        self.base_filled = self.base_filled.safe_add(base_filled)?;
        self.quote_filled = self.quote_filled.safe_add(quote_filled)?;
        note_worst_fill_price(
            &mut self.worst_fill_price,
            direction,
            base_filled,
            quote_filled,
        )
    }

    /// Mark a loaded user as one this fill moved.
    fn mark_settled(&mut self, users: &UserMap, key: &Pubkey) {
        if let Some(index) = users.0.keys().position(|held| held == key) {
            if index < u64::BITS as usize {
                self.settled_users |= 1u64 << index;
            }
        }
    }
}

/// The external quoter books this fill may route to, and the leg that executes on them.
/// Everything an external book touches sits behind this. It is the only part of the fill
/// that borrows the quoting section and the executor's account region, so holding it
/// apart keeps three lifetimes off [`PerpFill`].
struct ExternalVenue<'a, 'r, 'b, 'info> {
    router: &'a mut RouterLeg<'r, 'b, 'info>,
    /// [`RouterLeg::books`], read out once so a step can hold a ladder
    /// while the executor runs.
    books: &'r [QuoterBook<'b>],
}

impl<'a, 'r, 'b, 'info> ExternalVenue<'a, 'r, 'b, 'info> {
    fn new(router: &'a mut RouterLeg<'r, 'b, 'info>) -> Self {
        let books = router.books;
        Self { router, books }
    }
}

/// What the split gave one external book: which book, the ladder it was cut
/// from, and the size.
struct BookShare<'l> {
    index: usize,
    levels: &'l [PriceLevel],
    allocation: &'l QuoterAllocation,
}

/// One external book's execution: which book answered, the quoter behind it, and the
/// bounds its response is held to. Every field is read before the CPI runs, so the checks
/// never reach back into the executor while a borrow of the response is live.
struct ExternalLeg {
    /// The quoter that answered, for the messages the checks emit.
    quoter_key: Pubkey,
    /// `State::signer`. No quoter may name this account as a fill subject.
    protocol_authority: Pubkey,
    /// The accounts this quoter is allowed to act against.
    subjects: crate::state::prop_amm::QuoterSubjects,
    /// The levels the allocation was cut from. They bound the price of every
    /// balance change the response carries.
    quoted: crate::math::router::QuotedPrefix,
    /// The price band this quoter's fills must stay inside, which is the
    /// tighter of its own declared band and the market's initial margin ratio.
    oracle_band: u32,
    /// CLOB orders are margin-reserved through velocity at placement, so their
    /// fills and culls unwind open-order aggregates. Custom PropAMM depth is
    /// never reserved, so there is nothing to unwind.
    maker_aggregates_tracked: bool,
}

/// One quote-and-split pass: what the split gave every source, and what the vAMM leg
/// already executed. The allocations are in book order, external books first.
/// `externals_end` says where that run ends, so it is also the vAMM's own index.
struct RoutedFill {
    /// One allocation per book.
    allocations: Vec<QuoterAllocation>,
    /// What the vAMM leg executed, when it won an allocation.
    amm_fill: Option<QuoterFill>,
    /// Where the external allocations end.
    externals_end: usize,
}

impl RoutedFill {
    /// What the split gave the external books.
    fn external_allocations(&self) -> &[QuoterAllocation] {
        &self.allocations[..self.externals_end]
    }

    /// What the split gave the vAMM.
    fn amm_allocation(&self) -> &QuoterAllocation {
        &self.allocations[self.externals_end]
    }
}

/// What the liquidity pass carries across its own steps.
///
/// The external books live in [`ExternalVenue`], and the market and the filler are
/// arguments rather than fields. The vAMM quoter borrows `market.amm` for the whole quote
/// window, and folding in the filler would put nine lifetimes on every step. The three
/// remaining lifetimes cannot merge, because each map holds `AccountInfo`, which is
/// invariant, and they reach the fill from separately elided caller regions.
struct PerpFill<'a, 'o, 'm, 's> {
    oracle_map: &'a mut OracleMap<'o>,
    makers_and_referrer: &'a UserMap<'m>,
    makers_and_referrer_stats: &'a UserStatsMap<'s>,
    /// What the pass has moved so far.
    tally: FillTally,
    rules: &'a PricingRules<'a>,
    /// How many external books the route carries. It is where the external
    /// allocations end and the vAMM's allocation begins.
    external_book_count: usize,
    taker: TakerSide<'a>,
    setup: FillMarketSetup,
    market_index: u16,
    /// Opposite the taker's, by construction.
    maker_direction: PositionDirection,
    /// The taker's side, as the router states it.
    route_direction: Direction,
    /// When this fill runs and what the market oracle lets it do. Held whole
    /// rather than copied field by field, so a reader can see where each of
    /// these values came from.
    conditions: FillConditions,
    /// True when a book stopped its walk at an owner this transaction does
    /// not carry. The fill owes the obligation check only then, so an
    /// ordinary fill does not pay for it.
    withheld_depth: bool,
    /// Resolves a wire user reference against the loaded set. Empty when no
    /// external book can name one.
    user_ref_index: BTreeMap<(Pubkey, u16), Pubkey>,
    taker_ref: ClobUserRefV0,
}

impl<'a, 'o, 'm, 's> PerpFill<'a, 'o, 'm, 's> {
    /// The context, from what the liquidity pass already holds.
    fn new(
        counterparties: FillCounterparties<'a, 'o, 'm, 's>,
        venue: &ExternalVenue,
        rules: &'a PricingRules<'a>,
        conditions: &FillConditions,
        setup: FillMarketSetup,
        taker: TakerSide<'a>,
    ) -> VelocityResult<Self> {
        let FillCounterparties {
            oracle_map,
            users: makers_and_referrer,
            stats: makers_and_referrer_stats,
        } = counterparties;
        let external_books = venue.books;
        // Only an external quoter's balance change names a wire user, so a
        // fill with no external book never resolves one. Building the index
        // loads every user, so skip it when nothing can be resolved.
        let user_ref_index = if external_books.is_empty() {
            BTreeMap::new()
        } else {
            makers_and_referrer.user_ref_index()?
        };

        Ok(Self {
            market_index: taker.order.market_index,
            maker_direction: taker.direction.opposite(),
            route_direction: match taker.direction {
                PositionDirection::Long => Direction::Long,
                PositionDirection::Short => Direction::Short,
            },

            taker_ref: taker.user.clob_user_ref(),
            withheld_depth: external_books
                .iter()
                .any(|book| book.withheld.price != 0 && book.withheld.size != 0),
            external_book_count: external_books.len(),
            user_ref_index,
            oracle_map,
            makers_and_referrer,
            makers_and_referrer_stats,
            tally: FillTally::default(),
            rules,
            taker,
            setup,
            conditions: *conditions,
        })
    }

    /// Where a ladder stops. Levels past the taker's effective limit are
    /// outside what this fill accepts.
    fn within_limit(&self, levels: &[PriceLevel]) -> usize {
        let Some(limit) = self.setup.effective_taker_limit else {
            return levels.len();
        };

        levels
            .iter()
            .position(|level| match self.taker.direction {
                PositionDirection::Long => level.price > limit,
                PositionDirection::Short => level.price < limit,
            })
            .unwrap_or(levels.len())
    }

    /// Hand the worst price any one source executed at back to the caller. The fill's
    /// return value is the base and the blended quote, and a blend hides the worst price
    /// it contains.
    fn report_worst_fill_price(&self, venue: &mut ExternalVenue) {
        venue.router.worst_fill_price = self.tally.worst_fill_price;
    }

    /// Record what one settled source moved.
    fn note_fill(&mut self, base_filled: u64, quote_filled: u64) -> VelocityResult {
        self.tally
            .note_fill(self.taker.direction, base_filled, quote_filled)
    }

    /// Mark a loaded user as one this fill moved. Only a fill that owes the
    /// obligation check reads the marks, so nothing else pays for them.
    fn mark_settled(&mut self, key: &Pubkey) {
        if !self.withheld_depth {
            return;
        }

        self.tally.mark_settled(self.makers_and_referrer, key);
    }

    /// Resolve a wire user reference against the loaded set.
    fn resolve_user(&self, user: &ClobUserRefV0) -> VelocityResult<Pubkey> {
        self.user_ref_index
            .get(&(user.authority, user.sub_account_id))
            .copied()
            .ok_or_else(|| {
                msg!(
                    "quoter returned a balance change for an unloaded user {}/{}",
                    user.authority,
                    user.sub_account_id
                );

                ErrorCode::DefaultError
            })
    }

    /// Quote every source, split the taker's size across them, and take the
    /// vAMM's share while the curve is still held.
    ///
    /// The vAMM executes here because it is the one source whose liquidity is
    /// the market account itself. Every other allocation settles after the
    /// curve is released.
    fn route_across_sources(
        &mut self,
        venue: &ExternalVenue,
        amm_quoter: &mut AmmQuoter,
        target_size: u64,
    ) -> VelocityResult<RoutedFill> {
        let rivals = self.rival_books(venue);
        let amm_levels = self.quote_vamm(amm_quoter, &rivals, target_size)?;

        // `vamm_quote_levels` already capped the ladder at the limit, and its per-rung
        // prices are rounded slice averages, so comparing them again would drop small
        // rungs whose true cost is inside it. `books` takes over the rival allocation and
        // is declared after `amm_levels`, so it drops first.
        let mut books = rivals;
        books.push(QuoterBook {
            priority: QuoterType::Vamm.default_priority(),
            levels: &amm_levels,
            withheld: PriceLevel::default(),
        });

        let allocations = split_across_quoters(
            self.route_direction,
            target_size,
            &books,
            self.setup.quote_inputs.step_size,
        )?;
        let externals_end = self.external_book_count;
        let amm_fill = self.execute_vamm(amm_quoter, &allocations[externals_end])?;

        Ok(RoutedFill {
            allocations,
            amm_fill,
            externals_end,
        })
    }

    /// Every external quoter book, each cut at the taker's effective limit, in one
    /// allocation with room for the vAMM's book after it.
    ///
    /// The ladders are already the depth this fill may settle, so the split and the settle
    /// pass read the same levels. The allocation carries one spare slot, because the
    /// allocator never reclaims and a `Vec` that doubles abandons its old buffer.
    fn rival_books<'l, 'r: 'l, 'b: 'l>(
        &self,
        venue: &ExternalVenue<'_, 'r, 'b, '_>,
    ) -> Vec<QuoterBook<'l>> {
        let mut books = Vec::with_capacity(venue.books.len() + 1);
        books.extend(venue.books.iter().map(|book| QuoterBook {
            priority: book.priority,
            levels: &book.levels[..self.within_limit(book.levels)],
            withheld: book.withheld,
        }));

        books
    }

    /// The vAMM ladder. The vAMM quotes last, so every other book is its last
    /// look.
    fn quote_vamm(
        &self,
        amm_quoter: &AmmQuoter,
        rivals: &[QuoterBook],
        target_size: u64,
    ) -> VelocityResult<Vec<PriceLevel>> {
        if !self.conditions.amm_is_available {
            return Ok(vec![]);
        }

        vamm_quote_levels(
            &*amm_quoter.amm,
            self.route_direction,
            target_size,
            self.setup.quote_inputs.step_size,
            rivals,
            // Fall back to the shared limit when the order has no limit of
            // its own. A market order has no `amm_taker_limit`.
            self.setup
                .amm_taker_limit
                .or(self.setup.effective_taker_limit),
        )
    }

    /// Execute the vAMM's allocation, held to the ladder it quoted.
    fn execute_vamm(
        &self,
        amm_quoter: &mut AmmQuoter,
        allocation: &QuoterAllocation,
    ) -> VelocityResult<Option<QuoterFill>> {
        if allocation.base == 0 {
            return Ok(None);
        }

        let fill = RouterQuoter::execute(
            amm_quoter,
            &self.setup.quote_inputs.ctx(self.conditions.slot),
            self.route_direction,
            allocation.base,
        )?;

        validate!(
            fill.base_filled <= allocation.base,
            ErrorCode::DefaultError,
            "router vAMM overfilled: {} > {}",
            fill.base_filled,
            allocation.base
        )?;
        validate!(
            crate::controller::matching::fill_at_or_better(
                self.taker.direction,
                &fill,
                allocation,
                BASE_PRECISION_U64
            )?,
            ErrorCode::DefaultError,
            "router vAMM filled worse than quoted: fill {}/{} vs quoted {}/{}",
            fill.quote_filled,
            fill.base_filled,
            allocation.quote,
            allocation.base
        )?;

        Ok((fill.base_filled > 0).then_some(fill))
    }

    /// Settle each source's allocation into the accounts it moved.
    ///
    /// The vAMM goes first, then the external books. The external ladders are
    /// rebuilt from the levels the split was cut from, so a balance change is
    /// held to the prices it was allocated at.
    fn settle_routed_fill(
        &mut self,
        market: &mut PerpMarket,
        filler: &mut FillerSide,
        venue: &mut ExternalVenue,
        routed: &RoutedFill,
    ) -> VelocityResult {
        if let Some(amm_fill) = routed.amm_fill.as_ref() {
            self.settle_vamm_allocation(market, filler, amm_fill, routed.amm_allocation())?;
        }

        let ladders = self.rival_books(venue);
        self.settle_external_allocations(
            market,
            filler,
            venue,
            &ladders,
            routed.external_allocations(),
        )
    }

    /// Settle the vAMM's fill against the house.
    ///
    /// Runs after the external book legs, which have released their borrows by
    /// here.
    fn settle_vamm_allocation(
        &mut self,
        market: &mut PerpMarket,
        filler: &mut FillerSide,
        fill: &QuoterFill,
        allocation: &QuoterAllocation,
    ) -> VelocityResult {
        // A maker that cranked this fill earns the reward on the vAMM slice too. It
        // arrives as `filler: None` naming itself, because it is already in the maker map
        // and cannot be loaded twice. The reward requires it to have filled.
        let cranking_maker_key = (filler.user.is_none()
            && self.tally.maker_fills.contains_key(&filler.key))
        .then_some(filler.key)
        .filter(|key| self.makers_and_referrer.0.contains_key(key));
        let mut cranking_maker = match cranking_maker_key {
            Some(key) => Some(self.makers_and_referrer.get_ref_mut(&key)?),
            None => None,
        };
        let mut cranking_maker_stats = match cranking_maker.as_deref() {
            Some(maker) if maker.authority != self.taker.user.authority => Some(
                self.makers_and_referrer_stats
                    .get_ref_mut(&maker.authority)?,
            ),
            _ => None,
        };
        let mut cranking_maker_opt: Option<&mut User> = cranking_maker.as_deref_mut();
        let mut cranking_maker_stats_opt: Option<&mut UserStats> =
            cranking_maker_stats.as_deref_mut();
        // Read off the order before the taker side is borrowed. A fill never
        // changes any of the three.
        let order_post_only = self.taker.order.post_only;
        let order_slot = self.taker.order.slot;
        let order_id = self.taker.order.order_id;
        let (base_filled, quote_filled) = settle_amm_house_fill(
            fill,
            &mut self.taker,
            &mut HouseSide {
                cranking_maker: &mut cranking_maker_opt,
                cranking_maker_stats: &mut cranking_maker_stats_opt,
            },
            &AmmAllocation {
                quote: allocation.quote,
                base: allocation.base,
                post_only: order_post_only,
                order_slot,
                order_id,
                taker_limit_price: self.setup.taker_limit_price,
            },
            filler,
            &mut SettleContext {
                market,
                rules: self.rules,
                mode: self.conditions.mode,
                oracle_map: self.oracle_map,
                now: self.conditions.now,
                slot: self.conditions.slot,
                filler_reward_paid: &mut self.tally.filler_reward_paid,
            },
        )?;

        self.note_fill(base_filled, quote_filled)
    }

    /// Execute and settle every external book's allocation through its CPI
    /// leg.
    fn settle_external_allocations(
        &mut self,
        market: &mut PerpMarket,
        filler: &mut FillerSide,
        venue: &mut ExternalVenue,
        ladders: &[QuoterBook],
        allocations: &[QuoterAllocation],
    ) -> VelocityResult {
        ladders
            .iter()
            .zip(allocations)
            .enumerate()
            .try_for_each(|(index, (ladder, allocation))| {
                self.settle_external_allocation(
                    market,
                    filler,
                    venue,
                    BookShare {
                        index,
                        levels: ladder.levels,
                        allocation,
                    },
                )
            })
    }

    /// Read the two bounds an external response is held to, before the CPI that produces
    /// it. A book-backed quoter's permitted subjects live in state the execute consumes,
    /// and the execute overwrites the buffer the ladder was read from.
    ///
    /// The prefix is quantized at the same step the split used, because the split skips a
    /// level's sub-step tail and reaches further down the ladder. A prefix that stopped
    /// short would refuse a fill priced at the level the split allocated at.
    fn bound_external_leg(
        &self,
        venue: &ExternalVenue,
        index: usize,
        levels: &[PriceLevel],
        allocation: &QuoterAllocation,
    ) -> VelocityResult<(
        crate::state::prop_amm::QuoterSubjects,
        crate::math::router::QuotedPrefix,
    )> {
        let subjects =
            venue
                .router
                .executor
                .subjects(index, self.route_direction, allocation.base)?;
        let quoted = crate::math::router::quoted_prefix(
            levels,
            self.setup.quote_inputs.step_size,
            allocation.base,
        )?;

        Ok((subjects, quoted))
    }

    /// Execute one external book's allocation and settle what it answers.
    ///
    /// The response is untrusted in three ways, and each one is bounded before
    /// a single balance moves. The volume is never more than the allocation.
    /// The price stays inside the levels this quoter quoted in this same
    /// transaction. The subject is a user this quoter is allowed to act
    /// against. The loaded set is far wider than that set of subjects, and it
    /// holds the taker and every rival quoter's makers.
    fn settle_external_allocation(
        &mut self,
        market: &mut PerpMarket,
        filler: &mut FillerSide,
        venue: &mut ExternalVenue,
        share: BookShare,
    ) -> VelocityResult {
        let BookShare {
            index,
            levels,
            allocation,
        } = share;

        if allocation.base == 0 {
            return Ok(());
        }

        let (subjects, quoted) = self.bound_external_leg(venue, index, levels, allocation)?;
        // The guard lives here, for exactly as long as this leg reads the
        // response, so the records below borrow out of the quoter's account
        // instead of being copied onto velocity's heap.
        let located =
            venue
                .router
                .executor
                .execute(index, self.route_direction, allocation.base)?;
        let data = located.borrow()?;
        let response = located.execute_response(&data)?;
        let leg = ExternalLeg {
            quoter_key: venue.router.executor.quoter_key(index),
            protocol_authority: venue.router.standing.protocol_authority,
            subjects,
            quoted,
            oracle_band: venue
                .router
                .executor
                .oracle_band(index, market.margin_ratio_initial),
            maker_aggregates_tracked: venue
                .router
                .executor
                .quoter_type(index)
                .tracks_maker_aggregates(),
        };

        self.validate_external_volume_and_price(&leg, &response, allocation)?;

        for change_index in 0..response.changes.len() {
            self.settle_external_change(market, filler, &leg, &response, change_index)?;
        }

        if leg.maker_aggregates_tracked {
            self.unwind_culled_remainders(market, &leg, &response)?;
        }

        Ok(())
    }

    /// Hold a whole response to the allocation it answers.
    ///
    /// A quote is what its quoter can deliver. The CLOB spends execute's own
    /// fill and user budget while it walks, and a custom quoter's ladder was
    /// already cut to what its own margin supports, so the allocation is
    /// fillable in full. Anything less contradicts the quoter's own quote.
    ///
    /// Delivering nothing is the same contradiction as delivering part, and is
    /// refused the same way. A skipped empty response lets a quoter win base
    /// off a tight quote and leave the taker unfilled for that size, which a
    /// source that would have filled it never saw. The caller already skips an
    /// allocation of zero, so a response that reaches here answers real size.
    fn validate_external_volume_and_price(
        &self,
        leg: &ExternalLeg,
        response: &crate::state::prop_amm::ExecuteResponseV0,
        allocation: &QuoterAllocation,
    ) -> VelocityResult {
        let (ext_base, ext_quote) = response.changes.iter().try_fold(
            (0u64, 0u64),
            |(base, quote), change| -> VelocityResult<(u64, u64)> {
                // The per-change band and the subject check skip a change with
                // `base_size == 0` while its quote is still summed, so admitting one lets
                // a quoter carry quote on a record nothing else bounds.
                validate!(
                    change.base_size > 0,
                    ErrorCode::QuoterFillOffQuote,
                    "quoter {} returned a zero-base balance change carrying {} quote",
                    leg.quoter_key,
                    change.quote_size
                )?;

                Ok((
                    base.safe_add(change.base_size)?,
                    quote.safe_add(change.quote_size)?,
                ))
            },
        )?;

        validate!(
            ext_base <= allocation.base,
            ErrorCode::QuoterOverfilled,
            "quoter {} filled {} of the {} it quoted",
            leg.quoter_key,
            ext_base,
            allocation.base
        )?;
        validate!(
            ext_base == allocation.base,
            ErrorCode::QuoterFilledShort,
            "quoter {} filled {} of the {} it quoted",
            leg.quoter_key,
            ext_base,
            allocation.base
        )?;

        // Held to the notional the split accrued off the ladder, which the
        // allocation carries.
        validate!(
            crate::math::router::validate_allocated_notional(allocation, ext_quote)?,
            ErrorCode::QuoterFillOffQuote,
            "quoter {} filled {}/{} off its quote of {}",
            leg.quoter_key,
            ext_quote,
            ext_base,
            allocation.scaled_quote
        )?;

        Ok(())
    }

    /// Hold one balance change to the quote it came off and to the subject
    /// rule of the quoter that returned it.
    fn check_external_change(
        &self,
        leg: &ExternalLeg,
        change: &crate::state::prop_amm::UserBalanceChangeV0,
        maker_key: &Pubkey,
        completed: usize,
    ) -> VelocityResult {
        validate!(
            leg.subjects.permits(
                &change.user,
                maker_key,
                &self.taker_ref,
                &leg.protocol_authority
            ),
            ErrorCode::QuoterSubjectNotPermitted,
            "quoter {} may not act against user {}",
            leg.quoter_key,
            maker_key
        )?;
        validate!(
            crate::math::router::validate_change_notional(
                &leg.quoted,
                change.base_size,
                change.quote_size,
                merged_orders(completed)?,
            )?,
            ErrorCode::QuoterFillOffQuote,
            "quoter {} priced user {} outside its quoted band",
            leg.quoter_key,
            maker_key
        )?;

        // Per-leg oracle band. The aggregate checks bound a change against the blended
        // average, but one maker can sit far from oracle while the blend passes.
        // One-sided, because `validate_change_notional` already holds every change inside
        // a quoted prefix trimmed to the taker's effective limit, and a second band there
        // would refuse fills at the price the taker asked for.
        let change_price = (change.quote_size as u128)
            .safe_mul(BASE_PRECISION_U64.cast()?)?
            .safe_div(change.base_size.cast()?)?
            .cast::<u64>()?;
        validate!(
            !crate::math::orders::limit_price_breaches_maker_oracle_price_bands(
                change_price,
                self.maker_direction,
                self.setup.quote_inputs.oracle_price,
                leg.oracle_band,
            )?,
            ErrorCode::QuoterFillOffQuote,
            "quoter {} filled user {} at {} outside the oracle band",
            leg.quoter_key,
            maker_key,
            change_price
        )?;

        Ok(())
    }

    /// Settle one balance change out of an external book's response.
    fn settle_external_change(
        &mut self,
        market: &mut PerpMarket,
        filler: &mut FillerSide,
        leg: &ExternalLeg,
        response: &crate::state::prop_amm::ExecuteResponseV0,
        change_index: usize,
    ) -> VelocityResult {
        let change = &response.changes[change_index];
        if change.base_size == 0 {
            return Ok(());
        }

        // The count of orders this change completed, walked once for both the
        // band check and the open-order decrement.
        let completed = response.completed_count(change_index);
        let maker_key = self.resolve_user(&change.user)?;
        self.mark_settled(&maker_key);
        self.check_external_change(leg, change, &maker_key, completed)?;
        let mut maker = self.makers_and_referrer.get_ref_mut(&maker_key)?;
        self.settle_maker_funding(&mut maker, &maker_key, market)?;
        let mut maker_stats = maker_stats_for(
            self.makers_and_referrer_stats,
            self.taker.user.authority,
            &maker,
        )?;
        let mut maker_side = MakerSide::bind(
            &mut maker,
            maker_stats.as_deref_mut(),
            maker_key,
            self.taker.direction,
            self.market_index,
            leg.maker_aggregates_tracked,
            response.sole_client_order_id(change_index),
        )?;
        let (base_filled, quote_filled) = settle_external_match_fill(
            FillAmounts {
                base: change.base_size,
                quote: change.quote_size,
            },
            &mut self.taker,
            &mut maker_side,
            &ExternalMatch {
                effective_taker_limit: self.setup.effective_taker_limit,
                oracle_price: self.setup.quote_inputs.oracle_price,
            },
            filler,
            &mut SettleContext {
                market,
                rules: self.rules,
                mode: self.conditions.mode,
                oracle_map: self.oracle_map,
                now: self.conditions.now,
                slot: self.conditions.slot,
                filler_reward_paid: &mut self.tally.filler_reward_paid,
            },
        )?;

        self.note_fill(base_filled, quote_filled)?;

        let maker_position_index = get_position_index(&maker.perp_positions, self.market_index)?;
        let is_isolated = maker.perp_positions[maker_position_index].is_isolated();
        self.note_maker_fill(&maker_key, base_filled, is_isolated)?;
        if leg.maker_aggregates_tracked {
            self.release_completed_book_orders(
                &mut maker,
                maker_position_index,
                response.completed_for(change_index),
                completed,
            )?;
        }

        Ok(())
    }

    /// Bring a maker's funding stamp current before the fill touches its position. The
    /// settle helpers update positions directly, and a stale stamp fails
    /// `update_position_and_market`.
    fn settle_maker_funding(
        &self,
        maker: &mut User,
        maker_key: &Pubkey,
        market: &mut PerpMarket,
    ) -> VelocityResult {
        settle_funding_payment(maker, maker_key, market, self.conditions.now)
    }

    /// Record what one maker filled, for the post-fill checks to read.
    fn note_maker_fill(
        &mut self,
        maker_key: &Pubkey,
        base_filled: u64,
        is_isolated: bool,
    ) -> VelocityResult {
        update_maker_fills_map(
            &mut self.tally.maker_fills,
            maker_key,
            self.maker_direction,
            base_filled,
            is_isolated,
        )
    }

    /// Release the reservations the orders this change consumed outright held.
    ///
    /// Only a quoter whose makers are margin-reserved through velocity owes
    /// this. A fully-consumed order may also be a placed trigger's live half,
    /// and the shadow slot frees with it.
    fn release_completed_book_orders(
        &self,
        maker: &mut User,
        position_index: usize,
        completed_order_ids: impl Iterator<Item = u64>,
        completed: usize,
    ) -> VelocityResult {
        position::release_reserved_open_orders(
            &mut maker.perp_positions[position_index],
            completed.cast()?,
        )?;

        for clob_order_id in completed_order_ids {
            maker.decrement_open_orders(false);
            maker.release_placed_trigger_slot(
                self.market_index,
                clob_order_id,
                OrderStatus::Filled,
            );
        }

        Ok(())
    }

    /// Hold one culled remainder to the subject rule and to the market's own
    /// minimum.
    ///
    /// A cull is a remainder the book refused to let rest, so it is below the
    /// book's own minimum, and the attach requires that minimum to be at or
    /// under the market's. The release holds the figure to the maker's whole
    /// reservation. This holds it to the one order a cull can be about. A
    /// market with no minimum of its own bounds nothing, which is the same
    /// case the attach lets through.
    fn check_cull(
        &self,
        leg: &ExternalLeg,
        cancelled: &crate::state::prop_amm::CancelledRemainderV0,
        maker_key: &Pubkey,
        market: &PerpMarket,
    ) -> VelocityResult {
        validate!(
            leg.subjects.permits(
                &cancelled.user,
                maker_key,
                &self.taker_ref,
                &leg.protocol_authority
            ),
            ErrorCode::QuoterSubjectNotPermitted,
            "quoter {} may not cancel for user {}",
            leg.quoter_key,
            maker_key
        )?;
        validate!(
            market.market_stats.min_order_size == 0
                || cancelled.base_asset_amount < market.market_stats.min_order_size,
            ErrorCode::QuoterFillOffQuote,
            "quoter {} culled {} base, at or above the market minimum {}",
            leg.quoter_key,
            cancelled.base_asset_amount,
            market.market_stats.min_order_size
        )?;

        Ok(())
    }

    /// Unwind the sub-minimum remainders a quoter culled with this fill.
    ///
    /// The maker was just filled, so they are loaded. A cull releases a
    /// margin reservation, so it is held to the same subject rule as a
    /// balance change.
    fn unwind_culled_remainders(
        &mut self,
        market: &mut PerpMarket,
        leg: &ExternalLeg,
        response: &crate::state::prop_amm::ExecuteResponseV0,
    ) -> VelocityResult {
        for cancelled in response.cancelled {
            let maker_key = self.resolve_user(&cancelled.user)?;
            self.check_cull(leg, cancelled, &maker_key, market)?;
            let mut maker = self.makers_and_referrer.get_ref_mut(&maker_key)?;
            let maker_position_index =
                get_position_index(&maker.perp_positions, self.market_index)?;
            position::release_reserved_open_base(
                &mut maker.perp_positions[maker_position_index],
                &self.maker_direction,
                cancelled.base_asset_amount,
            )?;
            position::release_reserved_open_orders(
                &mut maker.perp_positions[maker_position_index],
                1,
            )?;

            maker.decrement_open_orders(false);
            maker.release_placed_trigger_slot(
                self.market_index,
                cancelled.order_id,
                OrderStatus::Canceled,
            );

            let is_isolated = maker.perp_positions[maker_position_index].is_isolated();
            drop(maker);
            // The cull is the only removal on this path velocity authors, and
            // it is bounded at one per book, so the record is one record.
            // Everything else the fill removed was consumed, and a consumed
            // order is reported by the fill.
            crate::instructions::emit_clob_cancel_record(
                self.conditions.now,
                market.market_stats.historical_oracle_data.last_oracle_price,
                &maker_key,
                crate::instructions::ClobOrderFacts {
                    order_id: cancelled.client_order_id,
                    market_index: self.market_index,
                    direction: self.maker_direction,
                    price: cancelled.price,
                    base_asset_amount: cancelled.base_asset_amount,
                    base_asset_amount_filled: 0,
                    max_ts: 0,
                    slot: self.conditions.slot,
                    taker_origin: false,
                },
                OrderActionExplanation::ClobRemainderCulled,
                None,
                None,
                is_isolated,
            )?;
        }

        Ok(())
    }

    /// The deferred mark TWAP and the 24-hour volume. Both are gated on a
    /// real fill.
    fn update_mark_twap_and_volume(&self, market: &mut PerpMarket) -> VelocityResult {
        let twap_trade_price = match self.taker.direction {
            PositionDirection::Long => self.setup.amm_ask_price,
            PositionDirection::Short => self.setup.amm_bid_price,
        };

        market.market_stats.update_mark_twap_with_amm_bid_ask(
            self.setup.amm_bid_price,
            self.setup.amm_ask_price,
            self.setup.amm_base_spread,
            self.setup.amm_long_spread,
            self.setup.amm_short_spread,
            self.conditions.now,
            Some(twap_trade_price),
            Some(self.taker.direction),
            self.setup.quote_inputs.sanitize_clamp_denominator,
            self.setup.quote_inputs.tick_size,
        )?;

        market.market_stats.update_volume_24h(
            self.tally.quote_filled,
            self.taker.direction,
            self.conditions.now,
        )
    }

    /// The taker's once-per-order open-orders counter. Only a taker that reserved at
    /// placement unwinds one. A fresh ephemeral taker never incremented one, so a
    /// decrement would underflow the per-position `u8`.
    fn decrement_taker_open_orders(&mut self) -> VelocityResult {
        if !self.taker.reserved || self.taker.order.get_base_asset_amount_unfilled(None)? != 0 {
            return Ok(());
        }

        let has_auction = self.taker.order.has_auction();
        self.taker.user.decrement_open_orders(has_auction);
        self.taker.user.perp_positions[self.taker.position_index].open_orders -= 1;
        Ok(())
    }

    /// Report what the pass moved, and apply what a settled pass owes.
    ///
    /// A pass that moved nothing owes none of it: the mark TWAP has no trade to
    /// record, the taker's reservation is untouched, and no book was reached.
    fn close_out(
        &mut self,
        market: &mut PerpMarket,
        venue: &mut ExternalVenue,
        filler_key: &Pubkey,
    ) -> VelocityResult<FillAmounts> {
        self.report_worst_fill_price(venue);
        let filled = FillAmounts {
            base: self.tally.base_filled,
            quote: self.tally.quote_filled,
        };

        if filled.base == 0 {
            return Ok(filled);
        }

        self.update_mark_twap_and_volume(market)?;
        self.decrement_taker_open_orders()?;
        self.check_withheld_obligation(venue, filler_key)?;
        Ok(filled)
    }

    /// What the party that built the transaction owes when a book withheld
    /// depth.
    ///
    /// A book asked for an owner this transaction does not carry. Whoever
    /// built the transaction owes the taker every maker it had room for, so
    /// count the loaded users that filled nothing and hold no role in the
    /// fill. Those accounts spent locks the missing maker needed.
    fn check_withheld_obligation(
        &self,
        venue: &ExternalVenue,
        filler_key: &Pubkey,
    ) -> VelocityResult {
        if !self.withheld_depth {
            return Ok(());
        }

        // `settled_users` and `idle_loaded_users` address loaded users by a bit in a u64,
        // so a map past 64 users would read a filled maker as idle and fail an honest
        // fill. `MAX_QUOTER_WIRE_USERS` keeps a real fill well under that. Widen the
        // bitmap rather than miscounting if the caps grow.
        validate!(
            self.makers_and_referrer.0.len() <= u64::BITS as usize,
            ErrorCode::DefaultError,
            "loaded user map has {} users, past the 64 the obligation bitmap covers",
            self.makers_and_referrer.0.len()
        )?;

        let idle = idle_loaded_users(
            self.makers_and_referrer,
            self.tally.settled_users,
            &self.taker.key,
            filler_key,
            self.taker.stats.referrer,
            &*venue.router.executor,
        )?;

        crate::math::router::withheld_obligation(
            &venue.router.standing.obligation,
            idle,
            self.attributable_writable_locks(venue, filler_key),
        )
    }

    /// Writable locks this transaction spends on work velocity can name.
    ///
    /// The transaction's own lock count states what the caller claims. Every
    /// account counted here was loaded as the type velocity expected, so a
    /// key that names nothing adds nothing, and a caller cannot raise this
    /// number by naming more keys.
    ///
    /// Undercounts on purpose. It counts the accounts this fill holds and
    /// leaves out everything else the transaction may carry, such as the
    /// accounts of a force-cancel that runs ahead of the fill. A prefix of
    /// that kind names the same taker, market and makers as the fill, so it
    /// adds few locks of its own. Counting low refuses a withhold rather than
    /// excusing one, which is the direction the taker is safe in.
    fn attributable_writable_locks(&self, venue: &ExternalVenue, filler_key: &Pubkey) -> usize {
        let loaded_users = self
            .makers_and_referrer
            .0
            .keys()
            .filter(|key| **key != self.taker.key)
            .count();
        let loaded_stats = self
            .makers_and_referrer_stats
            .0
            .keys()
            .filter(|authority| **authority != self.taker.user.authority)
            .count();
        // A filler that is not a loaded maker holds its own `User` and
        // `UserStats`. A maker that cranked its own fill is already counted
        // above, and a fill with no filler names no key at all.
        let filler_locks = if *filler_key != Pubkey::default()
            && *filler_key != self.taker.key
            && !self.makers_and_referrer.0.contains_key(filler_key)
        {
            crate::math::router::MAKER_ACCOUNT_COST
        } else {
            0
        };

        // One writable account per consulted quoter: the account it writes
        // its response into. Its registry slab is shared by the whole route
        // and the `User` it settles for is a loaded user, so both are counted
        // elsewhere or not at all.
        let quoter_locks = (0..crate::state::prop_amm::MAX_ROUTE_QUOTERS)
            .filter(|index| venue.router.executor.quoter_key(*index) != Pubkey::default())
            .count();
        crate::math::router::FILL_FIXED_WRITABLE_LOCKS
            .saturating_add(loaded_users)
            .saturating_add(loaded_stats)
            .saturating_add(filler_locks)
            .saturating_add(quoter_locks)
    }
}

/// Draw the taker's size from every liquidity source, in one pass.
///
/// This layer governs liquidity. It quotes each source, splits the taker's unfilled size
/// across them by priority tier, executes each allocation and settles what comes back. It
/// measures no risk of its own, because the caller sets the limits it runs inside.
///
/// Quoting and routing happen together over one curve. So there is no scratch-AMM
/// projection, no separate JIT participant, and no per-step fallback recompute. The vAMM
/// quotes last, with every other source as its last look, and one effective taker limit
/// bounds every book up front.
///
/// An allocation on an external book executes through `RouterLeg::executor`. A quoter
/// requotes off the same oracle, slot and tick it was quoted against, and the settle
/// pass's `validate_fill_price` enforces that. [`PerpFill`] carries what the steps
/// share.
pub(super) fn fill_from_liquidity_sources(
    taker: &mut TakerSide,
    rules: &PricingRules,
    conditions: &FillConditions,
    parties: &mut FillParties,
    liquidity: &mut OfferedLiquidity,
    filler: &mut FillerSide,
) -> VelocityResult<(u64, u64, MakerFills)> {
    let market_index = taker.order.market_index;
    let target_size = taker.unfilled_target()?;
    if target_size == 0 {
        return Ok((0, 0, MakerFills::new()));
    }

    // ---- The market snapshot every source quotes against. ----
    let AccountMaps {
        perp_market_map,
        oracle_map,
        ..
    } = &mut *parties.maps;
    let mut market = perp_market_map.get_ref_mut(&market_index)?;
    let (mut amm_quoter, setup) =
        refresh_and_read_market(market.deref_mut(), oracle_map, taker, rules, conditions)?;

    let mut venue = ExternalVenue::new(liquidity.router);
    let mut fill = PerpFill::new(
        FillCounterparties {
            oracle_map,
            users: parties.makers_and_referrer,
            stats: parties.makers_and_referrer_stats,
        },
        &venue,
        rules,
        conditions,
        setup,
        taker.reborrow(),
    )?;

    // ---- Quote, split, and take the vAMM's share while the curve is held. ----
    let routed = fill.route_across_sources(&venue, &mut amm_quoter, target_size)?;

    // ---- Release `market.amm`, then settle each source. ----
    drop(amm_quoter);
    fill.settle_routed_fill(market.deref_mut(), filler, &mut venue, &routed)?;

    let filled = fill.close_out(market.deref_mut(), &mut venue, &filler.key)?;
    Ok((filled.base, filled.quote, fill.tally.maker_fills))
}

/// Refresh the vAMM and read the snapshot every source quotes against.
///
/// The quoter comes back still holding `market.amm`, because the caller quotes
/// and executes the vAMM leg before it releases the curve.
fn refresh_and_read_market<'m>(
    market: &'m mut PerpMarket,
    oracle_map: &mut OracleMap,
    taker: &TakerSide,
    rules: &PricingRules,
    conditions: &FillConditions,
) -> VelocityResult<(AmmQuoter<'m>, FillMarketSetup)> {
    let oracle_price_data = *oracle_map.get_price_data(&market.oracle_id())?;
    let quote_inputs = QuoteInputs::load(
        market,
        oracle_price_data,
        conditions.slot,
        rules.validity_guard_rails,
        oracle_map.slot_clock,
    )?;
    let market_fee_adjustment = market.fee_adjustment;
    let mut amm_quoter = AmmQuoter::for_amm(&mut market.amm);
    let setup = FillMarketSetup::load(
        &mut amm_quoter,
        quote_inputs,
        taker,
        rules,
        conditions,
        market_fee_adjustment,
    )?;

    Ok((amm_quoter, setup))
}

/// The maker's own stats, when it has any loaded.
///
/// A maker that is another subaccount of the taker's authority is the same
/// `UserStats` account the taker already holds, so it cannot be loaded a
/// second time. Its volume and rebate land on the taker's stats instead.
fn maker_stats_for<'m>(
    stats: &'m UserStatsMap,
    taker_authority: Pubkey,
    maker: &User,
) -> VelocityResult<Option<RefMut<'m, UserStats>>> {
    if maker.authority == taker_authority {
        return Ok(None);
    }

    Ok(Some(stats.get_ref_mut(&maker.authority)?))
}

/// Loaded users that the fill did not move and that hold no role in it.
///
/// A role is one of four: the taker, the filler, the taker's referrer, or the
/// account a registered quoter fills for. Each of those has to be loaded whether
/// or not it receives a balance change. Everything else in the map is there to
/// be filled, and one that filled nothing spent two account locks for nothing.
fn idle_loaded_users<'info>(
    makers_and_referrer: &UserMap,
    settled_users: u64,
    taker_key: &Pubkey,
    filler_key: &Pubkey,
    referrer_authority: Pubkey,
    executor: &dyn crate::state::prop_amm::ExternalQuoterExecutor<'info>,
) -> VelocityResult<usize> {
    let mut idle = 0usize;
    for (index, (key, loader)) in makers_and_referrer.0.iter().enumerate() {
        if index < u64::BITS as usize && settled_users & (1u64 << index) != 0 {
            continue;
        }
        if key == taker_key || key == filler_key {
            continue;
        }

        let authority = loader
            .load()
            .map_err(|_| ErrorCode::UnableToLoadAccountLoader)?
            .authority;
        if authority == referrer_authority && referrer_authority != Pubkey::default() {
            continue;
        }
        if (0..crate::state::prop_amm::MAX_ROUTE_QUOTERS).any(|i| executor.quoter_user(i) == *key) {
            continue;
        }

        idle = idle.saturating_add(1);
    }

    Ok(idle)
}

/// How many of a quoter's own orders one balance change merges, as the
/// response itself declares: every order the change consumed outright, plus
/// at most one it left a remainder on. Bounds the integer rounding a merged
/// record can carry (see `math::router::validate_change_notional`).
fn merged_orders(consumed: usize) -> VelocityResult<u64> {
    consumed.cast::<u64>()?.safe_add(1)
}
