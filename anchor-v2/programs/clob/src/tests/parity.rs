//! Quote and execute parity: the depth `quote` publishes is the depth
//! `execute` delivers when a caller sends the quoted size with the same
//! arguments.

use {
    super::market::{assert_consistent, params, place, test_config, user, TestMarket},
    crate::{
        book::{ClobBook, NodeArena},
        state::{
            ClobMarketV0, Direction, ExecuteResponseV0, MarketConfigV0, PlaceOrderParams,
            QuoteResponseV0, Side, UserCapV0, UserCapsV0, UserRefV0, BASE_PRECISION,
        },
    },
    std::collections::BTreeMap,
};

/// The arguments `quote` and `execute` share.
struct Sweep<'a> {
    direction: Direction,
    users: &'a [UserRefV0],
    caps: &'a UserCapsV0,
    reference_price: i64,
    taker: Option<&'a UserRefV0>,
    include_taker_origin_reservations: bool,
    slot: u64,
    now: i64,
}

/// What a sweep trades, as `(price, base)` levels in walk order.
type Levels = Vec<(u64, u64)>;

fn quote_levels(book: &mut ClobMarketV0, sweep: &Sweep, size: u64, limit_price: u64) -> Levels {
    let pointer = book
        .quote(
            sweep.direction,
            size,
            sweep.users,
            sweep.caps,
            sweep.reference_price,
            sweep.taker,
            limit_price,
            sweep.include_taker_origin_reservations,
            sweep.slot,
            sweep.now,
        )
        .expect("quote succeeds");
    let bytes = super::response::streamed(book, pointer);
    QuoteResponseV0::parse(&bytes)
        .expect("quote response")
        .levels
        .iter()
        .map(|level| (level.price, level.size))
        .collect()
}

/// The fills of one execute, merged into levels by the price of each filled
/// order, and the quote the response attributes to them.
struct Executed {
    levels: Levels,
    quote_total: u128,
}

fn execute_levels(book: &mut ClobMarketV0, sweep: &Sweep, size: u64) -> Executed {
    let prices: BTreeMap<u64, u64> = (0..book.len() as u32)
        .map(|index| book.read_node(index).unwrap())
        .filter(|node| node.is_open())
        .map(|node| (node.order_id, node.price))
        .collect();

    let outcome = book
        .execute(
            sweep.direction,
            size,
            sweep.users,
            sweep.caps,
            sweep.reference_price,
            sweep.taker,
            sweep.include_taker_origin_reservations,
            sweep.slot,
            sweep.now,
        )
        .expect("execute succeeds");

    let mut levels: Levels = Vec::new();
    for fill in &outcome.fills {
        let price = prices[&fill.order_id];
        match levels.last_mut() {
            Some((last_price, base)) if *last_price == price => *base += fill.base_size,
            _ => levels.push((price, fill.base_size)),
        }
    }

    let bytes = super::response::streamed(book, outcome.response);
    let response = ExecuteResponseV0::parse(&bytes).expect("execute response");
    let quote_total = response
        .changes
        .iter()
        .map(|change| change.quote_size as u128)
        .sum();
    let base_total: u64 = response.changes.iter().map(|change| change.base_size).sum();
    assert_eq!(
        base_total,
        levels.iter().map(|(_, base)| base).sum::<u64>(),
        "the balance changes carry every fill"
    );

    Executed {
        levels,
        quote_total,
    }
}

/// Quote, then execute the quoted size with the same arguments, and require
/// the same levels and the notional the quote implies.
#[track_caller]
fn assert_execute_delivers_quote(book: &mut ClobMarketV0, sweep: &Sweep, size: u64, limit: u64) {
    let quoted = quote_levels(book, sweep, size, limit);
    let quoted_base: u64 = quoted.iter().map(|(_, base)| base).sum();
    if quoted_base == 0 {
        return;
    }

    let executed = execute_levels(book, sweep, quoted_base);
    assert_eq!(executed.levels, quoted, "execute fills the quoted levels");

    let notional: u128 = quoted
        .iter()
        .map(|(price, base)| *price as u128 * *base as u128)
        .sum();
    assert_eq!(
        executed.quote_total,
        notional / BASE_PRECISION as u128,
        "execute attributes the quoted notional"
    );
    assert_consistent(book);
}

fn empty_sweep(direction: Direction) -> Sweep<'static> {
    Sweep {
        direction,
        users: &[],
        caps: &UserCapsV0::EMPTY,
        reference_price: 0,
        taker: None,
        include_taker_origin_reservations: false,
        slot: 0,
        now: 0,
    }
}

/// With no user set, `execute` stops at `max_execute_users` balance changes, so
/// `quote` must stop there too. An owner already counted costs nothing, so one
/// that returns later in the queue still fills.
#[test]
fn an_empty_set_quote_counts_owners_as_execute_does() {
    let config = test_config();
    let market = TestMarket::new_with(32, config);
    let mut book = market.book();
    let max_users = config.max_execute_users as u8;
    assert!(config.max_execute_fills as u8 > max_users + 2);

    let repeat = user(1);
    place(&mut book, Side::Ask, 99, 5, repeat);
    for seed in 2..=max_users {
        place(&mut book, Side::Ask, 100, 5, user(seed));
    }

    place(&mut book, Side::Ask, 100, 5, repeat);
    for seed in max_users + 1..=max_users + 2 {
        place(&mut book, Side::Ask, 100, 5, user(seed));
    }

    let sweep = empty_sweep(Direction::Long);
    let quoted = quote_levels(&mut book, &sweep, u64::MAX, 0);
    assert_eq!(quoted, vec![(99, 5), (100, 5 * max_users as u64)]);

    let executed = execute_levels(&mut book, &sweep, u64::MAX);
    assert_eq!(executed.levels, quoted);
    assert_consistent(&book);
}

/// splitmix64. Deterministic, so a failing seed reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

const OWNERS: u8 = 7;

fn random_config(rng: &mut Rng) -> MarketConfigV0 {
    MarketConfigV0 {
        min_order_size: [1, BASE_PRECISION / 10][rng.below(2) as usize],
        blocking_min_size: [0, 2 * BASE_PRECISION][rng.below(2) as usize],
        unknown_user_grace_slots: rng.below(4) as u32,
        max_quote_levels: 1 + rng.below(8) as u16,
        max_execute_fills: 1 + rng.below(10) as u16,
        max_execute_users: 1 + rng.below(5) as u16,
        ..test_config()
    }
}

fn place_random_order(book: &mut ClobMarketV0, rng: &mut Rng) {
    let side = if rng.chance(50) { Side::Bid } else { Side::Ask };
    let price = 95 + rng.below(11);
    let size = BASE_PRECISION / 10 * (1 + rng.below(40));
    let activation_slot = if rng.chance(30) { rng.below(20) } else { 0 };
    let max_ts = if rng.chance(25) {
        50 + rng.below(100) as i64
    } else {
        0
    };

    // A placement the book refuses, such as one below the minimum, is skipped.
    let _ = book.place(PlaceOrderParams {
        activation_slot,
        max_ts,
        taker_origin: rng.chance(20),
        reduce_only: rng.chance(15),
        ..params(side, price, size, user(1 + rng.below(OWNERS as u64) as u8))
    });
}

fn random_caps(rng: &mut Rng, set_len: usize) -> UserCapsV0 {
    let mut caps = UserCapsV0::EMPTY;
    if set_len == 0 || rng.chance(30) {
        return caps;
    }

    for index in 0..set_len {
        if rng.chance(15) {
            caps.exclude(index);
            continue;
        }

        if rng.chance(50) {
            let unbounded_or = |rng: &mut Rng, bound: u64| {
                if rng.chance(40) {
                    u64::MAX
                } else {
                    rng.below(bound)
                }
            };
            caps.caps[caps.len as usize] = UserCapV0 {
                quote_cap: unbounded_or(rng, 50),
                base_cap: unbounded_or(rng, 3 * BASE_PRECISION),
                index: index as u8,
            };
            caps.len += 1;
        }
    }

    caps
}

/// Random books with remainders, reduce-only orders, expiries, activation
/// delays, caps, and both an empty and a named user set. Each quote is then
/// executed at its own size with the same arguments.
#[test]
fn random_books_execute_exactly_what_they_quote() {
    let mut rng = Rng(0x5EED_CAFE);
    for _ in 0..8_000 {
        let market = TestMarket::new_with(64, random_config(&mut rng));
        let mut book = market.book();
        for _ in 0..rng.below(24) {
            place_random_order(&mut book, &mut rng);
        }

        let owners: Vec<UserRefV0> = (1..=OWNERS).map(user).collect();
        let users: Vec<UserRefV0> = if rng.chance(40) {
            Vec::new()
        } else {
            owners.iter().copied().filter(|_| rng.chance(60)).collect()
        };
        let caps = random_caps(&mut rng, users.len());
        let taker = owners[rng.below(OWNERS as u64) as usize];
        let sweep = Sweep {
            direction: if rng.chance(50) {
                Direction::Long
            } else {
                Direction::Short
            },
            users: &users,
            caps: &caps,
            reference_price: 95 + rng.below(11) as i64,
            taker: rng.chance(30).then_some(&taker),
            include_taker_origin_reservations: rng.chance(20),
            slot: rng.below(60),
            now: rng.below(200) as i64,
        };
        let size = if rng.chance(30) {
            u64::MAX
        } else {
            BASE_PRECISION / 10 * (1 + rng.below(200))
        };
        let limit = if rng.chance(20) {
            95 + rng.below(11)
        } else {
            0
        };

        assert_execute_delivers_quote(&mut book, &sweep, size, limit);
    }
}
