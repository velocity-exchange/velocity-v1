//! Size every maker the caller has loaded before the route's books are
//! quoted, so a quote never stands on liquidity the fill would refuse.
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
        instructions::{optional_accounts::AccountMaps, router::quoted_route::QuoteInputs},
        math::{
            casting::Cast,
            constants::BASE_PRECISION_U64,
            margin::{
                calculate_margin_requirement_and_total_collateral_and_liability_info,
                calculate_net_equity_for_floor, MarginRequirementType,
            },
            safe_math::SafeMath,
        },
        state::{
            margin_calculation::{MarginContext, MarginTypeConfig},
            prop_amm::{
                clob_slot_index, find_account, ClobSide, QuoterSlabExt, QuoterSlabV0, QuoterType,
                QuoterUserCapV0, QuoterUserCapsV0,
                MAX_CONSTRAINED_WIRE_USERS as USER_CAPS_CAPACITY, MAX_ROUTE_QUOTERS,
            },
            user::{MarketType, OrderStatus},
            user_map::{UserMap, UserStatsMap},
        },
    },
    anchor_lang::{prelude::*, Discriminator},
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
    pub slot: u64,
    pub now: i64,
}

/// Budgets for every named maker this fill could put out of margin.
pub fn build_user_caps<'info>(
    slab: Option<&AccountLoader<'info, QuoterSlabV0>>,
    tail: &'info [AccountInfo<'info>],
    inputs: &QuoteInputs<'_>,
    ctx: &mut CapInputs<'_, 'info>,
) -> Result<QuoterUserCapsV0> {
    // The side a taker of this direction sweeps, which is the only side these
    // books will be asked for. The other stays unconstrained.
    let resting_side = inputs.direction.side();

    // One budget goes to every book in the route, so a maker resting on two of
    // them would be offered the same room twice and could take it on each. The
    // executes all run after every quote is taken, so a budget cannot be
    // decremented between them without the second book's execute disagreeing
    // with its own quote. Splitting by the count keeps the total inside it.
    // Nothing else on a route reads a budget, so a route without a book has
    // no reason to price one. This is not only a saving: a margin walk leaves
    // allocations on a heap that never reclaims, and there are enough named
    // users on a busy fill to exhaust it.
    let books = clob_books_in_route(slab, tail)?;
    if books == 0 {
        return Ok(QuoterUserCapsV0::EMPTY);
    }

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
        let budget = ctx.maker_budget(
            &key,
            inputs.market_index,
            resting_side,
            inputs.size,
            inputs.reference_price,
            books,
        )?;
        // The most base the book may fill against this user's reduce-only
        // orders on the swept side: the position they may reduce. `0` when they
        // hold none, which fails a reduce-only order closed rather than letting
        // it grow a position it should shrink. A reduce-only order rests only
        // when the owner is capped here, so this is always computed, even for a
        // maker whose quote budget does not bind.
        let base_cover = ctx.maker_reduce_cover(&key, inputs.market_index, resting_side)?;
        if budget == u64::MAX && base_cover == u64::MAX {
            continue;
        }
        caps.push(QuoterUserCapV0 {
            index: index as u8,
            budget,
            base_cover,
        });
    }
    Ok(QuoterUserCapsV0::from_caps(caps))
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
/// A book is told each loaded maker's quote budget; a custom quoter is told
/// the base its own account carries. Both are priced here, before the quote,
/// so a quoter never publishes depth this fill would refuse to settle
/// against — and both come from the same pass over the same state, so the
/// two venue kinds cannot be sized against different facts.
pub fn with_counterparty_room<'a, 'info>(
    tail: &'info [AccountInfo<'info>],
    taker_key: &Pubkey,
    inputs: QuoteInputs<'a>,
    ctx: &mut CapInputs<'_, 'info>,
) -> Result<QuoteInputs<'a>> {
    // Found once. Both halves read the same slab, and locating it means a
    // scan of the tail that borrows every account on it.
    let slab = route_slab(tail, inputs.market_index)?;
    let caps = build_user_caps(slab.as_ref(), tail, &inputs, ctx)?;
    let rooms = build_quoter_rooms(
        slab.as_ref(),
        tail,
        inputs.maker_direction(),
        taker_key,
        ctx,
    )?;
    Ok(QuoteInputs {
        caps,
        rooms,
        ..inputs
    })
}

/// The base each custom quoter in the route may take on, from its own margin.
///
/// A book needs none of this. Its makers rest depth that was margin-reserved
/// at placement, so what a fill costs them is the price gap, which
/// [`build_user_caps`] prices per user. A custom quoter reserves nothing: the
/// fill creates the position from scratch, so what bounds it is initial
/// margin on the base it takes, and that is what this measures.
///
/// Runs beside the caps and before the quote, so the number velocity trims
/// the returned ladder to is the same number the quoter was told to size
/// itself against.
pub fn build_quoter_rooms<'info>(
    slab: Option<&AccountLoader<'info, QuoterSlabV0>>,
    tail: &'info [AccountInfo<'info>],
    maker_direction: PositionDirection,
    taker_key: &Pubkey,
    ctx: &mut CapInputs<'_, 'info>,
) -> Result<QuoterRooms> {
    let Some(slab) = slab else {
        return Ok(QuoterRooms::NONE);
    };
    // Which slots need a walk, read in one borrow of the slab. Only a custom
    // quoter does: a book's makers are sized by `build_user_caps`, and the
    // route consults a slot only when its response account rides the tail.
    // A market whose slab holds no live custom quoter — the common one —
    // never scans the tail at all.
    let (market_index, sized) = {
        let market_index = slab.load()?.market;
        let slots = slab.slots()?;
        let sized: Vec<(usize, Pubkey)> = crate::state::prop_amm::occupied_slots(&slots)
            .filter(|(_, slot)| {
                slot.config.quoter_type == QuoterType::Custom
                    && slot.quotes()
                    && find_account(tail, &slot.config.response_account).is_some()
            })
            .map(|(index, slot)| (index, slot.config.user))
            .take(MAX_ROUTE_QUOTERS)
            .collect();
        (market_index, sized)
    };

    sized
        .into_iter()
        .try_fold(QuoterRooms::NONE, |mut rooms, (index, quoter_user)| {
            // A quoter quoting for the taker themselves is a self-trade, so
            // it has no room at all. Said here rather than trimmed later, so
            // the quoter can decline before it spends a walk of its own.
            let room = if quoter_user == *taker_key {
                0
            } else {
                ctx.quoter_base_room(&quoter_user, market_index, maker_direction)?
            };
            rooms.push(index, room);
            Ok(rooms)
        })
}

/// The market's slab, when the transaction carries one.
///
/// Reads the market's slab, which velocity owns, and never the book arenas
/// its slots point at.
fn route_slab<'info>(
    tail: &'info [AccountInfo<'info>],
    market_index: u16,
) -> Result<Option<AccountLoader<'info, QuoterSlabV0>>> {
    for info in tail {
        let is_slab = info.owner == &crate::ID
            && info
                .try_borrow_data()
                .is_ok_and(|data| data.get(..8) == Some(QuoterSlabV0::DISCRIMINATOR));
        if !is_slab {
            continue;
        }
        let loader = AccountLoader::<QuoterSlabV0>::try_from(info)?;
        if loader.load()?.market != market_index {
            continue;
        }
        return Ok(Some(loader));
    }
    Ok(None)
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
