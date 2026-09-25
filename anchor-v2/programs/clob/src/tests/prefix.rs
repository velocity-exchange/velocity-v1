//! Velocity floors each quoted level to the market step and may allocate any
//! step-aligned prefix of the ladder, because it splits a fill pro rata with
//! other sources. Every such prefix must execute to exactly that base.
//!
//! Each case here once quoted depth behind an order the walk truncated. A
//! prefix that ended inside that depth then needed a second partial record,
//! and the execute stopped short.

use {
    super::{
        market::{execute_args, place, place_taker_origin, quote_args, test_config, user},
        response::streamed,
    },
    crate::{
        book::ClobBook,
        state::{
            ClobMarketV0, DirectionV0, ExecuteResponseV0, MarketConfigV0, QuoteResponseV0, SideV0,
            UserCapV0, UserCapsV0, UserRefV0, BASE_PRECISION,
        },
        tests::market::TestMarket,
    },
    quoter_spec::{ExecuteArgsV0, QuoteArgsV0},
};

const B: u64 = BASE_PRECISION;

/// A tenth of a base, so a budget can buy a size off the step grid.
const STEP: u64 = B / 10;

/// What one caller asks of a two-maker book. The first maker's cap sits at
/// caps index 0.
struct Caller {
    users: [UserRefV0; 2],
    caps: UserCapsV0,
    reference_price: Option<u64>,
}

impl Caller {
    fn uncapped() -> Self {
        Self {
            users: [user(1), user(2)],
            caps: UserCapsV0::EMPTY,
            reference_price: None,
        }
    }

    fn first_maker_capped(quote_cap: u64, base_cap: u64, reference_price: u64) -> Self {
        let mut caps = UserCapsV0::EMPTY;
        caps.caps[0] = UserCapV0 {
            index: 0,
            quote_cap,
            base_cap,
        };
        caps.len = 1;

        Self {
            users: [user(1), user(2)],
            caps,
            reference_price: Some(reference_price),
        }
    }

    fn quote(&self, book: &mut ClobMarketV0, size: u64) -> Vec<(u64, u64)> {
        let args = QuoteArgsV0 {
            users: &self.users,
            caps: self.caps,
            reference_price: self.reference_price,
            ..quote_args(DirectionV0::Long, size)
        };
        let pointer = book.quote(&args, 5, 0).expect("quote succeeds");
        let bytes = streamed(book, pointer);
        QuoteResponseV0::parse(&bytes)
            .unwrap()
            .levels
            .iter()
            .map(|level| (level.price, level.size))
            .collect()
    }

    /// The base and quote an execute of `size` delivered.
    fn execute(&self, book: &mut ClobMarketV0, size: u64) -> (u64, u64) {
        let args = ExecuteArgsV0 {
            users: &self.users,
            caps: self.caps,
            reference_price: self.reference_price,
            ..execute_args(DirectionV0::Long, size)
        };
        let outcome = book.execute(&args, 5, 0).expect("execute succeeds");
        let bytes = streamed(book, outcome.response);
        let response = ExecuteResponseV0::parse(&bytes).unwrap();
        response
            .changes
            .iter()
            .fold((0, 0), |(base, quote), change| {
                (base + change.base_size, quote + change.quote_size)
            })
    }
}

/// The quote of `base` taken best first off `levels`, floored once.
fn prefix_quote(levels: &[(u64, u64)], base: u64) -> u64 {
    let mut left = base;
    let notional: u128 = levels
        .iter()
        .map(|&(price, size)| {
            let take = size.min(left);
            left -= take;
            price as u128 * take as u128
        })
        .sum();
    (notional / B as u128) as u64
}

/// Quotes a fresh book, then executes step-aligned prefixes of the ladder on
/// fresh copies of the same book. The prefixes are a tenth of a base apart at
/// least, so a one-unit step does not ask for billions of them.
fn assert_every_prefix_executes(
    config: MarketConfigV0,
    build: impl Fn(&mut ClobMarketV0),
    caller: &Caller,
    expected_ladder: &[(u64, u64)],
) {
    let market = TestMarket::new_with(16, config);
    let mut book = market.book();
    build(&mut book);
    let ladder = caller.quote(&mut book, 20 * B);
    assert_eq!(ladder, expected_ladder);
    drop(book);

    let quoted: u64 = ladder.iter().map(|&(_, size)| size).sum();
    let stride = config.order_step_size.max(STEP);
    for allocation in (1..=quoted / stride).map(|k| k * stride) {
        let market = TestMarket::new_with(16, config);
        let mut book = market.book();
        build(&mut book);
        assert_eq!(
            caller.execute(&mut book, allocation),
            (allocation, prefix_quote(&ladder, allocation)),
            "allocation {allocation} of ladder {ladder:?}"
        );
    }
}

/// The first maker can lose 40, and its ask costs it 10 a base against the
/// reference, so its budget buys 4 of its 10.
#[test]
fn a_budget_cut_ends_the_ladder_at_the_cut_order() {
    assert_every_prefix_executes(
        test_config(),
        |book| {
            place(book, SideV0::Ask, 100, 10 * B, user(1));
            place(book, SideV0::Ask, 101, 10 * B, user(2));
        },
        &Caller::first_maker_capped(40, u64::MAX, 110),
        &[(100, 4 * B)],
    );
}

/// The first maker's budget buys 4.03 base. The cut floors to the step, so the
/// level velocity reads is the level the book fills.
#[test]
fn a_budget_cut_off_the_step_grid_floors_to_the_step() {
    let config = MarketConfigV0 {
        order_step_size: STEP,
        min_order_size: STEP,
        blocking_min_size: 2 * STEP,
        ..test_config()
    };

    assert_every_prefix_executes(
        config,
        |book| {
            place(book, SideV0::Ask, 100, 10 * B, user(1));
            place(book, SideV0::Ask, 101, 10 * B, user(2));
        },
        &Caller::first_maker_capped(403, u64::MAX, 200),
        &[(100, 4 * B)],
    );
}

/// A reduce-only ask larger than its owner's cover is cut to the cover.
#[test]
fn a_cover_cut_ends_the_ladder_at_the_cut_order() {
    assert_every_prefix_executes(
        test_config(),
        |book| {
            book.place(crate::state::PlaceOrderParams {
                reduce_only: true,
                ..super::market::params(SideV0::Ask, 100, 10 * B, user(1))
            })
            .expect("placement succeeds");
            place(book, SideV0::Ask, 101, 10 * B, user(2));
        },
        &Caller::first_maker_capped(u64::MAX, 3 * B, 100),
        &[(100, 3 * B)],
    );
}

/// A taker-origin bid claims 3 of the best ask. The ask is passed over whole,
/// so the depth behind it stays on offer and no prefix cuts two orders.
#[test]
fn a_partly_claimed_order_is_passed_over_whole() {
    assert_every_prefix_executes(
        test_config(),
        |book| {
            place(book, SideV0::Ask, 100, 10 * B, user(1));
            place(book, SideV0::Ask, 101, 10 * B, user(2));
            place_taker_origin(book, SideV0::Bid, 100, 3 * B, user(3));
        },
        &Caller::uncapped(),
        &[(101, 10 * B)],
    );
}
