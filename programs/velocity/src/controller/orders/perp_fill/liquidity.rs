//! The liquidity layer of a perp fill.
//!
//! This layer governs liquidity. It quotes every source, splits the taker's
//! unfilled size across them, executes each allocation and settles what comes
//! back. It measures no risk of its own: [`super::taker_risk`] sets the limits
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
            router::{split_across_quoters, QuoterAllocation, QuoterBook, RouterFillInputs},
            safe_math::SafeMath,
        },
        state::{
            events::OrderActionExplanation,
            oracle::OraclePriceData,
            oracle_map::OracleMap,
            perp_market::PerpMarket,
            prop_amm::{ClobUserRefV0, Direction, PriceLevel, QuoterType},
            quoter::{
                DlobOrderQuoter, MarketQuoteInputs as QuoteInputs, QuoteContext, QuoterFill,
                RouterQuoter,
            },
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

/// One maker order the fill may match, as the single price level the split
/// sees. The price is the sanitized one discovery froze the order at.
struct RouterMaker {
    key: Pubkey,
    order_index: usize,
    price: u64,
    unfilled: u64,
    is_isolated: bool,
}

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
    /// The ceiling the vAMM ladder is cut at, tighter than the one the maker
    /// books get.
    ///
    /// A post-only taker acts as a maker, so its limit is buffered by the
    /// maker rebate it earns and stepped one tick inside the limit. A ladder
    /// bounded by the raw limit instead lets a post-only order sweep past the
    /// buffer. That is more size, and every unit priced at the raw limit
    /// rather than the buffered one, which is LP value handed to the taker.
    /// The maker books keep the raw limit, because the buffer is the AMM's
    /// and not theirs.
    amm_taker_limit: Option<u64>,
    /// The one ceiling every maker book is cut at. A market order falls back
    /// to the AMM fallback price, so a router sweep stays price-bounded the
    /// way the legacy match legs were.
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
        policy: &FillPolicy,
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
            &crate::math::fees::determine_user_fee_tier(
                taker.stats,
                policy.fee_structure,
                &MarketType::Perp,
                now,
                policy.promo_fee_tier,
            )?,
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
/// stands in for one. That keeps a router sweep price-bounded the way the
/// legacy match legs were.
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

/// One perp fill in progress: what every step of the route reads, built once
/// by [`PerpFill::new`]. A step takes this and its own arguments.
///
/// Two things are deliberately not fields. The market is one: the vAMM quoter
/// borrows `market.amm` for the whole quote window, so a step reached through
/// this struct while that borrow is live would collide with it. The filler is
/// the other: its four lifetimes are load-bearing, and folding them in would
/// put nine lifetime parameters on every step. Both are named by the steps
/// that use them.
/// What one liquidity pass has moved so far.
///
/// Every source settles into this and nothing reads a running total except the
/// steps that close the pass out, so the accumulators are kept apart from the
/// snapshot the pass quotes against.
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

/// The external quoter books this fill may route to, and the leg that executes
/// on them.
///
/// Everything an external book touches sits behind this: the ladders the split
/// reads, the CPI that executes an allocation, and what the party that built
/// the transaction owes when a book withheld depth. It is also the only part
/// of the fill that borrows the quoting section and the executor's account
/// region, so holding it apart keeps three lifetimes off [`PerpFill`].
struct ExternalVenue<'a, 'r, 'b, 'info> {
    router: &'a mut RouterFillInputs<'r, 'b, 'info>,
    /// [`RouterFillInputs::books`], read out once so a step can hold a ladder
    /// while the executor runs.
    books: &'r [QuoterBook<'b>],
}

impl<'a, 'r, 'b, 'info> ExternalVenue<'a, 'r, 'b, 'info> {
    fn new(router: &'a mut RouterFillInputs<'r, 'b, 'info>) -> Self {
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

/// One external book's execution: which book answered, the quoter that stands
/// behind it, and the bounds its response is held to.
///
/// Every field is read before the CPI runs. The checks that hold a response
/// honest therefore never reach back into the executor while a borrow of that
/// response is live, and each of them names the quoter directly rather than an
/// index into somebody else's table.
struct ExternalLeg {
    /// The quoter that answered, for the messages the checks emit.
    quoter_key: Pubkey,
    /// `State::signer` — the account no quoter may name as a fill subject.
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

/// One quote-and-split pass: what the DLOB makers offered, what the split gave
/// every source, and what the vAMM leg already executed.
///
/// The allocations are in book order: the external books first, then the DLOB
/// makers, then the vAMM. The two boundaries say where each run ends.
struct RoutedFill {
    /// The DLOB maker orders the pass quoted, in book order.
    router_makers: Vec<RouterMaker>,
    /// One allocation per book.
    allocations: Vec<QuoterAllocation>,
    /// What the vAMM leg executed, when it won an allocation.
    amm_fill: Option<QuoterFill>,
    /// Where the external allocations end.
    externals_end: usize,
    /// Where the DLOB maker allocations end.
    makers_end: usize,
}

impl RoutedFill {
    /// What the split gave the external books.
    fn external_allocations(&self) -> &[QuoterAllocation] {
        &self.allocations[..self.externals_end]
    }

    /// What the split gave the DLOB makers.
    fn maker_allocations(&self) -> &[QuoterAllocation] {
        &self.allocations[self.externals_end..self.makers_end]
    }

    /// What the split gave the vAMM.
    fn amm_allocation(&self) -> &QuoterAllocation {
        &self.allocations[self.makers_end]
    }
}

/// What the liquidity pass carries across its own steps.
///
/// The external books are not here. They live in [`ExternalVenue`], which each
/// step that reaches one takes as an argument, so the three lifetimes the
/// router carries stay out of this context.
///
/// The three that remain cannot merge. `OracleMap<'o>`, `UserMap<'m>` and
/// `UserStatsMap<'s>` each hold `AccountInfo`, which is invariant in its
/// lifetime, and the three maps reach the fill from three separately elided
/// caller regions. Merging any pair makes two unrelated caller regions equal,
/// and every caller then fails to compile. `'a` is the borrow of the caller's
/// own frame.
struct PerpFill<'a, 'o, 'm, 's> {
    oracle_map: &'a mut OracleMap<'o>,
    makers_and_referrer: &'a UserMap<'m>,
    makers_and_referrer_stats: &'a UserStatsMap<'s>,
    /// What the pass has moved so far.
    tally: FillTally,
    policy: &'a FillPolicy<'a>,
    /// How many external books the route carries. It is where the external
    /// allocations end and the DLOB maker allocations begin.
    external_book_count: usize,
    taker: TakerSide<'a>,
    setup: FillMarketSetup,
    market_index: u16,
    /// Opposite the taker's, by construction.
    maker_direction: PositionDirection,
    /// The taker's side, as the router states it.
    route_direction: Direction,
    now: i64,
    slot: u64,
    /// Whether the vAMM may fill this order at all.
    amm_is_available: bool,
    vamm_maker_rebate: bool,
    taker_limit_price: Option<u64>,
    /// True when a book stopped its walk at an owner this transaction does
    /// not carry. Only then does the fill owe the obligation check, which is
    /// what keeps that cost off every ordinary fill.
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
        policy: &'a FillPolicy<'a>,
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
        // loads every user, so skip it when there is nothing to resolve for.
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
            policy,
            taker,
            taker_limit_price: setup.taker_limit_price,
            setup,
            amm_is_available: conditions.amm_is_available,
            vamm_maker_rebate: policy.vamm_maker_rebate,
            now: conditions.now,
            slot: conditions.slot,
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

    /// Hand the worst price any one source executed at back to the caller.
    ///
    /// The fill's own return value is the base and the blended quote, and a
    /// blend hides its own tail. A caller that must know whether every unit
    /// cleared a price reads this instead.
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

    /// The maker orders this fill may match, as plain price levels frozen at
    /// the prices discovery sanitized them to.
    ///
    /// A maker order with nothing left to fill is dropped here, so the split
    /// never allocates to it.
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
        dlob_makers: &[MakerOrderInfo],
        target_size: u64,
    ) -> VelocityResult<RoutedFill> {
        let router_makers = self.quote_dlob_makers(dlob_makers)?;
        let maker_levels: Vec<[PriceLevel; 1]> = router_makers
            .iter()
            .map(|maker| {
                [PriceLevel {
                    price: maker.price,
                    size: maker.unfilled,
                }]
            })
            .collect();
        let rivals = self.rival_books(venue, &maker_levels);
        let amm_levels = self.quote_vamm(amm_quoter, &rivals, target_size)?;

        // The vAMM book is NOT re-truncated: `vamm_quote_levels` already
        // capped the ladder at the limit, and its per-rung prices are rounded
        // slice averages. Comparing those to the limit would drop dust rungs
        // whose true cost is inside it.
        //
        // `books` takes the rival allocation over. It is declared after
        // `amm_levels` so it drops first, which is what lets it hold a level
        // of the vAMM's ladder.
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
        let makers_end = externals_end + maker_levels.len();
        let amm_fill = self.execute_vamm(amm_quoter, &allocations[makers_end])?;

        Ok(RoutedFill {
            router_makers,
            allocations,
            amm_fill,
            externals_end,
            makers_end,
        })
    }

    fn quote_dlob_makers(
        &self,
        maker_orders_info: &[MakerOrderInfo],
    ) -> VelocityResult<Vec<RouterMaker>> {
        maker_orders_info.iter().try_fold(
            Vec::with_capacity(maker_orders_info.len()),
            |mut makers, info| -> VelocityResult<Vec<RouterMaker>> {
                let key = info.key(self.makers_and_referrer)?;
                let order_index = info.slot();
                let maker = self.makers_and_referrer.get_ref(&key)?;
                let position = maker.get_perp_position(self.market_index)?;
                let unfilled = maker.orders[order_index]
                    .get_base_asset_amount_unfilled(Some(position.base_asset_amount))?;
                if unfilled > 0 {
                    makers.push(RouterMaker {
                        key,
                        order_index,
                        price: info.price,
                        unfilled,
                        is_isolated: position.is_isolated(),
                    });
                }
                Ok(makers)
            },
        )
    }

    /// Every book but the vAMM's, each cut at the taker's effective limit,
    /// in one allocation with room for the vAMM's own book after it.
    ///
    /// The vAMM shades against this set as its last look, and the split then
    /// runs over the same set with the vAMM appended. The caller hands this
    /// allocation on rather than collecting the same rows a second time: the
    /// runtime's allocator never reclaims, so a second copy is heap the
    /// instruction does not get back.
    ///
    /// Sized for the whole set, the vAMM's slot included, for the same
    /// reason. A `Vec` at capacity doubles when pushed into, and doubling
    /// abandons a buffer as large as the one it replaces — so the caller's
    /// one `push` of the vAMM book cost the set's own size a second time,
    /// which at a full maker ladder is kilobytes the fill never gets back.
    fn rival_books<'l, 'r: 'l, 'b: 'l>(
        &self,
        venue: &ExternalVenue<'_, 'r, 'b, '_>,
        maker_levels: &'l [[PriceLevel; 1]],
    ) -> Vec<QuoterBook<'l>> {
        let clob_tier = QuoterType::Clob.default_priority();
        let mut books = Vec::with_capacity(venue.books.len() + maker_levels.len() + 1);
        books.extend(venue.books.iter().map(|book| QuoterBook {
            priority: book.priority,
            levels: &book.levels[..self.within_limit(book.levels)],
            withheld: book.withheld,
        }));
        books.extend(maker_levels.iter().map(|levels| QuoterBook {
            priority: clob_tier,
            levels: &levels[..self.within_limit(levels.as_slice())],
            withheld: PriceLevel::default(),
        }));
        books
    }

    /// The external quoter books alone, each cut to the taker's limit.
    ///
    /// The ladders are already the depth this fill may settle: a custom
    /// quoter's is trimmed to its own band and its own account's room when
    /// the route is assembled, and a book sizes its makers as it walks them.
    /// So the split and the settle pass read the same levels by
    /// construction, rather than by rebuilding a clamp the same way twice.
    fn external_rival_books<'l, 'r: 'l, 'b: 'l>(
        &self,
        venue: &ExternalVenue<'_, 'r, 'b, '_>,
    ) -> Vec<QuoterBook<'l>> {
        venue
            .books
            .iter()
            .map(|book| QuoterBook {
                priority: book.priority,
                levels: &book.levels[..self.within_limit(book.levels)],
                withheld: book.withheld,
            })
            .collect()
    }

    /// The vAMM ladder. The vAMM quotes last, so every other book is its last
    /// look.
    fn quote_vamm(
        &self,
        amm_quoter: &AmmQuoter,
        rivals: &[QuoterBook],
        target_size: u64,
    ) -> VelocityResult<Vec<PriceLevel>> {
        if !self.amm_is_available {
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
            &self.setup.quote_inputs.ctx(self.slot),
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

    /// The oracle a DLOB maker is re-quoted against.
    ///
    /// The *same* price discovery froze the book at, which is the MM price
    /// and not the confidence-bounded safe price. An oracle-offset maker
    /// prices off `ctx.oracle`, so a different price here re-quotes it away
    /// from its quoted level and trips the at-or-better check. That fails the
    /// whole fill closed instead of filling.
    fn discovery_oracle(&self) -> OraclePriceData {
        OraclePriceData {
            price: self.setup.quote_inputs.mm_oracle.get_price(),
            ..self.setup.quote_inputs.safe_oracle
        }
    }

    /// The context one DLOB maker's order re-quotes through.
    fn dlob_quote_context<'q>(&'q self, oracle: &'q OraclePriceData) -> QuoteContext<'q> {
        QuoteContext {
            stats: &self.setup.quote_inputs.stats,
            oracle,
            mm_oracle: None,
            oracle_validity: None,
            fee_budget: 0,
            tick: self.setup.quote_inputs.tick_size,
            step_size: self.setup.quote_inputs.step_size,
            slot: self.slot,
            slot_clock: self.oracle_map.slot_clock,
            base_precision: BASE_PRECISION_U64,
            market_status: MarketStatus::default(),
            market_config: 0,
        }
    }

    /// Hold one maker's fill to the allocation it answers.
    fn check_dlob_fill(
        &self,
        maker_key: &Pubkey,
        fill: &QuoterFill,
        allocation: &QuoterAllocation,
    ) -> VelocityResult {
        validate!(
            fill.base_filled <= allocation.base,
            ErrorCode::DefaultError,
            "router maker {} overfilled: {} > {}",
            maker_key,
            fill.base_filled,
            allocation.base
        )?;
        validate!(
            crate::controller::matching::fill_at_or_better(
                self.taker.direction,
                fill,
                allocation,
                BASE_PRECISION_U64
            )?,
            ErrorCode::DefaultError,
            "router maker {} filled worse than quoted",
            maker_key
        )?;
        Ok(())
    }

    /// Execute and settle every DLOB maker's allocation.
    /// Settle each source's allocation into the accounts it moved.
    ///
    /// The DLOB legs go first, then the vAMM, then the external books. The
    /// external ladders are rebuilt from the levels the split was cut from,
    /// so a balance change is held to the prices it was allocated at.
    fn settle_routed_fill(
        &mut self,
        market: &mut PerpMarket,
        filler: &mut FillerSide,
        venue: &mut ExternalVenue,
        routed: &RoutedFill,
    ) -> VelocityResult {
        self.settle_dlob_allocations(
            market,
            filler,
            &routed.router_makers,
            routed.maker_allocations(),
        )?;
        if let Some(amm_fill) = routed.amm_fill.as_ref() {
            self.settle_vamm_allocation(market, filler, amm_fill, routed.amm_allocation())?;
        }
        let ladders = self.external_rival_books(venue);
        self.settle_external_allocations(
            market,
            filler,
            venue,
            &ladders,
            routed.external_allocations(),
        )
    }

    fn settle_dlob_allocations(
        &mut self,
        market: &mut PerpMarket,
        filler: &mut FillerSide,
        makers: &[RouterMaker],
        allocations: &[QuoterAllocation],
    ) -> VelocityResult {
        makers
            .iter()
            .zip(allocations)
            .try_for_each(|(maker, allocation)| {
                self.settle_dlob_allocation(market, filler, maker, allocation)
            })
    }

    /// Execute one maker's allocation off their resting order, then settle it.
    fn settle_dlob_allocation(
        &mut self,
        market: &mut PerpMarket,
        filler: &mut FillerSide,
        router_maker: &RouterMaker,
        allocation: &QuoterAllocation,
    ) -> VelocityResult {
        if allocation.base == 0 {
            return Ok(());
        }
        let mut maker = self.makers_and_referrer.get_ref_mut(&router_maker.key)?;
        self.settle_maker_funding(&mut maker, &router_maker.key, market)?;
        let (maker_position_index, maker_existing_position_params) =
            self.resting_maker_position(&maker)?;
        // A `DlobMatch` only lands from a step that set an effective taker
        // limit, so the match is always price-bounded on the taker side.
        let taker_limit_for_match = self
            .setup
            .effective_taker_limit
            .ok_or_else(print_error!(ErrorCode::DefaultError))?;

        let Some(fill) = self.execute_dlob_order(&mut maker, router_maker, allocation)? else {
            return Ok(());
        };

        let mut maker_stats = maker_stats_for(
            self.makers_and_referrer_stats,
            self.taker.user.authority,
            &maker,
        )?;
        // A resting DLOB order reserved its worst case at placement, so this
        // fill unwinds that reservation.
        let matched = DlobMatch {
            order_index: router_maker.order_index,
            maker_price: router_maker.price,
            taker_limit: taker_limit_for_match,
            oracle_price: self.setup.quote_inputs.oracle_price,
        };
        let (base_filled, quote_filled, maker_filled) = settle_dlob_match_fill(
            &fill,
            &mut self.taker,
            &mut MakerSide {
                user: &mut maker,
                stats: maker_stats.as_deref_mut(),
                key: router_maker.key,
                direction: self.maker_direction,
                position_index: maker_position_index,
                existing_position_params: maker_existing_position_params,
                reserved: true,
                order_id: None,
            },
            &matched,
            filler,
            &mut SettleContext {
                market,
                policy: self.policy,
                oracle_map: self.oracle_map,
                now: self.now,
                slot: self.slot,
                filler_reward_paid: &mut self.tally.filler_reward_paid,
            },
        )?;
        self.note_fill(base_filled, quote_filled)?;
        if maker_filled != 0 {
            self.note_maker_fill(&router_maker.key, maker_filled, router_maker.is_isolated)?;
        }
        self.release_filled_maker_order(&mut maker, router_maker.order_index)
    }

    /// Quote the maker's resting order against this allocation and take what
    /// it fills.
    ///
    /// `None` when the order had nothing left to give, which is not an error:
    /// the split allocated off a frozen price, and the order may have moved.
    fn execute_dlob_order(
        &mut self,
        maker: &mut User,
        router_maker: &RouterMaker,
        allocation: &QuoterAllocation,
    ) -> VelocityResult<Option<QuoterFill>> {
        let discovery_oracle = self.discovery_oracle();
        let fill = {
            let ctx = self.dlob_quote_context(&discovery_oracle);
            let mut dlob = DlobOrderQuoter::new(
                &mut maker.orders[router_maker.order_index],
                router_maker.unfilled,
            );
            RouterQuoter::execute(&mut dlob, &ctx, self.route_direction, allocation.base)?
        };
        if fill.base_filled == 0 {
            return Ok(None);
        }
        self.mark_settled(&router_maker.key);
        self.check_dlob_fill(&router_maker.key, &fill, allocation)?;
        Ok(Some(fill))
    }

    /// Release the open-orders counter a fully-filled maker order held.
    ///
    /// The settle leg already released the order's open base. This is the
    /// once-per-order half, which only the order's last fill owes.
    fn release_filled_maker_order(&self, maker: &mut User, order_index: usize) -> VelocityResult {
        if maker.orders[order_index].get_base_asset_amount_unfilled(None)? != 0 {
            return Ok(());
        }
        let position_index = get_position_index(&maker.perp_positions, self.market_index)?;
        let has_auction = maker.orders[order_index].has_auction();
        maker.decrement_open_orders(has_auction);
        maker.perp_positions[position_index].open_orders -= 1;
        Ok(())
    }

    /// Settle the vAMM's fill against the house.
    ///
    /// Runs after the DLOB legs, which have released their borrows by here.
    fn settle_vamm_allocation(
        &mut self,
        market: &mut PerpMarket,
        filler: &mut FillerSide,
        fill: &QuoterFill,
        allocation: &QuoterAllocation,
    ) -> VelocityResult {
        // A maker that cranked this fill did the keeper's work for the *whole*
        // order, not just its own slice, so it earns the reward on the vAMM
        // slice too. It arrives as `filler: None` with `filler_key` naming
        // itself. It is already loaded in the maker map, so it cannot be
        // loaded a second time as the filler. The reward is gated on it having
        // actually filled, so a maker that names itself but wins no allocation
        // earns nothing.
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
                pays_maker_rebate: self.vamm_maker_rebate,
            },
            &AmmAllocation {
                quote: allocation.quote,
                base: allocation.base,
                post_only: order_post_only,
                order_slot,
                order_id,
                taker_limit: self.taker_limit_price,
            },
            filler,
            &mut SettleContext {
                market,
                policy: self.policy,
                oracle_map: self.oracle_map,
                now: self.now,
                slot: self.slot,
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

    /// Read the two bounds an external response is held to, before the CPI
    /// that produces it.
    ///
    /// A book-backed quoter's permitted subjects live in the state its execute
    /// is about to consume, and the execute overwrites the very buffer the
    /// ladder was read from, so both are read first.
    ///
    /// The prefix is quantized at the same step the split used. The split
    /// skips a level's sub-step tail, so it reaches further down the ladder
    /// than an unquantized walk of the same base does, and the two then
    /// disagree about which levels the allocation was cut from. The price band
    /// is built from this prefix's best and worst price, so a prefix that
    /// stopped short would refuse an honest fill priced at the level the split
    /// allocated at.
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
    /// The response is untrusted on three axes, and each is bounded before a
    /// single balance moves: the volume (never more than allocated), the
    /// price (inside the levels this quoter quoted moments ago, in this same
    /// transaction), and the subject (a user this quoter is allowed to act
    /// against — the loaded set is far wider than that, and it holds the
    /// taker and every rival quoter's makers).
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
            protocol_authority: venue.router.protocol_authority,
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
    /// fillable in full. Anything else is the quoter contradicting its own
    /// quote.
    ///
    /// Delivering nothing is the same contradiction as delivering part, and
    /// is treated the same way. It used to be skipped, which let a quoter win
    /// base off a tight quote and hand the taker a hole: the size went
    /// nowhere, and a source that would have filled it never saw it. An
    /// allocation of zero is already skipped by the caller, so reaching here
    /// with nothing means this quoter was given real size.
    fn validate_external_volume_and_price(
        &self,
        leg: &ExternalLeg,
        response: &crate::state::prop_amm::ExecuteResponseV0,
        allocation: &QuoterAllocation,
    ) -> VelocityResult {
        let (ext_base, ext_quote) = response.changes.iter().try_fold(
            (0u64, 0u64),
            |(base, quote), change| -> VelocityResult<(u64, u64)> {
                // A zero-base change is not a fill, so it has no place in the
                // response. Admitting one lets a quoter carry quote on a
                // record the per-change band and the subject check both skip
                // (they continue on base_size == 0), while its quote was
                // already summed here. A short taker is then settled at the
                // quoter's worst rung and the quoter keeps the difference.
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
        // Held to the number the split accrued off the ladder, so the ladder
        // itself is dead the moment routing ends.
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
        // Per-leg oracle band. The quoted-band and the aggregate checks bound
        // a change against the quoter's own quote and the blended average,
        // but a single maker can still sit far from oracle while the blend
        // passes. That is value moved onto that maker at a price the average
        // hides. Bound each maker's fill price the way the DLOB match path
        // bounds a resting maker order.
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
                taker_limit: self.setup.effective_taker_limit,
                oracle_price: self.setup.quote_inputs.oracle_price,
            },
            filler,
            &mut SettleContext {
                market,
                policy: self.policy,
                oracle_map: self.oracle_map,
                now: self.now,
                slot: self.slot,
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

    /// Bring a maker's funding stamp current before the fill touches its
    /// position.
    ///
    /// The settle helpers update positions directly, and a stale stamp fails
    /// `update_position_and_market`. Every match path runs this same
    /// pre-flight.
    fn settle_maker_funding(
        &self,
        maker: &mut User,
        maker_key: &Pubkey,
        market: &mut PerpMarket,
    ) -> VelocityResult {
        settle_funding_payment(maker, maker_key, market, self.now)
    }

    /// The resting position a DLOB maker order settles into, and what the fill
    /// record needs to know about it before it moves.
    ///
    /// A resting order holds `open_orders` on its owner's position, so that
    /// position always exists.
    fn resting_maker_position(&self, maker: &User) -> VelocityResult<(usize, Option<(u64, u64)>)> {
        let position_index = get_position_index(&maker.perp_positions, self.market_index)?;
        let existing = maker.perp_positions[position_index]
            .get_existing_position_params_for_order_action(self.maker_direction);
        Ok((position_index, existing))
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
    /// reservation; this holds it to the one order a cull can be about. A
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
                self.now,
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
                    slot: self.slot,
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
            self.now,
            Some(twap_trade_price),
            Some(self.taker.direction),
            self.setup.quote_inputs.sanitize_clamp_denominator,
            self.setup.quote_inputs.tick_size,
        )?;
        market.market_stats.update_volume_24h(
            self.tally.quote_filled,
            self.taker.direction,
            self.now,
        )
    }

    /// The taker's once-per-order open-orders counter.
    ///
    /// Only a taker that reserved at placement unwinds a count here. A fresh
    /// ephemeral taker never incremented one, so a decrement would underflow
    /// the per-position `u8` and wrongly drop the user-level count.
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
        // `settled_users` and `idle_loaded_users` address loaded users by a
        // bit in a u64. A loaded map past 64 users cannot mark a filled maker
        // beyond index 64 as settled, so it would read as idle and fail an
        // honest fill. Guard the assumption loudly. The wire user set is
        // capped well under 64 (`MAX_QUOTER_WIRE_USERS`), so a real fill never
        // reaches this. If the caps ever grow, widen the bitmap instead of
        // silently miscounting.
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
        crate::math::router::withheld_obligation(&venue.router.obligation, idle)
    }
}

/// Draw the taker's size from every liquidity source, in one pass.
///
/// This layer governs liquidity. It quotes each source, splits the taker's
/// unfilled size across them, executes each allocation and settles what comes
/// back. It measures no risk of its own: the caller sets the limits it runs
/// inside.
///
/// Quote (sanitized DLOB makers as single-level books, external CPI books,
/// the vAMM ladder last with everything else as its last look), split the
/// taker's unfilled size across the union by priority tier, then execute and
/// settle each allocation through the fee-policy-keyed settle functions.
///
/// One quote/route pass rather than the route-then-quote-per-method loop this
/// replaced, which is why there is no scratch-AMM projection (routing and
/// quoting see the same curve), no separate JIT participant (the vAMM's
/// last-look shading is its general form), and no per-step fallback recompute
/// (one effective taker limit bounds every book up front).
///
/// External books are priced into the split; allocations that land on them
/// execute through `RouterFillInputs::executor` (the CPI leg the fill
/// entrypoint supplies) and settle per returned balance change against the
/// loaded makers. Maker prices are the sanitized frozen prices from
/// discovery; `DlobOrderQuoter::execute` requotes off the same
/// oracle/slot/tick so the two agree by construction, and
/// `settle_dlob_match_fill`'s `validate_fill_price` enforces it.
///
/// The steps run in the order they are written below. [`PerpFill`] carries
/// what they share.
pub(super) fn fill_from_liquidity_sources(
    taker: &mut TakerSide,
    policy: &FillPolicy,
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
        refresh_and_read_market(market.deref_mut(), oracle_map, taker, policy, conditions)?;

    let mut venue = ExternalVenue::new(liquidity.router);
    let mut fill = PerpFill::new(
        FillCounterparties {
            oracle_map,
            users: parties.makers_and_referrer,
            stats: parties.makers_and_referrer_stats,
        },
        &venue,
        policy,
        conditions,
        setup,
        taker.reborrow(),
    )?;

    // ---- Quote, split, and take the vAMM's share while the curve is held. ----
    let routed =
        fill.route_across_sources(&venue, &mut amm_quoter, liquidity.dlob_makers, target_size)?;

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
    policy: &FillPolicy,
    conditions: &FillConditions,
) -> VelocityResult<(AmmQuoter<'m>, FillMarketSetup)> {
    let oracle_price_data = *oracle_map.get_price_data(&market.oracle_id())?;
    let quote_inputs = QuoteInputs::load(
        market,
        oracle_price_data,
        conditions.slot,
        policy.validity_guard_rails,
        oracle_map.slot_clock,
    )?;
    let market_fee_adjustment = market.fee_adjustment;
    let mut amm_quoter = AmmQuoter::for_amm(&mut market.amm);
    let setup = FillMarketSetup::load(
        &mut amm_quoter,
        quote_inputs,
        taker,
        policy,
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
