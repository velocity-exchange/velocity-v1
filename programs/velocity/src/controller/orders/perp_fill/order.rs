//! The order layer of a perp fill.
//!
//! This layer governs the order. It resolves the order from its owner's slot
//! or from the caller's handle, admits or refuses the fill, refreshes the
//! market oracle statistics, binds the keeper, and applies the bookkeeping the
//! fill leaves behind: the write-back, the fill-price band, the reduce-only
//! cancel, the open-interest cap and the funding update.
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
            router::RouterFillInputs, safe_unwrap::SafeUnwrap,
        },
        print_error,
        state::{
            events::OrderActionExplanation,
            fill_mode::FillMode,
            market_status::MarketStatus,
            paused_operations::PerpOperation,
            revenue_share::RevenueShareEscrowZeroCopyMut,
            state::State,
            user::{MarketType, Order, OrderStatus, ReferrerStatus, User, UserStats},
            user_map::{UserMap, UserStatsMap},
        },
        validate,
    },
    anchor_lang::prelude::{msg, Clock, Pubkey},
    std::{cell::RefMut, ops::DerefMut},
};

/// [`fill_perp_order`] with no external quoter books. The route still runs
/// over the vAMM ladder and the passed DLOB makers. The split has no CPI book
/// to price in. This is every fill entrypoint that carries no quoter accounts
/// (place-and-take flows; external books there are a planned follow-up).
pub fn fill_perp_order_without_external_books<'info>(
    order_id: u32,
    state: &State,
    user: &AccountLoader<'info, User>,
    user_stats: &AccountLoader<'info, UserStats>,
    maps: &mut AccountMaps,
    filler: &AccountLoader<'info, User>,
    filler_stats: &AccountLoader<'info, UserStats>,
    makers_and_referrer: &UserMap,
    makers_and_referrer_stats: &UserStatsMap,
    clock: &Clock,
    fill_mode: FillMode,
    rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut<'info>>,
    referrer_is_accelerated: bool,
) -> VelocityResult<(u64, u64)> {
    let mut no_externals = crate::state::prop_amm::NoExternalQuoters;
    let mut router_inputs = crate::math::router::RouterFillInputs {
        books: &[],
        executor: &mut no_externals,
        protocol_authority: state.signer,
        taker_exposure_closed_by_caller: false,
        // This path carries no external book, so nothing can withhold depth
        // and the obligation is never reached.
        obligation: crate::math::router::FillerObligation::default(),
        worst_fill_price: None,
    };
    fill_perp_order(
        FillRequest {
            target: FillTarget::Slot(order_id),
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

/// Which order a fill is for, and whether the owner has a slot holding it.
///
/// A fill works on the order itself. Most orders live in their owner's `orders`
/// array and the fill reads one out and writes it back; a remainder lifted off
/// a book lives nowhere, and passing it directly is what lets the same fill
/// path serve both without either one needing a spare slot.
pub enum FillTarget<'a> {
    /// The open order with this id in the owner's `orders` array.
    Slot(u32),
    /// An order held by the caller. Nothing is written back: the caller owns
    /// what happens to whatever the fill leaves unfilled.
    ///
    /// `reserved` says whether the taker owns an `open_bids`/`open_asks` +
    /// `open_orders` reservation the fill must unwind as it fills. An order
    /// lifted off the book rested first, so it reserved: `reserved: true`. A
    /// fresh ephemeral taker that routes straight to the book never reserved:
    /// `reserved: false`, and the fill must not unwind exposure it never took,
    /// or it eats a co-resident order's reservation and underflows the counter.
    Detached {
        order: &'a mut Order,
        reserved: bool,
    },
}

/// What one fill is asked to do.
pub struct FillRequest<'a> {
    /// Which order to fill, and where it lives.
    pub target: FillTarget<'a>,
    pub mode: FillMode,
    /// Whether the taker's referrer is on the accelerated schedule.
    pub referrer_is_accelerated: bool,
}

/// The two seats a perp fill loads for itself: whose order it is, and who
/// turns it. Everyone else the fill touches arrives in [`FillParties`].
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

    /// Bind the position this order settles into.
    ///
    /// An ephemeral taker holds only the empty position `build_perp_order`
    /// added, which `get_position_index` skips as available.
    /// `add_new_position` reuses that same slot, so the fill settles into it.
    /// A slot order always has a findable position from its placement, so the
    /// fallback never fires for one.
    ///
    /// Binding can add a position, so it runs only once the fill is admitted.
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

/// The keeper that turns a fill, and what the flat reward is worth.
///
/// Both accounts are absent when the filler is the taker, one of the makers,
/// or another subaccount of the taker's authority. Such a filler earns no
/// reward and is never loaded a second time.
pub(super) struct KeeperSide<'a> {
    pub user: Option<&'a mut User>,
    pub stats: Option<&'a mut UserStats>,
    pub key: Pubkey,
    /// The flat reward the keeper earns for expiring or cancelling the order.
    pub flat_filler_fee: u64,
}

/// The order a fill works on, and where it goes back to.
///
/// `Order` is `Copy` and 104 bytes, so the fill works on this copy. A slot
/// order is read out of the owner's `orders` array and written back there. A
/// detached order is the caller's, and is written back through the handle the
/// caller passed.
pub(super) struct OrderSlot<'a> {
    pub order: Order,
    /// The index in the owner's `orders` array, when the order lives in one.
    index: Option<usize>,
    /// The caller's handle, when the order lives nowhere else.
    detached: Option<&'a mut Order>,
    /// Whether the taker owns an `open_bids`/`open_asks` + `open_orders`
    /// reservation the fill must unwind as it fills. A slot order reserved at
    /// placement. A detached order says for itself.
    pub reserved: bool,
    /// The `reduce_only` flag the order was read with, before a reduce-only
    /// market stamped the flag on. The auction-duration skip reads the flag
    /// the order was placed with.
    reduce_only_at_entry: bool,
}

impl<'a> OrderSlot<'a> {
    /// Find the order this fill is for.
    fn resolve(target: FillTarget<'a>, user: &User) -> VelocityResult<Self> {
        let mut detached = None;
        let (index, reserved, order) = match target {
            FillTarget::Slot(order_id) => {
                let index = user
                    .orders
                    .iter()
                    .position(|order| {
                        order.order_id == order_id && order.status == OrderStatus::Open
                    })
                    .ok_or_else(print_error!(ErrorCode::OrderDoesNotExist))?;
                (Some(index), true, user.orders[index])
            }
            FillTarget::Detached { order, reserved } => {
                let snapshot = *order;
                detached = Some(order);
                (None, reserved, snapshot)
            }
        };
        validate!(
            order.market_type == MarketType::Perp,
            ErrorCode::InvalidOrderMarketType,
            "must be perp order"
        )?;
        Ok(Self {
            reduce_only_at_entry: order.reduce_only,
            order,
            index,
            detached,
            reserved,
        })
    }

    /// The slot index. Only a slot order can be cancelled, so only a slot
    /// order reaches a caller that needs one.
    fn index(&self) -> VelocityResult<usize> {
        self.index.safe_unwrap()
    }

    /// Put the fill's progress back where the order came from, so everything
    /// downstream — the reduce-only check, and the caller's own lookup by
    /// order id — sees what the fill did to it.
    fn write_back(&mut self, user: &mut User) {
        let order = self.order;
        if let Some(index) = self.index {
            user.orders[index] = order;
        }
        if let Some(detached) = self.detached.as_deref_mut() {
            *detached = order;
        }
    }
}

/// Fill one perp order, from the owner's slot or from the caller (see
/// [`FillTarget`]).
///
/// This layer governs the order. It resolves the order, admits or refuses the
/// fill, refreshes the market oracle statistics, binds the keeper and collects
/// the DLOB maker orders the fill may match. [`OrderUnderFill`] carries all of
/// that through the fill and the bookkeeping it leaves behind.
pub fn fill_perp_order(
    request: FillRequest<'_>,
    state: &State,
    clock: &Clock,
    accounts: PerpFillAccounts<'_, '_, '_>,
    parties: &mut FillParties,
    router: &mut RouterFillInputs,
) -> VelocityResult<(u64, u64)> {
    let filler_key = accounts.filler.key();
    let user_key = accounts.user.key();
    let mut user = load_mut!(accounts.user)?;
    let mut user_stats = load_mut!(accounts.user_stats)?;
    let mut taker = Taker::new(&mut user, &mut user_stats, user_key);
    let terms = FillTerms::of(state, request.referrer_is_accelerated);

    let mut order = OrderSlot::resolve(request.target, taker.user)?;
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
        return Ok((0, 0));
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
        keeper: KeeperSide {
            user: filler.as_deref_mut(),
            stats: filler_stats.as_deref_mut(),
            key: filler_key,
            flat_filler_fee: terms.fee_structure.flat_filler_fee,
        },
        state,
        terms,
        conditions,
        market_index,
    }
    .run(parties, router, rev_share_escrow)
}

/// The market admits the fill.
///
/// A `ReduceOnly` market forces every order it fills to be risk-reducing.
/// Placement only stamps `order.reduce_only` from the market status at the time
/// the order was created (`place_perp_order` -> `force_reduce_only`), so a
/// legacy order placed while the market was `Active` still carries
/// `reduce_only = false` after the market is flipped to `ReduceOnly`. Every
/// downstream reduce-only guard keys off the stored flag — the fill-size clamp
/// in `get_base_asset_amount_unfilled`, `should_cancel_reduce_only_order` and
/// the trigger-path risk check — so the flag is re-derived from the live market
/// status here and stamped onto the order. This mirrors placement: once a
/// market is reduce-only, its orders are reduce-only.
fn admit_perp_market(
    order: &mut OrderSlot,
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
    order: &OrderSlot,
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

/// The taker's `RevenueShareEscrow` is an optional account, so a keeper could
/// omit it and the associated fees would silently resolve to zero. Two cases
/// require it to be supplied:
///
/// 1. the taker order carries a builder code, so the builder fee must accrue;
/// 2. the taker is referred and their escrow exists, so the referee discount
///    and the referrer reward must apply. `BuilderReferral` is set only when an
///    escrow was initialized with a referrer, and escrows cannot be closed.
///
/// Skipped when the builder-codes feature is globally disabled, because the
/// keeper then passes no escrow by design, and skipped for liquidations,
/// because the liquidatee's order is force-filled without an escrow.
fn require_revenue_share_escrow(
    order: &OrderSlot,
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

/// Load the keeper that turns the fill, when it is a third party.
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

/// One perp order, as the fill works on it.
///
/// This is the order layer's own subject: the order and where it goes back to,
/// the taker that owns it, the keeper that turns the fill, and the rules the
/// three run under. The accounts arrive per step, because every other layer
/// takes them the same way and a field would tie this context's life to
/// theirs. [`Self::run`] names the steps and they run in the order they are
/// written.
struct OrderUnderFill<'a> {
    order: OrderSlot<'a>,
    taker: Taker<'a>,
    keeper: KeeperSide<'a>,
    state: &'a State,
    terms: FillTerms<'a>,
    conditions: FillConditions,
    market_index: u16,
}

impl OrderUnderFill<'_> {
    /// Fill the order, then apply the bookkeeping the fill leaves behind.
    fn run(
        &mut self,
        parties: &mut FillParties,
        router: &mut RouterFillInputs,
        rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
    ) -> VelocityResult<(u64, u64)> {
        let mut dlob_makers = self.discover_dlob_makers(parties)?;
        self.gate_match_fills(&mut dlob_makers, router);

        if self.conditions.oracle_too_divergent_with_twap(self.state)? {
            // update filler last active so tx doesn't revert
            if let Some(filler) = self.keeper.user.as_deref_mut() {
                filler.update_last_active_slot(self.conditions.slot);
            }
            return Ok((0, 0));
        }

        self.taker.bind_position(self.market_index)?;
        if self.expire_or_cancel(parties)? == Admission::Skip {
            return Ok((0, 0));
        }

        let filled = self.fill(&dlob_makers, parties, router, rev_share_escrow)?;
        self.order.write_back(self.taker.user);

        self.record_fill_price(filled, parties)?;
        self.cancel_reduce_only_after_fill(parties)?;
        self.cancel_dangling_trigger_orders(parties)?;
        if filled.base == 0 {
            return Ok((filled.base, filled.quote));
        }

        self.enforce_open_interest_cap(parties)?;
        self.update_funding(parties)?;
        self.taker
            .user
            .update_last_active_slot(self.conditions.slot);
        Ok((filled.base, filled.quote))
    }

    /// Every DLOB maker order this fill may match, best price first.
    fn discover_dlob_makers(
        &mut self,
        parties: &mut FillParties,
    ) -> VelocityResult<Vec<MakerOrderInfo>> {
        get_maker_orders_info(
            parties.maps,
            parties.makers_and_referrer,
            &mut self.keeper.user,
            &MakerSearch {
                taker_key: &self.taker.key,
                taker_order: &self.order.order,
                maker_direction: self.order.order.direction.opposite(),
                filler_key: &self.keeper.key,
                filler_reward: self.keeper.flat_filler_fee,
                oracle_price: self.conditions.oracle_price,
                exchange_match_fills_allowed: self.conditions.exchange_match_fills_allowed,
                now: self.conditions.now,
                slot: self.conditions.slot,
            },
        )
    }

    /// Withhold every maker-priced source the oracle does not admit.
    ///
    /// A DLOB match and an external quoter book both execute at their maker's
    /// price with no auction protection, so a NonPositive, TooVolatile or
    /// TooUncertain oracle blocks them the way the AMM's own gates block an
    /// AMM fill. `OracleOrderPrice` is weaker and only decides whether an
    /// oracle-relative limit resolves. The vAMM keeps its own inclusion gate.
    ///
    /// Runs after maker discovery so its expired-maker-order cleanup still
    /// happens. Only the matching itself is withheld.
    fn gate_match_fills(
        &mut self,
        dlob_makers: &mut Vec<MakerOrderInfo>,
        router: &mut RouterFillInputs,
    ) {
        let taker_can_match = can_floored_user_match_with_exchange_oracle(
            self.taker.user,
            self.conditions.exchange_match_fills_allowed,
        );
        if self.conditions.safe_match_fills_allowed && taker_can_match {
            return;
        }
        if !dlob_makers.is_empty() {
            msg!(
                "Perp market = {} oracle not valid for match fills (safe={}, taker_exchange={})",
                self.market_index,
                self.conditions.safe_match_fills_allowed,
                taker_can_match,
            );
            dlob_makers.clear();
        }
        router.books = &[];
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
        let explanation = if should_expire {
            OrderActionExplanation::OrderExpired
        } else {
            OrderActionExplanation::ReduceOnlyOrderIncreasedPosition
        };
        self.cancel_and_reward(explanation, parties)?;
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

    /// Pay the keeper the flat reward for the work, then cancel the order.
    ///
    /// Only a slot order can be reduce-only or expire under a keeper, so only
    /// a slot order reaches here.
    fn cancel_and_reward(
        &mut self,
        explanation: OrderActionExplanation,
        parties: &mut FillParties,
    ) -> VelocityResult {
        let filler_reward = {
            let mut market = parties
                .maps
                .perp_market_map
                .get_ref_mut(&self.market_index)?;
            pay_keeper_flat_reward_for_perps(
                self.taker.user,
                self.keeper.user.as_deref_mut(),
                market.deref_mut(),
                self.keeper.flat_filler_fee,
                self.conditions.slot,
            )?
        };
        cancel_order(
            self.order.index()?,
            self.taker.user,
            &self.taker.key,
            parties.maps,
            self.conditions.now,
            self.conditions.slot,
            explanation,
            Some(&self.keeper.key),
            filler_reward,
            false,
        )
    }

    /// Hand the order to the taker's risk limits, which fill it.
    ///
    /// The fill takes the order itself, not a slot index — `Order` is `Copy`
    /// and 104 bytes, so this costs nothing and it is what lets an order that
    /// lives nowhere (a remainder lifted off a book) be filled by the same
    /// path.
    fn fill(
        &mut self,
        dlob_makers: &[MakerOrderInfo],
        parties: &mut FillParties,
        router: &mut RouterFillInputs,
        rev_share_escrow: &mut Option<&mut RevenueShareEscrowZeroCopyMut>,
    ) -> VelocityResult<FillAmounts> {
        let mut taker = TakerSide::bind(
            self.taker.user,
            self.taker.stats,
            self.taker.key,
            &mut self.order.order,
            self.order.reserved,
        )?;
        let (base, quote) = fill_within_taker_risk_limits(
            &mut taker,
            &self.terms,
            &self.conditions,
            parties,
            &mut OfferedLiquidity {
                dlob_makers,
                router,
            },
            &mut FillerSide {
                user: &mut self.keeper.user,
                stats: &mut self.keeper.stats,
                key: self.keeper.key,
                rev_share_escrow,
            },
        )?;
        Ok(FillAmounts { base, quote })
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

    /// Cancel a reduce-only order the fill left pointing the wrong way.
    fn cancel_reduce_only_after_fill(&mut self, parties: &mut FillParties) -> VelocityResult {
        if !self.should_cancel_reduce_only(parties)? {
            return Ok(());
        }
        self.cancel_and_reward(
            OrderActionExplanation::ReduceOnlyOrderIncreasedPosition,
            parties,
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
            Some(&self.keeper.key),
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
    /// The reserve price is passed as `None` so the funding update recomputes
    /// it from the POST-fill AMM. The fills just moved the reserves, so gating
    /// the mark/oracle divergence check — and the oracle-TWAP sanitization
    /// that shares this value — on the pre-fill mark would test a stale price.
    /// That lets a fill which pushes the mark past the divergence band still
    /// update funding, or blocks a funding update the post-fill mark no longer
    /// warrants.
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
    /// The market's own oracle bookkeeping — the TWAPs, the reference-price
    /// offset and `last_oracle_valid` — is advanced here. The AMM is not
    /// touched: the liquidity pass builds an `AmmQuoter` and refreshes it
    /// before it quotes, which is the only non-admin AMM refresh.
    pub(super) fn read(
        state: &State,
        maps: &mut AccountMaps,
        taker: &Taker,
        slot_order: &OrderSlot,
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
/// The TWAP is snapshotted *before* the refresh because this fill's own band
/// checks — `is_oracle_too_divergent_with_twap_5min` and
/// `validate_fill_price_within_price_bands` — both measure against it, and the
/// refresh pulls it toward the live oracle price. Reading it afterwards let a
/// currently-divergent oracle normalize itself inside the same instruction and
/// clear the very checks meant to stop the fill.
///
/// Unlike the funding crank, the refresh itself stays. A fill is one of the
/// paths that legitimately advances the TWAPs, and it does not gate on them,
/// so snapshotting the reader is the whole fix.
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
/// `None` when the oracle is not valid for it. Allow the oracle price to be
/// used to calculate a limit price if it is valid, or stale for the AMM.
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
/// A DLOB maker match is gated on the raw feed as well as on the safe one.
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
