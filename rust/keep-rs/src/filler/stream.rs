//! The filler's gRPC subscription
//!
//! The filler streams its own fill txs for confirmation, every user account into the DLOB, and
//! slots. Each slot refreshes the DLOB's oracle prices and is forwarded to the run loop. A
//! market whose oracle stays missing from the cache exits the process for a restart.

use {
    crate::{
        common::{
            grpc::{subscribe, subscription_opts, sync_accounts},
            oracle::{chain_safe_price, ExchangeState},
            tx::TxSender,
        },
        filler::TARGET,
    },
    anchor_lang::Discriminator,
    std::collections::BTreeMap,
    velocity_rs::{
        dlob::{DLOBNotifier, DLOB},
        grpc::{grpc_subscriber::AccountFilter, AccountUpdate},
        types::{accounts::User, MarketId},
        Pubkey, VelocityClient,
    },
};

/// Max consecutive slots a market's oracle may be missing before the process exits.
///
/// A panic here is NOT protective: this closure runs on the gRPC dispatch thread, and a
/// thread panic doesn't stop the process — the bot would keep running with a frozen book
/// (zombie). A transient miss is skipped and retried next slot; a persistent one exits the
/// process so the supervisor restarts it with fresh subscriptions.
// ~2min of slot ticks at 400ms (shrinks in wall-clock as slot time drops —
// deliberate: this is a dead-feed restart tripwire, firing sooner is fine)
pub(super) const MAX_CONSECUTIVE_ORACLE_MISSES: u32 = 300;

fn on_slot_update(
    velocity: VelocityClient,
    market_ids: Vec<MarketId>,
    dlob_notifier: DLOBNotifier,
    slot_tx: tokio::sync::mpsc::Sender<u64>,
) -> impl Fn(u64) + Send + Sync + 'static {
    // single gRPC dispatch thread: the mutex is uncontended
    let consecutive_misses_ref = std::sync::Mutex::new(BTreeMap::<u16, u32>::new());
    move |new_slot| {
        // one `State` read serves every market this slot
        let exchange = ExchangeState::load(&velocity);
        for market in market_ids.iter() {
            // a transiently missing oracle must not kill the gRPC dispatch thread;
            // skip the market this slot and let the next tick retry
            let Some(price) = exchange.as_ref().and_then(|exchange| {
                chain_safe_price(&velocity, exchange, market.index(), new_slot)
            }) else {
                let mut misses = consecutive_misses_ref.lock().unwrap();
                let count = misses.entry(market.index()).or_insert(0);
                *count += 1;
                log::warn!(target: TARGET, "no oracle price for market {} ({count} consecutive), skipping slot update", market.index());
                if *count >= MAX_CONSECUTIVE_ORACLE_MISSES {
                    log::error!(target: TARGET, "oracle for market {} missing for {count} consecutive slots, exiting for restart", market.index());
                    std::process::exit(1);
                }
                continue;
            };
            consecutive_misses_ref
                .lock()
                .unwrap()
                .insert(market.index(), 0);
            dlob_notifier.slot_and_oracle_update(*market, new_slot, price as u64);
        }
        if let Err(err) = slot_tx.try_send(new_slot) {
            log::debug!(target: TARGET, "failed slot update: {err:?}");
        }
    }
}

fn on_account_update(
    dlob_notifier: DLOBNotifier,
    velocity: VelocityClient,
) -> impl Fn(&AccountUpdate) + Send + Sync + 'static {
    move |update| {
        // Skip closed / empty-data updates rather than panic on `&data[8..]`.
        let Some(new_user) = velocity_rs::utils::try_deser_zero_copy::<User>(update.data) else {
            if update.lamports == 0 {
                // account closed/deleted: diff its last known state against an empty
                // account so its open orders are removed from the book
                if let Some(old_user) = velocity
                    .backend()
                    .account_map()
                    .account_data_and_slot::<User>(&update.pubkey)
                {
                    dlob_notifier.user_update(
                        update.pubkey,
                        Some(&old_user.data),
                        &User::default(),
                        update.slot,
                    );
                }
            }
            return;
        };
        // this hook runs before the account_map write and diffs against the map's old
        // copy, so it must skip exactly the updates the map skips: feeding the book an
        // update the map drops leaves `old_user` behind and strands orders (BE-592)
        let account_map = velocity.backend().account_map();
        if account_map.is_stale(&update.pubkey, update.slot) {
            log::debug!(
                target: TARGET,
                "skip stale user update: {} slot={}",
                update.pubkey,
                update.slot
            );
            return;
        }
        let existing = account_map.account_data_and_slot::<User>(&update.pubkey);
        dlob_notifier.user_update(
            update.pubkey,
            existing.as_ref().map(|x| &x.data),
            &new_user,
            update.slot,
        );
    }
}

/// Setup gRPC subscriptions
///
/// Syncs User orders and UserStat accounts
pub(super) async fn setup_grpc(
    velocity: VelocityClient,
    dlob: &'static DLOB,
    tx_worker_ref: TxSender,
    market_ids: Vec<MarketId>,
    filler_subaccount: Pubkey,
) -> tokio::sync::mpsc::Receiver<u64> {
    let dlob_notifier = dlob.spawn_notifier();
    sync_accounts(&velocity, &dlob_notifier).await;

    let (slot_tx, slot_rx) = tokio::sync::mpsc::channel(64);

    subscribe_filler_streams(
        velocity,
        dlob_notifier,
        slot_tx,
        tx_worker_ref,
        market_ids,
        filler_subaccount,
    )
    .await;

    slot_rx
}

async fn subscribe_filler_streams(
    velocity: VelocityClient,
    dlob_notifier: DLOBNotifier,
    slot_tx: tokio::sync::mpsc::Sender<u64>,
    tx_sender: TxSender,
    market_ids: Vec<MarketId>,
    filler_subaccount: Pubkey,
) {
    let opts = subscription_opts(vec![filler_subaccount], tx_sender)
        .on_slot(on_slot_update(
            velocity.clone(),
            market_ids,
            dlob_notifier.clone(),
            slot_tx,
        ))
        .on_account(
            AccountFilter::partial().with_discriminator(User::DISCRIMINATOR),
            on_account_update(dlob_notifier, velocity.clone()),
        );
    subscribe(&velocity, opts).await;
}
