//! Discovery: which pair on a crossed book this crank resolves, which side of
//! it is the aggressor, and how it leaves the book. The input is what the book
//! reports through `next_cross_v0`, so these cases say nothing about the
//! book's memory; the litesvm crank tests pin the reporting against the real
//! CLOB program.

use {super::*, crate::state::prop_amm::ClobOrderRefV0};

/// One matchable order at the top of a side.
#[derive(Clone, Copy)]
struct Head {
    authority: u8,
    price: u64,
    base_asset_amount: u64,
    order_id: u64,
    taker_origin: bool,
}

impl Head {
    fn maker(authority: u8, price: u64) -> Self {
        Self {
            authority,
            price,
            base_asset_amount: 10,
            order_id: 1,
            taker_origin: true,
        }
        .as_maker()
    }

    fn as_maker(mut self) -> Self {
        self.taker_origin = false;
        self
    }

    /// A migrated taker remainder. Ids are handed out from a counter that only
    /// increases, so the id is also when it rested relative to the other side.
    fn remainder(authority: u8, price: u64, order_id: u64) -> Self {
        Self {
            authority,
            price,
            base_asset_amount: 10,
            order_id,
            taker_origin: true,
        }
    }

    fn size(mut self, base_asset_amount: u64) -> Self {
        self.base_asset_amount = base_asset_amount;
        self
    }
}

fn head(node: Option<Head>, node_index: u32) -> ClobOrderViewV0 {
    let Some(node) = node else {
        return ClobOrderViewV0::NONE;
    };
    ClobOrderViewV0 {
        order_ref: ClobOrderRefV0 {
            node_index,
            order_id: node.order_id,
        },
        client_order_id: 0,
        user: ClobUserRefV0 {
            authority: Pubkey::new_from_array([node.authority; 32]),
            sub_account_id: 0,
        },
        side: if node_index == 0 {
            ClobSide::Bid
        } else {
            ClobSide::Ask
        },
        price: node.price,
        base_asset_amount: node.base_asset_amount,
        placed_slot: 0,
        max_ts: 0,
        taker_origin: node.taker_origin,
    }
}

fn find(bid: Option<Head>, ask: Option<Head>) -> Option<TakerOriginCross> {
    find_taker_origin_cross(&ClobNextCrossV0 {
        bid: head(bid, 0),
        ask: head(ask, 1),
    })
}

#[test]
fn an_ordinary_crossed_book_is_not_this_cranks_work() {
    // Two makers crossing is unclaimed arbitrage, not somebody's improvement:
    // `crank_cross_match`'s case, and this crank declines it.
    assert!(find(Some(Head::maker(1, 101)), Some(Head::maker(2, 99))).is_none());
    // A remainder nothing crosses is ordinary depth.
    assert!(find(Some(Head::remainder(1, 99, 1)), Some(Head::maker(2, 101))).is_none());
    // One side empty is no cross at all.
    assert!(find(Some(Head::remainder(1, 101, 1)), None).is_none());
}

#[test]
fn a_lone_remainder_is_the_aggressor_whichever_side_it_rests_on() {
    let bid_side = find(Some(Head::remainder(1, 101, 7)), Some(Head::maker(2, 99))).unwrap();
    assert_eq!(bid_side.side, ClobSide::Bid);
    assert_eq!(bid_side.taker_origin.order_ref.order_id, 7);
    assert_eq!(bid_side.counterparty.price, 99);
    assert!(matches!(bid_side.kind, TakerOriginCrossKind::Maker));

    let ask_side = find(Some(Head::maker(2, 101)), Some(Head::remainder(1, 99, 7))).unwrap();
    assert_eq!(ask_side.side, ClobSide::Ask);
    assert_eq!(ask_side.taker_origin.order_ref.order_id, 7);
    assert_eq!(ask_side.counterparty.price, 101);
    assert!(matches!(ask_side.kind, TakerOriginCrossKind::Maker));
}

/// Price-time priority: the remainder that rested first is the maker, keeps its
/// own price, and the later arrival is the aggressor that crosses into it.
///
/// The id is the rest order. A book's `next_order_id` only increases and it
/// never reuses one, so the lower id was placed first — no slot has to travel
/// with the order for the comparison to hold. Which side of the book each rests
/// on has nothing to do with it.
#[test]
fn between_two_remainders_the_earlier_one_is_the_maker() {
    // The bid took id 2, the ask id 3: the ask aggresses into the bid, and the
    // match will settle at the bid's 101.
    let cross = find(
        Some(Head::remainder(1, 101, 2)),
        Some(Head::remainder(2, 99, 3)),
    )
    .unwrap();
    assert_eq!(cross.side, ClobSide::Ask);
    assert_eq!(cross.taker_origin.order_ref.order_id, 3);
    assert_eq!(cross.counterparty.price, 101);
    assert_eq!(cross.counterparty.order_ref.node_index, 0);
    assert!(matches!(cross.kind, TakerOriginCrossKind::Pair));

    // The other way round, same rule: the ask rested first, so the bid is the
    // aggressor and pays the ask's 99.
    let cross = find(
        Some(Head::remainder(1, 101, 3)),
        Some(Head::remainder(2, 99, 2)),
    )
    .unwrap();
    assert_eq!(cross.side, ClobSide::Bid);
    assert_eq!(cross.taker_origin.order_ref.order_id, 3);
    assert_eq!(cross.counterparty.price, 99);
    assert_eq!(cross.counterparty.order_ref.node_index, 1);
    assert!(matches!(cross.kind, TakerOriginCrossKind::Pair));
}

/// A user cannot be handed its own liquidity, whichever shape the cross takes.
#[test]
fn a_self_cross_is_refused() {
    assert!(find(Some(Head::remainder(1, 101, 2)), Some(Head::maker(1, 99))).is_none());
    assert!(find(
        Some(Head::remainder(1, 101, 2)),
        Some(Head::remainder(1, 99, 3))
    )
    .is_none());
}

/// The match is the smaller of the two orders, so the bigger one keeps a
/// leftover — and either side can be the bigger one once both are remainders.
#[test]
fn the_match_is_sized_to_the_smaller_order() {
    let cross = find(
        Some(Head::remainder(1, 101, 2).size(7)),
        Some(Head::remainder(2, 99, 3).size(4)),
    )
    .unwrap();
    assert_eq!(cross.size(), 4);
    let cross = find(
        Some(Head::remainder(1, 101, 2).size(3)),
        Some(Head::remainder(2, 99, 3).size(4)),
    )
    .unwrap();
    assert_eq!(cross.size(), 3);
}
