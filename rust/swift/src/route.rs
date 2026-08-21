//! `/route` — the router's answer as an HTTP endpoint.
//!
//! Runs the same quote-view simulation the book-publisher runs on its tick
//! (`quote_router` under simulation, per-source verified books out of the
//! buffer's post-state) and then the program's own `split_across_quoters`
//! over those books — so the response is the split the chain would produce,
//! including margin clamps and priority tiers, not an off-chain estimate.
//!
//! Deliberately read-only: quoting is simulation-only and simulation skips
//! signature verification, so this rides an existing quote buffer
//! (normally the book-publisher's, discovered by market) naming its stored
//! authority — swift holds no key and pays no rent for it. No buffer for a
//! market means the publisher isn't covering it yet: 503, not a fallback.

use {
    axum::{
        extract::{Query, State},
        http::StatusCode,
        response::IntoResponse,
        Json,
    },
    relay_chain_source::{ChainSource, RpcSource},
    serde::{Deserialize, Serialize},
    solana_pubkey::Pubkey,
    std::collections::HashMap,
    tokio::sync::RwLock,
    velocity_router_sim::{
        find_quote_buffer,
        quote_view::{build_quote_router_ix, perp_market_pda, read_zero_copy, simulate_quote_view},
        split_across_quoters, Direction, PriceLevel, QuoterBook,
    },
    velocity_rs::program::state::{prop_amm::ClobUserRefV0, router_quote::QuotedSourceKind},
};

/// Everything `/route` needs, independent of the rest of the server: its
/// own thin RPC source (the simulation transport) and the per-market
/// buffer discovery cache.
pub struct RouteContext {
    source: RpcSource,
    velocity: Pubkey,
    /// `(buffer, authority, step_size)` per market. Re-discovered when a
    /// cached buffer stops simulating (closed, republished elsewhere).
    markets: RwLock<HashMap<u16, MarketRoute>>,
}

#[derive(Clone, Copy)]
struct MarketRoute {
    buffer: Pubkey,
    authority: Pubkey,
    step_size: u64,
}

impl RouteContext {
    pub fn new(rpc_url: String, velocity: Pubkey) -> Self {
        Self {
            source: RpcSource::new(rpc_url),
            velocity,
            markets: RwLock::new(HashMap::new()),
        }
    }

    async fn market_route(&self, market_index: u16) -> Result<MarketRoute, RouteError> {
        if let Some(route) = self.markets.read().await.get(&market_index) {
            return Ok(*route);
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
        let route = MarketRoute {
            buffer,
            authority,
            step_size: market.order_step_size,
        };
        self.markets.write().await.insert(market_index, route);
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
    /// "long"/"buy" or "short"/"sell" — the taker's side.
    direction: String,
    /// Taker base size, BASE_PRECISION units.
    size: u64,
    /// The taker's own authority and sub-account, when it has one. A book
    /// never fills a user against itself, so a caller that also rests on the
    /// book gets a different answer than one that does not. Omitted means "no
    /// resting liquidity of my own here", which is the common case and is
    /// safe: the worst it costs is naming one maker the fill will skip.
    taker_authority: Option<String>,
    #[serde(default)]
    taker_sub_account_id: u16,
}

/// One source's verified book in the response. Prices and sizes are
/// strings: u64s in the protocol's fixed precisions overflow JS numbers.
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
    key: String,
    kind: &'static str,
    /// Base routed to this source and its quote notional at the quoted
    /// levels — execute must land at-or-better than this per unit.
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
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RouteResponse {
    market_index: u16,
    direction: &'static str,
    size: String,
    /// Slot the quote simulation ran at — the staleness bound.
    slot: u64,
    step_size: String,
    books: Vec<BookOut>,
    /// Parallel to `books`; zero-base entries omitted.
    allocations: Vec<AllocationOut>,
    filled_base: String,
    filled_quote: String,
    unfilled_base: String,
    /// The makers resting on the route's CLOB books that this fill would
    /// sweep, best price first, with the two accounts each costs.
    ///
    /// A fill settles only for users whose accounts it carries, and a book
    /// stores its makers as an authority and a sub-account rather than an
    /// account key — so this is the one part of a fill's account set that
    /// cannot be worked out without reading the book. The list is the
    /// program's own walk, not an estimate.
    ///
    /// Carry them in this order and stop where the transaction runs out of
    /// room: the book stops at the first maker the caller did not bring, so a
    /// prefix fills and a gap forfeits everything behind it.
    ///
    /// DLOB makers are not here and cannot be — they rest in `User.orders`,
    /// which this endpoint does not read. They come from `/topMakers`.
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

    // One retry through rediscovery: the cached buffer may have been closed
    // or the publisher may have moved markets since we last looked.
    let mut view = None;
    for attempt in 0..2 {
        let route = ctx.market_route(query.market_index).await?;
        let ix = build_quote_router_ix(
            &ctx.source,
            &ctx.velocity,
            &route.authority,
            &route.buffer,
            query.market_index,
            direction,
            query.size,
        )
        .await
        .map_err(RouteError::internal)?;
        match simulate_quote_view(&ctx.source, ix, &route.authority, &route.buffer).await {
            Ok(quoted) => {
                view = Some((quoted, route));
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

    // The program's own split over the verified books — no mirror to drift.
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
    let allocations = split_across_quoters(direction, query.size, &books, route.step_size, None)
        .map_err(|err| RouteError::Simulation(format!("split failed: {err:?}")))?;

    let filled_base: u64 = allocations.iter().map(|a| a.base).sum();
    let filled_quote: u64 = allocations.iter().map(|a| a.quote).sum();
    let clob_makers = clob_makers_for_route(&ctx.source, &ctx.velocity, &view, direction, &query)
        .await
        .unwrap_or_else(|err| {
            // A book that cannot be read costs the caller the maker half of
            // its answer, not the whole route: the split above stands on the
            // simulation, which already succeeded.
            log::warn!(target: "route", "clob makers unavailable: {err:#}");
            Vec::new()
        });
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

/// The makers a fill would sweep off every CLOB book on the route.
///
/// Walked at the taker's whole size rather than at what the split allocated:
/// on-chain the book is quoted before anything is split, so the makers it
/// touches are the ones within the full size. Sizing this to the allocation
/// would name fewer accounts than the fill needs.
async fn clob_makers_for_route(
    source: &RpcSource,
    velocity: &Pubkey,
    view: &velocity_router_sim::quote_view::QuoteView,
    direction: Direction,
    query: &RouteQuery,
) -> anyhow::Result<Vec<MakerOut>> {
    use velocity_rs::program::state::prop_amm::{QuoterType, QuoterV0};

    let taker = ClobUserRefV0 {
        authority: match query.taker_authority.as_deref() {
            Some(text) => anchor_lang::prelude::Pubkey::new_from_array(
                text.parse::<Pubkey>()
                    .map_err(|err| anyhow::anyhow!("takerAuthority: {err}"))?
                    .to_bytes(),
            ),
            None => anchor_lang::prelude::Pubkey::default(),
        },
        sub_account_id: query.taker_sub_account_id,
    };

    let entry_keys: Vec<Pubkey> = view
        .books
        .iter()
        .filter(|book| book.kind == QuotedSourceKind::Quoter)
        .map(|book| book.key)
        .collect();
    if entry_keys.is_empty() {
        return Ok(Vec::new());
    }
    let entries = source.get_multiple_accounts(&entry_keys).await?;

    let mut books = Vec::new();
    for account in entries.into_iter().flatten() {
        let entry: QuoterV0 = read_zero_copy(&account.data)?;
        if entry.quoter_type == QuoterType::Clob {
            books.push(entry.response_account);
        }
    }
    if books.is_empty() {
        return Ok(Vec::new());
    }

    let side = velocity_rs::clob::swept_side(match direction {
        Direction::Long => velocity_rs::program::controller::position::PositionDirection::Long,
        Direction::Short => velocity_rs::program::controller::position::PositionDirection::Short,
    });
    let mut out = Vec::new();
    for (key, account) in books
        .iter()
        .zip(source.get_multiple_accounts(&books).await?)
    {
        let Some(account) = account else {
            anyhow::bail!("clob market {key} missing");
        };
        // No cap: the endpoint reports what the book holds and the caller
        // decides how much of it fits in a transaction.
        for maker in velocity_rs::clob::resting_makers(
            &account.data,
            side,
            query.size,
            taker,
            view.slot,
            chrono_now(),
            usize::MAX,
        ) {
            let authority = Pubkey::new_from_array(maker.authority.to_bytes());
            let user = Pubkey::find_program_address(
                &[
                    b"user",
                    authority.as_ref(),
                    &maker.sub_account_id.to_le_bytes(),
                ],
                velocity,
            )
            .0;
            let user_stats =
                Pubkey::find_program_address(&[b"user_stats", authority.as_ref()], velocity).0;
            out.push(MakerOut {
                authority: authority.to_string(),
                sub_account_id: maker.sub_account_id,
                user: user.to_string(),
                user_stats: user_stats.to_string(),
            });
        }
    }
    Ok(out)
}

/// Wall clock in seconds, for the book walk's expiry check.
fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}
