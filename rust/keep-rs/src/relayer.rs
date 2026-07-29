//! Pyth Lazer oracle relayer
//!
//! Subscribes to Lazer price feeds for the configured perp (and optionally
//! spot) markets, and posts each price update on-chain via Velocity's
//! `post_pyth_lazer_oracle_update` instruction.
use {
    crate::{Config, UseMarkets},
    std::{
        borrow::Cow,
        collections::HashMap,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    },
    tokio::time::sleep,
    velocity_rs::{
        math::liquidation::calculate_collateral,
        types::{
            accounts::User, MarginRequirementType, MarketId, RpcSendTransactionConfig,
            SpotBalanceType,
        },
        TransactionBuilder, VelocityClient, Wallet,
    },
};

const TARGET: &str = "relayer";
const CU_LIMIT: u32 = 80_000;
const SUMMARY_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Stats {
    sent: AtomicU64,
    failed: AtomicU64,
    skipped_throttle: AtomicU64,
}

pub async fn run(config: Config, velocity: VelocityClient) {
    let perp_market_ids = match config.use_markets() {
        UseMarkets::All => velocity.get_all_perp_market_ids(),
        UseMarkets::Subset(m) => m,
    };
    let spot_market_ids: Vec<MarketId> = if config.use_spot_liquidation {
        velocity
            .program_data()
            .spot_market_configs()
            .iter()
            .map(|m| MarketId::spot(m.market_index))
            .collect()
    } else {
        Vec::new()
    };

    let min_interval = Duration::from_millis(config.relayer_min_interval_ms);
    let extra_feeds: Vec<u32> = config
        .relayer_extra_feeds
        .split(',')
        .filter_map(|s| s.trim().parse::<u32>().ok())
        .collect();
    log::info!(
        target: TARGET,
        "starting: perp_markets={} spot_markets={} extra_feeds={:?} min_interval={:?} dry={}",
        perp_market_ids.len(),
        spot_market_ids.len(),
        extra_feeds,
        min_interval,
        config.dry,
    );

    let pyth_access_token = std::env::var("PYTH_LAZER_TOKEN").expect("PYTH_LAZER_TOKEN set");
    let pyth_feed_cli = pyth_lazer_client::LazerClient::new(
        "wss://pyth-lazer.dourolabs.app/v1/stream",
        pyth_access_token.as_str(),
    )
    .expect("pyth lazer client connects");
    let mut feed = crate::util::subscribe_price_feeds(
        pyth_feed_cli,
        &perp_market_ids,
        &spot_market_ids,
        &extra_feeds,
    );

    velocity
        .subscribe_blockhashes()
        .await
        .expect("subscribed blockhashes");

    let subaccount = velocity.wallet.sub_account(config.sub_account_id);
    let user = velocity
        .get_user_account(&subaccount)
        .await
        .expect("bot subaccount exists (run --init-user first)");

    let velocity: &'static VelocityClient = Box::leak(Box::new(velocity));
    let user: &'static User = Box::leak(Box::new(user));
    let stats = Arc::new(Stats::default());

    log::info!(target: TARGET, "subaccount={subaccount} authority={}", velocity.wallet.authority());

    {
        let stats = Arc::clone(&stats);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(SUMMARY_INTERVAL);
            interval.tick().await; // skip the immediate tick
            loop {
                interval.tick().await;
                log::info!(
                    target: TARGET,
                    "stats: sent={} failed={} throttled={}",
                    stats.sent.load(Ordering::Relaxed),
                    stats.failed.load(Ordering::Relaxed),
                    stats.skipped_throttle.load(Ordering::Relaxed),
                );
            }
        });
    }

    // Dedup + throttle: each pyth message can fan out to multiple market events,
    // and Lazer can stream sub-millisecond ticks. Cap to one tx per feed per
    // `min_interval`, and never re-send the same payload timestamp.
    let mut last_sent_ts: HashMap<u32, u64> = HashMap::new();
    let mut last_sent_at: HashMap<u32, Instant> = HashMap::new();

    while let Some(update) = feed.recv().await {
        let ts_us = update.ts.0;
        if last_sent_ts.get(&update.feed_id).copied() == Some(ts_us) {
            continue;
        }
        if let Some(prev) = last_sent_at.get(&update.feed_id) {
            if prev.elapsed() < min_interval {
                stats.skipped_throttle.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        }
        last_sent_ts.insert(update.feed_id, ts_us);
        last_sent_at.insert(update.feed_id, Instant::now());

        if config.dry {
            log::info!(
                target: TARGET,
                "[dry] feed={} mid={} type={:?} price={}",
                update.feed_id,
                update.market_id,
                update.market_type,
                update.price,
            );
            continue;
        }

        let tx = TransactionBuilder::new(
            velocity.program_data(),
            subaccount,
            Cow::Borrowed(user),
            false,
        )
        .with_priority_fee(config.priority_fee, Some(CU_LIMIT))
        .post_pyth_lazer_oracle_update(&[update.feed_id], &update.message)
        .build();

        let stats = Arc::clone(&stats);
        tokio::spawn(async move {
            let blockhash = match velocity.get_latest_blockhash().await {
                Ok(b) => b,
                Err(e) => {
                    stats.failed.fetch_add(1, Ordering::Relaxed);
                    log::warn!(target: TARGET, "feed={} blockhash error: {e}", update.feed_id);
                    return;
                }
            };
            let signed = match velocity.wallet().sign_tx(tx, blockhash) {
                Ok(t) => t,
                Err(e) => {
                    stats.failed.fetch_add(1, Ordering::Relaxed);
                    log::warn!(target: TARGET, "feed={} sign error: {e}", update.feed_id);
                    return;
                }
            };
            let cfg = RpcSendTransactionConfig {
                skip_preflight: true,
                max_retries: Some(0),
                ..Default::default()
            };
            match velocity
                .rpc()
                .send_transaction_with_config(&signed, cfg)
                .await
            {
                Ok(sig) => {
                    stats.sent.fetch_add(1, Ordering::Relaxed);
                    log::info!(
                        target: TARGET,
                        "sent feed={} mid={} price={} sig={}",
                        update.feed_id,
                        update.market_id,
                        update.price,
                        sig,
                    );
                }
                Err(e) => {
                    stats.failed.fetch_add(1, Ordering::Relaxed);
                    log::warn!(target: TARGET, "send failed feed={}: {e}", update.feed_id);
                }
            }
        });
    }

    log::warn!(target: TARGET, "pyth feed channel closed; exiting relayer");
}

/// Preflight: ensure every velocity sub-account the bot will use exists,
/// creating any that are missing. Covers both the filler's `--sub-account-id`
/// and the liquidator's `--subaccounts` list (the liquidator picks its
/// take-over account from that list, so a missing one makes liquidations fail
/// with `AccountNotFound`). Idempotent — already-initialized subaccounts are
/// skipped — so callers can run it on every startup before the bot loop.
pub async fn init_user(config: Config, velocity: VelocityClient) {
    let authority = *velocity.wallet.authority();

    // Union of the single filler id and the liquidator subaccount list.
    // Ascending order matters: sub-account 0 carries the one-time UserStats
    // init, which every non-zero `InitializeUser` depends on.
    let mut ids: Vec<u16> = std::iter::once(config.sub_account_id)
        .chain(config.get_subaccounts())
        .collect();
    ids.sort_unstable();
    ids.dedup();

    // A non-zero subaccount can only be created once UserStats exists, and
    // UserStats is created alongside sub-account 0. If stats is missing, force
    // sub-account 0 to be created first even if the operator didn't list it.
    if velocity.get_user_stats(&authority).await.is_err() && !ids.contains(&0) {
        log::info!(target: TARGET, "UserStats missing; will initialize sub-account 0 first");
        ids.insert(0, 0);
    }

    for id in ids {
        let subaccount = velocity.wallet.sub_account(id);
        if velocity.get_user_account(&subaccount).await.is_ok() {
            log::info!(target: TARGET, "subaccount id={id} ({subaccount}) already exists; skipping");
            continue;
        }
        if let Err(e) = init_one_subaccount(&config, &velocity, id).await {
            // Stop the preflight: later (non-zero) inits depend on this one
            // having landed (UserStats / ordering), so don't push on blindly.
            log::error!(target: TARGET, "failed to initialize subaccount id={id}: {e}");
            return;
        }
    }
}

/// Create a single sub-account and wait until it is visible on-chain, so a
/// dependent (non-zero) init later in the same preflight sees the UserStats /
/// account it needs.
async fn init_one_subaccount(
    config: &Config,
    velocity: &VelocityClient,
    sub_account_id: u16,
) -> Result<(), String> {
    let subaccount = velocity.wallet.sub_account(sub_account_id);
    log::info!(target: TARGET, "initializing subaccount id={sub_account_id} pubkey={subaccount}");

    // TransactionBuilder reads authority from the User; set it to our wallet
    // so signing matches.
    let placeholder = User {
        authority: *velocity.wallet.authority(),
        ..User::default()
    };
    let tx = TransactionBuilder::new(
        velocity.program_data(),
        subaccount,
        Cow::Owned(placeholder),
        false,
    )
    .with_priority_fee(config.priority_fee, Some(200_000))
    .initialize_user_account(sub_account_id, None, None)
    .build();

    let blockhash = velocity
        .get_latest_blockhash()
        .await
        .map_err(|e| format!("fetch blockhash: {e}"))?;
    let signed = velocity
        .wallet()
        .sign_tx(tx, blockhash)
        .map_err(|e| format!("sign tx: {e}"))?;
    let cfg = RpcSendTransactionConfig {
        skip_preflight: false,
        ..Default::default()
    };
    let sig = velocity
        .rpc()
        .send_transaction_with_config(&signed, cfg)
        .await
        .map_err(|e| format!("send tx: {e}"))?;
    log::info!(target: TARGET, "init user id={sub_account_id} submitted: sig={sig}");

    // Confirm the account is materialized before returning, so a dependent init
    // in the same preflight doesn't race ahead of UserStats creation.
    for _ in 0..30 {
        sleep(Duration::from_millis(1_000)).await;
        if velocity.get_user_account(&subaccount).await.is_ok() {
            log::info!(target: TARGET, "subaccount id={sub_account_id} confirmed on-chain");
            return Ok(());
        }
    }
    Err(format!(
        "subaccount {subaccount} not visible on-chain within timeout after init"
    ))
}

/// Preflight: sweep idle wallet token balances into the liquidator's take-over
/// subaccount (the first id in `--subaccounts`) when it has no free
/// collateral. An unfunded liquidator skips every perp liquidation with
/// `no_free_collateral`, so tokens left sitting in the authority wallet are
/// dead weight — deposit them at startup instead. Idempotent: once the
/// subaccount has free collateral (or the wallet ATAs are empty) this is a
/// no-op. Disable with `--no-auto-deposit`.
pub async fn auto_deposit_idle_funds(config: &Config, velocity: &VelocityClient) {
    let authority = *velocity.wallet.authority();
    let Some(&sub_account_id) = config.get_subaccounts().first() else {
        log::warn!(target: TARGET, "auto-deposit: no subaccounts configured; skipping");
        return;
    };
    let subaccount = velocity.wallet.sub_account(sub_account_id);

    let user_account = match velocity.get_user_account(&subaccount).await {
        Ok(user) => user,
        Err(e) => {
            log::warn!(
                target: TARGET,
                "auto-deposit: could not load subaccount id={sub_account_id} ({subaccount}): {e:?}; skipping (run --init-user first)"
            );
            return;
        }
    };

    // "Needs funds" = no free (initial) collateral. Market/oracle caches may
    // not be warm this early in startup; if the margin calc fails, fall back
    // to treating a subaccount with no deposit positions as unfunded.
    let needs_funds = match calculate_collateral(
        velocity,
        &user_account,
        MarginRequirementType::Initial,
    ) {
        Ok(info) => info.free <= 0,
        Err(e) => {
            log::debug!(
                target: TARGET,
                "auto-deposit: collateral calc unavailable ({e:?}); falling back to deposit-position check"
            );
            !user_account
                .spot_positions
                .iter()
                .any(|p| matches!(p.balance_type, SpotBalanceType::Deposit) && !p.is_available())
        }
    };
    if !needs_funds {
        log::debug!(
            target: TARGET,
            "auto-deposit: subaccount id={sub_account_id} already has free collateral; skipping"
        );
        return;
    }

    for spot_market in velocity.program_data().spot_market_configs() {
        if spot_market.has_transfer_hook() {
            log::info!(
                target: TARGET,
                "auto-deposit: skipping spot market {} (transfer hook token)",
                spot_market.market_index
            );
            continue;
        }
        let ata = Wallet::derive_associated_token_address(&authority, spot_market);
        let wallet_balance: u64 = match velocity.rpc().get_token_account_balance(&ata).await {
            Ok(balance) => balance.amount.parse().unwrap_or(0),
            // no ATA for this mint
            Err(_) => continue,
        };
        if wallet_balance == 0 {
            continue;
        }

        if config.dry {
            log::info!(
                target: TARGET,
                "auto-deposit (dry run): would deposit {wallet_balance} into spot market {} for subaccount id={sub_account_id}",
                spot_market.market_index
            );
            continue;
        }

        let tx = TransactionBuilder::new(
            velocity.program_data(),
            subaccount,
            Cow::Borrowed(&user_account),
            false,
        )
        .with_priority_fee(config.priority_fee, Some(200_000))
        .deposit(wallet_balance, spot_market.market_index, None, None)
        .build();

        let blockhash = match velocity.get_latest_blockhash().await {
            Ok(b) => b,
            Err(e) => {
                log::error!(target: TARGET, "auto-deposit: fetch blockhash failed: {e:?}");
                return;
            }
        };
        let signed = match velocity.wallet().sign_tx(tx, blockhash) {
            Ok(s) => s,
            Err(e) => {
                log::error!(target: TARGET, "auto-deposit: sign tx failed: {e:?}");
                return;
            }
        };
        let cfg = RpcSendTransactionConfig {
            skip_preflight: false,
            ..Default::default()
        };
        match velocity
            .rpc()
            .send_transaction_with_config(&signed, cfg)
            .await
        {
            Ok(sig) => {
                log::info!(
                    target: TARGET,
                    "auto-deposit: deposited {wallet_balance} into spot market {} for subaccount id={sub_account_id}: sig={sig}",
                    spot_market.market_index
                );
            }
            Err(e) => {
                log::error!(
                    target: TARGET,
                    "auto-deposit: deposit into spot market {} failed: {e:?}",
                    spot_market.market_index
                );
            }
        }
    }
}
