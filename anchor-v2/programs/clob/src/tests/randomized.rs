//! Randomized properties of the book, checked over generated books and
//! operation sequences rather than hand-built cases.
//!
//! Velocity refuses an execute that delivers less than the depth its quote
//! promised, so quote and execute must agree for every book and every caller,
//! not only the ones a hand-written test thought of. The same holds for the
//! cross reservation, whose allocation these tests recompute from its stated
//! rules, and for the book's structural invariants across any sequence of
//! operations.

use {
    super::{
        market::{assert_consistent, execute_args, quote_args, test_config, user, TestMarket},
        response::streamed,
    },
    crate::{
        book::{ClobBook, NodeArena},
        state::{
            CancelSidesV0, ClobHeaderV0, ClobMarketV0, ClobOrderRefV0, DirectionV0,
            ExecuteResponseV0, L3ResponseV0, MarketConfigV0, OrderNodeV0, PlaceOrderParams,
            QuoteResponseV0, SideV0, UserCapV0, UserCapsV0, UserRefV0, L3_ROWS_CEILING, NIL,
        },
    },
    quoter_spec::{ExecuteArgsV0, QuoteArgsV0, L3_ROW_FLAG_RESERVED},
};

/// A tenth of a base unit, so sizes are large enough for per-user budgets to
/// bind at the prices these tests use.
const UNIT: u64 = 100_000_000;

/// Deterministic xorshift, so a failing seed reproduces.
struct Rng(u64);

impl Rng {
    fn seeded(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }

    fn side(&mut self) -> SideV0 {
        if self.chance(50) {
            SideV0::Ask
        } else {
            SideV0::Bid
        }
    }

    fn direction(&mut self) -> DirectionV0 {
        if self.chance(50) {
            DirectionV0::Long
        } else {
            DirectionV0::Short
        }
    }
}

/// A market config with the knobs that change what a walk skips drawn at
/// random. Returns the minimum order size in units alongside it.
fn random_config(rng: &mut Rng) -> (u64, MarketConfigV0) {
    let min_units = rng.pick(&[1u64, 1, 2, 3]);
    let config = MarketConfigV0 {
        order_step_size: UNIT,
        min_order_size: min_units * UNIT,
        blocking_min_size: rng.pick(&[0u64, 0, 4 * UNIT]),
        unknown_user_grace_slots: rng.pick(&[0u32, 2, 5]),
        max_execute_fills: rng.pick(&[2u16, 3, 5, 8]),
        max_execute_users: rng.pick(&[1u16, 2, 3, 4]),
        max_quote_levels: rng.pick(&[2u16, 3, 8]),
        evict_threshold_per_side: rng.pick(&[2u32, 6, 12]),
        ..test_config()
    };

    (min_units, config)
}

/// A loop rather than `filter` then `map`, because both closures would need
/// `rng` mutably at once.
fn random_caps(rng: &mut Rng, users: usize) -> UserCapsV0 {
    let mut caps = Vec::new();
    for index in 0..users {
        if rng.chance(40) {
            caps.push(UserCapV0 {
                index: index as u8,
                quote_cap: rng.pick(&[0u64, 1, 3, 10, 40, u64::MAX]),
                base_cap: rng.pick(&[u64::MAX, u64::MAX, UNIT, 3 * UNIT, 8 * UNIT]),
            });
        }
    }

    UserCapsV0::from_caps(caps).expect("each index is named once")
}

/// Place a random order, letting taker-origin orders cross so that claims
/// arise. Placement may fail on a full side, which a caller treats as a no-op.
fn place_random(
    book: &mut ClobMarketV0,
    rng: &mut Rng,
    pool: &[UserRefV0],
    min_units: u64,
    slot: u64,
    now: i64,
) {
    let side = rng.side();
    let taker_origin = rng.chance(20);
    let price = match (side, taker_origin) {
        (SideV0::Ask, false) => 98 + rng.below(12),
        (SideV0::Bid, false) => 90 + rng.below(12),
        (SideV0::Ask, true) => 92 + rng.below(12),
        (SideV0::Bid, true) => 96 + rng.below(12),
    };

    let _ = book.place(PlaceOrderParams {
        side,
        price,
        base_asset_amount: (min_units + rng.below(10)) * UNIT,
        user: rng.pick(pool),
        activation_slot: slot + rng.pick(&[0u64, 0, 0, 3, 8]),
        placed_slot: slot,
        max_ts: if rng.chance(25) {
            now + rng.pick(&[5i64, 30])
        } else {
            0
        },
        now,
        taker_origin,
        client_order_id: 0,
        reject_if_crossed: false,
        reduce_only: rng.chance(15),
    });
}

fn live_orders(book: &ClobMarketV0) -> Vec<(u32, OrderNodeV0)> {
    (0..book.capacity() as u32)
        .map(|index| (index, book.read_node(index).unwrap()))
        .filter(|(_, node)| node.is_open())
        .collect()
}

fn best_first(book: &ClobMarketV0, side: SideV0) -> Vec<(u32, OrderNodeV0)> {
    let mut orders = Vec::new();
    let mut cursor = book.best(side);
    while cursor != NIL {
        let node = book.read_node(cursor).unwrap();
        orders.push((cursor, node));
        cursor = node.next;
    }

    orders
}

fn order_ref(index: u32, node: &OrderNodeV0) -> ClobOrderRefV0 {
    ClobOrderRefV0 {
        node_index: index,
        order_id: node.order_id,
    }
}

/// One caller's view of a side.
struct Caller {
    direction: DirectionV0,
    size: u64,
    users: Vec<UserRefV0>,
    caps: UserCapsV0,
    reference_price: Option<u64>,
    taker: Option<UserRefV0>,
    include_reserved: bool,
}

impl Caller {
    fn random(rng: &mut Rng, pool: &[UserRefV0], allow_empty_set: bool) -> Self {
        let users: Vec<UserRefV0> = if allow_empty_set && rng.chance(15) {
            Vec::new()
        } else {
            pool.iter().copied().filter(|_| rng.chance(75)).collect()
        };
        let caps = random_caps(rng, users.len());

        Self {
            direction: rng.direction(),
            size: (1 + rng.below(40)) * UNIT,
            users,
            caps,
            reference_price: Some(rng.pick(&[95u64, 100, 105])),
            taker: rng.chance(40).then(|| rng.pick(pool)),
            include_reserved: rng.chance(15),
        }
    }
}

/// What a quote promised and what an execute of that size then delivered.
struct Delivery {
    quoted_base: u64,
    quoted_notional_floor: u64,
    filled_base: u64,
    filled_quote: u64,
}

/// Quote, then execute exactly the quoted base with the same arguments.
/// `None` when the quote offers nothing.
fn quote_then_execute(
    book: &mut ClobMarketV0,
    caller: &Caller,
    slot: u64,
    now: i64,
) -> Option<Delivery> {
    let pointer = book
        .quote(
            &QuoteArgsV0 {
                users: &caller.users,
                caps: caller.caps,
                reference_price: caller.reference_price,
                taker: caller.taker.as_ref().copied(),
                include_taker_origin_reservations: caller.include_reserved,
                ..quote_args(caller.direction, caller.size)
            },
            slot,
            now,
        )
        .expect("a quote on a consistent book succeeds");
    let bytes = streamed(book, pointer);
    let levels = QuoteResponseV0::parse(&bytes).unwrap().levels.to_vec();
    let quoted_base: u64 = levels.iter().map(|level| level.size).sum();
    if quoted_base == 0 {
        return None;
    }

    let notional: u128 = levels
        .iter()
        .map(|level| level.price as u128 * level.size as u128)
        .sum();
    let outcome = book
        .execute(
            &ExecuteArgsV0 {
                users: &caller.users,
                caps: caller.caps,
                reference_price: caller.reference_price,
                taker: caller.taker.as_ref().copied(),
                include_taker_origin_reservations: caller.include_reserved,
                ..execute_args(caller.direction, quoted_base)
            },
            slot,
            now,
        )
        .expect("an execute of the quoted size succeeds");
    let bytes = streamed(book, outcome.response);
    let executed = ExecuteResponseV0::parse(&bytes).unwrap();

    Some(Delivery {
        quoted_base,
        quoted_notional_floor: (notional / crate::state::BASE_PRECISION as u128) as u64,
        filled_base: executed.changes.iter().map(|change| change.base_size).sum(),
        filled_quote: executed
            .changes
            .iter()
            .map(|change| change.quote_size)
            .sum(),
    })
}

/// Seeds whose execute delivered a different base or notional than the quote
/// promised, and how many seeds had depth to check.
fn delivery_mismatches(allow_empty_set: bool) -> (usize, Vec<u64>) {
    let pool: Vec<UserRefV0> = (1..=6).map(user).collect();
    let mut checked = 0;
    let mut mismatched = Vec::new();
    for seed in 1..=20_000u64 {
        let mut rng = Rng::seeded(seed);
        let (min_units, config) = random_config(&mut rng);
        let market = TestMarket::new_with(64, config);
        let mut book = market.book();
        for _ in 0..rng.below(24) {
            place_random(&mut book, &mut rng, &pool, min_units, 0, 0);
        }

        let caller = Caller::random(&mut rng, &pool, allow_empty_set);
        let (slot, now) = (rng.below(14), rng.pick(&[0i64, 10, 60]));
        let Some(delivery) = quote_then_execute(&mut book, &caller, slot, now) else {
            continue;
        };

        checked += 1;
        assert_consistent(&book);
        if delivery.filled_base != delivery.quoted_base
            || delivery.filled_quote != delivery.quoted_notional_floor
        {
            mismatched.push(seed);
        }
    }

    (checked, mismatched)
}

/// For any book and any caller that names its users, an execute of the quoted
/// size fills that size in full, at the floor of the quoted notional.
#[test]
fn execute_delivers_what_quote_promised_to_a_named_user_set() {
    let (checked, mismatched) = delivery_mismatches(false);
    assert!(checked > 10_000, "only {checked} seeds had depth to check");
    assert!(
        mismatched.is_empty(),
        "{} seeds delivered a different fill than they quoted, first {:?}",
        mismatched.len(),
        &mismatched[..mismatched.len().min(5)]
    );
}

/// The same property with an unrestricted, empty user set. `quote` then counts
/// owners by ref against `max_execute_users`, as `execute` does.
#[test]
fn execute_delivers_what_quote_promised_to_an_unrestricted_caller() {
    let (_, mismatched) = delivery_mismatches(true);
    assert!(
        mismatched.is_empty(),
        "{} seeds delivered a different fill than they quoted, first {:?}",
        mismatched.len(),
        &mismatched[..mismatched.len().min(5)]
    );
}

/// No live order's expiry or pending activation is earlier than the hint
/// that is meant to wake a crank for it.
fn assert_hints_not_late(book: &ClobMarketV0, slot: u64, context: &str) {
    for (index, node) in live_orders(book) {
        assert!(
            node.max_ts == 0 || node.max_ts >= book.next_expiry_ts,
            "{context}: expiry hint {} is later than node {index} expiring at {}",
            book.next_expiry_ts,
            node.max_ts
        );
        assert!(
            node.activation_slot <= slot || node.activation_slot >= book.next_activation_slot,
            "{context}: activation hint {} is later than node {index} activating at {}",
            book.next_activation_slot,
            node.activation_slot
        );
    }
}

/// Random sequences of every book operation, with the clock moving. After each
/// step the book passes the exhaustive consistency check, and neither wake hint
/// is later than the order it should wake a crank for. An operation that the
/// book's own rules permit must succeed.
#[test]
fn random_operation_sequences_keep_the_book_consistent() {
    let pool: Vec<UserRefV0> = (1..=5).map(user).collect();
    for seed in 1..=3_000u64 {
        let mut rng = Rng::seeded(seed);
        let (min_units, config) = random_config(&mut rng);
        let market = TestMarket::new_with(48, config);
        let (mut slot, mut now) = (0u64, 0i64);

        for step in 0..60 {
            slot += rng.pick(&[0u64, 0, 1, 2, 5]);
            now += rng.pick(&[0i64, 0, 1, 3, 10]);
            let mut book = market.book();
            let context = format!("seed {seed} step {step}");
            match rng.below(7) {
                0 => place_random(&mut book, &mut rng, &pool, min_units, slot, now),
                1 => {
                    let orders = live_orders(&book);
                    if let Some(&(index, node)) =
                        orders.get(rng.below(orders.len() as u64) as usize)
                    {
                        let force = rng.chance(30);
                        let bound = is_bound(&book, &node, slot);
                        let result =
                            book.cancel(node.user_ref(), order_ref(index, &node), slot, force);
                        assert_eq!(
                            result.is_ok(),
                            force || !bound,
                            "{context}: cancel of a bound remainder without force"
                        );
                    }
                }
                2 => {
                    let sides = rng.pick(&[
                        CancelSidesV0::Bids,
                        CancelSidesV0::Asks,
                        CancelSidesV0::Both,
                    ]);
                    book.cancel_all(
                        rng.pick(&pool),
                        sides,
                        slot,
                        rng.chance(30),
                        &mut |_| Ok(()),
                    )
                    .unwrap_or_else(|error| panic!("{context}: cancel_all failed: {error:?}"));
                }
                3 => {
                    let side = rng.side();
                    let count = book.node_count(side);
                    if count > 0 && count >= book.evict_threshold_per_side {
                        let all_bound = live_orders(&book)
                            .iter()
                            .filter(|(_, node)| node.side() == side)
                            .all(|(_, node)| is_bound(&book, node, slot));
                        let result = book.evict_worst(side, slot);
                        assert_eq!(
                            result.is_ok(),
                            !all_bound,
                            "{context}: evict must pass over bound remainders and only them"
                        );
                    }
                }
                4 => {
                    let expired: Vec<_> = live_orders(&book)
                        .into_iter()
                        .filter(|(_, node)| node.is_expired(now))
                        .collect();
                    if let Some(&(index, node)) =
                        expired.get(rng.below(expired.len() as u64) as usize)
                    {
                        book.remove_expired(order_ref(index, &node), now)
                            .unwrap_or_else(|error| {
                                panic!("{context}: remove_expired failed: {error:?}")
                            });
                    }
                }
                5 => {
                    let remainders: Vec<_> = live_orders(&book)
                        .into_iter()
                        .filter(|(_, node)| node.is_taker_origin())
                        .collect();
                    if let Some(&(index, node)) =
                        remainders.get(rng.below(remainders.len() as u64) as usize)
                    {
                        let amount = (1 + rng.below(node.base_asset_amount / UNIT)) * UNIT;
                        let live = node.is_active(slot) && !node.is_expired(now);
                        let result = book.fill(
                            order_ref(index, &node),
                            amount.min(node.base_asset_amount),
                            slot,
                            now,
                        );
                        assert_eq!(
                            result.is_ok(),
                            live,
                            "{context}: fill must take a live remainder and only a live one"
                        );
                    }
                }
                _ => {
                    let caller = Caller::random(&mut rng, &pool, false);
                    if let Some(delivery) = quote_then_execute(&mut book, &caller, slot, now) {
                        assert_eq!(
                            delivery.filled_base, delivery.quoted_base,
                            "{context}: execute delivered a different base than it quoted"
                        );
                    }
                }
            }

            assert_consistent(&book);
            assert_hints_not_late(&book, slot, &context);
        }
    }
}

/// A taker remainder whose claim is still honoured, recomputed from the
/// documented rule: the bind lasts until activation plus the reservation grace.
fn is_bound(book: &ClobMarketV0, node: &OrderNodeV0, slot: u64) -> bool {
    node.is_taker_origin() && slot < node.activation_slot + book.reservation_grace_slots as u64
}

/// The units of each live order on `cover` that no ordinary caller may take,
/// recomputed from the reservation's documented rules: each taker-origin order
/// on the other side, oldest first, claims the best cover units still
/// unclaimed that it crosses; a claim withholds at least `min_order_size`; and
/// a taker-origin order on `cover` that a live counterparty crosses is
/// withheld whole.
fn reference_withheld(book: &ClobMarketV0, cover: SideV0, slot: u64, now: i64) -> Vec<(u32, u64)> {
    let claiming = cover.opposite();
    let grace = book.reservation_grace_slots as u64;
    let lapsed = |node: &OrderNodeV0| slot >= node.activation_slot.saturating_add(grace);
    let matchable = |node: &OrderNodeV0| !node.is_expired(now) && node.is_active(slot);

    let covers: Vec<(u32, OrderNodeV0)> = best_first(book, cover)
        .into_iter()
        .filter(|(_, node)| matchable(node))
        .collect();
    let mut claimants: Vec<OrderNodeV0> = best_first(book, claiming)
        .into_iter()
        .map(|(_, node)| node)
        .filter(|node| node.is_taker_origin() && !lapsed(node) && !node.is_expired(now))
        .collect();
    claimants.sort_by_key(|node| node.order_id);

    let mut claimed = vec![0u64; covers.len()];
    let mut position = 0;
    for claimant in claimants {
        let mut demand = claimant.base_asset_amount;
        while demand > 0 && position < covers.len() {
            let cover_order = covers[position].1;
            if !cover.is_crossed_by(cover_order.price, claimant.price) {
                break;
            }

            let take = demand.min(cover_order.base_asset_amount - claimed[position]);
            claimed[position] += take;
            demand -= take;
            if claimed[position] == cover_order.base_asset_amount {
                position += 1;
            }
        }
    }

    let counterparty = best_first(book, claiming)
        .into_iter()
        .map(|(_, node)| node)
        .find(|node| matchable(node))
        .map(|node| node.price);

    covers
        .iter()
        .zip(claimed)
        .map(|((index, node), claimed)| {
            let claimed = match claimed {
                0 => 0,
                units => units.max(book.min_order_size).min(node.base_asset_amount),
            };
            let crossed = counterparty.is_some_and(|price| cover.is_crossed_by(node.price, price));
            let withheld_whole = claimed < node.base_asset_amount
                && node.is_taker_origin()
                && !lapsed(node)
                && crossed;

            (
                *index,
                if withheld_whole {
                    node.base_asset_amount
                } else {
                    claimed
                },
            )
        })
        .collect()
}

/// Every row `quote_l3` reports withholds exactly the units the reference
/// allocation says it should, and flags the row reserved exactly when it does.
#[test]
fn the_reservation_matches_its_reference_allocation() {
    let pool: Vec<UserRefV0> = (1..=5).map(user).collect();
    let mut reads_with_claims = 0;
    for seed in 1..=20_000u64 {
        let mut rng = Rng::seeded(seed ^ 0x2545_F491_4F6C_DD1D);
        let (min_units, config) = random_config(&mut rng);
        let market = TestMarket::new_with(64, config);
        let mut book = market.book();
        book.reservation_grace_slots = rng.pick(&[0u16, 4, 32]);
        let mut slot = 0;
        for _ in 0..rng.below(20) {
            slot += rng.pick(&[0u64, 1, 3]);
            place_random(&mut book, &mut rng, &pool, min_units, slot, 0);
        }

        let read_slot = slot + rng.pick(&[0u64, 1, 4, 10, 40]);
        let now = rng.pick(&[0i64, 10, 30]);
        for direction in [DirectionV0::Long, DirectionV0::Short] {
            let expected = reference_withheld(&book, direction.side(), read_slot, now);
            let pointer = book
                .quote_l3(direction, 0, L3_ROWS_CEILING, false, read_slot, now)
                .unwrap();
            let bytes = streamed(&book, pointer);
            let rows = L3ResponseV0::parse(&bytes).unwrap().rows.to_vec();
            let actual: Vec<(u32, u64)> = rows
                .iter()
                .map(|row| {
                    let node = book.read_node(row.node_index).unwrap();
                    (row.node_index, node.base_asset_amount - row.size)
                })
                .collect();

            assert_eq!(
                actual, expected,
                "seed {seed} {direction:?} at slot {read_slot}: withheld units per row"
            );
            for row in &rows {
                let node = book.read_node(row.node_index).unwrap();
                assert_eq!(
                    row.flags & L3_ROW_FLAG_RESERVED != 0,
                    row.size < node.base_asset_amount,
                    "seed {seed}: reserved flag on node {}",
                    row.node_index
                );
            }

            if expected.iter().any(|(_, withheld)| *withheld > 0) {
                reads_with_claims += 1;
            }
        }
    }

    assert!(
        reads_with_claims > 5_000,
        "only {reads_with_claims} reads exercised a claim"
    );
}

/// Everything in the account outside the response buffer: the header with its
/// response zeroed, then every arena slot.
fn state_outside_response(book: &ClobMarketV0) -> Vec<u8> {
    let mut header: ClobHeaderV0 = **book;
    header.response.fill(0);
    let mut bytes = bytemuck::bytes_of(&header).to_vec();
    for index in 0..book.capacity() as u32 {
        bytes.extend_from_slice(bytemuck::bytes_of(&book.read_node(index).unwrap()));
    }

    bytes
}

/// `quote_v0` and `quote_l3_v0` take no signer and a writable market, so they
/// must change nothing but the response buffer.
#[test]
fn quotes_write_only_the_response_buffer() {
    let pool: Vec<UserRefV0> = (1..=5).map(user).collect();
    for seed in 1..=5_000u64 {
        let mut rng = Rng::seeded(seed);
        let (min_units, config) = random_config(&mut rng);
        let market = TestMarket::new_with(48, config);
        let mut book = market.book();
        for _ in 0..rng.below(20) {
            place_random(&mut book, &mut rng, &pool, min_units, 0, 0);
        }

        let before = state_outside_response(&book);
        let caller = Caller::random(&mut rng, &pool, true);
        let (slot, now) = (rng.below(40), rng.pick(&[0i64, 30]));
        book.quote(
            &QuoteArgsV0 {
                users: &caller.users,
                caps: caller.caps,
                reference_price: caller.reference_price,
                taker: caller.taker.as_ref().copied(),
                limit_price: rng.pick(&[0u64, 100]),
                include_taker_origin_reservations: caller.include_reserved,
                ..quote_args(caller.direction, caller.size)
            },
            slot,
            now,
        )
        .unwrap();
        book.quote_l3(
            caller.direction,
            0,
            L3_ROWS_CEILING,
            caller.include_reserved,
            slot,
            now,
        )
        .unwrap();

        assert!(
            state_outside_response(&book) == before,
            "seed {seed}: a quote changed state outside the response buffer"
        );
    }
}
