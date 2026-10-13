//! The liquidation worker and the liquidator's periodic tasks
//!
//! The run loop queues each liquidatable user as a `LiquidationRequest`. The worker drops
//! requests older than `MAX_LIQUIDATION_AGE_MS`, rate limits each user by slot, backs off
//! exponentially on users whose attempts keep skipping, and runs each remaining request on its
//! own task under `LIQUIDATION_DEADLINE_MS`. Separate loops close the liquidator's own
//! positions every 30s and reconcile collateral reservations whose outcome never arrived.

use {
    crate::{
        common::{
            collateral::CollateralBook,
            keeper::unix_now_ms,
            metrics::UserMarginStatus,
            oracle::PythPriceUpdate,
            tx::{TxIntent, TxSender},
        },
        liquidator::{execute::LiquidationEngine, LiquidationOutcome, TARGET},
    },
    dashmap::DashMap,
    solana_account_decoder_client_types::UiAccountEncoding,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::config::RpcAccountInfoConfig,
    solana_signature::Signature,
    std::{
        collections::BTreeMap,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
        time::Duration,
    },
    velocity_rs::{
        math::liquidation::calculate_collateral,
        priority_fee_subscriber::PriorityFeeSubscriber,
        program::math::time::{Millis, SlotDuration},
        types::{
            accounts::User, solana_sdk::message::Hash, CommitmentConfig, MarginRequirementType,
            MarketType, OrderParams, OrderType, PositionDirection,
        },
        Pubkey, VelocityClient,
    },
};

/// Min wall-clock time between successive liquidation attempts on the same user, applied in
/// slots at the live slot duration.
const LIQUIDATION_RATE_LIMIT: Millis = Millis::from_secs(2);
/// Maximum time allowed for a liquidation attempt in milliseconds.
const LIQUIDATION_DEADLINE_MS: u64 = 1_000;
/// How often reservations without a delivered outcome are reconciled.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);
/// A request older than this when the worker reaches it is dropped, which sheds backpressure.
const MAX_LIQUIDATION_AGE_MS: u64 = 1_000;
/// Base cooldown in milliseconds after a skipped liquidation, doubling with each one.
const FAILURE_COOLDOWN_BASE_MS: u64 = 5_000;
/// Cap on the exponential cooldown: 5 minutes.
const FAILURE_COOLDOWN_MAX_MS: u64 = 300_000;
const TRACKER_CLEANUP_INTERVAL_MS: u64 = 60_000;
/// A tracker entry for a user not attempted this long ago is dropped.
const TRACKER_ENTRY_MAX_AGE_MS: u64 = 600_000;

/// A liquidatable user, queued by the run loop for the worker.
pub(super) struct LiquidationRequest {
    /// liquidatee subaccount
    pub pubkey: Pubkey,
    /// liquidatee account snapshot at detection time
    pub user: User,
    /// slot the liquidatable status was observed at
    pub slot: u64,
    /// wall-clock time the request was enqueued (ms)
    pub timestamp_ms: u64,
    /// fresh pyth prices for the user's perp markets, keyed by market index
    pub pyth_price_updates: BTreeMap<u16, PythPriceUpdate>,
    pub status: UserMarginStatus,
}

/// Per-user liquidation attempt history for backoff.
#[derive(Clone, Debug)]
pub(super) struct LiquidationAttemptTracker {
    /// Number of consecutive skipped attempts
    pub consecutive_failures: u32,
    pub last_attempt_ms: u64,
    pub last_attempt_slot: u64,
}

impl LiquidationAttemptTracker {
    pub fn new(slot: u64) -> Self {
        Self {
            consecutive_failures: 0,
            last_attempt_ms: unix_now_ms(),
            last_attempt_slot: slot,
        }
    }

    /// The cooldown in ms after the consecutive failures so far.
    pub fn cooldown_ms(&self) -> u64 {
        if self.consecutive_failures == 0 {
            return 0;
        }
        let cooldown = FAILURE_COOLDOWN_BASE_MS * (1u64 << (self.consecutive_failures - 1).min(10));
        cooldown.min(FAILURE_COOLDOWN_MAX_MS)
    }

    pub fn is_cooled_down(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_attempt_ms) >= self.cooldown_ms()
    }

    pub fn record_attempt(&mut self, slot: u64) {
        self.last_attempt_ms = unix_now_ms();
        self.last_attempt_slot = slot;
        self.consecutive_failures += 1;
    }

    pub fn reset(&mut self) {
        self.consecutive_failures = 0;
    }
}

/// Queue a liquidatable user for the worker without blocking. A full queue drops the request.
pub(super) fn send_liquidation(
    liquidations: &tokio::sync::mpsc::Sender<LiquidationRequest>,
    request: LiquidationRequest,
) {
    let pubkey = request.pubkey;
    match liquidations.try_send(request) {
        Ok(()) => {}
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            log::warn!(target: TARGET, "liquidation channel full, dropping liquidation for {pubkey:?}");
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            log::error!(target: TARGET, "liquidation channel closed");
        }
    }
}

/// Run queued liquidations, each on its own task.
///
/// `slot_duration_ms` is the live slot duration, kept current by the run loop, so the rate limit
/// re-paces on a slot duration switch without a restart.
pub(super) fn spawn_liquidation_worker(
    engine: Arc<LiquidationEngine>,
    mut requests: tokio::sync::mpsc::Receiver<LiquidationRequest>,
    cu_limit: u32,
    priority_fees: Arc<PriorityFeeSubscriber>,
    slot_duration_ms: Arc<AtomicU64>,
) {
    let trackers = Arc::new(DashMap::<Pubkey, LiquidationAttemptTracker>::new());

    tokio::spawn(async move {
        let mut last_tracker_cleanup_ms = unix_now_ms();

        while let Some(request) = requests.recv().await {
            let liquidatee = request.pubkey;
            let slot = request.slot;
            let rate_limit_slots = LIQUIDATION_RATE_LIMIT.to_slots(SlotDuration::from_state_ms(
                slot_duration_ms.load(Ordering::Relaxed) as u16,
            ));

            let now = unix_now_ms();
            if now.saturating_sub(request.timestamp_ms) > MAX_LIQUIDATION_AGE_MS {
                continue;
            }

            if now.saturating_sub(last_tracker_cleanup_ms) > TRACKER_CLEANUP_INTERVAL_MS {
                trackers.retain(|_, tracker| {
                    now.saturating_sub(tracker.last_attempt_ms) < TRACKER_ENTRY_MAX_AGE_MS
                });
                last_tracker_cleanup_ms = now;
            }

            if let Some(tracker) = trackers.get(&liquidatee) {
                if slot.abs_diff(tracker.last_attempt_slot) < rate_limit_slots {
                    log::debug!(target: TARGET, "rate limited liquidation for {liquidatee:?} (current: {slot})");
                    continue;
                }
                if !tracker.is_cooled_down(now) {
                    let remaining_ms = tracker
                        .cooldown_ms()
                        .saturating_sub(now.saturating_sub(tracker.last_attempt_ms));
                    log::debug!(
                        target: TARGET,
                        "backoff: skipping {liquidatee:?} (failures={}, cooldown={}ms, remaining={remaining_ms}ms)",
                        tracker.consecutive_failures,
                        tracker.cooldown_ms(),
                    );
                    engine.keeper.metrics.liquidation_backoff_skips.inc();
                    continue;
                }
            }

            // count the attempt as a failure up front; a sent tx resets it
            let attempt = {
                let tracker = trackers
                    .entry(liquidatee)
                    .and_modify(|tracker| tracker.record_attempt(slot))
                    .or_insert_with(|| LiquidationAttemptTracker::new(slot));
                tracker.consecutive_failures
            };
            if attempt > 1 {
                log::info!(target: TARGET, "retrying liquidation for {liquidatee:?} (attempt #{attempt})");
            }

            let priority_fee = priority_fees.priority_fee_nth(0.6);
            let engine = Arc::clone(&engine);
            let trackers = Arc::clone(&trackers);
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                let result = tokio::time::timeout(
                    Duration::from_millis(LIQUIDATION_DEADLINE_MS),
                    engine.liquidate(&request, priority_fee, cu_limit),
                )
                .await;
                let elapsed_ms = started.elapsed().as_millis();
                let metrics = &engine.keeper.metrics;

                match result {
                    Ok(LiquidationOutcome::TxSent) => {
                        // A sent tx resets the failure count, so an in-flight tx is not punished
                        // with backoff. If it fails onchain, the account stays liquidatable and
                        // is retried from zero.
                        if let Some(mut tracker) = trackers.get_mut(&liquidatee) {
                            tracker.reset();
                        }
                    }
                    Ok(LiquidationOutcome::Skipped(reason)) => {
                        // a skip keeps the failure count, so backoff applies
                        log::info!(
                            target: TARGET,
                            "liquidation skipped: liquidatee={liquidatee:?} reason={reason} attempt={attempt} elapsed_ms={elapsed_ms} slot={slot}",
                        );
                        metrics
                            .liquidation_skipped
                            .with_label_values(&[reason])
                            .inc();
                    }
                    Err(_) => {
                        // a timeout keeps the failure count
                        log::warn!(
                            target: TARGET,
                            "liquidation timed out: liquidatee={liquidatee:?} deadline_ms={LIQUIDATION_DEADLINE_MS} attempt={attempt} slot={slot}",
                        );
                        metrics
                            .liquidation_skipped
                            .with_label_values(&["timeout"])
                            .inc();
                    }
                }
            });
        }
    });
}

/// Resolve the collateral reservations whose outcome the tx worker did not deliver, every
/// `RECONCILE_INTERVAL`. Nothing is released on time alone:
///
/// - A reservation with no outcome after `RESERVATION_OUTCOME_TIMEOUT_MS` is decided by
///   [`resolve`] from its tx's status.
/// - A settled reservation whose subaccount has no snapshot from after the confirmation gets an
///   RPC read of the account at a later slot (`min_context_slot`). At confirmed commitment that
///   read includes the tx under the usual assumption that a confirmed block is not rolled back;
///   `min_context_slot` bounds the slot, not the fork.
pub(super) fn spawn_collateral_reconciler(velocity: &'static VelocityClient, book: CollateralBook) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
        loop {
            interval.tick().await;
            reconcile_unresolved(velocity, &book).await;
            reconcile_unreflected(velocity, &book).await;
        }
    });
}

async fn reconcile_unresolved(velocity: &VelocityClient, book: &CollateralBook) {
    let rpc = velocity.rpc();
    for (id, signature, blockhash) in book.unresolved() {
        match resolve(rpc.as_ref(), signature, blockhash).await {
            Resolution::Settle(slot) => book.settle(id, slot),
            Resolution::Release => {
                log::info!(target: TARGET, "released collateral of tx {signature}: it failed or never landed");
                book.release(id);
            }
            Resolution::Keep => {}
        }
    }
}

/// A signature's status at confirmed commitment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SignatureStatus {
    /// The RPC has no status for it.
    Absent,
    /// Seen, but not confirmed yet.
    Unconfirmed,
    Succeeded {
        slot: u64,
    },
    Failed,
}

/// The RPC reads that settle an unresolved reservation. An `Err` is an RPC failure, which
/// proves nothing about the tx.
pub(super) trait TxOutcomeSource {
    /// `search_history` also searches the ledger history, not only the recent status cache.
    async fn signature_status(
        &self,
        signature: Signature,
        search_history: bool,
    ) -> Result<SignatureStatus, ()>;

    async fn blockhash_expired(&self, blockhash: Hash) -> Result<bool, ()>;
}

impl TxOutcomeSource for RpcClient {
    async fn signature_status(
        &self,
        signature: Signature,
        search_history: bool,
    ) -> Result<SignatureStatus, ()> {
        let response = if search_history {
            self.get_signature_statuses_with_history(&[signature]).await
        } else {
            self.get_signature_statuses(&[signature]).await
        };
        let status = response.map_err(|_| ())?.value.into_iter().next().flatten();
        Ok(match status {
            None => SignatureStatus::Absent,
            Some(status) if !status.satisfies_commitment(CommitmentConfig::confirmed()) => {
                SignatureStatus::Unconfirmed
            }
            Some(status) if status.err.is_some() => SignatureStatus::Failed,
            Some(status) => SignatureStatus::Succeeded { slot: status.slot },
        })
    }

    async fn blockhash_expired(&self, blockhash: Hash) -> Result<bool, ()> {
        self.is_blockhash_valid(&blockhash, CommitmentConfig::confirmed())
            .await
            .map(|valid| !valid)
            .map_err(|_| ())
    }
}

/// What to do with an unresolved reservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Resolution {
    /// The tx confirmed at this slot.
    Settle(u64),
    /// The tx failed, or provably never landed.
    Release,
    /// The outcome is still unknown.
    Keep,
}

/// Decide an unresolved reservation from its tx's status. Only evidence releases collateral:
/// a confirmed failure, or no status anywhere in the ledger history once the blockhash has
/// expired. Absence from the recent status cache proves nothing, since a landed tx drops out of
/// it, and every RPC error keeps the reservation for the next round.
pub(super) async fn resolve<S: TxOutcomeSource>(
    source: &S,
    signature: Signature,
    blockhash: Hash,
) -> Resolution {
    match source.signature_status(signature, false).await {
        Ok(SignatureStatus::Succeeded { slot }) => return Resolution::Settle(slot),
        Ok(SignatureStatus::Failed) => return Resolution::Release,
        Ok(SignatureStatus::Unconfirmed) | Err(()) => return Resolution::Keep,
        Ok(SignatureStatus::Absent) => {}
    }
    // until the blockhash expires the tx can still land
    if source.blockhash_expired(blockhash).await != Ok(true) {
        return Resolution::Keep;
    }
    // it can no longer land, so the full history says whether it already did
    match source.signature_status(signature, true).await {
        Ok(SignatureStatus::Succeeded { slot }) => Resolution::Settle(slot),
        Ok(SignatureStatus::Failed | SignatureStatus::Absent) => Resolution::Release,
        Ok(SignatureStatus::Unconfirmed) | Err(()) => Resolution::Keep,
    }
}

async fn reconcile_unreflected(velocity: &VelocityClient, book: &CollateralBook) {
    for (subaccount, confirmed_slot) in book.awaiting_snapshot() {
        let config = RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64Zstd),
            commitment: Some(CommitmentConfig::confirmed()),
            // the account as of a later slot than the confirmation includes the tx
            min_context_slot: Some(confirmed_slot + 1),
            ..Default::default()
        };
        let response = match velocity
            .rpc()
            .get_ui_account_with_config(&subaccount, config)
            .await
        {
            Ok(response) => response,
            Err(err) => {
                log::debug!(target: TARGET, "collateral reconcile read failed for {subaccount}: {err}");
                continue;
            }
        };
        let Some(account) = response.value.and_then(|ui| ui.to_account()) else {
            continue;
        };
        let Some(user) = velocity_rs::utils::try_deser_zero_copy::<User>(&account.data) else {
            continue;
        };
        match calculate_collateral(velocity, &user, MarginRequirementType::Maintenance) {
            Ok(info) => book.observe(subaccount, info.free, response.context.slot),
            Err(err) => {
                log::warn!(target: TARGET, "collateral reconcile failed for {subaccount}: {err:?}");
            }
        }
    }
}

/// Close the liquidator subaccounts' own perp positions every 30s: a reduce-only market order
/// for an open position, a pnl settle for a closed one.
pub(super) fn spawn_derisk_loop(
    velocity: &'static VelocityClient,
    tx: TxSender,
    subaccounts: Vec<Pubkey>,
    priority_fees: Arc<PriorityFeeSubscriber>,
    cu_limit: u32,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            let priority_fee = priority_fees.priority_fee_nth(0.6);
            for subaccount in &subaccounts {
                derisk_subaccount(velocity, &tx, *subaccount, priority_fee, cu_limit).await;
            }
        }
    });
}

async fn derisk_subaccount(
    velocity: &VelocityClient,
    tx: &TxSender,
    subaccount: Pubkey,
    priority_fee: u64,
    cu_limit: u32,
) {
    let Ok(user) = velocity.try_get_account::<User>(&subaccount) else {
        return;
    };

    for position in user.perp_positions.iter() {
        if position.base_asset_amount == 0 && position.quote_asset_amount == 0 {
            continue;
        }

        let tx_builder = velocity_rs::TransactionBuilder::new(
            velocity.program_data(),
            subaccount,
            std::borrow::Cow::Owned(user),
            false,
        )
        .with_priority_fee(priority_fee, Some(cu_limit));

        let (tx_builder, intent) = if position.base_asset_amount != 0 {
            let direction = if position.base_asset_amount > 0 {
                PositionDirection::Short
            } else {
                PositionDirection::Long
            };
            (
                tx_builder.place_orders(vec![OrderParams {
                    order_type: OrderType::Market,
                    market_type: MarketType::Perp,
                    direction,
                    base_asset_amount: position.base_asset_amount.unsigned_abs(),
                    market_index: position.market_index,
                    reduce_only: true,
                    max_ts: Some((unix_now_ms() / 1000 + 15) as i64),
                    ..Default::default()
                }]),
                TxIntent::Derisk {
                    market_index: position.market_index,
                    subaccount,
                },
            )
        } else {
            (
                tx_builder.settle_pnl(position.market_index, None, None),
                TxIntent::SettlePnl {
                    market_index: position.market_index,
                    subaccount,
                },
            )
        };

        tx.send_tx(tx_builder.build(), intent, cu_limit as u64)
            .await;
    }

    // spot positions are not derisked yet: they would swap back to USDC through Jupiter or Titan
}

#[cfg(test)]
mod tests {
    use {
        super::{LiquidationAttemptTracker, FAILURE_COOLDOWN_BASE_MS, FAILURE_COOLDOWN_MAX_MS},
        crate::common::keeper::unix_now_ms,
    };

    #[test]
    fn attempt_tracker_backoff_and_reset() {
        let mut tracker = LiquidationAttemptTracker::new(100);
        assert_eq!(tracker.cooldown_ms(), 0);
        assert!(tracker.is_cooled_down(unix_now_ms()));

        tracker.record_attempt(105);
        assert_eq!(tracker.consecutive_failures, 1);
        assert_eq!(tracker.cooldown_ms(), FAILURE_COOLDOWN_BASE_MS);
        assert!(!tracker.is_cooled_down(unix_now_ms()));

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
        assert!(tracker.is_cooled_down(unix_now_ms()));
    }

    struct FakeRpc {
        recent: Result<super::SignatureStatus, ()>,
        expired: Result<bool, ()>,
        history: Result<super::SignatureStatus, ()>,
    }

    impl super::TxOutcomeSource for FakeRpc {
        async fn signature_status(
            &self,
            _signature: super::Signature,
            search_history: bool,
        ) -> Result<super::SignatureStatus, ()> {
            if search_history {
                self.history
            } else {
                self.recent
            }
        }

        async fn blockhash_expired(&self, _blockhash: super::Hash) -> Result<bool, ()> {
            self.expired
        }
    }

    #[tokio::test]
    async fn only_evidence_releases_collateral() {
        use super::{resolve, Hash, Resolution, Signature, SignatureStatus::*};

        let case = |recent, expired, history| FakeRpc {
            recent,
            expired,
            history,
        };
        let outcome = |rpc: FakeRpc| async move {
            resolve(&rpc, Signature::default(), Hash::default()).await
        };

        // a confirmed outcome in the recent cache decides it
        assert_eq!(
            outcome(case(Ok(Succeeded { slot: 7 }), Err(()), Err(()))).await,
            Resolution::Settle(7)
        );
        assert_eq!(
            outcome(case(Ok(Failed), Err(()), Err(()))).await,
            Resolution::Release
        );
        // an unconfirmed status or an RPC error keeps it
        assert_eq!(
            outcome(case(Ok(Unconfirmed), Ok(true), Ok(Absent))).await,
            Resolution::Keep
        );
        assert_eq!(
            outcome(case(Err(()), Ok(true), Ok(Absent))).await,
            Resolution::Keep
        );
        // absent from the cache: wait while the tx can still land, or the expiry is unknown
        assert_eq!(
            outcome(case(Ok(Absent), Ok(false), Ok(Absent))).await,
            Resolution::Keep
        );
        assert_eq!(
            outcome(case(Ok(Absent), Err(()), Ok(Absent))).await,
            Resolution::Keep
        );
        // expired: the history decides, and a failed history lookup decides nothing
        assert_eq!(
            outcome(case(Ok(Absent), Ok(true), Err(()))).await,
            Resolution::Keep
        );
        assert_eq!(
            outcome(case(Ok(Absent), Ok(true), Ok(Succeeded { slot: 9 }))).await,
            Resolution::Settle(9)
        );
        assert_eq!(
            outcome(case(Ok(Absent), Ok(true), Ok(Unconfirmed))).await,
            Resolution::Keep
        );
        assert_eq!(
            outcome(case(Ok(Absent), Ok(true), Ok(Absent))).await,
            Resolution::Release
        );
    }
}
