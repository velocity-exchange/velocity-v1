//! Example Liquidator Bot
//!
//! Subscribes to velocity accounts, market, and oracles via gRPC.
//! Identifies liquidatable accounts and forwards them to a strategy impl
//! for processing.
//!
//! The default strategy tries to liquidate perp positions against resting orders
//!
use {
    crate::{
        filler::{TxSender, TxWorker, MAX_COMPUTE_UNITS},
        http::{
            DashboardState, DashboardStateRef, HighRiskUser, MarginStatus, Metrics,
            OraclePriceInfo, UserMarginStatus,
        },
        util::{preview_pyth_lazer_oracle, PerpFillFallback, PythPriceUpdate, TxIntent},
        Config, UseMarkets,
    },
    anchor_lang::Discriminator,
    dashmap::DashMap,
    futures_util::FutureExt,
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_sdk::{clock::Slot, signature::Signature},
    std::{
        collections::{BTreeMap, HashMap, HashSet},
        sync::{Arc, RwLock},
        time::{Duration, SystemTime, UNIX_EPOCH},
    },
    tokio::sync::mpsc::error::TryRecvError,
    velocity_rs::{
        constants::{derive_clob_authority, derive_clob_crank_conditions},
        dlob::{DLOBNotifier, L3Order, DLOB},
        grpc::{
            grpc_subscriber::{AccountFilter, GrpcConnectionOpts},
            TransactionUpdate,
        },
        jupiter::JupiterSwapApi,
        market_state::{MarketStateData, SimplifiedMarginCalculation},
        math::{
            constants::{
                BASE_PRECISION, MARGIN_PRECISION_U128, PRICE_PRECISION, QUOTE_PRECISION,
                SPOT_WEIGHT_PRECISION_U128,
            },
            liquidation::{calculate_collateral, CollateralInfo},
            tiers::{perp_tier_is_as_safe_as, AssetTierExt, ContractTierExt},
        },
        priority_fee_subscriber::PriorityFeeSubscriber,
        program::{
            instructions::ForceCancelClobRefV0,
            math::{
                oracle::{
                    is_oracle_valid_for_action, oracle_validity, LogMode, OracleValidity,
                    VelocityAction,
                },
                time::{Millis, SlotDuration},
            },
            state::prop_amm::{ClobOrderRefV0, ClobSide, QuoterV0},
        },
        titan::{self, TitanSwapApi},
        types::{
            accounts::{PerpMarket, SpotMarket, User},
            MarginRequirementType, MarketId, MarketStatus, MarketType, OraclePriceData,
            OracleSource, OrderParams, OrderType, PerpPosition, PositionDirection, SpotBalanceType,
            SpotPosition,
        },
        ClobFillAccounts, GrpcSubscribeOpts, MarketState, Pubkey, TransactionBuilder,
        VelocityClient,
    },
};

/// min wall-clock time between successive liquidation attempts on same user
/// (expressed in actual slots at the current slot duration)
const LIQUIDATION_RATE_LIMIT: Millis = Millis::from_secs(2);

/// Maximum time allowed for a liquidation attempt in milliseconds
const LIQUIDATION_DEADLINE_MS: u64 = 1_000;

/// Maximum age for liquidation entries in milliseconds
const MAX_LIQUIDATION_AGE_MS: u64 = 1_000;

/// Base cooldown in milliseconds after a failed liquidation (doubles each failure)
const FAILURE_COOLDOWN_BASE_MS: u64 = 5_000;

/// Maximum cooldown in milliseconds (cap for exponential backoff) — 5 minutes
const FAILURE_COOLDOWN_MAX_MS: u64 = 300_000;

const TARGET: &str = "liquidator";

/// Consecutive-failure limit for gRPC callback data errors before panicking.
///
/// A transient decode/lookup failure is skipped (with a warn log); a persistent one
/// means the feed is effectively dead, so panic the subscription thread — its channel
/// senders drop, the main loop sees the closed channel and exits, and the service
/// restarts rather than silently continuing on frozen data.
const GRPC_CALLBACK_FAILURE_LIMIT: u32 = 1_000;

/// Tracks per-account liquidation attempt history for backoff
#[derive(Clone, Debug)]
struct LiquidationAttemptTracker {
    /// Number of consecutive failed/no-effect attempts
    consecutive_failures: u32,
    /// Timestamp (ms) of the last attempt
    last_attempt_ms: u64,
    /// Slot of the last attempt
    last_attempt_slot: u64,
}

impl LiquidationAttemptTracker {
    fn new(slot: u64) -> Self {
        Self {
            consecutive_failures: 0,
            last_attempt_ms: current_time_millis(),
            last_attempt_slot: slot,
        }
    }

    /// Returns the cooldown duration in ms based on consecutive failures
    fn cooldown_ms(&self) -> u64 {
        if self.consecutive_failures == 0 {
            return 0;
        }
        let cooldown = FAILURE_COOLDOWN_BASE_MS * (1u64 << (self.consecutive_failures - 1).min(10));
        cooldown.min(FAILURE_COOLDOWN_MAX_MS)
    }

    /// Whether enough time has passed since the last attempt
    fn is_cooled_down(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_attempt_ms) >= self.cooldown_ms()
    }

    fn record_attempt(&mut self, slot: u64) {
        self.last_attempt_ms = current_time_millis();
        self.last_attempt_slot = slot;
        self.consecutive_failures += 1;
    }

    fn reset(&mut self) {
        self.consecutive_failures = 0;
    }
}

/// Threshold for considering a user high-risk: free margin < 10% of margin requirement
const HIGH_RISK_FREE_MARGIN_RATIO: f64 = 0.1;

/// Maximum oracle price age before considering stale (~20s, expressed in
/// actual slots at the current slot duration)
const MAX_ORACLE_AGE: Millis = Millis::from_secs(20);
/// Maximum age for Pyth prices in milliseconds before considering stale
const MAX_PYTH_AGE_MS: u64 = 5000;

/// Permanently blocked spot markets (untradable tokens)
const BLOCKED_SPOT_MARKETS: &[u16] = &[40];

/// Metadata tracking for user accounts to detect staleness
#[derive(Clone, Debug)]
struct UserAccountMetadata {
    user: User,
    last_updated_slot: u64,
    last_updated_timestamp_ms: u64,
}

/// Metadata tracking for oracle prices to detect staleness
#[derive(Clone, Debug)]
struct OraclePriceMetadata {
    price_data: OraclePriceData,
    last_updated_slot: u64,
    last_updated_timestamp_ms: u64,
}

/// Errors indicating data staleness
#[derive(Debug, Clone)]
enum StalenessError {
    OraclePriceStale { market: MarketId, age_slots: u64 },
    PythPriceStale { market_id: u16, age_ms: u64 },
}

/// Helper to get current time in milliseconds since epoch
fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Validate that the oracle prices backing `user_meta`'s positions are fresh enough.
///
/// Deliberately does NOT check user-account age: gRPC only pushes account updates
/// on change, so an idle account is old but not stale.
fn validate_data_freshness(
    user_meta: &UserAccountMetadata,
    oracle_prices: &HashMap<MarketId, OraclePriceMetadata>,
    current_slot: u64,
    slot_duration: SlotDuration,
) -> Result<(), StalenessError> {
    let max_oracle_age_slots = MAX_ORACLE_AGE.to_slots(slot_duration);
    // Check oracle prices for all markets user has positions in
    for pos in &user_meta.user.perp_positions {
        if pos.base_asset_amount != 0 {
            let market_id = MarketId::perp(pos.market_index);
            if let Some(oracle_meta) = oracle_prices.get(&market_id) {
                let oracle_age_slots = current_slot.saturating_sub(oracle_meta.last_updated_slot);
                if oracle_age_slots > max_oracle_age_slots {
                    return Err(StalenessError::OraclePriceStale {
                        market: market_id,
                        age_slots: oracle_age_slots,
                    });
                }
            }
        }
    }

    for pos in &user_meta.user.spot_positions {
        if !pos.is_available() {
            let market_id = MarketId::spot(pos.market_index);
            if let Some(oracle_meta) = oracle_prices.get(&market_id) {
                let oracle_age_slots = current_slot.saturating_sub(oracle_meta.last_updated_slot);
                if oracle_age_slots > max_oracle_age_slots {
                    return Err(StalenessError::OraclePriceStale {
                        market: market_id,
                        age_slots: oracle_age_slots,
                    });
                }
            }
        }
    }

    Ok(())
}

/// Validate Pyth price freshness
fn validate_pyth_price_freshness(pyth_update: &PythPriceUpdate) -> Result<(), StalenessError> {
    let now_ms = current_time_millis();
    // Pyth timestamp is in microseconds, convert to milliseconds
    let pyth_ts_ms = pyth_update.ts.0 / 1000;
    let age_ms = now_ms.saturating_sub(pyth_ts_ms);

    if age_ms > MAX_PYTH_AGE_MS {
        return Err(StalenessError::PythPriceStale {
            market_id: pyth_update.market_id,
            age_ms,
        });
    }

    Ok(())
}

/// Update dashboard state with current high-risk users and oracle prices
async fn update_dashboard_state(
    velocity: &VelocityClient,
    dashboard_state: &DashboardStateRef,
    users: &BTreeMap<Pubkey, UserAccountMetadata>,
    oracle_prices: &HashMap<MarketId, OraclePriceMetadata>,
    high_risk: &HashSet<Pubkey>,
    current_slot: u64,
    market_state: &'static MarketState,
    liquidation_margin_buffer_ratio: u32,
) {
    let now_ms = current_time_millis();
    let mut high_risk_users = Vec::new();

    for pubkey in high_risk {
        if let Some(user_meta) = users.get(pubkey) {
            let margin_info = match market_state.calculate_simplified_margin_requirement(
                &user_meta.user,
                MarginRequirementType::Maintenance,
                Some(liquidation_margin_buffer_ratio),
            ) {
                Ok(info) => info,
                Err(_) => continue,
            };

            let free_margin = margin_info.total_collateral - margin_info.margin_requirement as i128;
            let free_margin_ratio = if margin_info.margin_requirement > 0 {
                free_margin as f64 / margin_info.margin_requirement as f64
            } else {
                0.0
            };

            let status = check_margin_status(&margin_info);
            let display_status = if status.is_liquidatable() {
                MarginStatus::Liquidatable
            } else if status.is_at_risk() {
                MarginStatus::HighRisk
            } else {
                continue;
            };

            let mut positions = Vec::new();
            for pos in &user_meta.user.perp_positions {
                if pos.base_asset_amount != 0 {
                    positions.push(crate::http::PositionInfo {
                        market_type: crate::http::MarketType::Perp,
                        market_index: pos.market_index,
                        base_asset_amount: pos.base_asset_amount,
                        quote_asset_amount: pos.quote_asset_amount,
                    });
                }
            }
            for pos in &user_meta.user.spot_positions {
                if pos.scaled_balance != 0 {
                    // Calculate quote_asset_amount = base_asset_amount * spot oracle price
                    // Note: base_asset_amount calculation would require spot market access
                    // which isn't directly available from MarketState in this context.
                    // We'll calculate an approximation using scaled_balance and oracle price.
                    let spot_market = match velocity.try_get_spot_market_account(pos.market_index) {
                        Ok(market) => market,
                        Err(_) => continue,
                    };
                    let (base, quote) = if let Some(oracle_meta) =
                        oracle_prices.get(&MarketId::spot(pos.market_index))
                    {
                        let base = match pos.get_signed_token_amount(&spot_market) {
                            Ok(amount) => amount,
                            Err(_) => continue,
                        };
                        (
                            base,
                            base.saturating_mul(oracle_meta.price_data.price as i128)
                                / PRICE_PRECISION as i128,
                        )
                    } else {
                        (0, 0) // No oracle price available
                    };

                    positions.push(crate::http::PositionInfo {
                        market_type: crate::http::MarketType::Spot,
                        market_index: pos.market_index,
                        base_asset_amount: base as i64,
                        quote_asset_amount: quote as i64,
                    });
                }
            }

            high_risk_users.push(HighRiskUser {
                pubkey: pubkey.to_string(),
                authority: user_meta.user.authority.to_string(),
                total_collateral: margin_info.total_collateral,
                margin_requirement: margin_info.margin_requirement,
                free_margin,
                free_margin_ratio,
                status: display_status,
                last_updated_slot: user_meta.last_updated_slot,
                last_updated_ms: user_meta.last_updated_timestamp_ms,
                positions,
            });
        }
    }

    let mut oracle_price_infos = Vec::new();
    for (market_id, oracle_meta) in oracle_prices {
        let age_slots = current_slot.saturating_sub(oracle_meta.last_updated_slot);
        let age_ms = now_ms.saturating_sub(oracle_meta.last_updated_timestamp_ms);
        let is_stale = age_slots
            > MAX_ORACLE_AGE.to_slots(crate::util::client_slot_duration(velocity, current_slot));

        oracle_price_infos.push(OraclePriceInfo {
            market_type: if market_id.is_perp() {
                crate::http::MarketType::Perp
            } else {
                crate::http::MarketType::Spot
            },
            market_index: market_id.index(),
            price: oracle_meta.price_data.price,
            last_updated_slot: oracle_meta.last_updated_slot,
            last_updated_ms: oracle_meta.last_updated_timestamp_ms,
            age_slots,
            age_ms,
            is_stale,
        });
    }

    let dashboard_data = DashboardState {
        high_risk_users,
        oracle_prices: oracle_price_infos,
        current_slot,
        last_updated_ms: now_ms,
    };

    *dashboard_state.write().await = Some(dashboard_data);
}

/// Check margin status (liquidatable, high-risk, or safe)
///
/// Mirrors the program's liquidation eligibility checks
/// (`MarginCalculation::meets_cross_margin_requirement` /
/// `IsolatedMarginCalculation::meets_margin_requirement`): liquidatable iff
/// `total_collateral < margin_requirement`, using the unbuffered requirement.
/// Insolvent positions (`total_collateral <= 0`) are liquidatable, not skippable.
fn check_margin_status(margin_info: &SimplifiedMarginCalculation) -> UserMarginStatus {
    let mut isolated = Vec::with_capacity(8);

    // Check isolated positions
    for calc in &margin_info.isolated_margin_calculations {
        if calc.is_empty() {
            continue;
        }

        if calc.total_collateral < calc.margin_requirement as i128 {
            isolated.push((calc.market_index, MarginStatus::Liquidatable));
        } else if calc.margin_requirement > 0 {
            let free_margin = calc.total_collateral - calc.margin_requirement as i128;
            let free_margin_ratio = free_margin as f64 / calc.margin_requirement as f64;
            if free_margin_ratio < HIGH_RISK_FREE_MARGIN_RATIO {
                isolated.push((calc.market_index, MarginStatus::HighRisk));
            }
        }
    }

    // Check cross margin
    let cross = if margin_info.total_collateral < margin_info.margin_requirement as i128 {
        MarginStatus::Liquidatable
    } else if margin_info.margin_requirement > 0 {
        let free_margin = margin_info.total_collateral - margin_info.margin_requirement as i128;
        let free_margin_ratio = free_margin as f64 / margin_info.margin_requirement as f64;
        if free_margin_ratio < HIGH_RISK_FREE_MARGIN_RATIO {
            MarginStatus::HighRisk
        } else {
            MarginStatus::Safe
        }
    } else {
        MarginStatus::Safe
    };

    UserMarginStatus { cross, isolated }
}

/// Outcome of a liquidation attempt, used to drive backoff decisions
#[derive(Debug, Clone)]
pub enum LiquidationOutcome {
    /// A transaction was built and sent to the tx worker
    TxSent,
    /// Liquidation was skipped due to missing data, no positions, no makers, etc.
    Skipped(&'static str),
}

impl LiquidationOutcome {
    pub fn is_sent(&self) -> bool {
        matches!(self, Self::TxSent)
    }

    pub fn reason(&self) -> &'static str {
        match self {
            Self::TxSent => "sent",
            Self::Skipped(reason) => reason,
        }
    }
}

/// Trait for pluggable liquidation strategies
pub trait LiquidationStrategy {
    /// Execute liquidation logic a user, including selecting makers and sending txs.
    fn liquidate_user<'a>(
        &'a self,
        liquidatee: Pubkey,
        user_account: Arc<User>,
        tx_sender: TxSender,
        priority_fee: u64,
        cu_limit: u32,
        slot: u64,
        pyth_price_updates: BTreeMap<u16, PythPriceUpdate>,
        status: UserMarginStatus,
    ) -> futures_util::future::BoxFuture<'a, LiquidationOutcome>;
}

pub enum GrpcEvent {
    OracleUpdate {
        oracle_price_data: OraclePriceData,
        market: MarketId,
        slot: Slot,
    },
    SpotMarketUpdate {
        market: SpotMarket,
        slot: Slot,
    },
    PerpMarketUpdate {
        market: PerpMarket,
        slot: Slot,
    },
    UserUpdate {
        pubkey: Pubkey,
        user: User,
        slot: Slot,
    },
}

pub struct LiquidatorBot {
    velocity: VelocityClient,
    dlob_notifier: DLOBNotifier,
    config: Config,
    /// stores velocity perp+spot market metadata and oracle prices
    market_state: Arc<RwLock<MarketState>>,
    /// receives new updates from grpc
    events_rx: tokio::sync::mpsc::Receiver<GrpcEvent>,
    /// sends liquidatable accounts to work thread
    liq_tx: tokio::sync::mpsc::Sender<LiquidationRequest>,
    pyth_price_feed: Option<tokio::sync::mpsc::Receiver<PythPriceUpdate>>,
    /// Dashboard state for HTTP API
    dashboard_state: DashboardStateRef,
    subaccount_pubkeys: Vec<Pubkey>,
    /// Track collateral info per subaccount
    collateral_info_per_subaccount: Arc<DashMap<Pubkey, CollateralInfo>>,
    /// In flight txs tracking
    txs_in_flight: Arc<DashMap<Pubkey, HashSet<Signature>>>,
    // Map(Signature,(collateral, ts))
    tx_sig_to_collateral: Arc<DashMap<Signature, (u128, u64)>>,
    free_collateral_per_subaccount: Arc<DashMap<Pubkey, u128>>,
    /// Live slot duration (ms) shared with the liquidation worker so its rate
    /// limiter re-paces on a mid-run gate switch without a restart; updated by
    /// the main loop whenever it refreshes its own `slot_duration`.
    liquidation_slot_duration_ms: Arc<std::sync::atomic::AtomicU64>,
}

impl LiquidatorBot {
    pub async fn new(
        config: Config,
        velocity: VelocityClient,
        metrics: Arc<Metrics>,
        dashboard_state: DashboardStateRef,
    ) -> Self {
        let dlob: &'static DLOB = Box::leak(Box::new(DLOB::default()));

        let mut perp_market_ids = match config.use_markets() {
            UseMarkets::All => velocity.get_all_perp_market_ids(),
            UseMarkets::Subset(m) => m,
        };

        let spot_market_ids: Vec<MarketId> = velocity
            .program_data()
            .spot_market_configs()
            .iter()
            .map(|m| MarketId::spot(m.market_index))
            .collect();

        // remove bet perp markets
        perp_market_ids.retain(|x| {
            let market = velocity
                .program_data()
                .perp_market_config_by_index(x.index())
                .unwrap();
            let name = core::str::from_utf8(&market.name)
                .unwrap()
                .to_ascii_lowercase();

            !name.contains("bet") && market.status != MarketStatus::Initialized
        });

        let market_pubkeys: Vec<Pubkey> = perp_market_ids
            .iter()
            .map(|x| {
                velocity
                    .program_data()
                    .perp_market_config_by_index(x.index())
                    .unwrap()
                    .pubkey
            })
            .collect();

        let priority_fee_subscriber =
            PriorityFeeSubscriber::new(velocity.rpc().url(), &market_pubkeys);
        let priority_fee_subscriber = priority_fee_subscriber.subscribe();

        let subaccounts: Vec<Pubkey> = config
            .get_subaccounts()
            .iter()
            .map(|id| velocity.wallet.sub_account(*id))
            .collect();

        log::info!(target: TARGET, "liquidator 🫠 bot started: authority={:?}, subaccount={:?}", velocity.wallet.authority(), subaccounts);

        velocity.subscribe_blockhashes().await.expect("subscribed");

        let subaccount_pubkeys: Vec<Pubkey> = config
            .get_subaccounts()
            .iter()
            .map(|id| velocity.wallet.sub_account(*id))
            .collect();

        let collateral_info_per_subaccount =
            Arc::new(get_collateral_info_per_subaccount(&velocity, &subaccount_pubkeys).await);

        // In flight txs tracking to prevent over committing
        let (txs_in_flight, free_collateral_per_subaccount) = {
            let txs = DashMap::new();
            let free = DashMap::new();
            for &subaccount in &subaccount_pubkeys {
                txs.insert(subaccount, HashSet::new());
                if let Some(info) = collateral_info_per_subaccount.get(&subaccount) {
                    free.insert(subaccount, info.free.max(0) as u128);
                }
            }
            (Arc::new(txs), Arc::new(free))
        };

        let tx_sig_to_collateral: Arc<DashMap<Signature, (u128, u64)>> = Arc::new(DashMap::new());
        let perp_fill_fallbacks: Arc<DashMap<(Pubkey, u16), PerpFillFallback>> =
            Arc::new(DashMap::new());

        let tx_worker = TxWorker::new(
            velocity.clone(),
            Arc::clone(&metrics),
            config.dry,
            Some(Arc::clone(&txs_in_flight)),
            Some(Arc::clone(&tx_sig_to_collateral)),
            Some(Arc::clone(&free_collateral_per_subaccount)),
            Some(Arc::clone(&perp_fill_fallbacks)),
        );
        let rt = tokio::runtime::Handle::current();
        let tx_sender = tx_worker.run(rt);

        let dlob_notifier = dlob.spawn_notifier();
        let events_rx = setup_grpc(
            velocity.clone(),
            dlob_notifier.clone(),
            tx_sender.clone(),
            perp_market_ids.clone(),
        )
        .await;
        log::info!(target: TARGET, "subscribed gRPC");

        // populate market data
        let mut market_state = MarketStateData::default();
        // Only use pyth price when it differs from oracle by >5 bps
        market_state.pyth_oracle_diff_threshold_bps = 5;

        for market in velocity.program_data().perp_market_configs() {
            market_state.set_perp_market(*market);
            if let Some(oracle) = velocity
                .backend()
                .oracle_map()
                .get_by_market(&MarketId::perp(market.market_index))
            {
                market_state.set_perp_oracle_price(market.market_index, oracle.data);
            }
        }

        for market in velocity.program_data().spot_market_configs() {
            market_state.set_spot_market(*market);
            if let Some(oracle) = velocity
                .backend()
                .oracle_map()
                .get_by_market(&MarketId::spot(market.market_index))
            {
                market_state.set_spot_oracle_price(market.market_index, oracle.data);
            }
        }

        let market_state = Arc::new(RwLock::new(MarketState::new(market_state)));

        let pyth_access_token = std::env::var("PYTH_LAZER_TOKEN").expect("pyth access token");
        let pyth_feed_cli = pyth_lazer_client::LazerClient::new(
            "wss://pyth-lazer.dourolabs.app/v1/stream",
            pyth_access_token.as_str(),
        )
        .expect("pyth price feed connects");

        let pyth_price_feed: tokio::sync::mpsc::Receiver<_> = if config.use_spot_liquidation {
            crate::util::subscribe_price_feeds(
                pyth_feed_cli,
                &perp_market_ids,
                &spot_market_ids,
                &[],
            )
        } else {
            crate::util::subscribe_price_feeds(pyth_feed_cli, &perp_market_ids, &[], &[])
        };

        log::info!(target: TARGET, "subscribed pyth price feeds");

        let cu_limit = std::env::var("FILL_CU_LIMIT")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(config.fill_cu_limit);

        // start liquidation worker
        let (liq_tx, liq_rx) = tokio::sync::mpsc::channel::<LiquidationRequest>(102400);
        // Live slot duration shared with the worker; seeded from the real chain
        // slot so a restart after a gate switch re-paces immediately, then kept
        // current by the main loop (see `run`).
        let startup_slot = velocity.get_slot().await.unwrap_or(0);
        let liquidation_slot_duration_ms = Arc::new(std::sync::atomic::AtomicU64::new(
            crate::util::client_slot_duration(&velocity, startup_slot).as_ms(),
        ));
        spawn_liquidation_worker(
            tx_sender.clone(),
            Arc::new(PrimaryLiquidationStrategy {
                dlob,
                velocity: velocity.clone(),
                market_state: Arc::clone(&market_state),
                subaccounts: subaccounts.clone(),
                metrics: Arc::clone(&metrics),
                use_spot_liquidation: config.use_spot_liquidation,
                dlob_url: config.dlob_url.clone(),
                txs_in_flight: Arc::clone(&txs_in_flight),
                tx_sig_to_collateral: Arc::clone(&tx_sig_to_collateral),
                free_collateral_per_subaccount: Arc::clone(&free_collateral_per_subaccount),
                perp_fill_fallbacks,
            }),
            liq_rx,
            cu_limit,
            Arc::clone(&priority_fee_subscriber),
            Arc::clone(&metrics),
            Arc::clone(&liquidation_slot_duration_ms),
        );

        log::info!(target: TARGET, "spawned liquidation worker");

        spawn_derisk_loop(
            velocity.clone(),
            tx_sender.clone(),
            subaccounts,
            Arc::clone(&priority_fee_subscriber),
            cu_limit,
        );

        log::info!(target: TARGET, "spawned derisk worker");

        let tx_sig_to_collateral_clone = Arc::clone(&tx_sig_to_collateral);
        let txs_in_flight_clone = Arc::clone(&txs_in_flight);
        let free_collateral_per_subaccount_clone = Arc::clone(&free_collateral_per_subaccount);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                clean_stale_in_flight_txs(
                    Arc::clone(&tx_sig_to_collateral_clone),
                    Arc::clone(&txs_in_flight_clone),
                    Arc::clone(&free_collateral_per_subaccount_clone),
                );
            }
        });

        log::info!(target: TARGET, "spawned stale in flight txs cleanup worker");

        LiquidatorBot {
            velocity,
            dlob_notifier,
            events_rx,
            config,
            market_state,
            liq_tx,
            pyth_price_feed: Some(pyth_price_feed),
            dashboard_state,
            subaccount_pubkeys,
            collateral_info_per_subaccount,
            txs_in_flight,
            tx_sig_to_collateral,
            free_collateral_per_subaccount,
            liquidation_slot_duration_ms,
        }
    }

    pub async fn run(self) {
        let mut events_rx = self.events_rx;
        let velocity: &'static VelocityClient = Box::leak(Box::new(self.velocity));
        let config = self.config.clone();
        let dlob_notifier = self.dlob_notifier;
        let mut current_slot = 0;
        let mut users = BTreeMap::<Pubkey, UserAccountMetadata>::new();
        let mut oracle_prices = HashMap::<MarketId, OraclePriceMetadata>::new();
        let mut high_risk = HashSet::<Pubkey>::new();
        let liquidation_margin_buffer_ratio = velocity
            .state_account()
            .map(|x| x.liquidation_margin_buffer_ratio)
            .expect("State has liquidation_margin_buffer_ratio");
        // refreshed on the collateral-refresh cadence below, so a mid-run slot
        // duration flip is picked up without a bot restart
        // seed with the real chain slot so a restart after a gate switch
        // reflects it immediately, not only after the first refresh below
        let startup_slot = velocity.get_slot().await.unwrap_or(0);
        let mut slot_duration = crate::util::client_slot_duration(velocity, startup_slot);
        // keep the worker's rate limiter in sync with the resolved duration
        self.liquidation_slot_duration_ms
            .store(slot_duration.as_ms(), std::sync::atomic::Ordering::Relaxed);

        const RECHECK_CYCLE_INTERVAL: u32 = 1024;
        /// Max wall-clock time between full user sweeps; the cycle-count trigger
        /// alone has no time bound (one cycle per recv_many batch)
        const FULL_RECHECK_INTERVAL_MS: u64 = 30_000;

        let mut cycle_count = 0u32;
        let mut last_full_recheck_ms: u64 = current_time_millis();

        // initialize local User storage
        let mut exclude_count = 0;
        let mut initial_high_risk_count = 0;

        let mut last_collateral_refresh_ms: u64 = 0;
        const COLLATERAL_REFRESH_INTERVAL_MS: u64 = 5_000;

        log::info!(target: TARGET, "starting user account initialization");

        velocity
            .backend()
            .account_map()
            .iter_accounts_with::<User>(|pubkey, user, slot| {
                let margin_info = match self
                    .market_state
                    .read()
                    .unwrap()
                    .calculate_simplified_margin_requirement(
                        user,
                        MarginRequirementType::Maintenance,
                        Some(liquidation_margin_buffer_ratio),
                    ) {
                    Ok(info) => info,
                    Err(e) => {
                        // Keep the user under watch anyway: a transiently missing
                        // oracle/market at startup must not permanently hide an
                        // account from monitoring (it only re-enters on self-update)
                        log::warn!(target: TARGET, "margin calc failed at init: user={pubkey:?} error={e:?}");
                        users.insert(
                            *pubkey,
                            UserAccountMetadata {
                                user: *user,
                                last_updated_slot: slot,
                                last_updated_timestamp_ms: current_time_millis(),
                            },
                        );
                        return;
                    }
                };

                if margin_info.total_collateral < config.min_collateral as i128
                    && margin_info.margin_requirement < config.min_collateral as u128
                {
                    exclude_count += 1;
                    // log::debug!(target: TARGET, "excluding user: {:?}. insignificant collateral: {}/{}", user.authority, margin_info.total_collateral, margin_info.margin_requirement);
                } else {
                    let now_ms = current_time_millis();
                    users.insert(
                        *pubkey,
                        UserAccountMetadata {
                            user: *user,
                            last_updated_slot: slot,
                            last_updated_timestamp_ms: now_ms,
                        },
                    );
                    // Check margin status and add to high-risk set if needed
                    let status = check_margin_status(&margin_info);

                    if status.is_at_risk() {
                        high_risk.insert(*pubkey);
                        initial_high_risk_count += 1;
                    }
                }
            });

        log::info!(target: TARGET, "filtered #{exclude_count} accounts with dust collateral");
        log::info!(target: TARGET, "identified #{initial_high_risk_count} high-risk accounts for monitoring");

        // main loop
        let mut event_buffer = Vec::<GrpcEvent>::with_capacity(64);
        let mut oracle_update;

        // Pyth Feed
        let mut pyth_price_feed = match self.pyth_price_feed {
            Some(rx) => rx,
            None => {
                log::error!(target: TARGET, "pyth price feed not initialized");
                return;
            }
        };
        let mut pyth_perp_prices = BTreeMap::<u16, PythPriceUpdate>::new();

        log::info!(target: TARGET, "entering main event loop");

        loop {
            oracle_update = false;

            // Drain pyth updates first (non-blocking)
            'pyth: loop {
                match pyth_price_feed.try_recv() {
                    Ok(update) => {
                        let market_id = update.market_id;
                        let price = update.price;

                        match update.market_type {
                            MarketType::Perp => {
                                pyth_perp_prices.insert(market_id, update);
                                // Update market state with perp pyth price for margin calculation
                                if price > 0 {
                                    self.market_state
                                        .write()
                                        .unwrap()
                                        .set_perp_pyth_price(market_id, price as i64);
                                }
                            }
                            MarketType::Spot => {
                                // Update market state with spot pyth price for margin calculation
                                if price > 0 {
                                    self.market_state
                                        .write()
                                        .unwrap()
                                        .set_spot_pyth_price(market_id, price as i64);
                                }
                            }
                        }
                    }
                    Err(TryRecvError::Disconnected) => {
                        log::error!(target: TARGET, "pyth price feed disconnected");
                        return;
                    }
                    Err(TryRecvError::Empty) => break 'pyth,
                }
            }

            if events_rx.recv_many(&mut event_buffer, 64).await == 0 {
                // channel closed: gRPC subscription is gone, nothing more to process
                log::error!(target: TARGET, "grpc event channel closed, exiting liquidator loop");
                return;
            }
            for event in event_buffer.drain(..) {
                match event {
                    GrpcEvent::UserUpdate {
                        pubkey,
                        user,
                        slot: update_slot,
                    } => {
                        let now_ms = current_time_millis();
                        if now_ms.saturating_sub(last_collateral_refresh_ms)
                            >= COLLATERAL_REFRESH_INTERVAL_MS
                        {
                            last_collateral_refresh_ms = now_ms;

                            // Pick up a mid-run slot-duration flip
                            slot_duration =
                                crate::util::client_slot_duration(velocity, update_slot);
                            // re-pace the worker's rate limiter on the flip
                            self.liquidation_slot_duration_ms
                                .store(slot_duration.as_ms(), std::sync::atomic::Ordering::Relaxed);

                            // Update collaterals
                            let new_collateral = get_collateral_info_per_subaccount(
                                &velocity,
                                &self.subaccount_pubkeys,
                            )
                            .await;

                            self.collateral_info_per_subaccount.clear();

                            for (subaccount_pubkey, collateral_info) in new_collateral {
                                self.collateral_info_per_subaccount
                                    .insert(subaccount_pubkey, collateral_info);
                            }

                            // Update free collateral accounting for in flight txs
                            for entry in self.collateral_info_per_subaccount.iter() {
                                let subaccount_pubkey = entry.key();
                                let collateral_info = entry.value();
                                let mut free = collateral_info.free;

                                if let Some(in_flight) = self.txs_in_flight.get(subaccount_pubkey) {
                                    for sig in in_flight.value().iter() {
                                        if let Some(reserved) = self.tx_sig_to_collateral.get(sig) {
                                            free = free.saturating_sub(reserved.value().0 as i128);
                                        }
                                    }
                                }
                                self.free_collateral_per_subaccount
                                    .insert(*subaccount_pubkey, free.max(0) as u128);
                            }
                        }

                        let old_user = users.get(&pubkey).map(|m| &m.user);
                        dlob_notifier.user_update(pubkey, old_user, &user, update_slot);
                        let now_ms = current_time_millis();
                        users.insert(
                            pubkey,
                            UserAccountMetadata {
                                user: user.clone(),
                                last_updated_slot: update_slot,
                                last_updated_timestamp_ms: now_ms,
                            },
                        );

                        // calculate user margin after update
                        let margin_info = match self
                            .market_state
                            .read()
                            .unwrap()
                            .calculate_simplified_margin_requirement(
                                &user,
                                MarginRequirementType::Maintenance,
                                Some(liquidation_margin_buffer_ratio),
                            ) {
                            Ok(info) => info,
                            Err(e) => {
                                log::warn!(target: TARGET, "margin calc failed: user={pubkey:?} error={e:?}");
                                continue;
                            }
                        };

                        if margin_info.total_collateral < config.min_collateral as i128
                            && margin_info.margin_requirement < config.min_collateral as u128
                        {
                            // log::debug!(target: TARGET, "filtered account with dust collateral: {pubkey:?}");
                            high_risk.remove(&pubkey);
                            users.remove(&pubkey);
                        } else {
                            let status = check_margin_status(&margin_info);
                            if status.is_at_risk() {
                                high_risk.insert(pubkey);
                            } else {
                                high_risk.remove(&pubkey);
                            }
                        }
                    }
                    GrpcEvent::OracleUpdate {
                        oracle_price_data,
                        market,
                        slot,
                    } => {
                        // Per-update firehose: kept at trace so it stays retrievable
                        // (RUST_LOG=…,liquidator=trace) but is off in the debug stream.
                        // The oracle price at the moment of a liquidation is logged on the
                        // attempt events below instead.
                        log::trace!(target: TARGET, "oracle update received: market={:?}, slot={}", market, slot);
                        if slot >= current_slot {
                            if market.is_perp() {
                                if oracle_price_data.price > 0 {
                                    self.market_state
                                        .write()
                                        .unwrap()
                                        .set_perp_oracle_price(market.index(), oracle_price_data);
                                }
                            } else {
                                if oracle_price_data.price > 0 {
                                    self.market_state
                                        .write()
                                        .unwrap()
                                        .set_spot_oracle_price(market.index(), oracle_price_data);
                                }
                            }
                            let now_ms = current_time_millis();
                            oracle_prices.insert(
                                market,
                                OraclePriceMetadata {
                                    price_data: oracle_price_data,
                                    last_updated_slot: slot,
                                    last_updated_timestamp_ms: now_ms,
                                },
                            );
                            current_slot = slot;
                            oracle_update = true;
                        }
                    }
                    GrpcEvent::PerpMarketUpdate { market, slot } => {
                        if slot >= current_slot {
                            self.market_state.write().unwrap().set_perp_market(market);
                            current_slot = slot;
                        }
                    }
                    GrpcEvent::SpotMarketUpdate { market, slot } => {
                        if slot >= current_slot {
                            self.market_state.write().unwrap().set_spot_market(market);
                            current_slot = slot;
                        }
                    }
                }
            }

            // Only recheck margin for high-risk users on oracle price updates
            // Process in batches to avoid blocking the main event loop
            if oracle_update {
                let _high_risk_count = high_risk.len();

                let _t0 = current_time_millis();
                let mut liquidatable_users = Vec::new();

                // Update dashboard state
                // update_dashboard_state(
                //     velocity,
                //     &self.dashboard_state,
                //     &users,
                //     &oracle_prices,
                //     &high_risk,
                //     current_slot,
                //     self.market_state,
                //     liquidation_margin_buffer_ratio,
                // )
                // .await;

                for pubkey in &high_risk {
                    if let Some(user_meta) = users.get(&pubkey) {
                        // Don't act on stale oracle data: a liquidation decision made off a
                        // dead price feed is more likely wrong than late.
                        if let Err(StalenessError::OraclePriceStale { market, age_slots }) =
                            validate_data_freshness(
                                user_meta,
                                &oracle_prices,
                                current_slot,
                                slot_duration,
                            )
                        {
                            log::warn!(
                                target: TARGET,
                                "skipping liquidation check: reason=stale_oracle user={pubkey:?} market={market:?} age_slots={age_slots} current_slot={current_slot}"
                            );
                            continue;
                        }

                        let margin_info = match self
                            .market_state
                            .read()
                            .unwrap()
                            .calculate_simplified_margin_requirement(
                                &user_meta.user,
                                MarginRequirementType::Maintenance,
                                Some(liquidation_margin_buffer_ratio),
                            ) {
                            Ok(info) => info,
                            Err(e) => {
                                log::warn!(target: TARGET, "margin calc failed: user={pubkey:?} error={e:?}");
                                continue;
                            }
                        };

                        let status = check_margin_status(&margin_info);
                        if status.is_liquidatable() {
                            liquidatable_users.push((pubkey, user_meta.user.clone(), status));
                        }
                    }
                }

                // Send liquidations outside the loop
                for (pubkey, user, status) in liquidatable_users {
                    let pyth_price_updates = fresh_pyth_updates_for_user(&user, &pyth_perp_prices);
                    send_liquidation(
                        &self.liq_tx,
                        *pubkey,
                        user,
                        current_slot,
                        pyth_price_updates,
                        status,
                    );
                }

                // log::debug!(
                //     target: TARGET,
                //     "processed {} high-risk margin updates in {}ms",
                //     high_risk_count,
                //     current_time_millis() - t0,
                // );

                // Update high_risk set synchronously but quickly (just remove safe users)
                high_risk.retain(|pubkey| {
                    if let Some(user_meta) = users.get(pubkey) {
                        // With a stale oracle the margin picture is unreliable — keep the
                        // user under watch rather than dropping them.
                        if validate_data_freshness(user_meta, &oracle_prices, current_slot, slot_duration)
                            .is_err()
                        {
                            return true;
                        }

                        let margin_info = match self
                            .market_state
                            .read()
                            .unwrap()
                            .calculate_simplified_margin_requirement(
                                &user_meta.user,
                                MarginRequirementType::Maintenance,
                                Some(liquidation_margin_buffer_ratio),
                            ) {
                            Ok(info) => info,
                            Err(e) => {
                                log::warn!(target: TARGET, "margin calc failed: user={pubkey:?} error={e:?}");
                                return false;
                            }
                        };

                        check_margin_status(&margin_info).is_at_risk()
                    } else {
                        false
                    }
                });

                // log::debug!(
                //     target: TARGET,
                //     "updated high_risk set in {}ms (now {} users)",
                //     current_time_millis() - t0,
                //     high_risk.len(),
                // );
            }

            // Periodically recheck all users to find new high-risk users. Cycle-count
            // alone is unbounded in wall-clock time (one cycle per recv_many batch), so
            // also trigger on elapsed time — a fast price move can take a user straight
            // from safe to liquidatable between cycle-based sweeps.
            cycle_count += 1;
            let now_ms = current_time_millis();
            if cycle_count % RECHECK_CYCLE_INTERVAL == 0
                || now_ms.saturating_sub(last_full_recheck_ms) >= FULL_RECHECK_INTERVAL_MS
            {
                last_full_recheck_ms = now_ms;
                let t0 = current_time_millis();
                let mut newly_high_risk = 0;

                for (pubkey, user_meta) in users.iter() {
                    if high_risk.contains(pubkey) {
                        continue;
                    }

                    // Don't act on stale oracle data (see the high-risk scan above)
                    if validate_data_freshness(
                        user_meta,
                        &oracle_prices,
                        current_slot,
                        slot_duration,
                    )
                    .is_err()
                    {
                        continue;
                    }

                    let margin_info = match self
                        .market_state
                        .read()
                        .unwrap()
                        .calculate_simplified_margin_requirement(
                            &user_meta.user,
                            MarginRequirementType::Maintenance,
                            Some(liquidation_margin_buffer_ratio),
                        ) {
                        Ok(info) => info,
                        Err(e) => {
                            log::warn!(target: TARGET, "margin calc failed: user={pubkey:?} error={e:?}");
                            continue;
                        }
                    };

                    let status = check_margin_status(&margin_info);

                    if status.is_liquidatable() {
                        high_risk.insert(*pubkey);
                        newly_high_risk += 1;
                        log::info!(
                            target: TARGET,
                            "found liquidatable user: user={pubkey:?} total_collateral={} margin_requirement={} status={status:?} slot={current_slot}",
                            margin_info.total_collateral,
                            margin_info.margin_requirement,
                        );

                        let pyth_price_updates =
                            fresh_pyth_updates_for_user(&user_meta.user, &pyth_perp_prices);
                        send_liquidation(
                            &self.liq_tx,
                            *pubkey,
                            user_meta.user.clone(),
                            current_slot,
                            pyth_price_updates,
                            status,
                        );
                    } else if status.is_at_risk() {
                        high_risk.insert(*pubkey);
                        newly_high_risk += 1;
                    }
                }

                log::debug!(
                    target: TARGET,
                    "margin recheck: users={} newly_high_risk={newly_high_risk} high_risk_total={} took_ms={}",
                    users.len(),
                    high_risk.len(),
                    current_time_millis() - t0
                );
            }
        }
    }
}

/// Payload sent to the liquidation worker
struct LiquidationRequest {
    /// liquidatee subaccount
    pubkey: Pubkey,
    /// liquidatee account snapshot at detection time
    user: User,
    /// slot the liquidatable status was observed at
    slot: u64,
    /// wall-clock time the request was enqueued (ms)
    timestamp_ms: u64,
    /// fresh pyth prices for the user's perp markets, keyed by market index
    pyth_price_updates: BTreeMap<u16, PythPriceUpdate>,
    status: UserMarginStatus,
}

/// Fresh pyth prices for every perp market the user holds a position in,
/// keyed by market index. Carried per market so each position the strategy
/// selects (an isolated position is not necessarily the largest one) can ship
/// its own market's update.
fn fresh_pyth_updates_for_user(
    user: &User,
    pyth_perp_prices: &BTreeMap<u16, PythPriceUpdate>,
) -> BTreeMap<u16, PythPriceUpdate> {
    user.perp_positions
        .iter()
        .filter(|p| p.base_asset_amount != 0)
        .filter_map(|p| {
            let pyth_update = pyth_perp_prices.get(&p.market_index)?;
            if validate_pyth_price_freshness(pyth_update).is_ok() {
                Some((p.market_index, pyth_update.clone()))
            } else {
                log::debug!(
                    target: TARGET,
                    "skipping stale pyth price: market={}",
                    p.market_index
                );
                None
            }
        })
        .collect()
}

/// Forward a liquidatable user to the liquidation worker (non-blocking)
fn send_liquidation(
    liq_tx: &tokio::sync::mpsc::Sender<LiquidationRequest>,
    pubkey: Pubkey,
    user: User,
    slot: u64,
    pyth_price_updates: BTreeMap<u16, PythPriceUpdate>,
    status: UserMarginStatus,
) {
    match liq_tx.try_send(LiquidationRequest {
        pubkey,
        user,
        slot,
        timestamp_ms: current_time_millis(),
        pyth_price_updates,
        status,
    }) {
        Ok(()) => {}
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            log::warn!(
                target: TARGET,
                "liquidation channel full, dropping liquidation for {:?}",
                pubkey
            );
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            log::error!(target: TARGET, "liquidation channel closed");
        }
    }
}

fn clean_stale_in_flight_txs(
    tx_sig_to_collateral: Arc<DashMap<Signature, (u128, u64)>>,
    txs_in_flight: Arc<DashMap<Pubkey, HashSet<Signature>>>,
    free_collateral_per_subaccount: Arc<DashMap<Pubkey, u128>>,
) {
    let now = current_time_millis();
    let timeout_ms = 60_000;

    let stale: Vec<(Signature, u128)> = tx_sig_to_collateral
        .iter()
        .filter_map(|entry| {
            if now.saturating_sub(entry.value().1) > timeout_ms {
                Some((*entry.key(), entry.value().0))
            } else {
                None
            }
        })
        .collect();

    for (sig, collateral) in stale {
        tx_sig_to_collateral.remove(&sig);
        for mut entry in txs_in_flight.iter_mut() {
            if entry.value_mut().remove(&sig) {
                if let Some(mut free) = free_collateral_per_subaccount.get_mut(entry.key()) {
                    *free = free.saturating_add(collateral);
                }
                break;
            }
        }
    }
}

async fn derisk_subaccount(
    velocity: &VelocityClient,
    tx_sender: &TxSender,
    subaccount: Pubkey,
    priority_fee: u64,
    cu_limit: u32,
) {
    let user = match velocity.try_get_account::<User>(&subaccount) {
        Ok(u) => u,
        Err(_) => return,
    };

    for position in user.perp_positions.iter() {
        if position.base_asset_amount == 0 && position.quote_asset_amount == 0 {
            continue;
        }

        let mut tx_builder = TransactionBuilder::new(
            velocity.program_data(),
            subaccount,
            std::borrow::Cow::Owned(user.clone()),
            false,
        )
        .with_priority_fee(priority_fee, Some(cu_limit));

        if position.base_asset_amount != 0 {
            let direction = if position.base_asset_amount > 0 {
                PositionDirection::Short
            } else {
                PositionDirection::Long
            };

            tx_builder = tx_builder.place_orders(vec![OrderParams {
                order_type: OrderType::Market,
                market_type: MarketType::Perp,
                direction,
                base_asset_amount: position.base_asset_amount.unsigned_abs(),
                market_index: position.market_index,
                reduce_only: true,
                max_ts: Some((current_time_millis() / 1000 + 15) as i64), // ~15s
                ..Default::default()
            }]);

            tx_sender
                .send_tx(
                    tx_builder.build(),
                    TxIntent::Derisk {
                        market_index: position.market_index,
                        subaccount,
                    },
                    cu_limit as u64,
                )
                .await;
        } else {
            tx_builder = tx_builder.settle_pnl(position.market_index, None, None);

            tx_sender
                .send_tx(
                    tx_builder.build(),
                    TxIntent::SettlePnl {
                        market_index: position.market_index,
                        subaccount,
                    },
                    cu_limit as u64,
                )
                .await;
        }
    }

    // TODO: Add spot derisking, swap any position back to USDC using jupiter/titan
}

fn spawn_derisk_loop(
    velocity: VelocityClient,
    tx_sender: TxSender,
    subaccounts: Vec<Pubkey>,
    priority_fee_subscriber: Arc<PriorityFeeSubscriber>,
    cu_limit: u32,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            let priority_fee = priority_fee_subscriber.priority_fee_nth(0.6);
            for subaccount in &subaccounts {
                derisk_subaccount(&velocity, &tx_sender, *subaccount, priority_fee, cu_limit).await;
            }
        }
    });
}

async fn get_collateral_info_per_subaccount(
    velocity: &VelocityClient,
    subaccounts: &[Pubkey],
) -> DashMap<Pubkey, CollateralInfo> {
    let collateral_info_per_subaccount = DashMap::<Pubkey, CollateralInfo>::new();
    for &subaccount_pubkey in subaccounts {
        match velocity.get_user_account(&subaccount_pubkey).await {
            Ok(user_account) => {
                match calculate_collateral(
                    &velocity,
                    &user_account,
                    MarginRequirementType::Maintenance,
                ) {
                    Ok(collateral_info) => {
                        collateral_info_per_subaccount.insert(subaccount_pubkey, collateral_info);
                        log::debug!(
                            target: TARGET,
                            "Subaccount {}: free_collateral = {}, total_collateral = {}",
                            subaccount_pubkey,
                            collateral_info.free,
                            collateral_info.total
                        );
                    }
                    Err(e) => {
                        log::warn!(
                            target: TARGET,
                            "Failed to calculate collateral for subaccount {}: {:?}",
                            subaccount_pubkey,
                            e
                        );
                    }
                }
            }
            Err(e) => {
                log::warn!(
                    target: TARGET,
                    "Failed to load subaccount {}: {:?}",
                    subaccount_pubkey,
                    e
                );
            }
        }
    }
    collateral_info_per_subaccount
}

fn on_transaction_update_fn(
    tx_sender: TxSender,
) -> impl Fn(&TransactionUpdate) + Send + Sync + 'static {
    move |tx: &TransactionUpdate| {
        if let Some(sig) = tx.transaction.signatures.first() {
            tx_sender.confirm_tx((sig.as_slice().try_into()).expect("valid signature"));
        } else {
            log::warn!(target: TARGET, "received tx without sig: {tx:?}");
        }
    }
}

fn on_slot_update_fn(
    dlob_notifier: DLOBNotifier,
    velocity: VelocityClient,
    market_ids: &[MarketId],
) -> impl Fn(u64) + Send + Sync + 'static {
    let market_ids: Vec<MarketId> = market_ids.to_vec();
    let consecutive_failures = std::sync::atomic::AtomicU32::new(0);
    move |new_slot| {
        // keep the DLOB's slot clock in sync with `State` (no-op unless an
        // IBRL transition was synchronized since the last slot)
        dlob_notifier.slot_clock_update(velocity.slot_clock());
        for market in market_ids.iter() {
            // tolerate transient failures; panic (=> service restart) if persistent
            match velocity.try_get_mmoracle_for_perp_market(market.index(), new_slot) {
                Ok(oracle_price_data) => {
                    consecutive_failures.store(0, std::sync::atomic::Ordering::Relaxed);
                    dlob_notifier.slot_and_oracle_update(
                        *market,
                        new_slot,
                        oracle_price_data.price as u64,
                    );
                }
                Err(e) => {
                    let fails =
                        consecutive_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    log::warn!(
                        target: TARGET,
                        "mm oracle lookup failed: market={market:?} slot={new_slot} consecutive_failures={fails} error={e:?}"
                    );
                    assert!(
                        fails < GRPC_CALLBACK_FAILURE_LIMIT,
                        "mm oracle lookup persistently failing, restarting"
                    );
                }
            }
        }
    }
}

async fn setup_grpc(
    velocity: VelocityClient,
    dlob_notifier: DLOBNotifier,
    transaction_tx: TxSender,
    market_ids: Vec<MarketId>,
) -> tokio::sync::mpsc::Receiver<GrpcEvent> {
    let (tx, rx) = tokio::sync::mpsc::channel(102400);

    let _ = tokio::try_join!(
        crate::filler::sync_stats_accounts(&velocity),
        crate::filler::sync_user_accounts(&velocity, &dlob_notifier),
    );

    let mut oracle_to_market = HashMap::<Pubkey, Vec<(MarketId, OracleSource)>>::default();

    for (market, (oracle, source)) in velocity.backend().oracle_map().oracle_by_market.iter() {
        oracle_to_market
            .entry(*oracle)
            .and_modify(|f| f.push((*market, *source)))
            .or_insert(vec![(*market, *source)]);
    }

    log::info!(target: TARGET, "oracle map has {} oracles", oracle_to_market.len());

    let _res = velocity
        .grpc_subscribe(
            std::env::var("GRPC_ENDPOINT")
                .unwrap_or_else(|_| "https://api.rpcpool.com".to_string())
                .into(),
            std::env::var("GRPC_X_TOKEN").expect("GRPC_X_TOKEN set"),
            GrpcSubscribeOpts::default()
                .connection_opts(GrpcConnectionOpts::default().enable_compression())
                .commitment(solana_commitment_config::CommitmentLevel::Processed)
                .transaction_include_accounts(vec![velocity.wallet().default_sub_account()])
                .on_transaction(on_transaction_update_fn(transaction_tx.clone()))
                .on_slot(on_slot_update_fn(
                    dlob_notifier,
                    velocity.clone(),
                    market_ids.as_ref(),
                ))
                .usermap_on()
                .statsmap_on()
                .on_account(
                    AccountFilter::partial().with_discriminator(User::DISCRIMINATOR),
                    {
                        let tx = tx.clone();
                        move |acc| {
                            let user = velocity_rs::utils::deser_zero_copy::<User>(acc.data);
                            if let Err(err) = tx.try_send(GrpcEvent::UserUpdate {
                                pubkey: acc.pubkey,
                                user,
                                slot: acc.slot,
                            }) {
                                log::error!(target: TARGET, "failed to forward user update event: {err:?}");
                            }
                        }
                    },
                )
                .on_account(
                    AccountFilter::partial().with_discriminator(PerpMarket::DISCRIMINATOR),
                    {
                        let tx = tx.clone();
                        move |acc| {
                            let market = velocity_rs::utils::deser_zero_copy::<PerpMarket>(&acc.data);
                            if let Err(err) = tx.try_send(GrpcEvent::PerpMarketUpdate {
                                market,
                                slot: acc.slot,
                            }) {
                                log::error!(target: TARGET, "failed to forward perp market update event: {err:?}");
                            }
                        }
                    },
                )
                .on_account(
                    AccountFilter::partial().with_discriminator(SpotMarket::DISCRIMINATOR),
                    {
                        let tx = tx.clone();
                        move |acc| {
                            let market = velocity_rs::utils::deser_zero_copy::<SpotMarket>(acc.data);
                            if let Err(err) = tx.try_send(GrpcEvent::SpotMarketUpdate {
                                market,
                                slot: acc.slot,
                            }) {
                                log::error!(target: TARGET, "failed to forward spot market update event: {err:?}");
                            }
                        }
                    },
                )
                .on_oracle_update({
                    let tx = tx.clone();
                    let consecutive_failures = std::sync::atomic::AtomicU32::new(0);
                    move |acc| {
                        // tolerate transient failures; panic (=> service restart) if persistent
                        let Some(oracle_markets) = oracle_to_market.get(&acc.pubkey) else {
                            log::warn!(target: TARGET, "update for unknown oracle: pubkey={:?} slot={}", acc.pubkey, acc.slot);
                            return;
                        };
                        let lamports = acc.lamports;
                        let slot = acc.slot;
                        for (market, oracle_source) in oracle_markets {
                            let mut data = acc.data.to_vec();
                            let mut lamports = lamports;
                            let owner = acc.owner;
                            let pubkey = acc.pubkey;
                            let account_info = anchor_lang::prelude::AccountInfo::new(
                                &pubkey,
                                false,
                                false,
                                &mut lamports,
                                &mut data,
                                &owner,
                                false,
                            );
                            let oracle_price_data = match velocity_rs::program::state::oracle::get_oracle_price(
                                oracle_source,
                                &account_info,
                                slot,
                            ) {
                                Ok(data) => {
                                    consecutive_failures.store(0, std::sync::atomic::Ordering::Relaxed);
                                    data
                                }
                                Err(e) => {
                                    let fails = consecutive_failures
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                        + 1;
                                    log::warn!(
                                        target: TARGET,
                                        "oracle price decode failed: market={market:?} oracle={pubkey:?} source={oracle_source:?} slot={slot} consecutive_failures={fails} error={e:?}"
                                    );
                                    assert!(
                                        fails < GRPC_CALLBACK_FAILURE_LIMIT,
                                        "oracle decode persistently failing, restarting"
                                    );
                                    continue;
                                }
                            };
                            if let Err(err) = tx.try_send(GrpcEvent::OracleUpdate {
                                oracle_price_data,
                                market: *market,
                                slot: acc.slot,
                            }) {
                                log::error!(target: TARGET, "failed to forward oracle update event: {err:?}");
                            }
                        }
                    }
                }),
            true,
        )
        .await;

    rx
}

fn spawn_liquidation_worker(
    tx_sender: TxSender,
    strategy: Arc<dyn LiquidationStrategy + Send + Sync>,
    mut liq_rx: tokio::sync::mpsc::Receiver<LiquidationRequest>,
    cu_limit: u32,
    priority_fee_subscriber: Arc<PriorityFeeSubscriber>,
    metrics: Arc<Metrics>,
    slot_duration_ms: Arc<std::sync::atomic::AtomicU64>,
) {
    let attempt_tracker = Arc::new(DashMap::<Pubkey, LiquidationAttemptTracker>::new());

    tokio::spawn(async move {
        // Periodically clean stale tracker entries (accounts no longer in the liquidation pipeline)
        let mut last_tracker_cleanup_ms = current_time_millis();
        const TRACKER_CLEANUP_INTERVAL_MS: u64 = 60_000;
        const TRACKER_ENTRY_MAX_AGE_MS: u64 = 600_000; // 10 minutes

        while let Some(LiquidationRequest {
            pubkey: liquidatee,
            user: user_account,
            slot,
            timestamp_ms,
            pyth_price_updates,
            status,
        }) = liq_rx.recv().await
        {
            // wall-clock rate limit expressed in actual slots, at the live slot
            // duration (kept current by the main loop) so a mid-run gate switch
            // re-paces without a restart
            let liquidation_slot_rate_limit =
                LIQUIDATION_RATE_LIMIT.to_slots(SlotDuration::from_state_ms(
                    slot_duration_ms.load(std::sync::atomic::Ordering::Relaxed) as u16,
                ));
            // Drop entries older than 1 second to handle backpressure
            let now = current_time_millis();
            if now.saturating_sub(timestamp_ms) > MAX_LIQUIDATION_AGE_MS {
                continue;
            }

            // Clean up old tracker entries periodically
            if now.saturating_sub(last_tracker_cleanup_ms) > TRACKER_CLEANUP_INTERVAL_MS {
                attempt_tracker.retain(|_, tracker| {
                    now.saturating_sub(tracker.last_attempt_ms) < TRACKER_ENTRY_MAX_AGE_MS
                });
                last_tracker_cleanup_ms = now;
            }

            // Check slot-based rate limit AND failure-based cooldown
            if let Some(tracker) = attempt_tracker.get(&liquidatee) {
                // Basic slot rate limit
                if slot.abs_diff(tracker.last_attempt_slot) < liquidation_slot_rate_limit {
                    log::debug!(target: TARGET, "rate limited liquidation for {:?} (current: {})", liquidatee, slot);
                    continue;
                }

                // Exponential backoff for repeated failures
                if !tracker.is_cooled_down(now) {
                    let remaining_ms = tracker
                        .cooldown_ms()
                        .saturating_sub(now.saturating_sub(tracker.last_attempt_ms));
                    log::debug!(
                        target: TARGET,
                        "backoff: skipping {:?} (failures={}, cooldown={}ms, remaining={}ms)",
                        liquidatee,
                        tracker.consecutive_failures,
                        tracker.cooldown_ms(),
                        remaining_ms
                    );
                    metrics.liquidation_backoff_skips.inc();
                    continue;
                }
            }

            // Record this attempt (increment failure count preemptively; reset on TxSent)
            attempt_tracker
                .entry(liquidatee)
                .and_modify(|t| t.record_attempt(slot))
                .or_insert_with(|| LiquidationAttemptTracker::new(slot));

            let tracker_failures = attempt_tracker
                .get(&liquidatee)
                .map(|t| t.consecutive_failures)
                .unwrap_or(0);

            if tracker_failures > 1 {
                log::info!(
                    target: TARGET,
                    "retrying liquidation for {:?} (attempt #{})",
                    liquidatee,
                    tracker_failures
                );
            }

            let pf = priority_fee_subscriber.priority_fee_nth(0.6);
            let strategy_clone = Arc::clone(&strategy);
            let liquidatee_clone = liquidatee;
            let user_account_clone = Arc::new(user_account);
            let tx_sender_clone = tx_sender.clone();
            let tracker_clone = Arc::clone(&attempt_tracker);
            let metrics_clone = Arc::clone(&metrics);

            tokio::spawn(async move {
                let deadline = std::time::Duration::from_millis(LIQUIDATION_DEADLINE_MS);
                let start = std::time::Instant::now();
                let result = tokio::time::timeout(
                    deadline,
                    strategy_clone.liquidate_user(
                        liquidatee_clone,
                        user_account_clone,
                        tx_sender_clone,
                        pf,
                        cu_limit,
                        slot,
                        pyth_price_updates,
                        status,
                    ),
                )
                .await;

                let elapsed = start.elapsed();
                match result {
                    Ok(outcome) => {
                        if elapsed.as_millis() > LIQUIDATION_DEADLINE_MS as u128 {
                            log::warn!(
                                target: TARGET,
                                "liquidation for {:?} took {}ms (exceeded {}ms deadline but completed)",
                                liquidatee_clone,
                                elapsed.as_millis(),
                                LIQUIDATION_DEADLINE_MS
                            );
                        }

                        match &outcome {
                            LiquidationOutcome::TxSent => {
                                // Tx was sent — reset the failure counter so we don't
                                // punish with backoff while the tx is in-flight.
                                // If the tx later fails on-chain, the account will stay
                                // liquidatable and get re-attempted (failure count starts
                                // from 0 again, giving it a fresh chance).
                                if let Some(mut tracker) = tracker_clone.get_mut(&liquidatee_clone)
                                {
                                    tracker.reset();
                                }
                            }
                            LiquidationOutcome::Skipped(reason) => {
                                // Strategy decided not to send a tx — keep the failure
                                // count incrementing so backoff kicks in.
                                log::info!(
                                    target: TARGET,
                                    "liquidation skipped: liquidatee={liquidatee_clone:?} reason={reason} attempt={tracker_failures} elapsed_ms={} slot={slot}",
                                    elapsed.as_millis(),
                                );
                                metrics_clone
                                    .liquidation_skipped
                                    .with_label_values(&[reason])
                                    .inc();
                            }
                        }
                    }
                    Err(_) => {
                        // Timeout — the backoff failure counter stays incremented
                        log::warn!(
                            target: TARGET,
                            "liquidation timed out: liquidatee={liquidatee_clone:?} deadline_ms={LIQUIDATION_DEADLINE_MS} attempt={tracker_failures} slot={slot}",
                        );
                        metrics_clone
                            .liquidation_skipped
                            .with_label_values(&["timeout"])
                            .inc();
                    }
                }
            });
        }
    });
}

#[derive(Debug, PartialEq)]
enum LiquidationType {
    SettlePnl,
    PerpWithFill,
    PerpTakeover,
    PerpPnlForDeposit,
    BorrowForPerpPnl,
    SpotForSpot,
    Skip,
}

#[derive(Debug, Clone)]
struct PositionInfo {
    market_type: MarketType,
    market_index: u16,
    is_asset: bool,
    collateral_required: i128,
    base_amount: i64,
    quote_amount: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PerpOracleRoutePolicy {
    safe_match_allowed: bool,
    exchange_match_allowed: bool,
    liquidation_allowed: bool,
    uses_pyth_update: bool,
}

/// Takeover routings a fallback marker may drive before it is dropped.
const PERP_FILL_FALLBACK_MAX_ATTEMPTS: u32 = 3;
/// Lifetime of a fallback marker; a marker this old describes a book and an
/// oracle state that no longer exist.
const PERP_FILL_FALLBACK_EXPIRY_MS: u64 = 30_000;

/// Whether a pending fill-failed fallback should force the takeover route.
/// The marker is NOT removed here: a takeover can still die between this
/// decision and the send (no eligible subaccount, account-load failure,
/// `send_tx` returning None), and consuming the one-shot signal on those
/// paths sent the next pass back to the maker route that already failed
/// onchain. The marker is removed when the forced takeover's tx is actually
/// sent, and expires here after `PERP_FILL_FALLBACK_MAX_ATTEMPTS` routings
/// or `PERP_FILL_FALLBACK_EXPIRY_MS`, so it cannot become permanent.
fn peek_perp_fill_fallback(
    fallbacks: &DashMap<(Pubkey, u16), PerpFillFallback>,
    key: (Pubkey, u16),
    liquidation_allowed: bool,
    collateral_available: u128,
    collateral_required: u128,
    now_ms: u64,
) -> bool {
    let Some(mut entry) = fallbacks.get_mut(&key) else {
        return false;
    };
    if now_ms.saturating_sub(entry.recorded_ms) > PERP_FILL_FALLBACK_EXPIRY_MS
        || entry.attempts >= PERP_FILL_FALLBACK_MAX_ATTEMPTS
    {
        drop(entry);
        fallbacks.remove(&key);
        return false;
    }
    if !liquidation_allowed || collateral_available < collateral_required {
        return false;
    }
    entry.attempts += 1;
    true
}

/// Primary liquidation strategy
pub struct PrimaryLiquidationStrategy {
    pub velocity: VelocityClient,
    pub dlob: &'static DLOB,
    pub market_state: Arc<RwLock<MarketState>>,
    pub subaccounts: Vec<Pubkey>,
    pub metrics: Arc<Metrics>,
    pub use_spot_liquidation: bool,
    /// Base URL of the dlob-server, source of a liquidatee's resting CLOB
    /// orders for force-cancel before a perp liquidation.
    pub dlob_url: String,
    pub txs_in_flight: Arc<DashMap<Pubkey, HashSet<Signature>>>,
    // Map(Signature,(collateral, ts))
    pub tx_sig_to_collateral: Arc<DashMap<Signature, (u128, u64)>>,
    pub free_collateral_per_subaccount: Arc<DashMap<Pubkey, u128>>,
    pub perp_fill_fallbacks: Arc<DashMap<(Pubkey, u16), PerpFillFallback>>,
}

impl PrimaryLiquidationStrategy {
    /// Prefer a valid maker fill, then use collateral takeover.
    fn decide_perp_method(
        collateral_available: u128,
        collateral_required: u128,
        has_makers: bool,
        match_allowed: bool,
        liquidation_allowed: bool,
        force_takeover: bool,
    ) -> LiquidationType {
        if force_takeover {
            if liquidation_allowed && collateral_available >= collateral_required {
                return LiquidationType::PerpTakeover;
            }
            return LiquidationType::Skip;
        }
        if match_allowed && has_makers {
            LiquidationType::PerpWithFill
        } else if liquidation_allowed && collateral_available >= collateral_required {
            LiquidationType::PerpTakeover
        } else {
            LiquidationType::Skip
        }
    }

    fn route_policy_from_validities(
        exchange_validity: OracleValidity,
        safe_validity: OracleValidity,
        uses_pyth_update: bool,
    ) -> Option<PerpOracleRoutePolicy> {
        Some(PerpOracleRoutePolicy {
            safe_match_allowed: is_oracle_valid_for_action(
                safe_validity,
                Some(VelocityAction::FillOrderMatch),
            )
            .ok()?,
            exchange_match_allowed: is_oracle_valid_for_action(
                exchange_validity,
                Some(VelocityAction::FillOrderMatch),
            )
            .ok()?,
            // liquidate_perp validates the selected safe/MM oracle onchain
            // (`update_amm_and_check_validity` under `VelocityAction::Liquidate`),
            // so takeover eligibility must read the same view; raw exchange
            // validity stays a separate signal for floored DLOB filtering
            liquidation_allowed: is_oracle_valid_for_action(
                safe_validity,
                Some(VelocityAction::Liquidate),
            )
            .ok()?,
            uses_pyth_update,
        })
    }

    fn perp_oracle_route_policy(
        velocity: &VelocityClient,
        market_index: u16,
        slot: u64,
        pyth_price_update: Option<&PythPriceUpdate>,
    ) -> Option<PerpOracleRoutePolicy> {
        let market = velocity.try_get_perp_market_account(market_index).ok()?;
        let state = velocity.state_account().ok()?;
        let oracle = velocity.try_get_oracle_price_data_and_slot(MarketId::perp(market_index))?;
        let mut exchange_oracle = oracle.data;
        let elapsed_slots = slot.saturating_sub(oracle.slot);
        exchange_oracle.delay = exchange_oracle
            .delay
            .saturating_add(i64::try_from(elapsed_slots).unwrap_or(i64::MAX));

        // Model the update the tx would actually post. The program accepts a
        // post on feed-timestamp freshness alone (a same-price message still
        // refreshes staleness), so the preview keys on freshness too, and the
        // previewed oracle is parsed from the retained signed message with
        // the same confidence the program would store, not fabricated from
        // the scaled price.
        let previewed_oracle = pyth_price_update
            .filter(|update| {
                update.market_type == MarketType::Perp && update.market_id == market_index
            })
            .and_then(|update| preview_pyth_lazer_oracle(update, &market.oracle_source));
        let uses_pyth_update = previewed_oracle.as_ref().is_some_and(|preview| {
            match (preview.sequence_id, exchange_oracle.sequence_id) {
                (Some(next), Some(current)) => next > current,
                (Some(_), None) => true,
                (None, _) => false,
            }
        });
        if uses_pyth_update {
            exchange_oracle = previewed_oracle?;
        }

        let validity_guard_rails: velocity_rs::program::state::state::ValidityGuardRails =
            unsafe { std::mem::transmute_copy(&state.oracle_guard_rails.validity) };
        let slot_clock = velocity.slot_clock();
        let exchange_validity = oracle_validity(
            MarketType::Perp,
            market.market_index,
            market
                .market_stats
                .historical_oracle_data
                .last_oracle_price_twap,
            &exchange_oracle,
            &validity_guard_rails,
            market.get_max_confidence_interval_multiplier().ok()?,
            &market.oracle_source,
            LogMode::ExchangeOracle,
            market.oracle_slot_delay_override,
            false,
            market.oracle_low_risk_slot_delay_override,
            slot,
            slot_clock,
        )
        .ok()?;

        let mm_oracle = market
            .get_mm_oracle_price_data(exchange_oracle, slot, &validity_guard_rails, slot_clock)
            .ok()?;
        let safe_oracle = mm_oracle.get_safe_oracle_price_data();
        let safe_validity = oracle_validity(
            MarketType::Perp,
            market.market_index,
            market
                .market_stats
                .historical_oracle_data
                .last_oracle_price_twap,
            &safe_oracle,
            &validity_guard_rails,
            market.get_max_confidence_interval_multiplier().ok()?,
            &market.oracle_source,
            LogMode::SafeMMOracle,
            market.oracle_slot_delay_override,
            mm_oracle.is_safe_price_mm_sourced(),
            market.oracle_low_risk_slot_delay_override,
            slot,
            slot_clock,
        )
        .ok()?;

        Self::route_policy_from_validities(exchange_validity, safe_validity, uses_pyth_update)
    }

    /// Whether the liquidatee can participate in a DLOB match at all under
    /// the current oracle policy. Maker-level eligibility is applied per
    /// maker inside [`Self::find_top_makers`].
    fn match_participation_allowed(liquidatee: &User, policy: PerpOracleRoutePolicy) -> bool {
        policy.safe_match_allowed && (liquidatee.equity_floor == 0 || policy.exchange_match_allowed)
    }

    /// Whether one maker can take the other side of a floored-participant
    /// match: a floored maker needs the raw exchange oracle valid for the
    /// match policy, an unfloored maker always can.
    fn maker_matchable(maker: &User, exchange_match_allowed: bool) -> bool {
        maker.equity_floor == 0 || exchange_match_allowed
    }

    /// The book side whose resting orders can fill the liquidation: the
    /// liquidation order is the position's opposite (closing a long places a
    /// short taker order), so a long liquidatee fills against resting bids
    /// and a short one against resting asks.
    fn liquidation_makers_are_bids(base_asset_amount: i64) -> bool {
        base_asset_amount >= 0
    }

    fn decide_liquidation_type(
        liability: &PositionInfo,
        asset: Option<&PositionInfo>,
        has_pnl_only: bool,
    ) -> LiquidationType {
        if has_pnl_only {
            return LiquidationType::SettlePnl;
        }
        match (liability.market_type, asset.map(|a| a.market_type)) {
            (MarketType::Perp, None) => LiquidationType::PerpTakeover,
            (MarketType::Perp, Some(MarketType::Spot)) => LiquidationType::PerpPnlForDeposit,
            (MarketType::Spot, Some(MarketType::Perp)) => LiquidationType::BorrowForPerpPnl,
            (MarketType::Spot, Some(MarketType::Spot)) => LiquidationType::SpotForSpot,
            _ => LiquidationType::Skip,
        }
    }

    fn calculate_perp_collateral_requirement(
        market_state: Arc<RwLock<MarketState>>,
        market_index: u16,
        base_asset_amount: i64,
    ) -> Option<u128> {
        let state = market_state.read().unwrap().load();

        let perp_market = state.perp_market(market_index)?;
        let oracle = state.perp_oracle(market_index)?;

        let margin_ratio = perp_market
            .get_margin_ratio(
                base_asset_amount.unsigned_abs() as u128,
                MarginRequirementType::Initial,
            )
            .ok()?;

        let collateral = (base_asset_amount.abs() as u128)
            .saturating_mul(oracle.price as u128)
            .saturating_mul(QUOTE_PRECISION)
            .saturating_mul(margin_ratio as u128)
            .saturating_div(MARGIN_PRECISION_U128)
            .saturating_div(PRICE_PRECISION)
            .saturating_div(BASE_PRECISION);

        Some(collateral)
    }

    fn extract_collateral_params(
        market_state: Arc<RwLock<MarketState>>,
        liability: &PositionInfo,
        asset: &PositionInfo,
    ) -> (i64, u128, u128, u128, i64, u128, u128, u128) {
        let state = market_state.read().unwrap().load();

        let (liability_oracle, liability_precision) = if liability.market_type == MarketType::Spot {
            let oracle = state
                .spot_oracle(liability.market_index)
                .expect("liability oracle");
            let market = state
                .spot_market(liability.market_index)
                .expect("liability market");
            (oracle.price, 10_u128.pow(market.decimals))
        } else {
            let oracle = state.spot_oracle(0).expect("USDC oracle");
            (oracle.price, QUOTE_PRECISION)
        };

        let liability_weight = if liability.market_type == MarketType::Spot {
            let market = state
                .spot_market(liability.market_index)
                .expect("liability spot market");
            market.initial_liability_weight as u128
        } else {
            1u128
        };

        let (asset_oracle, asset_precision) = if asset.market_type == MarketType::Spot {
            let oracle = state.spot_oracle(asset.market_index).expect("asset oracle");
            let market = state.spot_market(asset.market_index).expect("asset market");
            (oracle.price, 10_u128.pow(market.decimals))
        } else {
            let oracle = state.spot_oracle(0).expect("USDC oracle");
            (oracle.price, QUOTE_PRECISION)
        };

        let (asset_weight, asset_weight_precision) = if asset.market_type == MarketType::Spot {
            let market = state
                .spot_market(asset.market_index)
                .expect("asset spot market");
            (
                market.initial_asset_weight as u128,
                SPOT_WEIGHT_PRECISION_U128,
            )
        } else {
            (SPOT_WEIGHT_PRECISION_U128, SPOT_WEIGHT_PRECISION_U128)
        };

        (
            liability_oracle,
            liability_precision,
            liability_weight,
            SPOT_WEIGHT_PRECISION_U128,
            asset_oracle,
            asset_precision,
            asset_weight,
            asset_weight_precision,
        )
    }

    fn calculate_net_collateral_requirement_with_params(
        liability_size: u128,
        liability: &PositionInfo,
        asset: &PositionInfo,
        liability_oracle_price: i64,
        liability_precision: u128,
        liability_weight: u128,
        liability_weight_precision: u128,
        asset_oracle_price: i64,
        asset_precision: u128,
        asset_weight: u128,
        asset_weight_precision: u128,
    ) -> i128 {
        let asset_amount_back_in_tokens = liability_size
            .saturating_mul(liability_oracle_price as u128)
            .saturating_div(asset_oracle_price as u128)
            .saturating_mul(asset_precision)
            .saturating_div(liability_precision);

        let asset_amount_back_in_collateral = asset_amount_back_in_tokens
            .saturating_mul(asset_oracle_price as u128)
            .saturating_mul(QUOTE_PRECISION)
            .saturating_mul(asset_weight)
            .saturating_div(asset_weight_precision)
            .saturating_div(asset_precision)
            .saturating_div(PRICE_PRECISION);

        let liability_collateral_impact = if liability.market_type == MarketType::Spot {
            liability_size
                .saturating_mul(liability_oracle_price as u128)
                .saturating_div(PRICE_PRECISION)
                .saturating_mul(QUOTE_PRECISION)
                .saturating_div(liability_precision)
                .saturating_mul(liability_weight)
                .saturating_div(liability_weight_precision)
        } else {
            liability
                .collateral_required
                .unsigned_abs()
                .min(liability_size)
        };

        let net_impact = liability_collateral_impact.saturating_sub(
            asset_amount_back_in_collateral.min(asset.collateral_required.unsigned_abs()),
        );

        net_impact as i128
    }

    fn calculate_net_collateral_requirement(
        liability_size: u128,
        liability: &PositionInfo,
        asset: &PositionInfo,
        market_state: Arc<RwLock<MarketState>>,
    ) -> i128 {
        let (l_oracle, l_prec, l_weight, l_weight_prec, a_oracle, a_prec, a_weight, a_weight_prec) =
            Self::extract_collateral_params(market_state, liability, asset);

        Self::calculate_net_collateral_requirement_with_params(
            liability_size,
            liability,
            asset,
            l_oracle,
            l_prec,
            l_weight,
            l_weight_prec,
            a_oracle,
            a_prec,
            a_weight,
            a_weight_prec,
        )
    }

    /// finds the max liquidation amount whose collateral impact fits within available collateral,
    /// allowing unused collateral up to `tolerance`.
    fn find_max_liq_amount<F>(
        max_amount: u128,
        available_collateral: i128,
        tolerance: i128,
        impact_fn: F,
    ) -> u128
    where
        F: Fn(u128) -> i128,
    {
        let mut low = 0u128;
        let mut high = max_amount;
        let mut best = 0u128;

        while low <= high {
            let mid = (low + high) / 2;
            let impact = impact_fn(mid);
            let difference = available_collateral - impact;

            if difference < 0 {
                if mid == 0 {
                    break;
                }
                high = mid - 1;
            } else {
                best = mid;

                if difference > tolerance {
                    low = mid + 1;
                } else {
                    break;
                }
            }
        }

        best
    }

    /// Calculate collateral requirement for all perp positions
    fn get_perp_positions_info(
        market_state: Arc<RwLock<MarketState>>,
        perp_positions: &[PerpPosition],
    ) -> Vec<PositionInfo> {
        let state = market_state.read().unwrap().load();

        perp_positions
            .iter()
            .filter(|p| p.base_asset_amount != 0 || p.quote_asset_amount != 0)
            .filter_map(|pos| {
                let perp_market = state.perp_market(pos.market_index)?;
                let oracle = state.perp_oracle(pos.market_index)?;

                if pos.base_asset_amount == 0 && pos.quote_asset_amount != 0 {
                    let _usdc = state.spot_market(0)?;

                    // signed: positive = claimable by the user (asset), negative = owed (liability)
                    let claimable_pnl: i128 = pos.get_claimable_pnl(oracle.price, 0).unwrap_or(0);

                    Some(PositionInfo {
                        market_type: MarketType::Perp,
                        market_index: pos.market_index,
                        is_asset: claimable_pnl > 0,
                        collateral_required: claimable_pnl.abs(),
                        base_amount: 0,
                        quote_amount: pos.quote_asset_amount,
                    })
                } else {
                    let margin_ratio = perp_market
                        .get_margin_ratio(
                            pos.base_asset_amount.unsigned_abs() as u128,
                            MarginRequirementType::Initial,
                        )
                        .ok()?;

                    let collateral = (pos.base_asset_amount.abs() as u128)
                        .saturating_mul(oracle.price as u128)
                        .saturating_mul(QUOTE_PRECISION)
                        .saturating_mul(margin_ratio as u128)
                        .saturating_div(MARGIN_PRECISION_U128)
                        .saturating_div(PRICE_PRECISION)
                        .saturating_div(BASE_PRECISION);

                    Some(PositionInfo {
                        market_type: MarketType::Perp,
                        market_index: pos.market_index,
                        is_asset: pos.quote_asset_amount > 0,
                        collateral_required: collateral as i128,
                        base_amount: pos.base_asset_amount,
                        quote_amount: pos.quote_asset_amount,
                    })
                }
            })
            .collect()
    }

    /// Calculate collateral required for all spot positions
    fn get_spot_positions_info(
        market_state: Arc<RwLock<MarketState>>,
        spot_positions: &[SpotPosition],
    ) -> Vec<PositionInfo> {
        let state = market_state.read().unwrap().load();

        spot_positions
            .iter()
            .filter(|p| !p.is_available())
            .filter_map(|pos| {
                let spot_market = state.spot_market(pos.market_index)?;
                let oracle = state.spot_oracle(pos.market_index)?;

                let token_amount = pos.get_signed_token_amount(&spot_market).ok()?;

                let token_precision = 10_u128.pow(spot_market.decimals);
                let weight = if pos.balance_type == SpotBalanceType::Deposit {
                    spot_market.initial_asset_weight
                } else {
                    spot_market.initial_liability_weight
                };

                let collateral_impact = (token_amount.abs() as u128)
                    .saturating_mul(oracle.price as u128)
                    .saturating_div(PRICE_PRECISION)
                    .saturating_mul(QUOTE_PRECISION)
                    .saturating_div(token_precision)
                    .saturating_mul(weight as u128)
                    .saturating_div(SPOT_WEIGHT_PRECISION_U128);

                Some(PositionInfo {
                    market_type: MarketType::Spot,
                    market_index: pos.market_index,
                    is_asset: pos.balance_type == SpotBalanceType::Deposit,
                    collateral_required: collateral_impact as i128,
                    base_amount: token_amount as i64,
                    quote_amount: 0,
                })
            })
            .collect()
    }

    // Port of  https://github.com/velocity-exchange/velocity-v1/blob/master/packages/sdk/src/user.ts#L3941-L3971
    fn get_safest_tiers(user_account: &User, velocity: &VelocityClient) -> (u8, u8) {
        let mut safest_perp_tier = 4;
        let mut safest_spot_tier = 4;

        for perp_position in user_account
            .perp_positions
            .iter()
            .filter(|p| !p.is_available())
        {
            // a zero-base position with positive unsettled pnl is a claim on
            // the market's pnl pool, not a liability (mirrors
            // calculate_user_safest_position_tiers in the program)
            if !perp_position.is_open_position()
                && !perp_position.has_open_order()
                && perp_position.isolated_position_scaled_balance == 0
                && perp_position.quote_asset_amount > 0
            {
                continue;
            }
            if let Some(perp_market) = velocity
                .program_data()
                .perp_market_config_by_index(perp_position.market_index)
            {
                safest_perp_tier = safest_perp_tier.min(perp_market.contract_tier.to_number());
            }
        }

        for spot_position in user_account
            .spot_positions
            .iter()
            .filter(|p| !p.is_available() && p.balance_type != SpotBalanceType::Deposit)
        {
            if let Some(spot_market) = velocity
                .program_data()
                .spot_market_config_by_index(spot_position.market_index)
            {
                safest_spot_tier = safest_spot_tier.min(spot_market.asset_tier.to_number());
            }
        }

        (safest_perp_tier, safest_spot_tier)
    }

    /// Pick the largest liability and best matching asset
    fn pick_best_asset_liability_combo(
        market_state: Arc<RwLock<MarketState>>,
        perp_positions: &[PositionInfo],
        spot_positions: &[PositionInfo],
        safest_perp_tier: u8,
        safest_spot_tier: u8,
    ) -> Option<(PositionInfo, Option<PositionInfo>)> {
        let state = market_state.read().unwrap().load();

        let (mut liabilities, mut assets): (Vec<PositionInfo>, Vec<PositionInfo>) = perp_positions
            .iter()
            .chain(spot_positions)
            .cloned()
            .partition(|p| !p.is_asset);

        liabilities.sort_by(|a, b| {
            b.collateral_required
                .abs()
                .cmp(&a.collateral_required.abs())
        });

        assets.sort_by(|a, b| {
            b.collateral_required
                .abs()
                .cmp(&a.collateral_required.abs())
        });

        let largest_liability = liabilities.into_iter().find(|liability| {
            if liability.market_type == MarketType::Spot {
                true
            } else {
                match state.perp_market(liability.market_index) {
                    Some(perp_market) => perp_tier_is_as_safe_as(
                        perp_market.contract_tier.to_number(),
                        safest_perp_tier,
                        safest_spot_tier,
                    ),
                    None => false,
                }
            }
        })?;

        let best_asset = assets.first().cloned();

        Some((largest_liability, best_asset))
    }

    /// Returns the subaccount with most free collateral that has position room
    fn find_best_subaccount_for_liquidation(
        velocity: &VelocityClient,
        subaccounts: &[Pubkey],
        needs_perp_room: bool,
        needs_spot_room: bool,
        free_collateral_per_subaccount: &DashMap<Pubkey, u128>,
        min_collateral_required: u128,
    ) -> Option<Pubkey> {
        let mut candidates: Vec<(Pubkey, u128)> = subaccounts
            .iter()
            .filter_map(|&subaccount| {
                let Some(free_collateral_info) = free_collateral_per_subaccount.get(&subaccount)
                else {
                    return None;
                };

                if *free_collateral_info < min_collateral_required {
                    return None;
                }

                let user = velocity.try_get_account::<User>(&subaccount).ok()?;

                if needs_perp_room {
                    let active_perp_positions = user
                        .perp_positions
                        .iter()
                        .filter(|p| p.base_asset_amount != 0 || p.quote_asset_amount != 0)
                        .count();

                    if active_perp_positions >= 8 {
                        return None;
                    }
                }

                if needs_spot_room {
                    let active_spot_positions = user
                        .spot_positions
                        .iter()
                        .filter(|p| !p.is_available())
                        .count();

                    if active_spot_positions >= 8 {
                        return None;
                    }
                }

                Some((subaccount, *free_collateral_info))
            })
            .collect();

        if candidates.is_empty() {
            return None;
        }

        candidates.sort_by_key(|&(_, free_collateral)| std::cmp::Reverse(free_collateral));

        Some(candidates[0].0)
    }

    /// Find top makers for a perp position
    /// Scan one side of the book until three loaded, unique, eligible makers
    /// are collected. Eligibility is applied during the scan, not after a
    /// cap: a prefix of duplicate, unloadable or floored-ineligible entries
    /// must not hide an eligible maker further down the book.
    fn collect_top_makers(
        velocity: &VelocityClient,
        orders: impl Iterator<Item = L3Order>,
        exchange_match_allowed: bool,
    ) -> Vec<User> {
        let mut seen = HashSet::new();
        let mut makers: Vec<User> = Vec::with_capacity(3);
        for order in orders {
            if !order.is_maker() || !seen.insert(order.user) {
                continue;
            }
            let Ok(maker) = velocity.try_get_account::<User>(&order.user) else {
                continue;
            };
            if !Self::maker_matchable(&maker, exchange_match_allowed) {
                continue;
            }
            makers.push(maker);
            if makers.len() == 3 {
                break;
            }
        }
        makers
    }

    fn find_top_makers(
        velocity: &VelocityClient,
        dlob: &'static DLOB,
        market_state: Arc<RwLock<MarketState>>,
        market_index: u16,
        base_asset_amount: i64,
        exchange_match_allowed: bool,
    ) -> Option<Vec<User>> {
        let l3_book = dlob.get_l3_snapshot_safe(market_index, MarketType::Perp)?;

        let oracle_price = {
            let state = market_state.read().unwrap();
            match state.get_perp_oracle_price(market_index) {
                Some(data) if data.price > 0 => data.price as u64,
                _ => return None,
            }
        };

        // only want maker orders so don't pass vamm or trigger price
        let makers = if Self::liquidation_makers_are_bids(base_asset_amount) {
            Self::collect_top_makers(
                velocity,
                l3_book.bids(Some(oracle_price), None, None),
                exchange_match_allowed,
            )
        } else {
            Self::collect_top_makers(
                velocity,
                l3_book.asks(Some(oracle_price), None, None),
                exchange_match_allowed,
            )
        };

        if makers.is_empty() {
            log::warn!(target: TARGET, "no eligible makers found. market={}", market_index);
            return None;
        }

        Some(makers)
    }

    /// Try to fill liquidation with order match
    async fn try_liquidate_with_match(
        velocity: &VelocityClient,
        market_index: u16,
        subaccount: Pubkey,
        liquidatee_subaccount: Pubkey,
        top_makers: &[User],
        tx_sender: TxSender,
        priority_fee: u64,
        cu_limit: u32,
        slot: u64,
        pyth_price_update: Option<PythPriceUpdate>,
        dlob_url: &str,
    ) -> LiquidationOutcome {
        if top_makers.is_empty() {
            log::debug!(target: TARGET, "skip empty maker cross. market={market_index} user={liquidatee_subaccount}");
            return LiquidationOutcome::Skipped("no_makers");
        }

        let keeper_account_data = velocity.try_get_account::<User>(&subaccount);
        if keeper_account_data.is_err() {
            log::debug!(target: TARGET, "keeper acc lookup failed={subaccount:?}");
            return LiquidationOutcome::Skipped("keeper_account_lookup_failed");
        }
        let liquidatee_subaccount_data = velocity.try_get_account::<User>(&liquidatee_subaccount);
        if liquidatee_subaccount_data.is_err() {
            log::debug!(target: TARGET, "liquidatee acc lookup failed={liquidatee_subaccount:?}");
            return LiquidationOutcome::Skipped("liquidatee_account_lookup_failed");
        }

        let mut tx_builder = TransactionBuilder::new(
            velocity.program_data(),
            subaccount,
            std::borrow::Cow::Owned(keeper_account_data.unwrap()),
            false,
        )
        .with_priority_fee(priority_fee, Some(cu_limit));

        if let Some(ref update) = pyth_price_update {
            tx_builder =
                tx_builder.post_pyth_lazer_oracle_update(&[update.feed_id], &update.message);
        }

        let liquidatee_data = liquidatee_subaccount_data.unwrap();

        // A perp liquidation reverts if the account holds resting CLOB orders.
        // Force-cancel them first, in the same transaction. On a feed failure
        // the account may still hold orders, so skip this attempt and retry.
        match resolve_clob_force_cancel(
            velocity,
            dlob_url,
            liquidatee_subaccount,
            &liquidatee_data,
            market_index,
        )
        .await
        {
            Ok(Some((order_refs, clob_fill))) => {
                tx_builder =
                    tx_builder.force_cancel_clob_orders(&liquidatee_data, order_refs, clob_fill);
            }
            Ok(None) => {}
            Err(()) => return LiquidationOutcome::Skipped("clob_force_cancel_unavailable"),
        }

        tx_builder =
            tx_builder.liquidate_perp_with_fill(market_index, &liquidatee_data, top_makers);

        // Ask for the ceiling here and let the send path size it down. It
        // simulates before it signs, so the limit that gets signed comes from
        // what the transaction burned rather than from a guess about the shape
        // of its account list — and the limit that gets signed is the one the
        // network bills.
        tx_builder = tx_builder.set_ix(
            1,
            ComputeBudgetInstruction::set_compute_unit_limit(MAX_COMPUTE_UNITS as u32),
        );

        let tx = tx_builder.build();

        // Fill doesn't require collateral management
        match tx_sender
            .send_tx(
                tx,
                TxIntent::LiquidateWithFill {
                    market_index,
                    liquidatee: liquidatee_subaccount,
                    slot,
                },
                cu_limit as u64,
            )
            .await
        {
            Some(sig) => {
                log::info!(
                    target: TARGET,
                    "liquidation tx sent: kind=perp_with_fill liquidatee={liquidatee_subaccount:?} market={market_index} makers={} sig={sig} slot={slot}",
                    top_makers.len(),
                );
                LiquidationOutcome::TxSent
            }
            None => LiquidationOutcome::Skipped("tx_send_failed"),
        }
    }

    /// Try to liquidate by taking over position
    async fn try_liquidate_with_collateral(
        &self,
        velocity: &VelocityClient,
        market_index: u16,
        subaccount: Pubkey,
        liquidatee_subaccount: Pubkey,
        base_asset_amount: u64,
        collateral_required: u128,
        tx_sender: TxSender,
        priority_fee: u64,
        cu_limit: u32,
        slot: u64,
        pyth_price_update: Option<PythPriceUpdate>,
        dlob_url: &str,
    ) -> LiquidationOutcome {
        let keeper_account_data = velocity.try_get_account::<User>(&subaccount);
        if keeper_account_data.is_err() {
            log::debug!(target: TARGET, "keeper acc lookup failed={subaccount:?}");
            return LiquidationOutcome::Skipped("keeper_account_lookup_failed");
        }
        let liquidatee_subaccount_data = velocity.try_get_account::<User>(&liquidatee_subaccount);
        if liquidatee_subaccount_data.is_err() {
            log::debug!(target: TARGET, "liquidatee acc lookup failed={liquidatee_subaccount:?}");
            return LiquidationOutcome::Skipped("liquidatee_account_lookup_failed");
        }

        let mut tx_builder = TransactionBuilder::new(
            velocity.program_data(),
            subaccount,
            std::borrow::Cow::Owned(keeper_account_data.unwrap()),
            false,
        )
        .with_priority_fee(priority_fee, Some(cu_limit));

        if let Some(ref update) = pyth_price_update {
            tx_builder =
                tx_builder.post_pyth_lazer_oracle_update(&[update.feed_id], &update.message);
        }

        let liquidatee_data = liquidatee_subaccount_data.unwrap();

        // A perp liquidation reverts if the account holds resting CLOB orders.
        // Force-cancel them first, in the same transaction. On a feed failure
        // the account may still hold orders, so skip this attempt and retry.
        match resolve_clob_force_cancel(
            velocity,
            dlob_url,
            liquidatee_subaccount,
            &liquidatee_data,
            market_index,
        )
        .await
        {
            Ok(Some((order_refs, clob_fill))) => {
                tx_builder =
                    tx_builder.force_cancel_clob_orders(&liquidatee_data, order_refs, clob_fill);
            }
            Ok(None) => {}
            Err(()) => return LiquidationOutcome::Skipped("clob_force_cancel_unavailable"),
        }

        tx_builder =
            tx_builder.liquidate_perp(market_index, &liquidatee_data, base_asset_amount, None);

        // Ask for the ceiling here and let the send path size it down. It
        // simulates before it signs, so the limit that gets signed comes from
        // what the transaction burned rather than from a guess about the shape
        // of its account list — and the limit that gets signed is the one the
        // network bills.
        tx_builder = tx_builder.set_ix(
            1,
            ComputeBudgetInstruction::set_compute_unit_limit(MAX_COMPUTE_UNITS as u32),
        );

        let tx = tx_builder.build();

        match tx_sender
            .send_tx(
                tx,
                TxIntent::LiquidatePerp {
                    market_index,
                    liquidatee: liquidatee_subaccount,
                    slot,
                },
                cu_limit as u64,
            )
            .await
        {
            Some(sig) => {
                self.txs_in_flight
                    .entry(subaccount)
                    .or_insert_with(HashSet::new)
                    .insert(sig);

                // Reserve and release must use the same amount: the TxWorker credits
                // back whatever is stored here when the tx fails or goes stale
                self.tx_sig_to_collateral
                    .insert(sig, (collateral_required, current_time_millis()));

                if let Some(mut free) = self.free_collateral_per_subaccount.get_mut(&subaccount) {
                    *free = free.saturating_sub(collateral_required);
                }

                log::info!(
                    target: TARGET,
                    "liquidation tx sent: kind=perp_takeover liquidatee={liquidatee_subaccount:?} market={market_index} base_asset_amount={base_asset_amount} collateral_reserved={collateral_required} subaccount={subaccount:?} sig={sig} slot={slot}",
                );
                LiquidationOutcome::TxSent
            }
            None => LiquidationOutcome::Skipped("tx_send_failed"),
        }
    }

    async fn try_liquidate_perp_position(
        &self,
        velocity: &VelocityClient,
        dlob: &'static DLOB,
        market_state: Arc<RwLock<MarketState>>,
        metrics: Arc<Metrics>,
        subaccounts: &[Pubkey],
        liquidatee: Pubkey,
        user_account: &User,
        position: &PerpPosition,
        kind: &'static str,
        tx_sender: TxSender,
        priority_fee: u64,
        cu_limit: u32,
        slot: u64,
        pyth_price_update: Option<PythPriceUpdate>,
    ) -> LiquidationOutcome {
        let Some(match_subaccount) = subaccounts.first() else {
            return LiquidationOutcome::Skipped("no_subaccount");
        };
        let Some(policy) = Self::perp_oracle_route_policy(
            velocity,
            position.market_index,
            slot,
            pyth_price_update.as_ref(),
        ) else {
            return LiquidationOutcome::Skipped("invalid_oracle");
        };
        let Some(collateral_required) = Self::calculate_perp_collateral_requirement(
            Arc::clone(&market_state),
            position.market_index,
            position.base_asset_amount,
        ) else {
            return LiquidationOutcome::Skipped("collateral_calc_failed");
        };

        let free_collateral = subaccounts
            .iter()
            .filter_map(|subaccount| {
                self.free_collateral_per_subaccount
                    .get(subaccount)
                    .map(|value| *value)
            })
            .max()
            .unwrap_or(0);
        let makers = if Self::match_participation_allowed(user_account, policy) {
            Self::find_top_makers(
                velocity,
                dlob,
                Arc::clone(&market_state),
                position.market_index,
                position.base_asset_amount,
                policy.exchange_match_allowed,
            )
        } else {
            None
        };
        let fallback_key = (liquidatee, position.market_index);
        let force_takeover = peek_perp_fill_fallback(
            &self.perp_fill_fallbacks,
            fallback_key,
            policy.liquidation_allowed,
            free_collateral,
            collateral_required,
            current_time_millis(),
        );
        let method = Self::decide_perp_method(
            free_collateral,
            collateral_required,
            makers.is_some(),
            policy.safe_match_allowed,
            policy.liquidation_allowed,
            force_takeover,
        );
        let pyth_update = pyth_price_update.filter(|_| policy.uses_pyth_update);

        metrics
            .liquidation_attempts
            .with_label_values(&["perp"])
            .inc();
        log::info!(
            target: TARGET,
            "attempting liquidation: kind={kind} liquidatee={liquidatee:?} market={} method={method:?} force_takeover={force_takeover} slot={slot}",
            position.market_index,
        );

        let outcome = match method {
            LiquidationType::PerpWithFill => {
                let Some(makers) = makers else {
                    return LiquidationOutcome::Skipped("no_makers");
                };
                Self::try_liquidate_with_match(
                    velocity,
                    position.market_index,
                    *match_subaccount,
                    liquidatee,
                    makers.as_slice(),
                    tx_sender,
                    priority_fee,
                    cu_limit,
                    slot,
                    pyth_update,
                    &self.dlob_url,
                )
                .await
            }
            LiquidationType::PerpTakeover => {
                let Some(subaccount) = Self::find_best_subaccount_for_liquidation(
                    velocity,
                    subaccounts,
                    true,
                    false,
                    &self.free_collateral_per_subaccount,
                    collateral_required,
                ) else {
                    return LiquidationOutcome::Skipped("no_subaccount_for_takeover");
                };
                self.try_liquidate_with_collateral(
                    velocity,
                    position.market_index,
                    subaccount,
                    liquidatee,
                    position.base_asset_amount.unsigned_abs(),
                    collateral_required,
                    tx_sender,
                    priority_fee,
                    cu_limit,
                    slot,
                    pyth_update,
                    &self.dlob_url,
                )
                .await
            }
            _ if !policy.liquidation_allowed && makers.is_none() => {
                LiquidationOutcome::Skipped("oracle_not_eligible")
            }
            _ => LiquidationOutcome::Skipped("no_eligible_route"),
        };

        // the fallback marker is one-shot against a *sent* takeover, not
        // against the decision to route one; see `peek_perp_fill_fallback`
        if force_takeover && outcome.is_sent() {
            self.perp_fill_fallbacks.remove(&fallback_key);
        }

        outcome
    }

    /// Attempt perp liquidation with order matching or collateral
    async fn liquidate_perp(
        &self,
        velocity: &VelocityClient,
        dlob: &'static DLOB,
        market_state: Arc<RwLock<MarketState>>,
        metrics: Arc<Metrics>,
        subaccounts: &[Pubkey],
        liquidatee: Pubkey,
        user_account: Arc<User>,
        tx_sender: TxSender,
        priority_fee: u64,
        cu_limit: u32,
        slot: u64,
        pyth_price_updates: &BTreeMap<u16, PythPriceUpdate>,
        status: &UserMarginStatus,
    ) -> LiquidationOutcome {
        let mut last_skip: Option<&'static str> = None;
        for (market_index, iso_status) in &status.isolated {
            if *iso_status != MarginStatus::Liquidatable {
                continue;
            }
            let Some(position) = user_account.perp_positions.iter().find(|position| {
                position.market_index == *market_index
                    && position.isolated_position_scaled_balance != 0
            }) else {
                continue;
            };

            let outcome = self
                .try_liquidate_perp_position(
                    velocity,
                    dlob,
                    Arc::clone(&market_state),
                    Arc::clone(&metrics),
                    subaccounts,
                    liquidatee,
                    &user_account,
                    position,
                    "perp_isolated",
                    tx_sender.clone(),
                    priority_fee,
                    cu_limit,
                    slot,
                    pyth_price_updates.get(market_index).cloned(),
                )
                .await;
            if outcome.is_sent() {
                return outcome;
            }
            last_skip = Some(outcome.reason());
        }

        // Cross margin
        if status.cross != MarginStatus::Liquidatable {
            return LiquidationOutcome::Skipped(last_skip.unwrap_or("cross_not_liquidatable"));
        }

        let Some(pos) = user_account
            .perp_positions
            .iter()
            .filter(|p| p.base_asset_amount != 0)
            .max_by_key(|p| p.quote_asset_amount.unsigned_abs())
        else {
            log::info!(
                target: TARGET,
                "no perp positions with base_asset_amount for {:?}, skipping perp liquidation",
                liquidatee
            );
            return LiquidationOutcome::Skipped("no_perp_positions");
        };

        self.try_liquidate_perp_position(
            velocity,
            dlob,
            market_state,
            metrics,
            subaccounts,
            liquidatee,
            &user_account,
            pos,
            "perp_cross",
            tx_sender,
            priority_fee,
            cu_limit,
            slot,
            pyth_price_updates.get(&pos.market_index).cloned(),
        )
        .await
    }

    /// Attempt spot liquidation with Jupiter swap
    async fn liquidate_spot(
        velocity: VelocityClient,
        metrics: Arc<Metrics>,
        market_state: Arc<RwLock<MarketState>>,
        subaccounts: &[Pubkey],
        liquidatee: Pubkey,
        user_account: Arc<User>,
        tx_sender: TxSender,
        priority_fee: u64,
        cu_limit: u32,
        slot: u64,
    ) -> LiquidationOutcome {
        let authority = velocity.wallet.authority();
        let Some(subaccount) = subaccounts.first() else {
            log::warn!(target: TARGET, "no subaccount configured");
            return LiquidationOutcome::Skipped("no_subaccount");
        };

        let mut any_sent = false;
        for pos in user_account
            .spot_positions
            .iter()
            .filter(|p| matches!(p.balance_type, SpotBalanceType::Borrow) && !p.is_available())
        {
            // skip permanently blocked markets
            if BLOCKED_SPOT_MARKETS.contains(&pos.market_index) {
                continue;
            }

            let spot_market = {
                let state = market_state.read().unwrap();
                let state_data = state.load();
                match state_data.spot_market(pos.market_index) {
                    Some(m) => *m,
                    None => continue,
                }
            };

            let token_amount = match pos.get_token_amount(&spot_market) {
                Ok(amount) => amount as u64,
                Err(_) => continue,
            };

            // Filter dust positions
            if token_amount < spot_market.min_order_size * 2 {
                // log::debug!(
                //     target: TARGET,
                //     "skip dust spot position. market={}, amount={}",
                //     pos.market_index,
                //     token_amount
                // );
                continue;
            }

            metrics
                .liquidation_attempts
                .with_label_values(&["spot"])
                .inc();

            // Find their largest deposit to use as collateral
            let Some(asset_market_index) = user_account
                .spot_positions
                .iter()
                .filter(|p| matches!(p.balance_type, SpotBalanceType::Deposit) && !p.is_available())
                .max_by_key(|p| p.scaled_balance)
                .map(|p| p.market_index)
            else {
                log::warn!(
                    target: TARGET,
                    "no asset found for user {:?}, skipping spot liquidation",
                    liquidatee
                );
                continue;
            };

            log::info!(
                target: TARGET,
                "attempting spot liquidation: user={:?}, asset_market={}, liability_market={}, amount={}",
                liquidatee,
                asset_market_index,
                pos.market_index,
                token_amount
            );

            // Fetch accounts once
            let keeper_account_data = match velocity.try_get_account::<User>(&subaccount) {
                Ok(data) => data,
                Err(_) => {
                    log::info!(target: TARGET, "keeper account not found: {:?}", &subaccount);
                    continue;
                }
            };

            let liquidatee_account_data = match velocity.try_get_account::<User>(&liquidatee) {
                Ok(data) => data,
                Err(_) => {
                    log::info!(target: TARGET, "liquidatee account not found: {liquidatee:?}");
                    continue;
                }
            };

            // Fetch market configs inside async block to avoid lifetime issues
            let asset_spot_market = velocity
                .program_data()
                .spot_market_config_by_index(asset_market_index)
                .expect("asset spot market");

            let liability_market_index = pos.market_index;

            let liability_spot_market = velocity
                .program_data()
                .spot_market_config_by_index(liability_market_index)
                .expect("liability spot market");

            let in_token_account =
                velocity_rs::Wallet::derive_associated_token_address(authority, asset_spot_market);
            let out_token_account = velocity_rs::Wallet::derive_associated_token_address(
                authority,
                liability_spot_market,
            );

            let t0 = std::time::Instant::now();
            let (jupiter_result, titan_result) = tokio::join!(
                velocity.jupiter_swap_query(
                    &authority,
                    token_amount,
                    100,
                    asset_market_index,
                    liability_market_index,
                    None,
                    None,
                ),
                velocity.titan_swap_query(
                    &authority,
                    token_amount,
                    Some(50),
                    titan::SwapMode::ExactIn,
                    100,
                    asset_market_index,
                    liability_market_index,
                    Some(true),
                    None,
                    None,
                )
            );

            let quote_latency_ms = t0.elapsed().as_millis() as i64;
            metrics.swap_quote_latency_ms.set(quote_latency_ms);

            if jupiter_result.is_err() && titan_result.is_err() {
                metrics.jupiter_quote_failures.inc();
                metrics.titan_quote_failures.inc();
                log::warn!(target: TARGET, "both quotes failed after {}ms", quote_latency_ms);
                continue;
            }

            let use_titan = match (&jupiter_result, &titan_result) {
                (Ok(jup), Ok(titan)) => {
                    let use_titan = titan.quote.out_amount > jup.quote.out_amount;
                    // log::debug!(
                    //     target: TARGET,
                    //     "got quotes in {}ms - jup: {}, titan: {} - using {}",
                    //     quote_latency_ms,
                    //     jup.quote.out_amount,
                    //     titan.quote.out_amount,
                    //     if use_titan { "titan" } else { "jupiter" }
                    // );
                    use_titan
                }
                (Ok(_), Err(e)) => {
                    metrics.titan_quote_failures.inc();
                    log::warn!(target: TARGET, "titan failed in {}ms, using jupiter: {:?}", quote_latency_ms, e);
                    false
                }
                (Err(e), Ok(_)) => {
                    metrics.jupiter_quote_failures.inc();
                    log::warn!(target: TARGET, "jupiter failed in {}ms, using titan: {:?}", quote_latency_ms, e);
                    true
                }
                _ => unreachable!(),
            };

            let tx = if use_titan {
                TransactionBuilder::new(
                    velocity.program_data(),
                    *subaccount,
                    std::borrow::Cow::Owned(keeper_account_data),
                    false,
                )
                .with_priority_fee(priority_fee, Some(cu_limit))
                .titan_swap_liquidate(
                    titan_result.unwrap(),
                    asset_spot_market,
                    liability_spot_market,
                    &in_token_account,
                    &out_token_account,
                    asset_market_index,
                    liability_market_index,
                    &liquidatee_account_data,
                )
                .build()
            } else {
                TransactionBuilder::new(
                    velocity.program_data(),
                    *subaccount,
                    std::borrow::Cow::Owned(keeper_account_data),
                    false,
                )
                .with_priority_fee(priority_fee, Some(cu_limit))
                .jupiter_swap_liquidate(
                    jupiter_result.unwrap(),
                    asset_spot_market,
                    liability_spot_market,
                    &in_token_account,
                    &out_token_account,
                    asset_market_index,
                    liability_market_index,
                    &liquidatee_account_data,
                )
                .build()
            };
            match tx_sender
                .send_tx(
                    tx,
                    TxIntent::LiquidateSpot {
                        asset_market_index,
                        liability_market_index,
                        liquidatee,
                        slot,
                    },
                    cu_limit as u64,
                )
                .await
            {
                Some(sig) => {
                    any_sent = true;
                    log::info!(
                        target: TARGET,
                        "liquidation tx sent: kind=spot liquidatee={liquidatee:?} asset_market={asset_market_index} liability_market={liability_market_index} amount={token_amount} venue={} sig={sig} slot={slot}",
                        if use_titan { "titan" } else { "jupiter" },
                    );
                }
                None => {
                    log::warn!(
                        target: TARGET,
                        "spot liquidation tx send failed: liquidatee={liquidatee:?} liability_market={liability_market_index}"
                    );
                }
            }
        }

        if any_sent {
            LiquidationOutcome::TxSent
        } else {
            LiquidationOutcome::Skipped("no_spot_liquidation_sent")
        }
    }

    async fn try_liquidate_perp_pnl_for_deposit(
        &self,
        velocity: &VelocityClient,
        subaccount: Pubkey,
        liquidatee: Pubkey,
        liability: &PositionInfo,
        asset: &PositionInfo,
        liq_amount: u128,
        tx_sender: TxSender,
        priority_fee: u64,
        cu_limit: u32,
        slot: u64,
        pyth_price_update: Option<PythPriceUpdate>,
    ) -> LiquidationOutcome {
        let keeper_account = match velocity.try_get_account::<User>(&subaccount) {
            Ok(data) => data,
            Err(_) => {
                log::warn!(target: TARGET, "keeper account not found");
                return LiquidationOutcome::Skipped("keeper_account_lookup_failed");
            }
        };

        let liquidatee_account = match velocity.try_get_account::<User>(&liquidatee) {
            Ok(data) => data,
            Err(_) => {
                log::warn!(target: TARGET, "liquidatee account not found");
                return LiquidationOutcome::Skipped("liquidatee_account_lookup_failed");
            }
        };

        let mut tx_builder = TransactionBuilder::new(
            velocity.program_data(),
            subaccount,
            std::borrow::Cow::Owned(keeper_account),
            false,
        )
        .with_priority_fee(priority_fee, Some(cu_limit));

        if let Some(ref update) = pyth_price_update {
            tx_builder =
                tx_builder.post_pyth_lazer_oracle_update(&[update.feed_id], &update.message);
        }

        tx_builder = tx_builder.liquidate_perp_pnl_for_deposit(
            &liquidatee_account,
            liability.market_index,
            asset.market_index,
            u128::from(liq_amount),
            None,
        );

        let tx = tx_builder.build();

        match tx_sender
            .send_tx(
                tx,
                TxIntent::LiquidatePerpPnlForDeposit {
                    perp_market_index: liability.market_index,
                    spot_market_index: asset.market_index,
                    liquidatee,
                    slot,
                },
                cu_limit as u64,
            )
            .await
        {
            Some(sig) => {
                self.txs_in_flight
                    .entry(subaccount)
                    .or_insert_with(HashSet::new)
                    .insert(sig);

                self.tx_sig_to_collateral
                    .insert(sig, (liq_amount, current_time_millis()));

                if let Some(mut free) = self.free_collateral_per_subaccount.get_mut(&subaccount) {
                    *free = free.saturating_sub(liq_amount);
                }

                log::info!(
                    target: TARGET,
                    "liquidation tx sent: kind=perp_pnl_for_deposit liquidatee={liquidatee:?} perp_market={} spot_market={} amount={liq_amount} subaccount={subaccount:?} sig={sig} slot={slot}",
                    liability.market_index,
                    asset.market_index,
                );
                LiquidationOutcome::TxSent
            }
            None => LiquidationOutcome::Skipped("tx_send_failed"),
        }
    }

    /// Attempt perp pnl for deposit liquidation
    async fn liquidate_perp_pnl_for_deposit(
        &self,
        velocity: &VelocityClient,
        market_state: Arc<RwLock<MarketState>>,
        subaccounts: &[Pubkey],
        liquidatee: Pubkey,
        liability: PositionInfo,
        asset: PositionInfo,
        tx_sender: TxSender,
        priority_fee: u64,
        cu_limit: u32,
        slot: u64,
        pyth_price_update: Option<PythPriceUpdate>,
    ) -> LiquidationOutcome {
        let net_collateral_req = Self::calculate_net_collateral_requirement(
            liability.collateral_required.unsigned_abs(),
            &liability,
            &asset,
            Arc::clone(&market_state),
        );

        let Some(subaccount) = Self::find_best_subaccount_for_liquidation(
            velocity,
            subaccounts,
            false,
            true,
            &self.free_collateral_per_subaccount,
            net_collateral_req.max(0) as u128,
        ) else {
            return LiquidationOutcome::Skipped("no_subaccount_with_collateral");
        };

        // copy out: holding the guard across the await below deadlocks against get_mut
        let Some(free_collateral) = self
            .free_collateral_per_subaccount
            .get(&subaccount)
            .map(|fc| *fc)
        else {
            return LiquidationOutcome::Skipped("no_free_collateral");
        };

        let available = free_collateral as i128;
        let tolerance = available / 10;

        let params = Self::extract_collateral_params(Arc::clone(&market_state), &liability, &asset);

        let liq_amount = if net_collateral_req <= available {
            liability.collateral_required.unsigned_abs()
        } else {
            Self::find_max_liq_amount(
                liability.collateral_required.unsigned_abs(),
                available,
                tolerance,
                |size| {
                    Self::calculate_net_collateral_requirement_with_params(
                        size, &liability, &asset, params.0, params.1, params.2, params.3, params.4,
                        params.5, params.6, params.7,
                    )
                },
            )
        };

        if liq_amount == 0 {
            return LiquidationOutcome::Skipped("zero_liq_amount");
        }

        self.try_liquidate_perp_pnl_for_deposit(
            velocity,
            subaccount,
            liquidatee,
            &liability,
            &asset,
            liq_amount,
            tx_sender,
            priority_fee,
            cu_limit,
            slot,
            pyth_price_update,
        )
        .await
    }

    async fn try_liquidate_borrow_for_perp_pnl(
        &self,
        velocity: &VelocityClient,
        subaccount: Pubkey,
        liquidatee: Pubkey,
        liability: &PositionInfo,
        asset: &PositionInfo,
        liq_amount: u128,
        tx_sender: TxSender,
        priority_fee: u64,
        cu_limit: u32,
        slot: u64,
        pyth_price_update: Option<PythPriceUpdate>,
    ) -> LiquidationOutcome {
        let keeper_account = match velocity.try_get_account::<User>(&subaccount) {
            Ok(data) => data,
            Err(_) => {
                log::warn!(target: TARGET, "keeper account not found");
                return LiquidationOutcome::Skipped("keeper_account_lookup_failed");
            }
        };

        let liquidatee_account = match velocity.try_get_account::<User>(&liquidatee) {
            Ok(data) => data,
            Err(_) => {
                log::warn!(target: TARGET, "liquidatee account not found");
                return LiquidationOutcome::Skipped("liquidatee_account_lookup_failed");
            }
        };

        let mut tx_builder = TransactionBuilder::new(
            velocity.program_data(),
            subaccount,
            std::borrow::Cow::Owned(keeper_account),
            false,
        )
        .with_priority_fee(priority_fee, Some(cu_limit));

        if let Some(ref update) = pyth_price_update {
            tx_builder =
                tx_builder.post_pyth_lazer_oracle_update(&[update.feed_id], &update.message);
        }

        tx_builder = tx_builder.liquidate_borrow_for_perp_pnl(
            &liquidatee_account,
            asset.market_index,
            liability.market_index,
            u128::from(liq_amount),
            None,
        );

        let tx = tx_builder.build();

        match tx_sender
            .send_tx(
                tx,
                TxIntent::LiquidateBorrowForPerpPnl {
                    perp_market_index: asset.market_index,
                    spot_market_index: liability.market_index,
                    liquidatee,
                    slot,
                },
                cu_limit as u64,
            )
            .await
        {
            Some(sig) => {
                self.txs_in_flight
                    .entry(subaccount)
                    .or_insert_with(HashSet::new)
                    .insert(sig);

                self.tx_sig_to_collateral
                    .insert(sig, (liq_amount, current_time_millis()));

                if let Some(mut free) = self.free_collateral_per_subaccount.get_mut(&subaccount) {
                    *free = free.saturating_sub(liq_amount);
                }

                log::info!(
                    target: TARGET,
                    "liquidation tx sent: kind=borrow_for_perp_pnl liquidatee={liquidatee:?} perp_market={} spot_market={} amount={liq_amount} subaccount={subaccount:?} sig={sig} slot={slot}",
                    asset.market_index,
                    liability.market_index,
                );
                LiquidationOutcome::TxSent
            }
            None => LiquidationOutcome::Skipped("tx_send_failed"),
        }
    }

    /// Attempt borrow for perp pnl liquidation
    async fn liquidate_borrow_for_perp_pnl(
        &self,
        velocity: &VelocityClient,
        market_state: Arc<RwLock<MarketState>>,
        subaccounts: &[Pubkey],
        liquidatee: Pubkey,
        liability: PositionInfo,
        asset: PositionInfo,
        tx_sender: TxSender,
        priority_fee: u64,
        cu_limit: u32,
        slot: u64,
        pyth_price_update: Option<PythPriceUpdate>,
    ) -> LiquidationOutcome {
        let net_collateral_req = Self::calculate_net_collateral_requirement(
            liability.base_amount.unsigned_abs() as u128,
            &liability,
            &asset,
            Arc::clone(&market_state),
        );

        let Some(subaccount) = Self::find_best_subaccount_for_liquidation(
            velocity,
            subaccounts,
            true,
            false,
            &self.free_collateral_per_subaccount,
            net_collateral_req.max(0) as u128,
        ) else {
            return LiquidationOutcome::Skipped("no_subaccount_with_collateral");
        };

        // copy out: holding the guard across the await below deadlocks against get_mut
        let Some(free_collateral) = self
            .free_collateral_per_subaccount
            .get(&subaccount)
            .map(|fc| *fc)
        else {
            return LiquidationOutcome::Skipped("no_free_collateral");
        };

        let available = free_collateral as i128;
        let tolerance = available / 10;

        let params = Self::extract_collateral_params(Arc::clone(&market_state), &liability, &asset);

        let liq_amount = if net_collateral_req <= available {
            liability.collateral_required.unsigned_abs()
        } else {
            Self::find_max_liq_amount(
                liability.collateral_required.unsigned_abs(),
                available,
                tolerance,
                |size| {
                    Self::calculate_net_collateral_requirement_with_params(
                        size, &liability, &asset, params.0, params.1, params.2, params.3, params.4,
                        params.5, params.6, params.7,
                    )
                },
            )
        };

        if liq_amount == 0 {
            return LiquidationOutcome::Skipped("zero_liq_amount");
        }

        self.try_liquidate_borrow_for_perp_pnl(
            velocity,
            subaccount,
            liquidatee,
            &liability,
            &asset,
            liq_amount,
            tx_sender,
            priority_fee,
            cu_limit,
            slot,
            pyth_price_update,
        )
        .await
    }

    // Settle perp pnl
    async fn settle_perp_pnl(
        velocity: &VelocityClient,
        subaccount: Pubkey,
        liquidatee: Pubkey,
        market_indexes: &[u16],
        tx_sender: TxSender,
        priority_fee: u64,
        cu_limit: u32,
    ) -> LiquidationOutcome {
        if market_indexes.is_empty() {
            return LiquidationOutcome::Skipped("no_settleable_markets");
        }

        let keeper_account = match velocity.try_get_account::<User>(&subaccount) {
            Ok(data) => data,
            Err(_) => {
                log::warn!(target: TARGET, "keeper account not found");
                return LiquidationOutcome::Skipped("keeper_account_lookup_failed");
            }
        };

        let liquidatee_account = match velocity.try_get_account::<User>(&liquidatee) {
            Ok(data) => data,
            Err(_) => {
                log::warn!(target: TARGET, "liquidatee account not found");
                return LiquidationOutcome::Skipped("liquidatee_account_lookup_failed");
            }
        };

        let mut tx_builder = TransactionBuilder::new(
            velocity.program_data(),
            subaccount,
            std::borrow::Cow::Owned(keeper_account),
            false,
        )
        .with_priority_fee(priority_fee, Some(cu_limit));

        for &market_index in market_indexes {
            tx_builder =
                tx_builder.settle_pnl(market_index, Some(&liquidatee), Some(&liquidatee_account));
        }

        let tx = tx_builder.build();

        match tx_sender
            .send_tx(
                tx,
                TxIntent::SettlePnl {
                    market_index: market_indexes[0],
                    subaccount: liquidatee,
                },
                cu_limit as u64,
            )
            .await
        {
            Some(sig) => {
                log::info!(
                    target: TARGET,
                    "liquidation tx sent: kind=settle_pnl liquidatee={liquidatee:?} markets={market_indexes:?} sig={sig}",
                );
                LiquidationOutcome::TxSent
            }
            None => LiquidationOutcome::Skipped("tx_send_failed"),
        }
    }
}

impl LiquidationStrategy for PrimaryLiquidationStrategy {
    fn liquidate_user<'a>(
        &'a self,
        liquidatee: Pubkey,
        user_account: Arc<User>,
        tx_sender: TxSender,
        priority_fee: u64,
        cu_limit: u32,
        slot: u64,
        pyth_price_updates: BTreeMap<u16, PythPriceUpdate>,
        status: UserMarginStatus,
    ) -> futures_util::future::BoxFuture<'a, LiquidationOutcome> {
        let perp_positions = Self::get_perp_positions_info(
            Arc::clone(&self.market_state),
            &user_account.perp_positions,
        );

        let spot_positions = Self::get_spot_positions_info(
            Arc::clone(&self.market_state),
            &user_account.spot_positions,
        );

        let (safest_perp_tier, safest_spot_tier) =
            Self::get_safest_tiers(&user_account, &self.velocity);

        let Some((liability, asset)) = Self::pick_best_asset_liability_combo(
            Arc::clone(&self.market_state),
            &perp_positions,
            &spot_positions,
            safest_perp_tier,
            safest_spot_tier,
        ) else {
            // Possibility of PerpWithFill still exists since doesn't depend on best asset/liability
            // Attempt liquidation, if none exist it will guard and fail internally
            return async move {
                self.liquidate_perp(
                    &self.velocity,
                    self.dlob,
                    Arc::clone(&self.market_state),
                    Arc::clone(&self.metrics),
                    self.subaccounts.as_slice(),
                    liquidatee,
                    Arc::clone(&user_account),
                    tx_sender,
                    priority_fee,
                    cu_limit,
                    slot,
                    &pyth_price_updates,
                    &status,
                )
                .await
            }
            .boxed();
        };

        let has_pnl_only = has_settleable_pnl_only(&perp_positions, &spot_positions);

        let liq_type = Self::decide_liquidation_type(&liability, asset.as_ref(), has_pnl_only);

        async move {
            match liq_type {
                LiquidationType::SettlePnl => {
                    let markets: Vec<u16> = perp_positions
                        .iter()
                        .filter(|p| p.base_amount == 0 && p.quote_amount != 0 && p.is_asset)
                        .map(|p| p.market_index)
                        .collect();

                    Self::settle_perp_pnl(
                        &self.velocity,
                        self.subaccounts[0],
                        liquidatee,
                        &markets,
                        tx_sender,
                        priority_fee,
                        cu_limit,
                    )
                    .await
                }
                LiquidationType::PerpTakeover | LiquidationType::PerpWithFill => {
                    self.liquidate_perp(
                        &self.velocity,
                        self.dlob,
                        Arc::clone(&self.market_state),
                        Arc::clone(&self.metrics),
                        self.subaccounts.as_slice(),
                        liquidatee,
                        Arc::clone(&user_account),
                        tx_sender.clone(),
                        priority_fee,
                        cu_limit,
                        slot,
                        &pyth_price_updates,
                        &status,
                    )
                    .await
                }
                LiquidationType::SpotForSpot => {
                    if self.use_spot_liquidation {
                        Self::liquidate_spot(
                            self.velocity.clone(),
                            Arc::clone(&self.metrics),
                            Arc::clone(&self.market_state),
                            self.subaccounts.as_slice(),
                            liquidatee,
                            user_account,
                            tx_sender.clone(),
                            priority_fee,
                            400_000,
                            slot,
                        )
                        .await
                    } else {
                        LiquidationOutcome::Skipped("spot_liquidation_disabled")
                    }
                }
                LiquidationType::PerpPnlForDeposit => {
                    let Some(asset) = asset else {
                        return LiquidationOutcome::Skipped("no_asset_for_perp_pnl_for_deposit");
                    };
                    // the perp market in this route is the liability side
                    let pyth_price_update =
                        pyth_price_updates.get(&liability.market_index).cloned();
                    self.liquidate_perp_pnl_for_deposit(
                        &self.velocity,
                        Arc::clone(&self.market_state),
                        self.subaccounts.as_slice(),
                        liquidatee,
                        liability,
                        asset,
                        tx_sender,
                        priority_fee,
                        cu_limit,
                        slot,
                        pyth_price_update,
                    )
                    .await
                }
                LiquidationType::BorrowForPerpPnl => {
                    let Some(asset) = asset else {
                        return LiquidationOutcome::Skipped("no_asset_for_borrow_for_perp_pnl");
                    };
                    // the perp market in this route is the asset side
                    let pyth_price_update = pyth_price_updates.get(&asset.market_index).cloned();
                    self.liquidate_borrow_for_perp_pnl(
                        &self.velocity,
                        Arc::clone(&self.market_state),
                        self.subaccounts.as_slice(),
                        liquidatee,
                        liability,
                        asset,
                        tx_sender,
                        priority_fee,
                        cu_limit,
                        slot,
                        pyth_price_update,
                    )
                    .await
                }
                LiquidationType::Skip => LiquidationOutcome::Skipped("no_viable_strategy"),
            }
        }
        .boxed()
    }
}

/// A user's only remaining exposure is settleable positive perp pnl: no perp base
/// positions, no spot liabilities, and at least one settled-pnl-only perp position.
///
/// A user with any open perp base position must go through a real liquidation path —
/// settled pnl in one market must not shadow a liquidatable position in another.
fn has_settleable_pnl_only(
    perp_positions: &[PositionInfo],
    spot_positions: &[PositionInfo],
) -> bool {
    perp_positions.iter().all(|p| p.base_amount == 0)
        && perp_positions
            .iter()
            .any(|p| p.base_amount == 0 && p.quote_amount != 0 && p.is_asset)
        && !spot_positions.iter().any(|s| !s.is_asset)
}

#[cfg(test)]
mod tests {
    use {super::*, velocity_rs::market_state::IsolatedMarginCalculation};

    fn margin_calc(
        total_collateral: i128,
        margin_requirement: u128,
    ) -> SimplifiedMarginCalculation {
        SimplifiedMarginCalculation {
            total_collateral,
            total_collateral_buffer: 0,
            margin_requirement,
            margin_requirement_plus_buffer: margin_requirement,
            isolated_margin_calculations: Default::default(),
            with_perp_isolated_liability: false,
            with_spot_isolated_liability: false,
        }
    }

    fn iso_calc(
        market_index: u16,
        total_collateral: i128,
        margin_requirement: u128,
    ) -> IsolatedMarginCalculation {
        IsolatedMarginCalculation {
            market_index,
            margin_requirement,
            total_collateral,
            total_collateral_buffer: 0,
            margin_requirement_plus_buffer: margin_requirement,
        }
    }

    fn perp_info(market_index: u16, is_asset: bool, base: i64, quote: i64) -> PositionInfo {
        PositionInfo {
            market_type: MarketType::Perp,
            market_index,
            is_asset,
            collateral_required: 1_000,
            base_amount: base,
            quote_amount: quote,
        }
    }

    fn spot_info(market_index: u16, is_asset: bool) -> PositionInfo {
        PositionInfo {
            market_type: MarketType::Spot,
            market_index,
            is_asset,
            collateral_required: 1_000,
            base_amount: 100,
            quote_amount: 0,
        }
    }

    #[test]
    fn cross_liquidatable_below_maintenance_margin() {
        // mirrors program: liquidatable iff total_collateral < margin_requirement
        let status = check_margin_status(&margin_calc(99, 100));
        assert_eq!(status.cross, MarginStatus::Liquidatable);
        assert!(status.is_liquidatable());
    }

    #[test]
    fn cross_insolvent_is_liquidatable() {
        let status = check_margin_status(&margin_calc(-500, 100));
        assert_eq!(status.cross, MarginStatus::Liquidatable);
    }

    #[test]
    fn cross_exactly_at_requirement_is_not_liquidatable() {
        // program uses >=: meeting the requirement exactly is not liquidatable
        let status = check_margin_status(&margin_calc(100, 100));
        assert_ne!(status.cross, MarginStatus::Liquidatable);
        // but zero free margin is high risk
        assert_eq!(status.cross, MarginStatus::HighRisk);
    }

    #[test]
    fn cross_high_risk_and_safe_thresholds() {
        // free margin ratio 5% < 10% threshold
        assert_eq!(
            check_margin_status(&margin_calc(105, 100)).cross,
            MarginStatus::HighRisk
        );
        // free margin ratio 50%
        assert_eq!(
            check_margin_status(&margin_calc(150, 100)).cross,
            MarginStatus::Safe
        );
        // empty account
        assert_eq!(
            check_margin_status(&margin_calc(0, 0)).cross,
            MarginStatus::Safe
        );
    }

    #[test]
    fn insolvent_isolated_position_is_liquidatable() {
        // regression: insolvent (total_collateral <= 0) isolated positions were
        // skipped entirely and never flagged liquidatable
        let mut calc = margin_calc(1_000, 100); // cross is safe
        calc.isolated_margin_calculations[0] = iso_calc(3, -50, 100);
        calc.isolated_margin_calculations[1] = iso_calc(7, 0, 100);

        let status = check_margin_status(&calc);
        assert!(status.isolated.contains(&(3, MarginStatus::Liquidatable)));
        assert!(status.isolated.contains(&(7, MarginStatus::Liquidatable)));
        assert!(status.is_liquidatable());
    }

    #[test]
    fn healthy_isolated_position_not_flagged() {
        let mut calc = margin_calc(1_000, 100);
        calc.isolated_margin_calculations[0] = iso_calc(3, 200, 100);
        let status = check_margin_status(&calc);
        assert!(status.isolated.is_empty());
        assert!(!status.is_liquidatable());
    }

    #[test]
    fn decide_perp_method_prefers_fill_falls_back_to_takeover() {
        assert_eq!(
            PrimaryLiquidationStrategy::decide_perp_method(0, 100, true, true, true, false),
            LiquidationType::PerpWithFill
        );
        assert_eq!(
            PrimaryLiquidationStrategy::decide_perp_method(100, 100, true, false, true, false),
            LiquidationType::PerpTakeover
        );
        assert_eq!(
            PrimaryLiquidationStrategy::decide_perp_method(99, 100, true, false, true, false),
            LiquidationType::Skip
        );
        assert_eq!(
            PrimaryLiquidationStrategy::decide_perp_method(100, 100, true, true, true, true),
            LiquidationType::PerpTakeover
        );
    }

    #[test]
    fn perp_fill_fallback_survives_failed_routings() {
        let fallbacks = DashMap::new();
        let key = (Pubkey::new_unique(), 7);
        let now = 1_000_u64;
        let marker = PerpFillFallback {
            recorded_ms: now,
            attempts: 0,
        };

        // gates failing must not consume the one-shot signal
        fallbacks.insert(key, marker);
        assert!(!peek_perp_fill_fallback(
            &fallbacks, key, true, 99, 100, now
        ));
        assert!(fallbacks.contains_key(&key));
        assert!(!peek_perp_fill_fallback(
            &fallbacks, key, false, 100, 100, now
        ));
        assert!(fallbacks.contains_key(&key));

        // a passing peek routes the takeover but keeps the marker: the send
        // can still fail, and the next pass must not fall back to the maker
        // route that already failed onchain
        assert!(peek_perp_fill_fallback(
            &fallbacks, key, true, 100, 100, now
        ));
        assert!(fallbacks.contains_key(&key));
        assert_eq!(fallbacks.get(&key).unwrap().attempts, 1);
    }

    #[test]
    fn perp_fill_fallback_is_bounded() {
        let fallbacks = DashMap::new();
        let key = (Pubkey::new_unique(), 7);
        let now = 1_000_u64;

        // attempts cap
        fallbacks.insert(
            key,
            PerpFillFallback {
                recorded_ms: now,
                attempts: 0,
            },
        );
        for _ in 0..PERP_FILL_FALLBACK_MAX_ATTEMPTS {
            assert!(peek_perp_fill_fallback(
                &fallbacks, key, true, 100, 100, now
            ));
        }
        assert!(!peek_perp_fill_fallback(
            &fallbacks, key, true, 100, 100, now
        ));
        assert!(!fallbacks.contains_key(&key), "capped marker is dropped");

        // wall-clock expiry
        fallbacks.insert(
            key,
            PerpFillFallback {
                recorded_ms: now,
                attempts: 0,
            },
        );
        assert!(!peek_perp_fill_fallback(
            &fallbacks,
            key,
            true,
            100,
            100,
            now + PERP_FILL_FALLBACK_EXPIRY_MS + 1,
        ));
        assert!(!fallbacks.contains_key(&key), "expired marker is dropped");
    }

    #[test]
    fn oracle_policy_uses_takeover_when_matching_is_not_allowed() {
        let policy = PrimaryLiquidationStrategy::route_policy_from_validities(
            OracleValidity::TooUncertain,
            OracleValidity::TooUncertain,
            false,
        )
        .unwrap();

        assert!(!policy.safe_match_allowed);
        assert!(!policy.exchange_match_allowed);
        assert!(policy.liquidation_allowed);
        assert_eq!(
            PrimaryLiquidationStrategy::decide_perp_method(
                100,
                100,
                true,
                policy.exchange_match_allowed,
                policy.liquidation_allowed,
                false,
            ),
            LiquidationType::PerpTakeover
        );
    }

    #[test]
    fn oracle_policy_skips_when_liquidation_is_not_allowed() {
        // liquidation eligibility reads the safe/MM oracle, the view
        // liquidate_perp validates onchain
        for validity in [OracleValidity::NonPositive, OracleValidity::TooVolatile] {
            let policy = PrimaryLiquidationStrategy::route_policy_from_validities(
                OracleValidity::Valid,
                validity,
                false,
            )
            .unwrap();

            assert!(!policy.liquidation_allowed);
            assert_eq!(
                PrimaryLiquidationStrategy::decide_perp_method(
                    100,
                    100,
                    false,
                    policy.safe_match_allowed,
                    policy.liquidation_allowed,
                    false,
                ),
                LiquidationType::Skip
            );
        }
    }

    #[test]
    fn takeover_eligibility_follows_safe_validity_not_exchange() {
        // a fresh MM price can keep the safe oracle valid while the raw
        // exchange oracle is not; the program accepts the takeover in that
        // state, so the keeper must not skip it
        let policy = PrimaryLiquidationStrategy::route_policy_from_validities(
            OracleValidity::TooVolatile,
            OracleValidity::Valid,
            false,
        )
        .unwrap();
        assert!(policy.liquidation_allowed);
        assert!(!policy.exchange_match_allowed);

        // the reverse disagreement is rejected onchain, so it must skip
        let policy = PrimaryLiquidationStrategy::route_policy_from_validities(
            OracleValidity::Valid,
            OracleValidity::TooVolatile,
            false,
        )
        .unwrap();
        assert!(!policy.liquidation_allowed);
    }

    #[test]
    fn invalid_exchange_oracle_excludes_floored_match_participants() {
        let policy = PrimaryLiquidationStrategy::route_policy_from_validities(
            OracleValidity::TooUncertain,
            OracleValidity::Valid,
            false,
        )
        .unwrap();
        let mut liquidatee = User::default();
        let mut floored_maker = User::default();
        floored_maker.equity_floor = 1;
        let regular_maker = User::default();

        liquidatee.equity_floor = 1;
        assert!(!PrimaryLiquidationStrategy::match_participation_allowed(
            &liquidatee,
            policy
        ));

        liquidatee.equity_floor = 0;
        assert!(PrimaryLiquidationStrategy::match_participation_allowed(
            &liquidatee,
            policy
        ));
        assert!(!PrimaryLiquidationStrategy::maker_matchable(
            &floored_maker,
            policy.exchange_match_allowed
        ));
        assert!(PrimaryLiquidationStrategy::maker_matchable(
            &regular_maker,
            policy.exchange_match_allowed
        ));
    }

    #[test]
    fn liquidation_makers_come_from_the_opposite_book_side() {
        // the liquidation order is the position's opposite
        // (`get_liquidation_order_params` uses `existing_direction.opposite()`):
        // closing a long places a short taker order, which fills against bids
        assert!(PrimaryLiquidationStrategy::liquidation_makers_are_bids(
            1_000
        ));
        assert!(!PrimaryLiquidationStrategy::liquidation_makers_are_bids(
            -1_000
        ));
    }

    #[test]
    fn pnl_only_requires_no_open_perp_positions() {
        // regression: a settled-pnl-only market used to shadow an open perp
        // position, routing the user to SettlePnl and never liquidating them
        let pnl_only = perp_info(0, true, 0, 500);
        let open_position = perp_info(1, false, 1_000_000, -500);

        assert!(has_settleable_pnl_only(&[pnl_only.clone()], &[]));
        assert!(!has_settleable_pnl_only(
            &[pnl_only.clone(), open_position],
            &[]
        ));
        // spot liability also disqualifies
        assert!(!has_settleable_pnl_only(
            &[pnl_only.clone()],
            &[spot_info(1, false)]
        ));
        // spot deposits are fine
        assert!(has_settleable_pnl_only(&[pnl_only], &[spot_info(1, true)]));
        // no settleable pnl at all
        assert!(!has_settleable_pnl_only(&[], &[]));
    }

    #[test]
    fn largest_position_selected_by_absolute_quote() {
        // regression: selection used signed quote_asset_amount, so long positions
        // (negative quote = cost basis) always lost to any short
        let mut user = User::default();
        user.perp_positions[0].market_index = 0;
        user.perp_positions[0].base_asset_amount = 1_000;
        user.perp_positions[0].quote_asset_amount = 500; // small short
        user.perp_positions[1].market_index = 1;
        user.perp_positions[1].base_asset_amount = -100_000;
        user.perp_positions[1].quote_asset_amount = -50_000; // large long

        let selected = user
            .perp_positions
            .iter()
            .filter(|p| p.base_asset_amount != 0)
            .max_by_key(|p| p.quote_asset_amount.unsigned_abs())
            .unwrap();
        assert_eq!(selected.market_index, 1);
    }

    #[test]
    fn pyth_updates_are_carried_per_market() {
        use pyth_lazer_protocol::router::TimestampUs;

        let mut user = User::default();
        user.perp_positions[0].market_index = 0;
        user.perp_positions[0].base_asset_amount = 1_000;
        user.perp_positions[0].quote_asset_amount = 500;
        user.perp_positions[1].market_index = 1;
        user.perp_positions[1].base_asset_amount = -100_000;
        user.perp_positions[1].quote_asset_amount = -50_000;

        let now_us = current_time_millis() * 1_000;
        let mut prices = BTreeMap::new();
        for market_id in [0u16, 1, 9] {
            prices.insert(
                market_id,
                PythPriceUpdate {
                    market_type: MarketType::Perp,
                    market_id,
                    feed_id: market_id as u32,
                    price: 42,
                    message: vec![],
                    ts: TimestampUs(now_us),
                },
            );
        }

        // one update per held market, so each selected position (isolated
        // ones included) ships its own market's price; markets the user does
        // not hold are not carried
        let updates = fresh_pyth_updates_for_user(&user, &prices);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates.get(&0).unwrap().market_id, 0);
        assert_eq!(updates.get(&1).unwrap().market_id, 1);

        // a stale market drops out individually, the fresh one stays
        let mut stale = prices.get(&1).unwrap().clone();
        stale.ts = TimestampUs(now_us - (MAX_PYTH_AGE_MS + 1_000) * 1_000);
        prices.insert(1, stale);
        let updates = fresh_pyth_updates_for_user(&user, &prices);
        assert_eq!(updates.len(), 1);
        assert!(updates.contains_key(&0));
    }

    #[test]
    fn find_max_liq_amount_respects_available_collateral() {
        // identity impact: liquidating N costs N collateral
        let impact = |size: u128| size as i128;
        let best = PrimaryLiquidationStrategy::find_max_liq_amount(1_000, 100, 10, impact);
        assert!(best <= 100, "must not exceed available collateral");
        assert!(
            best >= 90,
            "should use available collateral up to tolerance"
        );

        // nothing affordable
        assert_eq!(
            PrimaryLiquidationStrategy::find_max_liq_amount(1_000, -5, 10, impact),
            0
        );
    }

    #[test]
    fn attempt_tracker_backoff_and_reset() {
        let mut tracker = LiquidationAttemptTracker::new(100);
        assert_eq!(tracker.cooldown_ms(), 0);
        assert!(tracker.is_cooled_down(current_time_millis()));

        tracker.record_attempt(105);
        assert_eq!(tracker.consecutive_failures, 1);
        assert_eq!(tracker.cooldown_ms(), FAILURE_COOLDOWN_BASE_MS);
        assert!(!tracker.is_cooled_down(current_time_millis()));

        tracker.record_attempt(110);
        assert_eq!(tracker.cooldown_ms(), FAILURE_COOLDOWN_BASE_MS * 2);

        // many failures cap at the max cooldown
        for slot in 0..20 {
            tracker.record_attempt(slot);
        }
        assert_eq!(tracker.cooldown_ms(), FAILURE_COOLDOWN_MAX_MS);

        // a sent tx resets the counter so backoff stops
        tracker.reset();
        assert_eq!(tracker.cooldown_ms(), 0);
        assert!(tracker.is_cooled_down(current_time_millis()));
    }

    #[test]
    fn stale_in_flight_tx_refunds_reserved_collateral() {
        // regression: reserve deducted collateral_required but the refund credited
        // the recorded map value; they must be the same amount or free collateral
        // drifts on every stale tx
        let sig = Signature::default();
        let subaccount = Pubkey::new_unique();

        let tx_sig_to_collateral: Arc<DashMap<Signature, (u128, u64)>> = Arc::new(DashMap::new());
        // recorded 2 minutes ago -> stale
        tx_sig_to_collateral.insert(sig, (700, current_time_millis() - 120_000));

        let txs_in_flight: Arc<DashMap<Pubkey, HashSet<Signature>>> = Arc::new(DashMap::new());
        txs_in_flight.insert(subaccount, HashSet::from([sig]));

        let free: Arc<DashMap<Pubkey, u128>> = Arc::new(DashMap::new());
        free.insert(subaccount, 300);

        clean_stale_in_flight_txs(
            Arc::clone(&tx_sig_to_collateral),
            Arc::clone(&txs_in_flight),
            Arc::clone(&free),
        );

        assert_eq!(*free.get(&subaccount).unwrap(), 1_000);
        assert!(tx_sig_to_collateral.is_empty());
        assert!(txs_in_flight.get(&subaccount).unwrap().is_empty());
    }
}

/// The dlob-server `/userOrders` row, narrowed to the fields a force-cancel
/// needs: the book node the order rests at, its book id, and its side. The
/// numeric fields arrive as JSON numbers or strings, so they stay untyped here.
#[derive(serde::Deserialize)]
struct UserClobOrderRow {
    #[serde(rename = "nodeIndex")]
    node_index: serde_json::Value,
    #[serde(rename = "clobOrderId")]
    clob_order_id: serde_json::Value,
    direction: String,
}

#[derive(serde::Deserialize)]
struct UserOrdersResponse {
    orders: Vec<UserClobOrderRow>,
}

/// Read a JSON value that may be a number or a decimal string into a `u64`.
fn json_u64(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

/// Read a liquidatee's resting CLOB orders in one market from the dlob-server
/// feed and turn them into force-cancel refs. The book id is the CLOB's own
/// `clobOrderId`, not velocity's client order id.
async fn fetch_clob_force_cancel_refs(
    dlob_url: &str,
    liquidatee: &Pubkey,
    market_index: u16,
) -> reqwest::Result<Vec<ForceCancelClobRefV0>> {
    let url = format!(
        "{}/userOrders?userPubkey={}&marketIndexes={}",
        dlob_url.trim_end_matches('/'),
        liquidatee,
        market_index,
    );
    let response: UserOrdersResponse = reqwest::get(&url).await?.json().await?;
    Ok(response
        .orders
        .iter()
        .filter_map(|row| {
            let node_index = u32::try_from(json_u64(&row.node_index)?).ok()?;
            let order_id = json_u64(&row.clob_order_id)?;
            let side = if row.direction == "long" {
                ClobSide::Bid
            } else {
                ClobSide::Ask
            };
            Some(ForceCancelClobRefV0 {
                order_ref: ClobOrderRefV0 {
                    node_index,
                    order_id,
                },
                side,
            })
        })
        .collect())
}

/// Resolve a liquidatee's resting CLOB orders in `market_index` into a
/// force-cancel: the order refs to cancel and the book accounts to cancel them
/// with. `Ok(None)` means the account has no CLOB orders to clear, so a plain
/// liquidation runs. `Err(())` means the account may hold book orders but the
/// feed was unreachable. A perp liquidation then reverts, so the caller skips
/// the attempt and retries later.
async fn resolve_clob_force_cancel(
    velocity: &VelocityClient,
    dlob_url: &str,
    liquidatee: Pubkey,
    liquidatee_user: &User,
    market_index: u16,
) -> Result<Option<(Vec<ForceCancelClobRefV0>, ClobFillAccounts)>, ()> {
    // Cheap gate: an account with no resting perp exposure in the market has no
    // resting CLOB orders there. Skip the feed round trip in the common case.
    let has_resting_exposure = liquidatee_user.perp_positions.iter().any(|position| {
        position.market_index == market_index
            && (position.open_bids != 0 || position.open_asks != 0)
    });
    if !has_resting_exposure {
        return Ok(None);
    }

    let order_refs = fetch_clob_force_cancel_refs(dlob_url, &liquidatee, market_index)
        .await
        .map_err(|error| {
            log::warn!(
                target: TARGET,
                "userOrders fetch for {liquidatee} market {market_index} failed: {error}; \
                 skip liquidation attempt (perp liquidation reverts on resting CLOB orders)"
            );
        })?;
    if order_refs.is_empty() {
        return Ok(None);
    }

    // Book accounts for the force-cancel, read from the market's CLOB entry.
    let clob_quoter = velocity
        .try_get_perp_market_account(market_index)
        .map_err(|error| {
            log::warn!(target: TARGET, "perp market {market_index} unavailable for force-cancel: {error}");
        })?
        .clob_quoter;
    let entry = velocity
        .get_account_value::<QuoterV0>(&clob_quoter)
        .await
        .map_err(|error| {
            log::warn!(target: TARGET, "quoter {clob_quoter} unavailable for force-cancel: {error}");
        })?;
    let clob_fill = ClobFillAccounts {
        market_index,
        quoter: clob_quoter,
        clob_market: entry.response_account,
        clob_program: entry.program_id,
        clob_authority: derive_clob_authority(),
        crank_conditions: Some(derive_clob_crank_conditions(market_index)),
    };
    Ok(Some((order_refs, clob_fill)))
}
