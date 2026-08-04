//! Discovery: which pair on a crossed book this crank resolves, which side of
//! it is the aggressor, and how it leaves the book. The node bytes here mirror
//! [`crate::state::prop_amm::read_clob_node`]'s offsets; the litesvm crank
//! tests pin those offsets against the real CLOB program.

use {
    super::*,
    crate::state::prop_amm::{
        read_clob_node, CLOB_NIL, CLOB_NODE_LEN, CLOB_ORDERS_OFFSET, CLOB_ORDER_BIT_FLAG_OPEN,
        CLOB_ORDER_BIT_FLAG_TAKER_ORIGIN,
    },
};

/// One live order in a synthetic book.
#[derive(Clone, Copy)]
struct Node {
    authority: u8,
    price: u64,
    base_asset_amount: u64,
    order_id: u64,
    placed_slot: u64,
    taker_origin: bool,
}

impl Node {
    fn maker(authority: u8, price: u64) -> Self {
        Self {
            authority,
            price,
            base_asset_amount: 10,
            order_id: 1,
            placed_slot: 0,
            taker_origin: false,
        }
    }

    /// A migrated taker remainder, identified by when it rested.
    fn remainder(authority: u8, price: u64, order_id: u64, placed_slot: u64) -> Self {
        Self {
            authority,
            price,
            base_asset_amount: 10,
            order_id,
            placed_slot,
            taker_origin: true,
        }
    }

    fn size(mut self, base_asset_amount: u64) -> Self {
        self.base_asset_amount = base_asset_amount;
        self
    }
}

/// A book holding one order on each side: the bid at node 0, the ask at node 1.
/// Everything the discovery reads is the head of each side plus that node, so a
/// deeper book adds nothing this can't say.
fn book_bytes(bid: Option<Node>, ask: Option<Node>) -> Vec<u8> {
    let mut data = vec![0u8; CLOB_ORDERS_OFFSET + 2 * CLOB_NODE_LEN];
    for (head_offset, index, node) in [
        (CLOB_BEST_BID_OFFSET, 0usize, bid),
        (CLOB_BEST_ASK_OFFSET, 1usize, ask),
    ] {
        let Some(node) = node else {
            data[head_offset..head_offset + 4].copy_from_slice(&CLOB_NIL.to_le_bytes());
            continue;
        };
        data[head_offset..head_offset + 4].copy_from_slice(&(index as u32).to_le_bytes());
        let at = CLOB_ORDERS_OFFSET + index * CLOB_NODE_LEN;
        data[at] = node.authority;
        data[at + 32..at + 40].copy_from_slice(&node.price.to_le_bytes());
        data[at + 40..at + 48].copy_from_slice(&node.base_asset_amount.to_le_bytes());
        data[at + 64..at + 72].copy_from_slice(&node.order_id.to_le_bytes());
        data[at + 72..at + 80].copy_from_slice(&node.placed_slot.to_le_bytes());
        data[at + 84..at + 88].copy_from_slice(&CLOB_NIL.to_le_bytes());
        data[at + 88] = CLOB_ORDER_BIT_FLAG_OPEN
            | if node.taker_origin {
                CLOB_ORDER_BIT_FLAG_TAKER_ORIGIN
            } else {
                0
            };
    }
    data
}

fn find(bid: Option<Node>, ask: Option<Node>) -> Option<TakerOriginCross> {
    find_taker_origin_cross(&book_bytes(bid, ask), 100, 1_000).unwrap()
}

#[test]
fn an_ordinary_crossed_book_is_not_this_cranks_work() {
    // Two makers crossing is unclaimed arbitrage, not somebody's improvement:
    // `crank_cross_match`'s case, and this crank declines it.
    assert!(find(Some(Node::maker(1, 101)), Some(Node::maker(2, 99))).is_none());
    // A remainder nothing crosses is ordinary depth.
    assert!(find(
        Some(Node::remainder(1, 99, 1, 0)),
        Some(Node::maker(2, 101))
    )
    .is_none());
    // One side empty is no cross at all.
    assert!(find(Some(Node::remainder(1, 101, 1, 0)), None).is_none());
}

#[test]
fn a_lone_remainder_is_the_aggressor_whichever_side_it_rests_on() {
    let bid_side = find(
        Some(Node::remainder(1, 101, 7, 5)),
        Some(Node::maker(2, 99)),
    )
    .unwrap();
    assert_eq!(bid_side.side, ClobSide::Bid);
    assert_eq!(bid_side.taker_origin.order_id, 7);
    assert_eq!(bid_side.counterparty.price, 99);
    assert!(matches!(bid_side.kind, TakerOriginCrossKind::Maker));

    let ask_side = find(
        Some(Node::maker(2, 101)),
        Some(Node::remainder(1, 99, 7, 5)),
    )
    .unwrap();
    assert_eq!(ask_side.side, ClobSide::Ask);
    assert_eq!(ask_side.taker_origin.order_id, 7);
    assert_eq!(ask_side.counterparty.price, 101);
    assert!(matches!(ask_side.kind, TakerOriginCrossKind::Maker));
}

/// Price-time priority: the remainder that rested first is the maker, keeps its
/// own price, and the later arrival is the aggressor that crosses into it. Which
/// side of the book each is on has nothing to do with it.
#[test]
fn between_two_remainders_the_earlier_one_is_the_maker() {
    // The bid rested at slot 5, the ask at slot 6: the ask aggresses into the
    // bid, and the match will settle at the bid's 101.
    let cross = find(
        Some(Node::remainder(1, 101, 2, 5)),
        Some(Node::remainder(2, 99, 3, 6)),
    )
    .unwrap();
    assert_eq!(cross.side, ClobSide::Ask);
    assert_eq!(cross.taker_origin.order_id, 3);
    assert_eq!(cross.counterparty.price, 101);
    assert!(matches!(
        cross.kind,
        TakerOriginCrossKind::Pair {
            counterparty_node: 0
        }
    ));

    // The other way round, same rule: the ask rested first, so the bid is the
    // aggressor and pays the ask's 99.
    let cross = find(
        Some(Node::remainder(1, 101, 3, 6)),
        Some(Node::remainder(2, 99, 2, 5)),
    )
    .unwrap();
    assert_eq!(cross.side, ClobSide::Bid);
    assert_eq!(cross.taker_origin.order_id, 3);
    assert_eq!(cross.counterparty.price, 99);
    assert!(matches!(
        cross.kind,
        TakerOriginCrossKind::Pair {
            counterparty_node: 1
        }
    ));
}

/// Two remainders that rested in the same slot are ordered by CLOB order id.
/// Sound rather than arbitrary: a book's `next_order_id` only increases, so the
/// lower id was placed first — and the tie is common, since a slot holds many
/// transactions.
#[test]
fn a_same_slot_tie_breaks_on_the_lower_order_id() {
    let cross = find(
        Some(Node::remainder(1, 101, 41, 5)),
        Some(Node::remainder(2, 99, 42, 5)),
    )
    .unwrap();
    assert_eq!(cross.side, ClobSide::Ask, "the higher id aggresses");
    assert_eq!(cross.counterparty.order_id, 41);
    assert_eq!(cross.counterparty.price, 101);

    let cross = find(
        Some(Node::remainder(1, 101, 42, 5)),
        Some(Node::remainder(2, 99, 41, 5)),
    )
    .unwrap();
    assert_eq!(cross.side, ClobSide::Bid);
    assert_eq!(cross.counterparty.order_id, 41);
    assert_eq!(cross.counterparty.price, 99);
}

/// The slot outranks the id: an order placed earlier rested earlier, however
/// the ids fell out.
#[test]
fn the_slot_decides_before_the_id_does() {
    assert!(rested_first(
        &node_view(Node::remainder(1, 101, 99, 5)),
        &node_view(Node::remainder(2, 99, 1, 6))
    ));
    assert!(!rested_first(
        &node_view(Node::remainder(1, 101, 1, 6)),
        &node_view(Node::remainder(2, 99, 99, 5))
    ));
    // And an order never rests before itself.
    let same = node_view(Node::remainder(1, 101, 1, 5));
    assert!(!rested_first(&same, &same));
}

fn node_view(node: Node) -> ClobNodeView {
    read_clob_node(&book_bytes(Some(node), None), 0).unwrap()
}

/// A user cannot be handed its own liquidity, whichever shape the cross takes.
#[test]
fn a_self_cross_is_refused() {
    assert!(find(
        Some(Node::remainder(1, 101, 2, 5)),
        Some(Node::maker(1, 99))
    )
    .is_none());
    assert!(find(
        Some(Node::remainder(1, 101, 2, 5)),
        Some(Node::remainder(1, 99, 3, 6))
    )
    .is_none());
}

/// The match is the smaller of the two orders, so the bigger one keeps a
/// leftover — and either side can be the bigger one once both are remainders.
#[test]
fn the_match_is_sized_to_the_smaller_order() {
    let cross = find(
        Some(Node::remainder(1, 101, 2, 5).size(7)),
        Some(Node::remainder(2, 99, 3, 6).size(4)),
    )
    .unwrap();
    assert_eq!(cross.size(), 4);
    let cross = find(
        Some(Node::remainder(1, 101, 2, 5).size(3)),
        Some(Node::remainder(2, 99, 3, 6).size(4)),
    )
    .unwrap();
    assert_eq!(cross.size(), 3);
}
