//! Finding the DLOB maker orders one fill may match.
//!
//! Discovery walks every loaded maker, judges each of its resting orders
//! against the market and the taker, and returns what is matchable, best price
//! first. It also cleans up as it walks: a stale resting order is cancelled
//! here and the keeper earns the flat reward for it.

use super::*;

#[allow(clippy::type_complexity)]
/// One matchable maker order: which loaded maker holds it, where it sits in
/// that maker's orders, and the price it rests at.
///
/// The maker is its position in the loaded set rather than its key. A maker
/// contributes a row per order slot it holds, and a key on every row is
/// thirty-two bytes repeated — on a heap the runtime never reclaims, and a
/// fill against a full book carries dozens of rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MakerOrderInfo {
    pub maker: u16,
    pub order_index: u16,
    pub price: u64,
}

impl MakerOrderInfo {
    /// The key of the maker this row names, from the set the row indexes.
    pub fn key(&self, makers: &UserMap) -> VelocityResult<Pubkey> {
        makers
            .0
            .iter()
            .nth(self.maker as usize)
            .map(|(key, _)| *key)
            .ok_or(ErrorCode::UnableToLoadUserAccount)
    }

    pub fn slot(&self) -> usize {
        self.order_index as usize
    }
}

/// What maker discovery is looking for, and what it may admit.
pub(crate) struct MakerSearch<'a> {
    pub taker_key: &'a Pubkey,
    pub taker_order: &'a Order,
    /// Opposite the taker's, by construction.
    pub maker_direction: PositionDirection,
    /// The keeper that earns the flat reward for each stale order it cleans up.
    pub filler_key: &'a Pubkey,
    pub filler_reward: u64,
    pub oracle_price: i64,
    /// Whether the raw exchange oracle admits a match fill at all.
    pub exchange_match_fills_allowed: bool,
    pub now: i64,
    pub slot: u64,
}

/// One loaded maker, as the fill's maker map holds it.
struct LoadedMaker<'a, 'info> {
    /// The maker's position in the loaded map, which is how a discovered order
    /// names its owner.
    slot: u16,
    key: &'a Pubkey,
    loader: &'a AccountLoader<'info, User>,
}

/// The market facts discovery reads once per maker.
#[derive(Clone, Copy)]
struct MakerMarketFacts {
    initial_margin_ratio: u32,
    step_size: u64,
    /// A `ReduceOnly` market forces resting maker orders risk-reducing too,
    /// regardless of the flag they were placed with. Stamped onto each maker
    /// order so the reduce-only cancel check and the position-capped fill size
    /// both apply.
    reduce_only: bool,
}

/// One of a maker's resting orders, as discovery found it.
struct MakerCandidate<'a> {
    key: &'a Pubkey,
    index: usize,
    /// The sanitized price the order is frozen at for this fill.
    price: u64,
}

/// What discovery decided about one of a maker's resting orders.
enum MakerAdmission {
    /// The order is not matchable, or discovery cancelled it.
    Skip,
    /// The order is matchable. `unfilled` is what it still has to give, which
    /// is what the reducing-set judgement measures a floored maker's orders by.
    Matchable { unfilled: u64 },
}

/// A maker's resting orders on the side this fill needs, as
/// `(order index, sanitized price)`.
type MakerCandidates = Vec<(usize, u64)>;

/// Every DLOB maker order this fill may match, best price first.
///
/// Discovery also cleans up as it walks: a resting order whose price has left
/// the oracle band, or that expired, or that a reduce-only market turned
/// risk-increasing, is cancelled here and the keeper earns the flat reward for
/// it. That cleanup runs whether or not the order was going to be matchable.
pub(super) fn get_maker_orders_info(
    maps: &mut AccountMaps,
    makers_and_referrer: &UserMap,
    filler: &mut Option<&mut User>,
    search: &MakerSearch,
) -> VelocityResult<Vec<MakerOrderInfo>> {
    // One entry per matchable maker order. Sized so a full book of makers does
    // not grow the buffer part way through: a doubling abandons the old one on
    // an allocator that never reclaims.
    let mut maker_orders_info = Vec::with_capacity(
        makers_and_referrer.0.len() * crate::math::constants::MAX_OPEN_ORDERS as usize,
    );
    for (slot, (key, loader)) in makers_and_referrer.0.iter().enumerate() {
        if key == search.taker_key {
            continue;
        }
        collect_maker_orders(
            LoadedMaker {
                slot: slot as u16,
                key,
                loader,
            },
            &mut maker_orders_info,
            maps,
            filler,
            search,
        )?;
    }
    Ok(maker_orders_info)
}

/// Walk one maker's resting orders, cleaning up what is stale and admitting
/// what is matchable.
fn collect_maker_orders(
    maker: LoadedMaker,
    into: &mut Vec<MakerOrderInfo>,
    maps: &mut AccountMaps,
    filler: &mut Option<&mut User>,
    search: &MakerSearch,
) -> VelocityResult {
    let mut user = load_mut!(maker.loader)?;
    if user.is_being_liquidated() {
        return Ok(());
    }
    let Some((candidates, facts)) = open_maker_orders(&mut user, maker.key, maps, search)? else {
        return Ok(());
    };

    let maker_can_match =
        can_floored_user_match_with_exchange_oracle(&user, search.exchange_match_fills_allowed);
    let floor_unverifiable = maker_can_match && maker_floor_unverifiable(&user, maps)?;

    // Candidates of an unverifiable floored maker that survive the cleanup, as
    // (order index, price, unfilled base). The admit-or-prune decision is made
    // on the whole set afterwards, in `admit_reducing_maker_orders`. Sized to
    // the most a user can hold: growing inside the loop doubles the buffer,
    // and the runtime's allocator never reclaims the one it grew out of.
    let mut deferred: Vec<(usize, u64, u64)> =
        Vec::with_capacity(crate::math::constants::MAX_OPEN_ORDERS as usize);

    for (index, price) in candidates.iter() {
        let candidate = MakerCandidate {
            key: maker.key,
            index: *index,
            price: *price,
        };
        let MakerAdmission::Matchable { unfilled } =
            admit_maker_order(&mut user, &candidate, facts, maps, filler, search)?
        else {
            continue;
        };
        // A selected MM oracle may be fresh enough to quote while the raw
        // exchange oracle the equity floor reads is not valid for margin. The
        // cleanup above stays live, but a floored maker does not execute a
        // DLOB leg its floor check cannot cover.
        if !maker_can_match {
            continue;
        }
        if floor_unverifiable {
            deferred.push((*index, *price, unfilled));
            continue;
        }
        insert_maker_order_info(
            into,
            MakerOrderInfo {
                maker: maker.slot,
                order_index: *index as u16,
                price: *price,
            },
            search.maker_direction,
        );
    }

    if maker_can_match && floor_unverifiable {
        admit_deferred_maker_orders(&user, maker.slot, deferred, into, search)?;
    }
    Ok(())
}

/// Whether this maker's buffered floor cannot be verified for this fill.
///
/// A floored maker with any invalid oracle cannot prove it clears its buffered
/// floor, so the fill-time gate would reject its risk-increasing fills — and
/// by then the maker's leg has executed, so the rejection poisons the taker's
/// whole transaction. Oracle validity cannot change across the fill, so it is
/// resolved here instead: such a maker's risk-increasing orders are pruned,
/// and its provably reducing orders stay matchable because the gate exempts
/// them. Read once per maker, and free when no floor is set.
fn maker_floor_unverifiable(maker: &User, maps: &mut AccountMaps) -> VelocityResult<bool> {
    Ok(match calculate_net_equity_for_floor(maker, maps)? {
        Some(net_equity) => !net_equity.all_oracles_valid,
        None => false,
    })
}

/// The maker's orders that rest on the side this fill needs, and the market
/// facts every one of them is judged against.
///
/// `None` when the maker has nothing resting on that side, which is the
/// common case and the one worth leaving early for.
fn open_maker_orders(
    maker: &mut User,
    maker_key: &Pubkey,
    maps: &mut AccountMaps,
    search: &MakerSearch,
) -> VelocityResult<Option<(MakerCandidates, MakerMarketFacts)>> {
    let mut market = maps
        .perp_market_map
        .get_ref_mut(&search.taker_order.market_index)?;
    let candidates = find_maker_orders(
        maker,
        &search.maker_direction,
        &MarketType::Perp,
        search.taker_order.market_index,
        Some(search.oracle_price),
        search.slot,
        market.order_tick_size,
        maps.oracle_map.slot_clock,
    )?;
    if candidates.is_empty() {
        return Ok(None);
    }
    maker.update_last_active_slot(search.slot);
    settle_funding_payment(maker, maker_key, &mut market, search.now)?;
    let facts = MakerMarketFacts {
        initial_margin_ratio: market.margin_ratio_initial,
        step_size: market.order_step_size,
        reduce_only: market.is_reduce_only()?,
    };
    Ok(Some((candidates, facts)))
}

/// Decide what becomes of one of a maker's resting orders.
fn admit_maker_order(
    maker: &mut User,
    candidate: &MakerCandidate,
    facts: MakerMarketFacts,
    maps: &mut AccountMaps,
    filler: &mut Option<&mut User>,
    search: &MakerSearch,
) -> VelocityResult<MakerAdmission> {
    let order = &maker.orders[candidate.index];
    if !is_maker_for_taker(
        order,
        search.taker_order,
        search.slot,
        maps.oracle_map.slot_clock,
    )? || !are_orders_same_market_but_different_sides(order, search.taker_order)
    {
        return Ok(MakerAdmission::Skip);
    }
    let breaches_oracle_price_limits = limit_price_breaches_maker_oracle_price_bands(
        candidate.price,
        order.direction,
        search.oracle_price,
        facts.initial_margin_ratio,
    )?;
    if facts.reduce_only {
        maker.orders[candidate.index].reduce_only = true;
    }
    let expired = should_expire_order(&maker.orders[candidate.index], search.now)?;
    let existing_base_asset_amount = maker
        .get_perp_position(maker.orders[candidate.index].market_index)?
        .base_asset_amount;
    let increases_position = should_cancel_reduce_only_order(
        &maker.orders[candidate.index],
        existing_base_asset_amount,
        facts.step_size,
    )?;

    if breaches_oracle_price_limits || expired || increases_position {
        let explanation = if breaches_oracle_price_limits {
            OrderActionExplanation::OraclePriceBreachedLimitPrice
        } else if expired {
            OrderActionExplanation::OrderExpired
        } else {
            OrderActionExplanation::ReduceOnlyOrderIncreasedPosition
        };
        cancel_stale_maker_order(maker, candidate, explanation, maps, filler, search)?;
        return Ok(MakerAdmission::Skip);
    }

    Ok(MakerAdmission::Matchable {
        unfilled: maker.orders[candidate.index]
            .get_base_asset_amount_unfilled(Some(existing_base_asset_amount))?,
    })
}

/// Cancel one stale maker order and pay the keeper the flat cleanup reward.
fn cancel_stale_maker_order(
    maker: &mut User,
    candidate: &MakerCandidate,
    explanation: OrderActionExplanation,
    maps: &mut AccountMaps,
    filler: &mut Option<&mut User>,
    search: &MakerSearch,
) -> VelocityResult {
    let filler_reward = {
        let mut market = maps
            .perp_market_map
            .get_ref_mut(&maker.orders[candidate.index].market_index)?;
        pay_keeper_flat_reward_for_perps(
            maker,
            filler.as_deref_mut(),
            market.deref_mut(),
            search.filler_reward,
            search.slot,
        )?
    };
    cancel_order(
        candidate.index,
        maker,
        candidate.key,
        maps,
        search.now,
        search.slot,
        explanation,
        Some(search.filler_key),
        filler_reward,
        false,
    )
}

/// Admit the deferred orders of an unverifiable floored maker that are
/// reducing as a set.
///
/// Admission is deferred so the candidates are judged together: the reducing
/// budget then goes to the best-priced orders instead of the lowest order
/// slots.
fn admit_deferred_maker_orders(
    maker: &User,
    maker_slot: u16,
    deferred: Vec<(usize, u64, u64)>,
    into: &mut Vec<MakerOrderInfo>,
    search: &MakerSearch,
) -> VelocityResult {
    let resting_base_asset_amount = maker
        .get_perp_position(search.taker_order.market_index)
        .map(|position| position.base_asset_amount)
        .unwrap_or(0);
    for (index, price) in
        admit_reducing_maker_orders(deferred, search.maker_direction, resting_base_asset_amount)?
    {
        insert_maker_order_info(
            into,
            MakerOrderInfo {
                maker: maker_slot,
                order_index: index as u16,
                price,
            },
            search.maker_direction,
        );
    }
    Ok(())
}

/// The exchange oracle is the canonical valuation source for the equity floor.
/// An MM oracle may still quote the AMM, but it cannot authorize a floored user
/// to participate in a DLOB match while the exchange oracle is invalid for the
/// match/margin policy.
/// This rule only applies to DLOB matches. Existing AMM gates remain unchanged.
#[inline(always)]
pub(super) fn can_floored_user_match_with_exchange_oracle(
    user: &User,
    exchange_match_fills_allowed: bool,
) -> bool {
    user.equity_floor == 0 || exchange_match_fills_allowed
}

/// The subset of an unverifiable floored maker's candidate orders
/// `(order index, price, unfilled base)` that is reducing as a set against
/// the maker's resting position, judged best price for the taker first
/// (ascending for maker sells, descending for maker buys). Reducing is a
/// property of the admitted set, not of one order: a maker long 1 with two
/// resting sells of 0.75 has each order reducing against the resting
/// position while the pair flips it short, so each candidate is judged
/// against the position the previously admitted orders would leave behind.
/// Judging best price first spends that budget on the orders the taker
/// wants matched. Every admitted order's fill is exempt at the fill-time
/// floor gate (`is_order_position_reducing` is the shared predicate), so a
/// pruned maker can never revert the taker's transaction.
pub(super) fn admit_reducing_maker_orders(
    mut candidates: Vec<(usize, u64, u64)>,
    maker_direction: PositionDirection,
    resting_base_asset_amount: i64,
) -> VelocityResult<Vec<(usize, u64)>> {
    match maker_direction {
        PositionDirection::Long => candidates.sort_by_key(|c| std::cmp::Reverse(c.1)),
        PositionDirection::Short => candidates.sort_by_key(|a| a.1),
    }

    let mut projected_base_asset_amount = resting_base_asset_amount;
    let mut admitted = Vec::with_capacity(candidates.len());

    for (order_index, order_price, unfilled) in candidates {
        if !is_order_position_reducing(&maker_direction, unfilled, projected_base_asset_amount)? {
            continue;
        }

        // admitted, so the next candidate is judged against what this one
        // would leave behind
        let signed = match maker_direction {
            PositionDirection::Long => unfilled.cast::<i64>()?,
            PositionDirection::Short => -unfilled.cast::<i64>()?,
        };
        projected_base_asset_amount = projected_base_asset_amount.safe_add(signed)?;

        admitted.push((order_index, order_price));
    }

    Ok(admitted)
}

#[inline(always)]
pub(super) fn insert_maker_order_info(
    maker_orders_info: &mut Vec<MakerOrderInfo>,
    maker_order_info: MakerOrderInfo,
    direction: PositionDirection,
) {
    let price = maker_order_info.price;
    let index = match maker_orders_info.binary_search_by(|item| match direction {
        PositionDirection::Short => item.price.cmp(&price),
        PositionDirection::Long => price.cmp(&item.price),
    }) {
        Ok(index) => index,
        Err(index) => index,
    };

    if index < maker_orders_info.capacity() {
        maker_orders_info.insert(index, maker_order_info);
    }
}
