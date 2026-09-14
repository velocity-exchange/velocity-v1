//! Size every counterparty a route may settle against, before it is quoted,
//! so a quote never stands on liquidity the fill would refuse.
//!
//! # Two sizings, one question
//!
//! This module produces two numbers and they are easy to read as parallel
//! mechanisms. They are not. They are one question — how much can this fill
//! cost the account behind the depth — asked of two kinds of depth, and the
//! kind is decided by [`crate::state::prop_amm::QuoterType::depth_is_margin_reserved`], not by which
//! program is quoting.
//!
//! A book's depth is resting orders. Each was gated at placement and
//! reserved into its owner's `open_bids`/`open_asks`, so filling one does not
//! grow that owner's worst case: the base was priced in already. What a fill
//! costs them is the gap between their limit and the mark, which is a *quote*
//! figure. A book walks the orders of many owners, so it gets a *list* —
//! [`build_user_caps`], carried in `UserCapsV0`, spent by the book as it
//! walks.
//!
//! Every other quoter computes its depth when asked, and nothing was set
//! aside for it. A fill there grows its owner's worst case, so what bounds it
//! is initial margin on the base taken, which is a *base* figure. Such a
//! quoter fills from the single `user` on its registry slot, so it gets a
//! *scalar* — [`build_quoter_rooms`], carried in `QuoteArgsV0::self_base_room`.
//!
//! Both ride the wire to every quoter, because the wire is one shape. Each
//! quoter reads the one that describes its own depth; a book ignores the
//! scalar and an unreserved quoter ignores the list.
//!
//! The two are not independent of each other. A resting order already costs
//! its owner room in the *base* figure, because
//! `worst_case_liability_value` prices `base + open_bids` and
//! `base + open_asks`. The reverse does not hold: a fill a quoter has not
//! made yet is reserved nowhere, so a quote budget cannot see it. One owner
//! backing both a book presence and an unreserved quoter can therefore be
//! offered room twice in one fill, and the post-fill check refuses it. That
//! is the same shape the book-count divisor below guards against within the
//! book set, and it is not guarded across the two kinds.
//!
//! `fulfill_perp_order_post_checks` refuses a fill that leaves a maker short
//! of what it owes, and the refusal takes the whole transaction — the taker
//! and every other maker in it. Until something intervenes the next fill does
//! the same. Handing the book each maker's room turns that from a revert into
//! a skip, and a skip is mid-book: the depth *behind* a maker who is out of
//! room stays quoted and stays fillable.
//!
//! # A budget, not a base amount
//!
//! What a fill costs a maker is collateral, and converting that into a base
//! amount needs the price each order fills at. This module never reads a
//! book, so it does not have those prices — the quoter does. So velocity
//! sends what it can compute from the maker's own account and the quoter
//! spends it against the prices it fills at.
//!
//! The requirement itself barely moves. `worst_case_liability_value` prices
//! `base + open_bids` and `base + open_asks` at the oracle, so filling a
//! resting bid — base up, open bids down by the same amount — leaves that
//! side of the worst case where it was. A resting order was already
//! margin-reserved at placement, and the fill converts the reservation into
//! the position it stood for.
//!
//! What moves is collateral, and it moves with size. The maker pays their own
//! limit price and receives a position the margin walk values at the oracle,
//! so a fill of `f` at price `P` costs them `f * (P - O)`. That is unpriced
//! before the fill: the reservation assumed the order would fill at the
//! oracle. The budget is how much of that a maker can absorb — the smaller of
//! free collateral at the tier the fill judges by and equity above
//! `floor + buffer`.
//!
//! # Which price
//!
//! The live exchange oracle, and not a twap, a confidence bound or a strict
//! price. The budget exists to predict one thing — whether
//! `fulfill_perp_order_post_checks` accepts the fill — so it has to be
//! measured in that check's own terms. That check values a perp position at
//! `oracle_price_data.price`, straight from the oracle map, and
//! `calculate_net_equity_for_floor` values it the same way. Neither applies a
//! twap or a confidence band to it.
//!
//! Two nearby prices are deliberately not used. The strict quote price scales
//! the *requirement*, and a fill leaves the requirement where it was. The
//! settlement `expiry_price` replaces the oracle in the margin walk, but a
//! market in settlement refuses fills before this runs.
//!
//! A safer price would not be conservative here, it would be wrong in an
//! unknown direction: too low a mark understates what a resting ask costs its
//! owner and overstates what a bid costs, so the budget would be loose on one
//! side and tight on the other.
//!
//! Room is measured at the tier the fill will judge by, not the tier a
//! placement would. `select_margin_type_for_perp_maker` answers `Fill` for a
//! maker taking on risk, so that is what a budget is sized against; asking at
//! `Initial` would deny liquidity the fill would have accepted.
//!
//! The reducing direction is left unconstrained. It answers to maintenance
//! margin, a maker reducing is the action the protocol wants, and pricing it
//! would double the walks for the side that almost never refuses.
//!
//! One budget per maker, not one per side: a take sweeps one side of the
//! book, and that is the side the budget is for.
//!
//! # Who is worth a slot
//!
//! Eight budgets fit on the wire, so a maker who *cannot* be hurt by this
//! fill should not take one of them. That is decidable from aggregates alone.
//! The most base this maker can give up is the smaller of what it has resting
//! on the swept side and what the taker asked for, and the most one base can
//! cost it is the oracle price. So a budget above that product is a budget
//! this fill cannot reach, and the maker goes out unconstrained.
//!
//! The price bound is exact on the ask side, where a maker sells and the
//! worst it can sell at is zero. On the bid side it assumes no maker rests a
//! bid above twice the mark, which is an offer of free money that the book
//! would not keep for long. A maker who does defeats the test and reverts the
//! fill, which is what every maker did before budgets existed.
//!
//! # What is not priced
//!
//! A fill grows the *opposite* side's worst case, so a maker whose other side
//! is the binding one pays margin as well as the price gap. A two-sided maker
//! is the common case and is unaffected — filling a bid moves
//! `base + open_asks` toward zero, not away — and modelling the lopsided case
//! means reproducing the margin walk here, where it would drift away from the
//! real one.
//!
//! # What they are not
//!
//! Not a trust boundary. A book that ignores a budget leaves velocity exactly
//! where it stands without one — the post-fill checks still refuse the fill.
//! What honouring them buys is that the honest case stops reverting. The one
//! exception is exclusion, which also drops that maker from the permitted
//! subject set velocity derives from the book itself, and a response naming
//! someone outside that set is refused outright.

use {
    crate::{
        controller::position::{add_new_position, get_position_index, PositionDirection},
        instructions::{
            optional_accounts::AccountMaps,
            router::quoted_route::{route_slab, QuoteInputs},
        },
        math::{
            casting::Cast,
            constants::{BASE_PRECISION_U64, MARGIN_PRECISION},
            margin::{
                calculate_margin_requirement_and_total_collateral_and_liability_info,
                calculate_net_equity_for_floor, MarginRequirementType,
            },
            safe_math::SafeMath,
        },
        state::{
            margin_calculation::{MarginContext, MarginTypeConfig},
            prop_amm::{
                clob_slot_index, find_account, ClobSide, QuoterSlabExt, QuoterSlabV0,
                QuoterUserCapV0, QuoterUserCapsV0,
                MAX_CONSTRAINED_WIRE_USERS as USER_CAPS_CAPACITY, MAX_ROUTE_QUOTERS,
            },
            user::{MarketType, OrderStatus},
            user_map::{UserMap, UserStatsMap},
        },
    },
    anchor_lang::prelude::*,
};

/// Fraction of a budget held back, in bps, for the costs it does not model:
/// the maker's fee on the fill, funding that settles with it, and the
/// rounding in between. All are small against a price gap wide enough to bind
/// a budget, and spending less than the budget only ever caps lower.
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
    pub slot: u64,
    pub now: i64,
}

/// Budgets for every named maker this fill could put out of margin.
pub fn build_user_caps<'info>(
    slab: Option<&AccountLoader<'info, QuoterSlabV0>>,
    tail: &'info [AccountInfo<'info>],
    inputs: &QuoteInputs<'_>,
    ctx: &mut CapInputs<'_, 'info>,
) -> Result<(QuoterUserCapsV0, QuoterRooms)> {
    // The side a taker of this direction sweeps, which is the only side these
    // quoters will be asked for. The other stays unconstrained.
    let resting_side = inputs.direction.side();

    // One budget goes to every book in the route, so a maker resting on two of
    // them would be offered the same room twice and could take it on each. The
    // executes all run after every quote is taken, so a budget cannot be
    // decremented between them without the second book's execute disagreeing
    // with its own quote. Splitting by the count keeps the total inside it.
    // Zero books leaves every budget unbounded rather than ending the pass:
    // the unreserved half of a cap is still owed to whichever quoters are
    // consulted, and a route may carry those without a book at all.
    let books = clob_books_in_route(slab, tail)?;

    // Which loaded user each unreserved quoter settles for, and the slot that
    // has to be told. Read once here so the walk below prices a user's two
    // bounds together.
    let sized_quoters = unreserved_quoters(slab, tail)?;

    let mut rooms = QuoterRooms::NONE;
    let mut caps: Vec<QuoterUserCapV0> = Vec::with_capacity(USER_CAPS_CAPACITY);
    for (index, user_ref) in inputs.users.iter().enumerate() {
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
        // reserved for them. Unbounded when no book is consulted, because
        // nothing would spend it.
        let quote_cap = if books == 0 {
            u64::MAX
        } else {
            ctx.maker_budget(
                &key,
                inputs.market_index,
                resting_side,
                inputs.size,
                inputs.reference_price,
                books,
            )?
        };

        // The base cap, which two readers share, so it is the tighter of what
        // each needs. Both measure the same thing — base this user may give
        // up on the swept side — and in all but one shape only one of them
        // binds, because the other is unbounded.
        //
        // A book holds reduce-only orders to the position they may reduce.
        // `0` when the owner holds none, which fails such an order closed
        // rather than letting it grow a position it exists to shrink. A
        // reduce-only order rests only when its owner is capped here, so this
        // is always priced, even for a maker whose quote cap does not bind.
        let reduce_cover = ctx.maker_reduce_cover(&key, inputs.market_index, resting_side)?;
        // An unreserved quoter holds its own depth to what its account's
        // margin carries. Priced only for a user some consulted quoter
        // settles for: nobody else is reachable that way, and the walk is not
        // free.
        let quoter_room = match sized_quoters.slot_for(&key) {
            Some(slot) => {
                // A quoter quoting for the taker themselves is a self-trade,
                // so it has no room at all. Said here rather than trimmed
                // later, so the quoter can decline before it walks.
                let room = if key == *ctx.taker_key {
                    0
                } else {
                    ctx.quoter_base_room(&key, inputs.market_index, inputs.maker_direction())?
                };
                // The book's claim on this user is taken first, so what the
                // quoter is offered is what survives it.
                let room = room.saturating_sub(base_funded_by(
                    quote_cap,
                    inputs.reference_price,
                    inputs.margin_ratio_initial,
                )?);
                // Kept by slot as well: the quote step trims the ladder to
                // this and reaches it by slot, having no way to resolve a
                // user there.
                rooms.push(slot, room);
                room
            }
            None => u64::MAX,
        };
        let base_cap = reduce_cover.min(quoter_room);

        if quote_cap == u64::MAX && base_cap == u64::MAX {
            continue;
        }
        caps.push(QuoterUserCapV0 {
            index: index as u8,
            quote_cap,
            base_cap,
        });
    }
    Ok((QuoterUserCapsV0::from_caps(caps), rooms))
}

/// The base room of every custom quoter the route consults, by slab slot.
///
/// Small and copied rather than borrowed: a route consults at most
/// [`MAX_ROUTE_QUOTERS`] slots, and the quote step wants this after the
/// account maps it was built from are no longer in scope.
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

/// Size every counterparty this quote may stand on, both kinds.
///
/// Every named user gets one cap, carrying both bounds: the quote it may
/// lose to depth already reserved on a book, and the base it may take on from
/// depth that never was. Priced before the quote, so a quoter never publishes
/// depth this fill would refuse to settle against.
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
        inputs: QuoteInputs {
            caps,
            rooms,
            ..inputs
        },
        slab,
    })
}

/// A quote whose counterparties are priced, and the slab they were priced
/// from.
///
/// The slab rides along because the quote reads it next: finding it costs a
/// scan of the account tail, and doing that twice for one fill is the kind of
/// cost that hides in a helper.
pub struct SizedQuote<'a, 'info> {
    pub inputs: QuoteInputs<'a>,
    pub slab: Option<AccountLoader<'info, QuoterSlabV0>>,
}

/// Base that `quote` of collateral funds at a market's initial margin.
///
/// How much of an unreserved quoter's room a book's claim on the same user
/// takes away. The two claims are on one pool of collateral, and the book's
/// is senior: its depth was margin-reserved when the order was placed, where
/// the quoter's is computed on demand and reserved nowhere. That ordering is
/// also the routing tiers' own, where a book fills ahead of a custom quoter.
///
/// The conversion is the margin one, not the price-gap one a quote cap is
/// spent at. Whatever the book takes leaves the user's collateral, and what
/// leaving collateral costs the quoter is the base that collateral would have
/// carried.
///
/// Conservative twice over. It assumes the book spends the whole cap, which
/// it may not; and it prices at the market's own margin ratio, which is the
/// lowest a user can face, so the base it accounts for is the most that
/// collateral could have carried. `0` when the oracle or the ratio cannot
/// size it, leaving the room untouched — a fill whose oracle is not positive
/// has already failed elsewhere.
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
/// Only a quoter whose depth was never margin-reserved needs a base bound —
/// a book's makers are bounded by their budgets. The route consults a slot
/// only when its response account rides the tail, so a market whose slab
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

/// How many of the route's consulted quoters are CLOB books on this market.
///
/// A slot is consulted when its response account rides the tail — the same
/// rule the route uses.
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
    /// Quote this maker may lose filling on `resting_side`: `0` when the fill
    /// would refuse them outright, `u64::MAX` when this fill cannot reach far
    /// enough to matter.
    ///
    /// The cheap answers come first, and deliberately so. A margin walk leaves
    /// allocations on a heap that never reclaims, so every named user that can be
    /// answered without one has to be.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn maker_budget(
        &mut self,
        key: &Pubkey,
        market_index: u16,
        resting_side: ClobSide,
        taker_size: u64,
        reference_price: i64,
        books: u32,
    ) -> Result<u64> {
        let maker = self.makers_and_referrer.get_ref(key)?;

        // The most base this maker can give up, which bounds everything below. A
        // user with nothing on a book — a referrer, a maker who only quotes the
        // DLOB, a maker quoting the other way — cannot lose a cent to this fill,
        // and is answered before any walk is spent on it.
        let position = maker.get_perp_position(market_index).ok();
        let resting = clob_resting_base(&maker, market_index, resting_side)?.min(taker_size);
        if resting == 0 {
            return Ok(u64::MAX);
        }
        // And the most it can cost them, which is that base sold for nothing.
        let worst_loss = resting
            .cast::<i128>()?
            .safe_mul(reference_price.max(0).cast()?)?
            .safe_div(BASE_PRECISION_U64.cast()?)?;

        // The two that answer without pricing anything: an authority-wide latch
        // bars every subaccount from risk-increasing activity, and a floor the
        // program cannot verify cannot authorise one either.
        if self
            .makers_and_referrer_stats
            .get_ref(&maker.authority)
            .map(|stats| stats.is_equity_breaker_tripped())
            .unwrap_or(false)
        {
            return Ok(0);
        }
        // Equity above `floor + buffer` is the first budget. A floor that cannot
        // be verified, or one already breached, leaves no budget at all.
        let mut budget = i128::MAX;
        if let Some(net_equity) = calculate_net_equity_for_floor(&maker, self.maps)? {
            if !net_equity.all_oracles_valid || !net_equity.clears_buffered_floor(&maker) {
                return Ok(0);
            }
            if maker.equity_floor > 0 {
                budget = net_equity
                    .value
                    .safe_sub(maker.buffered_equity_floor().cast::<i128>()?)?;
            }
        }

        // Free collateral at the tier the fill will judge by is the second. A
        // maker who already fails it is skipped outright rather than sized: the
        // fill would have to *earn* them back through the floor, and a budget
        // that counted on that would be routing to an account the checks
        // currently refuse.
        let margin_type_config = if position.is_some_and(|position| position.is_isolated()) {
            MarginTypeConfig::IsolatedPositionOverride {
                market_index,
                margin_requirement_type: MarginRequirementType::Fill,
                default_isolated_margin_requirement_type: MarginRequirementType::Maintenance,
                cross_margin_requirement_type: MarginRequirementType::Maintenance,
            }
        } else {
            MarginTypeConfig::CrossMarginOverride {
                margin_requirement_type: MarginRequirementType::Fill,
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
    fn quoter_base_room(
        &mut self,
        quoter_user_key: &Pubkey,
        market_index: u16,
        maker_direction: PositionDirection,
    ) -> Result<u64> {
        let position_index = {
            let mut maker = self.makers_and_referrer.get_ref_mut(quoter_user_key)?;
            get_position_index(&maker.perp_positions, market_index)
                .or_else(|_| add_new_position(&mut maker.perp_positions, market_index))?
        };
        let maker = self.makers_and_referrer.get_ref(quoter_user_key)?;
        Ok(crate::math::orders::calculate_max_perp_order_size(
            &maker,
            position_index,
            market_index,
            maker_direction,
            self.maps,
        )?)
    }

    /// The most base the book may fill against this user's reduce-only orders on
    /// `resting_side`, or `u64::MAX` when the user holds none.
    ///
    /// A user with no reduce-only order resting stays uncapped, so a normal maker
    /// never spends one of the scarce cap slots. A user who does hold one is capped
    /// to the position those orders reduce: a reduce-only ask reduces a long, and a
    /// reduce-only bid reduces a short, so the cover is the position held in the
    /// reduce direction. It is `0` when the user holds none of that position, which
    /// is the whole guard: the book is position-blind, so without this a reduce-only
    /// order rested against a flat account would grow a position it exists to
    /// shrink. The cover reads from live position every call, so a position closed
    /// elsewhere shrinks the cover on the next fill.
    fn maker_reduce_cover(
        &mut self,
        key: &Pubkey,
        market_index: u16,
        resting_side: ClobSide,
    ) -> Result<u64> {
        let maker = self.makers_and_referrer.get_ref(key)?;
        let Ok(position) = maker.get_perp_position(market_index) else {
            return Ok(u64::MAX);
        };
        if !position.has_reduce_only_clob() {
            return Ok(u64::MAX);
        }
        let base = position.base_asset_amount;
        Ok(match resting_side {
            ClobSide::Ask => base.max(0).unsigned_abs(),
            ClobSide::Bid => base.min(0).unsigned_abs(),
        })
    }
}

/// Base this maker has resting on a CLOB book for `market_index`, on the side
/// the taker sweeps.
///
/// `open_bids` and `open_asks` reserve for every open order the maker has on
/// the market, whichever book it rests on. The DLOB's share of that is on the
/// account, in `orders`, so what is left over is on a CLOB. That remainder is
/// the only base a budget can ever be spent against.
///
/// Reading it here is what keeps the cost of this module down: a maker who
/// only quotes the DLOB is answered from its own account, and never costs a
/// margin walk on a heap that cannot give the memory back.
fn clob_resting_base(
    maker: &crate::state::user::User,
    market_index: u16,
    resting_side: ClobSide,
) -> Result<u64> {
    let Ok(position) = maker.get_perp_position(market_index) else {
        return Ok(0);
    };
    let (reserved, direction) = match resting_side {
        ClobSide::Bid => (position.open_bids.unsigned_abs(), PositionDirection::Long),
        ClobSide::Ask => (position.open_asks.unsigned_abs(), PositionDirection::Short),
    };
    let on_the_dlob = maker
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
    Ok(reserved.saturating_sub(on_the_dlob))
}

#[cfg(test)]
mod tests;

/// What a book's claim takes from an unreserved quoter sharing its user.
#[cfg(test)]
mod apportion_tests {
    use super::base_funded_by;

    const PRICE: i64 = 100 * crate::math::constants::PRICE_PRECISION_I64;
    /// Ten percent, which is `MARGIN_PRECISION / 10`.
    const RATIO: u32 = crate::math::constants::MARGIN_PRECISION / 10;
    const QUOTE: u64 = crate::math::constants::QUOTE_PRECISION_U64;
    const BASE: u64 = crate::math::constants::BASE_PRECISION_U64;

    #[test]
    fn collateral_converts_at_the_margin_ratio() {
        // 100 quote of collateral, a mark of 100, ten percent initial margin:
        // it carries ten base, so a book promised that much takes ten base
        // off the quoter sharing the account.
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
        // A fill whose oracle is not positive has already failed elsewhere;
        // this must not turn that into a silent zero.
        assert_eq!(base_funded_by(100 * QUOTE, 0, RATIO).unwrap(), 0);
        assert_eq!(base_funded_by(100 * QUOTE, -1, RATIO).unwrap(), 0);
        assert_eq!(base_funded_by(100 * QUOTE, PRICE, 0).unwrap(), 0);
    }
}
