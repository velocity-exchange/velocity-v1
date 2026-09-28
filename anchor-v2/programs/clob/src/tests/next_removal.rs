//! `next_removal_v0`'s expiry preview must agree with the predicate
//! `remove_expired` enforces on chain, or a caller acts on work the book then
//! refuses.

use {
    super::market::{self, TestMarket},
    crate::{
        book::ClobBook,
        instructions::next_removal_v0::{expired, OrderViewV0},
        state::{PlaceOrderParams, SideV0},
    },
};

#[test]
fn expired_excludes_an_order_at_its_own_max_ts() {
    let test_market = TestMarket::new(8);
    let mut book = test_market.book();
    book.place(PlaceOrderParams {
        max_ts: 1_000,
        ..market::params(SideV0::Ask, 100, 5, market::user(1))
    })
    .unwrap();

    assert_eq!(expired(&book, 1_000).unwrap(), OrderViewV0::NONE);
    assert_ne!(expired(&book, 1_001).unwrap(), OrderViewV0::NONE);
}
