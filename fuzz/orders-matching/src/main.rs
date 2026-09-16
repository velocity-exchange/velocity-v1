//! P6 `orders-matching` — host-tier property harnesses for Family IV
//! (fill bounds / order & auction / matching math).
//!
//! Every property fuzzes a pure velocity `math::{orders,auction,matching}`
//! function directly (no LiteSVM). The `StdFixture` only exists to satisfy
//! `#[fuzz_fixture]`'s `TestContext`/action requirements — the math is
//! stateless so `action_noop` is the sole (trivial) action.
//!
//! One `[features]` entry / `fn main` is generated per `#[crucible_fuzz]`; the
//! function bodies themselves are always compiled, so all properties live in
//! this one file and `crucible run orders-matching <prop>` selects one.

use {
    crucible_fuzzer::*,
    velocity::{
        controller::position::PositionDirection,
        math::{
            auction::{calculate_auction_price, is_auction_complete},
            constants::BASE_PRECISION_U64,
            matching::{calculate_fill_for_matched_orders, do_orders_cross},
            orders::{
                is_multiple_of_step_size, is_new_order_risk_increasing, is_order_position_reducing,
                standardize_base_asset_amount, standardize_base_asset_amount_ceil,
                standardize_base_asset_amount_with_remainder_i128, standardize_price,
                validate_fill_price,
            },
            time::SlotClock,
        },
        state::user::{Order, OrderType},
    },
};

#[derive(Clone)]
struct StdFixture {
    // Host-tier harnesses don't drive instructions, but #[fuzz_fixture] requires
    // a TestContext field for its snapshot/clone wiring. Left unused.
    ctx: TestContext,
}

#[fuzz_fixture]
impl StdFixture {
    pub fn setup() -> Self {
        StdFixture {
            ctx: TestContext::new(),
        }
    }

    // #[fuzz_fixture] requires at least one discovered action.
    pub fn action_noop(&mut self) {
        let _ = &self.ctx;
    }
}

fn dir(is_long: bool) -> PositionDirection {
    if is_long {
        PositionDirection::Long
    } else {
        PositionDirection::Short
    }
}

/// Property 1a (Family IV): `standardize_base_asset_amount` /
/// `standardize_base_asset_amount_ceil` — floor ≤ x ≤ ceil, both are exact
/// step multiples, floor is idempotent, ceil − floor ∈ {0, step}, and the
/// remainder decomposition (`standardized + remainder == x`) is exact.
#[cfg(feature = "prop_standardize_base")]
#[crucible_fuzz]
fn prop_standardize_base(
    fixture: &mut StdFixture,
    #[range(0..1_000_000_000_000_000u64)] amount: u64,
    #[range(1..1_000_000_000u64)] step: u64,
) {
    let _ = &fixture.ctx;

    let floor = standardize_base_asset_amount(amount, step).unwrap();
    let ceil = standardize_base_asset_amount_ceil(amount, step).unwrap();

    // floor never exceeds the input; both endpoints are exact step multiples.
    fuzz_assert_le!(floor, amount);
    fuzz_assert_eq!(floor % step, 0u64);
    fuzz_assert_eq!(ceil % step, 0u64);
    fuzz_assert!(is_multiple_of_step_size(floor, step).unwrap());
    fuzz_assert!(is_multiple_of_step_size(ceil, step).unwrap());

    // floor ≤ x ≤ ceil.
    fuzz_assert_le!(floor, amount);
    fuzz_assert_le!(amount, ceil);

    // Idempotence: standardizing an already-standard value is a no-op.
    let floor2 = standardize_base_asset_amount(floor, step).unwrap();
    fuzz_assert_eq!(floor2, floor);
    fuzz_assert_eq!(
        standardize_base_asset_amount_ceil(floor, step).unwrap(),
        floor
    );

    // ceil − floor is 0 (already aligned) or exactly one step.
    let gap = ceil - floor;
    fuzz_assert!(gap == 0u64 || gap == step);
    // Already-aligned inputs pin both endpoints to the input.
    if amount % step == 0 {
        fuzz_assert_eq!(floor, amount);
        fuzz_assert_eq!(ceil, amount);
    }

    // Remainder decomposition is exact: standardized + remainder == x.
    let (std_i128, rem_i128) =
        standardize_base_asset_amount_with_remainder_i128(amount as i128, step as u128).unwrap();
    fuzz_assert_eq!(std_i128 + rem_i128, amount as i128);
    fuzz_assert_eq!(std_i128, floor as i128);
}

/// Property 1b (Family IV): `standardize_price` — result is an exact tick
/// multiple, idempotent, Long rounds down / Short rounds up (within one tick),
/// and floor(Long) ≤ price ≤ ceil(Short).
#[cfg(feature = "prop_standardize_price")]
#[crucible_fuzz]
fn prop_standardize_price(
    fixture: &mut StdFixture,
    #[range(0..1_000_000_000_000_000u64)] price: u64,
    #[range(1..1_000_000_000u64)] tick: u64,
    is_long: bool,
) {
    let _ = &fixture.ctx;

    let d = dir(is_long);
    let s = standardize_price(price, tick, d).unwrap();

    // Exact tick multiple and idempotent.
    fuzz_assert_eq!(s % tick, 0u64);
    fuzz_assert_eq!(standardize_price(s, tick, d).unwrap(), s);

    // Directional rounding, always within one tick of the input.
    let down = standardize_price(price, tick, PositionDirection::Long).unwrap();
    let up = standardize_price(price, tick, PositionDirection::Short).unwrap();
    fuzz_assert_le!(down, price);
    fuzz_assert_le!(price, up);
    fuzz_assert_le!(price - down, tick);
    fuzz_assert_le!(up - price, tick);

    // price == 0 is a fixed point in both directions.
    if price == 0 {
        fuzz_assert_eq!(s, 0u64);
    }
}

/// Property 2 (Family IV): fixed-auction price is monotone in slot and always
/// within `[auction_start_price, auction_end_price]`; once the auction is
/// complete (`is_auction_complete`) the price pins to the end price.
#[cfg(feature = "prop_auction_price")]
#[crucible_fuzz]
fn prop_auction_price(
    fixture: &mut StdFixture,
    // Prices are taken as u64 and cast to i64: crucible's `#[range]` maps a
    // signed field via `start + (raw % size)`, and `%` preserves the sign of a
    // negative raw i64 — so an i64 range does NOT guarantee positivity. A u64
    // range does (`raw % size` is always non-negative), and the auction math
    // casts start/end back to u64, which would error on a negative price.
    #[range(1..1_000_000_000_000u64)] price_a: u64,
    #[range(1..1_000_000_000_000u64)] price_b: u64,
    #[range(1..200u8)] duration: u8,
    #[range(0..500u64)] slot: u64,
    is_long: bool,
) {
    let _ = &fixture.ctx;

    // Order the two prices; Long ramps lo→hi, Short ramps hi→lo (the fixed
    // auction math requires end≥start for Long and start≥end for Short).
    let lo = price_a.min(price_b) as i64;
    let hi = price_a.max(price_b) as i64;
    let (start, end) = if is_long { (lo, hi) } else { (hi, lo) };

    // tick_size 1 makes standardize_price a no-op so monotonicity is exact.
    let tick = 1u64;
    let order = Order {
        order_type: OrderType::Market,
        direction: dir(is_long),
        slot: 0,
        auction_duration: duration,
        auction_start_price: start,
        auction_end_price: end,
        ..Order::default()
    };

    // The 400ms baseline clock: this fixture sets no IBRL transition.
    let clock = SlotClock::default();
    let p0 = calculate_auction_price(&order, slot, tick, None, clock).unwrap();
    let p1 = calculate_auction_price(&order, slot + 1, tick, None, clock).unwrap();

    // Always within the [lo, hi] band.
    fuzz_assert_le!(lo as u64, p0);
    fuzz_assert_le!(p0, hi as u64);

    // Monotone in slot: Long non-decreasing, Short non-increasing.
    if is_long {
        fuzz_assert_le!(p0, p1);
    } else {
        fuzz_assert_le!(p1, p0);
    }

    // Auction completeness: complete iff slots_elapsed > duration; once
    // complete the price equals the end price.
    let complete = is_auction_complete(0, duration, slot, clock).unwrap();
    fuzz_assert_eq!(complete, slot > duration as u64);
    if slot >= duration as u64 {
        fuzz_assert_eq!(p0, end as u64);
    }
}

/// Property 3 (Family IV): a matched fill respects BOTH sides' limits and the
/// maker fill amount equals the taker fill amount. The fill executes at the
/// maker's limit price; when the orders cross, the taker is price-improved and
/// `validate_fill_price` (with its is_taker rounding) accepts both sides.
#[cfg(feature = "prop_matched_fill")]
#[crucible_fuzz]
fn prop_matched_fill(
    fixture: &mut StdFixture,
    #[range(1_000_000..1_000_000_000_000u64)] maker_price: u64,
    #[range(1_000_000..1_000_000_000_000u64)] taker_price: u64,
    #[range(1_000_000_000..1_000_000_000_000_000u64)] maker_base: u64,
    #[range(1_000_000_000..1_000_000_000_000_000u64)] taker_base: u64,
    maker_is_long: bool,
) {
    let _ = &fixture.ctx;

    let maker_direction = dir(maker_is_long);
    let taker_direction = maker_direction.opposite();
    let base_decimals = 9u32; // BASE_PRECISION == 1e9

    let (base, quote) = calculate_fill_for_matched_orders(
        maker_base,
        maker_price,
        taker_base,
        base_decimals,
        maker_direction,
    )
    .unwrap();

    // Maker fill amount == taker fill amount == min(sizes).
    fuzz_assert_eq!(base, maker_base.min(taker_base));

    // The maker always fills at its own limit price (rounding favors the
    // maker), so validate_fill_price accepts the maker side unconditionally.
    fuzz_assert!(validate_fill_price(
        quote,
        base,
        BASE_PRECISION_U64,
        maker_direction,
        maker_price,
        false,
    )
    .is_ok());

    // When the orders cross, the taker never fills worse than its own limit.
    let crosses = do_orders_cross(maker_direction, maker_price, taker_price);
    if crosses && base > 0 {
        fuzz_assert!(validate_fill_price(
            quote,
            base,
            BASE_PRECISION_U64,
            taker_direction,
            taker_price,
            true,
        )
        .is_ok());
    }
}

/// Property 4 (Family IV): fillable size never exceeds the order's remaining
/// size; reduce-only orders can never fill more than the opposing position
/// (so they never increase it); `is_order_position_reducing` and
/// `is_new_order_risk_increasing` agree with their definitions and with each
/// other (a strictly-reducing order with no other open orders is not
/// risk-increasing, and any reduce-only order is never risk-increasing).
#[cfg(feature = "prop_fill_bounds_reduce_only")]
#[crucible_fuzz]
fn prop_fill_bounds_reduce_only(
    fixture: &mut StdFixture,
    #[range(0..1_000_000_000_000u64)] order_base: u64,
    #[range(0..1_000_000_000_000u64)] raw_filled: u64,
    #[range(-1_000_000_000_000..1_000_000_000_000i64)] position: i64,
    reduce_only: bool,
    is_long: bool,
) {
    let _ = &fixture.ctx;

    let d = dir(is_long);
    // filled must not exceed the order size.
    let filled = if order_base == 0 {
        0
    } else {
        raw_filled % (order_base + 1)
    };

    let order = Order {
        direction: d,
        base_asset_amount: order_base,
        base_asset_amount_filled: filled,
        reduce_only,
        ..Order::default()
    };

    // Remaining unfilled size never exceeds base − filled.
    let unfilled = order
        .get_base_asset_amount_unfilled(Some(position))
        .unwrap();
    fuzz_assert_le!(unfilled, order_base - filled);

    // A matched fill against this remaining size can never exceed it.
    let (fill_base, _) =
        calculate_fill_for_matched_orders(unfilled, 1_000_000, order_base, 9, d).unwrap();
    fuzz_assert_le!(fill_base, unfilled);

    // Reduce-only never increases the position: fillable ≤ |existing position|.
    if reduce_only && position != 0 {
        fuzz_assert_le!(unfilled, position.unsigned_abs());
    }

    // Reduce-only detection vs the definition (opposite side & size ≤ |pos|).
    let reducing = is_order_position_reducing(&d, order_base, position).unwrap();
    let expected_reducing = match d {
        PositionDirection::Long => position < 0 && order_base <= position.unsigned_abs(),
        PositionDirection::Short => position > 0 && order_base <= position.unsigned_abs(),
    };
    fuzz_assert_eq!(reducing, expected_reducing);

    // Risk-increasing detection: reduce-only orders are never risk-increasing.
    let ro_order = Order {
        reduce_only: true,
        ..order.clone()
    };
    fuzz_assert!(!is_new_order_risk_increasing(&ro_order, position, 0, 0).unwrap());

    // With no other open orders, a strictly position-reducing (non-reduce-only)
    // order is not risk-increasing.
    let plain_order = Order {
        reduce_only: false,
        ..order.clone()
    };
    let risk_increasing = is_new_order_risk_increasing(&plain_order, position, 0, 0).unwrap();
    if reducing {
        fuzz_assert!(!risk_increasing);
    }
}
