use {
    futures_util::StreamExt,
    pyth_lazer_client::AnyResponse,
    pyth_lazer_protocol::{
        message::{Message, SolanaMessage},
        payload::{PayloadData, PayloadPropertyValue},
        router::{
            Channel, DeliveryFormat, FixedRate, Format, JsonBinaryEncoding, PriceFeedId,
            PriceFeedProperty, SubscriptionParams, SubscriptionParamsRepr, TimestampUs,
        },
        subscription::{Response, SubscribeRequest, SubscriptionId},
    },
    std::{collections::HashSet, time::Duration},
    velocity_rs::{
        constants::{
            perp_market_index_to_pyth_lazer_feed_id, pyth_lazer_feed_id_to_perp_market_index,
            pyth_lazer_feed_id_to_spot_market_index, spot_market_index_to_pyth_lazer_feed_id,
        },
        math::constants::PRICE_PRECISION,
        program::{
            math::{
                oracle::{
                    is_oracle_valid_for_action, oracle_validity, LogMode, OracleValidity,
                    VelocityAction,
                },
                time::{legacy_slot_duration_i64, Millis, SlotClock},
            },
            state::{
                oracle::MMOraclePriceData,
                state::{State as ProgramState, ValidityGuardRails},
            },
        },
        types::{
            accounts::{PerpMarket, State as IdlState},
            MarketId, MarketType, OraclePriceData, OracleSource, StateExt,
        },
        VelocityClient,
    },
};

/// The exchange `State` that the keeper's program gates read, loaded once per decision pass.
///
/// The client caches the IDL `State`. The program's own `State` is zero-copy and its host
/// layout differs from the onchain bytes, so `program` is a default program `State` that
/// carries only the fields the gates read: `exchange_status` for the AMM pauses, the oracle
/// validity guard rails and the slot clock. A program gate that starts reading another `State`
/// field needs that field copied here.
#[derive(Clone, Copy)]
pub struct ExchangeState {
    pub program: ProgramState,
    pub slot_clock: SlotClock,
    pub median_trigger_price: bool,
    pub liquidation_margin_buffer_ratio: u32,
}

impl ExchangeState {
    /// `None` while the `State` account is not in the client cache.
    pub fn load(velocity: &VelocityClient) -> Option<Self> {
        velocity
            .state_account()
            .ok()
            .map(|state| Self::from_idl(&state))
    }

    pub fn from_idl(state: &IdlState) -> Self {
        let mut program = ProgramState {
            exchange_status: state.exchange_status,
            slot_duration_ms: state.slot_duration_ms,
            pending_slot_duration_ms: state.pending_slot_duration_ms,
            slot_duration_effective_slot: state.slot_duration_effective_slot,
            slot_duration_transition_slots: state.slot_duration_transition_slots,
            ..ProgramState::default()
        };
        let validity = &state.oracle_guard_rails.validity;
        program.oracle_guard_rails.validity = ValidityGuardRails {
            slots_before_stale_for_amm: legacy_slot_duration_i64(
                validity.slots_before_stale_for_amm,
            ),
            slots_before_stale_for_margin: legacy_slot_duration_i64(
                validity.slots_before_stale_for_margin,
            ),
            confidence_interval_max_size: validity.confidence_interval_max_size,
            too_volatile_ratio: validity.too_volatile_ratio,
        };

        Self {
            slot_clock: program.slot_clock(),
            program,
            median_trigger_price: state.has_median_trigger_price_feature(),
            liquidation_margin_buffer_ratio: state.liquidation_margin_buffer_ratio,
        }
    }

    pub fn validity_guard_rails(&self) -> &ValidityGuardRails {
        &self.program.oracle_guard_rails.validity
    }

    /// The AMM staleness window as wall clock.
    pub fn stale_for_amm(&self) -> Millis {
        self.validity_guard_rails().stale_for_amm_ms()
    }
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

/// The oracle view the program validates for a perp fill landing at `slot`.
/// `safe` is what `get_mm_oracle_price_data` selects: the MM oracle when it is
/// as fresh as the exchange oracle and within 1% of it, else the exchange oracle.
#[derive(Clone, Copy, Debug)]
pub struct ProjectedPerpOracle {
    pub exchange_validity: OracleValidity,
    pub safe: OraclePriceData,
    pub safe_validity: OracleValidity,
    /// The MM and exchange prices the program picks `safe` from, which its AMM gates also read.
    pub mm: MMOraclePriceData,
    /// The tx's pyth-lazer post is fresher than the cached oracle, so the program uses it.
    pub uses_pyth_update: bool,
}

impl ProjectedPerpOracle {
    /// Whether a fill reads the same oracle inputs from `self` as from `other`: the raw exchange
    /// price and confidence, the safe price, confidence, delay and source, the MM-to-exchange gap
    /// the AMM volatility gate reads, the safe validity, and the exchange validity's verdict on
    /// DLOB matches, its only use in a fill.
    ///
    /// An MM-sourced safe price still carries the exchange oracle's confidence, and the fill
    /// refreshes its oracle TWAP and std from the raw exchange observation before quoting, so a
    /// post that leaves the MM oracle as the source can still change the fill. Only an identical
    /// result makes the post redundant.
    pub fn same_fill_inputs(&self, other: &Self) -> bool {
        let (a, b) = (&self.safe, &other.safe);
        let (exchange_a, exchange_b) = (
            self.mm.get_exchange_oracle_price_data(),
            other.mm.get_exchange_oracle_price_data(),
        );
        // the fill's oracle TWAP and std refresh reads the raw exchange price and confidence,
        // and the spread it quotes reads those in the same instruction
        exchange_a.price == exchange_b.price
            && exchange_a.confidence == exchange_b.confidence
            && a.price == b.price
            && a.confidence == b.confidence
            && a.delay == b.delay
            && a.sequence_id == b.sequence_id
            && self.mm.is_safe_price_mm_sourced() == other.mm.is_safe_price_mm_sourced()
            && self.mm.get_mm_exchange_diff_bps() == other.mm.get_mm_exchange_diff_bps()
            && self.safe_validity == other.safe_validity
            && exchange_match_allowed(self.exchange_validity)
                == exchange_match_allowed(other.exchange_validity)
    }
}

fn exchange_match_allowed(validity: OracleValidity) -> bool {
    is_oracle_valid_for_action(validity, Some(VelocityAction::FillOrderMatch)).unwrap_or(false)
}

/// The exchange oracle a tx landing at `slot` reads.
#[derive(Clone, Copy, Debug)]
pub struct ExchangeObservation {
    pub data: OraclePriceData,
    /// The tx's pyth-lazer post is newer than the cached oracle, so the program reads the post.
    pub uses_pyth_update: bool,
}

/// The exchange oracle a tx landing at `slot` reads: the cached observation with its delay aged
/// to `slot`, or the update the tx posts when it is newer. `None` while the oracle is not
/// cached.
///
/// Every view of the tx (oracle validity, the safe price, the AMM quote projection) must start
/// from this one observation, or the views can select different safe prices.
pub fn observe_exchange_oracle(
    velocity: &VelocityClient,
    market: &PerpMarket,
    slot: u64,
    pyth_price_update: Option<&PythPriceUpdate>,
) -> Option<ExchangeObservation> {
    let oracle =
        velocity.try_get_oracle_price_data_and_slot(MarketId::perp(market.market_index))?;
    let mut cached = oracle.data;
    cached.delay = cached
        .delay
        .saturating_add(i64::try_from(slot.saturating_sub(oracle.slot)).unwrap_or(i64::MAX));

    // The program accepts a post on feed-timestamp freshness alone (a same-price message still
    // refreshes staleness), so the preview keys on freshness too. The previewed oracle is parsed
    // from the retained signed message with the confidence the program would store.
    let preview = pyth_price_update
        .filter(|update| {
            update.market_type == MarketType::Perp && update.market_id == market.market_index
        })
        .and_then(|update| preview_pyth_lazer_oracle(update, &market.oracle_source))
        .filter(|preview| match (preview.sequence_id, cached.sequence_id) {
            (Some(next), Some(current)) => next > current,
            (Some(_), None) => true,
            (None, _) => false,
        });

    Some(match preview {
        Some(data) => ExchangeObservation {
            data,
            uses_pyth_update: true,
        },
        None => ExchangeObservation {
            data: cached,
            uses_pyth_update: false,
        },
    })
}

/// Classify `observation` and the safe price the program selects from it at `slot`.
pub fn classify_perp_oracle(
    exchange: &ExchangeState,
    market: &PerpMarket,
    observation: ExchangeObservation,
    slot: u64,
) -> Option<ProjectedPerpOracle> {
    perp_oracle_validity(
        market,
        observation.data,
        observation.uses_pyth_update,
        slot,
        exchange.validity_guard_rails(),
        exchange.slot_clock,
    )
}

/// The safe oracle price a tx that posts nothing reads for `market_index` at `slot`. `None`
/// while the market or its oracle is not cached.
pub fn chain_safe_price(
    velocity: &VelocityClient,
    exchange: &ExchangeState,
    market_index: u16,
    slot: u64,
) -> Option<i64> {
    let market = velocity.try_get_perp_market_account(market_index).ok()?;
    project_perp_oracle(velocity, exchange, &market, slot, None).map(|oracle| oracle.safe.price)
}

/// [`observe_exchange_oracle`] then [`classify_perp_oracle`].
pub fn project_perp_oracle(
    velocity: &VelocityClient,
    exchange: &ExchangeState,
    market: &PerpMarket,
    slot: u64,
    pyth_price_update: Option<&PythPriceUpdate>,
) -> Option<ProjectedPerpOracle> {
    let observation = observe_exchange_oracle(velocity, market, slot, pyth_price_update)?;
    classify_perp_oracle(exchange, market, observation, slot)
}

/// Classifies `exchange_oracle` (already aged to `slot`) and the safe price
/// the program selects from it, as `update_amm_and_check_validity` and
/// `fill_perp_order` do.
pub(crate) fn perp_oracle_validity(
    market: &PerpMarket,
    exchange_oracle: OraclePriceData,
    uses_pyth_update: bool,
    slot: u64,
    validity_guard_rails: &ValidityGuardRails,
    slot_clock: SlotClock,
) -> Option<ProjectedPerpOracle> {
    let validity = |price_data: &OraclePriceData, log_mode, price_is_mm_sourced| {
        oracle_validity(
            MarketType::Perp,
            market.market_index,
            market
                .market_stats
                .historical_oracle_data
                .last_oracle_price_twap,
            price_data,
            validity_guard_rails,
            market.get_max_confidence_interval_multiplier().ok()?,
            &market.oracle_source,
            log_mode,
            market.oracle_slot_delay_override,
            price_is_mm_sourced,
            market.oracle_low_risk_slot_delay_override,
            slot,
            slot_clock,
        )
        .ok()
    };

    let exchange_validity = validity(&exchange_oracle, LogMode::ExchangeOracle, false)?;
    let mm_oracle = market
        .get_mm_oracle_price_data(exchange_oracle, slot, validity_guard_rails, slot_clock)
        .ok()?;
    let safe = mm_oracle.get_safe_oracle_price_data();
    let safe_validity = validity(
        &safe,
        LogMode::SafeMMOracle,
        mm_oracle.is_safe_price_mm_sourced(),
    )?;

    Some(ProjectedPerpOracle {
        exchange_validity,
        safe,
        safe_validity,
        mm: mm_oracle,
        uses_pyth_update,
    })
}

/// The oracle state `update_pyth_lazer_oracle` would persist for `update`,
/// read back the way `get_pyth_price` reads it, with `delay: 0` since the
/// posting tx stamps the current slot. Mirrors the program end to end:
/// confidence is the widest of the 20bps floor, the bid/ask distance and the
/// signed confidence property (`calculate_lazer_conf`), price and confidence
/// scale by the feed exponent and the source multiple, and a stablecoin
/// source snaps to $1 inside the tighter of 5bps and the confidence
/// (`get_pyth_stable_coin_price`). Returns `None` when the retained message
/// does not parse or does not carry the update's feed.
pub fn preview_pyth_lazer_oracle(
    update: &PythPriceUpdate,
    oracle_source: &OracleSource,
) -> Option<OraclePriceData> {
    let message = SolanaMessage::deserialize_slice(&update.message).ok()?;
    let data = PayloadData::deserialize_slice_le(&message.payload).ok()?;
    let feed = data
        .feeds
        .iter()
        .find(|feed| feed.feed_id.0 == update.feed_id)?;

    let mut price: Option<i64> = None;
    let mut best_bid: Option<i64> = None;
    let mut best_ask: Option<i64> = None;
    let mut exponent: Option<i16> = None;
    let mut signed_confidence: Option<i64> = None;
    let mut feed_ts_us: Option<u64> = None;

    for property in &feed.properties {
        match property {
            PayloadPropertyValue::Price(value) => price = value.map(|p| p.0.get()),
            PayloadPropertyValue::BestBidPrice(value) => best_bid = value.map(|p| p.0.get()),
            PayloadPropertyValue::BestAskPrice(value) => best_ask = value.map(|p| p.0.get()),
            PayloadPropertyValue::Exponent(exp) => exponent = Some(*exp),
            PayloadPropertyValue::Confidence(value) => signed_confidence = value.map(|p| p.0.get()),
            PayloadPropertyValue::FeedUpdateTimestamp(ts) => feed_ts_us = ts.map(|t| t.0),
            _ => {}
        }
    }

    let price = price.filter(|p| *p != 0)?;
    let exponent = exponent?;

    // widest-of-three confidence, as `calculate_lazer_conf` stores it
    let mut conf = price / 500;
    if let (Some(bid), Some(ask)) = (best_bid, best_ask) {
        let spread = i128::from(ask)
            .saturating_sub(i128::from(bid))
            .abs()
            .min(i64::MAX.into()) as i64;
        conf = conf.max(spread);
    }
    if let Some(signed_confidence) = signed_confidence {
        conf = conf.max(signed_confidence);
    }

    // scale mantissas to PRICE_PRECISION, as `get_pyth_price` reads them
    let multiple = match oracle_source {
        OracleSource::PythLazer | OracleSource::PythLazerStableCoin => 1u128,
        OracleSource::PythLazer1K => 1_000,
        OracleSource::PythLazer1M => 1_000_000,
        _ => return None,
    };
    let precision = 10_u128.checked_pow(u32::from(exponent.unsigned_abs()))?;
    if precision <= multiple {
        return None;
    }
    let precision = precision / multiple;
    let (scale_mult, scale_div) = if precision > PRICE_PRECISION {
        (1u128, precision / PRICE_PRECISION)
    } else {
        (PRICE_PRECISION / precision, 1u128)
    };

    let mut price_scaled = i64::try_from(
        i128::from(price)
            .checked_mul(scale_mult as i128)?
            .checked_div(scale_div as i128)?,
    )
    .ok()?;
    let conf_scaled = u64::try_from(
        u128::from(conf.unsigned_abs())
            .checked_mul(scale_mult)?
            .checked_div(scale_div)?,
    )
    .ok()?;

    if matches!(oracle_source, OracleSource::PythLazerStableCoin) {
        let five_bps = 500_i64;
        if (price_scaled - PRICE_PRECISION as i64).abs()
            <= five_bps.min(i64::try_from(conf_scaled).unwrap_or(i64::MAX))
        {
            price_scaled = PRICE_PRECISION as i64;
        }
    }

    Some(OraclePriceData {
        price: price_scaled,
        confidence: conf_scaled,
        delay: 0,
        has_sufficient_number_of_data_points: true,
        sequence_id: feed_ts_us,
    })
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
            for (sub_id, feed_id) in feed_ids.iter().enumerate() {
                let subscribe_request = SubscribeRequest {
                    subscription_id: SubscriptionId(sub_id as u64),
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
                            if let Message::Solana(solana) = message {
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
                                            PayloadPropertyValue::FeedUpdateTimestamp(ts) => *ts,
                                            _ => None,
                                        })
                                        .unwrap_or(data.timestamp_us);
                                    for p in f.properties {
                                        if let PayloadPropertyValue::Price(Some(new_price)) = p {
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
                                                && pyth_lazer_feed_id_to_perp_market_index(feed_id)
                                                    .is_none()
                                                && pyth_lazer_feed_id_to_spot_market_index(feed_id)
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
            perp_oracle_validity, preview_pyth_lazer_oracle, pyth_update_is_fresh, PythPriceUpdate,
        },
        pyth_lazer_protocol::{
            message::SolanaMessage,
            payload::{PayloadData, PayloadFeedData, PayloadPropertyValue},
            router::{ChannelId, Price, PriceFeedId, TimestampUs},
        },
        std::num::NonZeroI64,
        velocity_rs::{
            program::math::time::SlotClock,
            types::{MarketType, OracleSource},
        },
    };

    // The program validates the fill's safe oracle, not the raw MM slot. With
    // the MM oracle ~34h stale (ZEC-PERP, never cranked) the safe price is the
    // exchange oracle, whose immediate threshold is zero: a same-tx pyth-lazer
    // post (delay 0) passes, one slot old does not. A fresh MM oracle is the
    // safe price and passes within its write gap.
    #[test]
    fn immediate_fill_validity_follows_safe_oracle_source() {
        use velocity_rs::{
            program::{
                math::{
                    oracle::{is_oracle_valid_for_action, VelocityAction},
                    time::legacy_slot_duration_i64,
                },
                state::state::ValidityGuardRails,
            },
            types::{accounts::PerpMarket, OraclePriceData},
        };

        let guard_rails = ValidityGuardRails {
            slots_before_stale_for_amm: legacy_slot_duration_i64(10),
            slots_before_stale_for_margin: legacy_slot_duration_i64(120),
            confidence_interval_max_size: 20_000,
            too_volatile_ratio: 5,
        };
        let slot = 449_781_953;
        let price = 1_515_532_437;
        let exchange_seq = 1_790_187_409_000_000;
        let exchange = |delay| OraclePriceData {
            price,
            confidence: (price / 500) as u64,
            delay,
            has_sufficient_number_of_data_points: true,
            sequence_id: Some(exchange_seq),
        };
        let immediate_ok = |market: &PerpMarket, delay| {
            let projected = perp_oracle_validity(
                market,
                exchange(delay),
                delay == 0,
                slot,
                &guard_rails,
                SlotClock::baseline(),
            )
            .expect("classifies");
            let ok = is_oracle_valid_for_action(
                projected.safe_validity,
                Some(VelocityAction::FillOrderAmmImmediate),
            )
            .unwrap();
            (projected.safe.price, ok)
        };

        let mut market = PerpMarket {
            market_index: 4,
            oracle_source: OracleSource::PythLazer,
            oracle_slot_delay_override: -1,
            ..Default::default()
        };
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap = price;
        market.market_stats.mm_oracle_price = 1_506_838_551;
        market.market_stats.mm_oracle_slot = slot - 458_505;
        market.market_stats.mm_oracle_sequence_id = 1_790_065_354_600_000;
        assert_eq!(
            immediate_ok(&market, 0),
            (price, true),
            "same-tx lazer post"
        );
        assert_eq!(
            immediate_ok(&market, 1),
            (price, false),
            "no post this slot"
        );

        let mm_price = price + price / 1_000; // within the 1% fallback band
        market.market_stats.mm_oracle_price = mm_price;
        market.market_stats.mm_oracle_slot = slot - 1;
        market.market_stats.mm_oracle_sequence_id = exchange_seq + 1;
        assert_eq!(
            immediate_ok(&market, 3),
            (mm_price, true),
            "fresh MM oracle"
        );
    }

    #[test]
    fn post_is_redundant_only_when_the_fill_reads_the_same_inputs() {
        use velocity_rs::{
            program::{math::time::legacy_slot_duration_i64, state::state::ValidityGuardRails},
            types::{accounts::PerpMarket, OraclePriceData},
        };

        let guard_rails = ValidityGuardRails {
            slots_before_stale_for_amm: legacy_slot_duration_i64(10),
            slots_before_stale_for_margin: legacy_slot_duration_i64(120),
            confidence_interval_max_size: 20_000,
            too_volatile_ratio: 5,
        };
        let slot = 449_781_953;
        let price = 1_515_532_437;
        let cached_seq = 1_790_187_409_000_000;
        let oracle = |price: i64, delay, sequence_id| OraclePriceData {
            price,
            confidence: (price / 500) as u64,
            delay,
            has_sufficient_number_of_data_points: true,
            sequence_id: Some(sequence_id),
        };
        // the cached exchange oracle, 3 slots old, and the update a fill tx would post
        let cached = oracle(price, 3, cached_seq);
        let posted = oracle(price, 0, cached_seq + 1);

        let mut market = PerpMarket {
            market_index: 4,
            oracle_source: OracleSource::PythLazer,
            oracle_slot_delay_override: -1,
            ..Default::default()
        };
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap = price;
        // a cranked MM oracle, newer than the update and within 1% of it
        market.market_stats.mm_oracle_price = price + price / 1_000;
        market.market_stats.mm_oracle_slot = slot - 1;
        market.market_stats.mm_oracle_sequence_id = cached_seq + 2;

        let project = |market: &PerpMarket, exchange: OraclePriceData, uses_pyth_update| {
            perp_oracle_validity(
                market,
                exchange,
                uses_pyth_update,
                slot,
                &guard_rails,
                SlotClock::baseline(),
            )
            .expect("classifies")
        };

        // the MM oracle stays the safe price and the update repeats the cached price
        let with_post = project(&market, posted, true);
        assert!(with_post.mm.is_safe_price_mm_sourced());
        assert!(with_post.same_fill_inputs(&project(&market, cached, false)));

        // a moved exchange price moves the MM price's confidence and the MM-to-exchange gap
        let moved = oracle(price + price / 2_000, 0, cached_seq + 1);
        assert!(!project(&market, moved, true).same_fill_inputs(&project(&market, cached, false)));

        // no MM crank (ZEC-PERP): the exchange oracle is the safe price, and only the post
        // makes it same-slot fresh
        market.market_stats.mm_oracle_slot = slot - 458_505;
        market.market_stats.mm_oracle_sequence_id = 1_790_065_354_600_000;
        let with_post = project(&market, posted, true);
        assert!(!with_post.mm.is_safe_price_mm_sourced());
        assert!(!with_post.same_fill_inputs(&project(&market, cached, false)));
    }

    #[test]
    fn post_with_a_different_exchange_price_is_not_redundant() {
        // Two exchange prices one unit either side of the MM price give the same MM-sourced
        // safe observation and a gap that rounds to zero, but the fill refreshes its oracle
        // TWAP and std from the raw exchange price, so the post still changes the fill.
        use velocity_rs::{
            program::{math::time::legacy_slot_duration_i64, state::state::ValidityGuardRails},
            types::{accounts::PerpMarket, OraclePriceData},
        };

        let guard_rails = ValidityGuardRails {
            slots_before_stale_for_amm: legacy_slot_duration_i64(10),
            slots_before_stale_for_margin: legacy_slot_duration_i64(120),
            confidence_interval_max_size: 20_000,
            too_volatile_ratio: 5,
        };
        let slot = 1_000;
        let mm_price = 100_000_000;
        let mut market = PerpMarket {
            oracle_source: OracleSource::PythLazer,
            oracle_slot_delay_override: -1,
            ..Default::default()
        };
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap = mm_price;
        market.market_stats.mm_oracle_price = mm_price;
        market.market_stats.mm_oracle_slot = slot - 1;
        market.market_stats.mm_oracle_sequence_id = 1_000_000_010;

        let project = |price, sequence_id, uses_pyth_update| {
            let exchange = OraclePriceData {
                price,
                confidence: 50_000,
                delay: 0,
                has_sufficient_number_of_data_points: true,
                sequence_id: Some(sequence_id),
            };
            perp_oracle_validity(
                &market,
                exchange,
                uses_pyth_update,
                slot,
                &guard_rails,
                SlotClock::baseline(),
            )
            .expect("classifies")
        };
        let cached = project(99_999_999, 1_000_000_000, false);
        let posted = project(100_000_001, 1_000_000_001, true);

        assert!(cached.mm.is_safe_price_mm_sourced() && posted.mm.is_safe_price_mm_sourced());
        assert_eq!(cached.safe.price, posted.safe.price);
        assert_eq!(cached.safe.confidence, posted.safe.confidence);
        assert_eq!(
            cached.mm.get_mm_exchange_diff_bps(),
            posted.mm.get_mm_exchange_diff_bps()
        );
        assert!(!posted.same_fill_inputs(&cached));
    }

    /// One lazer solana envelope carrying a single feed with the given
    /// properties, retained the way `subscribe_price_feeds` retains it.
    fn lazer_message(feed_id: u32, properties: Vec<PayloadPropertyValue>) -> Vec<u8> {
        let payload = PayloadData {
            timestamp_us: TimestampUs(0),
            channel_id: ChannelId(1),
            feeds: vec![PayloadFeedData {
                feed_id: PriceFeedId(feed_id),
                properties,
            }],
        };
        let mut payload_buf = Vec::new();
        payload
            .serialize::<byteorder::LE>(&mut payload_buf)
            .unwrap();
        let message = SolanaMessage {
            payload: payload_buf,
            signature: [0u8; 64],
            public_key: [0u8; 32],
        };
        let mut buf = Vec::new();
        message.serialize(&mut buf).unwrap();
        buf
    }

    fn price(mantissa: i64) -> Option<Price> {
        Some(Price(NonZeroI64::new(mantissa).unwrap()))
    }

    #[test]
    fn lazer_preview_matches_program_post_semantics() {
        // mantissas at exponent -8; PRICE_PRECISION is 1e6, so the read path
        // divides by 100
        let mantissa = 100_00000000_i64;
        let feed_ts = 1_700_000_000_000_000_u64;
        let message = lazer_message(
            5,
            vec![
                PayloadPropertyValue::Price(price(mantissa)),
                PayloadPropertyValue::BestBidPrice(price(mantissa - 300_000_000)),
                PayloadPropertyValue::BestAskPrice(price(mantissa + 300_000_000)),
                PayloadPropertyValue::Exponent(-8),
                PayloadPropertyValue::Confidence(price(mantissa / 1000)),
                PayloadPropertyValue::FeedUpdateTimestamp(Some(TimestampUs(feed_ts))),
            ],
        );
        let update = PythPriceUpdate {
            market_type: MarketType::Perp,
            market_id: 0,
            feed_id: 5,
            price: 100_000_000,
            message,
            ts: TimestampUs(feed_ts),
        };

        let preview = preview_pyth_lazer_oracle(&update, &OracleSource::PythLazer).unwrap();
        assert_eq!(preview.price, 100_000_000); // $100 at 1e6 precision
                                                // the bid/ask spread (600_000_000 raw) is the widest of the three
                                                // confidence signals; scaled by 100 like the price
        assert_eq!(preview.confidence, 6_000_000);
        assert_eq!(preview.delay, 0);
        assert_eq!(preview.sequence_id, Some(feed_ts));

        // signed confidence wins when it is widest
        let message = lazer_message(
            5,
            vec![
                PayloadPropertyValue::Price(price(mantissa)),
                PayloadPropertyValue::Exponent(-8),
                PayloadPropertyValue::Confidence(price(mantissa / 10)),
                PayloadPropertyValue::FeedUpdateTimestamp(Some(TimestampUs(feed_ts))),
            ],
        );
        let update = PythPriceUpdate { message, ..update };
        let preview = preview_pyth_lazer_oracle(&update, &OracleSource::PythLazer).unwrap();
        assert_eq!(preview.confidence, 10_000_000);

        // no properties beyond price: the 20bps floor stands
        let message = lazer_message(
            5,
            vec![
                PayloadPropertyValue::Price(price(mantissa)),
                PayloadPropertyValue::Exponent(-8),
                PayloadPropertyValue::FeedUpdateTimestamp(Some(TimestampUs(feed_ts))),
            ],
        );
        let update = PythPriceUpdate { message, ..update };
        let preview = preview_pyth_lazer_oracle(&update, &OracleSource::PythLazer).unwrap();
        assert_eq!(preview.confidence, 200_000);

        // a message without the update's feed does not preview
        let message = lazer_message(
            6,
            vec![
                PayloadPropertyValue::Price(price(mantissa)),
                PayloadPropertyValue::Exponent(-8),
            ],
        );
        let update = PythPriceUpdate { message, ..update };
        assert!(preview_pyth_lazer_oracle(&update, &OracleSource::PythLazer).is_none());
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
}
