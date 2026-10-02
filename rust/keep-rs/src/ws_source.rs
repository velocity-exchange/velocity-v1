//! The liquidator's account events from RPC websockets instead of Yellowstone gRPC.
//!
//! A local validator has no geyser plugin, so the gRPC subscription cannot run there. This
//! source sends the same [`GrpcEvent`]s and feeds the client's account map the same user
//! updates. It has no transaction stream, so a sent liquidation leaves the in-flight set only
//! when the stale-transaction sweep clears it.

use {
    crate::liquidator::GrpcEvent,
    futures_util::StreamExt,
    solana_account_decoder_client_types::UiAccountEncoding,
    solana_rpc_client_api::config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    std::collections::HashMap,
    tokio::sync::mpsc::Sender,
    velocity_rs::{
        grpc::AccountUpdate,
        program::state::{
            oracle::{get_oracle_price, OracleSource},
            perp_market::PerpMarket,
            spot_market::SpotMarket,
            user::User,
        },
        types::{AccountUpdate as WsAccountUpdate, MarketId},
        utils::{deser_zero_copy, get_ws_url},
        Pubkey, PubsubClient, VelocityClient,
    },
};

const TARGET: &str = "liquidator";

/// Start the websocket subscriptions and return the event channel the liquidator reads.
pub async fn setup_websocket(velocity: VelocityClient) -> tokio::sync::mpsc::Receiver<GrpcEvent> {
    let (tx, rx) = tokio::sync::mpsc::channel(102400);

    let _ = tokio::try_join!(
        crate::filler::sync_stats_accounts(&velocity),
        crate::filler::sync_user_accounts(&velocity),
    );

    let perp_tx = tx.clone();
    velocity
        .subscribe_all_perp_markets_with_callback(move |update| {
            let market = deser_zero_copy::<PerpMarket>(&update.data);
            forward(
                &perp_tx,
                GrpcEvent::PerpMarketUpdate {
                    market,
                    slot: update.slot,
                },
            );
        })
        .await
        .expect("perp markets subscribed");

    let spot_tx = tx.clone();
    velocity
        .subscribe_all_spot_markets_with_callback(move |update| {
            let market = deser_zero_copy::<SpotMarket>(&update.data);
            forward(
                &spot_tx,
                GrpcEvent::SpotMarketUpdate {
                    market,
                    slot: update.slot,
                },
            );
        })
        .await
        .expect("spot markets subscribed");

    let oracle_markets = oracle_markets(&velocity);
    let oracle_tx = tx.clone();
    velocity
        .subscribe_all_oracles_with_callback(move |update| {
            for event in oracle_events(&oracle_markets, update) {
                forward(&oracle_tx, event);
            }
        })
        .await
        .expect("oracles subscribed");

    let ws_url = get_ws_url(&velocity.rpc().url()).expect("websocket url from the rpc url");
    tokio::spawn(stream_users(velocity, ws_url, tx));
    rx
}

fn forward(tx: &Sender<GrpcEvent>, event: GrpcEvent) {
    if let Err(err) = tx.try_send(event) {
        log::error!(target: TARGET, "failed to forward websocket event: {err:?}");
    }
}

fn oracle_markets(velocity: &VelocityClient) -> HashMap<Pubkey, Vec<(MarketId, OracleSource)>> {
    let mut markets = HashMap::<Pubkey, Vec<(MarketId, OracleSource)>>::default();
    for (market, (oracle, source)) in velocity.backend().oracle_map().oracle_by_market.iter() {
        markets.entry(*oracle).or_default().push((*market, *source));
    }

    markets
}

/// One oracle update for every market that reads the oracle. A price that does not decode is
/// logged and skipped.
fn oracle_events(
    oracle_markets: &HashMap<Pubkey, Vec<(MarketId, OracleSource)>>,
    update: &WsAccountUpdate,
) -> Vec<GrpcEvent> {
    let Some(markets) = oracle_markets.get(&update.pubkey) else {
        return vec![];
    };

    markets
        .iter()
        .filter_map(|(market, source)| {
            let (mut lamports, mut data) = (update.lamports, update.data.clone());
            let account_info = anchor_lang::prelude::AccountInfo::new(
                &update.pubkey,
                false,
                false,
                &mut lamports,
                &mut data,
                &update.owner,
                false,
            );
            match get_oracle_price(source, &account_info, update.slot) {
                Ok(oracle_price_data) => Some(GrpcEvent::OracleUpdate {
                    oracle_price_data,
                    market: *market,
                    slot: update.slot,
                }),
                Err(err) => {
                    log::warn!(target: TARGET, "oracle price decode failed: market={market:?} error={err:?}");
                    None
                }
            }
        })
        .collect()
}

/// Every `User` write, reconnecting when the stream drops.
async fn stream_users(velocity: VelocityClient, ws_url: String, tx: Sender<GrpcEvent>) {
    let config = RpcProgramAccountsConfig {
        filters: Some(vec![velocity_rs::memcmp::get_user_filter()]),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            ..Default::default()
        },
        ..Default::default()
    };

    loop {
        let pubsub = match PubsubClient::new(&ws_url).await {
            Ok(pubsub) => pubsub,
            Err(err) => {
                log::error!(target: TARGET, "user stream connect failed: {err:?}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        let Ok((mut updates, _unsubscribe)) = pubsub
            .program_subscribe(&velocity_rs::constants::PROGRAM_ID, Some(config.clone()))
            .await
        else {
            log::error!(target: TARGET, "user stream subscribe failed");
            continue;
        };

        while let Some(message) = updates.next().await {
            let Some(data) = message.value.account.data.decode() else {
                continue;
            };

            let Ok(pubkey) = message.value.pubkey.parse::<Pubkey>() else {
                continue;
            };

            let slot = message.context.slot;
            velocity.backend().account_map().on_account_fn()(&AccountUpdate {
                pubkey,
                owner: velocity_rs::constants::PROGRAM_ID,
                data: &data,
                lamports: message.value.account.lamports,
                rent_epoch: u64::MAX,
                slot,
                write_version: 0,
                executable: false,
            });

            let user = deser_zero_copy::<User>(&data);
            forward(&tx, GrpcEvent::UserUpdate { pubkey, user, slot });
        }

        log::warn!(target: TARGET, "user stream ended, reconnecting");
    }
}
