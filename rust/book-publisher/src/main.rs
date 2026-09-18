//! The order-book publisher, which is the Rust half of the dlob-server split.
//!
//! Book production lives here. It quotes every source through velocity's
//! `quote_router` view, simulated against cached chain state. Book serving
//! stays in the TypeScript dlob-server, which holds the HTTP endpoints, the
//! websocket fan-out and the auth, and reads the same Redis keys this writes.
//! The payload is the existing L2 wire shape, with `clob` and `propamm` joining
//! `vamm` in the per-level `sources` breakdown, so consumers do not move.
//!
//! Simulation is the design rather than an optimization. A Custom quoter is an
//! arbitrary program with no off-chain decoder, so running `quote_v0` is the
//! only way to price it. The view also runs sources in fill order, so a
//! published book equals the fill-time book by construction, margin-clamped
//! PropAMMs and vAMM last-look shading included.

mod cross;
mod metrics_server;
mod payload;
mod user_orders;

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
        sync::{Arc, Mutex},
        time::{Duration, SystemTime, UNIX_EPOCH},
    },
    tracing::{info, warn},
    velocity_quoter_health::{metrics::Metrics, EntryRef, Health, Policy},
    velocity_router_sim::{
        as_versioned,
        health::{quote_market, watch_program_deploys, QuoteRequest},
        quote_view::{
            create_quote_buffer_ixs, perp_market_pda, read_zero_copy, state_pda, QuoteView,
        },
        router_subscriptions, Direction,
    },
};

/// Ticks between deploy-record checks. At the default 200 ms tick this is
/// about 30 seconds, which is prompt for something that happens rarely.
const DEPLOY_WATCH_TICKS: u32 = 150;

#[derive(Parser, Debug)]
#[clap(version)]
pub struct Config {
    #[clap(long, env = "RPC_URL")]
    pub rpc_url: String,
    /// How account state reaches the simulation cache: rpc, ws or grpc.
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
    /// Payer and quote-buffer authority keypair, as a JSON byte array.
    #[clap(long, env = "KEYPAIR_PATH")]
    pub keypair_path: String,
    /// Where per-market quote-buffer keypairs persist across restarts. A
    /// buffer is about 33 KB of rent, and a new one per run would leak it.
    #[clap(long, env = "BUFFER_DIR", default_value = "./quote-buffers")]
    pub buffer_dir: String,
    #[clap(long, env = "REDIS_URL", default_value = "redis://127.0.0.1:6379")]
    pub redis_url: String,
    /// Prefix on every key and channel. It matches the TypeScript side's
    /// client prefix, which ioredis applies implicitly and this sets
    /// explicitly.
    #[clap(long, env = "REDIS_KEY_PREFIX", default_value = "")]
    pub redis_prefix: String,
    /// Where `/metrics` is served. 9464 matches the exporters the rest of
    /// the stack already uses.
    #[clap(long, env = "METRICS_ADDR", default_value = "0.0.0.0:9464")]
    pub metrics_addr: std::net::SocketAddr,
    #[clap(long, env = "TICK_MS", default_value = "200")]
    pub tick_ms: u64,
    /// Size each side is quoted for, base precision.
    #[clap(long, env = "QUOTE_SIZE", default_value = "1000000000000")]
    pub quote_size: u64,
    /// Pooled in-process SVM instances; 0 simulates over RPC instead.
    #[clap(long, env = "LOCAL_SIM_POOL", default_value = "4")]
    pub local_sim_pool: usize,
    /// Submit `crank_cross_match` when the tick's books cross net of fees. The
    /// publisher is the fast path, and relay's poll is the liveness floor.
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

/// Compute units requested for `crank_cross_match`. A self-crossed book
/// measured at about 328,000, because each of the crank's two legs is a whole
/// router fill with its own quote, split, execute and post-fill checks. The
/// headroom covers a cross that reaches more sources than a book against
/// itself.
const CROSS_MATCH_COMPUTE_UNITS: u32 = 500_000;

/// Create and initialize a market's quote buffer when it does not exist yet.
async fn ensure_buffer(
    source: &Arc<dyn ChainSource>,
    velocity: &Pubkey,
    payer: &Keypair,
    buffer: &Keypair,
    market_index: u16,
) -> Result<()> {
    if let Some(existing) = source
        .get_multiple_accounts(&[buffer.pubkey()])
        .await?
        .pop()
        .flatten()
    {
        // A buffer persists across restarts, so one created before the layout
        // grew is still on chain and still too small. Every quote into it would
        // fail on a push. Report that rather than run. The old account holds
        // rent this process cannot reclaim, so replacing it is the operator's
        // decision.
        let wanted = program::state::router_quote::RouterQuoteBufferV0::SIZE;
        if existing.data.len() < wanted {
            bail!(
                "quote buffer {} for market {market_index} is {} bytes, the layout needs {wanted}. \
                 Close it, delete its keypair from the buffer directory, and restart to create a new one.",
                buffer.pubkey(),
                existing.data.len()
            );
        }
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
    // A legacy message, versioned only because that is what the source takes.
    // Nothing here needs an address table or a v1 message.
    let signature = source.send_transaction(&as_versioned(&tx)?).await?;
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

    // Discover quoter programs up front so the feed subscribes to them. The
    // slab subscription then keeps the approved set fresh.
    let rpc = RpcSource::new(config.rpc_url.clone());
    let mut quoter_programs: Vec<Pubkey> = Vec::new();
    for market in &markets {
        for slot in velocity_router_sim::quoter_slab_slots(&rpc, &velocity, *market).await? {
            if slot.quotes() && !quoter_programs.contains(&slot.config.program_id) {
                quoter_programs.push(slot.config.program_id);
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

    // The publisher simulates every market every tick, so it exercises every
    // registered quoter continuously, including ones no taker is routing to.
    // That makes it the router stack's health probe, at no extra cost.
    let registry = std::sync::Arc::new(prometheus::Registry::new());
    let metrics = std::sync::Arc::new(Metrics::register(&registry));
    let health = std::sync::Arc::new(Health::with_metrics(Policy::default(), metrics.clone()));
    metrics_server::serve(config.metrics_addr, registry, metrics, health.clone());

    // Registry entries seen on the last pass, for the deploy watch. Held
    // apart from the health layer because the layer holds no chain state.
    let carried: Arc<Mutex<Vec<EntryRef>>> = Arc::new(Mutex::new(Vec::new()));
    let mut deploy_watch = DEPLOY_WATCH_TICKS;
    // What the per-user index published last, so a tick that changed no user's
    // orders writes nothing. Most ticks change no user's orders.
    let mut user_orders = user_orders::UserOrdersIndex::default();

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
                health.as_ref(),
                &carried,
                &mut user_orders,
            )
            .await
            {
                warn!(market_index, error = %format!("{err:#}"), "tick failed");
            }
        }
        // Expiry is evaluated when a quoter is looked at, so a quarantine on
        // a quoter no market carries needs this to end.
        health.sweep();
        // A redeploy makes a quoter's score describe code that no longer runs.
        // The check runs on a slow cadence, because a deploy is rare and the
        // check costs one account read per distinct quoter program.
        deploy_watch = deploy_watch.saturating_sub(1);
        if deploy_watch == 0 {
            deploy_watch = DEPLOY_WATCH_TICKS;
            let entries = carried.lock().expect("deploy watch lock").clone();
            if let Err(err) = watch_program_deploys(&source, &health, &entries).await {
                warn!(error = %format!("{err:#}"), "deploy watch failed");
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
    health: &Health,
    carried: &Mutex<Vec<EntryRef>>,
    user_orders: &mut user_orders::UserOrdersIndex,
) -> Result<()> {
    let authority = &payer.pubkey();
    let perp_market_account = source
        .get_multiple_accounts(&[perp_market_pda(velocity, market_index)])
        .await?
        .pop()
        .flatten()
        .ok_or_else(|| anyhow!("perp market {market_index} not found"))?;
    let perp_market: PerpMarket = read_zero_copy(&perp_market_account.data)?;

    // The state, which carries the mm-oracle guard rails, and the oracle
    // itself, in one batch.
    let mut side_accounts = source
        .get_multiple_accounts(&[state_pda(velocity), perp_market.oracle])
        .await?;
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

    // The market's approved quoters, off its slab. The book slot's registered
    // market account is the source for L3 and for best makers. The slot is
    // vacant until a book is approved.
    let slab_slots = velocity_router_sim::quoter_slab_slots(source, velocity, market_index).await?;
    let clob_slot = program::state::prop_amm::clob_slot_index(&slab_slots);
    let clob_book_key = clob_slot.map(|index| slab_slots[index].config.response_account);
    // The quote view names a quoter source by its staging entry, so the CLOB
    // label resolves through the slab's book slot rather than the market.
    let clob_entry = clob_slot
        .map(|index| slab_slots[index].entry)
        .unwrap_or_default();

    // A long taker consumes asks and a short taker consumes bids. Both go
    // through the health layer, so a quoter that breaks the simulation costs
    // the market that one source instead of its whole book.
    let request = |direction| {
        QuoteRequest::whole_market(
            *velocity,
            *authority,
            *buffer,
            market_index,
            direction,
            quote_size,
        )
    };
    // Read in as many passes as the market's quoters need. One pass holds a
    // fixed number of sources, and the buffer refuses a push past it rather
    // than truncating, so a market that outgrew a single pass would publish
    // nothing at all.
    let live: Vec<Pubkey> = slab_slots
        .iter()
        .filter_map(|slot| slot.quotes().then_some(slot.entry))
        .collect();
    let asks_quote = quote_market(source, health, &request(Direction::Long), &live).await?;
    let bids_quote = quote_market(source, health, &request(Direction::Short), &live).await?;
    if !asks_quote.excluded.is_empty() || !bids_quote.excluded.is_empty() {
        warn!(
            market_index,
            excluded = ?asks_quote.excluded,
            "publishing without unhealthy quoters"
        );
    }
    {
        let mut seen = carried.lock().expect("deploy watch lock");
        for entry in asks_quote.entries.iter().chain(&bids_quote.entries) {
            let entry = entry.as_entry_ref();
            if !seen.contains(&entry) {
                seen.push(entry);
            }
        }
    }
    let asks = QuoteView {
        market: market_index,
        direction: 0,
        quoted_size: asks_quote.quoted_size,
        slot: asks_quote.slot,
        books: asks_quote.books,
        rows_truncated: asks_quote.rows_truncated,
    };
    let bids = QuoteView {
        market: market_index,
        direction: 1,
        quoted_size: bids_quote.quoted_size,
        slot: bids_quote.slot,
        books: bids_quote.books,
        rows_truncated: bids_quote.rows_truncated,
    };

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
        &clob_entry,
        &asks,
        &bids,
        &decorations,
        ts_ms,
    );

    // The channel gets the full document and the key gets a depth-100 slice,
    // which matches the TypeScript publisher's split between publish and SET.
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

    // L3 and best makers, out of the same view the ladders came from. Every
    // source describes who its depth belongs to. A book does so through its
    // `quote_l3_v0` leg, and every other source against the one user its
    // registry entry names. This therefore reads rows rather than decoding a
    // book.
    let l3_bids = payload::view_rows(velocity, &bids, &bids_quote.entries);
    let l3_asks = payload::view_rows(velocity, &asks, &asks_quote.entries);
    if bids_quote.rows_truncated || asks_quote.rows_truncated {
        warn!(
            market_index,
            "a pass filled its row region; L3 describes part of the book"
        );
    }
    let l3 = payload::l3_payload(
        market_index,
        &name,
        clock.slot,
        &decorations,
        ts_ms,
        l3_bids.clone(),
        l3_asks.clone(),
        quote_size,
    );
    redis
        .set::<_, _, ()>(
            format!("{prefix}last_update_orderbook_l3_perp_{market_index}"),
            l3.to_string(),
        )
        .await?;
    let best_makers = payload::best_makers_payload(clock.slot, l3_bids, l3_asks);
    redis
        .set::<_, _, ()>(
            format!("{prefix}last_update_orderbook_best_makers_perp_{market_index}"),
            best_makers.to_string(),
        )
        .await?;

    // Who is resting what, off the book's own account. This is independent of
    // the ladders above. A quoted book is what a taker of one size would reach,
    // and a maker that asks after their own orders wants all of them.
    if let Some(book_key) = clob_book_key {
        let book = source
            .get_multiple_accounts(&[book_key])
            .await?
            .pop()
            .flatten()
            .ok_or_else(|| anyhow!("clob market {book_key} not found"))?;
        let written = user_orders::publish(
            user_orders,
            redis,
            prefix,
            velocity,
            market_index,
            clock.slot,
            ts_ms,
            &book.data,
        )
        .await?;
        if written > 0 {
            info!(market_index, users = written, "user orders published");
        }
    }

    // Fast-path cross matching. With the books in hand a cross is free to see.
    // The simulation runs before the send. The executor is its own predicate,
    // so a clean simulation implies a profitable crank.
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
            // The crank runs two whole router fills, so it costs well past the
            // 200,000 an instruction gets by default. Without a request of its
            // own the simulation below always fails to complete and no cross is
            // ever sent.
            let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
                &[
                    solana_compute_budget_interface::ComputeBudgetInstruction::set_compute_unit_limit(
                        CROSS_MATCH_COMPUTE_UNITS,
                    ),
                    plan.instruction.clone(),
                ],
                Some(authority),
                &[payer],
                blockhash.hash,
            );
            let tx = as_versioned(&tx)?;
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
                    // A fill raced the crank, or the cross sits inside the
                    // on-chain fee gulf. This happens sometimes, because the
                    // estimate is conservative.
                    warn!(market_index, error = %err, "cross match simulation failed; not sent");
                }
            }
        }
    }
    Ok(())
}
