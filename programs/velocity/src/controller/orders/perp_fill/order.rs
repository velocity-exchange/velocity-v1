//! The order layer of a perp fill.
//!
//! This layer governs the order. It finds the order in its owner's slot or in
//! the caller's handle, admits or refuses the fill, refreshes the market
//! oracle statistics, and binds the keeper. It then applies the bookkeeping
//! the fill leaves behind: the write-back, the fill-price band, the
//! reduce-only cancel, the open-interest cap and the funding update.
//!
//! [`super::taker_risk`] applies the taker's own limits around the fill.

use {
    super::{super::*, context::*},
    crate::{
        controller::{
            funding::settle_funding_payment,
            position::{add_new_position, get_position_index},
        },
        error::{ErrorCode, VelocityResult},
        instructions::optional_accounts::AccountMaps,
        load_mut,
        math::{
            constants::BASE_PRECISION_U64, liquidation::validate_user_not_being_liquidated,
            router::RouterLeg, safe_unwrap::SafeUnwrap,
        },
        state::{
            fill_mode::FillMode,
            market_status::MarketStatus,
            paused_operations::PerpOperation,
            revenue_share::RevenueShareEscrowZeroCopyMut,
            state::State,
            user::{MarketType, Order, OrderStatus, ReferrerStatus, User, UserStats},
            user_map::UserMap,
        },
        validate,
    },
    anchor_lang::prelude::{msg, Clock, Pubkey},
    std::cell::RefMut,
};

/// [`fill_perp_order`] with no external quoter books.
///
/// The route still runs over the vAMM ladder and the quoter books. The
/// split has no CPI book to price in. Every fill entrypoint that carries no
/// quoter accounts arrives here.
#[cfg(test)]
pub(crate) fn fill_perp_order_without_external_books<'info>(
    order: &mut Order,
    reserved: bool,
    state: &State,
    user: &AccountLoader<'info, User>,
    user_stats: &AccountLoader<'info, UserStats>,
    maps: &mut AccountMaps,
    filler: &AccountLoader<'info, User>,
    filler_stats: &AccountLoader<'info, UserStats>,
    makers_and_referrer: &UserMap,
    makers_and_referrer_stats: &crate::state::user_map::UserStatsMap,
    clock: &Clock,
    fill_mode: FillMode,
    rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut<'info>>,
    referrer_is_accelerated: bool,
) -> VelocityResult<FillAmounts> {
    let mut no_externals = crate::state::prop_amm::NoExternalQuoters;
    let mut router_inputs = crate::math::router::RouterLeg {
        books: &[],
        executor: &mut no_externals,
        standing: crate::instructions::FillerStanding {
            protocol_authority: state.signer,
            taker_exposure_closed_by_caller: false,
            // This path carries no external book, so nothing can withhold
            // depth and the obligation is never reached.
            obligation: crate::math::router::FillerObligation::default(),
        },

        worst_fill_price: None,
    };

    fill_perp_order(
        FillRequest {
            order,
            reserved,
            mode: fill_mode,
            referrer_is_accelerated,
        },
        state,
        clock,
        PerpFillAccounts {
            user,
            user_stats,
            filler,
            filler_stats,
            rev_share_escrow,
        },
        &mut FillParties {
            maps,
            makers_and_referrer,
            makers_and_referrer_stats,
        },
        &mut router_inputs,
    )
}

/// What one fill is asked to do.
pub struct FillRequest<'a> {
    /// The order to fill. It is the caller's, and the fill writes its progress
    /// back through this handle. No order lives in a `User.orders` slot while
    /// it is live, so there is nowhere else for one to come from.
    pub order: &'a mut Order,
    /// Whether the order already holds an `open_bids`/`open_asks` and
    /// `open_orders` reservation. True for an order lifted off the book,
    /// false for an ephemeral taker routed straight to the book. Unwinding
    /// a false reservation would underflow another order's counter.
    pub reserved: bool,
    pub mode: FillMode,
    /// Whether the taker's referrer is on the accelerated schedule.
    pub referrer_is_accelerated: bool,
}

/// The two seats a perp fill loads for itself: the taker whose order it is,
/// and the keeper that runs the fill. Everyone else the fill touches arrives
/// in [`FillParties`].
pub struct PerpFillAccounts<'a, 'escrow, 'info> {
    pub user: &'a AccountLoader<'info, User>,
    pub user_stats: &'a AccountLoader<'info, UserStats>,
    pub filler: &'a AccountLoader<'info, User>,
    pub filler_stats: &'a AccountLoader<'info, UserStats>,
    /// The taker's revenue-share escrow, when the keeper supplied it.
    pub rev_share_escrow: &'a mut Option<&'escrow mut RevenueShareEscrowZeroCopyMut<'info>>,
}

/// The taker's loaded account, its stats, and the key they are addressed by.
pub(super) struct Taker<'a> {
    pub user: &'a mut User,
    pub stats: &'a mut UserStats,
    pub key: Pubkey,
    /// The position the order settles into. Bound by [`Self::bind_position`]
    /// once the fill is past the admission gates.
    position_index: Option<usize>,
}

impl<'a> Taker<'a> {
    fn new(user: &'a mut User, stats: &'a mut UserStats, key: Pubkey) -> Self {
        Self {
            user,
            stats,
            key,
            position_index: None,
        }
    }

    /// An ephemeral taker's empty position, added by `build_perp_order`, is
    /// skipped by `get_position_index` and reused by `add_new_position`. A
    /// slot order always finds its position, so the fallback never fires.
    /// Binding can add a position, so it runs only after the fill is admitted.
    fn bind_position(&mut self, market_index: u16) -> VelocityResult {
        let index = get_position_index(&self.user.perp_positions, market_index)
            .or_else(|_| add_new_position(&mut self.user.perp_positions, market_index))?;
        self.position_index = Some(index);
        Ok(())
    }

    /// The position this order settles into.
    fn position(&self) -> VelocityResult<&crate::state::user::PerpPosition> {
        Ok(&self.user.perp_positions[self.position_index.safe_unwrap()?])
    }
}

/// The filler that runs a fill, and the flat reward it is owed. Both
/// accounts are absent when the filler is the taker, a maker, or another
/// subaccount of the taker's authority: such a filler earns no reward and
/// is never loaded twice.
pub(super) struct Filler<'a> {
    pub user: Option<&'a mut User>,
    pub stats: Option<&'a mut UserStats>,
    pub key: Pubkey,
}

/// The order a fill works on, and the caller's handle it goes back to.
/// `Order` is `Copy` and 104 bytes, so the fill works on a copy and writes
/// it back once. The handle then lets the caller rest, cancel or discard
/// whatever the fill left unfilled.
pub(super) struct WorkingOrder<'a> {
    pub order: Order,
    /// The caller's handle on the order.
    handle: &'a mut Order,
    /// Whether the taker already owns an `open_bids`/`open_asks` and
    /// `open_orders` reservation the fill must unwind as it fills. See
    /// [`FillRequest::reserved`].
    pub reserved: bool,
    /// The `reduce_only` flag the order was read with, before a reduce-only
    /// market stamped the flag on. The auction-duration skip reads the flag
    /// the order was placed with.
    reduce_only_at_entry: bool,
}

impl<'a> WorkingOrder<'a> {
    /// Take the fill's own copy of the caller's order.
    fn of(handle: &'a mut Order, reserved: bool) -> VelocityResult<Self> {
        let order = *handle;
        validate!(
            order.market_type == MarketType::Perp,
            ErrorCode::InvalidOrderMarketType,
            "must be perp order"
        )?;

        Ok(Self {
            reduce_only_at_entry: order.reduce_only,
            order,
            handle,
            reserved,
        })
    }

    /// Put the fill's progress back on the caller's order, so the caller sees
    /// what the fill did to it.
    fn write_back(&mut self) {
        *self.handle = self.order;
    }
}

/// Fill one perp order, which the caller holds.
///
/// This layer governs the order. It finds the order, admits or refuses the
/// fill, refreshes the market oracle statistics, binds the keeper and collects
/// the makers the fill may settle against. [`OrderUnderFill`] carries all of
/// that through the fill and the bookkeeping it leaves behind.
pub fn fill_perp_order(
    request: FillRequest<'_>,
    state: &State,
    clock: &Clock,
    accounts: PerpFillAccounts<'_, '_, '_>,
    parties: &mut FillParties,
    router: &mut RouterLeg,
) -> VelocityResult<FillAmounts> {
    let filler_key = accounts.filler.key();
    let user_key = accounts.user.key();
    let mut user = load_mut!(accounts.user)?;
    let mut user_stats = load_mut!(accounts.user_stats)?;
    let mut taker = Taker::new(&mut user, &mut user_stats, user_key);
    let rules = PricingRules::of(state, request.referrer_is_accelerated);

    let mut order = WorkingOrder::of(request.order, request.reserved)?;
    let market_index = order.order.market_index;

    admit_perp_market(&mut order, &mut taker, parties.maps, clock.unix_timestamp)?;
    let rev_share_escrow = accounts.rev_share_escrow;
    if admit_taker(
        &order,
        &mut taker,
        state,
        parties,
        request.mode,
        &*rev_share_escrow,
    )? == Admission::Skip
    {
        return Ok(FillAmounts::default());
    }

    let conditions =
        FillConditions::read(state, parties.maps, &taker, &order, request.mode, clock)?;
    let (mut filler, mut filler_stats) = bind_filler(
        accounts.filler,
        accounts.filler_stats,
        parties.makers_and_referrer,
        &taker,
        &filler_key,
    )?;

    OrderUnderFill {
        order,
        taker,
        filler: Filler {
            user: filler.as_deref_mut(),
            stats: filler_stats.as_deref_mut(),
            key: filler_key,
        },

        state,
        rules,
        conditions,
        market_index,
    }
    .run(parties, router, rev_share_escrow)
}

/// The market admits the fill.
///
/// A `ReduceOnly` market forces every order it fills to be risk-reducing.
/// Placement stamps `order.reduce_only` from the market status at the time the
/// order was created. An order placed while the market was `Active` therefore
/// still carries `reduce_only = false` after an admin sets the market to
/// `ReduceOnly`. Every downstream reduce-only guard reads the stored flag, so
/// the flag is re-derived from the live market status here and stamped onto
/// the order. Those guards are the fill-size clamp in
/// `get_base_asset_amount_unfilled`, `should_cancel_reduce_only_order` and the
/// trigger-path risk check.
fn admit_perp_market(
    order: &mut WorkingOrder,
    taker: &mut Taker,
    maps: &mut AccountMaps,
    now: i64,
) -> VelocityResult {
    let mut market = maps
        .perp_market_map
        .get_ref_mut(&order.order.market_index)?;
    // settle lp position so its tradeable
    settle_funding_payment(taker.user, &taker.key, &mut market, now)?;
    validate!(
        matches!(
            market.status,
            MarketStatus::Active | MarketStatus::ReduceOnly
        ),
        ErrorCode::MarketFillOrderPaused,
        "Market not active",
    )?;
    validate!(
        !market.is_operation_paused(PerpOperation::Fill),
        ErrorCode::MarketFillOrderPaused,
        "Market fills paused",
    )?;

    let market_is_reduce_only = market.is_reduce_only()?;
    drop(market);

    if market_is_reduce_only {
        order.order.reduce_only = true;
    }

    Ok(())
}

/// The order and the taker admit the fill.
///
/// A bankrupt taker and a taker under liquidation both refuse the fill without
/// failing the transaction, so the keeper keeps whatever else it did.
fn admit_taker(
    order: &WorkingOrder,
    taker: &mut Taker,
    state: &State,
    parties: &mut FillParties,
    mode: FillMode,
    rev_share_escrow: &Option<&mut RevenueShareEscrowZeroCopyMut>,
) -> VelocityResult<Admission> {
    validate!(
        order.order.status == OrderStatus::Open,
        ErrorCode::OrderNotOpen,
        "Order not open"
    )?;
    validate!(
        !order.order.must_be_triggered() || order.order.triggered(),
        ErrorCode::OrderMustBeTriggeredFirst,
        "Order must be triggered first"
    )?;

    if taker.user.is_bankrupt() {
        msg!("user is bankrupt");
        return Ok(Admission::Skip);
    }

    if !mode.is_liquidation()
        && validate_user_not_being_liquidated(
            taker.user,
            parties.maps,
            state.liquidation_margin_buffer_ratio,
        )
        .is_err()
    {
        msg!("user is being liquidated");
        return Ok(Admission::Skip);
    }

    require_revenue_share_escrow(order, taker, state, mode, rev_share_escrow)?;
    Ok(Admission::Proceed)
}

/// The taker's `RevenueShareEscrow` is optional. A keeper that omits it
/// resolves the fees that depend on it to zero. Two cases require it:
///
/// 1. the order carries a builder code, so the builder fee must accrue;
/// 2. the taker is referred through an escrow, so the referee discount and
///    referrer reward must apply. `BuilderReferral` is set only when an
///    escrow was initialized with a referrer, and escrows cannot be closed.
///
/// Skipped when builder codes are globally disabled, since the keeper then
/// passes no escrow by design. Also skipped for liquidations, whose order
/// is force-filled without an escrow.
fn require_revenue_share_escrow(
    order: &WorkingOrder,
    taker: &Taker,
    state: &State,
    mode: FillMode,
    rev_share_escrow: &Option<&mut RevenueShareEscrowZeroCopyMut>,
) -> VelocityResult {
    if mode.is_liquidation() || !state.builder_codes_enabled() {
        return Ok(());
    }

    validate!(
        !order.order.is_has_builder() || rev_share_escrow.is_some(),
        ErrorCode::UnableToLoadRevenueShareAccount,
        "Order has builder but no RevenueShareEscrow account was included in the fill"
    )?;
    validate!(
        !ReferrerStatus::has_builder_referral(taker.stats.referrer_status)
            || rev_share_escrow.is_some(),
        ErrorCode::UnableToLoadRevenueShareAccount,
        "User is referred with an escrow but no RevenueShareEscrow account was included in the fill"
    )?;

    Ok(())
}

/// The keeper's two loaded accounts. Both are absent when the filler earns no
/// reward.
type BoundKeeper<'a> = (Option<RefMut<'a, User>>, Option<RefMut<'a, UserStats>>);

/// Load the keeper that runs the fill, when it is a third party.
///
/// A filler that is the taker, one of the makers, or another subaccount of the
/// taker's authority earns no reward and is not loaded: the taker and the
/// makers are already loaded, and loading one of them twice would alias the
/// account.
fn bind_filler<'f, 'info>(
    filler: &'f AccountLoader<'info, User>,
    filler_stats: &'f AccountLoader<'info, UserStats>,
    makers_and_referrer: &UserMap,
    taker: &Taker,
    filler_key: &Pubkey,
) -> VelocityResult<BoundKeeper<'f>> {
    if taker.key == *filler_key || makers_and_referrer.0.contains_key(filler_key) {
        return Ok((None, None));
    }

    let filler = load_mut!(filler)?;
    validate!(
        filler.pool_id == 0,
        ErrorCode::InvalidPoolId,
        "filler pool id ({}) != 0",
        filler.pool_id
    )?;

    if filler.authority == taker.user.authority {
        return Ok((None, None));
    }

    Ok((Some(filler), Some(load_mut!(filler_stats)?)))
}

/// One perp order, as the fill works on it. The account maps, router leg
/// and escrow are arguments, not fields. A field would root a market
/// borrow in `self`, colliding with the taker and filler mutations a step
/// needs.
struct OrderUnderFill<'a> {
    order: WorkingOrder<'a>,
    taker: Taker<'a>,
    filler: Filler<'a>,
    state: &'a State,
    rules: PricingRules<'a>,
    conditions: FillConditions,
    market_index: u16,
}

impl OrderUnderFill<'_> {
    /// Fill the order, then apply the bookkeeping the fill leaves behind.
    fn run(
        &mut self,
        parties: &mut FillParties,
        router: &mut RouterLeg,
        rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
    ) -> VelocityResult<FillAmounts> {
        self.gate_match_fills(router);

        if self.conditions.oracle_too_divergent_with_twap(self.state)? {
            // update filler last active so tx doesn't revert
            if let Some(filler) = self.filler.user.as_deref_mut() {
                filler.update_last_active_slot(self.conditions.slot);
            }

            return Ok(FillAmounts::default());
        }

        self.taker.bind_position(self.market_index)?;
        if self.expire_or_cancel(parties)? == Admission::Skip {
            return Ok(FillAmounts::default());
        }

        let filled = self.fill(parties, router, rev_share_escrow)?;
        self.order.write_back();

        self.record_fill_price(filled, parties)?;
        self.cancel_dangling_trigger_orders(parties)?;
        if filled.base == 0 {
            return Ok(filled);
        }

        self.enforce_open_interest_cap(parties)?;
        self.update_funding(parties)?;
        self.taker
            .user
            .update_last_active_slot(self.conditions.slot);
        Ok(filled)
    }

    /// Withhold every maker-priced source the oracle does not admit.
    ///
    /// An external quoter book executes at its maker's price with no auction
    /// protection, so a NonPositive, TooVolatile or TooUncertain oracle blocks
    /// it the way the AMM's own gates block an AMM fill. `OracleOrderPrice` is
    /// weaker and only decides whether an oracle-relative limit resolves. The
    /// vAMM keeps its own inclusion gate.
    fn gate_match_fills(&mut self, router: &mut RouterLeg) {
        let taker_can_match = self.taker_admits_match();
        if self.conditions.safe_match_fills_allowed && taker_can_match {
            return;
        }
        if !router.books.is_empty() {
            msg!(
                "Perp market = {} oracle not valid for match fills (safe={}, taker_exchange={})",
                self.market_index,
                self.conditions.safe_match_fills_allowed,
                taker_can_match,
            );
        }

        router.books = &[];
    }

    /// The exchange oracle values the equity floor. An MM oracle may still
    /// quote the AMM, but cannot authorize a floored taker to match against
    /// a book while the exchange oracle is invalid. This gate applies to
    /// match fills only, not AMM fills.
    fn taker_admits_match(&self) -> bool {
        self.taker.user.equity_floor == 0 || self.conditions.exchange_match_fills_allowed
    }

    /// Expire or cancel the order instead of filling it.
    ///
    /// An expired order, and a reduce-only order that would increase the
    /// position, both leave the book here.
    fn expire_or_cancel(&mut self, parties: &mut FillParties) -> VelocityResult<Admission> {
        let should_expire = should_expire_order(&self.order.order, self.conditions.now)?;
        let should_cancel_reduce_only = self.should_cancel_reduce_only(parties)?;
        if !should_expire && !should_cancel_reduce_only {
            return Ok(Admission::Proceed);
        }

        Ok(Admission::Skip)
    }

    /// Whether the order is reduce-only and would increase the position it
    /// settles into.
    fn should_cancel_reduce_only(&self, parties: &FillParties) -> VelocityResult<bool> {
        let step_size = parties
            .maps
            .perp_market_map
            .get_ref(&self.market_index)?
            .order_step_size;
        should_cancel_reduce_only_order(
            &self.order.order,
            self.taker.position()?.base_asset_amount,
            step_size,
        )
    }

    /// Hold the blended fill price to the market's price band, and record it
    /// as the market's last fill.
    fn record_fill_price(
        &mut self,
        filled: FillAmounts,
        parties: &mut FillParties,
    ) -> VelocityResult {
        if filled.base == 0 {
            return Ok(());
        }

        let fill_price = calculate_fill_price(filled.quote, filled.base, BASE_PRECISION_U64)?;
        let mut market = parties
            .maps
            .perp_market_map
            .get_ref_mut(&self.market_index)?;
        validate_fill_price_within_price_bands(
            fill_price,
            self.conditions.oracle_price,
            self.conditions.oracle_twap_5min,
            market.margin_ratio_initial,
            self.state
                .oracle_guard_rails
                .max_oracle_twap_5min_percent_divergence(),
            None,
        )?;

        market.last_fill_price = fill_price;
        Ok(())
    }

    /// Hand the order to the taker's risk limits, which fill it.
    ///
    /// The fill takes the order itself rather than a slot index. `Order` is
    /// `Copy` and 104 bytes, so the copy costs little, and an order that lives
    /// in no slot fills through the same path.
    fn fill(
        &mut self,
        parties: &mut FillParties,
        router: &mut RouterLeg,
        rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
    ) -> VelocityResult<FillAmounts> {
        let mut taker = TakerSide::bind(
            self.taker.user,
            self.taker.stats,
            self.taker.key,
            &mut self.order.order,
            self.order.reserved,
        )?;

        fill_within_taker_risk_limits(
            &mut taker,
            &self.rules,
            &self.conditions,
            parties,
            &mut OfferedLiquidity { router },
            &mut FillerSide {
                user: &mut self.filler.user,
                stats: &mut self.filler.stats,
                key: self.filler.key,
                rev_share_escrow,
            },
        )
    }

    /// Cancel the reduce-only trigger orders a flat position leaves behind.
    fn cancel_dangling_trigger_orders(&mut self, parties: &mut FillParties) -> VelocityResult {
        let position = self.taker.position()?;
        if position.base_asset_amount != 0 || position.open_asks != 0 || position.open_bids != 0 {
            return Ok(());
        }

        cancel_reduce_only_trigger_orders(
            self.taker.user,
            &self.taker.key,
            Some(&self.filler.key),
            parties.maps,
            self.conditions.now,
            self.conditions.slot,
            self.market_index,
        )
    }

    /// The market's open-interest cap holds after the fill.
    fn enforce_open_interest_cap(&self, parties: &FillParties) -> VelocityResult {
        let market = parties.maps.perp_market_map.get_ref(&self.market_index)?;
        let open_interest = market.get_open_interest();
        let max_open_interest = market.max_open_interest;
        validate!(
            max_open_interest == 0 || max_open_interest > open_interest,
            ErrorCode::MaxOpenInterest,
            "open interest ({}) > max open interest ({})",
            open_interest,
            max_open_interest
        )?;

        Ok(())
    }

    /// Try to update the funding rate at the end of every trade.
    ///
    /// The reserve price is passed as `None`, so the funding update recomputes
    /// it from the AMM after the fill. The fills just moved the reserves. The
    /// mark-to-oracle divergence check, and the oracle-TWAP sanitization that
    /// reads the same value, would test a stale price if they ran on the
    /// pre-fill mark. A fill that pushes the mark past the divergence band
    /// could then still update funding, or a funding update the post-fill mark
    /// no longer warrants could be blocked.
    fn update_funding(&mut self, parties: &mut FillParties) -> VelocityResult {
        let market = &mut parties
            .maps
            .perp_market_map
            .get_ref_mut(&self.market_index)?;
        let funding_paused = self.state.funding_paused()?
            || market.is_operation_paused(PerpOperation::UpdateFunding);
        controller::funding::update_funding_rate(
            self.market_index,
            market,
            &mut parties.maps.oracle_map,
            self.conditions.now,
            self.conditions.slot,
            &self.state.oracle_guard_rails,
            funding_paused,
            None,
        )?;

        Ok(())
    }
}

impl FillConditions {
    /// Read the market oracle and decide what this fill may use it for.
    ///
    /// This advances the market's own oracle bookkeeping: the TWAPs, the
    /// reference-price offset and `last_oracle_valid`. It does not touch the
    /// AMM. The liquidity pass builds an `AmmQuoter` and refreshes it before
    /// it quotes, which is the only AMM refresh outside the admin paths.
    pub(super) fn read(
        state: &State,
        maps: &mut AccountMaps,
        taker: &Taker,
        slot_order: &WorkingOrder,
        mode: FillMode,
        clock: &Clock,
    ) -> VelocityResult<Self> {
        let (now, slot) = (clock.unix_timestamp, clock.slot);
        let order = &slot_order.order;
        let market_index = order.market_index;
        let amm_not_globally_paused = !state.amm_paused()?;

        let market = &mut maps.perp_market_map.get_ref_mut(&market_index)?;
        validation::perp_market::validate_perp_market(market)?;
        validate!(
            !market.is_in_settlement(now),
            ErrorCode::MarketFillOrderPaused,
            "Market is in settlement mode",
        )?;

        let oracle_price_data = *maps.oracle_map.get_price_data(&market.oracle_id())?;
        let exchange_validity = exchange_oracle_validity(market, state, &oracle_price_data, slot)?;
        let (mm_oracle_price_data, safe_validity) =
            safe_mm_oracle_state(market, state, &oracle_price_data, slot)?;

        let can_skip_duration = taker
            .user
            .can_skip_auction_duration(taker.stats, slot_order.reduce_only_at_entry)?;
        let amm_can_fill = market.amm_can_fill_order(
            order,
            slot,
            mode,
            state,
            safe_validity,
            can_skip_duration,
            &mm_oracle_price_data,
        )?;
        let oracle_stale_for_margin = state
            .slot_clock()
            .elapsed_slot_delta(mm_oracle_price_data.get_delay().max(0) as u64, slot)
            > state.oracle_guard_rails.validity.stale_for_margin_ms();
        let oracle_twap_5min =
            refresh_market_oracle_stats(market, state, &mm_oracle_price_data, clock)?;
        let oracle_price = mm_oracle_price_data.get_price();

        Ok(Self {
            mode,
            now,
            slot,
            oracle_price,
            oracle_twap_5min,
            valid_oracle_price: limit_price_oracle(safe_validity, oracle_price, market_index)?,
            amm_is_available: amm_not_globally_paused && amm_can_fill,
            oracle_stale_for_margin,
            safe_match_fills_allowed: is_oracle_valid_for_action(
                safe_validity,
                Some(VelocityAction::FillOrderMatch),
            )?,

            exchange_match_fills_allowed: is_oracle_valid_for_action(
                exchange_validity,
                Some(VelocityAction::FillOrderMatch),
            )?,
        })
    }
}

/// Advance the market's own oracle bookkeeping, and report the 5-minute oracle
/// TWAP as it stood before that.
///
/// The TWAP is read before the refresh. This fill's own band checks,
/// `is_oracle_too_divergent_with_twap_5min` and
/// `validate_fill_price_within_price_bands`, both measure against it, and the
/// refresh pulls it toward the live oracle price. Reading it after the refresh
/// lets a divergent oracle normalize itself inside the same instruction and
/// clear the checks that are meant to stop the fill.
///
/// The refresh itself stays. A fill is one of the paths that advances the
/// TWAPs, and it does not gate on the refreshed value.
fn refresh_market_oracle_stats(
    market: &mut PerpMarket,
    state: &State,
    mm_oracle_price_data: &crate::state::oracle::MMOraclePriceData,
    clock: &Clock,
) -> VelocityResult<i64> {
    let twap_5min = market
        .market_stats
        .historical_oracle_data
        .last_oracle_price_twap_5min;
    let amm_refresh_validity =
        crate::vlp::amm::refresh::compute_amm_refresh_validity_with_guard_rails(
            market,
            mm_oracle_price_data,
            &state.oracle_guard_rails.validity,
            clock.slot,
            state.slot_clock(),
        )?;
    market.update_oracle_derived_stats(
        mm_oracle_price_data,
        amm_refresh_validity,
        clock.unix_timestamp,
        clock.slot,
        state.slot_clock(),
    )?;

    Ok(twap_5min)
}

/// The oracle price an oracle-relative limit resolves against.
///
/// `None` when the oracle is not valid for that action.
fn limit_price_oracle(
    safe_validity: OracleValidity,
    oracle_price: i64,
    market_index: u16,
) -> VelocityResult<Option<i64>> {
    if is_oracle_valid_for_action(safe_validity, Some(VelocityAction::OracleOrderPrice))? {
        return Ok(Some(oracle_price));
    }

    msg!("Perp market = {} oracle deemed invalid", market_index);
    Ok(None)
}

/// The raw exchange oracle's own validity, which only the order layer reads.
///
/// A maker match is gated on the raw feed as well as on the safe one.
fn exchange_oracle_validity(
    market: &PerpMarket,
    state: &State,
    oracle_price_data: &OraclePriceData,
    slot: u64,
) -> VelocityResult<OracleValidity> {
    oracle_validity(
        MarketType::Perp,
        market.market_index,
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap,
        oracle_price_data,
        &state.oracle_guard_rails.validity,
        market.get_max_confidence_interval_multiplier()?,
        &market.oracle_source,
        oracle::LogMode::ExchangeOracle,
        market.oracle_slot_delay_override,
        false,
        market.oracle_low_risk_slot_delay_override,
        slot,
        state.slot_clock(),
    )
}
