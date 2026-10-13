//! The gRPC subscription and account sync that every bot shares
//!
//! Each bot adds its own slot and account callbacks to `subscription_opts` and subscribes with
//! `subscribe`. The startup sync loads user stats and users with open orders over RPC, because
//! the stream only delivers accounts as they change.

use {
    crate::common::tx::TxSender,
    solana_account_decoder_client_types::UiAccountEncoding,
    solana_rpc_client_api::config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    velocity_rs::{
        constants::PROGRAM_ID,
        dlob::DLOBNotifier,
        grpc::{grpc_subscriber::GrpcConnectionOpts, AccountUpdate, TransactionUpdate},
        types::accounts::{User, UserStats},
        GrpcSubscribeOpts, Pubkey, VelocityClient, Wallet,
    },
};

/// The shared gRPC code logs under the filler's target, which existing `RUST_LOG` filters select.
const TARGET: &str = "filler";

/// Forward each streamed tx's signature to the tx worker, which confirms it.
fn on_transaction_update(
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

/// The subscription options every bot shares: processed commitment, compression, the user and
/// stats account maps, and confirmations for the txs that touch `watched`.
///
/// `watched` must list every subaccount the bot sends from. A tx that touches none of them is
/// never streamed, so its confirmation never fires and its pending entry is only cleared when the
/// buffer overwrites it.
pub(crate) fn subscription_opts(watched: Vec<Pubkey>, tx_sender: TxSender) -> GrpcSubscribeOpts {
    GrpcSubscribeOpts::default()
        .commitment(solana_commitment_config::CommitmentLevel::Processed)
        .connection_opts(GrpcConnectionOpts::default().enable_compression())
        .usermap_on()
        .statsmap_on()
        .transaction_include_accounts(watched)
        .on_transaction(on_transaction_update(tx_sender))
}

/// Subscribe at `GRPC_ENDPOINT`, authenticated by `GRPC_X_TOKEN`.
pub(crate) async fn subscribe(velocity: &VelocityClient, opts: GrpcSubscribeOpts) {
    let _res = velocity
        .grpc_subscribe(
            std::env::var("GRPC_ENDPOINT")
                .unwrap_or_else(|_| "https://api.rpcpool.com".to_string()),
            std::env::var("GRPC_X_TOKEN").expect("GRPC_X_TOKEN set"),
            opts,
            true,
        )
        .await;
}

/// Load every user stats account, and every user with open orders into the account cache and
/// the DLOB. A failed sync is logged and the bot starts on what the stream delivers.
pub(crate) async fn sync_accounts(velocity: &VelocityClient, dlob_notifier: &DLOBNotifier) {
    let _ = tokio::try_join!(
        sync_stats_accounts(velocity),
        sync_user_accounts(velocity, dlob_notifier),
    );
}

/// Fetch a fill counterparty's user account and stats from the local cache.
///
/// Returns `None` (with a warn log) when either is missing — the account may have been
/// closed between the DLOB snapshot and now, or its stats subscription hasn't landed yet;
/// callers skip the fill rather than panic. Shared by the auction, uncross, and
/// amm-taker fill paths.
pub(crate) fn fetch_user_and_stats(
    velocity: &VelocityClient,
    subaccount: &Pubkey,
    context: &str,
) -> Option<(User, UserStats)> {
    let Ok(user) = velocity.try_get_account::<User>(subaccount) else {
        log::warn!(target: TARGET, "{context}: user account {subaccount} not in cache, skipping");
        return None;
    };
    match velocity.try_get_account::<UserStats>(&Wallet::derive_stats_account(&user.authority)) {
        Ok(stats) => Some((user, stats)),
        Err(_) => {
            log::warn!(target: TARGET, "{context}: failed to fetch user stats: {:?}, skipping", user.authority);
            None
        }
    }
}

async fn sync_stats_accounts(
    velocity: &VelocityClient,
) -> Result<(), solana_rpc_client_api::client_error::Error> {
    let stats_sync_result = fetch_program_accounts(
        velocity,
        RpcProgramAccountsConfig {
            filters: Some(vec![velocity_rs::memcmp::get_user_stats_filter()]),
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64Zstd),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await;

    match stats_sync_result {
        Ok(accounts) => {
            for (pubkey, account) in accounts {
                velocity.backend().account_map().on_account_fn()(&AccountUpdate {
                    pubkey,
                    data: &account.data,
                    lamports: account.lamports,
                    owner: PROGRAM_ID,
                    rent_epoch: u64::MAX,
                    executable: false,
                    slot: 0,
                    write_version: 0,
                });
            }
            log::info!(target: "dlob", "syncd stats accounts");
            Ok(())
        }
        Err(err) => {
            log::error!(target: "dlob", "dlob sync error: {err:?}");
            Err(err)
        }
    }
}

async fn sync_user_accounts(
    velocity: &VelocityClient,
    dlob_notifier: &DLOBNotifier,
) -> Result<(), solana_rpc_client_api::client_error::Error> {
    let sync_result = fetch_program_accounts(
        velocity,
        RpcProgramAccountsConfig {
            filters: Some(vec![
                velocity_rs::memcmp::get_non_idle_user_filter(),
                velocity_rs::memcmp::get_user_filter(),
            ]),
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64Zstd),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await;

    match sync_result {
        Ok(accounts) => {
            for (pubkey, account) in accounts {
                let user = velocity_rs::utils::deser_zero_copy::<User>(&account.data);
                dlob_notifier.user_update(pubkey, None, &user, 0);
                velocity.backend().account_map().on_account_fn()(&AccountUpdate {
                    pubkey,
                    data: &account.data,
                    lamports: account.lamports,
                    owner: PROGRAM_ID,
                    rent_epoch: u64::MAX,
                    executable: false,
                    slot: 0,
                    write_version: 0,
                });
            }
            log::info!(target: "dlob", "synced initial orders");
            Ok(())
        }
        Err(err) => {
            log::error!(target: "dlob", "dlob sync error: {err:?}");
            Err(err)
        }
    }
}

/// `RpcClient::get_program_accounts_with_config` was removed in solana-rpc-client 4.2.
/// Decode `UiAccount`s from the replacement so callers still see binary `Account` data.
///
/// Anza's own helper panics on an account it cannot decode. This one runs on the
/// filler and liquidator startup path, where an RPC that ignores the requested
/// `Base64Zstd` encoding should degrade like any other sync failure, so skip those
/// accounts and report how many were lost.
async fn fetch_program_accounts(
    velocity: &VelocityClient,
    config: RpcProgramAccountsConfig,
) -> Result<Vec<(Pubkey, solana_account::Account)>, solana_rpc_client_api::client_error::Error> {
    let ui_accounts = velocity
        .rpc()
        .get_program_ui_accounts_with_config(&PROGRAM_ID, config)
        .await?;
    let returned = ui_accounts.len();
    let accounts: Vec<(Pubkey, solana_account::Account)> = ui_accounts
        .into_iter()
        .filter_map(|(pubkey, ui)| ui.to_account().map(|account| (pubkey, account)))
        .collect();
    if accounts.len() < returned {
        log::warn!(
            target: "dlob",
            "skipped {} of {returned} program accounts: not returned in a binary encoding",
            returned - accounts.len()
        );
    }

    Ok(accounts)
}
