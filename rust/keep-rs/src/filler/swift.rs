use {
    crate::{
        common::{
            metrics::{FeedHealth, Metrics},
            tx::{scale_cu_limit_for_accounts, with_spot_interest_cranks, TxIntent},
        },
        filler::{
            market::{AmmGate, MarketView},
            Filler, SlotTick, TARGET,
        },
    },
    std::time::Duration,
    velocity_rs::{
        dlob::{MakerCrosses, TakerOrder, DLOB},
        program::{
            math::{
                auction::calculate_auction_price,
                time::{Millis, SlotClock},
            },
            state::user::OrderBitFlag,
        },
        swift_order_subscriber::{SignedOrderInfo, SwiftOrderStream},
        types::{
            accounts::{User, UserStats},
            MarketId, MarketPrecision, Order, OrderParams, OrderParamsExt, OrderType,
            PositionDirection, PostOnlyParam,
        },
        VelocityClient, Wallet,
    },
};

/// Max age of a swift signed message before the program refuses to place it
/// (~200s, expressed in actual slots at the current slot duration).
///
/// Mirrors the staleness gate in `place_signed_msg_taker_order`
/// (programs/velocity/src/instructions/keeper.rs).
pub const SWIFT_SIGNED_MSG_MAX_AGE: Millis = Millis::from_secs(200);

/// Max lead of a resting swift limit's message slot over the current slot before the
/// program refuses to place it early (~30s; the UI stamps ~14s ahead).
///
/// Mirrors `max_resting_limit_lead` in `place_signed_msg_taker_order`.
pub const SWIFT_RESTING_LIMIT_MAX_LEAD: Millis = Millis::from_secs(30);

/// How to treat a swift order whose signed message may be stamped ahead of the chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwiftSlotWait {
    /// The message slot has arrived: the order can be filled or placed now.
    Ready,
    /// Stamped ahead of the chain, within a credible signing buffer: hold the order and
    /// re-evaluate once its slot arrives.
    Wait,
    /// Stamped so far ahead it cannot be a signing buffer: don't hold it.
    TooFarAhead,
}

/// True if a swift order is a limit order with no auction. It rests from placement, so the
/// program treats its message slot as a placement deadline rather than an auction start
/// and accepts it ahead of that slot (see [`swift_slot_wait`]).
pub fn is_resting_swift_limit(order_params: &OrderParams) -> bool {
    order_params.order_type == OrderType::Limit && order_params.auction_duration.unwrap_or(0) == 0
}

/// Classify a swift order's signed-message slot against the current slot.
///
/// For an auction order `place_signed_msg_taker_order` rejects `order_slot > clock.slot`
/// (`InvalidSignedMsgOrderParam`), so one stamped ahead of the chain can be neither
/// filled nor placed yet. Signers add a buffer so the message stays valid while it travels
/// (the UI stamps a few slots ahead), which makes this a normal arrival state rather than a
/// bad order: the swift feed delivers each order exactly once, so the only way not to lose
/// it is to hold it until its slot arrives. `max_wait` bounds how far ahead a stamp is
/// still credible as a signing buffer.
///
/// A resting limit (`resting_limit`, see [`is_resting_swift_limit`]) has no auction to
/// start: its message slot is the placement deadline (`max_slot`), stamped a whole signing
/// budget ahead, and the program places it before that slot as long as the stamp is within
/// [`SWIFT_RESTING_LIMIT_MAX_LEAD`]. Waiting would leave a single slot to land the tx, so it
/// is ready on arrival.
pub fn swift_slot_wait(
    order_slot: u64,
    current_slot: u64,
    max_wait: Millis,
    resting_limit: bool,
    slot_clock: SlotClock,
) -> SwiftSlotWait {
    if order_slot <= current_slot {
        return SwiftSlotWait::Ready;
    }
    let lead = slot_clock.elapsed(current_slot, order_slot);
    if resting_limit {
        return if lead > SWIFT_RESTING_LIMIT_MAX_LEAD {
            SwiftSlotWait::TooFarAhead
        } else {
            SwiftSlotWait::Ready
        };
    }
    if lead > max_wait {
        return SwiftSlotWait::TooFarAhead;
    }
    SwiftSlotWait::Wait
}

/// Classify a Swift order only once the chain slot is known. Startup RPC failure
/// is not evidence that a production-scale order slot is implausibly far ahead,
/// so the safe state is to hold the order until the slot feed produces a value.
pub fn swift_slot_wait_if_known(
    order_slot: u64,
    current_slot: Option<u64>,
    max_wait: Millis,
    resting_limit: bool,
    slot_clock: SlotClock,
) -> SwiftSlotWait {
    match current_slot {
        Some(current_slot) => swift_slot_wait(
            order_slot,
            current_slot,
            max_wait,
            resting_limit,
            slot_clock,
        ),
        None => SwiftSlotWait::Wait,
    }
}

/// Whether the biased filler select should poll Swift this iteration. When both
/// streams stay ready, `prefer_slot` alternates after each successful slot/Swift
/// poll so neither a slot backlog nor a busy Swift feed can starve the other.
pub fn should_poll_swift(
    swift_feed_live: bool,
    slot_update_pending: bool,
    prefer_slot: bool,
) -> bool {
    swift_feed_live && !(slot_update_pending && prefer_slot)
}

/// Returns true if a swift (signed-message) order is too old to be usefully filled or placed
/// on-chain, so the bot shouldn't spend a tx on it.
///
/// The two slot gates mirror `place_signed_msg_taker_order` exactly:
/// - **signed message staleness**: the program rejects once the order's
///   wall clock age (integrated per slot duration regime) exceeds ~200s
/// - **placement deadline**: program silently no-ops once `max_slot < current_slot`, where
///   `max_slot` is the first slot reaching the auction duration across all
///   known slot-duration transitions (identical formula for limit & market orders)
///
/// The `max_ts` check is an *additional* client-side guard (the program does not gate placement
/// on `max_ts`): an order whose `max_ts` has passed is already dead, so placing it would waste a
/// tx. Note `auction_duration` is a `u8` (≤ 255 units ≈ 102s), so the placement deadline always
/// binds before the ~200s staleness window; both are checked for completeness/robustness.
///
/// The opposite end of the window, an order stamped *ahead* of the chain, is
/// [`swift_slot_wait`]'s job: that order is not dead but early, and is held rather than dropped.
/// This function reports "not expired" for one, so callers must run the wait gate first.
pub fn swift_order_expired(
    order_slot: u64,
    auction_duration: u8,
    max_ts: i64,
    current_slot: u64,
    now_ts: i64,
    slot_clock: SlotClock,
) -> bool {
    // signed message too old for the program to accept
    if slot_clock.elapsed(order_slot, current_slot) > SWIFT_SIGNED_MSG_MAX_AGE {
        return true;
    }
    // placement deadline: program no-ops once max_slot < current_slot
    let max_slot = slot_clock.slot_at_or_after_duration(
        order_slot,
        Millis::from_stored_units(auction_duration as u64),
    );
    if current_slot > max_slot {
        return true;
    }
    // order-level timestamp expiry
    if max_ts != 0 && now_ts > max_ts {
        return true;
    }
    false
}

/// An auction order stamped further ahead than this is not a signing buffer (the UI's is a few
/// slots), so it is refused rather than held. A resting limit is never held: see
/// `swift_slot_wait`.
const MAX_SWIFT_ORDER_DEFERRAL: Millis = Millis::from_secs(10);
/// Cap on held orders, so a stuck slot feed cannot grow the queue without bound.
const MAX_DEFERRED_SWIFT_ORDERS: usize = 1_024;
/// A half-open ws never yields an error or `None`: the stream goes quiet, which looks the same
/// as a quiet market because the SDK consumes the server heartbeats. Reconnecting is cheap, so
/// after this much silence the stream is torn down and resubscribed.
const SWIFT_FEED_STALE_LIMIT: Duration = Duration::from_secs(300);

/// The swift order stream with its reconnect state and the orders waiting to be processed.
///
/// The feed delivers each order exactly once, so an order the program cannot accept yet (its
/// message slot is still ahead of the chain) is held in `deferred` until its slot arrives.
pub(super) struct SwiftFeed {
    pub stream: SwiftOrderStream,
    market_ids: Vec<MarketId>,
    /// A disconnected stream stays ready forever, and the run loop's select is biased, so
    /// polling it would starve every other arm. The arm is gated on this instead, and the
    /// reconnect runs on its own timer.
    pub live: bool,
    retries: u32,
    pub reconnect_at: tokio::time::Instant,
    last_message: std::time::Instant,
    /// Orders whose message slot has not arrived yet.
    deferred: Vec<SignedOrderInfo>,
    /// Orders to run the arrival path on this iteration: new orders and matured deferrals.
    arrived: Vec<SignedOrderInfo>,
}

impl SwiftFeed {
    pub async fn subscribe(
        velocity: &VelocityClient,
        market_ids: Vec<MarketId>,
        feed_health: &FeedHealth,
    ) -> Self {
        log::info!(target: TARGET, "subscribing swift orders (ws url override: {:?})", swift_ws_url());
        let stream = velocity
            .subscribe_swift_orders(&market_ids, Some(true), None, swift_ws_url())
            .await
            .expect("subscribed swift orders");
        feed_health.set_swift_connected(true);
        log::info!(target: TARGET, "subscribed swift orders");

        Self {
            stream,
            market_ids,
            live: true,
            retries: 0,
            reconnect_at: tokio::time::Instant::now(),
            last_message: std::time::Instant::now(),
            deferred: Vec::new(),
            arrived: Vec::new(),
        }
    }

    /// Record what the stream yielded: an order to process, or a disconnect to retry.
    pub fn on_message(&mut self, message: Option<SignedOrderInfo>, feed_health: &FeedHealth) {
        match message {
            Some(order) => {
                self.retries = 0;
                self.last_message = std::time::Instant::now();
                self.arrived.push(order);
            }
            None => {
                // Reconnect forever with capped backoff. Giving up after N retries left the
                // bot permanently deaf to swift flow while reporting healthy.
                feed_health.set_swift_connected(false);
                self.live = false;
                let backoff = self.schedule_reconnect();
                log::warn!(target: "swift", "feed disconnected, retry {} in {backoff}s", self.retries);
            }
        }
    }

    /// Resubscribe after a disconnect, keeping the ws url override of the first subscription.
    pub async fn reconnect(&mut self, velocity: &VelocityClient, feed_health: &FeedHealth) {
        match velocity
            .subscribe_swift_orders(&self.market_ids, Some(true), None, swift_ws_url())
            .await
        {
            Ok(stream) => {
                log::info!(target: "swift", "feed resubscribed after {} attempt(s)", self.retries);
                self.stream = stream;
                self.retries = 0;
                self.last_message = std::time::Instant::now();
                self.live = true;
                feed_health.set_swift_connected(true);
            }
            Err(err) => {
                let backoff = self.schedule_reconnect();
                log::error!(target: "swift", "resubscribe failed: {err:?}, retry {} in {backoff}s", self.retries);
            }
        }
    }

    /// Resubscribe when the stream has been silent for `SWIFT_FEED_STALE_LIMIT`, in case the ws
    /// is half-open.
    pub async fn resubscribe_if_quiet(
        &mut self,
        velocity: &VelocityClient,
        feed_health: &FeedHealth,
    ) {
        let quiet = self.last_message.elapsed();
        if quiet <= SWIFT_FEED_STALE_LIMIT {
            return;
        }
        log::warn!(target: "swift", "no swift orders for {}s, resubscribing in case the ws is half-open", quiet.as_secs());
        match velocity
            .subscribe_swift_orders(&self.market_ids, Some(true), None, swift_ws_url())
            .await
        {
            Ok(stream) => {
                log::info!(target: "swift", "feed resubscribed after stale window");
                self.stream = stream;
                feed_health.set_swift_connected(true);
            }
            // keep the old stream: it may still be alive in a quiet market
            Err(err) => log::error!(target: "swift", "stale resubscribe failed: {err:?}"),
        }
        // reset either way, so a failed attempt retries after a full window, not every tick
        self.last_message = std::time::Instant::now();
    }

    /// Returns the backoff in seconds.
    fn schedule_reconnect(&mut self) -> u64 {
        self.retries += 1;
        let backoff = 2u64.saturating_pow(self.retries.min(5)).min(30);
        self.reconnect_at = tokio::time::Instant::now() + Duration::from_secs(backoff);
        backoff
    }

    /// Move the held orders whose message slot has arrived back to the arrival path.
    pub fn release_matured(&mut self, slot: Option<u64>, slot_clock: SlotClock, metrics: &Metrics) {
        if self.deferred.is_empty() {
            return;
        }
        let mut waiting = Vec::with_capacity(self.deferred.len());
        for order in self.deferred.drain(..) {
            match swift_slot_wait_if_known(
                order.slot(),
                slot,
                MAX_SWIFT_ORDER_DEFERRAL,
                is_resting_swift_limit(&order.order_params()),
                slot_clock,
            ) {
                SwiftSlotWait::Ready => self.arrived.push(order),
                SwiftSlotWait::Wait => waiting.push(order),
                SwiftSlotWait::TooFarAhead => {
                    log::warn!(target: TARGET, "deferred swift order is too far ahead of slot {slot:?}, dropping. uuid={}", order.order_uuid_str());
                    metrics.swift_place_skipped.inc();
                }
            }
        }
        self.deferred = waiting;
    }

    /// The orders ready to fill or place now. An order stamped ahead of the chain is held, or
    /// dropped when it is too far ahead or the hold queue is full.
    pub fn take_ready(&mut self, slot: Option<u64>, slot_clock: SlotClock) -> Vec<SignedOrderInfo> {
        let mut ready = Vec::with_capacity(self.arrived.len());
        for order in std::mem::take(&mut self.arrived) {
            // Until the stamped message slot arrives the program accepts neither a fill nor a
            // bare placement of an auction order. A resting limit is the exception: its stamp
            // is a deadline, and it is placed right away.
            let order_slot = order.slot();
            match swift_slot_wait_if_known(
                order_slot,
                slot,
                MAX_SWIFT_ORDER_DEFERRAL,
                is_resting_swift_limit(&order.order_params()),
                slot_clock,
            ) {
                SwiftSlotWait::Ready => ready.push(order),
                SwiftSlotWait::TooFarAhead => {
                    log::warn!(target: TARGET, "swift order stamped at slot {order_slot}, too far ahead of slot {slot:?}, dropping. uuid={}", order.order_uuid_str());
                }
                SwiftSlotWait::Wait if self.deferred.len() >= MAX_DEFERRED_SWIFT_ORDERS => {
                    log::warn!(target: TARGET, "deferred swift orders at capacity ({MAX_DEFERRED_SWIFT_ORDERS}), dropping. uuid={}", order.order_uuid_str());
                }
                SwiftSlotWait::Wait => {
                    log::info!(target: TARGET, "swift order slot {order_slot} not reached (slot {slot:?}), deferring. uuid={}", order.order_uuid_str());
                    self.deferred.push(order);
                }
            }
        }
        ready
    }
}

/// `SWIFT_WS_URL` overrides the swift ws server base url (velocity-rs appends `/ws?pubkey=`).
/// Unset uses the SDK default for the cluster.
fn swift_ws_url() -> Option<String> {
    std::env::var("SWIFT_WS_URL").ok()
}

/// Evaluate whether a swift order will cross resting liquidity or the AMM when a fill tx sent now
/// lands.
///
/// Returns `Fillable(crosses)` when the order should fill at landing, `NotFillable(reason)`
/// when it is well-formed but won't cross yet (the caller places it onchain for the per-slot
/// fill loop to pick up), or `Drop` for malformed or unsupported orders.
///
/// A swift fill tx posts no pyth-lazer price, so the order is priced and gated on the chain
/// view at the landing slot. The taker's auction is priced on the onchain clock: the program
/// starts a signed-msg order's auction at the message slot (`signed_msg_taker_order_slot`), not
/// the placement slot, so by landing time the auction has already moved a few price steps.
pub(super) fn evaluate_swift_crosses(
    velocity: &VelocityClient,
    dlob: &DLOB,
    tick: &SlotTick,
    view: &MarketView,
    signed_order: &SignedOrderInfo,
) -> SwiftDecision {
    let perp_market = &view.chain.quote_market;
    let oracle_price = view.chain.oracle.safe.price;
    let landing_slot = tick.landing_slot;
    let slot_clock = tick.exchange.slot_clock;

    let mut order_params = signed_order.order_params();
    let _ = order_params.update_perp_auction_params(perp_market, oracle_price, true);

    // Post-only limits are maker orders: never taker-fill them, but do place them onchain so
    // they rest on the book (the program cancels or amends them if they'd cross on placement).
    if order_params.order_type == OrderType::Limit && order_params.post_only != PostOnlyParam::None
    {
        return SwiftDecision::NotFillable("post-only limit (maker order)".into());
    }

    let (start_price, end_price, duration) = (
        order_params.auction_start_price.unwrap_or_default(),
        order_params.auction_end_price.unwrap_or_default(),
        order_params.auction_duration.unwrap_or_default(),
    );
    // Onchain the order slot is `min(clock.slot, message slot)` (`get_order_slot`), so the
    // auction clock starts at the message slot, never before. Callers defer an auction order
    // whose message slot has not arrived (`swift_slot_wait`), but a resting limit is forwarded
    // early, so `min` mirrors that clamp as well as guarding `calculate_auction_price`'s
    // elapsed-slot underflow.
    let order_slot = signed_order.slot().min(landing_slot);
    let order = Order {
        slot: order_slot,
        price: order_params.price,
        base_asset_amount: order_params.base_asset_amount,
        trigger_price: order_params.trigger_price.unwrap_or_default(),
        auction_duration: duration,
        auction_start_price: start_price,
        auction_end_price: end_price,
        max_ts: order_params.max_ts.unwrap_or_default(),
        oracle_price_offset: order_params.oracle_price_offset.unwrap_or_default(),
        market_index: order_params.market_index,
        order_type: order_params.order_type,
        market_type: order_params.market_type,
        direction: order_params.direction,
        reduce_only: order_params.reduce_only,
        post_only: order_params.post_only != PostOnlyParam::None,
        immediate_or_cancel: order_params.immediate_or_cancel(),
        trigger_condition: order_params.trigger_condition,
        // `OrderParams.bit_flags` is a different flag set; the program starts the order's flags
        // from zero and sets these (`place_perp_order`)
        bit_flags: swift_order_bit_flags(signed_order.has_builder()),
        ..Default::default()
    };

    let reserve_price = perp_market.amm.reserve_price().unwrap_or(0);
    let amm_price = if order_params.direction == PositionDirection::Long {
        perp_market
            .amm
            .ask_price(
                reserve_price,
                perp_market.amm.long_spread,
                perp_market.amm.reference_price_offset,
            )
            .unwrap_or(0)
    } else {
        perp_market
            .amm
            .bid_price(
                reserve_price,
                perp_market.amm.short_spread,
                perp_market.amm.reference_price_offset,
            )
            .unwrap_or(0)
    };

    let price = match order_params.order_type {
        OrderType::Market | OrderType::Oracle => {
            match calculate_auction_price(
                &order,
                landing_slot,
                perp_market.price_tick(),
                Some(oracle_price),
                slot_clock,
            ) {
                Ok(price) => price,
                Err(err) => {
                    log::warn!(target: TARGET, "could not get auction price {err:?}, params: {order_params:?}, dropping...");
                    return SwiftDecision::Drop;
                }
            }
        }
        OrderType::Limit => {
            match order.get_limit_price(
                Some(oracle_price),
                Some(amm_price),
                landing_slot,
                perp_market.price_tick(),
                slot_clock,
            ) {
                Ok(Some(price)) => price,
                // No resolvable limit price at this slot (e.g. auction-limit with no final
                // price). Can't evaluate crossing without one, but the order is still valid
                // onchain, so place it rather than dropping it.
                _ => {
                    log::debug!(target: TARGET, "no limit price yet: {order_params:?}");
                    return SwiftDecision::NotFillable("no resolvable limit price yet".into());
                }
            }
        }
        // Swift orders are never trigger or unknown types. Drop one defensively so untrusted
        // feed input can't crash the bot.
        other => {
            log::warn!(target: TARGET, "unsupported swift order type {other:?}, dropping. uuid={}", signed_order.order_uuid_str());
            return SwiftDecision::Drop;
        }
    };

    let taker_order = TakerOrder::from_order_params(order_params, price);
    let crosses = dlob.find_crosses_for_taker_order(
        landing_slot,
        oracle_price as u64,
        taker_order,
        Some(perp_market),
        None,
    );
    // Well-formed but not (yet) fillable: NotFillable, so the caller can place it onchain.
    if crosses.is_empty() {
        let amm_side = if order_params.direction == PositionDirection::Long {
            "ask"
        } else {
            "bid"
        };
        return SwiftDecision::NotFillable(format!(
            "no cross at landing slot {landing_slot}: taker_price={price} amm_{amm_side}={amm_price} auction_elapsed={}/{duration}",
            landing_slot.saturating_sub(order_slot),
        ));
    }

    // An AMM-only cross fills only if the program lets the AMM fill this order. A swift order
    // is fresh at landing, so that is the immediate JIT leg. The order stays well-formed, so the
    // caller places it and the slot loop fills it once the AMM opens.
    if crosses.orders.is_empty() && crosses.has_vamm_cross {
        let gate = swift_amm_gate(velocity, tick, view, signed_order, &order);
        if !gate.open {
            return SwiftDecision::NotFillable(format!(
                "AMM-only cross but the AMM gate is closed (low_risk={} wants_jit={} immediate_stale={} can_skip_auction={} oracle={oracle_price})",
                gate.order_low_risk, gate.wants_jit, gate.safe_stale_immediate, gate.can_skip_auction,
            ));
        }
    }
    SwiftDecision::Fillable(crosses)
}

/// The flags `place_perp_order` gives a placed swift order. Isolated-position orders also get
/// `IsIsolatedPosition`, which no AMM gate reads.
fn swift_order_bit_flags(has_builder: bool) -> u8 {
    let mut bit_flags = OrderBitFlag::SignedMessage as u8;
    if has_builder {
        bit_flags |= OrderBitFlag::HasBuilder as u8;
    }
    bit_flags
}

/// The program's AMM gate for a swift order on the chain view. The taker's pause flags come
/// from the cached user stats. A taker whose account or stats are not cached is treated as
/// unable to skip the auction, which keeps the AMM out and places the order instead.
fn swift_amm_gate(
    velocity: &VelocityClient,
    tick: &SlotTick,
    view: &MarketView,
    signed_order: &SignedOrderInfo,
    order: &Order,
) -> AmmGate {
    let taker = velocity
        .try_get_account::<User>(&signed_order.taker_subaccount())
        .ok();
    let stats = velocity
        .try_get_account::<UserStats>(&Wallet::derive_stats_account(&signed_order.taker_authority))
        .ok();
    let can_skip_auction = match (taker, stats) {
        (Some(taker), Some(stats)) => taker
            .can_skip_auction_duration(&stats, order.reduce_only)
            .unwrap_or(false),
        _ => false,
    };
    AmmGate::evaluate(
        &view.market,
        &tick.exchange,
        Some(&view.chain.oracle),
        Some(order),
        can_skip_auction,
        tick.landing_slot,
    )
}

/// Outcome of evaluating a swift order against current liquidity.
// Returned by value once per swift order; boxing `MakerCrosses` would add an allocation per order.
#[allow(clippy::large_enum_variant)]
pub(super) enum SwiftDecision {
    /// Crosses resting liquidity or the AMM right now: fill it immediately.
    Fillable(MakerCrosses),
    /// Well-formed but not taker-fillable now (not marketable yet, post-only maker order, no
    /// resolvable limit price, or the AMM gate is closed): place it onchain so the slot loop can
    /// fill it later. Carries a human-readable reason for the placement log.
    NotFillable(String),
    /// Malformed or unsupported (bad auction price, non-market/limit type): drop it.
    Drop,
}

/// Place a swift order and fill it against `crosses` in one tx.
pub(super) async fn fill_swift_order(
    filler: &Filler,
    priority_fee: u64,
    swift_order: SignedOrderInfo,
    crosses: MakerCrosses,
) {
    log::info!(target: TARGET, "try fill swift order: {}", swift_order.order_uuid_str());
    let velocity = filler.keeper.velocity;
    let taker_order = swift_order.order_params();
    let taker_subaccount = swift_order.taker_subaccount();
    let taker_stats = Wallet::derive_stats_account(&swift_order.taker_authority);

    let filler_account = filler.account();
    let (taker_account, taker_stats) = match tokio::try_join!(
        velocity.get_account_value::<User>(&taker_subaccount),
        velocity.get_account_value::<UserStats>(&taker_stats)
    ) {
        Ok(accounts) => accounts,
        Err(err) => {
            log::warn!(target: TARGET, "swift fill: failed to load taker accounts {taker_subaccount}: {err:?}");
            return;
        }
    };

    // a maker missing from the cache shrinks the cross
    let makers: Vec<User> = crosses
        .orders
        .iter()
        .filter(|(maker_order, _fill_size)| maker_order.user != taker_subaccount) // can't fill itself
        .filter_map(|(maker_order, _fill_size)| filler.keeper.cached_user(&maker_order.user))
        .collect();

    if makers.is_empty() && !crosses.has_vamm_cross {
        log::warn!("invalid cross: {crosses:?}");
        return;
    }

    let tx_builder = filler
        .tx_builder(&filler_account, priority_fee, filler.swift_cu_limit)
        .place_swift_order(&swift_order, &taker_account);
    let tx_builder = with_spot_interest_cranks(tx_builder, velocity, &taker_account, &makers)
        .fill_perp_order(
            taker_order.market_index,
            taker_subaccount,
            &taker_account,
            &taker_stats,
            // the order id is unknown until the place lands, so the program fills the newest
            None,
            makers.as_slice(),
            Some(swift_order.has_builder()),
        );
    let (tx_builder, cu_limit) =
        scale_cu_limit_for_accounts(tx_builder, filler.swift_cu_limit, 30, 20);

    filler
        .keeper
        .tx
        .send_tx(
            tx_builder.build(),
            TxIntent::SwiftFill {
                uuid: swift_order.order_uuid(),
                market_index: taker_order.market_index,
                taker_user: taker_subaccount,
                maker_crosses: crosses,
            },
            cu_limit as u64,
        )
        .await;
}

/// Place a swift order onchain without filling it.
///
/// Used when the order is not immediately fillable on arrival: placing it makes it a regular
/// resting onchain order that the normal per-slot fill path (and other keepers) can fill while
/// it remains live, instead of dropping it. Emits a `swift_place` wide event at tx
/// confirmation so the gas spent on placements can be measured against the fills they yield.
pub(super) async fn place_swift_order_onchain(
    filler: &Filler,
    priority_fee: u64,
    swift_order: SignedOrderInfo,
    slot: u64,
) {
    let market_index = swift_order.order_params().market_index;
    let taker_subaccount = swift_order.taker_subaccount();

    let filler_account = filler.account();
    let taker_account = match filler
        .keeper
        .velocity
        .get_account_value::<User>(&taker_subaccount)
        .await
    {
        Ok(account) => account,
        Err(err) => {
            log::warn!(target: TARGET, "swift place: failed to load taker account {taker_subaccount}: {err:?}");
            return;
        }
    };

    let tx = filler
        .tx_builder(&filler_account, priority_fee, filler.swift_cu_limit)
        .place_swift_order(&swift_order, &taker_account)
        .build();

    filler
        .keeper
        .tx
        .send_tx(
            tx,
            TxIntent::SwiftPlace {
                uuid: swift_order.order_uuid(),
                market_index,
                taker_user: taker_subaccount,
                slot,
            },
            filler.swift_cu_limit as u64,
        )
        .await;
}

#[cfg(test)]
mod tests {
    use {
        super::{
            is_resting_swift_limit, should_poll_swift, swift_order_bit_flags, swift_order_expired,
            swift_slot_wait, swift_slot_wait_if_known, OrderParams, OrderType, SwiftSlotWait,
        },
        velocity_rs::program::math::time::{Millis, SlotClock},
    };

    #[test]
    fn swift_order_flags_follow_the_placed_order() {
        use velocity_rs::program::state::user::OrderBitFlag;
        let plain = swift_order_bit_flags(false);
        assert_eq!(plain, OrderBitFlag::SignedMessage as u8);
        // the params' immediate-or-cancel bit is 0b1, which as an order flag would read as
        // SignedMessage; nothing from the params set may leak in, least of all SafeTriggerOrder
        assert_eq!(plain & OrderBitFlag::SafeTriggerOrder as u8, 0);
        assert_eq!(
            swift_order_bit_flags(true),
            OrderBitFlag::SignedMessage as u8 | OrderBitFlag::HasBuilder as u8
        );
    }

    #[test]
    fn swift_slot_wait_holds_a_signing_buffer() {
        // The UI's buffer: a few slots ahead of the chain, the normal arrival state.
        assert_eq!(
            swift_slot_wait(
                107,
                100,
                Millis::from_secs(10),
                false,
                SlotClock::baseline()
            ),
            SwiftSlotWait::Wait
        );
        // Arrived: the program accepts the order from its own slot onward.
        assert_eq!(
            swift_slot_wait(
                100,
                100,
                Millis::from_secs(10),
                false,
                SlotClock::baseline()
            ),
            SwiftSlotWait::Ready
        );
        assert_eq!(
            swift_slot_wait(95, 100, Millis::from_secs(10), false, SlotClock::baseline()),
            SwiftSlotWait::Ready
        );
        // 25 baseline slots = 10s, exactly the bound; one more is not a signing buffer.
        assert_eq!(
            swift_slot_wait(
                125,
                100,
                Millis::from_secs(10),
                false,
                SlotClock::baseline()
            ),
            SwiftSlotWait::Wait
        );
        assert_eq!(
            swift_slot_wait(
                126,
                100,
                Millis::from_secs(10),
                false,
                SlotClock::baseline()
            ),
            SwiftSlotWait::TooFarAhead
        );
    }

    #[test]
    fn swift_slot_wait_places_a_resting_limit_ahead_of_its_slot() {
        // A no-auction limit's stamp is its placement deadline, set a whole signing budget
        // (~14s, 35 baseline slots) ahead: past the deferral bound, yet ready now.
        assert_eq!(
            swift_slot_wait(135, 100, Millis::from_secs(10), true, SlotClock::baseline()),
            SwiftSlotWait::Ready
        );
        // The program's 30s lead bound (75 baseline slots) still bounds the stamp.
        assert_eq!(
            swift_slot_wait(175, 100, Millis::from_secs(10), true, SlotClock::baseline()),
            SwiftSlotWait::Ready
        );
        assert_eq!(
            swift_slot_wait(176, 100, Millis::from_secs(10), true, SlotClock::baseline()),
            SwiftSlotWait::TooFarAhead
        );
        // A stamp at or behind the chain is ready either way.
        assert_eq!(
            swift_slot_wait(100, 100, Millis::from_secs(10), true, SlotClock::baseline()),
            SwiftSlotWait::Ready
        );
    }

    #[test]
    fn resting_swift_limit_is_a_limit_with_no_auction() {
        let mut params = OrderParams {
            order_type: OrderType::Limit,
            auction_duration: None,
            ..Default::default()
        };
        assert!(is_resting_swift_limit(&params));
        params.auction_duration = Some(0);
        assert!(is_resting_swift_limit(&params));
        params.auction_duration = Some(10);
        assert!(!is_resting_swift_limit(&params));
        params.order_type = OrderType::Market;
        params.auction_duration = None;
        assert!(!is_resting_swift_limit(&params));
    }

    #[test]
    fn swift_slot_wait_defers_until_the_chain_slot_is_known() {
        assert_eq!(
            swift_slot_wait_if_known(
                443_184_701,
                None,
                Millis::from_secs(10),
                false,
                SlotClock::baseline(),
            ),
            SwiftSlotWait::Wait
        );
        assert_eq!(
            swift_slot_wait_if_known(
                443_184_701,
                Some(443_184_694),
                Millis::from_secs(10),
                false,
                SlotClock::baseline(),
            ),
            SwiftSlotWait::Wait
        );
        assert_eq!(
            swift_slot_wait_if_known(
                443_184_720,
                Some(443_184_694),
                Millis::from_secs(10),
                false,
                SlotClock::baseline(),
            ),
            SwiftSlotWait::TooFarAhead
        );
    }

    #[test]
    fn swift_and_buffered_slots_take_bounded_turns() {
        assert!(!should_poll_swift(true, true, true));
        assert!(should_poll_swift(true, true, false));
        assert!(should_poll_swift(true, false, true));
        assert!(!should_poll_swift(false, false, false));
    }

    #[test]
    fn swift_expiry_placement_deadline_binds_before_staleness() {
        // `auction_duration` is a u8 (<=255), so the placement deadline
        // (order_slot + auction_duration) always binds before the 500-slot signed-message
        // window. The order is unplaceable one slot past the deadline, well before slot 500.
        assert!(!swift_order_expired(
            0,
            255,
            0,
            255,
            0,
            SlotClock::baseline()
        ));
        assert!(swift_order_expired(
            0,
            255,
            0,
            256,
            0,
            SlotClock::baseline()
        ));
    }

    #[test]
    fn swift_expiry_placement_deadline() {
        // max_slot = order_slot + auction_duration = 130. Program rejects once max_slot < slot.
        assert!(!swift_order_expired(
            100,
            30,
            0,
            130,
            0,
            SlotClock::baseline()
        )); // exactly at deadline: still placeable
        assert!(swift_order_expired(
            100,
            30,
            0,
            131,
            0,
            SlotClock::baseline()
        )); // one past: gone
            // Zero auction duration (limit order default): only placeable in the signing slot.
        assert!(!swift_order_expired(
            100,
            0,
            0,
            100,
            0,
            SlotClock::baseline()
        ));
        assert!(swift_order_expired(
            100,
            0,
            0,
            101,
            0,
            SlotClock::baseline()
        ));
    }

    #[test]
    fn swift_expiry_max_ts() {
        // max_ts == 0 disables the ts check.
        assert!(!swift_order_expired(
            100,
            200,
            0,
            100,
            i64::MAX,
            SlotClock::baseline()
        ));
        // now == max_ts is still valid; now > max_ts expires.
        assert!(!swift_order_expired(
            100,
            200,
            5_000,
            100,
            5_000,
            SlotClock::baseline()
        ));
        assert!(swift_order_expired(
            100,
            200,
            5_000,
            100,
            5_001,
            SlotClock::baseline()
        ));
    }
}
