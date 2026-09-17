//! Synthesize the dlob-server wire payloads from a pair of quote views.
//!
//! The TypeScript serving side keeps its Redis contract. That contract is one
//! JSON document per market under `last_update_orderbook_perp_{index}` and the
//! matching pub/sub channel, levels as decimal strings in on-chain precision, a
//! numeric top-level `slot` that the freshness key `selectMostRecentBySlot`
//! picks by, and a per-level `sources` breakdown. The four-source book rides
//! the existing `sources` field with two added keys, `clob` and `propamm`, so
//! a consumer that only knows `vamm` and `dlob` keeps working.
//!
//! Beyond the base L2 document this module mirrors the rest of the TypeScript
//! publisher's surface. That is the `oracle`, `oracleData`, `mmOracleData` and
//! `marketSlot` decorations, priced by the program's own host-compiled oracle
//! parser so the published oracle equals the fill-time oracle. It is also the
//! `_grouped_{1,10,100,500,1000}` aggregation channels, the per-order L3
//! document, and the best-makers key whose maker `User` PDAs derive from the
//! identity a row carries. The DLOB's L3 stays with the TypeScript publisher
//! until the DLOB dies.

use {
    anyhow::{anyhow, Result},
    program::state::{
        oracle::OraclePriceData, perp_market::PerpMarket, prop_amm::ClobUserRefV0,
        router_quote::QuotedSourceKind, state::State,
    },
    serde_json::{json, Map, Value},
    solana_sdk::pubkey::Pubkey,
    std::collections::BTreeMap,
    velocity_router_sim::quote_view::{CarriedEntry, QuoteView},
};

/// Aggregated levels for one side: price → (source label → size).
type SideLevels = BTreeMap<u64, BTreeMap<&'static str, u128>>;

fn source_label(kind: QuotedSourceKind, key: &Pubkey, clob_entry: &Pubkey) -> &'static str {
    match kind {
        QuotedSourceKind::Vamm => "vamm",
        QuotedSourceKind::DlobOrder => "dlob",
        QuotedSourceKind::Quoter => {
            if key == clob_entry {
                "clob"
            } else {
                "propamm"
            }
        }
    }
}

fn aggregate(view: &QuoteView, clob_entry: &Pubkey) -> SideLevels {
    let mut side: SideLevels = BTreeMap::new();
    for book in &view.books {
        let label = source_label(book.kind, &book.key, clob_entry);
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
    /// Primary oracle price as a plain number, for the legacy `oracle` field.
    pub oracle: i64,
    pub oracle_data: Value,
    pub mm_oracle_data: Value,
    pub market_slot: u64,
}

/// The TypeScript side's `SerializableOraclePriceData`. The numerics are
/// strings, and `slot` is recovered from the parse-time delay. The optional
/// twap and maxPrice fields are omitted, which matches `JSON.stringify`
/// dropping `undefined` members.
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
        .get_mm_oracle_price_data(
            oracle_data,
            clock_slot,
            &state.oracle_guard_rails.validity,
            state.slot_clock(),
        )
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
    clob_entry: &Pubkey,
    asks: &QuoteView,
    bids: &QuoteView,
    decorations: &Decorations,
    ts_ms: u128,
) -> Value {
    let ask_levels = aggregate(asks, clob_entry);
    let bid_levels = aggregate(bids, clob_entry);

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

/// The `User` a wire identity derives to. Both PDAs come off the same pair,
/// which is why the book stores identity this way.
fn user_pda(velocity: &Pubkey, user: &ClobUserRefV0) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"user",
            user.authority.as_ref(),
            user.sub_account_id.to_le_bytes().as_ref(),
        ],
        velocity,
    )
    .0
}

/// One attributed line of a book: depth, and the account that settles it.
///
/// Every source a taker can reach appears here, the same way every one of
/// them appears in the L2 ladder — a reader asking who is in this book wants
/// the whole picture, not the resting half of it.
///
/// What differs between them is what a row *is*. A CLOB row is a resting
/// order: it has an id, a queue position, and it can be cancelled. A PropAMM
/// row is a quote its maker would honour at the size the view was taken at,
/// so it carries a maker but no id, and it means nothing without the
/// document's `quotedSize`. A vAMM row is a rung of a curve, settled by the
/// market itself. The `source` field is what lets a consumer tell them apart,
/// and `orderId` is present on exactly the rows something can be done to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookRow {
    pub price: u64,
    pub size: u64,
    /// The account this depth settles against: a maker's `User`, or the perp
    /// market for the vAMM, whose counterparty is the market's own AMM.
    pub maker: Pubkey,
    pub quoter: Option<Pubkey>,
    pub order_id: Option<u64>,
    /// Routing tier, which is the order the router fills sources in at a
    /// shared price. Not published — it is what sorts a price level.
    pub priority: u8,
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
/// Every attributed line of one side, out of the quote view.
///
/// The view answers this now: each source carries the orders behind its
/// ladder, with the user each settles against — a book's own orders through
/// its `quote_l3_v0` leg, and a quoter that fills from one account as its
/// ladder against that account. So the publisher reads rows rather than
/// decoding a book, and a book may change its data structures without
/// changing this file.
pub fn view_rows(velocity: &Pubkey, view: &QuoteView, entries: &[CarriedEntry]) -> Vec<BookRow> {
    view.books
        .iter()
        .flat_map(|book| {
            let quoter = (book.kind == QuotedSourceKind::Quoter).then_some(book.key);
            let source = match book.kind {
                QuotedSourceKind::Quoter => entries
                    .iter()
                    .find(|entry| entry.quoter == book.key)
                    .map(|entry| {
                        if entry.attributes_to_one_maker() {
                            "propamm"
                        } else {
                            "clob"
                        }
                    })
                    .unwrap_or("propamm"),
                QuotedSourceKind::DlobOrder => "dlob",
                QuotedSourceKind::Vamm => "vamm",
            };
            // The vAMM names no user because it settles against the market's
            // own AMM, so its rungs are attributed to the market account and
            // taken from the ladder itself. Every other source described its
            // depth in rows.
            let vamm_rows: Vec<BookRow> = if book.kind == QuotedSourceKind::Vamm {
                book.levels
                    .iter()
                    .map(|level| BookRow {
                        price: level.price,
                        size: level.size,
                        maker: book.key,
                        quoter: None,
                        order_id: None,
                        priority: book.priority,
                        source,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            book.rows
                .iter()
                .map(move |row| BookRow {
                    price: row.price,
                    size: row.size,
                    maker: user_pda(velocity, &row.user),
                    quoter,
                    // A book row is an order: it has an id, a queue position
                    // and it can be cancelled. A quoted rung is none of those.
                    order_id: (row.order_id != 0).then_some(row.order_id),
                    priority: book.priority,
                    source,
                })
                .chain(vamm_rows)
        })
        .collect()
}

/// Merge two best-first sides into one, still best first.
/// Order the rows the way a taker would reach them: best price first, and
/// within a price, the order the router fills them in.
///
/// The tie-break is the routing tier, not the source's name — at a shared
/// price the split walks tiers ascending (the vAMM, then the book, then
/// customs), so listing them any other way would show a queue that does not
/// exist. The sort is stable, so rows inside one source keep the order that
/// source reported them in, which for a book is its own price-time queue.
fn merge_side(is_ask: bool, mut rows: Vec<BookRow>) -> Vec<BookRow> {
    rows.sort_by(|a, b| {
        if is_ask {
            a.price.cmp(&b.price)
        } else {
            b.price.cmp(&a.price)
        }
        .then(a.priority.cmp(&b.priority))
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
#[allow(clippy::too_many_arguments)]
pub fn l3_payload(
    market_index: u16,
    market_name: &str,
    slot: u64,
    decorations: &Decorations,
    ts_ms: u128,
    bids: Vec<BookRow>,
    asks: Vec<BookRow>,
    quoted_size: u64,
) -> Value {
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
pub fn best_makers_payload(slot: u64, bids: Vec<BookRow>, asks: Vec<BookRow>) -> Value {
    let side = |is_ask: bool, rows: Vec<BookRow>| -> Vec<String> {
        // The vAMM's rung is attributed to the market, which is not an
        // account a taker carries. This key answers "whose accounts does a
        // fill need", so the market is not one of them.
        let rows: Vec<BookRow> = rows
            .into_iter()
            .filter(|row| row.source != "vamm")
            .collect();
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
        "bids": side(false, bids),
        "asks": side(true, asks),
        "slot": slot,
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        program::state::{prop_amm::ClobUserRefV0 as UserRefV0, router_quote::QuotedLevelV0},
        velocity_router_sim::quote_view::{QuotedBook, QuotedRow},
    };

    fn view(direction: u8, books: Vec<QuotedBook>) -> QuoteView {
        QuoteView {
            market: 0,
            direction,
            quoted_size: 1_000,
            slot: 100,
            books,
            rows_truncated: false,
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
            rows: Vec::new(),
        }
    }

    /// A source that says who its depth belongs to: `(price, size, order id)`
    /// against one user, which is what a book's L3 leg reports per order and
    /// what the view attributes for everyone else.
    fn book_with_rows(
        kind: QuotedSourceKind,
        key: Pubkey,
        rows: &[(u64, u64, u64, UserRefV0)],
    ) -> QuotedBook {
        let levels: Vec<(u64, u64)> = rows
            .iter()
            .map(|(price, size, _, _)| (*price, *size))
            .collect();
        QuotedBook {
            rows: rows
                .iter()
                .map(|(price, size, order_id, user)| QuotedRow {
                    price: *price,
                    size: *size,
                    order_id: *order_id,
                    user: *user,
                    flags: 0,
                })
                .collect(),
            ..book(kind, key, &levels)
        }
    }

    fn user_ref(seed: u8, sub_account_id: u16) -> UserRefV0 {
        UserRefV0 {
            authority: anchor_lang::prelude::Pubkey::new_from_array([seed; 32]),
            sub_account_id,
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
        let clob_entry = Pubkey::new_unique();
        let vamm_key = Pubkey::new_unique();
        let asks = view(
            0,
            vec![
                book(QuotedSourceKind::Quoter, clob_entry, &[(99, 5), (100, 3)]),
                book(QuotedSourceKind::Vamm, vamm_key, &[(99, 2), (101, 9)]),
            ],
        );
        let bids = view(
            1,
            vec![book(QuotedSourceKind::Quoter, clob_entry, &[(97, 4)])],
        );

        let payload = l2_payload(
            7,
            "SOL-PERP",
            &clob_entry,
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
        let clob_entry = Pubkey::new_unique();
        let custom = Pubkey::new_unique();
        let asks = view(0, vec![book(QuotedSourceKind::Quoter, custom, &[(100, 1)])]);
        let bids = view(1, vec![]);
        let payload = l2_payload(0, "X", &clob_entry, &asks, &bids, &decorations(), 0);
        assert_eq!(payload["asks"][0]["sources"]["propamm"], "1");
        assert!(payload["bestBidPrice"].is_null());
        // One-sided book has no mark: falls back to the numeric oracle.
        assert_eq!(payload["markPrice"], 98);
    }

    #[test]
    fn groupings_bucket_away_from_the_touch_and_inherit_the_document() {
        let clob_entry = Pubkey::new_unique();
        // Tick = 2: prices 99/101 bucket to 98/100 (bid floor) and
        // 100/102 (ask ceil) at group 1.
        let asks = view(
            0,
            vec![book(
                QuotedSourceKind::Quoter,
                clob_entry,
                &[(99, 5), (101, 3)],
            )],
        );
        let bids = view(
            1,
            vec![book(
                QuotedSourceKind::Quoter,
                clob_entry,
                &[(97, 4), (95, 2)],
            )],
        );
        let l2 = l2_payload(3, "SOL-PERP", &clob_entry, &asks, &bids, &decorations(), 9);
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

    #[test]
    fn l3_reports_the_orders_the_view_described_with_derived_makers() {
        let velocity = Pubkey::new_unique();
        let quoter = Pubkey::new_unique();
        let a = user_ref(1, 0);
        let b = user_ref(2, 2);
        let bids = view_rows(
            &velocity,
            &view(
                1,
                vec![book_with_rows(
                    QuotedSourceKind::Quoter,
                    quoter,
                    &[(97, 4, 11, a), (96, 2, 12, b)],
                )],
            ),
            &[clob(quoter)],
        );
        let asks = view_rows(
            &velocity,
            &view(
                0,
                vec![book_with_rows(
                    QuotedSourceKind::Quoter,
                    quoter,
                    &[(99, 5, 13, a)],
                )],
            ),
            &[clob(quoter)],
        );
        let payload = l3_payload(3, "SOL-PERP", 50, &decorations(), 7, bids, asks, 1_000);

        assert_eq!(payload["bids"][0]["price"], "97");
        assert_eq!(payload["bids"][0]["size"], "4");
        assert_eq!(payload["bids"][0]["orderId"], 11);
        assert_eq!(payload["bids"][1]["price"], "96");
        assert_eq!(payload["asks"][0]["orderId"], 13);
        // Maker = the derived `User` PDA of (authority, sub_account_id).
        let expected = Pubkey::find_program_address(
            &[b"user", a.authority.as_ref(), 0u16.to_le_bytes().as_ref()],
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
        let quoter = Pubkey::new_unique();
        let makers: Vec<UserRefV0> = (0..6).map(|seed| user_ref(seed as u8 + 1, 0)).collect();
        // The first maker owns the two best bids — deduped to one entry; six
        // distinct asks cap at four.
        let bids = view_rows(
            &velocity,
            &view(
                1,
                vec![book_with_rows(
                    QuotedSourceKind::Quoter,
                    quoter,
                    &[
                        (99, 1, 1, makers[0]),
                        (98, 1, 2, makers[0]),
                        (97, 1, 3, makers[1]),
                    ],
                )],
            ),
            &[clob(quoter)],
        );
        let ask_rows: Vec<(u64, u64, u64, UserRefV0)> = makers
            .iter()
            .enumerate()
            .map(|(i, maker)| (100 + i as u64, 1, 10 + i as u64, *maker))
            .collect();
        let asks = view_rows(
            &velocity,
            &view(
                0,
                vec![book_with_rows(QuotedSourceKind::Quoter, quoter, &ask_rows)],
            ),
            &[clob(quoter)],
        );

        let payload = best_makers_payload(50, bids, asks);
        assert_eq!(payload["bids"].as_array().unwrap().len(), 2);
        assert_eq!(payload["asks"].as_array().unwrap().len(), 4);
        assert_eq!(payload["slot"], 50);
        let bid_maker = Pubkey::find_program_address(
            &[
                b"user",
                makers[0].authority.as_ref(),
                0u16.to_le_bytes().as_ref(),
            ],
            &velocity,
        )
        .0;
        assert_eq!(payload["bids"][0], bid_maker.to_string());
    }

    fn clob(quoter: Pubkey) -> CarriedEntry {
        CarriedEntry {
            quoter,
            program: Pubkey::new_unique(),
            user: Pubkey::new_unique(),
            quoter_type: program::state::prop_amm::QuoterType::Clob,
        }
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
    fn a_quoter_that_fills_from_one_account_attributes_its_ladder_to_it() {
        // The view says so: a quoter with no orders describes its ladder
        // against the one user its entry names, so the row carries a maker
        // and no order id.
        let quoter = Pubkey::new_unique();
        let velocity = Pubkey::new_unique();
        let user = user_ref(9, 1);
        let rows = view_rows(
            &velocity,
            &view(
                0,
                vec![book_with_rows(
                    QuotedSourceKind::Quoter,
                    quoter,
                    &[(100, 5, 0, user)],
                )],
            ),
            &[custom(quoter, Pubkey::new_unique())],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].maker, user_pda(&velocity, &user));
        assert_eq!(rows[0].quoter, Some(quoter));
        assert_eq!(rows[0].source, "propamm");
        assert!(
            rows[0].order_id.is_none(),
            "a quote is not an order and has no id to cancel"
        );
    }

    #[test]
    fn a_books_rows_are_its_orders_not_its_registrant() {
        // A book's entry names whoever registered it; its rows name the
        // makers resting on it, one per order, with the id each carries.
        let quoter = Pubkey::new_unique();
        let velocity = Pubkey::new_unique();
        let maker = user_ref(4, 0);
        let rows = view_rows(
            &velocity,
            &view(
                0,
                vec![book_with_rows(
                    QuotedSourceKind::Quoter,
                    quoter,
                    &[(100, 5, 42, maker)],
                )],
            ),
            &[clob(quoter)],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].maker, user_pda(&velocity, &maker));
        assert_eq!(rows[0].source, "clob");
        assert_eq!(rows[0].order_id, Some(42));
    }

    #[test]
    fn a_dlob_order_is_a_row_against_its_maker() {
        let velocity = Pubkey::new_unique();
        let maker = user_ref(5, 3);
        let rows = view_rows(
            &velocity,
            &view(
                0,
                vec![book_with_rows(
                    QuotedSourceKind::DlobOrder,
                    Pubkey::new_unique(),
                    &[(100, 5, 7, maker)],
                )],
            ),
            &[],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].maker, user_pda(&velocity, &maker));
        assert_eq!(rows[0].quoter, None);
        assert_eq!(rows[0].source, "dlob");
        assert_eq!(rows[0].order_id, Some(7));
    }

    #[test]
    fn the_vamm_is_attributed_to_the_market_it_settles_against() {
        // It has no maker's `User` because its counterparty is the market's
        // own AMM — which is an account, and the one this depth settles
        // against. A reader asking who is in this book gets the whole book.
        let market = Pubkey::new_unique();
        let rows = view_rows(
            &Pubkey::new_unique(),
            &view(
                0,
                vec![book(QuotedSourceKind::Vamm, market, &[(100, 5), (101, 7)])],
            ),
            &[],
        );
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.maker == market));
        assert!(rows.iter().all(|row| row.source == "vamm"));
        assert!(rows.iter().all(|row| row.order_id.is_none()));
        assert!(rows.iter().all(|row| row.quoter.is_none()));
    }

    /// A price level is filled in tier order, so it is listed in tier order:
    /// the vAMM, then the book, then customs. Showing them any other way
    /// would draw a queue that does not exist.
    #[test]
    fn rows_at_one_price_list_in_the_order_the_router_fills_them() {
        let row = |source: &'static str, priority: u8| BookRow {
            price: 100,
            size: 1,
            maker: Pubkey::new_unique(),
            quoter: None,
            order_id: None,
            priority,
            source,
        };
        let merged = merge_side(
            true,
            vec![row("propamm", 20), row("vamm", 0), row("clob", 10)],
        );
        assert_eq!(
            merged.iter().map(|row| row.source).collect::<Vec<_>>(),
            vec!["vamm", "clob", "propamm"]
        );

        // A better price still wins over a better tier.
        let mut better = row("propamm", 20);
        better.price = 99;
        let merged = merge_side(true, vec![row("vamm", 0), better]);
        assert_eq!(merged[0].price, 99);
    }

    #[test]
    fn merged_sides_stay_best_first() {
        let row = |price: u64, source: &'static str| BookRow {
            price,
            size: 1,
            maker: Pubkey::new_unique(),
            quoter: None,
            order_id: None,
            priority: 0,
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
            priority: 20,
            source: "propamm",
        }];
        let payload = best_makers_payload(50, propamm.clone(), propamm);
        assert_eq!(payload["bids"][0], json!(user.to_string()));
        assert_eq!(payload["asks"][0], json!(user.to_string()));
    }
}
