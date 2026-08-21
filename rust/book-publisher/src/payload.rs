//! Synthesize the dlob-server wire payloads from a pair of quote views.
//!
//! The TypeScript serving side keeps its Redis contract: one JSON document
//! per market under `last_update_orderbook_perp_{index}` (and the matching
//! pub/sub channel), levels as decimal strings in on-chain precision, a
//! numeric top-level `slot` (the freshness key `selectMostRecentBySlot`
//! picks by), and a per-level `sources` breakdown. The four-source book
//! rides the existing `sources` field with two new keys — `clob` and
//! `propamm` — so consumers that only know `vamm`/`dlob` keep working.
//!
//! Beyond the base L2 document this module mirrors the rest of the TS
//! publisher's surface: the `oracle`/`oracleData`/`mmOracleData`/
//! `marketSlot` decorations (priced by the program's own host-compiled
//! oracle parser, so the published oracle equals the fill-time oracle), the
//! `_grouped_{1,10,100,500,1000}` aggregation channels, the per-order L3
//! document (CLOB orders read straight off the book bytes — the DLOB's L3
//! stays with the TS publisher until the DLOB dies), and the best-makers
//! key (maker `User` PDAs derived from node identity).

use {
    anyhow::{anyhow, Result},
    program::state::{
        oracle::OraclePriceData,
        perp_market::PerpMarket,
        prop_amm::{
            clob_node_capacity, read_clob_node, read_clob_u32, ClobNodeView, CLOB_BEST_ASK_OFFSET,
            CLOB_BEST_BID_OFFSET, CLOB_NIL,
        },
        router_quote::QuotedSourceKind,
        state::State,
    },
    serde_json::{json, Map, Value},
    solana_sdk::pubkey::Pubkey,
    std::collections::BTreeMap,
    velocity_router_sim::quote_view::{CarriedEntry, QuoteView},
};

/// Aggregated levels for one side: price → (source label → size).
type SideLevels = BTreeMap<u64, BTreeMap<&'static str, u128>>;

fn source_label(kind: QuotedSourceKind, key: &Pubkey, clob_quoter: &Pubkey) -> &'static str {
    match kind {
        QuotedSourceKind::Vamm => "vamm",
        QuotedSourceKind::DlobOrder => "dlob",
        QuotedSourceKind::Quoter => {
            if key == clob_quoter {
                "clob"
            } else {
                "propamm"
            }
        }
    }
}

fn aggregate(view: &QuoteView, clob_quoter: &Pubkey) -> SideLevels {
    let mut side: SideLevels = BTreeMap::new();
    for book in &view.books {
        let label = source_label(book.kind, &book.key, clob_quoter);
        for level in &book.levels {
            *side
                .entry(level.price)
                .or_default()
                .entry(label)
                .or_default() += level.size as u128;
        }
    }
    side
}

fn levels_json<'a>(
    levels: impl Iterator<Item = (&'a u64, &'a BTreeMap<&'static str, u128>)>,
) -> Value {
    Value::Array(
        levels
            .map(|(price, sources)| {
                let total: u128 = sources.values().sum();
                let sources: Map<String, Value> = sources
                    .iter()
                    .map(|(label, size)| ((*label).to_string(), json!(size.to_string())))
                    .collect();
                json!({
                    "price": price.to_string(),
                    "size": total.to_string(),
                    "sources": sources,
                })
            })
            .collect(),
    )
}

/// The oracle/market decorations the TS publisher stamps on every book
/// document. Built once per market per tick and shared by L2, the grouped
/// channels (which inherit L2's fields wholesale), and L3.
pub struct Decorations {
    /// Primary oracle price as a plain number — the legacy `oracle` field.
    pub oracle: i64,
    pub oracle_data: Value,
    pub mm_oracle_data: Value,
    pub market_slot: u64,
}

/// The TS side's `SerializableOraclePriceData`: strings for the numerics,
/// with `slot` recovered from the parse-time delay. The optional
/// twap/maxPrice fields are omitted, matching `JSON.stringify` dropping
/// `undefined` members.
fn oracle_data_json(data: &OraclePriceData, clock_slot: u64) -> Value {
    json!({
        "price": data.price.to_string(),
        "slot": clock_slot.saturating_sub(data.delay.max(0) as u64).to_string(),
        "confidence": data.confidence.to_string(),
        "hasSufficientNumberOfDataPoints": data.has_sufficient_number_of_data_points,
    })
}

/// Price the market's oracle the way a fill would: the program's own parser
/// for the primary read, and its MM-oracle gating (freshness, validity,
/// divergence fallback) for the effective `mmOracleData` price.
pub fn build_decorations(
    perp_market: &PerpMarket,
    state: &State,
    oracle_owner: &Pubkey,
    oracle_bytes: &mut [u8],
    clock_slot: u64,
    market_slot: u64,
) -> Result<Decorations> {
    let oracle_data = program::sdk::oracle_price(
        &perp_market.oracle_source,
        &perp_market.oracle,
        oracle_owner,
        oracle_bytes,
        clock_slot,
    )
    .map_err(|e| {
        anyhow!(
            "parse oracle for market {}: {e:?}",
            perp_market.market_index
        )
    })?;
    let mm_oracle_data = perp_market
        .get_mm_oracle_price_data(oracle_data, clock_slot, &state.oracle_guard_rails.validity)
        .map_err(|e| anyhow!("mm oracle for market {}: {e:?}", perp_market.market_index))?
        .get_safe_oracle_price_data();
    Ok(Decorations {
        oracle: oracle_data.price,
        oracle_data: oracle_data_json(&oracle_data, clock_slot),
        mm_oracle_data: oracle_data_json(&mm_oracle_data, clock_slot),
        market_slot,
    })
}

/// Build the L2 document. `asks` is the long-direction view (a buyer
/// consumes asks), `bids` the short-direction view.
pub fn l2_payload(
    market_index: u16,
    market_name: &str,
    clob_quoter: &Pubkey,
    asks: &QuoteView,
    bids: &QuoteView,
    decorations: &Decorations,
    ts_ms: u128,
) -> Value {
    let ask_levels = aggregate(asks, clob_quoter);
    let bid_levels = aggregate(bids, clob_quoter);

    let best_ask = ask_levels.keys().next().copied();
    let best_bid = bid_levels.keys().next_back().copied();
    let mark = match (best_bid, best_ask) {
        (Some(bid), Some(ask)) => Some((bid as u128 + ask as u128) / 2),
        _ => None,
    };
    let spread_quote = match (best_bid, best_ask) {
        (Some(bid), Some(ask)) => Some((ask as i128) - (bid as i128)),
        _ => None,
    };
    // PERCENTAGE_PRECISION (1e6), matching the TS payload's units.
    let spread_pct = match (spread_quote, mark) {
        (Some(spread), Some(mark)) if mark > 0 => Some(spread * 1_000_000 / mark as i128),
        _ => None,
    };
    fn opt_string<T: ToString>(value: Option<T>) -> Value {
        match value {
            Some(value) => json!(value.to_string()),
            None => Value::Null,
        }
    }

    json!({
        // Asks best-first (ascending), bids best-first (descending).
        "asks": levels_json(ask_levels.iter()),
        "bids": levels_json(bid_levels.iter().rev()),
        "marketName": market_name,
        "marketType": "perp",
        "marketIndex": market_index,
        "ts": ts_ms as u64,
        "slot": asks.slot.max(bids.slot),
        "bestAskPrice": opt_string(best_ask),
        "bestBidPrice": opt_string(best_bid),
        // An empty book falls back to the numeric oracle price, exactly as
        // the TS publisher assigns `l2Formatted['oracle']` into markPrice.
        "markPrice": match mark {
            Some(mark) => json!(mark.to_string()),
            None => json!(decorations.oracle),
        },
        "spreadQuote": opt_string(spread_quote),
        "spreadPct": opt_string(spread_pct),
        "oracle": decorations.oracle,
        "oracleData": decorations.oracle_data,
        "mmOracleData": decorations.mm_oracle_data,
        "marketSlot": decorations.market_slot,
    })
}

/// The serving side's grouping ladder: bucket sizes in ticks, each channel
/// suffixed `_grouped_{n}`. Coarser groups aggregate from a finer group's
/// full (unsliced) result, mirroring `GROUPING_DEPENDENCIES`.
pub const GROUPING_OPTIONS: [(u64, Option<u64>); 5] = [
    (1, None),
    (10, Some(1)),
    (100, Some(10)),
    (500, Some(100)),
    (1000, Some(100)),
];

/// One aggregated grouping level: bucket price → (total size, per-source
/// sizes). Sizes are plain numbers on this wire (the TS aggregator works in
/// floats), unlike the base L2's strings.
type GroupedLevels = BTreeMap<u64, (u128, BTreeMap<String, u128>)>;

fn level_u64(level: &Value, field: &str) -> u64 {
    let value = &level[field];
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

fn aggregate_grouping(levels: &[Value], is_ask: bool, precision: u64) -> GroupedLevels {
    let precision = precision.max(1);
    let mut grouped: GroupedLevels = BTreeMap::new();
    for level in levels {
        let price = level_u64(level, "price");
        let bucket = if is_ask {
            price.div_ceil(precision) * precision
        } else {
            price / precision * precision
        };
        let entry = grouped.entry(bucket).or_default();
        entry.0 += level_u64(level, "size") as u128;
        if let Some(sources) = level["sources"].as_object() {
            for (label, size) in sources {
                let size = size
                    .as_u64()
                    .or_else(|| size.as_str().and_then(|s| s.parse().ok()))
                    .unwrap_or(0);
                *entry.1.entry(label.clone()).or_default() += size as u128;
            }
        }
    }
    grouped
}

fn grouped_levels_json<'a>(
    levels: impl Iterator<Item = (&'a u64, &'a (u128, BTreeMap<String, u128>))>,
) -> Vec<Value> {
    levels
        .map(|(price, (size, sources))| {
            let sources: Map<String, Value> = sources
                .iter()
                .map(|(label, size)| (label.clone(), json!(size)))
                .collect();
            json!({ "price": price, "size": size, "sources": sources })
        })
        .collect()
}

/// Build the `_grouped_{n}` channel documents from a finished L2 document:
/// levels bucketed to `n × tick` (bids floor, asks ceil), 20 levels per
/// side plus however many are crossed, every other field inherited.
pub fn grouped_payloads(l2: &Value, tick_size: u64) -> Vec<(u64, Value)> {
    let mut results: BTreeMap<u64, (Vec<Value>, Vec<Value>)> = BTreeMap::new();
    let mut documents = Vec::new();
    for (group, dependency) in GROUPING_OPTIONS {
        let precision = group.saturating_mul(tick_size);
        let (bid_input, ask_input) = match dependency.and_then(|d| results.get(&d)) {
            Some((bids, asks)) => (bids.as_slice(), asks.as_slice()),
            None => (
                l2["bids"].as_array().map(Vec::as_slice).unwrap_or(&[]),
                l2["asks"].as_array().map(Vec::as_slice).unwrap_or(&[]),
            ),
        };
        let bids = aggregate_grouping(bid_input, false, precision);
        let asks = aggregate_grouping(ask_input, true, precision);
        let full_bids = grouped_levels_json(bids.iter().rev());
        let full_asks = grouped_levels_json(asks.iter());

        // Keep 20 levels plus the crossed prefix, so a crossed book still
        // shows its first uncrossed levels.
        let best_ask = asks.keys().next().copied();
        let best_bid = bids.keys().next_back().copied();
        let crossed_bids = best_ask
            .map(|ask| bids.keys().filter(|bid| **bid >= ask).count())
            .unwrap_or(0);
        let crossed_asks = best_bid
            .map(|bid| asks.keys().filter(|ask| **ask <= bid).count())
            .unwrap_or(0);
        let depth = 20 + crossed_bids.max(crossed_asks);

        let mut document = l2.clone();
        document["bids"] = Value::Array(full_bids.iter().take(depth).cloned().collect());
        document["asks"] = Value::Array(full_asks.iter().take(depth).cloned().collect());
        documents.push((group, document));
        results.insert(group, (full_bids, full_asks));
    }
    documents
}

fn clob_user_pda(velocity: &Pubkey, node: &ClobNodeView) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"user",
            node.authority.as_ref(),
            node.sub_account_id.to_le_bytes().as_ref(),
        ],
        velocity,
    )
    .0
}

/// Walk one side of the CLOB best-first, collecting matchable nodes.
/// Iterations are capped at the arena capacity so corrupt next-pointers
/// can't loop forever.
fn walk_clob_side(data: &[u8], head_offset: usize, slot: u64, now: i64) -> Vec<ClobNodeView> {
    let mut nodes = Vec::new();
    let Some(mut cursor) = read_clob_u32(data, head_offset) else {
        return nodes;
    };
    let mut remaining = clob_node_capacity(data.len());
    while cursor != CLOB_NIL && remaining > 0 {
        remaining -= 1;
        let Some(node) = read_clob_node(data, cursor) else {
            break;
        };
        cursor = node.next;
        if node.is_matchable(slot, now) {
            nodes.push(node);
        }
    }
    nodes
}

/// One attributed line of a book.
///
/// A CLOB row is a resting order: it has an id, a queue position, and it can
/// be cancelled. A PropAMM row is none of those. It is a quote the maker
/// would honour at the size the view was taken at, so it carries a maker but
/// no id, and it means nothing without the document's `quotedSize`. The
/// `source` field is what lets a consumer tell them apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookRow {
    pub price: u64,
    pub size: u64,
    pub maker: Pubkey,
    pub quoter: Option<Pubkey>,
    pub order_id: Option<u64>,
    pub source: &'static str,
}

impl BookRow {
    fn to_json(&self) -> Value {
        json!({
            "price": self.price.to_string(),
            "size": self.size.to_string(),
            "maker": self.maker.to_string(),
            "orderId": self.order_id,
            "quoter": self.quoter.map(|key| key.to_string()),
            "source": self.source,
        })
    }
}

/// Resting CLOB orders on one side, best first.
pub fn clob_rows(
    velocity: &Pubkey,
    book_data: &[u8],
    head_offset: usize,
    slot: u64,
    now: i64,
) -> Vec<BookRow> {
    walk_clob_side(book_data, head_offset, slot, now)
        .iter()
        .map(|node| BookRow {
            price: node.price,
            size: node.base_asset_amount,
            maker: clob_user_pda(velocity, node),
            quoter: None,
            order_id: Some(node.order_id),
            source: "clob",
        })
        .collect()
}

/// PropAMM levels on one side, attributed to the maker each entry names.
///
/// A Custom entry settles against exactly one `User`, fixed at registration,
/// so every level it quotes belongs to that maker. A CLOB entry names its
/// registrant rather than the makers resting on its book, so it is skipped
/// here; its depth is attributed per order by [`clob_rows`].
pub fn propamm_rows(view: &QuoteView, entries: &[CarriedEntry]) -> Vec<BookRow> {
    view.books
        .iter()
        .filter(|book| book.kind == QuotedSourceKind::Quoter)
        .filter_map(|book| {
            let entry = entries.iter().find(|entry| entry.quoter == book.key)?;
            entry.attributes_to_one_maker().then_some((book, entry))
        })
        .flat_map(|(book, entry)| {
            book.levels.iter().map(move |level| BookRow {
                price: level.price,
                size: level.size,
                maker: entry.user,
                quoter: Some(entry.quoter),
                order_id: None,
                source: "propamm",
            })
        })
        .collect()
}

/// Merge two best-first sides into one, still best first.
fn merge_side(is_ask: bool, mut rows: Vec<BookRow>) -> Vec<BookRow> {
    rows.sort_by(|a, b| {
        if is_ask {
            a.price.cmp(&b.price)
        } else {
            b.price.cmp(&a.price)
        }
    });
    rows
}

/// Build the per-order L3 document.
///
/// The CLOB contributes resting orders and PropAMMs contribute attributed
/// quotes; the two are distinguished by `source`, because only the first can
/// be cancelled or holds a queue position. The vAMM contributes nothing: it
/// has no maker to attribute to. DLOB L3 stays with the TypeScript publisher
/// until the DLOB dies.
#[allow(clippy::too_many_arguments)]
pub fn l3_payload(
    velocity: &Pubkey,
    market_index: u16,
    market_name: &str,
    book_data: &[u8],
    slot: u64,
    now: i64,
    decorations: &Decorations,
    ts_ms: u128,
    propamm_bids: Vec<BookRow>,
    propamm_asks: Vec<BookRow>,
    quoted_size: u64,
) -> Value {
    let mut bids = clob_rows(velocity, book_data, CLOB_BEST_BID_OFFSET, slot, now);
    bids.extend(propamm_bids);
    let mut asks = clob_rows(velocity, book_data, CLOB_BEST_ASK_OFFSET, slot, now);
    asks.extend(propamm_asks);
    let render = |rows: Vec<BookRow>| -> Vec<Value> { rows.iter().map(BookRow::to_json).collect() };
    json!({
        "bids": render(merge_side(false, bids)),
        "asks": render(merge_side(true, asks)),
        "marketName": market_name,
        "marketType": "perp",
        "marketIndex": market_index,
        // PropAMM rows are a quote at this size, not a standing book. Their
        // depth means nothing without it.
        "quotedSize": quoted_size.to_string(),
        "ts": ts_ms as u64,
        "slot": slot,
        "oracle": decorations.oracle,
        "oracleData": decorations.oracle_data,
        "mmOracleData": decorations.mm_oracle_data,
        "marketSlot": decorations.market_slot,
    })
}

/// Makers per side on the best-makers key, matching the TS publisher's
/// `numMakers`.
const BEST_MAKERS: usize = 4;

/// Build the `last_update_orderbook_best_makers_*` document: the first
/// distinct maker `User` PDAs on each side, best first.
///
/// PropAMM makers count. A taker routed to the best price does not care
/// whether the maker behind it rested an order or quoted on request, and a
/// list that named only resting makers would miss whoever is actually on the
/// top of book.
pub fn best_makers_payload(
    velocity: &Pubkey,
    book_data: &[u8],
    slot: u64,
    now: i64,
    propamm_bids: Vec<BookRow>,
    propamm_asks: Vec<BookRow>,
) -> Value {
    let side = |head_offset: usize, is_ask: bool, propamm: Vec<BookRow>| -> Vec<String> {
        let mut rows = clob_rows(velocity, book_data, head_offset, slot, now);
        rows.extend(propamm);
        let mut makers: Vec<String> = Vec::new();
        for row in merge_side(is_ask, rows) {
            let maker = row.maker.to_string();
            if !makers.contains(&maker) {
                makers.push(maker);
                if makers.len() >= BEST_MAKERS {
                    break;
                }
            }
        }
        makers
    };
    json!({
        "bids": side(CLOB_BEST_BID_OFFSET, false, propamm_bids),
        "asks": side(CLOB_BEST_ASK_OFFSET, true, propamm_asks),
        "slot": slot,
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*, program::state::router_quote::QuotedLevelV0,
        velocity_router_sim::quote_view::QuotedBook,
    };

    fn view(direction: u8, books: Vec<QuotedBook>) -> QuoteView {
        QuoteView {
            market: 0,
            direction,
            quoted_size: 1_000,
            slot: 100,
            books,
        }
    }

    fn book(kind: QuotedSourceKind, key: Pubkey, levels: &[(u64, u64)]) -> QuotedBook {
        QuotedBook {
            key,
            kind,
            priority: 0,
            clamped: false,
            levels: levels
                .iter()
                .map(|(price, size)| QuotedLevelV0 {
                    price: *price,
                    size: *size,
                })
                .collect(),
        }
    }

    fn decorations() -> Decorations {
        Decorations {
            oracle: 98,
            oracle_data: json!({"price": "98", "slot": "100", "confidence": "1", "hasSufficientNumberOfDataPoints": true}),
            mm_oracle_data: json!({"price": "98", "slot": "100", "confidence": "1", "hasSufficientNumberOfDataPoints": true}),
            market_slot: 100,
        }
    }

    #[test]
    fn levels_merge_across_sources_with_a_per_source_breakdown() {
        let clob_quoter = Pubkey::new_unique();
        let vamm_key = Pubkey::new_unique();
        let asks = view(
            0,
            vec![
                book(QuotedSourceKind::Quoter, clob_quoter, &[(99, 5), (100, 3)]),
                book(QuotedSourceKind::Vamm, vamm_key, &[(99, 2), (101, 9)]),
            ],
        );
        let bids = view(
            1,
            vec![book(QuotedSourceKind::Quoter, clob_quoter, &[(97, 4)])],
        );

        let payload = l2_payload(
            7,
            "SOL-PERP",
            &clob_quoter,
            &asks,
            &bids,
            &decorations(),
            1_234,
        );
        assert_eq!(payload["marketIndex"], 7);
        assert_eq!(payload["slot"], 100);
        // Decorations ride every document.
        assert_eq!(payload["oracle"], 98);
        assert_eq!(payload["oracleData"]["price"], "98");
        assert_eq!(payload["mmOracleData"]["slot"], "100");
        assert_eq!(payload["marketSlot"], 100);
        let ask_levels = payload["asks"].as_array().unwrap();
        // Shared price level 99 merged, sources split out.
        assert_eq!(ask_levels[0]["price"], "99");
        assert_eq!(ask_levels[0]["size"], "7");
        assert_eq!(ask_levels[0]["sources"]["clob"], "5");
        assert_eq!(ask_levels[0]["sources"]["vamm"], "2");
        // Ascending asks.
        assert_eq!(ask_levels[1]["price"], "100");
        assert_eq!(ask_levels[2]["price"], "101");
        // Bids best-first (descending) and decorations present.
        assert_eq!(payload["bids"][0]["price"], "97");
        assert_eq!(payload["bestAskPrice"], "99");
        assert_eq!(payload["bestBidPrice"], "97");
        assert_eq!(payload["markPrice"], "98");
        assert_eq!(payload["spreadQuote"], "2");
        // 2 / 98 in PERCENTAGE_PRECISION.
        assert_eq!(payload["spreadPct"], "20408");
    }

    #[test]
    fn non_canonical_quoters_label_as_propamm() {
        let clob_quoter = Pubkey::new_unique();
        let custom = Pubkey::new_unique();
        let asks = view(0, vec![book(QuotedSourceKind::Quoter, custom, &[(100, 1)])]);
        let bids = view(1, vec![]);
        let payload = l2_payload(0, "X", &clob_quoter, &asks, &bids, &decorations(), 0);
        assert_eq!(payload["asks"][0]["sources"]["propamm"], "1");
        assert!(payload["bestBidPrice"].is_null());
        // One-sided book has no mark: falls back to the numeric oracle.
        assert_eq!(payload["markPrice"], 98);
    }

    #[test]
    fn groupings_bucket_away_from_the_touch_and_inherit_the_document() {
        let clob_quoter = Pubkey::new_unique();
        // Tick = 2: prices 99/101 bucket to 98/100 (bid floor) and
        // 100/102 (ask ceil) at group 1.
        let asks = view(
            0,
            vec![book(
                QuotedSourceKind::Quoter,
                clob_quoter,
                &[(99, 5), (101, 3)],
            )],
        );
        let bids = view(
            1,
            vec![book(
                QuotedSourceKind::Quoter,
                clob_quoter,
                &[(97, 4), (95, 2)],
            )],
        );
        let l2 = l2_payload(3, "SOL-PERP", &clob_quoter, &asks, &bids, &decorations(), 9);
        let grouped = grouped_payloads(&l2, 2);
        assert_eq!(grouped.len(), GROUPING_OPTIONS.len());

        let (group, doc) = &grouped[0];
        assert_eq!(*group, 1);
        // Asks ceil to the tick: 99→100, 101→102.
        assert_eq!(doc["asks"][0]["price"], 100);
        assert_eq!(doc["asks"][0]["size"], 5);
        assert_eq!(doc["asks"][0]["sources"]["clob"], 5);
        assert_eq!(doc["asks"][1]["price"], 102);
        // Bids floor: 97→96, 95→94, best-first.
        assert_eq!(doc["bids"][0]["price"], 96);
        assert_eq!(doc["bids"][1]["price"], 94);
        // The rest of the L2 document is inherited.
        assert_eq!(doc["marketIndex"], 3);
        assert_eq!(doc["oracle"], 98);

        // Group 10 (bucket 20), fed by group 1's output: asks 100/102 →
        // 100/120 (100 is already on the bucket); bids 96/94 merge at 80.
        let (group, doc) = &grouped[1];
        assert_eq!(*group, 10);
        assert_eq!(doc["asks"][0]["price"], 100);
        assert_eq!(doc["asks"][0]["size"], 5);
        assert_eq!(doc["asks"][1]["price"], 120);
        assert_eq!(doc["asks"][1]["size"], 3);
        assert_eq!(doc["bids"].as_array().unwrap().len(), 1);
        assert_eq!(doc["bids"][0]["price"], 80);
        assert_eq!(doc["bids"][0]["size"], 6);
        assert_eq!(doc["bids"][0]["sources"]["clob"], 6);
    }

    // A minimal CLOB market image: header with side heads, node arena.
    fn clob_book(
        bids: &[(Pubkey, u16, u64, u64, u64)],
        asks: &[(Pubkey, u16, u64, u64, u64)],
    ) -> Vec<u8> {
        use program::state::prop_amm::{
            CLOB_NODE_LEN, CLOB_ORDERS_OFFSET, CLOB_ORDER_BIT_FLAG_OPEN,
        };
        let total = bids.len() + asks.len();
        let mut data = vec![0u8; CLOB_ORDERS_OFFSET + total * CLOB_NODE_LEN];
        let mut write_side = |orders: &[(Pubkey, u16, u64, u64, u64)],
                              head_offset: usize,
                              base_index: usize| {
            let head = if orders.is_empty() {
                CLOB_NIL
            } else {
                base_index as u32
            };
            data[head_offset..head_offset + 4].copy_from_slice(&head.to_le_bytes());
            for (i, (authority, sub_account_id, price, size, order_id)) in orders.iter().enumerate()
            {
                let index = base_index + i;
                let next = if i + 1 < orders.len() {
                    (index + 1) as u32
                } else {
                    CLOB_NIL
                };
                let node = CLOB_ORDERS_OFFSET + index * CLOB_NODE_LEN;
                data[node..node + 32].copy_from_slice(authority.as_ref());
                data[node + 32..node + 40].copy_from_slice(&price.to_le_bytes());
                data[node + 40..node + 48].copy_from_slice(&size.to_le_bytes());
                // activation_slot 0, max_ts 0 (never expires).
                data[node + 64..node + 72].copy_from_slice(&order_id.to_le_bytes());
                data[node + 84..node + 88].copy_from_slice(&next.to_le_bytes());
                data[node + 88] = CLOB_ORDER_BIT_FLAG_OPEN;
                data[node + 90..node + 92].copy_from_slice(&sub_account_id.to_le_bytes());
            }
        };
        write_side(bids, CLOB_BEST_BID_OFFSET, 0);
        write_side(asks, CLOB_BEST_ASK_OFFSET, bids.len());
        data
    }

    #[test]
    fn l3_reads_orders_off_the_book_with_derived_makers() {
        let velocity = Pubkey::new_unique();
        let maker_a = Pubkey::new_unique();
        let maker_b = Pubkey::new_unique();
        let data = clob_book(
            &[(maker_a, 0, 97, 4, 11), (maker_b, 2, 96, 2, 12)],
            &[(maker_a, 0, 99, 5, 13)],
        );
        let payload = l3_payload(
            &velocity,
            3,
            "SOL-PERP",
            &data,
            50,
            1_000,
            &decorations(),
            7,
            Vec::new(),
            Vec::new(),
            1_000,
        );
        assert_eq!(payload["bids"][0]["price"], "97");
        assert_eq!(payload["bids"][0]["size"], "4");
        assert_eq!(payload["bids"][0]["orderId"], 11);
        assert_eq!(payload["bids"][1]["price"], "96");
        assert_eq!(payload["asks"][0]["orderId"], 13);
        // Maker = the derived User PDA of (authority, sub_account_id).
        let expected = Pubkey::find_program_address(
            &[b"user", maker_a.as_ref(), 0u16.to_le_bytes().as_ref()],
            &velocity,
        )
        .0;
        assert_eq!(payload["bids"][0]["maker"], expected.to_string());
        assert_eq!(payload["asks"][0]["maker"], expected.to_string());
        assert_ne!(payload["bids"][1]["maker"], expected.to_string());
        assert_eq!(payload["marketIndex"], 3);
        assert_eq!(payload["slot"], 50);
        assert_eq!(payload["oracleData"]["price"], "98");
    }

    #[test]
    fn best_makers_are_distinct_and_capped() {
        let velocity = Pubkey::new_unique();
        let makers: Vec<Pubkey> = (0..6).map(|_| Pubkey::new_unique()).collect();
        // Maker 0 owns the two best bids — deduped to one entry; six
        // distinct asks cap at four.
        let bids: Vec<(Pubkey, u16, u64, u64, u64)> = vec![
            (makers[0], 0, 99, 1, 1),
            (makers[0], 0, 98, 1, 2),
            (makers[1], 0, 97, 1, 3),
        ];
        let asks: Vec<(Pubkey, u16, u64, u64, u64)> = makers
            .iter()
            .enumerate()
            .map(|(i, maker)| (*maker, 0u16, 100 + i as u64, 1u64, 10 + i as u64))
            .collect();
        let data = clob_book(&bids, &asks);
        let payload = best_makers_payload(&velocity, &data, 50, 1_000, Vec::new(), Vec::new());
        assert_eq!(payload["bids"].as_array().unwrap().len(), 2);
        assert_eq!(payload["asks"].as_array().unwrap().len(), 4);
        assert_eq!(payload["slot"], 50);
        let bid_maker = Pubkey::find_program_address(
            &[b"user", makers[0].as_ref(), 0u16.to_le_bytes().as_ref()],
            &velocity,
        )
        .0;
        assert_eq!(payload["bids"][0], bid_maker.to_string());
    }

    fn custom(quoter: Pubkey, user: Pubkey) -> CarriedEntry {
        CarriedEntry {
            quoter,
            program: Pubkey::new_unique(),
            user,
            quoter_type: program::state::prop_amm::QuoterType::Custom,
        }
    }

    #[test]
    fn a_propamm_level_carries_the_maker_its_entry_names() {
        // A Custom entry settles against exactly one User, so every level it
        // quotes belongs to that maker even though no order rests anywhere.
        let quoter = Pubkey::new_unique();
        let user = Pubkey::new_unique();
        let rows = propamm_rows(
            &view(0, vec![book(QuotedSourceKind::Quoter, quoter, &[(100, 5)])]),
            &[custom(quoter, user)],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].maker, user);
        assert_eq!(rows[0].quoter, Some(quoter));
        assert_eq!(rows[0].source, "propamm");
        assert!(
            rows[0].order_id.is_none(),
            "a quote is not an order and has no id to cancel"
        );
    }

    #[test]
    fn a_clob_entrys_levels_are_not_attributed_to_its_registrant() {
        // The entry names whoever registered the book, not the makers
        // resting on it. Those are attributed per order off the book bytes.
        let quoter = Pubkey::new_unique();
        let entry = CarriedEntry {
            quoter,
            program: Pubkey::new_unique(),
            user: Pubkey::new_unique(),
            quoter_type: program::state::prop_amm::QuoterType::Clob,
        };
        let rows = propamm_rows(
            &view(0, vec![book(QuotedSourceKind::Quoter, quoter, &[(100, 5)])]),
            &[entry],
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn the_vamm_contributes_no_attributed_levels() {
        // There is no maker behind the curve to name.
        let rows = propamm_rows(
            &view(
                0,
                vec![book(
                    QuotedSourceKind::Vamm,
                    Pubkey::new_unique(),
                    &[(100, 5)],
                )],
            ),
            &[],
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn an_unknown_entry_is_never_attributed() {
        // A source whose entry did not come back from the registry read has
        // no maker the publisher can stand behind.
        let rows = propamm_rows(
            &view(
                0,
                vec![book(
                    QuotedSourceKind::Quoter,
                    Pubkey::new_unique(),
                    &[(100, 5)],
                )],
            ),
            &[],
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn merged_sides_stay_best_first() {
        let row = |price: u64, source: &'static str| BookRow {
            price,
            size: 1,
            maker: Pubkey::new_unique(),
            quoter: None,
            order_id: None,
            source,
        };
        let asks = merge_side(
            true,
            vec![row(102, "clob"), row(100, "propamm"), row(101, "clob")],
        );
        assert_eq!(
            asks.iter().map(|r| r.price).collect::<Vec<_>>(),
            vec![100, 101, 102],
            "an ask book reads cheapest first"
        );
        let bids = merge_side(
            false,
            vec![row(98, "clob"), row(100, "propamm"), row(99, "clob")],
        );
        assert_eq!(
            bids.iter().map(|r| r.price).collect::<Vec<_>>(),
            vec![100, 99, 98],
            "a bid book reads dearest first"
        );
    }

    #[test]
    fn a_propamm_on_the_top_of_book_is_named_among_the_best_makers() {
        // A taker routed to the best price does not care whether the maker
        // rested an order or quoted on request.
        let user = Pubkey::new_unique();
        let propamm = vec![BookRow {
            price: 1_000,
            size: 5,
            maker: user,
            quoter: Some(Pubkey::new_unique()),
            order_id: None,
            source: "propamm",
        }];
        let payload = best_makers_payload(
            &Pubkey::new_unique(),
            &[0u8; 0],
            50,
            1_000,
            propamm.clone(),
            propamm,
        );
        assert_eq!(payload["bids"][0], json!(user.to_string()));
        assert_eq!(payload["asks"][0], json!(user.to_string()));
    }
}
