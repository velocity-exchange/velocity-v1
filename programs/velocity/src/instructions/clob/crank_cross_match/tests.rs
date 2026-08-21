//! Which crossing prefix the cross resolver offers `crank_cross_match`.
//!
//! The book is built from `clob-spec`'s own node, so these fixtures cannot
//! drift from the layout the book writes; the litesvm crank tests pin the same
//! declaration against the real CLOB program.

use {
    super::*,
    crate::state::prop_amm::{
        ClobNodeView, ClobOrderBitFlag, ClobUserRefV0, CLOB_BEST_ASK_OFFSET, CLOB_BEST_BID_OFFSET,
        CLOB_NIL, CLOB_NODE_LEN, CLOB_ORDERS_OFFSET,
    },
    bytemuck::Zeroable,
};

const UNIT: u64 = crate::math::constants::BASE_PRECISION_U64;
const PRICE: u64 = crate::math::constants::PRICE_PRECISION_U64;

/// One live order, best-first within its side.
#[derive(Clone, Copy)]
struct Node {
    authority: u8,
    price: u64,
    base_asset_amount: u64,
    taker_origin: bool,
}

fn maker(authority: u8, price: u64, base_asset_amount: u64) -> Node {
    Node {
        authority,
        price,
        base_asset_amount,
        taker_origin: false,
    }
}

fn remainder(authority: u8, price: u64, base_asset_amount: u64) -> Node {
    Node {
        taker_origin: true,
        ..maker(authority, price, base_asset_amount)
    }
}

fn user(authority: u8) -> ClobUserRefV0 {
    ClobUserRefV0 {
        authority: Pubkey::new_from_array([authority; 32]),
        sub_account_id: 0,
    }
}

/// A book from two best-first side ladders: the bids occupy the first nodes,
/// the asks the rest, each side linked in the order given.
fn book_bytes(bids: &[Node], asks: &[Node]) -> Vec<u8> {
    let mut data = vec![0u8; CLOB_ORDERS_OFFSET + (bids.len() + asks.len()) * CLOB_NODE_LEN];
    let mut index = 0u32;
    for (head_offset, side) in [(CLOB_BEST_BID_OFFSET, bids), (CLOB_BEST_ASK_OFFSET, asks)] {
        let head = if side.is_empty() { CLOB_NIL } else { index };
        data[head_offset..head_offset + 4].copy_from_slice(&head.to_le_bytes());
        for (position, node) in side.iter().enumerate() {
            let at = CLOB_ORDERS_OFFSET + index as usize * CLOB_NODE_LEN;
            let next = if position + 1 == side.len() {
                CLOB_NIL
            } else {
                index + 1
            };
            let mut slot = ClobNodeView::zeroed();
            slot.authority = Pubkey::new_from_array([node.authority; 32]);
            slot.price = node.price;
            slot.base_asset_amount = node.base_asset_amount;
            slot.next = next;
            slot.prev = CLOB_NIL;
            slot.bit_flags = ClobOrderBitFlag::Open as u8
                | ClobOrderBitFlag::TakerOrigin.bit_if(node.taker_origin);
            data[at..at + CLOB_NODE_LEN].copy_from_slice(bytemuck::bytes_of(&slot));
            index += 1;
        }
    }
    data
}

fn find(bids: &[Node], asks: &[Node]) -> ClobCross {
    find_clob_cross(&book_bytes(bids, asks), 100, 1_000).unwrap()
}

#[test]
fn the_prefix_is_the_crossed_depth_and_the_makers_it_touches() {
    let cross = find(
        &[maker(1, 101 * PRICE, UNIT), maker(2, 98 * PRICE, UNIT)],
        &[maker(3, 99 * PRICE, UNIT / 2)],
    );
    // Only the 101 bid crosses the 99 ask, and only for the ask's half unit.
    assert_eq!(cross.size, UNIT / 2);
    assert_eq!(cross.buy_quote, 49_500_000);
    assert_eq!(cross.sell_quote, 50_500_000);
    assert_eq!(cross.makers, vec![user(1), user(3)]);
    // Nothing crossed at all is no work.
    assert_eq!(find(&[maker(1, 99 * PRICE, UNIT)], &[]).size, 0);
    assert_eq!(
        find(
            &[maker(1, 99 * PRICE, UNIT)],
            &[maker(2, 101 * PRICE, UNIT)]
        )
        .size,
        0
    );
}

/// A crossed taker remainder is not depth this crank may cross: the book
/// withholds it from `execute_v0` while a counterparty crosses it, and the one
/// case where it does not — a first leg that consumed the whole opposite side —
/// hands it over at its own resting price. What sits behind it is still an
/// ordinary cross, and stays in.
#[test]
fn a_crossed_taker_remainder_is_not_offered_to_the_arb_crank() {
    // The remainder is the best bid: the maker×maker cross behind it is what
    // remains, sized to the 98 bid rather than to the 101 remainder.
    let cross = find(
        &[
            remainder(1, 101 * PRICE, UNIT),
            maker(2, 100 * PRICE, UNIT / 2),
        ],
        &[maker(3, 99 * PRICE, UNIT)],
    );
    assert_eq!(cross.size, UNIT / 2);
    assert_eq!(cross.makers, vec![user(2), user(3)]);

    // Behind the best on its own side, with the maker in front too small to
    // absorb the whole crossing ask: the prefix stops at the remainder instead
    // of counting its base.
    let cross = find(
        &[
            maker(2, 102 * PRICE, UNIT / 2),
            remainder(1, 101 * PRICE, UNIT),
        ],
        &[maker(3, 99 * PRICE, UNIT)],
    );
    assert_eq!(cross.size, UNIT / 2);
    assert_eq!(cross.makers, vec![user(2), user(3)]);

    // A cross made only of remainders is entirely this crank's non-business.
    assert_eq!(
        find(
            &[remainder(1, 101 * PRICE, UNIT)],
            &[remainder(2, 99 * PRICE, UNIT)]
        )
        .size,
        0
    );
    assert_eq!(
        find(
            &[remainder(1, 101 * PRICE, UNIT)],
            &[maker(2, 99 * PRICE, UNIT)]
        )
        .size,
        0
    );
}
