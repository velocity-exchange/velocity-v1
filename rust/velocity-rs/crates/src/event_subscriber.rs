use std::{
    collections::VecDeque,
    str::FromStr,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
    time::Duration,
};

pub use crate::solana_sdk::commitment_config::CommitmentConfig;
use crate::solana_sdk::{
    pubkey::Pubkey, signature::Signature, transaction::versioned::VersionedTransaction,
};
use ahash::HashSet;
use anchor_lang::{AnchorDeserialize, Discriminator};
use base64::Engine;
use futures_util::{future::BoxFuture, stream::FuturesOrdered, FutureExt, Stream, StreamExt};
use log::{debug, info, warn};
use regex::Regex;
pub use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_rpc_client_api::{
    config::{RpcTransactionConfig, RpcTransactionLogsConfig, RpcTransactionLogsFilter},
    response::RpcLogsResponse,
};
use solana_transaction_status::{
    option_serializer::OptionSerializer, EncodedTransaction, EncodedTransactionWithStatusMeta,
    UiTransactionEncoding,
};
use tokio::{
    sync::{
        mpsc::{channel, Receiver, Sender},
        RwLock,
    },
    task::JoinHandle,
};
pub use velocity_pubsub_client::PubsubClient;

use crate::{
    constants::{self, PROGRAM_ID},
    grpc::{
        grpc_subscriber::{GeyserSubscribeOpts, GrpcConnectionOpts, VelocityGrpcClient},
        TransactionUpdate,
    },
    types::{events::SwapRecord, SdkResult},
    velocity_idl::{
        events::{FundingPaymentRecord, OrderActionRecord, OrderRecord},
        types::{MarketType, Order, OrderAction, OrderActionExplanation, PositionDirection},
    },
};

const LOG_TARGET: &str = "events";
const EMPTY_SIGNATURE: &str = "1111111111111111111111111111111111111111111111111111111111111111";

impl EventRpcProvider for RpcClient {
    fn get_tx(
        &self,
        signature: Signature,
    ) -> BoxFuture<'_, SdkResult<EncodedTransactionWithStatusMeta>> {
        async move {
            let result = self
                .get_transaction_with_config(
                    &signature,
                    RpcTransactionConfig {
                        encoding: Some(UiTransactionEncoding::Base64),
                        max_supported_transaction_version: Some(1),
                        ..Default::default()
                    },
                )
                .await?;

            Ok(result.transaction)
        }
        .boxed()
    }
    fn get_tx_signatures(
        &self,
        account: Pubkey,
        after: Option<Signature>,
        limit: Option<usize>,
    ) -> BoxFuture<'_, SdkResult<Vec<String>>> {
        async move {
            let results = self
                .get_signatures_for_address_with_config(
                    &account,
                    GetConfirmedSignaturesForAddress2Config {
                        until: after,
                        limit,
                        ..Default::default()
                    },
                )
                .await?;

            Ok(results.iter().map(|r| r.signature.clone()).collect())
        }
        .boxed()
    }
}

/// RPC functions required for velocity event subscriptions
pub trait EventRpcProvider: Send + Sync + 'static {
    /// Fetch tx signatures of account
    /// `after` only return txs more recent than this signature, if given
    /// `limit` return at most this many signatures, if given
    fn get_tx_signatures(
        &self,
        account: Pubkey,
        after: Option<Signature>,
        limit: Option<usize>,
    ) -> BoxFuture<'_, SdkResult<Vec<String>>>;
    /// Fetch tx with `signature`
    fn get_tx(
        &self,
        signature: Signature,
    ) -> BoxFuture<'_, SdkResult<EncodedTransactionWithStatusMeta>>;
}

/// Provides sub-account event streaming
pub struct EventSubscriber;

impl EventSubscriber {
    /// Subscribe to velocity events of `sub_account`, backed by Ws APIs
    ///
    /// * `sub_account` - pubkey of the user's sub-account to subscribe to (use Velocity Program ID to get all program events)
    ///
    /// passing the driftV2 address `dRiftyHA39MWEi3m9aunc5MzRF1JYuBsbn6VPcn33UH`
    /// will yield events from all sub-accounts.
    ///
    /// Returns a stream of events
    pub async fn subscribe(
        ws: Arc<PubsubClient>,
        sub_account: Pubkey,
    ) -> SdkResult<VelocityEventStream> {
        log_stream(ws, sub_account).await
    }
    /// Subscribe to velocity events of `sub_account`, backed by RPC polling APIs
    pub fn subscribe_polled(
        provider: impl EventRpcProvider,
        account: Pubkey,
    ) -> VelocityEventStream {
        polled_stream(provider, account)
    }

    pub async fn subscribe_grpc(
        endpoint: String,
        x_token: String,
        sub_account: Pubkey,
    ) -> SdkResult<VelocityEventStream> {
        grpc_log_stream(endpoint, x_token, sub_account).await
    }
}

struct LogEventStream {
    cache: Arc<RwLock<TxSignatureCache>>,
    provider: Arc<PubsubClient>,
    sub_account: Pubkey,
    event_tx: Sender<VelocityEvent>,
    commitment: CommitmentConfig,
}

impl LogEventStream {
    /// Returns a future for running the configured log event stream
    async fn stream_fn(self) {
        let sub_account = self.sub_account;
        info!(target: LOG_TARGET, "log stream connecting: {sub_account:?}");

        let subscribe_result = self
            .provider
            .logs_subscribe(
                RpcTransactionLogsFilter::Mentions(vec![self.sub_account.to_string()]),
                RpcTransactionLogsConfig {
                    commitment: Some(self.commitment),
                },
            )
            .await;

        if let Err(ref err) = subscribe_result {
            warn!(
                target: LOG_TARGET,
                "log subscription failed for: {sub_account:?}. {err:?}"
            );
            return;
        }

        let (mut log_stream, _unsub_fn) = subscribe_result.unwrap();
        debug!(
            target: LOG_TARGET,
            "start log subscription: {sub_account:?}"
        );

        while let Some(response) = log_stream.next().await {
            self.process_log(response.context.slot, response.value)
                .await;
        }
        warn!(target: LOG_TARGET, "log stream ended: {sub_account:?}");
    }

    /// Process a log response from RPC, emitting any relevant events
    async fn process_log(&self, slot: u64, response: RpcLogsResponse) {
        let signature = response.signature;
        if response.err.is_some() {
            debug!(target: LOG_TARGET, "skipping failed tx: {signature:?}");
            return;
        }
        if signature == EMPTY_SIGNATURE {
            debug!(target: LOG_TARGET, "skipping empty signature, logs");
            return;
        }
        {
            let mut cache = self.cache.write().await;
            if cache.contains(&signature) {
                debug!(target: LOG_TARGET, "skipping cached tx: {signature:?}");
                return;
            }
            cache.insert(signature.clone());
        }

        debug!(
            target: LOG_TARGET,
            "log extracting events, slot: {slot}, tx: {signature:?}"
        );
        for event in parse_velocity_logs(response.logs.iter().map(String::as_str), &signature) {
            // unrelated events from same tx should not be emitted e.g. a filler tx which produces other fill events
            if event.pertains_to(self.sub_account) && self.event_tx.send(event).await.is_err() {
                warn!("event receiver closed");
                return;
            }
        }
    }
}

struct GrpcLogEventStream {
    grpc_endpoint: String,
    grpc_x_token: String,
    sub_account: Pubkey,
    event_tx: Sender<VelocityEvent>,
    commitment: CommitmentConfig,
}

impl GrpcLogEventStream {
    /// Returns a future for running the configured log event stream
    async fn stream_fn(self) {
        let sub_account = self.sub_account;
        info!(
            target: LOG_TARGET,
            "grpc log stream connecting: {sub_account:?}"
        );

        let mut grpc =
            VelocityGrpcClient::new(self.grpc_endpoint.clone(), self.grpc_x_token.clone())
                .grpc_connection_opts(GrpcConnectionOpts::default());

        let (raw_event_tx, mut raw_event_rx): (
            Sender<TransactionUpdate>,
            Receiver<TransactionUpdate>,
        ) = channel(256);

        let raw_event_tx_clone = raw_event_tx.clone();
        grpc.on_transaction(Box::new(move |tx_update: &TransactionUpdate| {
            raw_event_tx_clone.try_send(tx_update.clone()).unwrap();
        }));

        // prevent dropping unsub_fn and unsubscribing from grpc
        let _unsub_fn = grpc
            .subscribe(
                self.commitment.commitment,
                GeyserSubscribeOpts {
                    transactions_accounts_include: vec![sub_account.to_string()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        info!(
            target: LOG_TARGET,
            "grpc log stream connected: {sub_account:?}"
        );

        while let Some(event) = raw_event_rx.recv().await {
            let start = std::time::Instant::now();
            let slot = event.slot;
            self.process_log(&event).await;
            let elapsed = start.elapsed();
            debug!(target: "grpc", "transaction slot: {}, len: {} callbacks took {:?}", slot, raw_event_rx.len(), elapsed);
        }
        info!(target: LOG_TARGET, "grpc log stream ended: {sub_account:?}");
    }

    /// Process a log response from RPC, emitting any relevant events
    async fn process_log(&self, event: &TransactionUpdate) {
        let signature = event.transaction.signatures.first();
        if signature.is_none() {
            debug!(target: LOG_TARGET, "skipping tx with no signatures");
            return;
        }
        let signature =
            Signature::from(<[u8; 64]>::try_from(signature.unwrap().as_slice()).unwrap());

        debug!(
            target: LOG_TARGET,
            "log extracting events, slot: {}, tx: {}", event.slot, signature
        );
        let logs = &event.meta.log_messages;
        for event in parse_velocity_logs(logs.iter().map(String::as_str), &signature.to_string()) {
            // unrelated events from same tx should not be emitted e.g. a filler tx which produces other fill events
            if event.pertains_to(self.sub_account) && self.event_tx.send(event).await.is_err() {
                warn!("event receiver closed");
                return;
            }
        }
    }
}

/// Creates a poll-ed stream using JSON-RPC interfaces
fn polled_stream(provider: impl EventRpcProvider, sub_account: Pubkey) -> VelocityEventStream {
    let (event_tx, event_rx) = channel(256);
    let cache = Arc::new(RwLock::new(TxSignatureCache::new(128)));
    let join_handle = tokio::spawn(
        PolledEventStream {
            cache: Arc::clone(&cache),
            provider,
            sub_account,
            event_tx,
        }
        .stream_fn(),
    );

    VelocityEventStream {
        rx: event_rx,
        task: join_handle,
    }
}

/// Creates a Ws-backed event stream using `logsSubscribe` interface
async fn log_stream(ws: Arc<PubsubClient>, sub_account: Pubkey) -> SdkResult<VelocityEventStream> {
    debug!(target: LOG_TARGET, "stream events for {sub_account:?}");
    let (event_tx, event_rx) = channel(256);
    let cache = Arc::new(RwLock::new(TxSignatureCache::new(256)));

    // spawn the event subscription task
    let join_handle = tokio::spawn(async move {
        LogEventStream {
            provider: ws,
            cache: Arc::clone(&cache),
            sub_account,
            event_tx: event_tx.clone(),
            commitment: CommitmentConfig::confirmed(),
        }
        .stream_fn()
        .await;
    });

    Ok(VelocityEventStream {
        rx: event_rx,
        task: join_handle,
    })
}

/// Creates a grpc-backed event stream
async fn grpc_log_stream(
    endpoint: String,
    x_token: String,
    sub_account: Pubkey,
) -> SdkResult<VelocityEventStream> {
    debug!(target: LOG_TARGET, "grpc stream events for {sub_account:?}");
    let (event_tx, event_rx) = channel(256);

    // spawn the event subscription task
    let join_handle = tokio::spawn(async move {
        GrpcLogEventStream {
            grpc_endpoint: endpoint.clone(),
            grpc_x_token: x_token.clone(),
            sub_account,
            event_tx: event_tx.clone(),
            commitment: CommitmentConfig::confirmed(),
        }
        .stream_fn()
        .await;
    });

    Ok(VelocityEventStream {
        rx: event_rx,
        task: join_handle,
    })
}

/// Whether a polled tx should have its logs walked for Velocity events.
///
/// Prefer the decoded message's static account keys as a cheap skip when
/// `PROGRAM_ID` is absent. A payload these crates cannot deserialize at all
/// (`decode()` is `None`: corrupt, or a wire version newer than this crate
/// stack) still has its logs walked, so Velocity events are not dropped.
/// Walking is not enough on its own: `parse_velocity_logs` only decodes
/// payloads while `PROGRAM_ID` is the executing program in the invocation
/// stack.
fn poll_should_parse_velocity_logs(transaction: &EncodedTransaction, signature: &str) -> bool {
    match transaction.decode() {
        Some(VersionedTransaction { message, .. }) => message
            .static_account_keys()
            .iter()
            .any(|k| k == &PROGRAM_ID),
        None => {
            // A corrupt payload, or a wire version newer than these crates. Keep it at
            // debug like the other per-tx poll messages.
            debug!(
                target: LOG_TARGET,
                "poll undecodable tx, walking logs without account-keys check: {signature}"
            );
            true
        }
    }
}

pub struct PolledEventStream<T: EventRpcProvider> {
    cache: Arc<RwLock<TxSignatureCache>>,
    event_tx: Sender<VelocityEvent>,
    provider: T,
    sub_account: Pubkey,
}

/// How often the poller re-reads signatures. A flat throttle to avoid spamming the
/// RPC, not a slot count: mainnet slot time keeps falling and this poll does not
/// track it, so each tick just returns more signatures.
const POLL_INTERVAL: Duration = Duration::from_millis(400);

impl<T: EventRpcProvider> PolledEventStream<T> {
    async fn stream_fn(self) {
        debug!(target: LOG_TARGET, "poll events for {:?}", self.sub_account);
        // poll for events in any tx after this tx
        // initially fetch the most recent tx from account
        debug!(target: LOG_TARGET, "fetch initial txs");
        let res = self
            .provider
            .get_tx_signatures(self.sub_account, None, Some(1))
            .await;
        debug!(target: LOG_TARGET, "fetched initial txs");

        let mut last_seen_tx = res.expect("fetched tx").first().cloned();
        let provider_ref = &self.provider;
        'outer: loop {
            tokio::time::sleep(POLL_INTERVAL).await;

            debug!(target: LOG_TARGET, "poll txs for events");
            let signatures = provider_ref
                .get_tx_signatures(
                    self.sub_account,
                    last_seen_tx
                        .clone()
                        .map(|s| Signature::from_str(s.as_str()).unwrap()),
                    None,
                )
                .await;

            if let Err(err) = signatures {
                warn!(target: LOG_TARGET, "poll tx signatures: {err:?}");
                continue;
            }

            let signatures = signatures.unwrap();
            // txs from RPC are ordered newest to oldest
            // process in reverse order, so subscribers receive events in chronological order
            let mut futs = {
                FuturesOrdered::from_iter(
                    signatures
                        .into_iter()
                        .map(|s| async move {
                            (
                                s.clone(),
                                provider_ref
                                    .get_tx(
                                        Signature::from_str(s.as_str()).expect("valid signature"),
                                    )
                                    .await,
                            )
                        })
                        .rev(),
                )
            };
            if futs.is_empty() {
                continue;
            }

            while let Some((signature, response)) = futs.next().await {
                debug!(
                    target: LOG_TARGET,
                    "poll extracting events, tx: {signature:?}"
                );
                if let Err(err) = response {
                    warn!(target: LOG_TARGET, "poll processing tx: {err:?}");
                    // retry querying the batch
                    continue 'outer;
                }

                last_seen_tx = Some(signature.clone());
                {
                    let mut cache = self.cache.write().await;
                    if cache.contains(&signature) {
                        debug!(target: LOG_TARGET, "poll skipping cached tx: {signature:?}");
                        continue;
                    }
                    cache.insert(signature.clone());
                }

                let EncodedTransactionWithStatusMeta {
                    meta, transaction, ..
                } = response.unwrap();
                if meta.is_none() {
                    continue;
                }
                let meta = meta.unwrap();

                // Prefer the account-keys cheap-skip. A payload that does not
                // deserialize (corrupt, or a wire version newer than these
                // crates) still has its logs walked rather than dropping
                // Velocity events. Parsing itself is invocation-gated.
                if !poll_should_parse_velocity_logs(&transaction, signature.as_str()) {
                    continue;
                }
                // ignore failed txs
                if meta.err.is_some() {
                    continue;
                }

                if let OptionSerializer::Some(logs) = meta.log_messages {
                    for event in
                        parse_velocity_logs(logs.iter().map(String::as_str), signature.as_str())
                    {
                        if event.pertains_to(self.sub_account) {
                            // A full channel or a closed receiver must not take the
                            // poll task down with it.
                            if let Err(err) = self.event_tx.try_send(event) {
                                warn!(target: LOG_TARGET, "poll dropping event: {err}");
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Provides a stream API of velocity sub-account events
pub struct VelocityEventStream {
    /// handle to end the stream task
    task: JoinHandle<()>,
    /// channel of events from stream task
    rx: Receiver<VelocityEvent>,
}

impl VelocityEventStream {
    /// End the event stream
    pub fn unsubscribe(&self) {
        self.task.abort();
    }
}

impl Drop for VelocityEventStream {
    fn drop(&mut self) {
        self.unsubscribe()
    }
}

impl Stream for VelocityEventStream {
    type Item = VelocityEvent;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.as_mut().rx.poll_recv(cx)
    }
}

const PROGRAM_LOG: &str = "Program log: ";
const PROGRAM_DATA: &str = "Program data: ";

/// CPI invocation stack while walking a transaction's log lines.
/// `true` frames are `PROGRAM_ID`; `false` frames are any other program.
#[derive(Default)]
pub struct ProgramInvocationStack {
    stack: Vec<bool>,
}

impl ProgramInvocationStack {
    fn observe(&mut self, raw: &str) {
        let log_start = raw.split_once(':').map(|(head, _)| head).unwrap_or(raw);
        if log_start.starts_with("Program ")
            && (log_start.ends_with(" success") || log_start.ends_with(" failed"))
        {
            self.stack.pop();
        } else if log_start.starts_with(velocity_program_invoke_prefix()) {
            self.stack.push(true);
        } else if log_start.contains(" invoke") {
            self.stack.push(false);
        }
    }

    fn is_velocity_executing(&self) -> bool {
        self.stack.last() == Some(&true)
    }
}

fn velocity_program_invoke_prefix() -> &'static str {
    static PREFIX: OnceLock<String> = OnceLock::new();
    PREFIX.get_or_init(|| format!("Program {PROGRAM_ID} invoke"))
}

/// Parse Velocity events from a transaction's logs.
///
/// Only `Program log:` / `Program data:` lines emitted while `PROGRAM_ID` is
/// the currently executing program (including nested CPI) are decoded. This
/// keeps unrelated programs' matching discriminators away from
/// [`VelocityEvent::from_discriminant`].
pub fn parse_velocity_logs<'a>(
    logs: impl IntoIterator<Item = &'a str>,
    signature: &str,
) -> Vec<VelocityEvent> {
    let mut invocation = ProgramInvocationStack::default();
    let mut events = Vec::new();
    for (tx_idx, log) in logs.into_iter().enumerate() {
        if log.starts_with("Log truncated") {
            break;
        }
        if let Some(event) = try_parse_log(&mut invocation, log, signature, tx_idx) {
            events.push(event);
        }
    }
    events
}

/// Try deserialize a velocity event type from raw log string
/// https://github.com/coral-xyz/anchor/blob/9d947cb26b693e85e1fd26072bb046ff8f95bdcf/client/src/lib.rs#L552
///
/// Updates `invocation` from invoke/success/failed lines and only decodes a
/// payload while `PROGRAM_ID` is executing. Callers walking a full tx log
/// should reuse the same stack across lines (see [`parse_velocity_logs`]).
pub fn try_parse_log(
    invocation: &mut ProgramInvocationStack,
    raw: &str,
    signature: &str,
    tx_idx: usize,
) -> Option<VelocityEvent> {
    invocation.observe(raw);
    if !invocation.is_velocity_executing() {
        return None;
    }
    try_parse_program_log(raw, signature, tx_idx)
}

/// Decode a single `Program log:` / `Program data:` payload with no CPI-stack
/// check. Prefer [`try_parse_log`] / [`parse_velocity_logs`] on real tx logs.
fn try_parse_program_log(raw: &str, signature: &str, tx_idx: usize) -> Option<VelocityEvent> {
    // Log emitted from the current program.
    if let Some(log) = raw
        .strip_prefix(PROGRAM_LOG)
        .or_else(|| raw.strip_prefix(PROGRAM_DATA))
    {
        if let Ok(borsh_bytes) = base64::engine::general_purpose::STANDARD.decode(log) {
            let (disc, mut data) = borsh_bytes.split_at(8);
            return match disc.try_into() {
                Ok(disc) => VelocityEvent::from_discriminant(disc, &mut data, signature, tx_idx),
                Err(_err) => {
                    log::debug!("event subscriber: invalid program log: {log}");
                    None
                }
            };
        }

        // experimental
        let order_cancel_missing_re = ORDER_CANCEL_MISSING_RE
            .get_or_init(|| Regex::new(r"could not find( user){0,1} order id (\d+)").unwrap());
        if let Some(captures) = order_cancel_missing_re.captures(log) {
            let order_id = captures
                .get(2)
                .unwrap()
                .as_str()
                .parse::<u32>()
                .expect("<u32");
            let event = if captures.get(1).is_some() {
                // cancel by user order Id
                VelocityEvent::OrderCancelMissing {
                    user_order_id: order_id as u8,
                    order_id: 0,
                    signature: signature.to_string(),
                }
            } else {
                // cancel by order id
                VelocityEvent::OrderCancelMissing {
                    user_order_id: 0,
                    order_id,
                    signature: signature.to_string(),
                }
            };

            return Some(event);
        }
    }

    None
}

static ORDER_CANCEL_MISSING_RE: OnceLock<Regex> = OnceLock::new();

/// Enum of all velocity program events
#[derive(Debug, PartialEq)]
pub enum VelocityEvent {
    OrderFill {
        maker: Option<Pubkey>,
        maker_fee: i64,
        maker_order_id: u32,
        maker_side: Option<PositionDirection>,
        taker: Option<Pubkey>,
        taker_fee: u64,
        taker_order_id: u32,
        taker_side: Option<PositionDirection>,
        base_asset_amount_filled: u64,
        quote_asset_amount_filled: u64,
        market_index: u16,
        market_type: MarketType,
        oracle_price: i64,
        signature: String,
        tx_idx: usize,
        ts: u64,
        bit_flags: u8,
    },
    OrderCancel {
        taker: Option<Pubkey>,
        maker: Option<Pubkey>,
        taker_order_id: u32,
        maker_order_id: u32,
        signature: String,
        tx_idx: usize,
        ts: u64,
    },
    /// An order cancel for a missing order Id / user order id
    OrderCancelMissing {
        user_order_id: u8,
        order_id: u32,
        signature: String,
    },
    OrderCreate {
        order: Order,
        user: Pubkey,
        ts: u64,
        signature: String,
        tx_idx: usize,
    },
    // sub-case of cancel?
    OrderExpire {
        order_id: u32,
        user: Option<Pubkey>,
        fee: u64,
        ts: u64,
        signature: String,
        tx_idx: usize,
    },
    FundingPayment {
        amount: i64,
        market_index: u16,
        user: Pubkey,
        ts: u64,
        signature: String,
        tx_idx: usize,
    },
    Swap {
        user: Pubkey,
        amount_in: u64,
        amount_out: u64,
        market_in: u16,
        market_out: u16,
        fee: u64,
        ts: u64,
        signature: String,
        tx_idx: usize,
    },
    OrderTrigger {
        /// trigger order owner
        user: Pubkey,
        order_id: u32,
        oracle_price: u64,
        /// base asset amount
        amount: u64,
    },
}

impl VelocityEvent {
    /// Return true if the event is connected to sub-account
    fn pertains_to(&self, sub_account: Pubkey) -> bool {
        if sub_account == PROGRAM_ID {
            return true;
        }
        let subject = &Some(sub_account);
        match self {
            Self::OrderCancel { maker, taker, .. } | Self::OrderFill { maker, taker, .. } => {
                maker == subject || taker == subject
            }
            Self::OrderCreate { user, .. } => *user == sub_account,
            Self::OrderExpire { user, .. } => user == subject,
            Self::OrderCancelMissing { .. } => true,
            Self::FundingPayment { user, .. } => *user == sub_account,
            Self::Swap { user, .. } => *user == sub_account,
            Self::OrderTrigger { user, .. } => *user == sub_account,
        }
    }
    /// Deserialize velocity event by discriminant
    fn from_discriminant(
        disc: [u8; 8],
        data: &mut &[u8],
        signature: &str,
        tx_idx: usize,
    ) -> Option<Self> {
        match disc.as_slice() {
            // deser should only fail on a breaking protocol changes
            OrderActionRecord::DISCRIMINATOR => Self::from_oar(
                OrderActionRecord::deserialize(data).expect("deserializes"),
                signature,
                tx_idx,
            ),
            OrderRecord::DISCRIMINATOR => Self::from_order_record(
                OrderRecord::deserialize(data).expect("deserializes"),
                signature,
                tx_idx,
            ),
            FundingPaymentRecord::DISCRIMINATOR => Some(Self::from_funding_payment_record(
                FundingPaymentRecord::deserialize(data).expect("deserializes"),
                signature,
                tx_idx,
            )),
            SwapRecord::DISCRIMINATOR => Some(Self::from_swap_record(
                SwapRecord::deserialize(data).expect("deserializes"),
                signature,
                tx_idx,
            )),
            _ => {
                debug!(target: LOG_TARGET, "unhandled event: {disc:?}");
                None
            }
        }
    }
    fn from_swap_record(value: SwapRecord, signature: &str, tx_idx: usize) -> Self {
        Self::Swap {
            amount_in: value.amount_in,
            amount_out: value.amount_out,
            market_in: value.in_market_index,
            market_out: value.out_market_index,
            fee: value.fee,
            ts: value.ts.unsigned_abs(),
            user: value.user,
            signature: signature.to_string(),
            tx_idx,
        }
    }
    fn from_funding_payment_record(
        value: FundingPaymentRecord,
        signature: &str,
        tx_idx: usize,
    ) -> Self {
        Self::FundingPayment {
            amount: value.funding_payment,
            market_index: value.market_index,
            ts: value.ts.unsigned_abs(),
            user: value.user,
            signature: signature.to_string(),
            tx_idx,
        }
    }
    fn from_order_record(value: OrderRecord, signature: &str, tx_idx: usize) -> Option<Self> {
        Some(VelocityEvent::OrderCreate {
            order: value.order,
            user: value.user,
            ts: value.ts.unsigned_abs(),
            signature: signature.to_string(),
            tx_idx,
        })
    }
    fn from_oar(value: OrderActionRecord, signature: &str, tx_idx: usize) -> Option<Self> {
        match value.action {
            OrderAction::Cancel => {
                if let OrderActionExplanation::OrderExpired = value.action_explanation {
                    // TODO: would be nice to report the `user_order_id` too...
                    Some(VelocityEvent::OrderExpire {
                        fee: value.filler_reward.unwrap_or_default(),
                        order_id: value
                            .maker_order_id
                            .or(value.taker_order_id)
                            .expect("order id set"),
                        ts: value.ts.unsigned_abs(),
                        signature: signature.to_string(),
                        tx_idx,
                        user: value.maker.or(value.taker),
                    })
                } else {
                    Some(VelocityEvent::OrderCancel {
                        maker: value.maker,
                        taker: value.taker,
                        maker_order_id: value.maker_order_id.unwrap_or_default(),
                        taker_order_id: value.taker_order_id.unwrap_or_default(),
                        ts: value.ts.unsigned_abs(),
                        signature: signature.to_string(),
                        tx_idx,
                    })
                }
            }
            OrderAction::Fill => Some(VelocityEvent::OrderFill {
                maker: value.maker,
                maker_fee: value.maker_fee.unwrap_or_default(),
                maker_order_id: value.maker_order_id.unwrap_or_default(),
                maker_side: value.maker_order_direction,
                taker: value.taker,
                taker_fee: value.taker_fee.unwrap_or_default(),
                taker_order_id: value.taker_order_id.unwrap_or_default(),
                taker_side: value.taker_order_direction,
                base_asset_amount_filled: value.base_asset_amount_filled.unwrap_or_default(),
                quote_asset_amount_filled: value.quote_asset_amount_filled.unwrap_or_default(),
                oracle_price: value.oracle_price,
                market_index: value.market_index,
                market_type: value.market_type,
                ts: value.ts.unsigned_abs(),
                signature: signature.to_string(),
                tx_idx,
                bit_flags: value.bit_flags,
            }),
            OrderAction::Trigger => Some(VelocityEvent::OrderTrigger {
                amount: value.taker_order_base_asset_amount.unwrap_or(0),
                oracle_price: value.oracle_price.unsigned_abs(),
                user: value.taker.unwrap_or_default(),
                order_id: value.taker_order_id.unwrap_or(0),
            }),
            // Place - parsed from `OrderRecord` event, ignored here due to lack of useful info
            // Expire - never emitted
            OrderAction::Place | OrderAction::Expire => None,
        }
    }
}

/// fixed capacity cache of tx signatures
struct TxSignatureCache {
    capacity: usize,
    entries: HashSet<String>,
    age: VecDeque<String>,
}

impl TxSignatureCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashSet::<String>::with_capacity_and_hasher(capacity, Default::default()),
            age: VecDeque::with_capacity(capacity),
        }
    }
    fn contains(&self, x: &str) -> bool {
        self.entries.contains(x)
    }
    fn insert(&mut self, x: String) {
        self.entries.insert(x.clone());
        self.age.push_back(x);

        if self.age.len() >= self.capacity {
            if let Some(ref oldest) = self.age.pop_front() {
                self.entries.remove(oldest);
            }
        }
    }
    #[cfg(test)]
    fn reset(&mut self) {
        self.entries.clear()
    }
}

#[cfg(test)]
mod test {
    use crate::solana_sdk::{
        instruction::{AccountMeta, Instruction},
        message::{v0, Hash, VersionedMessage},
        pubkey::Pubkey,
    };
    use ahash::HashMap;
    use anchor_lang::prelude::*;
    use base64::Engine;
    use futures_util::future::ready;
    use solana_transaction_status::{TransactionStatusMeta, VersionedTransactionWithStatusMeta};
    use tokio::sync::Mutex;

    use super::*;
    use crate::SdkError;

    #[cfg(feature = "rpc_tests")]
    #[tokio::test]
    async fn event_streaming_logs() {
        let ws = Arc::new(
            PubsubClient::new("wss://api.devnet.solana.com")
                .await
                .expect("ws connects"),
        );
        let mut event_stream = EventSubscriber::subscribe(
            ws,
            Pubkey::from_str("9JtczxrJjPM4J1xooxr2rFXmRivarb4BwjNiBgXDwe2p").unwrap(),
        )
        .await
        .unwrap()
        .take(5);

        // Bound the wait: this account has no event activity on the velocity devnet, so
        // the stream never yields and the test would hang forever (no harness timeout).
        // The real assertion above is that subscribe() connects + subscribes; drain
        // whatever arrives within the window and finish either way.
        let _ = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(event) = event_stream.next().await {
                dbg!(event);
            }
        })
        .await;
    }

    #[test]
    fn parses_nested_cpi_logs() {
        let _ = env_logger::try_init();

        // When another program CPIs into velocity, the velocity events
        // (OrderRecord place + OrderActionRecord fill) are emitted as
        // `Program log:`/`Program data:` lines nested under the outer program's
        // invocation. Synthesize those events from the current types and
        // confirm they parse out of the interleaved outer-program logs.
        let taker = Pubkey::new_unique();
        let maker = Pubkey::new_unique();

        let taker_order = Order {
            order_id: 11,
            base_asset_amount: 1_000_000,
            market_type: MarketType::Perp,
            direction: PositionDirection::Long,
            ..Default::default()
        };
        let maker_order = Order {
            order_id: 22,
            base_asset_amount: 1_000_000,
            market_type: MarketType::Perp,
            direction: PositionDirection::Short,
            ..Default::default()
        };

        let order_record = OrderRecord {
            ts: 1_700_000_000,
            user: taker,
            order: taker_order,
        };
        let fill = get_order_action_record(
            1_700_000_000,
            OrderAction::Fill,
            OrderActionExplanation::None,
            0,
            None,
            Some(1),
            None,
            Some(1_000_000),
            Some(2_000_000),
            Some(500),
            None,
            None,
            None,
            None,
            Some(taker),
            Some(taker_order),
            Some(maker),
            Some(maker_order),
            123_456,
            0,
        );

        let cpi_logs = &[
            "Program vAuLTsyrvSfZRuRB3XgvkPwNGgYSs9YRYymVebLKoxR invoke [1]".to_string(),
            "Program log: Instruction: FillOrder".to_string(),
            "Program dRiftyHA39MWEi3m9aunc5MzRF1JYuBsbn6VPcn33UH invoke [2]".to_string(),
            format!("{PROGRAM_DATA}{}", serialize_event(order_record)),
            format!("{PROGRAM_DATA}{}", serialize_event(fill)),
            "Program vAuLTsyrvSfZRuRB3XgvkPwNGgYSs9YRYymVebLKoxR success".to_string(),
        ];

        let events = parse_velocity_logs(cpi_logs.iter().map(String::as_str), "sig");

        assert!(
            events.iter().any(
                |e| matches!(e, VelocityEvent::OrderCreate { order, .. } if order.order_id == 11)
            ),
            "expected an OrderCreate event from the nested OrderRecord, got: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                VelocityEvent::OrderFill {
                    taker_order_id: 11,
                    maker_order_id: 22,
                    ..
                }
            )),
            "expected an OrderFill event from the nested OrderActionRecord, got: {events:?}"
        );
    }

    #[tokio::test]
    async fn polled_event_stream_caching() {
        let _ = env_logger::try_init();
        struct MockRpcProvider {
            tx_responses: HashMap<String, EncodedTransactionWithStatusMeta>,
            signatures: tokio::sync::Mutex<Vec<String>>,
        }

        impl MockRpcProvider {
            async fn add_signatures(&self, signatures: Vec<String>) {
                let mut all_signatures = self.signatures.lock().await;
                all_signatures.extend(signatures.into_iter());
            }
        }

        impl EventRpcProvider for Arc<MockRpcProvider> {
            fn get_tx(
                &self,
                signature: Signature,
            ) -> BoxFuture<SdkResult<EncodedTransactionWithStatusMeta>> {
                ready(
                    self.tx_responses
                        .get(signature.to_string().as_str())
                        .ok_or(SdkError::Deserializing)
                        .cloned(),
                )
                .boxed()
            }
            fn get_tx_signatures(
                &self,
                _account: Pubkey,
                after: Option<Signature>,
                _limit: Option<usize>,
            ) -> BoxFuture<SdkResult<Vec<String>>> {
                async move {
                    let after = after.map(|s| s.to_string());
                    let mut self_signatures = self.signatures.lock().await;
                    if after.is_none() {
                        return Ok(self_signatures.clone());
                    }

                    if let Some(idx) = self_signatures
                        .iter()
                        .position(|s| Some(s) == after.as_ref())
                    {
                        if idx > 0 {
                            // newest -> oldest
                            *self_signatures = self_signatures[..idx].to_vec();
                        } else {
                            self_signatures.clear();
                        }
                    }

                    Ok(self_signatures.clone())
                }
                .boxed()
            }
        }

        let (event_tx, mut event_rx) = channel(16);
        let sub_account = Pubkey::new_unique();
        let cache = Arc::new(RwLock::new(TxSignatureCache::new(16)));

        let mut order_events: Vec<(OrderActionRecord, OrderRecord)> = (0..5)
            .map(|id| {
                (
                    get_order_action_record(
                        id as i64,
                        OrderAction::Place,
                        OrderActionExplanation::None,
                        0,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some(sub_account.clone()),
                        Some(Order {
                            order_id: id,
                            ..Default::default()
                        }),
                        0,
                        0,
                    ),
                    OrderRecord {
                        ts: id as i64,
                        user: sub_account,
                        order: Order {
                            order_id: id,
                            ..Default::default()
                        },
                    },
                )
            })
            .collect();
        let signatures: Vec<String> = (0..order_events.len())
            .map(|_| Signature::new_unique().to_string())
            .collect();
        let mut tx_responses = HashMap::<String, EncodedTransactionWithStatusMeta>::default();
        for s in signatures.iter() {
            let (oar, or) = order_events.pop().unwrap();
            tx_responses.insert(
                s.clone(),
                make_transaction(
                    sub_account,
                    Signature::from_str(s).unwrap(),
                    Some(vec![
                        format!("Program {PROGRAM_ID} invoke [1]"),
                        format!("{PROGRAM_LOG}{}", serialize_event(oar)),
                        format!("{PROGRAM_LOG}{}", serialize_event(or)),
                        format!("Program {PROGRAM_ID} success"),
                    ]),
                ),
            );
        }

        let mock_rpc_provider = Arc::new(MockRpcProvider {
            tx_responses,
            signatures: Mutex::new(vec![signatures.first().unwrap().clone()]),
        });

        tokio::spawn(
            PolledEventStream {
                cache: Arc::clone(&cache),
                provider: Arc::clone(&mock_rpc_provider),
                sub_account,
                event_tx,
            }
            .stream_fn(),
        );
        tokio::time::sleep(Duration::from_secs(1)).await;

        // add 4 new tx signtaures
        // 1) cached
        // 2,3) emit events
        // 4) cached
        {
            let mut cache_ = cache.write().await;
            cache_.insert(signatures[1].clone());
            cache_.insert(signatures[4].clone());
        }
        mock_rpc_provider
            .add_signatures(signatures[1..].to_vec())
            .await;
        tokio::time::sleep(Duration::from_secs(1)).await;

        assert!(event_rx.recv().await.is_some_and(|f| {
            if let VelocityEvent::OrderCreate { order, .. } = f {
                println!("{}", order.order_id);
                order.order_id == 1
            } else {
                false
            }
        }));
        assert!(event_rx.recv().await.is_some_and(|f| {
            if let VelocityEvent::OrderCreate { order, .. } = f {
                println!("{}", order.order_id);
                order.order_id == 2
            } else {
                false
            }
        }));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn parses_swap_logs() {
        let _ = env_logger::try_init();

        // Build a current-layout `SwapRecord` and serialize it the way the
        // program emits events, rather than relying on a captured base64 blob
        // that goes stale whenever the event layout changes.
        let user = solana_pubkey::pubkey!("7q6FkeUEvTDS6DaM2WTHw6s1gTzbBasGTPATLzMZW41S");
        let swap = SwapRecord {
            ts: 1746413978,
            user,
            amount_out: 13814365,
            amount_in: 2000000,
            out_market_index: 1,
            in_market_index: 0,
            out_oracle_price: 0,
            in_oracle_price: 0,
            fee: 0,
        };

        let logs = [
            format!("Program {PROGRAM_ID} invoke [1]"),
            "Program log: Instruction: BeginSwap".to_string(),
            "Program log: Instruction: EndSwap".to_string(),
            format!("{PROGRAM_DATA}{}", serialize_event(swap)),
            format!("Program {PROGRAM_ID} success"),
        ];

        let sig = "2M1e4UJ1x6rwvjFR6kh5CDCWZg8NcGeqzT2GbDRGaC2TmZDgNTNbKSn4Y4pu11apErVycpk5p3Hq6Tg2nrFdGimm";
        let res = parse_velocity_logs(logs.iter().map(String::as_str), sig);
        assert_eq!(
            res[0],
            VelocityEvent::Swap {
                user,
                amount_in: 2000000,
                amount_out: 13814365,
                market_in: 0,
                market_out: 1,
                fee: 0,
                ts: 1746413978,
                signature: sig.try_into().unwrap(),
                tx_idx: 3,
            }
        );
    }

    fn undecodable_tx() -> EncodedTransaction {
        EncodedTransaction::Binary(
            "not-valid-base64!!!".into(),
            solana_transaction_status::TransactionBinaryEncoding::Base64,
        )
    }

    #[test]
    fn poll_parses_undecodable_tx_instead_of_dropping() {
        assert!(poll_should_parse_velocity_logs(&undecodable_tx(), "sig"));
    }

    /// A transaction v1 wire payload (SIMD-0385), laid out byte by byte: the
    /// `0x81` version byte, then the message whose compute budget lives in a
    /// config mask instead of instructions, then the signatures at the END with
    /// no length prefix. Assembled byte by byte rather than through the crate API
    /// so the test pins the wire layout, not whatever the crates round-trip.
    fn v1_wire_transaction(signature: &Signature, payer: &Pubkey) -> EncodedTransaction {
        let mut bytes = vec![0x81];
        // header: 1 required signature, 0 readonly signed, 1 readonly unsigned
        bytes.extend_from_slice(&[1, 0, 1]);
        // config mask: bit 2 set, so a compute unit limit follows the addresses
        bytes.extend_from_slice(&0b100u32.to_le_bytes());
        // lifetime specifier (recent blockhash)
        bytes.extend_from_slice(Hash::new_unique().as_ref());
        bytes.push(1); // one instruction
        bytes.push(2); // two addresses
        bytes.extend_from_slice(payer.as_ref());
        bytes.extend_from_slice(PROGRAM_ID.as_ref());
        // the compute unit limit named by the mask
        bytes.extend_from_slice(&300_000u32.to_le_bytes());
        // instruction header: program at address index 1, no accounts, no data
        bytes.extend_from_slice(&[1, 0, 0, 0]);
        bytes.extend_from_slice(signature.as_ref());

        EncodedTransaction::Binary(
            base64::engine::general_purpose::STANDARD.encode(&bytes),
            solana_transaction_status::TransactionBinaryEncoding::Base64,
        )
    }

    #[tokio::test]
    async fn polled_event_stream_parses_v1_wire_format_tx() {
        let _ = env_logger::try_init();

        struct OneTxProvider {
            signature: String,
            tx: EncodedTransactionWithStatusMeta,
        }

        impl EventRpcProvider for Arc<OneTxProvider> {
            fn get_tx(
                &self,
                _signature: Signature,
            ) -> BoxFuture<SdkResult<EncodedTransactionWithStatusMeta>> {
                ready(Ok(self.tx.clone())).boxed()
            }
            fn get_tx_signatures(
                &self,
                _account: Pubkey,
                _after: Option<Signature>,
                limit: Option<usize>,
            ) -> BoxFuture<SdkResult<Vec<String>>> {
                // the limited call is the initial cursor fetch; serve the tx to the
                // poll loop only, so it is processed exactly once
                let signatures = if limit.is_some() {
                    Vec::new()
                } else {
                    vec![self.signature.clone()]
                };
                ready(Ok(signatures)).boxed()
            }
        }

        let sub_account = Pubkey::new_unique();
        let signature = Signature::new_unique();
        let mut tx = make_transaction(
            sub_account,
            signature,
            Some(vec![
                format!("Program {PROGRAM_ID} invoke [1]"),
                format!(
                    "{PROGRAM_LOG}{}",
                    serialize_event(OrderRecord {
                        ts: 1_700_000_000,
                        user: sub_account,
                        order: Order {
                            order_id: 9,
                            ..Default::default()
                        },
                    })
                ),
                format!("Program {PROGRAM_ID} success"),
            ]),
        );
        // the only difference from a v0 poll: the RPC hands back a v1 payload.
        tx.transaction = v1_wire_transaction(&signature, &sub_account);
        // Assert the decode itself, not just the outcome: the log-walk fallback in
        // `poll_should_parse_velocity_logs` returns true for an UNDECODABLE tx too, so
        // without this the test passes on a crate stack that cannot read v1 at all.
        assert!(
            matches!(
                tx.transaction.decode().map(|t| t.message),
                Some(VersionedMessage::V1(_))
            ),
            "v1 wire payload must deserialize as V1, got: {:?}",
            tx.transaction.decode().map(|t| t.message)
        );
        assert!(poll_should_parse_velocity_logs(&tx.transaction, "sig"));

        let (event_tx, mut event_rx) = channel(16);
        tokio::spawn(
            PolledEventStream {
                cache: Arc::new(RwLock::new(TxSignatureCache::new(16))),
                provider: Arc::new(OneTxProvider {
                    signature: signature.to_string(),
                    tx,
                }),
                sub_account,
                event_tx,
            }
            .stream_fn(),
        );

        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("event before timeout")
            .expect("event");
        assert!(
            matches!(&event, VelocityEvent::OrderCreate { order, .. } if order.order_id == 9),
            "expected the v1 tx's OrderCreate, got: {event:?}"
        );
    }

    #[test]
    fn ignores_malformed_payload_emitted_outside_program_id() {
        let _ = env_logger::try_init();

        let mut malformed = OrderActionRecord::DISCRIMINATOR.to_vec();
        malformed.extend_from_slice(&[0xff; 4]);
        let malformed_b64 = base64::engine::general_purpose::STANDARD.encode(&malformed);

        let user = Pubkey::new_unique();
        let swap = SwapRecord {
            ts: 1_700_000_000,
            user,
            amount_out: 2,
            amount_in: 1,
            out_market_index: 1,
            in_market_index: 0,
            out_oracle_price: 0,
            in_oracle_price: 0,
            fee: 0,
        };

        let other = Pubkey::new_unique();
        let logs = [
            format!("Program {other} invoke [1]"),
            format!("{PROGRAM_DATA}{malformed_b64}"),
            format!("Program {other} success"),
            format!("Program {PROGRAM_ID} invoke [1]"),
            format!("{PROGRAM_DATA}{}", serialize_event(swap)),
            format!("Program {PROGRAM_ID} success"),
        ];

        let events = parse_velocity_logs(logs.iter().map(String::as_str), "sig");
        assert_eq!(
            events,
            vec![VelocityEvent::Swap {
                user,
                amount_in: 1,
                amount_out: 2,
                market_in: 0,
                market_out: 1,
                fee: 0,
                ts: 1_700_000_000,
                signature: "sig".into(),
                tx_idx: 4,
            }]
        );
    }

    /// Make transaction with dummy instruction for velocity program
    fn make_transaction(
        account: Pubkey,
        signature: Signature,
        logs: Option<Vec<String>>,
    ) -> EncodedTransactionWithStatusMeta {
        let mut meta = TransactionStatusMeta::default();
        meta.log_messages = logs;
        VersionedTransactionWithStatusMeta {
            transaction: VersionedTransaction {
                signatures: vec![signature],
                message: VersionedMessage::V0(
                    v0::Message::try_compile(
                        &account,
                        &[Instruction {
                            program_id: constants::PROGRAM_ID,
                            accounts: vec![AccountMeta::new_readonly(constants::PROGRAM_ID, true)],
                            data: Default::default(),
                        }],
                        &[],
                        Hash::new_unique(),
                    )
                    .expect("v0 message"),
                ),
            },
            meta,
        }
        .encode(UiTransactionEncoding::Base64, Some(1), false)
        .unwrap()
    }

    /// serialize event to string like Velocity program log
    pub fn serialize_event<T: AnchorSerialize + Discriminator>(event: T) -> String {
        let mut data_buf = T::DISCRIMINATOR.to_vec();
        event.serialize(&mut data_buf).expect("serializes");
        base64::engine::general_purpose::STANDARD.encode(data_buf)
    }

    pub fn get_order_action_record(
        ts: i64,
        action: OrderAction,
        action_explanation: OrderActionExplanation,
        market_index: u16,
        filler: Option<Pubkey>,
        fill_record_id: Option<u64>,
        filler_reward: Option<u64>,
        base_asset_amount_filled: Option<u64>,
        quote_asset_amount_filled: Option<u64>,
        taker_fee: Option<u64>,
        maker_rebate: Option<u64>,
        referrer_reward: Option<u64>,
        quote_asset_amount_surplus: Option<i64>,
        spot_fulfillment_method_fee: Option<u64>,
        taker: Option<Pubkey>,
        taker_order: Option<Order>,
        maker: Option<Pubkey>,
        maker_order: Option<Order>,
        oracle_price: i64,
        bit_flags: u8,
    ) -> OrderActionRecord {
        OrderActionRecord {
            bit_flags,
            ts,
            action,
            action_explanation,
            market_index,
            market_type: if let Some(taker_order) = taker_order {
                taker_order.market_type
            } else if let Some(maker_order) = maker_order {
                maker_order.market_type
            } else {
                panic!("invalid order");
            },
            filler,
            filler_reward,
            fill_record_id,
            base_asset_amount_filled,
            quote_asset_amount_filled,
            taker_fee,
            maker_fee: match maker_rebate {
                Some(maker_rebate) => Some(maker_rebate as i64),
                None => None,
            },
            referrer_reward: match referrer_reward {
                Some(referrer_reward) if referrer_reward > 0 => {
                    Some(referrer_reward.try_into().unwrap())
                }
                _ => None,
            },
            quote_asset_amount_surplus,
            spot_fulfillment_method_fee,
            taker,
            taker_order_id: taker_order.map(|order| order.order_id),
            taker_order_direction: taker_order.map(|order| order.direction),
            taker_order_base_asset_amount: taker_order.map(|order| order.base_asset_amount),
            taker_order_cumulative_base_asset_amount_filled: taker_order
                .map(|order| order.base_asset_amount_filled),
            taker_order_cumulative_quote_asset_amount_filled: taker_order
                .as_ref()
                .map(|order| order.quote_asset_amount_filled),
            maker,
            maker_order_id: maker_order.map(|order| order.order_id),
            maker_order_direction: maker_order.map(|order| order.direction),
            maker_order_base_asset_amount: maker_order.map(|order| order.base_asset_amount),
            maker_order_cumulative_base_asset_amount_filled: maker_order
                .map(|order| order.base_asset_amount_filled),
            maker_order_cumulative_quote_asset_amount_filled: maker_order
                .map(|order| order.quote_asset_amount_filled),
            oracle_price,
            maker_existing_base_asset_amount: None,
            maker_existing_quote_entry_amount: None,
            taker_existing_base_asset_amount: None,
            taker_existing_quote_entry_amount: None,
            trigger_price: None,
            builder_fee: None,
            builder_idx: None,
        }
    }
}
