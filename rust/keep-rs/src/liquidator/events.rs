//! The liquidator's gRPC subscription
//!
//! The run loop needs every user, perp market, spot market and oracle update to keep margin
//! current, so the callbacks decode each update and forward it as a `GrpcEvent` on one channel.
//! Each slot refreshes the DLOB's oracle prices, which the maker search reads. A decode or lookup
//! that keeps failing panics the gRPC thread: its senders drop, the run loop sees the closed
//! channel and exits, and the process restarts rather than run on frozen data.

use {
    crate::{
        common::{
            grpc::{subscribe, subscription_opts, sync_accounts},
            oracle::{chain_safe_price, ExchangeState},
            tx::TxSender,
        },
        liquidator::TARGET,
    },
    anchor_lang::Discriminator,
    solana_clock::Slot,
    std::{
        collections::HashMap,
        sync::atomic::{AtomicU32, Ordering},
    },
    velocity_rs::{
        dlob::DLOBNotifier,
        grpc::grpc_subscriber::AccountFilter,
        types::{
            accounts::{PerpMarket, SpotMarket, User},
            MarketId, OraclePriceData, OracleSource,
        },
        Pubkey, VelocityClient,
    },
};

/// Consecutive failures of a gRPC callback's decode or lookup before it panics.
const GRPC_CALLBACK_FAILURE_LIMIT: u32 = 1_000;

// Sent through a channel on every gRPC update; boxing the large variants would add an allocation
// per event. Variant names match the update kinds and would collide with the market types.
#[allow(clippy::large_enum_variant, clippy::enum_variant_names)]
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

/// Sync accounts over RPC, then subscribe. `subaccounts` are the liquidator's own, so their txs
/// stream back for confirmation.
pub(super) async fn subscribe_events(
    velocity: VelocityClient,
    dlob_notifier: DLOBNotifier,
    tx_sender: TxSender,
    market_ids: Vec<MarketId>,
    subaccounts: Vec<Pubkey>,
) -> tokio::sync::mpsc::Receiver<GrpcEvent> {
    let (events, events_rx) = tokio::sync::mpsc::channel(102400);
    sync_accounts(&velocity, &dlob_notifier).await;

    let mut oracle_to_market = HashMap::<Pubkey, Vec<(MarketId, OracleSource)>>::default();
    for (market, (oracle, source)) in velocity.backend().oracle_map().oracle_by_market.iter() {
        oracle_to_market
            .entry(*oracle)
            .or_default()
            .push((*market, *source));
    }
    log::info!(target: TARGET, "oracle map has {} oracles", oracle_to_market.len());

    let opts = subscription_opts(subaccounts, tx_sender)
        .on_slot(on_slot_update(dlob_notifier, velocity.clone(), &market_ids))
        .on_account(
            AccountFilter::partial().with_discriminator(User::DISCRIMINATOR),
            {
                let events = events.clone();
                move |acc| {
                    let user = velocity_rs::utils::deser_zero_copy::<User>(acc.data);
                    forward(&events, GrpcEvent::UserUpdate {
                        pubkey: acc.pubkey,
                        user,
                        slot: acc.slot,
                    });
                }
            },
        )
        .on_account(
            AccountFilter::partial().with_discriminator(PerpMarket::DISCRIMINATOR),
            {
                let events = events.clone();
                move |acc| {
                    let market = velocity_rs::utils::deser_zero_copy::<PerpMarket>(acc.data);
                    forward(&events, GrpcEvent::PerpMarketUpdate {
                        market,
                        slot: acc.slot,
                    });
                }
            },
        )
        .on_account(
            AccountFilter::partial().with_discriminator(SpotMarket::DISCRIMINATOR),
            {
                let events = events.clone();
                move |acc| {
                    let market = velocity_rs::utils::deser_zero_copy::<SpotMarket>(acc.data);
                    forward(&events, GrpcEvent::SpotMarketUpdate {
                        market,
                        slot: acc.slot,
                    });
                }
            },
        )
        .on_oracle_update({
            let consecutive_failures = AtomicU32::new(0);
            move |acc| {
                let Some(oracle_markets) = oracle_to_market.get(&acc.pubkey) else {
                    log::warn!(target: TARGET, "update for unknown oracle: pubkey={:?} slot={}", acc.pubkey, acc.slot);
                    return;
                };
                for (market, oracle_source) in oracle_markets {
                    let mut data = acc.data.to_vec();
                    let mut lamports = acc.lamports;
                    let account_info = anchor_lang::prelude::AccountInfo::new(
                        &acc.pubkey,
                        false,
                        false,
                        &mut lamports,
                        &mut data,
                        &acc.owner,
                        false,
                    );
                    match velocity_rs::program::state::oracle::get_oracle_price(
                        oracle_source,
                        &account_info,
                        acc.slot,
                    ) {
                        Ok(oracle_price_data) => {
                            consecutive_failures.store(0, Ordering::Relaxed);
                            forward(&events, GrpcEvent::OracleUpdate {
                                oracle_price_data,
                                market: *market,
                                slot: acc.slot,
                            });
                        }
                        Err(err) => {
                            let fails = consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
                            log::warn!(
                                target: TARGET,
                                "oracle price decode failed: market={market:?} oracle={:?} source={oracle_source:?} slot={} consecutive_failures={fails} error={err:?}",
                                acc.pubkey,
                                acc.slot,
                            );
                            assert!(
                                fails < GRPC_CALLBACK_FAILURE_LIMIT,
                                "oracle decode persistently failing, restarting"
                            );
                        }
                    }
                }
            }
        });
    subscribe(&velocity, opts).await;

    events_rx
}

fn forward(events: &tokio::sync::mpsc::Sender<GrpcEvent>, event: GrpcEvent) {
    if let Err(err) = events.try_send(event) {
        log::error!(target: TARGET, "failed to forward grpc event: {err:?}");
    }
}

/// Keep the DLOB's slot clock and oracle prices current on every slot.
fn on_slot_update(
    dlob_notifier: DLOBNotifier,
    velocity: VelocityClient,
    market_ids: &[MarketId],
) -> impl Fn(u64) + Send + Sync + 'static {
    let market_ids: Vec<MarketId> = market_ids.to_vec();
    let consecutive_failures = AtomicU32::new(0);
    move |new_slot| {
        // a no-op unless a slot duration transition was synchronized since the last slot
        // one `State` read serves the slot clock and every market this slot
        let exchange = ExchangeState::load(&velocity);
        dlob_notifier.slot_clock_update(
            exchange
                .as_ref()
                .map_or_else(|| velocity.slot_clock(), |exchange| exchange.slot_clock),
        );
        for market in market_ids.iter() {
            let price = exchange.as_ref().and_then(|exchange| {
                chain_safe_price(&velocity, exchange, market.index(), new_slot)
            });
            match price {
                Some(price) => {
                    consecutive_failures.store(0, Ordering::Relaxed);
                    dlob_notifier.slot_and_oracle_update(*market, new_slot, price as u64);
                }
                None => {
                    let fails = consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
                    log::warn!(
                        target: TARGET,
                        "safe oracle lookup failed: market={market:?} slot={new_slot} consecutive_failures={fails}"
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
