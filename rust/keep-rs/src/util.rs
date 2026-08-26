use {
    futures_util::StreamExt,
    pyth_lazer_client::AnyResponse,
    pyth_lazer_protocol::{
        message::Message,
        payload::{PayloadData, PayloadPropertyValue},
        router::{
            Channel, DeliveryFormat, FixedRate, Format, JsonBinaryEncoding, PriceFeedId,
            PriceFeedProperty, SubscriptionParams, SubscriptionParamsRepr, TimestampUs,
        },
        subscription::{Response, SubscribeRequest, SubscriptionId},
    },
    solana_sdk::signature::Signature,
    std::{
        collections::HashSet,
        time::{Duration, SystemTime, UNIX_EPOCH},
    },
    velocity_rs::{
        constants::{
            perp_market_index_to_pyth_lazer_feed_id, pyth_lazer_feed_id_to_perp_market_index,
            pyth_lazer_feed_id_to_spot_market_index, spot_market_index_to_pyth_lazer_feed_id,
        },
        dlob::{L3Order, MakerCrosses},
        types::{MarketId, MarketType},
        Pubkey,
    },
};

/// Live slot duration at `now_slot` from the client's cached `State`, resolved
/// through the full slot clock (transition archive first, legacy staging fields
/// as fallback); the 400ms baseline when State is not yet subscribed.
pub fn client_slot_duration(
    velocity: &velocity_rs::VelocityClient,
    now_slot: u64,
) -> velocity_rs::program::math::time::SlotDuration {
    velocity.slot_duration_at(now_slot)
}

pub struct OrderSlotLimiter<const N: usize> {
    slots: [Vec<u32>; N],
    generations: [u64; N],
}

impl<const N: usize> OrderSlotLimiter<N> {
    pub fn new() -> Self {
        let slots = std::array::from_fn(|_| Vec::new());
        let generations = [0; N];
        Self { slots, generations }
    }

    pub fn allow_event(&mut self, g: u64, id: u32) -> bool {
        let idx = (g % N as u64) as usize;

        // Replace old generation
        if self.generations[idx] != g {
            self.slots[idx].clear();
            self.generations[idx] = g;
        }

        // Count occurrences of id in generations g - 1 to g - 4
        let mut count = 0;
        for i in 2..=4 {
            let past_g = g.saturating_sub(i);
            let past_idx = (past_g % N as u64) as usize;

            if self.generations[past_idx] == past_g {
                if self.slots[past_idx].binary_search(&id).is_ok() {
                    count += 1;
                    if count >= 1 {
                        // Already appeared once, so this would be the second time
                        return false;
                    }
                }
            }
        }

        // Insert in sorted order
        let slot = &mut self.slots[idx];
        match slot.binary_search(&id) {
            Ok(_) => false, // Already present — shouldn't happen
            Err(pos) => {
                slot.insert(pos, id);
                true
            }
        }
    }

    pub fn check_event(&self, g: u64, id: u32) -> bool {
        // Check generations g - 1 and g - 4
        for i in 1..=4 {
            let past_g = g.saturating_sub(i);
            let past_idx = (past_g % N as u64) as usize;

            if self.generations[past_idx] == past_g {
                if self.slots[past_idx].binary_search(&id).is_ok() {
                    return false;
                }
            }
        }

        true
    }
}

#[derive(Clone, Default, Debug)]
pub enum TxIntent {
    #[default]
    None,
    AuctionFill {
        market_index: u16,
        taker_order_id: u32,
        /// taker subaccount the fill was sent for (order ids are per-user counters,
        /// so `taker_order_id` alone is ambiguous across users)
        taker_user: Pubkey,
        has_trigger: bool,
        maker_crosses: MakerCrosses,
    },
    SwiftFill {
        uuid: [u8; 8],
        market_index: u16,
        /// taker subaccount the fill was sent for (disambiguates the swift uuid across users)
        taker_user: Pubkey,
        maker_crosses: MakerCrosses,
    },
    /// place-only swift order: order placed on-chain (no immediate fill) so the normal
    /// per-slot fill path can pick it up while it remains live
    SwiftPlace {
        uuid: [u8; 8],
        market_index: u16,
        /// taker subaccount whose order was placed on-chain
        taker_user: Pubkey,
        slot: u64,
    },
    VAMMTakerFill {
        slot: u64,
        market_index: u16,
        maker_order_id: u32,
        /// taker (the resting-order user) filled against the vAMM
        taker_user: Pubkey,
    },
    /// limit orders crossed
    LimitUncross {
        slot: u64,
        market_index: u16,
        taker_order_id: u32,
        /// taker subaccount the fill was sent for (order ids are per-user counters,
        /// so `taker_order_id` alone is ambiguous across users)
        taker_user: Pubkey,
        /// order id of the best crossing counterparty attached as a maker account.
        /// Context only — the program picks the actual maker order(s) to match.
        maker_order_id: u32,
    },
    LiquidateWithFill {
        market_index: u16,
        liquidatee: Pubkey,
        slot: u64,
    },
    LiquidatePerp {
        market_index: u16,
        liquidatee: Pubkey,
        slot: u64,
    },
    LiquidatePerpPnlForDeposit {
        perp_market_index: u16,
        spot_market_index: u16,
        liquidatee: Pubkey,
        slot: u64,
    },
    LiquidateBorrowForPerpPnl {
        perp_market_index: u16,
        spot_market_index: u16,
        liquidatee: Pubkey,
        slot: u64,
    },
    LiquidateSpot {
        asset_market_index: u16,
        liability_market_index: u16,
        liquidatee: Pubkey,
        slot: u64,
    },
    Derisk {
        market_index: u16,
        subaccount: Pubkey,
    },
    SettlePnl {
        market_index: u16,
        subaccount: Pubkey,
    },
    /// standalone trigger of a trigger order whose condition is met but that does not yet cross
    Trigger {
        market_index: u16,
        order_id: u32,
        /// taker subaccount whose trigger order is being triggered
        taker_user: Pubkey,
        slot: u64,
    },
}

impl TxIntent {
    pub fn label(&self) -> &'static str {
        match self {
            TxIntent::None => "none",
            TxIntent::AuctionFill { maker_crosses, .. } => {
                if maker_crosses.has_vamm_cross {
                    "auction_fill_vamm"
                } else {
                    "auction_fill"
                }
            }
            TxIntent::SwiftFill { maker_crosses, .. } => {
                if maker_crosses.has_vamm_cross {
                    "swift_fill_vamm"
                } else {
                    "swift_fill"
                }
            }
            TxIntent::SwiftPlace { .. } => "swift_place",
            TxIntent::LimitUncross { .. } => "limit_uncross",
            TxIntent::VAMMTakerFill { .. } => "vamm_taker",
            TxIntent::LiquidateWithFill { .. } => "liq_with_fill",
            TxIntent::LiquidatePerp { .. } => "liq_perp",
            TxIntent::LiquidatePerpPnlForDeposit { .. } => "liq_perp_pnl_for_deposit",
            TxIntent::LiquidateBorrowForPerpPnl { .. } => "liq_borrow_for_perp_pnl",
            TxIntent::LiquidateSpot { .. } => "liq_spot",
            TxIntent::Derisk { .. } => "derisk",
            TxIntent::SettlePnl { .. } => "settle_pnl",
            TxIntent::Trigger { .. } => "trigger",
        }
    }

    pub fn expected_fill_count(&self) -> usize {
        match self {
            TxIntent::None => 0,
            TxIntent::AuctionFill { maker_crosses, .. } => {
                maker_crosses.orders.len() + if maker_crosses.has_vamm_cross { 1 } else { 0 }
            }
            TxIntent::SwiftFill { maker_crosses, .. } => {
                maker_crosses.orders.len() + if maker_crosses.has_vamm_cross { 1 } else { 0 }
            }
            // place-only: no fill expected in this tx (the fill happens later via the slot loop)
            TxIntent::SwiftPlace { .. } => 0,
            TxIntent::VAMMTakerFill { .. } => 1,
            TxIntent::LimitUncross { .. } => 1,
            TxIntent::LiquidateWithFill { .. } => 1,
            TxIntent::LiquidatePerp { .. } => 0,
            TxIntent::LiquidatePerpPnlForDeposit { .. } => 0,
            TxIntent::LiquidateBorrowForPerpPnl { .. } => 0,
            TxIntent::LiquidateSpot { .. } => 0,
            TxIntent::Derisk { .. } => 0,
            TxIntent::SettlePnl { .. } => 0,
            TxIntent::Trigger { .. } => 0,
        }
    }

    /// true if tx was expected to trigger the taker order
    pub fn expected_trigger(&self) -> bool {
        match self {
            TxIntent::AuctionFill { has_trigger, .. } => *has_trigger,
            TxIntent::Trigger { .. } => true,
            _ => false,
        }
    }

    pub fn crosses_and_slot(&self) -> (Vec<(L3Order, u64)>, u64) {
        match self {
            TxIntent::None => (vec![], 0),
            TxIntent::AuctionFill { maker_crosses, .. } => {
                (maker_crosses.orders.to_vec(), maker_crosses.slot)
            }
            TxIntent::SwiftFill { maker_crosses, .. } => {
                (maker_crosses.orders.to_vec(), maker_crosses.slot)
            }
            TxIntent::SwiftPlace { slot, .. } => (vec![], *slot),
            Self::VAMMTakerFill { slot, .. } => (vec![], *slot),
            Self::LimitUncross { slot, .. } => (vec![], *slot),
            Self::LiquidateWithFill { slot, .. } => (vec![], *slot),
            Self::LiquidatePerp { slot, .. } => (vec![], *slot),
            Self::LiquidatePerpPnlForDeposit { slot, .. } => (vec![], *slot),
            Self::LiquidateBorrowForPerpPnl { slot, .. } => (vec![], *slot),
            Self::LiquidateSpot { slot, .. } => (vec![], *slot),
            TxIntent::Derisk { .. } => (vec![], 0),
            TxIntent::SettlePnl { .. } => (vec![], 0),
            TxIntent::Trigger { slot, .. } => (vec![], *slot),
        }
    }

    pub fn slot(&self) -> Option<u64> {
        match self {
            Self::VAMMTakerFill { slot, .. }
            | Self::LimitUncross { slot, .. }
            | Self::LiquidateWithFill { slot, .. }
            | Self::LiquidatePerp { slot, .. }
            | Self::LiquidatePerpPnlForDeposit { slot, .. }
            | Self::LiquidateBorrowForPerpPnl { slot, .. }
            | Self::LiquidateSpot { slot, .. }
            | Self::SwiftPlace { slot, .. }
            | Self::Trigger { slot, .. } => Some(*slot),
            _ => None,
        }
    }

    /// Market index this tx acts on, where the intent carries one. Used for wide-event logging.
    pub fn market_index(&self) -> Option<u16> {
        match self {
            Self::AuctionFill { market_index, .. }
            | Self::SwiftFill { market_index, .. }
            | Self::SwiftPlace { market_index, .. }
            | Self::VAMMTakerFill { market_index, .. }
            | Self::LimitUncross { market_index, .. }
            | Self::LiquidateWithFill { market_index, .. }
            | Self::LiquidatePerp { market_index, .. }
            | Self::Derisk { market_index, .. }
            | Self::SettlePnl { market_index, .. }
            | Self::Trigger { market_index, .. } => Some(*market_index),
            Self::LiquidatePerpPnlForDeposit {
                perp_market_index, ..
            }
            | Self::LiquidateBorrowForPerpPnl {
                perp_market_index, ..
            } => Some(*perp_market_index),
            Self::LiquidateSpot {
                liability_market_index,
                ..
            } => Some(*liability_market_index),
            Self::None => None,
        }
    }

    /// Taker/target order id, where the intent carries one. Used for wide-event logging.
    pub fn order_id(&self) -> Option<u32> {
        match self {
            Self::AuctionFill { taker_order_id, .. }
            | Self::LimitUncross { taker_order_id, .. } => Some(*taker_order_id),
            Self::VAMMTakerFill { maker_order_id, .. } => Some(*maker_order_id),
            Self::Trigger { order_id, .. } => Some(*order_id),
            _ => None,
        }
    }

    /// Taker/target user subaccount, where the intent carries one. Used for wide-event
    /// logging to disambiguate per-user order ids / swift uuids, and — critically — so a
    /// single Loki query on the taker subaccount (`| json | user="<subaccount>"`, or a
    /// line filter) captures the whole fill lifecycle for one order across every fill-path
    /// intent, not just `limit_uncross`.
    pub fn user(&self) -> Option<Pubkey> {
        match self {
            Self::AuctionFill { taker_user, .. }
            | Self::SwiftFill { taker_user, .. }
            | Self::SwiftPlace { taker_user, .. }
            | Self::VAMMTakerFill { taker_user, .. }
            | Self::LimitUncross { taker_user, .. }
            | Self::Trigger { taker_user, .. } => Some(*taker_user),
            _ => None,
        }
    }

    /// Swift order uuid (hex), where applicable. Used for wide-event logging so swift
    /// placements/fills can be correlated and their gas cost attributed.
    pub fn swift_uuid(&self) -> Option<[u8; 8]> {
        match self {
            Self::SwiftFill { uuid, .. } | Self::SwiftPlace { uuid, .. } => Some(*uuid),
            _ => None,
        }
    }

    /// Returns the liquidatee pubkey if this is a liquidation intent
    pub fn liquidatee(&self) -> Option<Pubkey> {
        match self {
            Self::LiquidateWithFill { liquidatee, .. }
            | Self::LiquidatePerp { liquidatee, .. }
            | Self::LiquidatePerpPnlForDeposit { liquidatee, .. }
            | Self::LiquidateBorrowForPerpPnl { liquidatee, .. }
            | Self::LiquidateSpot { liquidatee, .. } => Some(*liquidatee),
            _ => None,
        }
    }

    /// Returns true if this intent is a liquidation type
    pub fn is_liquidation(&self) -> bool {
        matches!(
            self,
            Self::LiquidateWithFill { .. }
                | Self::LiquidatePerp { .. }
                | Self::LiquidatePerpPnlForDeposit { .. }
                | Self::LiquidateBorrowForPerpPnl { .. }
                | Self::LiquidateSpot { .. }
        )
    }
}

#[derive(Clone, Default, Debug)]
pub struct PendingTxMeta {
    pub signature: Signature,
    pub intent: TxIntent,
    pub cu_limit: u64,
    pub ts: u64,
}

impl PendingTxMeta {
    pub fn new(sig: Signature, intent: TxIntent, cu_limit: u64) -> Self {
        Self {
            signature: sig,
            ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            intent,
            cu_limit,
        }
    }
}

/// Circular buffer for pending transactions or similar FIFO workloads.
///
/// Usage example:
/// ```
/// let mut buf: PendingTxs<1024> = PendingTxs::new();
/// buf.insert(meta);
/// let confirmed = buf.confirm(|m| m.signature == sig);
/// ```
pub struct PendingTxs<const N: usize> {
    buffer: Box<[PendingTxMeta; N]>,
    head: usize,
    tail: usize,
    size: usize,
}

impl<const N: usize> PendingTxs<N> {
    pub fn new() -> Self {
        Self {
            buffer: Box::new([(); N].map(|_| PendingTxMeta::default())),
            head: 0,
            tail: 0,
            size: 0,
        }
    }

    /// Insert a new item, overwriting the oldest if full.
    pub fn insert(&mut self, item: PendingTxMeta) {
        self.buffer[self.tail] = item;
        self.tail = (self.tail + 1) % N;
        if self.size == N {
            self.head = (self.head + 1) % N;
        } else {
            self.size += 1;
        }
    }

    /// Confirm and return the first item with matching signature.
    ///
    /// Returns Some(item) if found, else None. The entry is consumed: a duplicate
    /// confirmation of the same signature (e.g. redelivered by the tx stream) returns
    /// None instead of re-running the confirmation accounting.
    pub fn confirm(&mut self, sig: &Signature) -> Option<PendingTxMeta> {
        for i in 0..self.size {
            let idx = (self.head + i) % N;
            // TODO: check if overwritten entry is confirmed or not
            if self.buffer[idx].signature == *sig {
                // leave a default (never-matching) hole; head/size stay untouched
                return Some(std::mem::take(&mut self.buffer[idx]));
            }
        }
        None
    }
}

/// Max age of a swift signed message before the program refuses to place it
/// (~200s, expressed in actual slots at the current slot duration).
///
/// Mirrors the staleness gate in `place_signed_msg_taker_order`
/// (programs/velocity/src/instructions/keeper.rs).
pub const SWIFT_SIGNED_MSG_MAX_AGE: velocity_rs::program::math::time::Millis =
    velocity_rs::program::math::time::Millis::from_secs(200);

/// Returns true if a swift (signed-message) order can no longer be usefully *placed* on-chain,
/// so the bot shouldn't spend a tx trying.
///
/// The two slot gates mirror `place_signed_msg_taker_order` exactly:
/// - **signed-message staleness**: the program rejects once the order's
///   wall-clock age (integrated per slot-duration regime) exceeds ~200s
/// - **placement deadline**: program silently no-ops once `max_slot < current_slot`, where
///   `max_slot = order_slot + auction_duration converted from 400ms units to
///   actual slots (ceil)` (identical formula for limit & market orders)
///
/// The `max_ts` check is an *additional* client-side guard (the program does not gate placement
/// on `max_ts`): an order whose `max_ts` has passed is already dead, so placing it would waste a
/// tx. Note `auction_duration` is a `u8` (≤ 255 units ≈ 102s), so the placement deadline always
/// binds before the ~200s staleness window; both are checked for completeness/robustness.
pub fn swift_placement_expired(
    order_slot: u64,
    auction_duration: u8,
    max_ts: i64,
    current_slot: u64,
    now_ts: i64,
    slot_clock: velocity_rs::program::math::time::SlotClock,
) -> bool {
    // signed message too old for the program to accept
    if slot_clock.elapsed(order_slot, current_slot) > SWIFT_SIGNED_MSG_MAX_AGE {
        return true;
    }
    // placement deadline: program no-ops once max_slot < current_slot
    let max_slot = order_slot.saturating_add(
        velocity_rs::program::math::time::Millis::from_stored_units(auction_duration as u64)
            .to_slots_ceil(slot_clock.slot_duration_at(current_slot)),
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

#[derive(Clone, Debug)]
pub struct PythPriceUpdate {
    pub market_type: MarketType,
    pub market_id: u16,
    pub feed_id: u32,
    pub price: u64,
    // original pyth message
    pub message: Vec<u8>,
    pub ts: TimestampUs,
}

/// Tolerated forward clock skew before a future-dated feed timestamp is treated as invalid —
/// a timestamp further ahead than this means a bad clock on one side, not a fresh price.
const PYTH_MAX_CLOCK_SKEW_US: u64 = 1_000_000;

/// Returns true if a pyth-lazer update's feed timestamp is within `max_age_us` of wall-clock
/// `now_us`. Used to gate consumption of the cached `PythPriceUpdate` on wall-clock age, since
/// a frozen websocket (see [`subscribe_price_feeds`]) leaves the cache holding a price that's
/// arbitrarily old with no signal of that in the update itself.
pub fn pyth_update_is_fresh(
    update_ts_us: TimestampUs,
    now_us: TimestampUs,
    max_age_us: u64,
) -> bool {
    now_us.saturating_us_since(update_ts_us) <= max_age_us
        && update_ts_us.saturating_us_since(now_us) <= PYTH_MAX_CLOCK_SKEW_US
}

fn fixed_rate(feed_id: u32) -> FixedRate {
    match feed_id {
        1 | 2 | 6 => FixedRate::MIN,
        10 => FixedRate::from_ms(50).unwrap(),
        _ => FixedRate::from_ms(200).unwrap(),
    }
}

// scale pyth lazer price into velocity price precision
#[inline(always)]
fn to_price_precision(price: u64, feed_id: u32, market_type: MarketType) -> u64 {
    match feed_id {
        // https://docs.pyth.network/lazer/price-feed-ids
        // LAZER_1M
        9 => match market_type {
            MarketType::Perp => price * 100,    // -10 => -6 * 1M
            MarketType::Spot => price / 10_000, // -10 => -6
        },
        4 => price * 100, // -10 => -6 * 1M
        // LAZER_1K
        1578 | 2396 | 137 => match market_type {
            MarketType::Perp => price * 10,  // -10 => -6 * 1K
            MarketType::Spot => price / 100, // -8 => -6
        },
        _ => price / 100, // -8 => -6
    }
}

pub fn subscribe_price_feeds(
    mut cli: pyth_lazer_client::LazerClient,
    perp_market_ids: &[MarketId],
    spot_market_ids: &[MarketId],
    extra_feed_ids: &[u32],
) -> tokio::sync::mpsc::Receiver<PythPriceUpdate> {
    let mut feed_id_set = HashSet::new();

    for m in perp_market_ids {
        if let Some(fid) = perp_market_index_to_pyth_lazer_feed_id(m.index()) {
            feed_id_set.insert(fid);
        }
    }

    for m in spot_market_ids {
        if let Some(fid) = spot_market_index_to_pyth_lazer_feed_id(m.index()) {
            feed_id_set.insert(fid);
        }
    }

    let extra_feeds: HashSet<u32> = extra_feed_ids.iter().copied().collect();
    feed_id_set.extend(extra_feeds.iter().copied());

    let feed_ids: Vec<PriceFeedId> = feed_id_set.into_iter().map(PriceFeedId).collect();

    const MAX_RETRIES: u32 = 10;
    // Pyth feeds tick every 50-200ms (see `fixed_rate`), so this much silence on the
    // websocket is unambiguous. A half-open socket never yields an error or `None` —
    // `stream.next()` just pends forever — so wrap it in a timeout and fall through
    // to the existing reconnect/backoff machinery below rather than trusting the socket.
    const PYTH_FEED_STALE_LIMIT: Duration = Duration::from_secs(30);

    let (price_tx, price_rx) = tokio::sync::mpsc::channel(512);

    let mut retries = 0u32;
    tokio::spawn(async move {
        loop {
            let pyth_lazer_stream = match cli.start().await {
                Ok(stream) => stream,
                Err(err) => {
                    retries += 1;

                    if retries >= MAX_RETRIES {
                        log::error!(
                            target: "pyth",
                            "FATAL: feed connection failed after {MAX_RETRIES} attempts; closing price feed channel"
                        );
                        return;
                    } else {
                        let backoff = 2u64.pow(retries).min(30); // 2^retries seconds, capped at 30s
                        log::warn!(
                            target: "pyth",
                            "feed connection failed: {err:?}, retry {retries}/{MAX_RETRIES} in {backoff}s"
                        );
                        tokio::time::sleep(Duration::from_secs(backoff)).await;
                        continue;
                    }
                }
            };

            // sub per feed
            let mut sub_id = 0;
            for feed_id in feed_ids.iter() {
                let subscribe_request = SubscribeRequest {
                    subscription_id: SubscriptionId(sub_id),
                    params: SubscriptionParams::new(SubscriptionParamsRepr {
                        price_feed_ids: vec![*feed_id],
                        // velocity program requires exponent + feed_update_timestamp to apply the update
                        properties: vec![
                            PriceFeedProperty::Price,
                            PriceFeedProperty::Exponent,
                            PriceFeedProperty::FeedUpdateTimestamp,
                        ],
                        delivery_format: DeliveryFormat::Binary,
                        json_binary_encoding: JsonBinaryEncoding::Hex,
                        parsed: false,
                        channel: Channel::FixedRate(fixed_rate(feed_id.0)),
                        formats: vec![Format::Solana],
                        ignore_invalid_feed_ids: false,
                    })
                    .expect("invalid subscription params"),
                };
                sub_id += 1;
                if let Err(err) = cli
                    .subscribe(pyth_lazer_protocol::subscription::Request::Subscribe(
                        subscribe_request,
                    ))
                    .await
                {
                    log::error!(target: "pyth", "pyth feed subscribe failed: {err:?}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }

            retries = 0u32; // retry on successful connect

            let mut stream = pyth_lazer_stream.boxed();
            loop {
                let update = match tokio::time::timeout(PYTH_FEED_STALE_LIMIT, stream.next()).await
                {
                    Ok(Some(update)) => update,
                    Ok(None) => break,
                    Err(_) => {
                        log::warn!(
                            target: "pyth",
                            "no pyth updates for {}s, reconnecting",
                            PYTH_FEED_STALE_LIMIT.as_secs()
                        );
                        break;
                    }
                };
                match update {
                    Ok(AnyResponse::Binary(outer)) => {
                        for message in outer.messages {
                            match message {
                                Message::Solana(solana) => {
                                    let mut buf = Vec::with_capacity(solana.payload.len() + 128);
                                    solana.serialize(&mut buf).expect("serialized");
                                    let data =
                                        PayloadData::deserialize_slice_le(&solana.payload).unwrap();

                                    log::trace!(target: "pyth", "got update: {data:?}");
                                    for f in data.feeds {
                                        // the program gates staleness and monotonicity on the
                                        // per-feed `FeedUpdateTimestamp` (see
                                        // `instructions/pyth_lazer_oracle.rs`), not the payload
                                        // timestamp — a fixed-rate channel keeps ticking a fresh
                                        // payload timestamp even when a feed's price is stalled,
                                        // so stamp updates with the timestamp the program checks
                                        let feed_update_ts = f
                                            .properties
                                            .iter()
                                            .find_map(|p| match p {
                                                PayloadPropertyValue::FeedUpdateTimestamp(ts) => {
                                                    *ts
                                                }
                                                _ => None,
                                            })
                                            .unwrap_or(data.timestamp_us);
                                        for p in f.properties {
                                            if let PayloadPropertyValue::Price(Some(new_price)) = p
                                            {
                                                // TODO: bulk msg to avoid bouncing around tokio, bucket in some way, one message updates multiple markets...
                                                let feed_id = f.feed_id.0;
                                                let price: u64 = new_price.0.unsigned_abs().into();

                                                if let Some(market_id) =
                                                    pyth_lazer_feed_id_to_perp_market_index(feed_id)
                                                {
                                                    let scaled_price = to_price_precision(
                                                        price,
                                                        feed_id,
                                                        MarketType::Perp,
                                                    );
                                                    let _ = price_tx.try_send(PythPriceUpdate {
                                                        market_type: MarketType::Perp,
                                                        market_id,
                                                        feed_id,
                                                        price: scaled_price,
                                                        message: buf.clone(),
                                                        ts: feed_update_ts,
                                                    });
                                                }

                                                if let Some(market_id) =
                                                    pyth_lazer_feed_id_to_spot_market_index(feed_id)
                                                {
                                                    let scaled_price = to_price_precision(
                                                        price,
                                                        feed_id,
                                                        MarketType::Spot,
                                                    );
                                                    let _ = price_tx.try_send(PythPriceUpdate {
                                                        market_type: MarketType::Spot,
                                                        market_id,
                                                        feed_id,
                                                        price: scaled_price,
                                                        message: buf.clone(),
                                                        ts: feed_update_ts,
                                                    });
                                                }

                                                // Extra feeds (cluster-specific): emit a synthetic
                                                // update so the relayer ships them. velocity-rs
                                                // derives the oracle PDA from feed_id alone, so
                                                // market_id is informational only.
                                                if extra_feeds.contains(&feed_id)
                                                    && pyth_lazer_feed_id_to_perp_market_index(
                                                        feed_id,
                                                    )
                                                    .is_none()
                                                    && pyth_lazer_feed_id_to_spot_market_index(
                                                        feed_id,
                                                    )
                                                    .is_none()
                                                {
                                                    let scaled_price = to_price_precision(
                                                        price,
                                                        feed_id,
                                                        MarketType::Spot,
                                                    );
                                                    let _ = price_tx.try_send(PythPriceUpdate {
                                                        market_type: MarketType::Spot,
                                                        market_id: u16::MAX,
                                                        feed_id,
                                                        price: scaled_price,
                                                        message: buf.clone(),
                                                        ts: feed_update_ts,
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                                _ => (),
                            }
                        }
                    }
                    other => match other {
                        Ok(AnyResponse::Json(Response::Subscribed(sub))) => {
                            log::info!(
                                target: "pyth",
                                "subscribed feed {}",
                                sub.subscription_id.0
                            );
                        }
                        Ok(AnyResponse::Json(msg)) => {
                            log::info!(target: "pyth", "control msg: {msg:?}");
                        }
                        Err(err) => {
                            log::warn!(
                                target: "pyth",
                                "websocket error from pyth stream: {err:?}"
                            );
                        }
                        Ok(other_ok) => {
                            log::info!(target: "pyth", "non-binary msg: {other_ok:?}");
                        }
                    },
                }
            }
            // stream ended, will retry
            retries += 1;
            if retries >= MAX_RETRIES {
                log::error!(
                    target: "pyth",
                    "FATAL: feed disconnected after {MAX_RETRIES} attempts; closing price feed channel"
                );
                return;
            }
            let backoff = 2u64.pow(retries).min(30);
            log::warn!(
                target: "pyth",
                "feed disconnected, retry {retries}/{MAX_RETRIES} in {backoff}s"
            );
            tokio::time::sleep(Duration::from_secs(backoff)).await;
        }
    });

    price_rx
}

#[cfg(test)]
mod tests {
    use {
        super::{
            pyth_update_is_fresh, swift_placement_expired, OrderSlotLimiter, PendingTxMeta,
            PendingTxs, Pubkey, TxIntent,
        },
        pyth_lazer_protocol::router::TimestampUs,
        solana_sdk::signature::Signature,
    };

    #[test]
    fn pending_txs_confirm_consumes_entry() {
        let mut pending = PendingTxs::<8>::new();
        let sig = Signature::from([7u8; 64]);
        pending.insert(PendingTxMeta::new(
            sig,
            TxIntent::Trigger {
                market_index: 0,
                order_id: 1,
                taker_user: Pubkey::new_unique(),
                slot: 2,
            },
            100,
        ));
        // first confirmation returns the entry
        assert!(pending.confirm(&sig).is_some());
        // a redelivered signature must not re-run the confirmation accounting
        assert!(pending.confirm(&sig).is_none());
    }

    #[test]
    fn swift_expiry_placement_deadline_binds_before_staleness() {
        // `auction_duration` is a u8 (<=255), so the placement deadline
        // (order_slot + auction_duration) always binds before the 500-slot signed-message
        // window. The order is unplaceable one slot past the deadline, well before slot 500.
        assert!(!swift_placement_expired(
            0,
            255,
            0,
            255,
            0,
            velocity_rs::program::math::time::SlotClock::baseline()
        ));
        assert!(swift_placement_expired(
            0,
            255,
            0,
            256,
            0,
            velocity_rs::program::math::time::SlotClock::baseline()
        ));
    }

    #[test]
    fn swift_expiry_placement_deadline() {
        // max_slot = order_slot + auction_duration = 130. Program rejects once max_slot < slot.
        assert!(!swift_placement_expired(
            100,
            30,
            0,
            130,
            0,
            velocity_rs::program::math::time::SlotClock::baseline()
        )); // exactly at deadline: still placeable
        assert!(swift_placement_expired(
            100,
            30,
            0,
            131,
            0,
            velocity_rs::program::math::time::SlotClock::baseline()
        )); // one past: gone
            // Zero auction duration (limit order default): only placeable in the signing slot.
        assert!(!swift_placement_expired(
            100,
            0,
            0,
            100,
            0,
            velocity_rs::program::math::time::SlotClock::baseline()
        ));
        assert!(swift_placement_expired(
            100,
            0,
            0,
            101,
            0,
            velocity_rs::program::math::time::SlotClock::baseline()
        ));
    }

    #[test]
    fn swift_expiry_max_ts() {
        // max_ts == 0 disables the ts check.
        assert!(!swift_placement_expired(
            100,
            200,
            0,
            100,
            i64::MAX,
            velocity_rs::program::math::time::SlotClock::baseline()
        ));
        // now == max_ts is still valid; now > max_ts expires.
        assert!(!swift_placement_expired(
            100,
            200,
            5_000,
            100,
            5_000,
            velocity_rs::program::math::time::SlotClock::baseline()
        ));
        assert!(swift_placement_expired(
            100,
            200,
            5_000,
            100,
            5_001,
            velocity_rs::program::math::time::SlotClock::baseline()
        ));
    }

    #[test]
    fn trigger_intent_metadata() {
        let taker = Pubkey::new_unique();
        let intent = TxIntent::Trigger {
            market_index: 3,
            order_id: 42,
            taker_user: taker,
            slot: 7,
        };
        assert_eq!(intent.label(), "trigger");
        assert!(intent.expected_trigger());
        assert_eq!(intent.expected_fill_count(), 0);
        assert_eq!(intent.slot(), Some(7));
        assert_eq!(intent.market_index(), Some(3));
        assert_eq!(intent.order_id(), Some(42));
        assert_eq!(intent.swift_uuid(), None);
        // taker subaccount must be carried so the tx event is filterable by user in Loki
        assert_eq!(intent.user(), Some(taker));
    }

    #[test]
    fn swift_place_intent_metadata() {
        let taker = Pubkey::new_unique();
        let intent = TxIntent::SwiftPlace {
            uuid: *b"abcd1234",
            market_index: 5,
            taker_user: taker,
            slot: 9,
        };
        assert_eq!(intent.label(), "swift_place");
        // place-only: no fill or trigger expected in this tx
        assert!(!intent.expected_trigger());
        assert_eq!(intent.expected_fill_count(), 0);
        assert_eq!(intent.slot(), Some(9));
        assert_eq!(intent.market_index(), Some(5));
        assert_eq!(intent.order_id(), None);
        assert_eq!(intent.swift_uuid(), Some(*b"abcd1234"));
        // even the place-only path carries the taker so its lifecycle is filterable by user
        assert_eq!(intent.user(), Some(taker));
    }

    #[test]
    fn pyth_update_freshness() {
        let update_ts = TimestampUs(1_000_000);
        // exactly at max_age: still fresh
        assert!(pyth_update_is_fresh(
            update_ts,
            TimestampUs(1_010_000),
            10_000
        ));
        // one us past max_age: stale
        assert!(!pyth_update_is_fresh(
            update_ts,
            TimestampUs(1_010_001),
            10_000
        ));
        // update from the near future (clock skew within PYTH_MAX_CLOCK_SKEW_US): fresh
        assert!(pyth_update_is_fresh(
            update_ts,
            TimestampUs(500_000),
            10_000
        ));
        // update from beyond the tolerated skew: invalid, treated as stale
        assert!(!pyth_update_is_fresh(
            TimestampUs(3_000_001),
            TimestampUs(2_000_000),
            10_000
        ));
    }

    #[test]
    fn order_slot_limiter_rejects_repeat_within_window() {
        let mut limiter: OrderSlotLimiter<40> = OrderSlotLimiter::new();
        // First trigger attempt for an order id at slot 100 is allowed.
        assert!(limiter.allow_event(100, 7));
        // Same slot again is rejected (already present).
        assert!(!limiter.allow_event(100, 7));
        // A couple slots later it is throttled (seen in generations slot-2..=slot-4).
        assert!(!limiter.allow_event(102, 7));
        // After the window passes it is allowed again.
        assert!(limiter.allow_event(110, 7));
    }
}
