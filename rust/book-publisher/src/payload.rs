//! Synthesize the dlob-server L2 wire payload from a pair of quote views.
//!
//! The TypeScript serving side keeps its Redis contract: one JSON document
//! per market under `last_update_orderbook_perp_{index}` (and the matching
//! pub/sub channel), levels as decimal strings in on-chain precision, a
//! numeric top-level `slot` (the freshness key `selectMostRecentBySlot`
//! picks by), and a per-level `sources` breakdown. The four-source book
//! rides the existing `sources` field with two new keys — `clob` and
//! `propamm` — so consumers that only know `vamm`/`dlob` keep working.

use {
    program::state::router_quote::QuotedSourceKind,
    serde_json::{json, Map, Value},
    solana_sdk::pubkey::Pubkey,
    std::collections::BTreeMap,
    velocity_router_sim::quote_view::QuoteView,
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

/// Build the L2 document. `asks` is the long-direction view (a buyer
/// consumes asks), `bids` the short-direction view.
pub fn l2_payload(
    market_index: u16,
    market_name: &str,
    clob_quoter: &Pubkey,
    asks: &QuoteView,
    bids: &QuoteView,
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
        "markPrice": opt_string(mark),
        "spreadQuote": opt_string(spread_quote),
        "spreadPct": opt_string(spread_pct),
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

        let payload = l2_payload(7, "SOL-PERP", &clob_quoter, &asks, &bids, 1_234);
        assert_eq!(payload["marketIndex"], 7);
        assert_eq!(payload["slot"], 100);
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
        let payload = l2_payload(0, "X", &clob_quoter, &asks, &bids, 0);
        assert_eq!(payload["asks"][0]["sources"]["propamm"], "1");
        assert!(payload["bestBidPrice"].is_null());
    }
}
