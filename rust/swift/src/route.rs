//! The router's answer as an HTTP endpoint, served at `/route`.
//!
//! The handler runs the same quote-view simulation the book-publisher runs on
//! its tick. It simulates `quote_router` and reads the per-source verified
//! books out of the buffer's post-state. It then runs the program's own
//! `split_across_quoters` over those books. The response is therefore the
//! split the chain would produce, with margin clamps and priority tiers, and
//! not an off-chain estimate.
//!
//! The endpoint is read-only. Quoting is simulation-only and simulation skips
//! signature verification, so the handler reuses an existing quote buffer and
//! names the authority stored on it. That buffer is normally the
//! book-publisher's, found by market. Swift holds no key for it and pays no
//! rent. A market with no buffer answers 503. The publisher does not cover
//! that market yet, and there is no fallback.

use {
    axum::{
        extract::{Query, State},
        http::StatusCode,
        response::IntoResponse,
        Json,
    },
    prometheus::Registry,
    relay_chain_source::RpcSource,
    serde::{Deserialize, Serialize},
    solana_pubkey::Pubkey,
    std::{collections::HashMap, sync::Arc},
    tokio::sync::RwLock,
    velocity_quoter_health::{metrics::Metrics as QuoterMetrics, Health, Policy},
    velocity_router_sim::{
        find_quote_buffer,
        health::{quote_market, QuoteRequest},
        quote_view::{perp_market_pda, read_zero_copy},
        split_across_quoters, Direction, PriceLevel, QuoterBook,
    },
    velocity_rs::program::state::prop_amm::ClobUserRefV0,
};

/// Everything `/route` needs, independent of the rest of the server. It holds
/// its own RPC source, which is the simulation transport, and the per-market
/// buffer discovery cache.
pub struct RouteContext {
    source: RpcSource,
    velocity: Pubkey,
    /// The discovered route per market. A market is discovered again when
    /// its cached buffer stops simulating, which happens when the buffer is
    /// closed or republished elsewhere.
    markets: RwLock<HashMap<u16, MarketRoute>>,
    /// Which quoters this endpoint is willing to carry. A quoter that keeps
    /// breaking the simulation is dropped from the route instead of turning
    /// every request for that market into an error.
    health: Arc<Health>,
}

#[derive(Clone)]
struct MarketRoute {
    buffer: Pubkey,
    authority: Pubkey,
    step_size: u64,
    /// The market's live quoters, by staging-entry address, so planning the
    /// view's passes costs no extra read. This is refreshed with the rest of
    /// the route when a buffer stops simulating, which is also when the set
    /// is most likely stale.
    quoters: Vec<Pubkey>,
}

impl RouteContext {
    pub fn new(rpc_url: String, velocity: Pubkey) -> Self {
        Self {
            source: RpcSource::new(rpc_url),
            velocity,
            markets: RwLock::new(HashMap::new()),
            health: Arc::new(Health::new(Policy::default())),
        }
    }

    /// Report quoter health into `registry` as well as acting on it.
    pub fn with_metrics(rpc_url: String, velocity: Pubkey, registry: &Registry) -> Self {
        Self {
            health: Arc::new(Health::with_metrics(
                Policy::default(),
                Arc::new(QuoterMetrics::register(registry)),
            )),
            ..Self::new(rpc_url, velocity)
        }
    }

    /// The market's live quoters, meaning the slab slots that may take flow.
    /// The view is read in as many passes as they need.
    async fn market_quoters(&self, market_index: u16) -> Result<Vec<Pubkey>, RouteError> {
        let slots =
            velocity_router_sim::quoter_slab_slots(&self.source, &self.velocity, market_index)
                .await
                .map_err(RouteError::internal)?;
        Ok(slots
            .into_iter()
            .filter_map(|slot| slot.quotes().then_some(slot.entry))
            .collect())
    }

    /// The health state behind this endpoint, for the metrics exporter and
    /// the operator surface.
    pub fn health(&self) -> &Arc<Health> {
        &self.health
    }

    async fn market_route(&self, market_index: u16) -> Result<MarketRoute, RouteError> {
        if let Some(route) = self.markets.read().await.get(&market_index) {
            return Ok(route.clone());
        }

        let (buffer, authority) = find_quote_buffer(&self.source, &self.velocity, market_index)
            .await
            .map_err(RouteError::internal)?
            .ok_or(RouteError::NoBuffer(market_index))?;
        let market_key = perp_market_pda(&self.velocity, market_index);
        let account =
            relay_chain_source::ChainSource::get_multiple_accounts(&self.source, &[market_key])
                .await
                .map_err(RouteError::internal)?
                .pop()
                .flatten()
                .ok_or(RouteError::NoMarket(market_index))?;
        let market: velocity_rs::program::state::perp_market::PerpMarket =
            read_zero_copy(&account.data).map_err(RouteError::internal)?;
        let quoters = self.market_quoters(market_index).await?;
        let route = MarketRoute {
            buffer,
            authority,
            step_size: market.order_step_size,
            quoters,
        };

        self.markets
            .write()
            .await
            .insert(market_index, route.clone());
        Ok(route)
    }

    async fn forget(&self, market_index: u16) {
        self.markets.write().await.remove(&market_index);
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteQuery {
    market_index: u16,
    /// The taker's side. Accepts "long", "buy", "short", or "sell".
    direction: String,
    /// Taker base size, BASE_PRECISION units.
    size: u64,
    /// The taker's own authority and sub-account, when it has one. A book
    /// never fills a user against itself, so supplying these changes the
    /// quote from a caller with no resting orders. Omitting them skips at
    /// most one maker.
    taker_authority: Option<String>,
    #[serde(default)]
    taker_sub_account_id: u16,
}

/// One source's verified book in the response. Prices and sizes are strings,
/// because u64s in the protocol's fixed precisions overflow JS numbers.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BookOut {
    key: String,
    kind: &'static str,
    priority: u8,
    /// Margin verification reduced this book below what the source quoted.
    clamped: bool,
    levels: Vec<LevelOut>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LevelOut {
    price: String,
    size: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AllocationOut {
    /// What `kind` names. It is the `QuoterV0` entry for `"quoter"` and the
    /// perp market for `"vamm"`.
    key: String,
    kind: &'static str,
    /// Base routed to this source, and its quote notional at the quoted
    /// levels. Execute must land at or better than this per unit.
    base: String,
    quote: String,
}

/// One book maker the fill has to carry, and the two accounts it costs.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MakerOut {
    authority: String,
    sub_account_id: u16,
    user: String,
    user_stats: String,
    /// This owner rests at least one order that can end a fill walk, so
    /// leaving it out forfeits the depth behind that order. False means
    /// carrying it wins only its own size. It is always true on a book where
    /// any order can end a walk, which is a book that sets no size floor.
    gates_depth: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RouteResponse {
    market_index: u16,
    direction: &'static str,
    size: String,
    /// The slot the quote simulation ran at. This bounds how stale the
    /// answer can be.
    slot: u64,
    step_size: String,
    books: Vec<BookOut>,
    /// Parallel to `books`. Zero-base entries are omitted.
    allocations: Vec<AllocationOut>,
    filled_base: String,
    filled_quote: String,
    unfilled_base: String,
    /// The makers resting on the route's CLOB books that this fill would
    /// sweep, in the order to carry them, with the two accounts each costs.
    ///
    /// A book stores its makers as an authority and a sub-account, not an
    /// account key. This is the only way to build the fill's account set
    /// without reading the book directly. The list is the program's own
    /// walk, not an estimate.
    ///
    /// allow-verbose: this is the account-ordering contract a caller must
    /// follow. Carry makers in this order and stop where the transaction
    /// runs out of room. The book stops at the first maker the caller did
    /// not bring, so a gap forfeits every maker behind it. The order is the
    /// book's own walk, best price first, except every maker with
    /// `gates_depth` true comes before one with it false, each group
    /// keeping walk order. `gates_depth` mirrors the book's own rule for
    /// which order can end a walk, so truncating this list always drops the
    /// makers that cost least to lose. These and `books` together decide
    /// the full account set.
    clob_makers: Vec<MakerOut>,
}

pub enum RouteError {
    BadRequest(String),
    NoBuffer(u16),
    NoMarket(u16),
    Simulation(String),
    Internal(String),
}

impl RouteError {
    fn internal(err: impl std::fmt::Display) -> Self {
        Self::Internal(err.to_string())
    }
}

impl IntoResponse for RouteError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::NoBuffer(market) => (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("no quote buffer exists for market {market} — is the book-publisher covering it?"),
            ),
            Self::NoMarket(market) => (
                StatusCode::BAD_REQUEST,
                format!("perp market {market} does not exist"),
            ),
            Self::Simulation(message) => (StatusCode::SERVICE_UNAVAILABLE, message),
            Self::Internal(message) => (StatusCode::INTERNAL_SERVER_ERROR, message),
        };

        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

fn kind_label(kind: velocity_rs::program::state::router_quote::QuotedSourceKind) -> &'static str {
    use velocity_rs::program::state::router_quote::QuotedSourceKind;
    match kind {
        QuotedSourceKind::Vamm => "vamm",
        QuotedSourceKind::DlobOrder => "dlob",
        QuotedSourceKind::Quoter => "quoter",
    }
}

pub async fn route_quote(
    State(state): State<&'static crate::swift_server::ServerParams>,
    Query(query): Query<RouteQuery>,
) -> Result<impl IntoResponse, RouteError> {
    let ctx = state.route();
    let (direction, direction_label) = match query.direction.to_lowercase().as_str() {
        "long" | "buy" => (Direction::Long, "long"),
        "short" | "sell" => (Direction::Short, "short"),
        other => {
            return Err(RouteError::BadRequest(format!(
                "direction must be long|short, got '{other}'"
            )))
        }
    };

    if query.size == 0 {
        return Err(RouteError::BadRequest("size must be nonzero".into()));
    }

    // One retry through rediscovery. The cached buffer may have been closed,
    // or the publisher may have moved markets since the last discovery. The
    // retry rebuilds the same account set, so it only helps when the buffer
    // moved. A quoter that reverts is handled a layer down, by dropping the
    // quoter the logs name and quoting the rest of the market.
    let mut view = None;
    for attempt in 0..2 {
        let route = ctx.market_route(query.market_index).await?;
        let request = QuoteRequest::whole_market(
            ctx.velocity,
            route.authority,
            route.buffer,
            query.market_index,
            direction,
            query.size,
        );

        match quote_market(&ctx.source, &ctx.health, &request, &route.quoters).await {
            Ok(quoted) => {
                if !quoted.excluded.is_empty() {
                    log::warn!(
                        target: "route",
                        "market {} routed without {:?}",
                        query.market_index,
                        quoted.excluded
                    );
                }

                view = Some((quoted.view, route));

                break;
            }
            Err(err) if attempt == 0 => {
                ctx.forget(query.market_index).await;
                log::warn!(target: "route", "quote simulation failed, rediscovering: {err:#}");
            }
            Err(err) => return Err(RouteError::Simulation(format!("{err:#}"))),
        }
    }

    let (view, route) = view.expect("loop either set view or returned");

    // The program's own split over the verified books. Nothing here mirrors
    // it, so nothing can drift from it.
    let level_arrays: Vec<Vec<PriceLevel>> = view
        .books
        .iter()
        .map(|book| {
            book.levels
                .iter()
                .map(|level| PriceLevel {
                    price: level.price,
                    size: level.size,
                })
                .collect()
        })
        .collect();
    let books: Vec<QuoterBook> = view
        .books
        .iter()
        .zip(&level_arrays)
        .map(|(book, levels)| QuoterBook {
            priority: book.priority,
            levels,
            // The view quotes unrestricted, so no book falls short of a user
            // set and none of them holds anything back.
            withheld: Default::default(),
        })
        .collect();
    let allocations = split_across_quoters(direction, query.size, &books, route.step_size)
        .map_err(|err| RouteError::Simulation(format!("split failed: {err:?}")))?;

    let filled_base: u64 = allocations.iter().map(|a| a.base).sum();
    let filled_quote: u64 = allocations.iter().map(|a| a.quote).sum();
    // The users a fill would settle for, out of the view that was already
    // simulated. They come from the same walk, at the same slot, and against
    // the same book state as the ladders above. A second read of the book
    // could not promise that.
    let clob_makers: Vec<MakerOut> = view
        .ranked_settleable_users()
        .into_iter()
        .filter(|(user, _)| *user != taker_ref(&query))
        .map(|(user, gates_depth)| {
            let authority = Pubkey::new_from_array(user.authority.to_bytes());
            let (user_key, user_stats) =
                derive_user_accounts(&ctx.velocity, &authority, user.sub_account_id);
            MakerOut {
                authority: authority.to_string(),
                sub_account_id: user.sub_account_id,
                user: user_key.to_string(),
                user_stats: user_stats.to_string(),
                gates_depth,
            }
        })
        .collect();
    Ok(Json(RouteResponse {
        market_index: view.market,
        direction: direction_label,
        size: query.size.to_string(),
        slot: view.slot,
        step_size: route.step_size.to_string(),
        books: view
            .books
            .iter()
            .map(|book| BookOut {
                key: book.key.to_string(),
                kind: kind_label(book.kind),
                priority: book.priority,
                clamped: book.clamped,
                levels: book
                    .levels
                    .iter()
                    .map(|level| LevelOut {
                        price: level.price.to_string(),
                        size: level.size.to_string(),
                    })
                    .collect(),
            })
            .collect(),
        allocations: view
            .books
            .iter()
            .zip(&allocations)
            .filter(|(_, allocation)| allocation.base > 0)
            .map(|(book, allocation)| AllocationOut {
                key: book.key.to_string(),
                kind: kind_label(book.kind),
                base: allocation.base.to_string(),
                quote: allocation.quote.to_string(),
            })
            .collect(),
        filled_base: filled_base.to_string(),
        filled_quote: filled_quote.to_string(),
        unfilled_base: query.size.saturating_sub(filled_base).to_string(),
        clob_makers,
    }))
}

/// The taker as the wire names it, so a route never reports the caller to
/// itself as one of its own makers.
fn taker_ref(query: &RouteQuery) -> ClobUserRefV0 {
    ClobUserRefV0 {
        authority: query
            .taker_authority
            .as_deref()
            .and_then(|text| text.parse::<Pubkey>().ok())
            .map(|key| anchor_lang::prelude::Pubkey::new_from_array(key.to_bytes()))
            .unwrap_or_default(),
        sub_account_id: query.taker_sub_account_id,
    }
}

/// The two accounts a maker's identity derives to.
fn derive_user_accounts(
    velocity: &Pubkey,
    authority: &Pubkey,
    sub_account_id: u16,
) -> (Pubkey, Pubkey) {
    let user = Pubkey::find_program_address(
        &[b"user", authority.as_ref(), &sub_account_id.to_le_bytes()],
        velocity,
    )
    .0;
    let stats = Pubkey::find_program_address(&[b"user_stats", authority.as_ref()], velocity).0;
    (user, stats)
}

/// Wall clock in seconds, for the book walk's expiry check.
fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}
