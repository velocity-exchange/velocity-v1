//! Per-user resting-order index.
//!
//! A CLOB order has no `User.orders` slot, so a client asking "what am I
//! resting" cannot get the answer from the account it already watches. This
//! builds that answer here, where the book's account is already resident, and
//! writes it to the same Redis the books go to.
//!
//! # Why the account and not a call
//!
//! The book answers about orders a caller already names (`orders_v0`) or about
//! the depth a taker of some size would reach (`quote_v0`). Neither is "every
//! order this user holds": the first needs the refs the question is asking
//! for, and the second stops at the size it was quoted at, so a user's order
//! deeper than that would simply be missing. The arena has all of them, and it
//! is public data this process already subscribes to. `clob-state` is the one
//! declaration of its layout.
//!
//! # Why almost every tick writes nothing
//!
//! Books are re-quoted continuously because prices move continuously. Resting
//! orders do not: a user's set changes when that user places, cancels or gets
//! filled, which is rare per user and rare per market next to a 200 ms tick.
//! So there are two gates, cheapest first. The market's whole arena is
//! fingerprinted, and an unchanged arena ends the tick before anything is
//! decoded. Past that, each user's own rows are fingerprinted, and only the
//! users whose rows moved are written and published.
//!
//! A user who *had* orders and now has none still gets one write — an empty
//! list — because a subscriber that heard nothing cannot tell an empty book
//! from a quiet one.

use {
    anyhow::{Context, Result},
    clob_state::{live_orders, OrderNodeV0, ORDERS_OFFSET},
    redis::AsyncCommands,
    serde_json::{json, Value},
    solana_sdk::pubkey::Pubkey,
    std::{
        collections::{
            hash_map::{DefaultHasher, Entry},
            HashMap, HashSet,
        },
        hash::{Hash, Hasher},
    },
};

/// What the last tick published, so this one can skip what has not moved.
#[derive(Default)]
pub struct UserOrdersIndex {
    /// Per market, the fingerprint of the whole node arena.
    arenas: HashMap<u16, u64>,
    /// Per user and market, the fingerprint of that user's published rows.
    /// A user drops out of here once their empty list has been published, so
    /// the emptying is written exactly once.
    users: HashMap<(Pubkey, u16), u64>,
}

/// One resting order, as a subscriber reads it.
///
/// `orderId` is velocity's, minted from the `User`'s own counter — the id a
/// client already names its orders by. `nodeIndex` and `clobOrderId` are the
/// book's handle for the same order, carried so a cancel or a modify needs no
/// lookup: they are exactly the `ClobOrderRefV0` those instructions take.
fn order_json_at(node: &OrderNodeV0, node_index: u32, market_index: u16) -> Value {
    json!({
        "orderId": node.client_order_id,
        "nodeIndex": node_index,
        // On the row and not only on the document: a reader after every market
        // a user rests in gets one flat list, and a row that could not name its
        // own market would be unusable in it.
        "marketIndex": market_index,
        "clobOrderId": node.order_id.to_string(),
        "direction": if node.side() == clob_state::Side::Bid { "long" } else { "short" },
        "price": node.price.to_string(),
        "baseAssetAmount": node.base_asset_amount.to_string(),
        "maxTs": node.max_ts.to_string(),
        "activationSlot": node.activation_slot.to_string(),
        "placedSlot": node.placed_slot.to_string(),
        "takerOrigin": node.is_taker_origin(),
        "venue": "clob",
    })
}

fn fingerprint<T: Hash>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Which users' rows moved since the last tick, and what they are now.
///
/// `None` when the market's arena is byte-identical to the last one seen,
/// which is the common case and ends the tick before anything is decoded.
/// `Some` carries only the users to write — a user whose rows are unchanged is
/// not in it, and a user who went from resting to empty is, with an empty
/// list.
///
/// Separated from the writing so the gates can be tested without a Redis.
pub fn changed_users(
    index: &mut UserOrdersIndex,
    velocity: &Pubkey,
    market_index: u16,
    book_account_data: &[u8],
) -> Option<Vec<(Pubkey, Value)>> {
    let arena = book_account_data.get(ORDERS_OFFSET..).unwrap_or_default();
    let arena_print = fingerprint(&arena);
    if index.arenas.get(&market_index) == Some(&arena_print) {
        return None;
    }
    index.arenas.insert(market_index, arena_print);

    // Group by owner. Node order inside a user is arena order, which is
    // stable for an unchanged book — the fingerprint below depends on it, and
    // a set that reshuffled without changing would publish for nothing.
    let mut by_user: HashMap<Pubkey, Vec<Value>> = HashMap::new();
    for (node_index, node) in live_orders(book_account_data) {
        let user = user_pda(velocity, &node.user_ref());
        by_user
            .entry(user)
            .or_default()
            .push(order_json_at(&node, node_index, market_index));
    }

    let mut changed = Vec::new();
    let mut still_resting: HashSet<Pubkey> = HashSet::with_capacity(by_user.len());
    for (user, orders) in by_user {
        still_resting.insert(user);
        let rows = Value::Array(orders);
        let print = fingerprint(&rows.to_string());
        match index.users.entry((user, market_index)) {
            Entry::Occupied(slot_entry) if *slot_entry.get() == print => continue,
            Entry::Occupied(mut slot_entry) => {
                slot_entry.insert(print);
            }
            Entry::Vacant(slot_entry) => {
                slot_entry.insert(print);
            }
        }
        changed.push((user, rows));
    }

    // Whoever this market held last tick and holds no longer. They are
    // dropped from the index in the same pass, so the emptying is published
    // once rather than on every tick after it.
    let emptied: Vec<Pubkey> = index
        .users
        .keys()
        .filter(|(user, market)| *market == market_index && !still_resting.contains(user))
        .map(|(user, _)| *user)
        .collect();
    for user in emptied {
        index.users.remove(&(user, market_index));
        changed.push((user, Value::Array(Vec::new())));
    }
    Some(changed)
}

/// Index one market's book and write what changed.
///
/// Returns how many users were written, which is zero on almost every tick.
#[allow(clippy::too_many_arguments)]
pub async fn publish(
    index: &mut UserOrdersIndex,
    redis: &mut redis::aio::MultiplexedConnection,
    prefix: &str,
    velocity: &Pubkey,
    market_index: u16,
    slot: u64,
    ts_ms: u128,
    book_account_data: &[u8],
) -> Result<usize> {
    let Some(changed) = changed_users(index, velocity, market_index, book_account_data) else {
        return Ok(0);
    };
    for (user, rows) in &changed {
        write_user(redis, prefix, user, market_index, slot, ts_ms, rows.clone()).await?;
    }
    Ok(changed.len())
}

/// One key per user per market.
///
/// A hash keyed by market would fit the shape better, but the serving side's
/// Redis wrapper exposes no hash commands and does expose `mget` — and the
/// market set is small and known to both ends, so a caller after every market
/// asks for the keys it wants in one round trip either way.
fn user_key(prefix: &str, user: &Pubkey, market_index: u16) -> String {
    format!("{prefix}last_update_user_orders_{user}_{market_index}")
}

/// One user's rows for one market: stored under that market's key, and
/// published on that user's channel for anyone already holding the rest.
///
/// An empty list is published but not stored. A subscriber has to hear that
/// the last order left, and a reader starting fresh should find no key at all
/// rather than an empty document that looks like stale state.
async fn write_user(
    redis: &mut redis::aio::MultiplexedConnection,
    prefix: &str,
    user: &Pubkey,
    market_index: u16,
    slot: u64,
    ts_ms: u128,
    orders: Value,
) -> Result<()> {
    let document = json!({
        "user": user.to_string(),
        "marketIndex": market_index,
        "marketType": "perp",
        "slot": slot,
        "ts": ts_ms as u64,
        "orders": orders,
    });
    let body = document.to_string();
    if document["orders"]
        .as_array()
        .is_some_and(|orders| orders.is_empty())
    {
        redis
            .del::<_, ()>(user_key(prefix, user, market_index))
            .await
            .context("clear user orders")?;
    } else {
        redis
            .set::<_, _, ()>(user_key(prefix, user, market_index), &body)
            .await
            .context("store user orders")?;
    }
    redis
        .publish::<_, _, ()>(format!("{prefix}user_orders_{user}"), body)
        .await
        .context("publish user orders")?;
    Ok(())
}

/// The velocity `User` PDA a node's identity derives to.
fn user_pda(velocity: &Pubkey, user: &clob_state::UserRefV0) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"user",
            user.authority.as_array(),
            &user.sub_account_id.to_le_bytes(),
        ],
        velocity,
    )
    .0
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        clob_state::{node_bytes, OrderBitFlag, NIL},
    };

    fn market(nodes: &[OrderNodeV0]) -> Vec<u8> {
        let mut data = vec![0u8; ORDERS_OFFSET];
        for node in nodes {
            data.extend_from_slice(node_bytes(node));
        }
        data
    }

    fn node(authority: u8, client_order_id: u32, size: u64) -> OrderNodeV0 {
        OrderNodeV0 {
            authority: solana_sdk::pubkey::Pubkey::new_from_array([authority; 32])
                .to_bytes()
                .into(),
            price: 100,
            base_asset_amount: size,
            activation_slot: 0,
            max_ts: 0,
            order_id: client_order_id as u64,
            placed_slot: 0,
            prev: NIL,
            next: NIL,
            bit_flags: OrderBitFlag::Open as u8,
            padding0: 0,
            sub_account_id: 0,
            client_order_id,
        }
    }

    fn velocity() -> Pubkey {
        Pubkey::new_from_array([9u8; 32])
    }

    /// The gate that matters: books are re-quoted every tick because prices
    /// move every tick, but nobody's resting orders moved, so nothing is
    /// written.
    #[test]
    fn an_unchanged_book_publishes_nothing() {
        let mut index = UserOrdersIndex::default();
        let data = market(&[node(1, 10, 5), node(2, 20, 7)]);
        assert_eq!(
            changed_users(&mut index, &velocity(), 0, &data)
                .expect("first sight is a change")
                .len(),
            2
        );
        assert!(changed_users(&mut index, &velocity(), 0, &data).is_none());
        assert!(changed_users(&mut index, &velocity(), 0, &data).is_none());
    }

    /// One user placing must not republish everyone else's unchanged rows.
    #[test]
    fn only_the_user_whose_orders_moved_is_written() {
        let mut index = UserOrdersIndex::default();
        let before = market(&[node(1, 10, 5), node(2, 20, 7)]);
        changed_users(&mut index, &velocity(), 0, &before).unwrap();

        // The second maker's order is partly filled; the first is untouched.
        let after = market(&[node(1, 10, 5), node(2, 20, 3)]);
        let changed = changed_users(&mut index, &velocity(), 0, &after).unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(
            changed[0].0,
            user_pda(&velocity(), &node(2, 20, 3).user_ref())
        );
    }

    /// A subscriber that heard nothing cannot tell an empty book from a quiet
    /// one, so the last order leaving is a write — and exactly one.
    #[test]
    fn emptying_publishes_once_and_then_goes_quiet() {
        let mut index = UserOrdersIndex::default();
        changed_users(&mut index, &velocity(), 0, &market(&[node(1, 10, 5)])).unwrap();

        let empty = market(&[]);
        let changed = changed_users(&mut index, &velocity(), 0, &empty).unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].1, Value::Array(Vec::new()));
        assert!(changed_users(&mut index, &velocity(), 0, &empty).is_none());
    }

    /// Markets are indexed independently: one book moving must not reprint a
    /// user's rows on another.
    #[test]
    fn each_market_is_gated_on_its_own_arena() {
        let mut index = UserOrdersIndex::default();
        let data = market(&[node(1, 10, 5)]);
        changed_users(&mut index, &velocity(), 0, &data).unwrap();
        assert!(changed_users(&mut index, &velocity(), 0, &data).is_none());
        assert_eq!(
            changed_users(&mut index, &velocity(), 1, &data)
                .expect("a different market is unseen")
                .len(),
            1
        );
    }

    /// The wire, pinned against the file the TypeScript side reads.
    ///
    /// Both ends assert against one fixture rather than against a literal each
    /// wrote itself. Two hand-written fixtures are blind to each other: a
    /// field this stopped emitting stays in the consumer's copy, both suites
    /// pass, and the feed is broken. Changing the payload means changing the
    /// fixture, which fails whichever side was not changed with it.
    #[test]
    fn a_row_is_exactly_what_the_sdk_fixture_declares() {
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packages/sdk/tests/fixtures/userClobOrderRow.json"
        )))
        .expect("fixture parses");

        let node = OrderNodeV0 {
            price: 99_000_000,
            base_asset_amount: 500_000_000,
            activation_slot: 1_234,
            max_ts: 0,
            order_id: u64::MAX,
            placed_slot: 1_233,
            bit_flags: OrderBitFlag::Open as u8 | OrderBitFlag::Ask as u8,
            client_order_id: 41,
            ..node(1, 41, 500_000_000)
        };
        assert_eq!(order_json_at(&node, 7, 3), fixture);
    }

    /// The rows carry the book's handle for each order, which is what a cancel
    /// or a modify takes — so holding the feed is enough to act on an order.
    #[test]
    fn a_row_carries_the_handle_a_cancel_needs() {
        let mut index = UserOrdersIndex::default();
        let data = market(&[node(1, 10, 5), node(1, 11, 6)]);
        let changed = changed_users(&mut index, &velocity(), 0, &data).unwrap();
        let rows = changed[0].1.as_array().expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["orderId"], 10);
        assert_eq!(rows[0]["nodeIndex"], 0);
        assert_eq!(rows[1]["nodeIndex"], 1);
        assert_eq!(rows[0]["venue"], "clob");
        // A row names its own market, because a reader after every market gets
        // one flat list and cannot recover it from the document it came in.
        assert_eq!(rows[0]["marketIndex"], 0);
        let mut other = UserOrdersIndex::default();
        let rows = changed_users(&mut other, &velocity(), 7, &data).unwrap()[0]
            .1
            .clone();
        assert_eq!(rows.as_array().unwrap()[0]["marketIndex"], 7);
    }
}
