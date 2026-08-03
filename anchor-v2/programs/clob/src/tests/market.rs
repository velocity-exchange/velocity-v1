//! Test harness: a real `ClobMarketV0` over a stack-backed account buffer,
//! plus the exhaustive book checker the on-chain `validate_book` is the O(1)
//! subset of.

use {
    crate::{
        book::{ClobBook, NodeArena, NIL},
        error::ClobError,
        state::{
            ClobHeaderV0, ClobMarketV0, MarketConfigV0, OrderBitFlag, OrderRefV0, PlaceOrderParams,
            Side, UserRefV0,
        },
    },
    anchor_lang_v2::{
        prelude::*,
        testing::{AccountBuffer, MIN_ACCOUNT_BUF},
        AnchorAccount,
    },
};

/// Largest arena the harness backs. Sized once so the buffer length is a
/// constant; smaller markets just set a shorter `data_len`. Big enough to hold
/// `EXECUTE_FILLS_CEILING` orders on one side (a side is half the arena), so
/// the response tests can drive the widest execute response a market can
/// produce.
pub const MAX_CAPACITY: u32 = 2 * crate::state::EXECUTE_FILLS_CEILING as u32;

const BUFFER_BYTES: usize = MIN_ACCOUNT_BUF + ClobMarketV0::space_for(MAX_CAPACITY);

pub fn test_config() -> MarketConfigV0 {
    MarketConfigV0 {
        market_index: 0,
        base_precision: 1,
        order_tick_size: 1,
        order_step_size: 1,
        min_order_size: 1,
        default_activation_delay_slots: 0,
        max_activation_delay_slots: 20,
        unknown_user_grace_slots: 0,
        evict_threshold_per_side: 1,
        max_quote_levels: 8,
        max_execute_fills: 8,
        max_execute_users: 4,
    }
}

/// Owns the account bytes; hand out a loaded market with [`Self::book`].
pub struct TestMarket {
    buffer: Box<AccountBuffer<BUFFER_BYTES>>,
}

impl TestMarket {
    pub fn new(capacity: u32) -> Self {
        Self::new_with(capacity, test_config())
    }

    pub fn new_with(capacity: u32, config: MarketConfigV0) -> Self {
        assert!(capacity <= MAX_CAPACITY, "raise MAX_CAPACITY");
        let market = Self::uninitialized(capacity);
        market
            .book()
            .initialize(
                Address::new_from_array([1u8; 32]),
                Address::new_from_array([2u8; 32]),
                config,
            )
            .unwrap();
        assert_consistent(&market.book());
        market
    }

    /// A zeroed, correctly-owned market account with the discriminator
    /// stamped but `initialize` not yet run.
    pub fn uninitialized(capacity: u32) -> Self {
        let buffer = Box::new(AccountBuffer::<BUFFER_BYTES>::new());
        buffer.init(
            [7u8; 32],
            crate::ID.to_bytes(),
            ClobMarketV0::space_for(capacity),
            false,
            true,
            false,
        );
        buffer.write_data(ClobHeaderV0::DISCRIMINATOR);
        Self { buffer }
    }

    /// Load the market for mutation. The returned value releases the
    /// account's borrow marker when dropped, so hold at most one at a time.
    pub fn book(&self) -> ClobMarketV0 {
        // SAFETY: the buffer is boxed and never moves, and callers hold one
        // book at a time (the Slab's borrow marker enforces it otherwise).
        unsafe { ClobMarketV0::load_mut(self.buffer.view()) }.expect("market loads")
    }
}

pub fn user(seed: u8) -> UserRefV0 {
    UserRefV0 {
        authority: Address::new_from_array([seed; 32]),
        sub_account_id: 0,
    }
}

/// Place an order and assert the book is still fully consistent.
pub fn place(
    book: &mut ClobMarketV0,
    side: Side,
    price: u64,
    size: u64,
    user: UserRefV0,
) -> OrderRefV0 {
    let order_ref = place_raw(book, side, price, size, user).expect("placement succeeds");
    assert_consistent(book);
    order_ref
}

pub fn place_raw(
    book: &mut ClobMarketV0,
    side: Side,
    price: u64,
    size: u64,
    user: UserRefV0,
) -> Result<OrderRefV0> {
    book.place(PlaceOrderParams {
        side,
        price,
        base_asset_amount: size,
        user,
        activation_slot: 0,
        placed_slot: 0,
        max_ts: 0,
    })
}

/// Assert a book operation failed with a specific [`ClobError`]. Anchor maps
/// them to `Custom(6000 + variant index)`.
#[track_caller]
pub fn assert_err<T>(result: Result<T>, expected: ClobError) {
    let expected_code = expected as u32 + 6000;
    match result.err().expect("expected failure") {
        ProgramError::Custom(code) => assert_eq!(code, expected_code, "wrong error code"),
        other => panic!("expected Custom({expected_code}), got {other:?}"),
    }
}

/// Exhaustive book check: walk both sides and the free list so every arena
/// slot is accounted for exactly once, links are mutual, prices are ordered,
/// and the header counts match reality.
#[track_caller]
pub fn assert_consistent(book: &ClobMarketV0) {
    let capacity = book.capacity();
    let mut seen = vec![false; capacity];

    for side in [Side::Bid, Side::Ask] {
        let mut cursor = book.best(side);
        let mut prev = NIL;
        let mut count = 0usize;
        let mut last_price: Option<u64> = None;
        while cursor != NIL {
            assert!((cursor as usize) < capacity, "link {cursor} out of arena");
            assert!(!seen[cursor as usize], "node {cursor} is on two lists");
            seen[cursor as usize] = true;
            let node = book.read_node(cursor).unwrap();
            assert!(
                node.is_bit_flag_set(OrderBitFlag::Open),
                "freed node {cursor} is linked into {side:?}"
            );
            assert_eq!(node.side(), side, "node {cursor} is on the wrong side");
            assert_eq!(node.prev, prev, "node {cursor} back-link");
            if let Some(previous) = last_price {
                assert!(
                    !side.is_worse_price(previous, node.price),
                    "{side:?} price order broken at node {cursor}"
                );
            }
            last_price = Some(node.price);
            prev = cursor;
            cursor = node.next;
            count += 1;
            assert!(count <= capacity, "{side:?} list cycles");
        }
        assert_eq!(count as u32, book.node_count(side), "{side:?} count");
        assert_eq!(prev, book.worst(side), "{side:?} tail pointer");
    }

    let mut cursor = book.free_head;
    let mut free = 0usize;
    while cursor != NIL {
        assert!(
            (cursor as usize) < capacity,
            "free link {cursor} out of arena"
        );
        assert!(!seen[cursor as usize], "node {cursor} is free and live");
        seen[cursor as usize] = true;
        let node = book.read_node(cursor).unwrap();
        assert!(
            !node.is_bit_flag_set(OrderBitFlag::Open),
            "live node {cursor} is on the free list"
        );
        cursor = node.next;
        free += 1;
        assert!(free <= capacity, "free list cycles");
    }
    assert_eq!(free as u32, book.free_count, "free count");
    assert!(
        seen.into_iter().all(|slot| slot),
        "an arena slot is on no list"
    );

    book.validate_book().expect("O(1) invariants hold");
}
