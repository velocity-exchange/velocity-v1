//! Order-book publisher: the Rust half of the dlob-server split.
//!
//! Book *production* moves here — quote every source through velocity's
//! `quote_router` view, simulated against cached chain state — while book
//! *serving* (HTTP endpoints, websocket fan-out, auth) stays in the
//! TypeScript dlob-server, reading the same Redis keys this writes. The
//! payload is the existing L2 wire shape with `clob`/`propamm` joining
//! `vamm`/`dlob` in the per-level `sources` breakdown, so consumers don't
//! move.
//!
//! Simulation is the design, not an optimization: a Custom quoter is an
//! arbitrary program with no off-chain decoder, so `quote_v0` (i.e. running
//! it) is the only way to price it — and the view runs sources in fill
//! order, so published books equal fill-time books by construction
//! (margin-clamped PropAMMs, vAMM last-look shading included).
//!
//! What the TS publisher keeps until the DLOB dies: DLOB maker books (this
//! quote view is built without `(User, UserStats)` maker pairs).

mod cross;
mod payload;

use {
    anyhow::{anyhow, bail, Context, Result},
    clap::Parser,
    program::state::{perp_market::PerpMarket, traits::Size},
    redis::AsyncCommands,
    relay_chain_source::{
        derive_ws_url, feed_channel, spawn_grpc_feed, spawn_ws_feed, CachedSource,
        CachedSourceConfig, ChainSource, GrpcFeedConfig, LocalSimConfig, LocalSimSource, RpcSource,
        SignatureOutcome,
    },
    solana_sdk::{pubkey::Pubkey, signature::Keypair, signer::Signer, transaction::Transaction},
    std::{
        str::FromStr,
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    },
    tracing::{info, warn},
    velocity_router_sim::{
        quote_view::{
            build_quote_router_ix, create_quote_buffer_ixs, perp_market_pda, read_zero_copy,
            simulate_quote_view, state_pda,
        },
        router_subscriptions, Direction,
    },
};

#[derive(Parser, Debug)]
#[clap(version)]
pub struct Config {
    #[clap(long, env = "RPC_URL")]
    pub rpc_url: String,
    /// rpc | ws | grpc — how account state reaches the simulation cache.
    #[clap(long, env = "TRANSPORT", default_value = "rpc")]
    pub transport: String,
    #[clap(long, env = "WS_URL")]
    pub ws_url: Option<String>,
    #[clap(long, env = "GRPC_ENDPOINT")]
    pub grpc_endpoint: Option<String>,
    #[clap(long, env = "GRPC_X_TOKEN")]
    pub grpc_x_token: Option<String>,
    #[clap(long, env = "VELOCITY_PROGRAM_ID")]
    pub velocity_program: String,
    /// Perp market indexes to publish, comma-separated.
    #[clap(long, env = "MARKETS", default_value = "0")]
    pub markets: String,
    /// Payer + quote-buffer authority keypair (JSON byte array).
    #[clap(long, env = "KEYPAIR_PATH")]
    pub keypair_path: String,
    /// Where per-market quote-buffer keypairs persist across restarts (a
    /// buffer is ~33 KB of rent; recreating one per run would leak it).
    #[clap(long, env = "BUFFER_DIR", default_value = "./quote-buffers")]
    pub buffer_dir: String,
    #[clap(long, env = "REDIS_URL", default_value = "redis://127.0.0.1:6379")]
    pub redis_url: String,
    /// Prefix on every key and channel, matching the TS side's client
    /// prefix (ioredis applies it implicitly; here it is explicit).
    #[clap(long, env = "REDIS_KEY_PREFIX", default_value = "")]
    pub redis_prefix: String,
    #[clap(long, env = "TICK_MS", default_value = "400")]
    pub tick_ms: u64,
    /// Size each side is quoted for, base precision.
    #[clap(long, env = "QUOTE_SIZE", default_value = "1000000000000")]
    pub quote_size: u64,
    /// Pooled in-process SVM instances; 0 simulates over RPC instead.
    #[clap(long, env = "LOCAL_SIM_POOL", default_value = "4")]
    pub local_sim_pool: usize,
    /// Submit `crank_cross_match` when the tick's books cross net of fees
    /// (the publisher is the fast path; relay's poll is the liveness floor).
    #[clap(long, env = "CROSS_MATCH", default_value = "true")]
    pub cross_match: bool,
}

fn load_keypair(path: &str) -> Result<Keypair> {
    let bytes: Vec<u8> = serde_json::from_str(
        &std::fs::read_to_string(path).with_context(|| format!("read keypair {path}"))?,
    )?;
    Keypair::try_from(bytes.as_slice()).map_err(|e| anyhow!("parse keypair {path}: {e}"))
}

fn load_or_create_buffer_keypair(dir: &str, market_index: u16) -> Result<Keypair> {
    std::fs::create_dir_all(dir)?;
    let path = format!("{dir}/market-{market_index}.json");
    if std::path::Path::new(&path).exists() {
        return load_keypair(&path);
    }
    let keypair = Keypair::new();
    std::fs::write(&path, serde_json::to_string(&keypair.to_bytes().to_vec())?)?;
    Ok(keypair)
}

fn market_name(perp_market: &PerpMarket) -> String {
    String::from_utf8_lossy(&perp_market.name)
        .trim_end_matches([' ', '\0'])
        .to_string()
}

fn maybe_local_sim<S: ChainSource + 'static>(inner: S, pool: usize) -> Arc<dyn ChainSource> {
    if pool == 0 {
        Arc::new(inner)
    } else {
        Arc::new(LocalSimSource::new(
            inner,
            LocalSimConfig {
                pool_size: pool,
                ..LocalSimConfig::default()
            },
        ))
    }
}

/// Create + initialize a market's quote buffer if it doesn't exist yet.
async fn ensure_buffer(
    source: &Arc<dyn ChainSource>,
    velocity: &Pubkey,
    payer: &Keypair,
    buffer: &Keypair,
    market_index: u16,
) -> Result<()> {
    if source
        .get_multiple_accounts(&[buffer.pubkey()])
        .await?
        .pop()
        .flatten()
        .is_some()
    {
        return Ok(());
    }
    let rent = solana_rent::Rent::default()
        .minimum_balance(program::state::router_quote::RouterQuoteBufferV0::SIZE);
    let ixs = create_quote_buffer_ixs(
        velocity,
        &payer.pubkey(),
        &payer.pubkey(),
        &buffer.pubkey(),
        market_index,
        rent,
    );
    let blockhash = source.latest_blockhash().await?;
    let tx = Transaction::new_signed_with_payer(
        &ixs,
        Some(&payer.pubkey()),
        &[payer, buffer],
        blockhash.hash,
    );
    let signature = source.send_transaction(&tx).await?;
    info!(%signature, market_index, buffer = %buffer.pubkey(), "creating quote buffer");
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        match source
            .signature_statuses(&[signature])
            .await?
            .pop()
            .flatten()
        {
            Some(SignatureOutcome::Landed) => return Ok(()),
            Some(SignatureOutcome::Failed(err)) => bail!("buffer creation failed: {err}"),
            None => continue,
        }
    }
    bail!("buffer creation for market {market_index} never landed")
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let config = Config::parse();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        "book-publisher starting"
    );

    let velocity = Pubkey::from_str(&config.velocity_program).context("VELOCITY_PROGRAM_ID")?;
    let markets: Vec<u16> = config
        .markets
        .split(',')
        .map(|m| m.trim().parse().context("MARKETS"))
        .collect::<Result<_>>()?;
    let payer = load_keypair(&config.keypair_path)?;

    // Discover quoter programs up front so the feed subscribes to them; the
    // registry subscription itself keeps the entry set fresh afterwards.
    let rpc = RpcSource::new(config.rpc_url.clone());
    let mut quoter_programs: Vec<Pubkey> = Vec::new();
    for market in &markets {
        for (_, account) in velocity_router_sim::quoter_entries(&rpc, &velocity, *market).await? {
            let entry: program::state::prop_amm::QuoterV0 = read_zero_copy(&account.data)?;
            if entry.is_active && entry.is_approved && !quoter_programs.contains(&entry.program_id)
            {
                quoter_programs.push(entry.program_id);
            }
        }
    }
    let subscriptions = router_subscriptions(velocity, None, &quoter_programs);

    let source: Arc<dyn ChainSource> = match config.transport.as_str() {
        "rpc" => maybe_local_sim(rpc, config.local_sim_pool),
        "ws" => {
            let ws_url = config
                .ws_url
                .clone()
                .unwrap_or_else(|| derive_ws_url(&config.rpc_url));
            let (sender, receiver) = feed_channel();
            spawn_ws_feed(ws_url.clone(), subscriptions.clone(), sender);
            info!(%ws_url, programs = quoter_programs.len() + 1, "websocket subscriptions enabled");
            maybe_local_sim(
                CachedSource::new(
                    rpc,
                    receiver,
                    CachedSourceConfig {
                        indexed_programs: subscriptions,
                        ..CachedSourceConfig::default()
                    },
                ),
                config.local_sim_pool,
            )
        }
        "grpc" => {
            let endpoint = config
                .grpc_endpoint
                .clone()
                .context("GRPC_ENDPOINT is required for the grpc transport")?;
            let (sender, receiver) = feed_channel();
            spawn_grpc_feed(
                GrpcFeedConfig {
                    endpoint: endpoint.clone(),
                    x_token: config.grpc_x_token.clone(),
                },
                subscriptions.clone(),
                sender,
            );
            info!(%endpoint, programs = quoter_programs.len() + 1, "yellowstone gRPC subscriptions enabled");
            maybe_local_sim(
                CachedSource::new(
                    rpc,
                    receiver,
                    CachedSourceConfig {
                        indexed_programs: subscriptions,
                        ..CachedSourceConfig::default()
                    },
                ),
                config.local_sim_pool,
            )
        }
        other => bail!("unknown transport {other} (rpc | ws | grpc)"),
    };

    // One persistent quote buffer per market.
    let mut buffers: Vec<(u16, Keypair)> = Vec::new();
    for market in &markets {
        let buffer = load_or_create_buffer_keypair(&config.buffer_dir, *market)?;
        ensure_buffer(&source, &velocity, &payer, &buffer, *market).await?;
        buffers.push((*market, buffer));
    }

    let redis_client = redis::Client::open(config.redis_url.clone())?;
    let mut redis = redis_client
        .get_multiplexed_tokio_connection()
        .await
        .context("connect redis")?;

    let mut tick = tokio::time::interval(Duration::from_millis(config.tick_ms));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    info!(markets = ?markets, tick_ms = config.tick_ms, cross_match = config.cross_match, "publishing");

    loop {
        tick.tick().await;
        for (market_index, buffer) in &buffers {
            if let Err(err) = publish_market(
                &source,
                &mut redis,
                &config.redis_prefix,
                &velocity,
                &payer,
                &buffer.pubkey(),
                *market_index,
                config.quote_size,
                config.cross_match,
            )
            .await
            {
                warn!(market_index, error = %format!("{err:#}"), "tick failed");
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn publish_market(
    source: &Arc<dyn ChainSource>,
    redis: &mut redis::aio::MultiplexedConnection,
    prefix: &str,
    velocity: &Pubkey,
    payer: &Keypair,
    buffer: &Pubkey,
    market_index: u16,
    quote_size: u64,
    cross_match: bool,
) -> Result<()> {
    let authority = &payer.pubkey();
    let perp_market_account = source
        .get_multiple_accounts(&[perp_market_pda(velocity, market_index)])
        .await?
        .pop()
        .flatten()
        .ok_or_else(|| anyhow!("perp market {market_index} not found"))?;
    let perp_market: PerpMarket = read_zero_copy(&perp_market_account.data)?;

    // State (mm-oracle guard rails), the oracle itself, and the canonical
    // CLOB entry (whose registered market account is the L3/best-makers
    // source) — one batch; the entry is absent until a book is attached.
    let mut side_accounts = source
        .get_multiple_accounts(&[
            state_pda(velocity),
            perp_market.oracle,
            perp_market.clob_quoter,
        ])
        .await?;
    let clob_entry_account = side_accounts.pop().flatten();
    let mut oracle_account = side_accounts
        .pop()
        .flatten()
        .ok_or_else(|| anyhow!("oracle account missing for market {market_index}"))?;
    let state: program::state::state::State = read_zero_copy(
        &side_accounts
            .pop()
            .flatten()
            .ok_or_else(|| anyhow!("state account missing"))?
            .data,
    )?;
    let clob_book_key = clob_entry_account
        .as_ref()
        .map(|account| read_zero_copy::<program::state::prop_amm::QuoterV0>(&account.data))
        .transpose()?
        .map(|entry| entry.response_account);

    // A long taker consumes asks; a short taker consumes bids.
    let asks_ix = build_quote_router_ix(
        source,
        velocity,
        authority,
        buffer,
        market_index,
        Direction::Long,
        quote_size,
    )
    .await?;
    let asks = simulate_quote_view(source, asks_ix, authority, buffer).await?;
    let bids_ix = build_quote_router_ix(
        source,
        velocity,
        authority,
        buffer,
        market_index,
        Direction::Short,
        quote_size,
    )
    .await?;
    let bids = simulate_quote_view(source, bids_ix, authority, buffer).await?;

    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let clock = source.clock().await?;
    let book_slot = asks.slot.max(bids.slot);
    let decorations = payload::build_decorations(
        &perp_market,
        &state,
        &oracle_account.owner,
        &mut oracle_account.data,
        clock.slot,
        book_slot,
    )?;
    let name = market_name(&perp_market);
    let l2 = payload::l2_payload(
        market_index,
        &name,
        &perp_market.clob_quoter,
        &asks,
        &bids,
        &decorations,
        ts_ms,
    );

    // The channel gets the full document; the key a depth-100 slice —
    // matching the TS publisher's publish/SET split.
    let mut l2_depth100 = l2.clone();
    for side in ["bids", "asks"] {
        if let Some(levels) = l2_depth100[side].as_array_mut() {
            levels.truncate(100);
        }
    }
    let key = format!("{prefix}last_update_orderbook_perp_{market_index}");
    let channel = format!("{prefix}orderbook_perp_{market_index}");
    redis.set::<_, _, ()>(&key, l2_depth100.to_string()).await?;
    redis.publish::<_, _, ()>(&channel, l2.to_string()).await?;

    for (group, document) in payload::grouped_payloads(&l2, perp_market.order_tick_size) {
        redis
            .publish::<_, _, ()>(
                format!("{prefix}orderbook_perp_{market_index}_grouped_{group}"),
                document.to_string(),
            )
            .await?;
    }

    // L3 + best makers come off the CLOB book directly (per-order data the
    // quote view deliberately flattens away).
    if let Some(book_key) = clob_book_key {
        if let Some(book) = source
            .get_multiple_accounts(&[book_key])
            .await?
            .pop()
            .flatten()
        {
            let l3 = payload::l3_payload(
                velocity,
                market_index,
                &name,
                &book.data,
                clock.slot,
                clock.unix_timestamp,
                &decorations,
                ts_ms,
            );
            redis
                .set::<_, _, ()>(
                    format!("{prefix}last_update_orderbook_l3_perp_{market_index}"),
                    l3.to_string(),
                )
                .await?;
            let best_makers = payload::best_makers_payload(
                velocity,
                &book.data,
                clock.slot,
                clock.unix_timestamp,
            );
            redis
                .set::<_, _, ()>(
                    format!("{prefix}last_update_orderbook_best_makers_perp_{market_index}"),
                    best_makers.to_string(),
                )
                .await?;
        }
    }

    // Fast-path cross matching: books in hand, a cross is free to see.
    // Simulate before sending — the executor is its own predicate, so a
    // clean simulation implies a profitable crank.
    if cross_match {
        if let Some(plan) = cross::find_cross_plan(
            source.as_ref(),
            velocity,
            authority,
            market_index,
            &perp_market.oracle,
            perp_market.quote_spot_market_index,
            program::math::constants::BASE_PRECISION_U64 as u128,
            &asks,
            &bids,
        )
        .await?
        {
            let blockhash = source.latest_blockhash().await?;
            let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
                &[plan.instruction.clone()],
                Some(authority),
                &[payer],
                blockhash.hash,
            );
            let sim = source.simulate_transaction(&tx, &[]).await?;
            match sim.err {
                None => {
                    let signature = source.send_transaction(&tx).await?;
                    info!(
                        market_index,
                        %signature,
                        size = plan.size,
                        estimated_surplus = plan.estimated_surplus as u64,
                        "submitted cross match"
                    );
                }
                Some(err) => {
                    // Raced by a fill or inside the fee gulf on-chain —
                    // expected occasionally; the estimate is conservative.
                    warn!(market_index, error = %err, "cross match simulation failed; not sent");
                }
            }
        }
    }
    Ok(())
}
