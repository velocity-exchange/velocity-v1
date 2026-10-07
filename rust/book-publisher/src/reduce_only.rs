//! The reduce-only book orders the quote view cannot show.
//!
//! The view quotes the CLOB with no user caps. A reduce-only order fills only
//! up to its owner's position, and the book cannot see a position, so the book
//! quotes such an order at zero. A fill loads the owner and its cover, so the
//! order is real depth. The publisher reads the owners' positions itself and
//! publishes each order at the size its owner's position covers.

use {
    crate::payload::BookRow,
    anyhow::Result,
    clob_state::{live_orders, OrderNodeV0, SideV0},
    program::state::user::User,
    relay_chain_source::{ChainSource, ClockSnapshot},
    solana_sdk::pubkey::Pubkey,
    std::collections::BTreeMap,
    velocity_router_sim::{pdas, quote_view::read_zero_copy},
};

/// The reduce-only orders a taker of either direction can reach.
#[derive(Default)]
pub struct ReduceOnlyDepth {
    pub bids: Vec<BookRow>,
    pub asks: Vec<BookRow>,
}

/// Size each matchable reduce-only order by what its owner's position still
/// covers. An owner's orders draw one cover down best price first, as a walk
/// reaches them.
pub async fn reduce_only_depth<S: ChainSource + ?Sized>(
    source: &S,
    velocity: &Pubkey,
    market_index: u16,
    book_quoter: Pubkey,
    book_priority: u8,
    book_data: &[u8],
    clock: &ClockSnapshot,
) -> Result<ReduceOnlyDepth> {
    let mut orders: Vec<(Pubkey, OrderNodeV0)> = live_orders(book_data)
        .map(|(_, node)| node)
        .filter(|node| node.is_reduce_only() && !node.is_taker_origin())
        .filter(|node| node.is_matchable(clock.slot, clock.unix_timestamp))
        .map(|node| (pdas::user_of(velocity, &node.user_ref()), node))
        .collect();
    if orders.is_empty() {
        return Ok(ReduceOnlyDepth::default());
    }

    let position_base = position_bases(source, &orders, market_index).await?;

    // Best price first on each side, then the book's own time priority.
    orders.sort_by(|(_, a), (_, b)| {
        let by_price = if is_ask(a) {
            a.price.cmp(&b.price)
        } else {
            b.price.cmp(&a.price)
        };
        is_ask(a)
            .cmp(&is_ask(b))
            .then(by_price)
            .then(a.order_id.cmp(&b.order_id))
    });

    let mut cover_left: BTreeMap<(Pubkey, bool), u64> = BTreeMap::new();
    let mut depth = ReduceOnlyDepth::default();
    for (owner, node) in orders {
        let cover = cover_left.entry((owner, is_ask(&node))).or_insert_with(|| {
            reduce_cover(
                position_base.get(&owner).copied().unwrap_or(0),
                is_ask(&node),
            )
        });
        let size = node.base_asset_amount.min(*cover);
        if size == 0 {
            continue;
        }

        *cover -= size;
        let row = BookRow {
            price: node.price,
            size,
            maker: owner,
            quoter: Some(book_quoter),
            order_id: (node.order_id != 0).then_some(node.order_id),
            priority: book_priority,
            source: "clob",
        };
        if is_ask(&node) {
            depth.asks.push(row);
        } else {
            depth.bids.push(row);
        }
    }

    Ok(depth)
}

/// Each owner's base in `market_index`. An owner whose account does not load is left out, so its
/// orders cover nothing.
async fn position_bases<S: ChainSource + ?Sized>(
    source: &S,
    orders: &[(Pubkey, OrderNodeV0)],
    market_index: u16,
) -> Result<BTreeMap<Pubkey, i64>> {
    let mut owners: Vec<Pubkey> = orders.iter().map(|(owner, _)| *owner).collect();
    owners.sort();
    owners.dedup();

    let accounts = source.get_multiple_accounts(&owners).await?;
    Ok(owners
        .iter()
        .zip(accounts)
        .filter_map(|(owner, account)| {
            let user: User = read_zero_copy(&account?.data).ok()?;
            let base = user
                .perp_positions
                .iter()
                .find(|position| position.market_index == market_index)
                .map_or(0, |position| position.base_asset_amount);
            Some((*owner, base))
        })
        .collect())
}

fn is_ask(node: &OrderNodeV0) -> bool {
    node.side() == SideV0::Ask
}

/// The base a reduce-only order may fill. An ask reduces a long and a bid
/// reduces a short.
fn reduce_cover(position_base: i64, is_ask: bool) -> u64 {
    if is_ask {
        position_base.max(0).unsigned_abs()
    } else {
        position_base.min(0).unsigned_abs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_order_reduces_only_the_position_it_faces() {
        assert_eq!(reduce_cover(5, true), 5);
        assert_eq!(reduce_cover(5, false), 0);
        assert_eq!(reduce_cover(-3, false), 3);
        assert_eq!(reduce_cover(-3, true), 0);
    }
}
