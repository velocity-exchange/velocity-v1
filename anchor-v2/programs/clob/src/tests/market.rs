//! Test harness: a real `ClobMarketV0` over a stack-backed account buffer,
//! plus the exhaustive book checker the on-chain `validate_book` is the O(1)
//! subset of.

use {
    crate::{
        book::{BookHeader, ClobBook, NodeArena},
        error::ClobError,
        state::{
            ClobHeaderV0, ClobMarketV0, ClobOrderRefV0, DirectionV0, MarketConfigV0, OrderBitFlag,
            PlaceOrderParams, SideV0, UserCapsV0, UserRefV0, NIL,
        },
    },
    anchor_lang::{
        prelude::*,
        testing::{AccountBuffer, MIN_ACCOUNT_BUF},
        AnchorAccount,
    },
    quoter_spec::{ExecuteArgsV0, QuoteArgsV0},
};

/// Arena capacity: a side is half, sized for the widest fills a market can produce.
const MAX_PER_SIDE: u32 =
    if crate::state::EXECUTE_FILLS_CEILING > crate::state::CANCEL_ALL_ORDERS_CEILING {
        crate::state::EXECUTE_FILLS_CEILING as u32
    } else {
        crate::state::CANCEL_ALL_ORDERS_CEILING as u32 + 4
    };

pub const MAX_CAPACITY: u32 = 2 * MAX_PER_SIDE;

const BUFFER_BYTES: usize = MIN_ACCOUNT_BUF + ClobMarketV0::space_for(MAX_CAPACITY);

pub fn test_config() -> MarketConfigV0 {
    MarketConfigV0 {
        market_index: 0,
        // The only denominator a market may carry. Sizes here are therefore
        // whole base units where a test reads a quote amount back, and plain
        // counts where it does not.
        base_precision: crate::state::BASE_PRECISION,
        order_tick_size: 1,
        order_step_size: 1,
        min_order_size: 1,
        // The default fixture leaves the floor off, so every existing test
        // reads the behaviour a market with zeroed reserved bytes gets.
        blocking_min_size: 0,
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
    side: SideV0,
    price: u64,
    size: u64,
    user: UserRefV0,
) -> ClobOrderRefV0 {
    let order_ref = place_raw(book, side, price, size, user).expect("placement succeeds");
    assert_consistent(book);
    order_ref
}

pub fn place_raw(
    book: &mut ClobMarketV0,
    side: SideV0,
    price: u64,
    size: u64,
    user: UserRefV0,
) -> Result<ClobOrderRefV0> {
    book.place(PlaceOrderParams {
        client_order_id: client_id(book.next_order_id),
        ..params(side, price, size, user)
    })
}

/// The client id every test placement carries, derived from the book id the
/// order is about to get. Tests name an order once and both of its ids follow,
/// which is what lets an assertion pin that the book reported the caller's id
/// back rather than its own.
pub fn client_id(order_id: u64) -> u32 {
    // Wrapping because one test drives `next_order_id` to its ceiling, and a
    // helper that only has to be deterministic should not panic there.
    (order_id as u32).wrapping_add(1_000)
}

/// Place a migrated taker remainder — same order, [`OrderBitFlag::TakerOrigin`]
/// set.
pub fn place_taker_origin(
    book: &mut ClobMarketV0,
    side: SideV0,
    price: u64,
    size: u64,
    user: UserRefV0,
) -> ClobOrderRefV0 {
    let order_ref = book
        .place(PlaceOrderParams {
            taker_origin: true,
            client_order_id: client_id(book.next_order_id),
            ..params(side, price, size, user)
        })
        .expect("placement succeeds");
    assert_consistent(book);
    order_ref
}

pub fn params(side: SideV0, price: u64, size: u64, user: UserRefV0) -> PlaceOrderParams {
    PlaceOrderParams {
        side,
        price,
        base_asset_amount: size,
        user,
        activation_slot: 0,
        placed_slot: 0,
        max_ts: 0,
        now: 0,
        taker_origin: false,
        client_order_id: 0,
        reject_if_crossed: false,
        reduce_only: false,
    }
}

/// A quote with no user set, no caps, no taker and no price bound. A test
/// overrides the fields it needs with struct update syntax.
pub fn quote_args(direction: DirectionV0, size: u64) -> QuoteArgsV0<'static> {
    QuoteArgsV0 {
        users: &[],
        direction,
        size,
        caps: UserCapsV0::EMPTY,
        reference_price: None,
        taker: None,
        limit_price: 0,
        taker_served_window: false,
        include_taker_origin_reservations: false,
    }
}

/// The execute counterpart of [`quote_args`].
pub fn execute_args(direction: DirectionV0, size: u64) -> ExecuteArgsV0<'static> {
    ExecuteArgsV0 {
        users: &[],
        direction,
        size,
        caps: UserCapsV0::EMPTY,
        reference_price: None,
        taker: None,
        taker_served_window: false,
        include_taker_origin_reservations: false,
    }
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
    let mut taker_origin = [0usize; 2];

    for side in [SideV0::Bid, SideV0::Ask] {
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
            if node.is_taker_origin() {
                taker_origin[side.tag() as usize] += 1;
            }

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
    assert_claimants_listed(book, taker_origin);
}

/// The exhaustive version of the claimant-list invariant the on-chain
/// `validate_book` checks the endpoints of.
///
/// Every listed node is a live taker-origin order of that side, the links are
/// mutual, and the ids ascend — the list is in rest order, which is what makes
/// the oldest remainder the first one paid. `taker_origin` is how many
/// taker-origin orders the price walk found on each side, so a listed order
/// that is not on the side, or an unlisted order that is, fails the count.
#[track_caller]
fn assert_claimants_listed(book: &ClobMarketV0, taker_origin: [usize; 2]) {
    for side in [SideV0::Bid, SideV0::Ask] {
        let list = side.tag() as usize;
        let mut cursor = book.first_claimant(side);
        let mut prev = NIL;
        let mut count = 0usize;
        let mut last_id = 0u64;
        while cursor != NIL {
            let node = book.read_node(cursor).unwrap();
            assert!(
                node.is_bit_flag_set(OrderBitFlag::Open),
                "freed node {cursor} is on the {side:?} claimant list"
            );
            assert!(
                node.is_taker_origin(),
                "node {cursor} is listed without the taker-origin flag"
            );
            assert_eq!(node.side(), side, "claimant {cursor} is on the wrong side");
            assert_eq!(node.taker_origin_prev, prev, "claimant {cursor} back-link");
            assert!(
                node.order_id > last_id,
                "the {side:?} claimant list is out of rest order at node {cursor}"
            );

            last_id = node.order_id;
            prev = cursor;
            cursor = node.taker_origin_next;
            count += 1;
            assert!(count <= book.capacity(), "{side:?} claimant list cycles");
        }

        assert_eq!(
            count, taker_origin[list],
            "{side:?} holds {} taker-origin orders and lists {count}",
            taker_origin[list]
        );
        assert_eq!(
            count,
            book.claimant_count(side) as usize,
            "{side:?} claimant count"
        );
        assert_eq!(prev, book.last_claimant(side), "{side:?} claimant tail");
    }
}
