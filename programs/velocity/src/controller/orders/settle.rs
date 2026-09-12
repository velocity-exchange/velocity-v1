//! Settling one filled allocation.
//!
//! A fill is a set of allocations, and each one settles through this module:
//! the vAMM house, a DLOB match, or an external quoter match. Each leg prices
//! on its own fee schedule, then the three walk one spine — accrue the
//! market's share, charge the taker, pay the maker and the keeper, accrue the
//! revenue share, and advance the taker's order.
//!
//! The seat types are the vocabulary of that spine: who takes, who makes, and
//! who fills.

use super::*;

#[inline(always)]
fn get_builder_escrow_info(
    escrow_opt: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
    sub_account_id: u16,
    order_id: u32,
    market_index: u16,
    order_has_builder: bool,
    builder_fee_allowed: bool,
) -> (Option<u32>, Option<u32>, Option<u16>, Option<u8>) {
    if let Some(escrow) = escrow_opt {
        // Only match a builder-order row for an order that actually carries the
        // `HasBuilder` flag, and bind the row to the market being filled. Escrow rows
        // are keyed on chain by `(sub_account_id, order_id)`, and order ids are reused
        // both within a market (a placement soft-skips after `add_builder_order` wrote
        // the row — e.g. an expired `max_ts`, which returns before `next_order_id` is
        // consumed) and across markets (ids are per-subaccount). Without the
        // `HasBuilder` gate a stale row would attach to a later non-builder order that
        // reuses the id (OtterSec #49); without the market binding a market-A row would
        // attach to a same-id market-B fill and be paid from market A's pnl pool
        // (OtterSec #88). `find_builder_order_index` enforces both. The referral lookup
        // is keyed by market, not order id, so it is unaffected and stays unconditional.
        let builder_order_idx = if order_has_builder {
            escrow.find_builder_order_index(
                sub_account_id,
                order_id,
                market_index,
                MarketType::Perp,
            )
        } else {
            None
        };
        let referrer_builder_order_idx = escrow.find_or_create_referral_index(market_index);

        let builder_order = builder_order_idx.and_then(|idx| escrow.get_order(idx).ok());
        // `builder_fee_allowed` is false when the taker does not meet initial
        // margin. The row stays bound so the fill still reports its builder in
        // the `OrderActionRecord` and `revoke_completed_orders` still closes
        // the row, but the fee for this fill is zero. See the gate in
        // `fulfill_perp_order` for why (OtterSec #83).
        let builder_order_fee_bps = if builder_fee_allowed {
            builder_order.map(|order| order.fee_tenth_bps)
        } else {
            None
        };
        let builder_idx = builder_order.map(|order| order.builder_idx);

        (
            builder_order_idx,
            referrer_builder_order_idx,
            builder_order_fee_bps,
            builder_idx,
        )
    } else {
        (None, None, None, None)
    }
}

/// Build and emit an `OrderActionRecord`.
///
/// The record is 480 bytes, and the two `Option<Order>` copies are large
/// again. This function is separate so that all three live in its own frame.
/// A settlement path that built them would hold them in a frame that is
/// already large, and the SBPF stack-overwrite check then fires.
///
/// The long parameter list is what makes that work. Do not group these
/// parameters into a struct. The caller builds a struct in its own frame,
/// which is the cost this function exists to avoid. The check runs only on
/// an SBF target, so a host test reports nothing when it regresses.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn emit_perp_action_record(
    market: &mut PerpMarket,
    oracle_map: &mut OracleMap,
    now: i64,
    action_explanation: OrderActionExplanation,
    filler_key: &Pubkey,
    filler_reward: u64,
    base_filled: u64,
    quote_filled: u64,
    taker_fee_plus_builder: u64,
    maker_rebate: Option<u64>,
    referrer_reward: u64,
    quote_asset_amount_surplus: Option<i64>,
    taker_record_key: Option<Pubkey>,
    taker_record_order: Option<Order>,
    maker_record_key: Option<Pubkey>,
    maker_record_order: Option<Order>,
    order_action_bit_flags: u8,
    taker_existing_quote_entry_amount: Option<u64>,
    taker_existing_base_asset_amount: Option<u64>,
    maker_existing_quote_entry_amount: Option<u64>,
    maker_existing_base_asset_amount: Option<u64>,
    builder_idx: Option<u8>,
    builder_fee_option: Option<u64>,
) -> VelocityResult {
    let fill_record_id = get_then_update_id!(market, next_fill_record_id);
    let oracle_price = oracle_map.get_price_data(&market.oracle_id())?.price;
    let mut record = get_order_action_record(
        now,
        OrderAction::Fill,
        action_explanation,
        market.market_index,
        Some(*filler_key),
        Some(fill_record_id),
        Some(filler_reward),
        Some(base_filled),
        Some(quote_filled),
        Some(taker_fee_plus_builder),
        maker_rebate,
        Some(referrer_reward),
        quote_asset_amount_surplus,
        None,
        taker_record_key,
        taker_record_order,
        maker_record_key,
        maker_record_order,
        oracle_price,
        order_action_bit_flags,
        taker_existing_quote_entry_amount,
        taker_existing_base_asset_amount,
        maker_existing_quote_entry_amount,
        maker_existing_base_asset_amount,
        None,
        builder_idx,
        builder_fee_option,
    )?;
    // A maker whose order rests on a book has no `Order` here to snapshot.
    // What the fill knows of it is its id and its side, which is what a
    // reader needs to attribute the fill; the order's size and its running
    // totals live in the reader's own table, built from the place record.
    // Reporting this fill's size as the order's size would be wrong, so the
    // fields say nothing instead.
    if maker_record_order.is_some_and(|order| order.is_placed_on_clob()) {
        record.maker_order_base_asset_amount = None;
        record.maker_order_cumulative_base_asset_amount_filled = None;
        record.maker_order_cumulative_quote_asset_amount_filled = None;
    }
    emit_stack::<_, { OrderActionRecord::SIZE }>(record)
}

/// Quote and AMM surplus for a normal (non-post_only) sole-AMM fill.
///
/// Returns `(taker_quote, taker_surplus)`. `taker_quote` is what the taker
/// pays or receives. `taker_surplus` is the AMM's spread profit booked for
/// the LPs, signed so a positive value grows the AMM's books.
///
/// Two adjustments to the live-curve quote, both booked through the surplus:
///
///  * Capture the shade. The router quoted this slice at the shaded
///    allocation quote, which is taker-worse than the live curve when a rival
///    rung undercut the curve. Charge the taker that quote and book the gap
///    for the LPs, so the shade does not leak to the taker as price
///    improvement.
///  * Hold the taker to its limit. The ladder's `top` understates the swap's
///    first marginal, so a limit just above `top` can still sit below the
///    curve. Cap the taker's quote at its limit and book the improvement
///    against the surplus, so the taker is never charged worse than its
///    limit.
pub(super) fn settle_amm_house_normal_quote(
    fill: &QuoterFill,
    taker_direction: PositionDirection,
    taker_limit_price: Option<u64>,
    amm_allocation_quote: u64,
    amm_allocation_base: u64,
) -> VelocityResult<(u64, i64)> {
    let curve_quote = fill.quote_filled;
    let mut taker_quote = curve_quote;

    // Shade: charge the shaded allocation quote when it is taker-worse than
    // the curve. Scale it to the base actually filled, taker-worse, so a
    // partial fill is not overcharged the whole allocation.
    if amm_allocation_base > 0 && fill.base_filled > 0 {
        let shade_quote = if fill.base_filled >= amm_allocation_base {
            amm_allocation_quote
        } else {
            let scaled = (amm_allocation_quote as u128).safe_mul(fill.base_filled as u128)?;
            match taker_direction {
                PositionDirection::Long => scaled.safe_div_ceil(amm_allocation_base as u128)?,
                PositionDirection::Short => scaled.safe_div(amm_allocation_base as u128)?,
            }
            .cast::<u64>()?
        };
        taker_quote = match taker_direction {
            PositionDirection::Long => taker_quote.max(shade_quote),
            PositionDirection::Short => taker_quote.min(shade_quote),
        };
    }

    // Limit cap: never charge worse than the taker's own limit.
    if let Some(limit) = taker_limit_price {
        let limit_quote = crate::math::orders::calculate_quote_asset_amount_for_maker_order(
            fill.base_filled,
            limit,
            crate::math::constants::PERP_DECIMALS,
            taker_direction,
        )?;
        taker_quote = match taker_direction {
            PositionDirection::Long => taker_quote.min(limit_quote),
            PositionDirection::Short => taker_quote.max(limit_quote),
        };
    }

    // Book the change against the AMM's spread surplus. Positive when the
    // shade earned more than the curve. Negative when the limit cap gave the
    // taker improvement.
    let delta = match taker_direction {
        PositionDirection::Long => taker_quote.cast::<i64>()?.safe_sub(curve_quote.cast()?)?,
        PositionDirection::Short => curve_quote.cast::<i64>()?.safe_sub(taker_quote.cast()?)?,
    };
    let taker_surplus = fill.quote_asset_amount_surplus.safe_add(delta)?;
    Ok((taker_quote, taker_surplus))
}

/// The taker side of one fill: who fills, the order they fill through, and
/// the position it lands in.
pub(crate) struct TakerSide<'a> {
    pub user: &'a mut User,
    pub stats: &'a mut UserStats,
    pub key: Pubkey,
    pub position_index: usize,
    pub order: &'a mut Order,
    pub direction: PositionDirection,
    /// Base and quote of the position before this fill, when the caller
    /// already read them.
    pub existing_position_params_before: Option<(u64, u64)>,
    /// Whether the taker owns an `open_bids`/`open_asks` + `open_orders`
    /// reservation the fill must unwind. False for a fresh ephemeral taker
    /// that never reserved.
    pub reserved: bool,
}

impl<'a> TakerSide<'a> {
    /// Bind the taker to the position this fill settles into.
    ///
    /// An ephemeral taker holds only the empty position `build_perp_order`
    /// added, which `get_position_index` skips as available.
    /// `add_new_position` reuses that same slot, so the fill settles into it.
    /// A slot order always has a findable position from its placement, so the
    /// fallback never fires for one.
    pub(crate) fn bind(
        user: &'a mut User,
        stats: &'a mut UserStats,
        key: Pubkey,
        order: &'a mut Order,
        reserved: bool,
    ) -> VelocityResult<Self> {
        let direction = order.direction;
        let market_index = order.market_index;
        let position_index = get_position_index(&user.perp_positions, market_index)
            .or_else(|_| add_new_position(&mut user.perp_positions, market_index))?;
        let existing_position_params_before = user.perp_positions[position_index]
            .get_existing_position_params_for_order_action(direction);
        Ok(Self {
            user,
            stats,
            key,
            position_index,
            order,
            direction,
            existing_position_params_before,
            reserved,
        })
    }

    /// The same taker seat, borrowed for a shorter life.
    ///
    /// A layer that holds the seat and hands it to a step below keeps its own
    /// access to the taker afterwards.
    pub(crate) fn reborrow(&mut self) -> TakerSide<'_> {
        TakerSide {
            user: self.user,
            stats: self.stats,
            key: self.key,
            position_index: self.position_index,
            order: self.order,
            direction: self.direction,
            existing_position_params_before: self.existing_position_params_before,
            reserved: self.reserved,
        }
    }

    /// How much of the order is still to fill, capped by the position it
    /// settles into.
    pub(crate) fn unfilled_target(&self) -> VelocityResult<u64> {
        self.order.get_base_asset_amount_unfilled(Some(
            self.user.perp_positions[self.position_index].base_asset_amount,
        ))
    }
}

/// The maker side of one external fill: whose liquidity filled it, and where
/// the fill lands in their account.
///
/// `stats` is absent when the maker and the taker are the same account. The
/// maker volume is then recorded on the taker's stats instead.
pub(crate) struct MakerSide<'a, 'stats> {
    pub user: &'a mut User,
    pub stats: Option<&'stats mut UserStats>,
    pub key: Pubkey,
    /// Opposite the taker's, by construction.
    pub direction: PositionDirection,
    pub position_index: usize,
    /// Base and quote of the position before this fill, when the position
    /// already had one.
    pub existing_position_params: Option<(u64, u64)>,
    /// Whether the maker owns an open-order reservation the fill must
    /// release. The quoter reports it: a quoter that keeps its makers'
    /// aggregates holds the reservation velocity took at placement. This is
    /// [`TakerSide::reserved`] for the other side of the same fill.
    pub reserved: bool,
    /// The maker's own id for the order this fill came off, when the response
    /// named exactly one. It is what lets the fill record attribute to a book
    /// order. `None` when the change merged several and no single order owns
    /// it.
    pub order_id: Option<u32>,
}

impl<'a, 'stats> MakerSide<'a, 'stats> {
    /// Bind the maker to the position this fill lands in. A maker that holds
    /// no position in this market gets one.
    ///
    /// A fresh position is cross-margined, so this fallback would settle a
    /// book order into the wrong collateral pool if it ever fired for one. It
    /// cannot: a resting CLOB order holds `open_orders` and `open_bids` or
    /// `open_asks` on its owner's position, so that slot is never available
    /// and never recycled, and the order keeps one margin regime for its whole
    /// life. The fallback is for a maker the fill reaches with no book order
    /// behind it.
    #[allow(clippy::too_many_arguments)]
    pub fn bind(
        user: &'a mut User,
        stats: Option<&'stats mut UserStats>,
        key: Pubkey,
        taker_direction: PositionDirection,
        market_index: u16,
        reserved: bool,
        order_id: Option<u32>,
    ) -> VelocityResult<Self> {
        let direction = taker_direction.opposite();
        let position_index = get_position_index(&user.perp_positions, market_index)
            .or_else(|_| add_new_position(&mut user.perp_positions, market_index))?;
        let existing_position_params = user.perp_positions[position_index]
            .get_existing_position_params_for_order_action(direction);
        Ok(Self {
            user,
            stats,
            key,
            direction,
            position_index,
            existing_position_params,
            reserved,
            order_id,
        })
    }
}

/// Who takes the filler reward, and where a builder fee is escrowed. A path
/// that pays no filler holds `None` in each option.
///
/// Each option comes from a different owner, so each inner reference keeps
/// its own lifetime. A `&mut` to a `&mut` is invariant, so one shared
/// lifetime would force three unrelated borrows to be equal.
pub(crate) struct FillerSide<'a, 'user, 'stats, 'escrow, 'info> {
    pub user: &'a mut Option<&'user mut User>,
    pub stats: &'a mut Option<&'stats mut UserStats>,
    pub key: Pubkey,
    pub rev_share_escrow: &'a mut Option<&'escrow mut RevenueShareEscrowZeroCopyMut<'info>>,
}

/// The rules a fill prices and charges under. Fixed for a whole instruction.
pub(crate) struct FillPolicy<'a> {
    pub fee_structure: &'a FeeStructure,
    /// The oracle tolerances the quote snapshot is read under.
    pub validity_guard_rails: &'a ValidityGuardRails,
    pub referrer_is_accelerated: bool,
    pub is_liquidation: bool,
    pub promo_fee_tier: u8,
    /// Whether a vAMM fill pays the maker rebate.
    pub vamm_maker_rebate: bool,
    /// False when the taker does not meet initial margin. The fill proceeds and
    /// charges no builder fee. [`FillTerms::policy`] takes the decision and
    /// [`builder_fee_allowed`] states the rule.
    pub builder_fee_allowed: bool,
}

impl<'a> FillPolicy<'a> {
    /// The rules for a path that only settles an already-matched pair. It
    /// prices nothing, so it routes no external book, charges no builder fee
    /// and pays no referral acceleration.
    pub(crate) fn for_settlement(state: &'a State) -> Self {
        Self {
            fee_structure: &state.perp_fee_structure,
            validity_guard_rails: &state.oracle_guard_rails.validity,
            referrer_is_accelerated: false,
            is_liquidation: false,
            promo_fee_tier: state.promo_fee_tier,
            vamm_maker_rebate: false,
            builder_fee_allowed: false,
        }
    }
}

/// The builder order a fill accrues revenue share against, as the taker's
/// escrow names it.
#[derive(Clone, Copy, Default)]
struct BuilderEscrow {
    /// The builder's order in the escrow, when the taker's order carries one.
    order_index: Option<u32>,
    /// The referrer's order in the escrow, when the taker is referred.
    referrer_order_index: Option<u32>,
    /// The builder's rate, in tenths of a basis point.
    fee_tenth_bps: Option<u16>,
    /// The builder, as the escrow indexes it. The fill record carries it.
    builder_index: Option<u8>,
}

impl BuilderEscrow {
    /// Read the taker's escrow for the orders this fill accrues against.
    fn read(
        filler: &mut FillerSide,
        taker: &TakerSide,
        market_index: u16,
        order_id: u32,
        builder_fee_allowed: bool,
    ) -> Self {
        let (order_index, referrer_order_index, fee_tenth_bps, builder_index) =
            get_builder_escrow_info(
                filler.rev_share_escrow,
                taker.user.sub_account_id,
                order_id,
                market_index,
                taker.order.is_has_builder(),
                builder_fee_allowed,
            );
        Self {
            order_index,
            referrer_order_index,
            fee_tenth_bps,
            builder_index,
        }
    }
}

/// What every settle leg carries beyond the two seats: the market they settle
/// into, the rules the leg prices under, the oracle map the fill record reads,
/// the moment, and the per-fill filler allowance the legs draw down.
pub(crate) struct SettleContext<'a, 'o> {
    /// The market both seats settle into.
    pub market: &'a mut PerpMarket,
    pub policy: &'a FillPolicy<'a>,
    pub oracle_map: &'a mut OracleMap<'o>,
    pub now: i64,
    pub slot: u64,
    /// Filler reward already paid by earlier legs of this same fill. The
    /// time-based component of the reward is size-independent, so it is a
    /// per-fill allowance the legs draw down rather than one each.
    pub filler_reward_paid: &'a mut u64,
}

/// The fee split one settled allocation produced, and what it accrues against.
///
/// Each leg prices its own schedule against its own counterparty. From here
/// the three walk one spine: accrue the market's share, charge the taker, pay
/// the keeper, accrue the revenue share, advance the taker's order and unwind
/// what it reserved.
struct SettledFees {
    fees: FillFees,
    escrow: BuilderEscrow,
    /// The builder's share, flattened. Zero when no builder is owed one.
    builder_fee: u64,
}

impl SettledFees {
    /// Keep the split, and draw this leg's share off the per-fill filler
    /// allowance.
    fn take(fees: FillFees, escrow: BuilderEscrow, cx: &mut SettleContext) -> Self {
        *cx.filler_reward_paid = cx.filler_reward_paid.saturating_add(fees.filler_reward);
        let builder_fee = fees.builder_fee.unwrap_or(0);
        Self {
            fees,
            escrow,
            builder_fee,
        }
    }

    /// What the taker pays for this leg: its own fee and the builder's.
    fn taker_debit(&self) -> VelocityResult<u64> {
        self.fees.user_fee.safe_add(self.builder_fee)
    }

    /// Accrue the builder's share against the builder's order.
    ///
    /// A builder fee with no escrow to accrue into is a fee the taker approved
    /// and the builder could never claim, so it fails the fill rather than
    /// resolving to zero.
    fn accrue_builder_fee(
        &self,
        filler: &mut FillerSide,
        cx: &mut SettleContext,
    ) -> VelocityResult {
        if self.builder_fee == 0 {
            return Ok(());
        }
        match (
            self.escrow.order_index,
            filler.rev_share_escrow.as_deref_mut(),
        ) {
            (Some(index), Some(escrow)) => {
                accrue_revenue_share(escrow, index, self.builder_fee, cx.market)
            }
            _ => {
                validate!(
                    false,
                    ErrorCode::UnableToLoadRevenueShareAccount,
                    "Order has builder fee but no escrow account found"
                )?;
                Ok(())
            }
        }
    }

    /// Accrue the referrer's reward against the referrer's order.
    fn accrue_referrer_reward(
        &self,
        filler: &mut FillerSide,
        cx: &mut SettleContext,
    ) -> VelocityResult {
        match (
            self.escrow.referrer_order_index,
            filler.rev_share_escrow.as_deref_mut(),
        ) {
            (Some(index), Some(escrow)) => {
                accrue_revenue_share(escrow, index, self.fees.referrer_reward, cx.market)
            }
            _ => Ok(()),
        }
    }

    /// Mark the builder's order complete once the taker's order is.
    fn mark_builder_order_complete(&self, filler: &mut FillerSide) {
        if let (Some(index), Some(escrow)) = (
            self.escrow.order_index,
            filler.rev_share_escrow.as_deref_mut(),
        ) {
            let _ = escrow
                .get_order_mut(index)
                .map(|order| order.add_bit_flag(RevenueShareOrderBitFlag::Completed));
        }
    }
}

/// Book the market's share of one settled allocation.
///
/// The AMM books ONLY its own money: its fee provision plus any spread surplus
/// (`fee_to_market = amm_fee + surplus`). The protocol and insurance-fund
/// carveouts never touch the AMM's ledger or pools. They accrue as pending
/// quote counters here, because the quote spot market is not in scope at fill;
/// their token value lands in the pnl pool as fills settle and
/// `sweep_market_fees` materializes it. The AMM provision also grows the
/// lifetime backstop-of-last-resort clawback cap.
///
/// `amm_surplus` is `Some` only on the house leg, which is the one leg that
/// can capture spread. A counterparty leg books the AMM's provision only when
/// the schedule produced one.
fn accrue_market_fees(
    cx: &mut SettleContext,
    fees: &FillFees,
    amm_surplus: Option<i64>,
) -> VelocityResult {
    match amm_surplus {
        Some(surplus) => {
            <crate::vlp::amm::AMM as crate::vlp::amm::quoter::AmmContract>::apply_fill_fees(
                &mut cx.market.amm,
                fees.fee_to_market,
                surplus,
            )?;
        }
        None if fees.amm_fee > 0 => {
            <crate::vlp::amm::AMM as crate::vlp::amm::quoter::AmmContract>::apply_fill_fees(
                &mut cx.market.amm,
                fees.fee_to_market,
                0,
            )?;
        }
        None => {}
    }
    cx.market.fee_ledger.accrue_fill_fees(
        fees.user_fee,
        fees.protocol_fee,
        fees.if_fee,
        fees.amm_fee,
    )?;
    Ok(())
}

/// Charge the taker what this leg costs it: its own fee and the builder's.
fn charge_taker(
    taker: &mut TakerSide,
    settled: &SettledFees,
    cx: &mut SettleContext,
) -> VelocityResult {
    controller::position::update_quote_asset_and_break_even_amount(
        &mut taker.user.perp_positions[taker.position_index],
        cx.market,
        -settled.taker_debit()?.cast::<i64>()?,
    )?;
    taker.stats.increment_total_fees(settled.fees.user_fee)?;
    taker
        .stats
        .increment_total_referee_discount(settled.fees.referee_discount)
}

/// Pay the maker its rebate, on the seat that earned it.
///
/// A maker that is another subaccount of the taker's authority has no stats of
/// its own loaded, so its rebate is recorded on the taker's stats instead.
fn credit_maker_rebate(
    maker: &mut MakerSide,
    taker: &mut TakerSide,
    rebate: u64,
    cx: &mut SettleContext,
) -> VelocityResult {
    controller::position::update_quote_asset_and_break_even_amount(
        &mut maker.user.perp_positions[maker.position_index],
        cx.market,
        rebate.cast()?,
    )?;
    match maker.stats.as_mut() {
        Some(stats) => stats.increment_total_rebate(rebate),
        None => taker.stats.increment_total_rebate(rebate),
    }
}

/// Move the maker's position by what this leg filled, and record its volume.
///
/// A maker that is another subaccount of the taker's authority has no stats of
/// its own loaded, so its volume is recorded on the taker's stats instead.
fn move_maker_position(
    maker: &mut MakerSide,
    taker: &mut TakerSide,
    filled: FillAmounts,
    cx: &mut SettleContext,
) -> VelocityResult {
    let delta = get_position_delta_for_fill(filled.base, filled.quote, maker.direction)?;
    update_position_and_market(
        &mut maker.user.perp_positions[maker.position_index],
        cx.market,
        &delta,
    )?;
    match maker.stats.as_mut() {
        Some(stats) => stats.update_maker_volume_30d(filled.quote, cx.now),
        None => taker.stats.update_maker_volume_30d(filled.quote, cx.now),
    }
}

/// Move the taker's position by what this leg filled.
///
/// The volume it counts as is the leg's own business: a post-only taker fills
/// as the maker, so the house leg records maker volume instead.
fn move_taker_position(
    taker: &mut TakerSide,
    filled: FillAmounts,
    cx: &mut SettleContext,
) -> VelocityResult {
    let delta = get_position_delta_for_fill(filled.base, filled.quote, taker.direction)?;
    update_position_and_market(
        &mut taker.user.perp_positions[taker.position_index],
        cx.market,
        &delta,
    )?;
    Ok(())
}

/// Pay the keeper that turned this fill, out of the reward the schedule
/// carved.
///
/// A keeper with no reward still has its last-active slot stamped, so its
/// transaction does not revert for idleness.
fn pay_fill_keeper(
    filler: &mut FillerSide,
    settled: &SettledFees,
    quote_filled: u64,
    cx: &mut SettleContext,
) -> VelocityResult {
    let Some(filler_user) = filler.user.as_mut() else {
        return Ok(());
    };
    if settled.fees.filler_reward > 0 {
        let market_index = cx.market.market_index;
        let position_index = get_position_index(&filler_user.perp_positions, market_index)
            .or_else(|_| add_new_position(&mut filler_user.perp_positions, market_index))?;
        controller::position::update_quote_asset_amount(
            &mut filler_user.perp_positions[position_index],
            cx.market,
            settled.fees.filler_reward.cast()?,
        )?;
        filler
            .stats
            .as_mut()
            .safe_unwrap()?
            .update_filler_volume(quote_filled, cx.now)?;
    }
    filler_user.update_last_active_slot(cx.slot);
    Ok(())
}

/// Advance the taker's order by what this leg filled, and unwind the
/// reservation it held for that size.
///
/// Only a reservation the taker actually took is unwound. A fresh ephemeral
/// taker never reserved, and unwinding here would eat a co-resident order's
/// `open_bids`/`open_asks`.
fn advance_taker_order(
    taker: &mut TakerSide,
    filler: &mut FillerSide,
    settled: &SettledFees,
    filled: FillAmounts,
) -> VelocityResult {
    // Update the taker order BEFORE the event emit.
    if update_order_after_fill(taker.order, filled.base, filled.quote)? {
        settled.mark_builder_order_complete(filler);
    }
    if taker.reserved {
        decrease_open_bids_and_asks(
            &mut taker.user.perp_positions[taker.position_index],
            &taker.direction,
            filled.base,
            taker.order.update_open_bids_and_asks(),
        )?;
    }
    Ok(())
}

/// The vAMM slice one house leg settles, as the router priced it.
pub(super) struct AmmAllocation {
    /// The shaded quote the router priced `base` at. The shade is taker-worse
    /// than the live curve, so charging this quote and not the curve keeps the
    /// shade for the LPs.
    pub quote: u64,
    pub base: u64,
    /// The taker's order as the AMM fee schedule reads it.
    pub post_only: bool,
    pub order_slot: u64,
    pub order_id: u32,
    pub taker_limit: Option<u64>,
}

/// The vAMM's seat in a house fill.
///
/// The house holds no position, so the only account here is a maker that
/// cranked the fill and therefore earns the keeper reward.
pub(super) struct HouseSide<'a, 'user, 'stats> {
    pub cranking_maker: &'a mut Option<&'user mut User>,
    pub cranking_maker_stats: &'a mut Option<&'stats mut UserStats>,
    /// Whether the house pays a maker rebate for making the fill.
    pub pays_maker_rebate: bool,
}

/// What the taker pays for a vAMM slice, and what the house keeps as spread.
///
/// A post-only sole-AMM step makes the taker the maker: it transacts at its
/// own limit, and the house keeps the curve-to-limit gap as spread surplus.
/// Every other step charges the router's shade and holds the taker to its
/// limit.
fn amm_house_taker_quote(
    fill: &QuoterFill,
    taker: &TakerSide,
    allocation: &AmmAllocation,
) -> VelocityResult<(u64, i64)> {
    match (allocation.post_only, allocation.taker_limit) {
        (true, Some(limit)) => crate::controller::position::calculate_quote_asset_amount_surplus(
            taker.direction,
            fill.quote_filled,
            fill.base_filled,
            limit,
        ),
        _ => settle_amm_house_normal_quote(
            fill,
            taker.direction,
            allocation.taker_limit,
            allocation.quote,
            allocation.base,
        ),
    }
}

/// Settle one vAMM slice against the house.
///
/// The taker is the only account holding a position, so this leg moves no
/// maker and unwinds nothing of a counterparty's. What it has instead is the
/// spread surplus, which only the house can capture, and a cranking maker to
/// pay when no separate keeper turned the fill.
pub(super) fn settle_amm_house_fill(
    fill: &QuoterFill,
    taker: &mut TakerSide,
    house: &mut HouseSide,
    allocation: &AmmAllocation,
    filler: &mut FillerSide,
    cx: &mut SettleContext,
) -> VelocityResult<(u64, u64)> {
    let (taker_quote, taker_surplus, settled) =
        price_amm_house_fill(fill, taker, house, allocation, filler, cx)?;
    let filled = FillAmounts {
        base: fill.base_filled,
        quote: taker_quote,
    };
    settled.accrue_builder_fee(filler, cx)?;

    move_taker_position(taker, filled, cx)?;
    accrue_market_fees(cx, &settled.fees, Some(taker_surplus))?;

    taker
        .stats
        .increment_total_rebate(settled.fees.maker_rebate)?;
    settled.accrue_referrer_reward(filler, cx)?;
    charge_taker(taker, &settled, cx)?;
    if settled.fees.maker_rebate != 0 {
        controller::position::update_quote_asset_and_break_even_amount(
            &mut taker.user.perp_positions[taker.position_index],
            cx.market,
            settled.fees.maker_rebate.cast()?,
        )?;
    }
    if allocation.post_only {
        taker.stats.update_maker_volume_30d(taker_quote, cx.now)?;
    } else {
        taker.stats.update_taker_volume_30d(taker_quote, cx.now)?;
    }
    pay_house_keeper(house, filler, &settled, taker_quote, cx)?;

    advance_taker_order(taker, filler, &settled, filled)?;
    emit_amm_house_record(taker, &settled, filled, taker_surplus, &filler.key, cx)?;
    Ok((fill.base_filled, taker_quote))
}

/// Price one vAMM slice on the house fee schedule.
///
/// Returns what the taker pays, what the house keeps as spread, and the split
/// of the fee between them.
fn price_amm_house_fill(
    fill: &QuoterFill,
    taker: &mut TakerSide,
    house: &mut HouseSide,
    allocation: &AmmAllocation,
    filler: &mut FillerSide,
    cx: &mut SettleContext,
) -> VelocityResult<(u64, i64, SettledFees)> {
    let (taker_quote, taker_surplus) = amm_house_taker_quote(fill, taker, allocation)?;
    let market_index = cx.market.market_index;
    let reward_referrer =
        can_reward_user_with_referral_reward(market_index, filler.rev_share_escrow);
    let reward_filler = can_reward_user_with_perp_pnl(filler.user, market_index)
        || can_reward_user_with_perp_pnl(house.cranking_maker, market_index);
    let escrow = BuilderEscrow::read(
        filler,
        taker,
        market_index,
        allocation.order_id,
        cx.policy.builder_fee_allowed,
    );
    let fees = fees::calculate_fee_for_fulfillment_with_amm(
        taker.stats,
        taker_quote,
        cx.policy.fee_structure,
        allocation.order_slot,
        cx.slot,
        reward_filler,
        reward_referrer,
        cx.policy.referrer_is_accelerated,
        taker_surplus,
        allocation.post_only,
        cx.market.fee_adjustment,
        escrow.fee_tenth_bps,
        house.pays_maker_rebate,
        cx.market.taker_fee_addon_tenth_bps,
        cx.now,
        cx.policy.promo_fee_tier,
        cx.oracle_map.slot_clock,
        *cx.filler_reward_paid,
    )?;
    Ok((
        taker_quote,
        taker_surplus,
        SettledFees::take(fees, escrow, cx),
    ))
}

/// Pay whoever turned the house fill: the keeper when one is loaded, otherwise
/// the maker that cranked it.
fn pay_house_keeper(
    house: &mut HouseSide,
    filler: &mut FillerSide,
    settled: &SettledFees,
    taker_quote: u64,
    cx: &mut SettleContext,
) -> VelocityResult {
    if let Some(filler_user) = filler.user.as_mut() {
        return credit_filler_perp_pnl(
            filler_user,
            filler.stats,
            cx.market,
            settled.fees.filler_reward,
            taker_quote,
            cx.now,
            cx.slot,
        );
    }
    if let Some(maker_user) = house.cranking_maker.as_mut() {
        return credit_filler_perp_pnl(
            maker_user,
            house.cranking_maker_stats,
            cx.market,
            settled.fees.filler_reward,
            taker_quote,
            cx.now,
            cx.slot,
        );
    }
    Ok(())
}

/// Emit the fill record for a house leg.
///
/// The house holds no position, so both of the record's seats are the taker's
/// own: a post-only taker filled as the maker and is reported on the maker
/// seat.
fn emit_amm_house_record(
    taker: &mut TakerSide,
    settled: &SettledFees,
    filled: FillAmounts,
    taker_surplus: i64,
    filler_key: &Pubkey,
    cx: &mut SettleContext,
) -> VelocityResult {
    let (taker_record_key, taker_record_order, maker_record_key, maker_record_order) =
        get_taker_and_maker_for_order_record(&taker.key, taker.order);
    let explanation = if cx.policy.is_liquidation {
        OrderActionExplanation::Liquidation
    } else {
        OrderActionExplanation::OrderFilledWithAMM
    };
    // The house is the counterparty, so it holds no position to be isolated.
    let bit_flags = fill_record_bit_flags(taker, false);
    let (existing_quote_entry_amount, existing_base_asset_amount) =
        calculate_existing_position_fields_for_order_action(
            filled.base,
            taker.existing_position_params_before,
        )?;
    let on_taker_seat = taker_record_key.is_some();
    let (taker_existing_quote, taker_existing_base) = if on_taker_seat {
        (existing_quote_entry_amount, existing_base_asset_amount)
    } else {
        (None, None)
    };
    let (maker_existing_quote, maker_existing_base) = if on_taker_seat {
        (None, None)
    } else {
        (existing_quote_entry_amount, existing_base_asset_amount)
    };
    emit_perp_action_record(
        cx.market,
        cx.oracle_map,
        cx.now,
        explanation,
        filler_key,
        settled.fees.filler_reward,
        filled.base,
        filled.quote,
        settled.taker_debit()?,
        (settled.fees.maker_rebate != 0).then_some(settled.fees.maker_rebate),
        settled.fees.referrer_reward,
        Some(taker_surplus),
        taker_record_key,
        taker_record_order,
        maker_record_key,
        maker_record_order,
        bit_flags,
        taker_existing_quote,
        taker_existing_base,
        maker_existing_quote,
        maker_existing_base,
        settled.escrow.builder_index,
        settled.fees.builder_fee,
    )
}

/// The resting maker order a DLOB match filled against, and the prices the
/// match is held to.
pub(crate) struct DlobMatch {
    /// The maker order's slot in its owner's `orders` array.
    pub order_index: usize,
    /// The sanitized price discovery froze the maker order at.
    pub maker_price: u64,
    /// The taker's effective limit. Its side of the fill must clear it.
    pub taker_limit: u64,
    /// The oracle price the filler-reward tier is measured against.
    pub oracle_price: i64,
}

/// The prices an external match is held to.
///
/// There is no maker price here. The route already bound the quoter's response
/// per unit against its own quoted levels, which is the maker-side contract.
pub(crate) struct ExternalMatch {
    /// The taker's effective limit, when the order carries one.
    pub taker_limit: Option<u64>,
    /// The oracle price the filler-reward tier is measured against.
    pub oracle_price: i64,
}

/// What the keeper's reward tier is measured against.
///
/// The tier reads the maker's own price, so a leg with no single maker price
/// hands in the average it filled at instead.
#[derive(Clone, Copy)]
struct RewardTier {
    maker_price: u64,
    oracle_price: i64,
}

/// Price one match on the maker fee schedule.
fn price_matched_fill(
    taker: &mut TakerSide,
    maker: &MakerSide,
    filled: FillAmounts,
    tier: RewardTier,
    filler: &mut FillerSide,
    cx: &mut SettleContext,
) -> VelocityResult<SettledFees> {
    let market_index = cx.market.market_index;
    let reward_referrer =
        can_reward_user_with_referral_reward(market_index, filler.rev_share_escrow);
    // A maker that cranks its own fill arrives as `filler: None` with the
    // filler key naming itself: it is already loaded in the maker map, and the
    // same account cannot be loaded mutably twice. It did the keeper's work on
    // a slice it actually filled, so it earns the reward for that slice, which
    // spreads a multi-maker fill's reward pro rata. A taker filling its own
    // order names *itself*, so this stays false and no reward is charged.
    let maker_is_filler = filler.key == maker.key;
    let reward_filler = can_reward_user_with_perp_pnl(filler.user, market_index) || maker_is_filler;
    let escrow = BuilderEscrow::read(
        filler,
        taker,
        market_index,
        taker.order.order_id,
        cx.policy.builder_fee_allowed,
    );
    let filler_multiplier = if reward_filler {
        calculate_filler_multiplier_for_matched_orders(
            tier.maker_price,
            maker.direction,
            tier.oracle_price,
        )?
    } else {
        0
    };
    let fees = fees::calculate_fee_for_fulfillment_with_match(
        taker.stats,
        &maker.stats,
        filled.quote,
        cx.policy.fee_structure,
        taker.order.slot,
        cx.slot,
        filler_multiplier,
        reward_referrer,
        cx.policy.referrer_is_accelerated,
        &MarketType::Perp,
        cx.market.fee_adjustment,
        escrow.fee_tenth_bps,
        cx.market.taker_fee_addon_tenth_bps,
        cx.now,
        cx.policy.promo_fee_tier,
        cx.oracle_map.slot_clock,
        *cx.filler_reward_paid,
    )?;
    Ok(SettledFees::take(fees, escrow, cx))
}

/// The spine both match legs walk once their own schedule has priced the fill.
///
/// Book the market's share, charge the taker, pay the maker its rebate, pay
/// the keeper, accrue the referrer's reward, and advance the taker's order.
/// What is left for each leg is its counterparty's own unwind and its record.
fn settle_matched_fill(
    taker: &mut TakerSide,
    maker: &mut MakerSide,
    settled: &SettledFees,
    filled: FillAmounts,
    filler: &mut FillerSide,
    cx: &mut SettleContext,
) -> VelocityResult {
    settled.accrue_builder_fee(filler, cx)?;
    accrue_market_fees(cx, &settled.fees, None)?;
    charge_taker(taker, settled, cx)?;
    credit_maker_rebate(maker, taker, settled.fees.maker_rebate, cx)?;
    pay_matched_keeper(maker, filler, settled, filled.quote, cx)?;
    settled.accrue_referrer_reward(filler, cx)?;
    advance_taker_order(taker, filler, settled, filled)
}

/// Pay the keeper that turned a matched fill.
///
/// A maker that cranked its own fill is paid on its own seat, because it is
/// already loaded as the maker and cannot be loaded a second time as the
/// filler.
fn pay_matched_keeper(
    maker: &mut MakerSide,
    filler: &mut FillerSide,
    settled: &SettledFees,
    quote_filled: u64,
    cx: &mut SettleContext,
) -> VelocityResult {
    if filler.user.is_some() {
        return pay_fill_keeper(filler, settled, quote_filled, cx);
    }
    if filler.key != maker.key {
        return Ok(());
    }
    credit_filler_perp_pnl(
        maker.user,
        &mut maker.stats.as_deref_mut(),
        cx.market,
        settled.fees.filler_reward,
        quote_filled,
        cx.now,
        cx.slot,
    )
}

/// Settle one DLOB match: the taker against one resting velocity order.
///
/// The maker's liquidity is a velocity `Order`, so this leg is the one that
/// advances that order and flips it to `Filled`, and the one whose fill record
/// carries a maker order.
pub(crate) fn settle_dlob_match_fill(
    fill: &QuoterFill,
    taker: &mut TakerSide,
    maker: &mut MakerSide,
    matched: &DlobMatch,
    filler: &mut FillerSide,
    cx: &mut SettleContext,
) -> VelocityResult<(u64, u64, u64)> {
    let filled = FillAmounts {
        base: fill.base_filled,
        quote: fill.quote_filled,
    };
    validate_fill_price(
        filled.quote,
        filled.base,
        BASE_PRECISION_U64,
        taker.direction,
        matched.taker_limit,
        true,
    )?;
    validate_fill_price(
        filled.quote,
        filled.base,
        BASE_PRECISION_U64,
        maker.direction,
        matched.maker_price,
        false,
    )?;

    move_maker_position(maker, taker, filled, cx)?;
    move_taker_position(taker, filled, cx)?;
    taker.stats.update_taker_volume_30d(filled.quote, cx.now)?;

    let tier = RewardTier {
        maker_price: matched.maker_price,
        oracle_price: matched.oracle_price,
    };
    let settled = price_matched_fill(taker, maker, filled, tier, filler, cx)?;
    settle_matched_fill(taker, maker, &settled, filled, filler, cx)?;
    unwind_matched_maker_order(maker, matched.order_index, filled.base)?;

    emit_matched_record(
        taker,
        maker,
        &settled,
        filled,
        MatchedRecord {
            explanation: OrderActionExplanation::OrderFilledWithMatch,
            maker_order: Some(maker.user.orders[matched.order_index]),
            filler_key: filler.key,
        },
        cx,
    )?;
    Ok((filled.base, filled.quote, filled.base))
}

/// Unwind the reservation the filled maker order held, and retire it once it
/// has nothing left.
///
/// The quoter already advanced the order's own filled counters, so only the
/// open-bids/asks aggregate and the status remain.
fn unwind_matched_maker_order(
    maker: &mut MakerSide,
    order_index: usize,
    base_filled: u64,
) -> VelocityResult {
    let updates_open_bids_and_asks = maker.user.orders[order_index].update_open_bids_and_asks();
    decrease_open_bids_and_asks(
        &mut maker.user.perp_positions[maker.position_index],
        &maker.direction,
        base_filled,
        updates_open_bids_and_asks,
    )?;
    if maker.user.orders[order_index].get_base_asset_amount_unfilled(None)? == 0 {
        maker.user.orders[order_index].status = OrderStatus::Filled;
    }
    Ok(())
}

/// Settle one external-quoter balance change.
///
/// The maker is a loaded `User` whose resting liquidity lives outside velocity
/// — a CLOB order or a PropAMM quote — so unlike [`settle_dlob_match_fill`]
/// there is no velocity `Order` to advance: the external program already
/// committed its own book state. Everything protocol-level is the same match
/// spine.
///
/// The maker side runs no `validate_fill_price`. The route already held the
/// response per unit to this quoter's own quoted levels, which is the
/// maker-side price contract here. The taker side clears its effective limit
/// as usual.
pub(crate) fn settle_external_match_fill(
    filled: FillAmounts,
    taker: &mut TakerSide,
    maker: &mut MakerSide,
    prices: &ExternalMatch,
    filler: &mut FillerSide,
    cx: &mut SettleContext,
) -> VelocityResult<(u64, u64)> {
    if let Some(limit) = prices.taker_limit {
        validate_fill_price(
            filled.quote,
            filled.base,
            BASE_PRECISION_U64,
            taker.direction,
            limit,
            true,
        )?;
    }

    move_maker_position(maker, taker, filled, cx)?;
    move_taker_position(taker, filled, cx)?;
    taker.stats.update_taker_volume_30d(filled.quote, cx.now)?;

    // An external fill has no single maker limit, so the average fill price
    // stands in for the filler-reward tier.
    let average_price = filled
        .quote
        .cast::<u128>()?
        .safe_mul(BASE_PRECISION_U64.cast()?)?
        .safe_div(filled.base.cast()?)?
        .cast::<u64>()?;
    let tier = RewardTier {
        maker_price: average_price,
        oracle_price: prices.oracle_price,
    };
    let settled = price_matched_fill(taker, maker, filled, tier, filler, cx)?;
    settle_matched_fill(taker, maker, &settled, filled, filler, cx)?;
    release_external_maker_reservation(maker, filled.base)?;

    emit_matched_record(
        taker,
        maker,
        &settled,
        filled,
        MatchedRecord {
            explanation: OrderActionExplanation::OrderFilledWithExternalQuoter,
            maker_order: maker.order_id.map(|order_id| Order {
                order_id,
                market_index: cx.market.market_index,
                market_type: MarketType::Perp,
                direction: maker.direction,
                status: OrderStatus::Open,
                order_type: OrderType::Limit,
                post_only: true,
                bit_flags: OrderBitFlag::PlacedOnClob as u8,
                ..Order::default()
            }),
            filler_key: filler.key,
        },
        cx,
    )?;
    Ok((filled.base, filled.quote))
}

/// Release the open-base a quoter's maker reserved for the size this leg
/// filled.
///
/// The maker's leg is the quoter's own claim about a user it does not own, so
/// it is held to that user's reservation rather than clamped to it. This is
/// the single place every external settlement passes through — the router
/// fill and both cross cranks — which is what stops a caller from settling one
/// without the bound.
///
/// CLOB orders are margin-reserved through velocity at placement, so their
/// fills release those aggregates. Custom PropAMM depth is never reserved.
fn release_external_maker_reservation(maker: &mut MakerSide, base_filled: u64) -> VelocityResult {
    if !maker.reserved {
        return Ok(());
    }
    position::release_reserved_open_base(
        &mut maker.user.perp_positions[maker.position_index],
        &maker.direction,
        base_filled,
    )
}

/// What one match leg's fill record says that the spine cannot.
struct MatchedRecord {
    explanation: OrderActionExplanation,
    /// The maker's order, as the record reports it. An external quoter has no
    /// velocity order, so it reconstructs the one its book row stands for.
    maker_order: Option<Order>,
    filler_key: Pubkey,
}

/// Emit the fill record for a matched leg.
fn emit_matched_record(
    taker: &mut TakerSide,
    maker: &MakerSide,
    settled: &SettledFees,
    filled: FillAmounts,
    record: MatchedRecord,
    cx: &mut SettleContext,
) -> VelocityResult {
    let explanation = if cx.policy.is_liquidation {
        OrderActionExplanation::Liquidation
    } else {
        record.explanation
    };
    let bit_flags = fill_record_bit_flags(
        taker,
        maker.user.perp_positions[maker.position_index].is_isolated(),
    );
    let (taker_existing_quote, taker_existing_base) =
        calculate_existing_position_fields_for_order_action(
            filled.base,
            taker.existing_position_params_before,
        )?;
    let (maker_existing_quote, maker_existing_base) =
        calculate_existing_position_fields_for_order_action(
            filled.base,
            maker.existing_position_params,
        )?;
    let taker_order = *taker.order;
    emit_perp_action_record(
        cx.market,
        cx.oracle_map,
        cx.now,
        explanation,
        &record.filler_key,
        settled.fees.filler_reward,
        filled.base,
        filled.quote,
        settled.taker_debit()?,
        Some(settled.fees.maker_rebate),
        settled.fees.referrer_reward,
        None,
        Some(taker.key),
        Some(taker_order),
        Some(maker.key),
        record.maker_order,
        bit_flags,
        taker_existing_quote,
        taker_existing_base,
        maker_existing_quote,
        maker_existing_base,
        settled.escrow.builder_index,
        settled.fees.builder_fee,
    )
}

/// Accrue a revenue-share amount against the builder's order.
///
/// The per-order accrual is mirrored into the market aggregate the fee sweep
/// reserves against (audit #73), so the two never drift.
fn accrue_revenue_share(
    escrow: &mut RevenueShareEscrowZeroCopyMut,
    order_index: u32,
    amount: u64,
    market: &mut PerpMarket,
) -> VelocityResult<()> {
    let order = escrow.get_order_mut(order_index)?;
    order.fees_accrued = order.fees_accrued.safe_add(amount)?;
    market.accrue_pending_revenue_share(amount)?;
    Ok(())
}

/// Fold one settled allocation into the worst price the fill has reached.
///
/// Worse means higher for a buy and lower for a sell. The price is the
/// allocation's own quote over its own base, floored, which is how every other
/// per-fill price in this file is derived. Both directions round the same way,
/// so a caller comparing a buy price against a sell price compares two numbers
/// that were rounded alike.
pub(super) fn note_worst_fill_price(
    worst: &mut Option<u64>,
    direction: PositionDirection,
    base_filled: u64,
    quote_filled: u64,
) -> VelocityResult {
    if base_filled == 0 {
        return Ok(());
    }
    let price = quote_filled
        .cast::<u128>()?
        .safe_mul(BASE_PRECISION_U64.cast()?)?
        .safe_div(base_filled.cast()?)?
        .cast::<u64>()?;
    *worst = Some(match (*worst, direction) {
        (None, _) => price,
        (Some(seen), PositionDirection::Long) => seen.max(price),
        (Some(seen), PositionDirection::Short) => seen.min(price),
    });
    Ok(())
}

/// The bit flags every fill record carries.
///
/// A signed-message order is marked so a consumer can tell swift flow from
/// on-chain flow. A fill is marked isolated when either side settles into an
/// isolated position, because the record then describes a position whose
/// collateral is not the account's.
fn fill_record_bit_flags(taker: &TakerSide, maker_is_isolated: bool) -> u8 {
    let flags = set_order_bit_flag(0, taker.order.is_signed_msg(), OrderBitFlag::SignedMessage);
    let taker_is_isolated = taker.user.perp_positions[taker.position_index].is_isolated();
    set_order_bit_flag(
        flags,
        taker_is_isolated || maker_is_isolated,
        OrderBitFlag::IsIsolatedPosition,
    )
}
