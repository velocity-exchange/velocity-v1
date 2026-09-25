//! P6 `orders-matching` — host-tier property harnesses for Family IV
//! (fill bounds / order sizing / worst price).
//!
//! Every property fuzzes a pure velocity `math::{orders,worst_price}`
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
            orders::{
                is_multiple_of_step_size, is_new_order_risk_increasing, is_order_position_reducing,
                standardize_base_asset_amount, standardize_base_asset_amount_ceil,
                standardize_base_asset_amount_with_remainder_i128, standardize_price,
            },
            worst_price::derive_worst_price,
        },
        state::{oracle::OraclePriceData, perp_market::ContractTier, user::Order},
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

/// Property 2 (Family IV): a named worst price is returned unchanged. An
/// unnamed one sits a tier's percentage of the oracle through the oracle, on
/// the side that lets a taker in `direction` cross.
#[cfg(feature = "prop_worst_price")]
#[crucible_fuzz]
fn prop_worst_price(
    fixture: &mut StdFixture,
    // A u64 range, cast to i64: crucible maps a signed range with `%`, which
    // keeps the sign of a negative raw value.
    #[range(1..1_000_000_000_000u64)] oracle_price: u64,
    #[range(0..2_000_000_000_000u64)] named_price: u64,
    #[range(0..6u64)] tier_index: u64,
    is_long: bool,
) {
    let _ = &fixture.ctx;

    let (tier, slippage_percent) = match tier_index {
        0 => (ContractTier::A, 2),
        1 => (ContractTier::B, 5),
        2 => (ContractTier::C, 5),
        3 => (ContractTier::Speculative, 10),
        4 => (ContractTier::HighlySpeculative, 20),
        _ => (ContractTier::Isolated, 20),
    };
    let oracle = OraclePriceData {
        price: oracle_price as i64,
        ..OraclePriceData::default()
    };
    let worst = derive_worst_price(&oracle, tier, dir(is_long), named_price).unwrap();

    if named_price > 0 {
        fuzz_assert_eq!(worst, named_price);
        return;
    }

    let slippage = oracle_price * slippage_percent / 100;
    let expected = if is_long {
        oracle_price + slippage
    } else {
        oracle_price - slippage
    };
    fuzz_assert_eq!(worst, expected);
}

/// Property 3 (Family IV): fillable size never exceeds the order's remaining
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
