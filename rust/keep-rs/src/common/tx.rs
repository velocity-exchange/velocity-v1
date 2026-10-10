use {
    crate::common::{
        collateral::{CollateralBook, ReservationGuard, ReservationId},
        metrics::Metrics,
    },
    dashmap::DashMap,
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_instruction::error::InstructionError,
    solana_rpc_client_api::config::RpcTransactionConfig,
    solana_signature::Signature,
    solana_transaction::TransactionError,
    solana_transaction_status_client_types::{UiTransactionEncoding, UiTransactionError},
    std::{
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    },
    tokio::{runtime::Handle, sync::RwLock},
    velocity_rs::{
        dlob::{L3Order, MakerCrosses},
        event_subscriber::{parse_velocity_logs, VelocityEvent},
        types::{
            accounts::User, CommitmentConfig, RpcSendTransactionConfig, VersionedMessage,
            VersionedTransaction,
        },
        Pubkey, TransactionBuilder, VelocityClient,
    },
};

/// The tx worker logs under the filler's target, which existing `RUST_LOG` filters select, for the
/// liquidator's txs too.
const TARGET: &str = "filler";

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

            if self.generations[past_idx] == past_g
                && self.slots[past_idx].binary_search(&id).is_ok()
            {
                count += 1;
                if count >= 1 {
                    // Already appeared once, so this would be the second time
                    return false;
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
}

// Variant fields are log context, read through `Debug` (`{intent:?}`), which dead-code analysis
// ignores.
#[derive(Clone, Default, Debug)]
#[allow(dead_code)]
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
    AmmTakerFill {
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
                    "auction_fill_amm"
                } else {
                    "auction_fill"
                }
            }
            TxIntent::SwiftFill { maker_crosses, .. } => {
                if maker_crosses.has_vamm_cross {
                    "swift_fill_amm"
                } else {
                    "swift_fill"
                }
            }
            TxIntent::SwiftPlace { .. } => "swift_place",
            TxIntent::LimitUncross { .. } => "limit_uncross",
            TxIntent::AmmTakerFill { .. } => "amm_taker",
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
            TxIntent::AmmTakerFill { .. } => 1,
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
            Self::AmmTakerFill { slot, .. } => (vec![], *slot),
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
            Self::AmmTakerFill { slot, .. }
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
            | Self::AmmTakerFill { market_index, .. }
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
            Self::AmmTakerFill { maker_order_id, .. } => Some(*maker_order_id),
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
            | Self::AmmTakerFill { taker_user, .. }
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
pub struct PendingTx {
    pub signature: Signature,
    pub intent: TxIntent,
    pub cu_limit: u64,
    /// The collateral the tx holds, settled or released when its outcome arrives.
    pub reservation: Option<ReservationId>,
}

impl PendingTx {
    pub fn new(sig: Signature, intent: TxIntent, cu_limit: u64) -> Self {
        Self {
            signature: sig,
            intent,
            cu_limit,
            reservation: None,
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
    buffer: Box<[PendingTx; N]>,
    head: usize,
    tail: usize,
    size: usize,
}

impl<const N: usize> PendingTxs<N> {
    pub fn new() -> Self {
        Self {
            buffer: Box::new([(); N].map(|_| PendingTx::default())),
            head: 0,
            tail: 0,
            size: 0,
        }
    }

    /// Insert a new item, overwriting the oldest when full.
    ///
    /// Returns the overwritten item when it was still unconfirmed. Its confirmation can no
    /// longer be matched, so the caller has to account for it now.
    pub fn insert(&mut self, item: PendingTx) -> Option<PendingTx> {
        let evicted = if self.size == N {
            let evicted = std::mem::replace(&mut self.buffer[self.tail], item);
            self.head = (self.head + 1) % N;
            (evicted.signature != Signature::default()).then_some(evicted)
        } else {
            self.buffer[self.tail] = item;
            self.size += 1;
            None
        };
        self.tail = (self.tail + 1) % N;
        evicted
    }

    /// Confirm and return the first item with matching signature.
    ///
    /// Returns Some(item) if found, else None. The entry is consumed: a duplicate
    /// confirmation of the same signature (e.g. redelivered by the tx stream) returns
    /// None instead of re-running the confirmation accounting.
    pub fn confirm(&mut self, sig: &Signature) -> Option<PendingTx> {
        for i in 0..self.size {
            let idx = (self.head + i) % N;
            if self.buffer[idx].signature == *sig {
                // leave a default (never-matching) hole; head/size stay untouched
                return Some(std::mem::take(&mut self.buffer[idx]));
            }
        }
        None
    }
}

/// One-shot marker recorded when a liquidate-with-fill tx fails onchain with
/// `LiquidationOrderFailedToFill`, telling the liquidator to route the next
/// attempt on that (liquidatee, market) straight to a collateral takeover.
/// The marker survives until a takeover tx is actually sent; `attempts`
/// counts takeover routings it has driven and `recorded_ms` bounds its
/// lifetime, so a takeover path that keeps failing before send cannot pin
/// the marker forever.
#[derive(Clone, Copy, Debug)]
pub struct TakeoverFallback {
    pub recorded_ms: u64,
    pub attempts: u32,
}

/// Add the interest cranks a fill needs, ahead of the fill instruction.
///
/// The program refuses a fill when the taker, or any maker, carries a borrow in a
/// spot market whose interest has not accrued recently enough
/// (`SpotMarketInterestStaleForMargin`): the margin check values that borrow
/// through a stale index and understates the debt. `fill_perp_order`
/// receives those markets read-only and cannot refresh them, so the permissionless
/// crank rides in the same transaction. Call this before `fill_perp_order`, which
/// also keeps the fill as the last instruction for the account-count check. The
/// liquidator reuses it for its own account, passing no makers.
///
/// A market this misses only costs a reverted fill.
pub(crate) fn with_spot_interest_cranks<'a>(
    mut tx_builder: TransactionBuilder<'a>,
    velocity: &VelocityClient,
    taker: &User,
    makers: &[User],
) -> TransactionBuilder<'a> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default();

    let mut users = Vec::with_capacity(makers.len() + 1);
    users.push(taker);
    users.extend(makers.iter());

    for market_index in velocity.stale_spot_interest_markets(&users, now) {
        tx_builder = tx_builder.update_spot_market_cumulative_interest(market_index);
    }

    tx_builder
}

/// Whether a failed `sendTransaction` call proves the tx was not accepted: the RPC answered
/// with a JSON-RPC error, or the tx was refused before it left. A transport failure (timeout,
/// dropped connection, unreadable response) can happen after the RPC forwarded the tx, so it
/// proves nothing.
fn send_error_is_rejection(err: &solana_rpc_client_api::client_error::Error) -> bool {
    use solana_rpc_client_api::{client_error::ErrorKind, request::RpcError};
    matches!(
        err.kind(),
        ErrorKind::RpcError(RpcError::RpcResponseError { .. })
            | ErrorKind::TransactionError(_)
            | ErrorKind::SigningError(_)
    )
}

/// Return a failed or unsent tx's reserved collateral.
fn release(collateral: &Option<CollateralBook>, reservation: Option<ReservationId>) {
    if let (Some(book), Some(id)) = (collateral, reservation) {
        book.release(id);
    }
}

/// Raise the compute unit limit to `cu_limit * scale_tenths / 10` when the last instruction
/// lists at least `min_accounts` accounts, since each account costs compute to load. Returns the
/// builder and the limit it now sets.
///
/// The limit is instruction 1, after the compute unit price that `with_priority_fee` puts first.
pub(crate) fn scale_cu_limit_for_accounts(
    tx_builder: TransactionBuilder<'_>,
    cu_limit: u32,
    min_accounts: usize,
    scale_tenths: u32,
) -> (TransactionBuilder<'_>, u32) {
    let crowded = tx_builder
        .ixs()
        .last()
        .is_some_and(|ix| ix.accounts.len() >= min_accounts);
    if !crowded {
        return (tx_builder, cu_limit);
    }
    let scaled = cu_limit * scale_tenths / 10;
    let tx_builder = tx_builder.set_ix(1, ComputeBudgetInstruction::set_compute_unit_limit(scaled));
    (tx_builder, scaled)
}

/// Build the broadcast transaction and, when needed, a guarded simulation variant.
///
/// The worker requires a matching nonzero `OrderFill` event from this simulation because
/// `RevertFill` only proves that the filler was active sometime in the current slot; activity from
/// an earlier transaction can otherwise produce a false positive. Ordinary fills strip
/// `RevertFill` after simulation to reduce transaction size and compute. Pyth-update transactions
/// retain it as an additional execution-time rollback guard.
pub(crate) fn build_fill_tx(
    tx_builder: TransactionBuilder<'_>,
    retain_revert_fill: bool,
) -> (VersionedMessage, Option<VersionedMessage>) {
    if retain_revert_fill {
        (tx_builder.revert_fill().build(), None)
    } else {
        let tx = tx_builder.clone().build();
        let simulation_tx = tx_builder.revert_fill().build();
        (tx, Some(simulation_tx))
    }
}

// Sent through the tx worker channel by value; boxing would add an allocation per transaction.
#[allow(clippy::large_enum_variant)]
pub enum TxCommand {
    Send {
        tx: VersionedTransaction,
        simulation_tx: Option<VersionedMessage>,
        require_fill_event: bool,
        intent: TxIntent,
        cu_limit: u64,
        reservation: Option<ReservationId>,
    },
    Confirm {
        tx: Signature,
    },
}

pub struct TxWorker {
    velocity: &'static VelocityClient,
    pending_txs: Arc<RwLock<PendingTxs<1024>>>,
    metrics: Arc<Metrics>,
    dry_run: bool,
    collateral: Option<CollateralBook>,
    perp_fill_fallbacks: Option<Arc<DashMap<(Pubkey, u16), TakeoverFallback>>>,
}

impl TxWorker {
    pub fn new(
        velocity: VelocityClient,
        metrics: Arc<Metrics>,
        dry_run: bool,
        collateral: Option<CollateralBook>,
        perp_fill_fallbacks: Option<Arc<DashMap<(Pubkey, u16), TakeoverFallback>>>,
    ) -> Self {
        Self {
            velocity: Box::leak(Box::new(velocity)),
            pending_txs: Arc::new(RwLock::new(PendingTxs::new())),
            metrics,
            dry_run,
            collateral,
            perp_fill_fallbacks,
        }
    }

    pub fn run(self, rt: tokio::runtime::Handle) -> TxSender {
        let (tx, rx) = crossbeam::channel::bounded(1024);
        let velocity = self.velocity;
        std::thread::spawn(move || {
            let _ = env_logger::try_init();
            while let Ok(work) = rx.recv() {
                match work {
                    TxCommand::Send {
                        tx,
                        simulation_tx,
                        require_fill_event,
                        intent,
                        cu_limit,
                        reservation,
                    } => {
                        if self.dry_run {
                            log::debug!(target: TARGET, "skip tx dry run: {intent:?}");
                            release(&self.collateral, reservation);
                            continue;
                        }
                        self.send_tx(
                            &rt,
                            tx,
                            simulation_tx,
                            require_fill_event,
                            intent,
                            cu_limit,
                            reservation,
                        );
                    }
                    TxCommand::Confirm { tx } => {
                        self.confirm_tx(&rt, tx);
                    }
                }
            }
        });
        TxSender { tx, velocity }
    }

    fn send_tx(
        &self,
        rt: &Handle,
        signed_tx: VersionedTransaction,
        simulation_tx: Option<VersionedMessage>,
        require_fill_event: bool,
        intent: TxIntent,
        cu_limit: u64,
        reservation: Option<ReservationId>,
    ) {
        log::debug!(target: TARGET, "txworker send tx: {intent:?}");
        let velocity = self.velocity;
        let pending_txs = Arc::clone(&self.pending_txs);
        let metrics = self.metrics.clone();
        let perp_fill_fallbacks = self.perp_fill_fallbacks.clone();
        let collateral = self.collateral.clone();
        let intent_label = intent.label();

        metrics.tx_sent.with_label_values(&[intent_label]).inc();
        metrics
            .fill_expected
            .with_label_values(&[intent_label])
            .inc();
        if intent.expected_trigger() {
            metrics.trigger_expected.inc();
        }

        rt.spawn(async move {
            let simulation_tx = simulation_tx.unwrap_or_else(|| signed_tx.message.clone());
            // simulate first, against processed state: the tx was built from the
            // processed-commitment gRPC view, so a default-commitment (finalized)
            // preflight lags ~32 slots and rejects valid fills for the whole
            // finalization window (e.g. `OrderMustBeTriggeredFirst` on fills of a
            // just-triggered order)
            match velocity
                .simulate_tx_with_commitment(
                    simulation_tx,
                    Some(CommitmentConfig::processed()),
                )
                .await
            {
                Ok(sim_result) => {
                    if let Some(err) = sim_result.err {
                        record_takeover_fallback(
                            &intent,
                            &TransactionError::from(err.clone()),
                            perp_fill_fallbacks.as_ref(),
                        );
                        if is_revert_fill_error(&err) {
                            log::debug!(
                                target: TARGET,
                                "fill produced no fills during simulation, intent: {intent_label}"
                            );
                            emit_tx_event(
                                &intent,
                                None,
                                "no_fills",
                                intent.crosses_and_slot().1,
                                None,
                                intent.expected_fill_count(),
                                0,
                                false,
                                cu_limit,
                                None,
                                None,
                                Some("RevertFill"),
                            );
                            release(&collateral, reservation);
                            return;
                        }
                        log::warn!(
                            target: TARGET,
                            "sim failed: {err:?}, intent: {intent_label}, liquidatee: {:?}, slot: {:?}",
                            intent.liquidatee(),
                            intent.slot()
                        );
                        // Log simulation logs for liquidation and uncross intents to help
                        // diagnose failures
                        if intent.is_liquidation()
                            || matches!(intent, TxIntent::LimitUncross { .. })
                        {
                            if let Some(logs) = sim_result.logs {
                                for log_line in &logs {
                                    if log_line.contains("Error") || log_line.contains("error") || log_line.contains("failed") || log_line.contains("Program log:") {
                                        log::warn!(target: TARGET, "  sim log: {}", log_line);
                                    }
                                }
                            }
                        }
                        metrics
                            .tx_failed
                            .with_label_values(&[intent_label, "sim_failed"])
                            .inc();
                        emit_tx_event(
                            &intent,
                            None,
                            "sim_failed",
                            intent.crosses_and_slot().1,
                            None,
                            intent.expected_fill_count(),
                            0,
                            false,
                            cu_limit,
                            None,
                            None,
                            Some(&format!("{err:?}")),
                        );
                        release(&collateral, reservation);
                        return;
                    }

                    if require_fill_event
                        && !simulation_has_expected_fill(sim_result.logs.as_deref(), &intent)
                    {
                        log::debug!(
                            target: TARGET,
                            "fill simulation emitted no matching fill event, intent: {intent_label}"
                        );
                        emit_tx_event(
                            &intent,
                            None,
                            "no_fills",
                            intent.crosses_and_slot().1,
                            None,
                            intent.expected_fill_count(),
                            0,
                            false,
                            cu_limit,
                            None,
                            None,
                            Some("NoFillEvent"),
                        );
                        release(&collateral, reservation);
                        return;
                    }
                }
                Err(err) => {
                    log::warn!(
                        target: TARGET,
                        "sim rpc error: {err}, intent: {intent_label}, liquidatee: {:?}",
                        intent.liquidatee()
                    );
                    metrics
                        .tx_failed
                        .with_label_values(&[intent_label, "sim_rpc_error"])
                        .inc();
                    emit_tx_event(
                        &intent,
                        None,
                        "sim_rpc_error",
                        intent.crosses_and_slot().1,
                        None,
                        intent.expected_fill_count(),
                        0,
                        false,
                        cu_limit,
                        None,
                        None,
                        Some(&format!("{err}")),
                    );
                    release(&collateral, reservation);
                    return;
                }
            }

            let config = RpcSendTransactionConfig {
                skip_preflight: true,
                max_retries: Some(0),
                ..Default::default()
            };

            // Register before broadcasting: the tx can land, and its confirmation stream in,
            // while the send call is still waiting on the RPC.
            let signature = signed_tx.signatures[0];
            let evicted = pending_txs.write().await.insert(PendingTx {
                reservation,
                ..PendingTx::new(signature, intent.clone(), cu_limit)
            });
            if let Some(evicted) = evicted {
                log::warn!(
                    target: TARGET,
                    "pending tx buffer full, dropped unconfirmed tx {} ({})",
                    evicted.signature,
                    evicted.intent.label()
                );
                metrics
                    .tx_failed
                    .with_label_values(&[evicted.intent.label(), "evicted_unconfirmed"])
                    .inc();
            }

            match velocity
                .rpc()
                .send_transaction_with_config(&signed_tx, config)
                .await
            {
                Ok(sig) => {
                    log::info!(
                        target: TARGET,
                        r#"{{"intent": "{}", "txn": "{}", "observed_slot": {}}}"#,
                        intent_label,
                        sig,
                        intent.slot().unwrap_or(0)
                    );
                }
                Err(err) => {
                    let rejected = send_error_is_rejection(&err);
                    log::info!(target: TARGET, "fill failed 🐢 (rejected={rejected}): {err}");
                    metrics
                        .tx_failed
                        .with_label_values(&[intent_label, "send_error"])
                        .inc();
                    emit_tx_event(
                        &intent,
                        None,
                        "send_error",
                        intent.crosses_and_slot().1,
                        None,
                        intent.expected_fill_count(),
                        0,
                        false,
                        cu_limit,
                        None,
                        None,
                        Some(&format!("{err}")),
                    );
                    // A rejection proves the tx never reached the cluster. Any other error
                    // leaves it pending: the RPC may have forwarded it before the transport
                    // failed, and its confirmation, if any, settles it.
                    if rejected {
                        pending_txs.write().await.confirm(&signature);
                        release(&collateral, reservation);
                    }
                }
            }
        });
    }

    fn confirm_tx(&self, rt: &Handle, tx: Signature) {
        // TODO: if CU limit is too low send it again with higher amount
        log::debug!(target: TARGET, "txworker confirm tx: {tx:?}");
        let velocity = self.velocity;
        let pending_txs = Arc::clone(&self.pending_txs);
        let metrics = self.metrics.clone();

        let collateral = self.collateral.clone();
        let perp_fill_fallbacks = self.perp_fill_fallbacks.clone();

        rt.spawn(async move {
            let pending_tx_meta = {
                let mut pending = pending_txs.write().await;
                pending.confirm(&tx)
            };
            if pending_tx_meta.is_none() {
                return;
            }
            let PendingTx {
                signature,
                intent,
                cu_limit: sent_cu_limit,
                reservation,
            } = pending_tx_meta.unwrap();

            let intent_label = intent.label();
            let expected_fill_count = intent.expected_fill_count();
            let (_, sent_slot) = intent.crosses_and_slot();
            let _ = tokio::time::sleep(Duration::from_secs(1)).await;
            match velocity
                .rpc()
                .get_transaction_with_config(
                    &tx,
                    RpcTransactionConfig {
                        encoding: Some(UiTransactionEncoding::Base64),
                        commitment: Some(CommitmentConfig::confirmed()),
                        max_supported_transaction_version: Some(1),
                    },
                )
                .await
            {
                Ok(tx_log) => {
                    if let Some(meta) = tx_log.transaction.meta {
                        match meta.err.map(TransactionError::from) {
                            None => {
                                // tx confirmed ok, so the chain balance reflects any position
                                // it took on
                                if let (Some(book), Some(id)) = (&collateral, reservation) {
                                    book.settle(id, tx_log.slot);
                                }
                                let sig = tx.to_string();
                                let logs = meta.log_messages.unwrap();
                                let tx_confirmed_slot = tx_log.slot;
                                let mut actual_fills = 0;
                                let mut triggered = false;
                                for event in parse_velocity_logs(
                                    logs.iter().map(String::as_str),
                                    &sig,
                                ) {
                                    if let VelocityEvent::OrderFill { .. } = event {
                                        actual_fills += 1;
                                    } else if let VelocityEvent::OrderTrigger { .. } = event {
                                        triggered = true;
                                        metrics.trigger_actual.inc();
                                    }
                                }
                                let confirmation_slots = tx_confirmed_slot - sent_slot;
                                log::debug!(target: TARGET, "txworker: {tx:?} confirmed after {confirmation_slots} slots");
                                metrics
                                    .fill_actual
                                    .with_label_values(&[intent_label])
                                    .inc();
                                metrics
                                    .confirmation_slots
                                    .with_label_values(&[intent_label])
                                    .observe(confirmation_slots as f64);
                                let cu_consumed: Option<u64> =
                                    meta.compute_units_consumed.clone().into();
                                // saturating: the account-count heuristic can raise the tx's
                                // actual CU limit above `sent_cu_limit` recorded at send time
                                let cus_spent = sent_cu_limit.saturating_sub(cu_consumed.unwrap_or(0));
                                metrics
                                    .cu_spent
                                    .with_label_values(&[intent_label])
                                    .observe(cus_spent as f64);

                                // For placement/trigger intents, success is not measured by
                                // fills, so don't mislabel them "no_fills". Detect the program's
                                // silent no-ops (uuid dedup / past placement window for a swift
                                // place; already-triggered for a trigger) so wasted gas is
                                // distinguishable from a real placement/trigger in the events.
                                let status = if expected_fill_count == 0 {
                                    match &intent {
                                        TxIntent::SwiftPlace { .. } => {
                                            if logs.iter().any(|l| l.contains("already exists")) {
                                                "place_noop_dup"
                                            } else if logs.iter().any(|l| l.contains("max_slot")) {
                                                "place_noop_expired"
                                            } else {
                                                "placed"
                                            }
                                        }
                                        TxIntent::Trigger { .. } => {
                                            if triggered {
                                                "triggered"
                                            } else {
                                                "trigger_noop"
                                            }
                                        }
                                        _ => "ok",
                                    }
                                } else if actual_fills == 0 {
                                    "no_fills"
                                } else if actual_fills < expected_fill_count as u64 {
                                    "partial"
                                } else {
                                    "ok"
                                };
                                metrics
                                    .tx_confirmed
                                    .with_label_values(&[intent_label, status])
                                    .inc();

                                emit_tx_event(
                                    &intent,
                                    Some(&sig),
                                    status,
                                    sent_slot,
                                    Some(tx_confirmed_slot),
                                    expected_fill_count,
                                    actual_fills,
                                    triggered,
                                    sent_cu_limit,
                                    cu_consumed,
                                    Some(meta.fee),
                                    None,
                                );

                                match intent {
                                    TxIntent::LiquidateWithFill { .. } => {
                                        metrics.liquidation_success.with_label_values(&["perp"]).inc();
                                    }
                                    TxIntent::LiquidateSpot { .. } => {
                                        metrics.liquidation_success.with_label_values(&["spot"]).inc();
                                    }
                                    _ => {}
                                }
                            }
                            Some(
                                TransactionError::InsufficientFundsForFee
                                | TransactionError::InsufficientFundsForRent { .. },
                            ) => {
                                log::error!(target: TARGET, "bot needs more SOL!");
                                release(&collateral, reservation);
                                metrics
                                    .tx_failed
                                    .with_label_values(&[
                                        intent_label,
                                        "insufficient_funds",
                                    ])
                                    .inc();
                                emit_tx_event(
                                    &intent,
                                    Some(&tx.to_string()),
                                    "insufficient_funds",
                                    sent_slot,
                                    Some(tx_log.slot),
                                    expected_fill_count,
                                    0,
                                    false,
                                    sent_cu_limit,
                                    meta.compute_units_consumed.clone().into(),
                                    Some(meta.fee),
                                    None,
                                );
                            }
                            Some(err) => {
                                record_takeover_fallback(
                                    &intent,
                                    &err,
                                    perp_fill_fallbacks.as_ref(),
                                );
                                log::warn!(
                                    target: TARGET,
                                    "tx failed: {err:?}, intent: {intent_label}, liquidatee: {:?}, sig: {signature}",
                                    intent.liquidatee()
                                );
                                let logs: Option<Vec<String>> = meta.log_messages.clone().into();
                                // Log program logs from failed liquidation txs
                                if intent.is_liquidation() {
                                    if let Some(logs) = logs.as_ref() {
                                        for log_line in logs {
                                            if log_line.contains("Error") || log_line.contains("error") || log_line.contains("failed") || log_line.contains("Program log:") {
                                                log::warn!(target: TARGET, "  tx log: {}", log_line);
                                            }
                                        }
                                    }
                                }
                                // tx failed with error. CU exhaustion lands here: the VM
                                // reports it as ProgramFailedToComplete, which it shares with
                                // other faults, so the log line is what identifies it.
                                let reason = if logs
                                    .as_ref()
                                    .is_some_and(|logs| logs.iter().any(|l| l.contains("exceeded CUs meter")))
                                {
                                    "insufficient_cus".to_string()
                                } else {
                                    format!("{err:?}")
                                };
                                metrics
                                    .tx_failed
                                    .with_label_values(&[intent_label, &reason])
                                    .inc();
                                emit_tx_event(
                                    &intent,
                                    Some(&signature.to_string()),
                                    "failed",
                                    sent_slot,
                                    Some(tx_log.slot),
                                    expected_fill_count,
                                    0,
                                    false,
                                    sent_cu_limit,
                                    meta.compute_units_consumed.clone().into(),
                                    Some(meta.fee),
                                    Some(&format!("{err:?}")),
                                );
                                match intent {
                                    TxIntent::LiquidateWithFill { .. } => {
                                        metrics.liquidation_failed.with_label_values(&["perp"]).inc();
                                    }
                                    TxIntent::LiquidateSpot { .. } => {
                                        metrics.liquidation_failed.with_label_values(&["spot"]).inc();
                                    }
                                    _ => {}
                                }

                                release(&collateral, reservation);
                            }
                        }
                    } else {
                        log::warn!(target: TARGET, "tx metadata missing");
                        metrics
                            .tx_failed
                            .with_label_values(&[intent_label, "metadata_missing"])
                            .inc();
                        emit_tx_event(
                            &intent,
                            Some(&tx.to_string()),
                            "metadata_missing",
                            sent_slot,
                            Some(tx_log.slot),
                            expected_fill_count,
                            0,
                            false,
                            sent_cu_limit,
                            None,
                            None,
                            None,
                        );
                    }
                }
                Err(err) => {
                    log::info!(target: TARGET, "tx confirmation failed 🐢: {err}");
                    metrics
                        .tx_failed
                        .with_label_values(&[intent_label, "confirmation_failed"])
                        .inc();
                    emit_tx_event(
                        &intent,
                        Some(&tx.to_string()),
                        "confirmation_failed",
                        sent_slot,
                        None,
                        expected_fill_count,
                        0,
                        false,
                        sent_cu_limit,
                        None,
                        None,
                        Some(&format!("{err}")),
                    );
                }
            }
        });
    }
}

/// Emit a single wide structured event (one JSON line, log target `tx_event`) capturing the
/// full outcome of a transaction.
///
/// This is the canonical per-tx event: every order placement, fill and trigger the bot sends
/// produces exactly one terminal event here (at confirmation, or at sim/send failure), carrying
/// enough dimensions — intent, market, order id / swift uuid, expected vs actual fills, trigger
/// flag, CU limit/consumed, and the exact `fee_lamports` paid — to attribute gas spend. In
/// particular it makes it possible to measure how much gas the `swift_place` (place-on-chain)
/// path costs versus the fills those placements ultimately yield.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_tx_event(
    intent: &TxIntent,
    sig: Option<&str>,
    status: &str,
    sent_slot: u64,
    confirmed_slot: Option<u64>,
    expected_fills: usize,
    actual_fills: u64,
    triggered: bool,
    cu_limit: u64,
    cu_consumed: Option<u64>,
    fee_lamports: Option<u64>,
    error: Option<&str>,
) {
    let latency_slots = confirmed_slot.map(|c| c.saturating_sub(sent_slot));
    let uuid = intent
        .swift_uuid()
        .map(|u| String::from_utf8_lossy(&u).into_owned());
    let event = serde_json::json!({
        "event": "tx",
        "intent": intent.label(),
        "market": intent.market_index(),
        "order_id": intent.order_id(),
        "user": intent.user().map(|u| u.to_string()),
        "uuid": uuid,
        "sig": sig,
        "status": status,
        "sent_slot": sent_slot,
        "confirmed_slot": confirmed_slot,
        "latency_slots": latency_slots,
        "expected_fills": expected_fills,
        "actual_fills": actual_fills,
        "triggered": triggered,
        "cu_limit": cu_limit,
        "cu_consumed": cu_consumed,
        "fee_lamports": fee_lamports,
        "error": error,
    });
    log::info!(target: "tx_event", "{event}");
}

fn is_revert_fill_error(error: &UiTransactionError) -> bool {
    matches!(
        TransactionError::from(error.clone()),
        TransactionError::InstructionError(_, InstructionError::Custom(6239))
    )
}

fn record_takeover_fallback(
    intent: &TxIntent,
    error: &TransactionError,
    fallbacks: Option<&Arc<DashMap<(Pubkey, u16), TakeoverFallback>>>,
) {
    let TransactionError::InstructionError(_, InstructionError::Custom(code)) = error else {
        return;
    };
    let expected = velocity_rs::program::error::ErrorCode::LiquidationOrderFailedToFill as u32
        + anchor_lang::error::ERROR_CODE_OFFSET;
    if *code != expected {
        return;
    }
    let TxIntent::LiquidateWithFill {
        market_index,
        liquidatee,
        ..
    } = intent
    else {
        return;
    };
    if let Some(fallbacks) = fallbacks {
        fallbacks.insert(
            (*liquidatee, *market_index),
            TakeoverFallback {
                recorded_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                attempts: 0,
            },
        );
        log::info!(
            target: TARGET,
            "recorded perp takeover fallback: liquidatee={liquidatee:?} market={market_index}"
        );
    }
}

fn is_expected_fill_event(event: &VelocityEvent, intent: &TxIntent) -> bool {
    matches!(
        event,
        VelocityEvent::OrderFill {
            taker,
            taker_order_id,
            base_asset_amount_filled,
            market_index,
            ..
        } if *base_asset_amount_filled > 0
            && *taker == intent.user()
            && Some(*taker_order_id) == intent.order_id()
            && Some(*market_index) == intent.market_index()
    )
}

fn simulation_has_expected_fill(logs: Option<&[String]>, intent: &TxIntent) -> bool {
    logs.map(|logs| {
        parse_velocity_logs(logs.iter().map(String::as_str), "simulation")
            .into_iter()
            .any(|event| is_expected_fill_event(&event, intent))
    })
    .unwrap_or(false)
}

#[derive(Clone)]
pub struct TxSender {
    tx: crossbeam::channel::Sender<TxCommand>,
    velocity: &'static VelocityClient,
}

impl TxSender {
    pub fn confirm_tx(&self, tx: Signature) {
        self.tx.send(TxCommand::Confirm { tx }).expect("sent");
    }

    pub async fn send_tx(
        &self,
        tx: VersionedMessage,
        intent: TxIntent,
        cu_limit: u64,
    ) -> Option<Signature> {
        self.enqueue_tx(tx, None, false, intent, cu_limit, None)
            .await
    }

    /// Send a tx that holds `reservation`. The worker settles or releases it with the tx's
    /// outcome. If the tx never reaches the worker, including when this future is dropped, the
    /// guard releases the reservation.
    pub async fn send_reserved_tx(
        &self,
        tx: VersionedMessage,
        intent: TxIntent,
        cu_limit: u64,
        reservation: ReservationGuard,
    ) -> Option<Signature> {
        self.enqueue_tx(tx, None, false, intent, cu_limit, Some(reservation))
            .await
    }

    pub async fn send_fill_tx(
        &self,
        tx: VersionedMessage,
        simulation_tx: Option<VersionedMessage>,
        intent: TxIntent,
        cu_limit: u64,
    ) -> Option<Signature> {
        self.enqueue_tx(tx, simulation_tx, true, intent, cu_limit, None)
            .await
    }

    async fn enqueue_tx(
        &self,
        tx: VersionedMessage,
        simulation_tx: Option<VersionedMessage>,
        require_fill_event: bool,
        intent: TxIntent,
        cu_limit: u64,
        reservation: Option<ReservationGuard>,
    ) -> Option<Signature> {
        // no blockhash = subscription dead AND rpc fallback failed; silently dropping
        // every tx from here would be worse than a restart
        let blockhash = self
            .velocity
            .get_latest_blockhash()
            .await
            .expect("blockhash available; restart");
        let signed_tx = self.velocity.wallet().sign_tx(tx, blockhash).ok()?;
        let sig = signed_tx.signatures[0];
        // the reconciler checks this tx's status and blockhash if no outcome arrives; a
        // reservation that is gone means the tx must not go out without collateral
        if let Some(reservation) = &reservation {
            if !reservation.attach(sig, *signed_tx.message.recent_blockhash()) {
                return None;
            }
        }

        self.tx
            .send(TxCommand::Send {
                tx: signed_tx,
                simulation_tx,
                require_fill_event,
                intent,
                cu_limit,
                reservation: reservation.as_ref().map(ReservationGuard::id),
            })
            .ok()?;
        // the worker owns the outcome from here; until now a dropped guard released it
        let _ = reservation.map(ReservationGuard::hand_off);

        Some(sig)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{
            build_fill_tx, is_expected_fill_event, is_revert_fill_error, record_takeover_fallback,
            OrderSlotLimiter, PendingTx, PendingTxs, Pubkey, TxIntent, VelocityEvent,
        },
        solana_instruction::error::InstructionError,
        solana_signature::Signature,
        solana_transaction::TransactionError,
        std::borrow::Cow,
        velocity_rs::{
            constants::ProgramData,
            types::accounts::{PerpMarket, SpotMarket, State, User},
            velocity_idl::types::MarketType as EventMarketType,
            TransactionBuilder,
        },
    };

    #[test]
    fn pending_txs_confirm_consumes_entry() {
        let mut pending = PendingTxs::<8>::new();
        let sig = Signature::from([7u8; 64]);
        pending.insert(PendingTx::new(
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
    fn pending_txs_reports_an_evicted_unconfirmed_tx() {
        let sig = |byte| Signature::from([byte; 64]);
        let mut pending = PendingTxs::<2>::new();
        assert!(pending
            .insert(PendingTx::new(sig(1), TxIntent::None, 0))
            .is_none());
        assert!(pending
            .insert(PendingTx::new(sig(2), TxIntent::None, 0))
            .is_none());

        // full, so the third insert overwrites the oldest entry, which is still unconfirmed
        let evicted = pending
            .insert(PendingTx::new(sig(3), TxIntent::None, 0))
            .expect("evicts the oldest");
        assert_eq!(evicted.signature, sig(1));

        // a confirmed entry leaves an empty slot, and overwriting it reports nothing
        assert!(pending.confirm(&sig(2)).is_some());
        assert!(pending
            .insert(PendingTx::new(sig(4), TxIntent::None, 0))
            .is_none());
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
    fn order_fill_event(taker: Pubkey, order_id: u32, base_filled: u64) -> VelocityEvent {
        VelocityEvent::OrderFill {
            maker: None,
            maker_fee: 0,
            maker_order_id: 0,
            maker_side: None,
            taker: Some(taker),
            taker_fee: 0,
            taker_order_id: order_id,
            taker_side: None,
            base_asset_amount_filled: base_filled,
            quote_asset_amount_filled: 1,
            market_index: 7,
            market_type: EventMarketType::Perp,
            oracle_price: 1,
            signature: "simulation".to_string(),
            tx_idx: 0,
            ts: 0,
            bit_flags: 0,
        }
    }

    #[test]
    fn fill_event_is_transaction_local_success_proof() {
        let taker = Pubkey::new_unique();
        let intent = TxIntent::LimitUncross {
            slot: 10,
            market_index: 7,
            taker_order_id: 42,
            taker_user: taker,
            maker_order_id: 9,
        };

        assert!(is_expected_fill_event(
            &order_fill_event(taker, 42, 1),
            &intent
        ));
        assert!(!is_expected_fill_event(
            &order_fill_event(taker, 42, 0),
            &intent
        ));
        assert!(!is_expected_fill_event(
            &order_fill_event(Pubkey::new_unique(), 42, 1),
            &intent
        ));
    }

    #[test]
    fn fill_tx_retains_revert_only_when_requested() {
        let program_data = ProgramData::new(
            vec![SpotMarket::default()],
            vec![PerpMarket::default()],
            vec![],
            State::default(),
        );

        let builder = TransactionBuilder::new(
            &program_data,
            Pubkey::new_unique(),
            Cow::Owned(User::default()),
            false,
        );
        let (send_tx, simulation_tx) = build_fill_tx(builder, false);
        assert_eq!(send_tx.instructions().len(), 0);
        assert_eq!(
            simulation_tx
                .expect("ordinary fills simulate with RevertFill")
                .instructions()
                .len(),
            1
        );

        let builder = TransactionBuilder::new(
            &program_data,
            Pubkey::new_unique(),
            Cow::Owned(User::default()),
            false,
        );
        let (send_tx, simulation_tx) = build_fill_tx(builder, true);
        assert_eq!(send_tx.instructions().len(), 1);
        assert!(
            simulation_tx.is_none(),
            "Pyth-update fills must retain RevertFill when sent"
        );
    }

    #[test]
    fn recognizes_revert_fill_simulation_error() {
        assert!(is_revert_fill_error(
            &TransactionError::InstructionError(3, InstructionError::Custom(6239)).into()
        ));
        assert!(!is_revert_fill_error(
            &TransactionError::InstructionError(3, InstructionError::Custom(6240)).into()
        ));
    }

    #[test]
    fn failed_liquidation_fill_records_takeover_fallback() {
        let liquidatee = Pubkey::new_unique();
        let intent = TxIntent::LiquidateWithFill {
            market_index: 7,
            liquidatee,
            slot: 42,
        };
        let fallbacks = std::sync::Arc::new(dashmap::DashMap::new());
        let error_code = velocity_rs::program::error::ErrorCode::LiquidationOrderFailedToFill
            as u32
            + anchor_lang::error::ERROR_CODE_OFFSET;

        record_takeover_fallback(
            &intent,
            &TransactionError::InstructionError(2, InstructionError::Custom(error_code)),
            Some(&fallbacks),
        );

        assert!(fallbacks.contains_key(&(liquidatee, 7)));
    }

    #[test]
    fn unrelated_failure_does_not_record_takeover_fallback() {
        let liquidatee = Pubkey::new_unique();
        let intent = TxIntent::LiquidateWithFill {
            market_index: 7,
            liquidatee,
            slot: 42,
        };
        let fallbacks = std::sync::Arc::new(dashmap::DashMap::new());

        record_takeover_fallback(
            &intent,
            &TransactionError::InstructionError(2, InstructionError::Custom(6239)),
            Some(&fallbacks),
        );

        assert!(fallbacks.is_empty());
    }
}
