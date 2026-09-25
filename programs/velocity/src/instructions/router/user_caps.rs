//! Size every counterparty a route may settle against, before it is quoted,
//! so a quote never stands on liquidity the fill would refuse.
//!
//! # Two sizings, one question
//!
//! This module produces two numbers, and they read as parallel mechanisms.
//! They are not parallel. They are one question asked of two kinds of depth:
//! how much can this fill cost the account behind the depth.
//! [`crate::state::prop_amm::QuoterType::depth_is_margin_reserved`] decides
//! the kind. The identity of the quoting program does not.
//!
//! A book's depth is resting orders. Each order was gated at placement and
//! reserved into its owner's `open_bids` or `open_asks`. Filling one does not
//! grow that owner's worst case, because the base was priced in already. What
//! a fill costs the owner is the gap between their limit and the mark, which
//! is a quote figure. A book walks the orders of many owners, so it gets a
//! list. [`build_user_caps`] builds the list, `UserCapsV0` carries it, and the
//! book spends it as it walks.
//!
//! Every other quoter computes its depth when asked, and nothing was set
//! aside for it. A fill there grows its owner's worst case, so the bound is
//! initial margin on the base taken, which is a base figure. Such a quoter
//! fills from the single `user` on its registry slot, so that user's row
//! carries the figure as [`UserCapV0::base_cap`].
//!
//! One walk prices both figures, because one user may back both kinds of
//! depth. [`build_user_caps`] writes them onto the same cap row. It also
//! returns the base figure a second time as [`QuoterRooms`], keyed by slab
//! slot. Velocity's own ladder trim reads that copy, and the copy survives
//! the eviction of a row from the wire list.
//!
//! The whole list rides the wire to every quoter, because the wire is one
//! shape. Each quoter reads the figure that describes its own depth. A book
//! reads `quote_cap` and an unreserved quoter reads `base_cap`.
//!
//! The two figures are not independent. A resting order already costs its
//! owner room in the base figure, because `worst_case_liability_value` prices
//! `base + open_bids` and `base + open_asks`. The reverse does not hold. A
//! fill a quoter has not made yet is reserved nowhere, so a quote budget
//! cannot see it. One owner that backs both a book presence and an unreserved
//! quoter can therefore be offered room twice in one fill, and the post-fill
//! check refuses that fill. The book-count divisor below guards the same
//! shape within the book set. Nothing guards it across the two kinds.
//!
//! `TakerRiskLimits::check_after_fill` refuses a fill that leaves a maker short
//! of what it owes. The refusal takes the whole transaction, which is the
//! taker and every other maker in it. The next fill does the same until
//! something changes. Handing the book each maker's room turns that revert
//! into a skip, and the skip happens mid-book. The depth behind a maker that
//! has no room stays quoted and stays fillable.
//!
//! # A budget, not a base amount
//!
//! What a fill costs a maker is collateral. Converting that into a base
//! amount needs the price each order fills at. This module never reads a
//! book, so it does not have those prices. The quoter has them. Velocity
//! sends what it can compute from the maker's own account, and the quoter
//! spends it against the prices it fills at.
//!
//! The requirement itself barely moves. `worst_case_liability_value` prices
//! `base + open_bids` and `base + open_asks` at the oracle. A fill of a
//! resting bid raises the base and lowers the open bids by the same amount,
//! so that side of the worst case stays where it was. A resting order was
//! already margin reserved at placement, and the fill converts the
//! reservation into the position it stood for.
//!
//! What moves is collateral, and it moves with size. The maker pays their own
//! limit price and receives a position the margin walk values at the oracle.
//! A fill of `f` at price `P` costs the maker `f * (P - O)`. That cost is
//! unpriced before the fill, because the reservation assumed the order would
//! fill at the oracle. The budget is how much of that cost a maker can
//! absorb. It is the smaller of free collateral at the tier the fill judges
//! by and equity above `floor + buffer`.
//!
//! # Which price
//!
//! The budget uses the live exchange oracle. It uses no twap, no confidence
//! bound, and no strict price. The budget predicts one thing, which is
//! whether `TakerRiskLimits::check_after_fill` accepts the fill. So it must be
//! measured in that check's own terms. That check values a perp position at
//! `oracle_price_data.price`, straight from the oracle map, and
//! `calculate_net_equity_for_floor` values it the same way. Neither applies a
//! twap or a confidence band to it.
//!
//! Two nearby prices are deliberately unused. The strict quote price scales
//! the requirement, and a fill leaves the requirement where it was. The
//! settlement `expiry_price` replaces the oracle in the margin walk, but a
//! market in settlement refuses fills before this code runs.
//!
//! A safer price would not be conservative here. It would be wrong in an
//! unknown direction. Too low a mark understates what a resting ask costs its
//! owner and overstates what a bid costs, so the budget would be loose on one
//! side and tight on the other.
//!
//! Room is measured at the tier the fill will judge by, not the tier a
//! placement would. `select_margin_type_for_perp_maker` answers `Fill` for a
//! maker that takes on risk, so a budget is sized against `Fill`. A budget
//! sized at `Initial` would deny liquidity the fill would have accepted.
//!
//! The reducing direction is left unconstrained. It answers to maintenance
//! margin, a maker that reduces is the action the protocol wants, and pricing
//! it would double the walks for the side that almost never refuses.
//!
//! One budget per maker, not one per side. A take sweeps one side of the
//! book, and that is the side the budget is for.
//!
//! # Who is worth a slot
//!
//! Eight budgets fit on the wire, so a maker this fill cannot hurt should not
//! take one of them. Aggregates alone decide that. The most base this maker
//! can give up is the smaller of what it has resting on the swept side and
//! what the taker asked for. The most one base can cost it is the oracle
//! price. A budget above that product is a budget this fill cannot reach, so
//! the maker goes out unconstrained.
//!
//! The price bound is exact on the ask side, where a maker sells and the
//! worst price it can sell at is zero. On the bid side it assumes no maker
//! rests a bid above twice the mark. Such a bid offers free money, and the
//! book would not keep it for long. A maker that rests one defeats the test
//! and reverts the fill, which is what every maker did before budgets
//! existed.
//!
//! # What is not priced
//!
//! A fill grows the opposite side's worst case, so a maker whose other side
//! is the binding one pays margin as well as the price gap. A two-sided maker
//! is the common case and is unaffected, because filling a bid moves
//! `base + open_asks` toward zero rather than away from it. To model the
//! lopsided case means to reproduce the margin walk here, where it would
//! drift away from the real one.
//!
//! # What they are not
//!
//! Budgets are not a trust boundary. A book that ignores a budget leaves
//! velocity where it stands without one, because the post-fill checks still
//! refuse the fill. What honouring a budget buys is that the honest case
//! stops reverting. Exclusion is the one exception. It also drops that maker
//! from the permitted subject set velocity derives from the book itself, and
//! a response that names someone outside that set is refused.

use {
    crate::{
        controller::position::PositionDirection,
        error::ErrorCode,
        instructions::{
            optional_accounts::AccountMaps,
            router::quoted_route::{route_slab, QuoteInputs},
        },
        math::{
            casting::Cast,
            constants::{BASE_PRECISION_U64, MARGIN_PRECISION},
            margin::{
                calculate_margin_requirement_and_total_collateral_and_liability_info,
                calculate_net_equity_for_floor, FloorNetEquity, MarginRequirementType,
            },
            safe_math::SafeMath,
        },
        state::{
            margin_calculation::{MarginContext, MarginTypeConfig},
            prop_amm::{
                clob_slot_index, find_account, QuoterSlabExt, QuoterSlabV0, SideV0, UserCapV0,
                UserCapsV0, MAX_ROUTE_QUOTERS, USER_CAPS_CAPACITY,
            },
            user::{MarketType, OrderStatus},
            user_map::{UserMap, UserStatsMap},
        },
    },
    anchor_lang::prelude::*,
};

/// Fraction of a budget held back, in bps, for the costs it does not model.
/// Those costs are the maker's fee on the fill, funding that settles with it,
/// and the rounding in between. All are small against a price gap wide enough
/// to bind a budget, and a budget spent short only ever caps lower.
const BUDGET_HAIRCUT_BPS: i128 = 1_000;
const BPS_DENOM: i128 = 10_000;

/// Everything sizing a maker needs that quoting does not.
pub struct CapInputs<'a, 'info> {
    pub makers_and_referrer: &'a UserMap<'info>,
    pub makers_and_referrer_stats: &'a UserStatsMap<'info>,
    pub maps: &'a mut AccountMaps<'info>,
    /// The taker's own `User`. A quoter settling for the taker is a
    /// self-trade and gets no room at all.
    pub taker_key: &'a Pubkey,
}

/// Budgets for every named maker this fill could put out of margin.
///
/// A quoter that settles for the taker is a self trade. It gets zero room, so the split
/// never allocates depth that `QuoterSubjects::permits` refuses, and that refusal fails the
/// whole fill. The zero is pushed before the walk, because `inputs.users` carries the loaded
/// makers and never the taker.
pub fn build_user_caps<'info>(
    slab: Option<&AccountLoader<'info, QuoterSlabV0>>,
    tail: &'info [AccountInfo<'info>],
    inputs: &QuoteInputs<'_>,
    ctx: &mut CapInputs<'_, 'info>,
) -> Result<(UserCapsV0, QuoterRooms)> {
    // The side a taker of this direction sweeps, which is the only side these
    // quoters will be asked for. The other side stays unconstrained.
    let resting_side = inputs.direction.side();

    // One budget goes to every book, so a maker resting on two could take it on each. Execute
    // always runs after every quote, leaving no chance to decrement a budget between them.
    // Division by the count keeps the total inside the budget. A consulted quoter with no book
    // still owes the unreserved half of a cap.
    let books = clob_books_in_route(slab, tail)?;

    // Which loaded user each unreserved quoter settles for, and the slot that
    // must be told. Read once here, so the walk below prices a user's two
    // bounds together.
    let sized_quoters = unreserved_quoters(slab, tail)?;

    let mut rooms = QuoterRooms::NONE;
    if let Some(slot) = sized_quoters.slot_for(ctx.taker_key) {
        rooms.push(slot, 0);
    }

    let mut caps: Vec<UserCapV0> = Vec::with_capacity(USER_CAPS_CAPACITY);
    for (index, user_ref) in inputs.users.iter().enumerate() {
        // The taker can be loaded as a maker too. It gets no budget of its own.
        if *user_ref == inputs.taker {
            continue;
        }

        let Some(key) = ctx
            .makers_and_referrer
            .0
            .iter()
            .find(|(_, loader)| {
                loader.load().is_ok_and(|maker| {
                    maker.authority == user_ref.authority
                        && maker.sub_account_id == user_ref.sub_account_id
                })
            })
            .map(|(key, _)| *key)
        else {
            continue;
        };

        // The quote cap: what this user may lose to depth a book already
        // reserved for them.
        let quote_cap = ctx.maker_budget(
            &key,
            inputs.market_index,
            resting_side,
            inputs.size,
            inputs.reference_price,
            books,
        )?;

        // The base cap is shared by two readers, so it is the tighter of what each needs. Usually only one
        // binds, since the other is unbounded. A book covers reduce-only orders to the position they may
        // reduce. That cover is `0` when the owner holds none of it. Without that cover, such an order could
        // grow a position it exists to shrink. So it is always priced, even when the quote cap does not bind.
        let reduce_cover = ctx.maker_reduce_cover(&key, inputs.market_index, resting_side)?;
        // An unreserved quoter holds its own depth to what its account's
        // margin carries. This is priced only for a user that some consulted
        // quoter settles for. Nobody else is reachable that way, and the walk
        // is not free.
        let quoter_room = match sized_quoters.slot_for(&key) {
            Some(slot) => {
                // The taker's own quoter slot is zeroed above, where the
                // taker is skipped. A user that reaches here is some other
                // account, so it is sized on its own margin.
                let room =
                    ctx.quoter_base_room(&key, inputs.market_index, inputs.maker_direction())?;
                // The book's claim on this user is taken first, so the quoter
                // is offered what survives it.
                let room = room.saturating_sub(base_funded_by(
                    quote_cap,
                    inputs.reference_price,
                    inputs.margin_ratio_initial,
                )?);

                // Kept by slot as well. The quote step trims the ladder to
                // this value and reaches it by slot, because it cannot resolve
                // a user there.
                rooms.push(slot, room);
                room
            }
            None => u64::MAX,
        };
        let base_cap = reduce_cover.min(quoter_room);

        if quote_cap == u64::MAX && base_cap == u64::MAX {
            continue;
        }

        caps.push(UserCapV0 {
            index: index as u8,
            quote_cap,
            base_cap,
        });
    }

    // Each index comes from one position of `inputs.users`, which the wire
    // already bounds, so a refusal here is a bug in this walk.
    let caps = UserCapsV0::from_caps(caps).map_err(|_| ErrorCode::TooManyQuoterWireUsers)?;
    Ok((caps, rooms))
}

/// The base room of every custom quoter the route consults, by slab slot.
/// Copied rather than borrowed, since a route consults at most
/// [`MAX_ROUTE_QUOTERS`] slots and the quote step wants this after the
/// account maps it was built from leave scope.
#[derive(Clone, Copy, Debug)]
pub struct QuoterRooms {
    entries: [(u16, u64); MAX_ROUTE_QUOTERS],
    len: u8,
}

impl Default for QuoterRooms {
    fn default() -> Self {
        Self::NONE
    }
}

impl QuoterRooms {
    /// No quoter is bounded. What a route with no custom quoter carries, and
    /// what a caller that does not size them passes.
    pub const NONE: Self = Self {
        entries: [(0, u64::MAX); MAX_ROUTE_QUOTERS],
        len: 0,
    };

    /// The base `slot_index`'s quoter may take on. `u64::MAX` when this route
    /// did not size it, which is every book and every unsized call.
    pub fn room(&self, slot_index: usize) -> u64 {
        self.entries[..self.len as usize]
            .iter()
            .find(|(index, _)| *index as usize == slot_index)
            .map(|(_, room)| *room)
            .unwrap_or(u64::MAX)
    }

    fn push(&mut self, slot_index: usize, room: u64) {
        if (self.len as usize) < MAX_ROUTE_QUOTERS {
            self.entries[self.len as usize] = (slot_index as u16, room);
            self.len += 1;
        }
    }
}

/// Size every counterparty this quote may stand on, of both kinds.
///
/// Every named user gets one cap that carries both bounds. The first is the
/// quote it may lose to depth a book already reserved. The second is the base
/// it may take on from depth that was never reserved. The caps are priced
/// before the quote, so a quoter never publishes depth this fill would refuse
/// to settle against.
pub fn with_counterparty_room<'a, 'info>(
    tail: &'info [AccountInfo<'info>],
    inputs: QuoteInputs<'a>,
    ctx: &mut CapInputs<'_, 'info>,
) -> Result<SizedQuote<'a, 'info>> {
    // Found once for everything that needs it. Locating the slab means a scan
    // of the account tail that borrows every account on it, and the quote
    // that follows reads the same one.
    let slab = route_slab(tail, inputs.market_index)?;
    let (caps, rooms) = build_user_caps(slab.as_ref(), tail, &inputs, ctx)?;
    Ok(SizedQuote {
        inputs,
        caps,
        rooms,
        slab,
    })
}

/// A quote whose counterparties are priced, and the slab they were priced
/// from. The slab rides along because the quote reads it next. Finding it
/// costs a scan of the account tail, and one fill must not pay that cost
/// twice.
pub struct SizedQuote<'a, 'info> {
    pub inputs: QuoteInputs<'a>,
    /// What each named user may lose, and how much base they may give up.
    /// Carried here rather than on the inputs, so the quote and the execute
    /// that binds to it cannot be given different numbers. Both read this one
    /// value.
    pub caps: UserCapsV0,
    /// The same `base_cap` these caps carry, indexed by slab slot rather than duplicated by chance.
    /// Resolving a slot's user into a cap index would mean deriving the user PDA per slot. No quote step
    /// can afford that. A cap can also be evicted from the wire list, but this copy keeps velocity's own
    /// ladder-trim bound intact.
    pub rooms: QuoterRooms,
    pub slab: Option<AccountLoader<'info, QuoterSlabV0>>,
}

/// Base that `quote` of collateral funds at a market's initial margin. A book's claim on the same user is
/// senior, and takes part of an unreserved quoter's room. The book's depth was reserved at placement, where
/// the quoter's depth is computed on demand and reserved nowhere. What leaving collateral costs the quoter
/// is the base that collateral would have carried. The result is conservative twice over. It assumes the
/// book spends the whole cap and prices at the market's lowest margin ratio. It returns `0` when the oracle
/// or the ratio cannot size it. A fill with a non-positive oracle has already failed elsewhere.
fn base_funded_by(quote: u64, oracle_price: i64, margin_ratio_initial: u32) -> Result<u64> {
    if quote == u64::MAX || oracle_price <= 0 || margin_ratio_initial == 0 {
        return Ok(0);
    }

    Ok(quote
        .cast::<u128>()?
        .safe_mul(BASE_PRECISION_U64.cast()?)?
        .safe_mul(MARGIN_PRECISION.cast()?)?
        .safe_div(oracle_price.cast::<u128>()?)?
        .safe_div(margin_ratio_initial.cast()?)?
        .min(u64::MAX.cast()?)
        .cast()?)
}

/// Which loaded user each unreserved quoter in the route settles for.
///
/// Only a quoter whose depth was never margin reserved needs a base bound,
/// because a book's makers are bounded by their budgets. The route consults a
/// slot only when its response account rides the tail, so a market whose slab
/// holds no live unreserved quoter never scans the tail at all.
fn unreserved_quoters<'info>(
    slab: Option<&AccountLoader<'info, QuoterSlabV0>>,
    tail: &'info [AccountInfo<'info>],
) -> Result<QuoterUsers> {
    let Some(slab) = slab else {
        return Ok(QuoterUsers::NONE);
    };
    let slots = slab.slots()?;
    Ok(crate::state::prop_amm::occupied_slots(&slots)
        .filter(|(_, slot)| {
            !slot.config.quoter_type.depth_is_margin_reserved()
                && slot.quotes()
                && find_account(tail, &slot.config.response_account).is_some()
        })
        .take(MAX_ROUTE_QUOTERS)
        .fold(QuoterUsers::NONE, |mut found, (index, slot)| {
            found.push(index, slot.config.user);
            found
        }))
}

/// The settlement user of each unreserved quoter the route consults.
#[derive(Clone, Copy)]
struct QuoterUsers {
    entries: [(u16, Pubkey); MAX_ROUTE_QUOTERS],
    len: u8,
}

impl QuoterUsers {
    const NONE: Self = Self {
        entries: [(0, Pubkey::new_from_array([0u8; 32])); MAX_ROUTE_QUOTERS],
        len: 0,
    };

    /// The slab slot this user quotes for, if any consulted quoter does.
    fn slot_for(&self, key: &Pubkey) -> Option<usize> {
        self.entries[..self.len as usize]
            .iter()
            .find(|(_, user)| user == key)
            .map(|(slot, _)| *slot as usize)
    }

    fn push(&mut self, slot: usize, user: Pubkey) {
        if (self.len as usize) < MAX_ROUTE_QUOTERS {
            self.entries[self.len as usize] = (slot as u16, user);
            self.len += 1;
        }
    }
}

/// Whether the route consults this market's CLOB book, as a count so it can
/// divide a budget. The market holds at most one book, so the answer is `0` or
/// `1`.
///
/// A slot is consulted when its response account rides the tail, which is the
/// same rule the route uses.
fn clob_books_in_route<'info>(
    slab: Option<&AccountLoader<'info, QuoterSlabV0>>,
    tail: &'info [AccountInfo<'info>],
) -> Result<u32> {
    let Some(loader) = slab else {
        return Ok(0);
    };

    {
        let slots = loader.slots()?;
        let consulted_book = clob_slot_index(&slots).is_some_and(|index| {
            find_account(tail, &slots[index].config.response_account).is_some()
        });

        Ok(consulted_book as u32)
    }
}

impl CapInputs<'_, '_> {
    /// Quote this maker may lose filling on `resting_side`. It is `0` when the
    /// fill would refuse them outright, and `u64::MAX` when no book is
    /// consulted or this fill cannot reach far enough to matter.
    ///
    /// The cheap answers come first, and that order is deliberate. A margin
    /// walk leaves allocations on a heap that never reclaims, so every named
    /// user that can be answered without one must be.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn maker_budget(
        &mut self,
        key: &Pubkey,
        market_index: u16,
        resting_side: SideV0,
        taker_size: u64,
        reference_price: i64,
        books: u32,
    ) -> Result<u64> {
        // No book spends a budget, and the divide by `books` below needs a non-zero count.
        if books == 0 {
            return Ok(u64::MAX);
        }

        let maker = self.makers_and_referrer.get_ref(key)?;

        // The most base this maker can be filled for, which bounds everything below. Some users rest
        // nothing on a book. Examples are a referrer, a maker that quotes only one side, and a maker that
        // quotes the other way. This fill cannot cost them anything, so they are answered before any walk
        // is spent on them.
        let position = maker.get_perp_position(market_index).ok();
        let resting = clob_resting_base(&maker, market_index, resting_side)?.min(taker_size);
        if resting == 0 {
            return Ok(u64::MAX);
        }

        if self.book_fill_barred(&maker, market_index, resting_side, resting)? {
            return Ok(0);
        }

        // And the most it can cost them. The maker can end up with nothing for that base, so the bound
        // is its whole value at reference.
        let worst_loss = resting
            .cast::<i128>()?
            .safe_mul(reference_price.max(0).cast()?)?
            .safe_div(BASE_PRECISION_U64.cast()?)?;

        // The tier this fill answers to, read from the same rule the fill
        // itself reads. A fill that only reduces is exempt from the gates
        // below, so a floored maker can still deleverage through the book.
        let signed_fill = match resting_side {
            SideV0::Ask => resting.cast::<i64>()?.safe_mul(-1)?,
            SideV0::Bid => resting.cast::<i64>()?,
        };
        let tier = crate::math::orders::maker_fill_tier(
            position.map_or(0, |position| position.base_asset_amount),
            signed_fill,
        )?;

        let gate = risk_gate(
            &maker,
            self.makers_and_referrer_stats,
            self.maps,
            tier.risk_increasing,
        )?;

        if gate.refuses_risk {
            return Ok(0);
        }

        // Equity above `floor + buffer` is the first budget.
        let mut budget = i128::MAX;
        if let Some(net_equity) = gate.floor_equity {
            if maker.equity_floor > 0 {
                budget = net_equity
                    .value
                    .safe_sub(maker.buffered_equity_floor().cast::<i128>()?)?;
            }
        }

        // Free collateral at the tier the fill will judge by is the second. A
        // maker that already fails it is skipped rather than sized. The fill
        // would have to earn them back through the floor, and a budget that
        // counted on that would route to an account the checks refuse now.
        let margin_type_config = if position.is_some_and(|position| position.is_isolated()) {
            MarginTypeConfig::IsolatedPositionOverride {
                market_index,
                margin_requirement_type: tier.requirement,
                default_isolated_margin_requirement_type: MarginRequirementType::Maintenance,
                cross_margin_requirement_type: MarginRequirementType::Maintenance,
            }
        } else {
            MarginTypeConfig::CrossMarginOverride {
                margin_requirement_type: tier.requirement,
                default_margin_requirement_type: MarginRequirementType::Maintenance,
            }
        };
        let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &maker,
            self.maps,
            MarginContext::standard_with_config(margin_type_config)
                .ignore_invalid_deposit_oracles(true),
        )?;

        if !calculation.meets_margin_requirement() {
            return Ok(0);
        }

        let free_collateral = if calculation.has_isolated_margin_calculation(market_index) {
            calculation.get_isolated_free_collateral(market_index)?
        } else {
            calculation.get_cross_free_collateral()?
        };

        budget = budget
            .min(free_collateral.cast::<i128>()?)
            .safe_mul(BPS_DENOM.safe_sub(BUDGET_HAIRCUT_BPS)?)?
            .safe_div(BPS_DENOM)?
            .safe_div(books.cast::<i128>()?)?;
        if budget <= 0 {
            return Ok(0);
        }

        // Worth a slot only if this fill can reach the budget at all.
        if budget >= worst_loss {
            return Ok(u64::MAX);
        }

        Ok(budget.cast()?)
    }

    /// The most base a custom quoter's own maker can take on.
    ///
    /// A custom quoter's depth is never margin-reserved, so the bound is what
    /// the account supports right now. The position the fill lands in is
    /// created first, because the margin walk sizes the order against the
    /// position it will settle into.
    ///
    /// The fill's own gates apply first, as [`Self::maker_budget`] applies
    /// them to a book maker. A user under liquidation gets no room. A tripped
    /// breaker, an unverifiable or breached floor, or a `ReduceOnly` market
    /// leaves only the room that reduces the position.
    fn quoter_base_room(
        &mut self,
        quoter_user_key: &Pubkey,
        market_index: u16,
        maker_direction: PositionDirection,
    ) -> Result<u64> {
        let maker = self.makers_and_referrer.get_ref(quoter_user_key)?;
        if maker.is_being_liquidated() {
            return Ok(0);
        }

        let room = crate::math::orders::max_perp_order_size_for_prospective_position(
            &maker,
            market_index,
            maker_direction,
            self.maps,
        )?;
        let reducing_only = self.market_is_reduce_only(market_index)?
            || risk_gate(&maker, self.makers_and_referrer_stats, self.maps, true)?.refuses_risk;
        if !reducing_only {
            return Ok(room);
        }

        let position_base = maker
            .get_perp_position(market_index)
            .map_or(0, |position| position.base_asset_amount);
        Ok(room.min(crate::math::orders::reduce_only_cover(
            position_base,
            maker_direction,
        )))
    }

    /// Whether the fill must take nothing of this maker's book depth.
    ///
    /// A maker under liquidation takes no fill. The book ignores `base_cap` on
    /// an ordinary order, so in a `ReduceOnly` market a maker whose ordinary
    /// orders could grow its position is excluded whole.
    fn book_fill_barred(
        &self,
        maker: &crate::state::user::User,
        market_index: u16,
        resting_side: SideV0,
        resting: u64,
    ) -> Result<bool> {
        if maker.is_being_liquidated() {
            return Ok(true);
        }

        Ok(self.market_is_reduce_only(market_index)?
            && rests_ordinary_clob_orders(maker, market_index)
            && resting > position_cover(maker, market_index, resting_side))
    }

    fn market_is_reduce_only(&self, market_index: u16) -> Result<bool> {
        Ok(self
            .maps
            .perp_market_map
            .get_ref(&market_index)?
            .is_reduce_only()?)
    }

    /// The most base the book may fill against this user's reduce-only orders on
    /// `resting_side`, or `u64::MAX` when the user holds none.
    /// A user with no resting reduce-only order stays uncapped, so a normal maker never spends
    /// one of the scarce cap slots. A user that holds one is capped to the position those
    /// orders reduce. A reduce-only ask reduces a long and a reduce-only bid reduces a short,
    /// so the cover is the position held in the reduce direction.
    /// The cover is `0` when the user holds none of that position, which is the whole guard.
    /// The book is position-blind, so without this cover a reduce-only order rested against a
    /// flat account would grow a position it exists to shrink. The cover reads live position on
    /// every call, so a position closed elsewhere shrinks the cover on the next fill.
    fn maker_reduce_cover(
        &mut self,
        key: &Pubkey,
        market_index: u16,
        resting_side: SideV0,
    ) -> Result<u64> {
        let maker = self.makers_and_referrer.get_ref(key)?;
        let Ok(position) = maker.get_perp_position(market_index) else {
            return Ok(u64::MAX);
        };

        if !position.has_reduce_only_clob() {
            return Ok(u64::MAX);
        }

        Ok(crate::math::orders::reduce_only_cover(
            position.base_asset_amount,
            resting_side_fill(resting_side),
        ))
    }
}

/// What the fill's equity gates say about a maker, read before any budget is
/// priced.
struct RiskGate {
    /// A tripped breaker, or a floor the program cannot verify or that is
    /// breached, refuses a risk-increasing fill.
    refuses_risk: bool,
    /// The floor equity the gate read, so a caller that prices a budget does
    /// not walk it twice. `None` when the maker sets no floor, or when the
    /// breaker answered first.
    floor_equity: Option<FloorNetEquity>,
}

/// The equity gates `TakerRiskLimits::check_maker` holds a maker to.
///
/// A fill that only reduces is exempt, so a floored maker can still deleverage.
/// The breaker is an authority-wide latch and costs no walk, so it answers
/// first.
fn risk_gate(
    maker: &crate::state::user::User,
    stats: &UserStatsMap,
    maps: &mut AccountMaps,
    risk_increasing: bool,
) -> Result<RiskGate> {
    let breaker_tripped = stats
        .get_ref(&maker.authority)
        .map(|stats| stats.is_equity_breaker_tripped())
        .unwrap_or(false);
    if risk_increasing && breaker_tripped {
        return Ok(RiskGate {
            refuses_risk: true,
            floor_equity: None,
        });
    }

    let floor_equity = calculate_net_equity_for_floor(maker, maps)?;
    let floor_refuses = floor_equity.as_ref().is_some_and(|net_equity| {
        !net_equity.all_oracles_valid || !net_equity.clears_buffered_floor(maker)
    });

    Ok(RiskGate {
        refuses_risk: risk_increasing && floor_refuses,
        floor_equity,
    })
}

/// Whether this maker rests an order on the market's book that is not
/// reduce-only.
fn rests_ordinary_clob_orders(maker: &crate::state::user::User, market_index: u16) -> bool {
    let reduce_only_orders = maker
        .get_perp_position(market_index)
        .map_or(0, |position| position.reduce_only_clob_orders);
    u16::from(maker.clob_resident_open_orders(market_index)) > reduce_only_orders
}

/// The base a fill of the orders on `resting_side` can take before it grows
/// the maker's position.
fn position_cover(
    maker: &crate::state::user::User,
    market_index: u16,
    resting_side: SideV0,
) -> u64 {
    let position_base = maker
        .get_perp_position(market_index)
        .map_or(0, |position| position.base_asset_amount);
    crate::math::orders::reduce_only_cover(position_base, resting_side_fill(resting_side))
}

/// The direction a maker fills in when an order on `resting_side` fills. An
/// ask sells, so filling it is a short fill.
fn resting_side_fill(resting_side: SideV0) -> PositionDirection {
    match resting_side {
        SideV0::Ask => PositionDirection::Short,
        SideV0::Bid => PositionDirection::Long,
    }
}

/// Base this maker has resting on a CLOB book for `market_index`, on the side
/// the taker sweeps.
///
/// `open_bids` and `open_asks` reserve for every open order the maker has on
/// the market, wherever it rests. An order still in a `User.orders` slot holds
/// its share on the account, in `orders`, so what is left is on a CLOB. That
/// remainder is the only base a budget can be spent against.
///
/// Reading it here keeps the cost of this module down. A maker whose
/// reservations are all in slots is answered from its own account, and never
/// costs a margin walk on a heap that cannot give the memory back.
fn clob_resting_base(
    maker: &crate::state::user::User,
    market_index: u16,
    resting_side: SideV0,
) -> Result<u64> {
    let Ok(position) = maker.get_perp_position(market_index) else {
        return Ok(0);
    };
    let (reserved, direction) = match resting_side {
        SideV0::Bid => (position.open_bids.unsigned_abs(), PositionDirection::Long),
        SideV0::Ask => (position.open_asks.unsigned_abs(), PositionDirection::Short),
    };
    let in_slots = maker
        .orders
        .iter()
        .filter(|order| {
            order.status == OrderStatus::Open
                && order.market_type == MarketType::Perp
                && order.market_index == market_index
                && order.direction == direction
                && order.update_open_bids_and_asks()
        })
        .try_fold(0u64, |total, order| {
            total.safe_add(
                order
                    .base_asset_amount
                    .saturating_sub(order.base_asset_amount_filled),
            )
        })?;
    Ok(reserved.saturating_sub(in_slots))
}

#[cfg(test)]
mod tests;

/// What a book's claim takes from an unreserved quoter that shares its user.
#[cfg(test)]
mod apportion_tests {
    use super::base_funded_by;

    const PRICE: i64 = 100 * crate::math::constants::PRICE_PRECISION_I64;
    /// Ten percent.
    const RATIO: u32 = crate::math::constants::MARGIN_PRECISION / 10;
    const QUOTE: u64 = crate::math::constants::QUOTE_PRECISION_U64;
    const BASE: u64 = crate::math::constants::BASE_PRECISION_U64;

    #[test]
    fn collateral_converts_at_the_margin_ratio() {
        // 100 quote of collateral, a mark of 100, and ten percent initial
        // margin. That collateral carries ten base, so a book promised the
        // whole cap takes ten base from the quoter that shares the account.
        assert_eq!(
            base_funded_by(100 * QUOTE, PRICE, RATIO).unwrap(),
            10 * BASE
        );
    }

    #[test]
    fn a_tighter_margin_ratio_accounts_for_less_base() {
        // The same collateral carries less base when each unit costs more
        // margin, so a book's claim costs the quoter less of its room.
        let tight = base_funded_by(100 * QUOTE, PRICE, RATIO * 2).unwrap();
        assert_eq!(tight, 5 * BASE);
    }

    #[test]
    fn an_unbounded_book_claim_takes_nothing() {
        // No book reached this user, so there is no senior claim to yield to.
        assert_eq!(base_funded_by(u64::MAX, PRICE, RATIO).unwrap(), 0);
    }

    #[test]
    fn an_unusable_mark_leaves_the_room_alone() {
        // A fill whose oracle is not positive has already failed elsewhere.
        // Returning `0` leaves the quoter's room untouched rather than
        // guessing at the book's claim.
        assert_eq!(base_funded_by(100 * QUOTE, 0, RATIO).unwrap(), 0);
        assert_eq!(base_funded_by(100 * QUOTE, -1, RATIO).unwrap(), 0);
        assert_eq!(base_funded_by(100 * QUOTE, PRICE, 0).unwrap(), 0);
    }
}
