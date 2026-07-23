//! P5 "spot" host-tier property harnesses — Family II of the Crucible campaign
//! (spot accounting <-> token reconciliation). These fuzz the pure math in
//! `velocity::math::{spot_balance,spot_swap,spot_withdraw}` directly, no LiteSVM.
//!
//! Each `#[crucible_fuzz]` fn is auto-gated by the macro under `#[cfg(feature =
//! "<fn_name>")]` (and generates its own `main`), so exactly one test compiles
//! per `crucible run spot <test>` (= `--features <test>`). A single shared
//! fixture is used by all of them.
//!
//! Because the math is `SafeMath`-based, arithmetic that would overflow returns
//! `Err` (an *expected reject*, not an invariant violation) — every harness
//! therefore skips on `Err` via `let Ok(..) = .. else { return }` and only
//! asserts the invariant on successfully-computed values.

// Only one test feature is active per build, so items used by the other tests
// (imports, the local reference helpers) are legitimately dead in that build.
#![allow(unused_imports, dead_code, unused_variables)]

use crucible_fuzzer::*;

use velocity::math::constants::{
    PERCENTAGE_PRECISION, SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_UTILIZATION_PRECISION,
};
use velocity::math::spot_balance::{
    calculate_accumulated_interest, calculate_borrow_rate, calculate_utilization,
    get_signed_token_amount, get_spot_balance, get_token_amount,
};
use velocity::math::spot_swap::calculate_swap_price;
use velocity::math::spot_withdraw::{
    calculate_max_borrow_token_amount, calculate_min_deposit_token_amount, check_withdraw_limits,
};
use velocity::state::spot_market::{SpotBalanceType, SpotMarket};

#[derive(Clone)]
struct SpotFixture {
    // Host-tier harnesses don't drive instructions, but #[fuzz_fixture] requires
    // a TestContext field for its snapshot/clone wiring. Left unused.
    ctx: TestContext,
}

#[fuzz_fixture]
impl SpotFixture {
    pub fn setup() -> Self {
        SpotFixture {
            ctx: TestContext::new(),
        }
    }

    // #[fuzz_fixture] requires at least one discovered action.
    pub fn action_noop(&mut self) {
        let _ = &self.ctx;
    }
}

/// Build a spot market with valid (non-zero) cumulative interest indices so the
/// `get_token_amount` / `get_spot_balance` divisions never trip a div-by-zero.
fn market_with_interest(decimals: u32, cum: u128) -> SpotMarket {
    SpotMarket {
        decimals,
        cumulative_deposit_interest: cum,
        cumulative_borrow_interest: cum,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Property 1 — scaled-balance <-> token round-trip favors the protocol.
// Deposits round DOWN, borrows round UP; converting scaled -> token -> scaled
// never gains value for a depositor and never shrinks a borrower's debt.
// ---------------------------------------------------------------------------
#[cfg(feature = "prop_scaled_token_roundtrip")]
#[crucible_fuzz]
fn prop_scaled_token_roundtrip(
    fixture: &mut SpotFixture,
    #[range(0..1_000_000_000_000_000u64)] balance: u64,
    // cumulative interest index >= SPOT_CUMULATIVE_INTEREST_PRECISION (1e10).
    #[range(10_000_000_000..1_000_000_000_000u64)] cum: u64,
    #[range(0..13u64)] decimals: u64,
) {
    let _ = &fixture.ctx;
    let balance = balance as u128;
    let market = market_with_interest(decimals as u32, cum as u128);

    let (Ok(token_deposit), Ok(token_borrow)) = (
        get_token_amount(balance, &market, &SpotBalanceType::Deposit),
        get_token_amount(balance, &market, &SpotBalanceType::Borrow),
    ) else {
        return;
    };

    // Same scaled balance: borrow (ceil) is never valued below deposit (floor).
    fuzz_assert_le!(token_deposit, token_borrow);

    // Deposit round-trip: scaled -> token(floor) -> scaled(floor) never gains.
    if let Ok(back) = get_spot_balance(token_deposit, &market, &SpotBalanceType::Deposit, false) {
        fuzz_assert_le!(back, balance);
    }

    // Borrow round-trip: scaled -> token(ceil) -> scaled(round_up) never shrinks
    // the debt (protocol-favoring).
    if let Ok(back) = get_spot_balance(token_borrow, &market, &SpotBalanceType::Borrow, true) {
        fuzz_assert_ge!(back, balance);
    }
}

// ---------------------------------------------------------------------------
// Property 2 — interest indices: borrow rate monotone non-decreasing in
// utilization and >= min rate; implied deposit rate <= borrow rate; and
// accumulated interest is monotone non-decreasing in time with the borrow
// accrual >= the deposit accrual (borrowers pay >= depositors earn).
// (`calculate_deposit_rate` is `#[cfg(feature="velocity-rs")]`-gated and not
// present in this build, so the deposit-side rate is reconstructed from the
// same formula the accrual uses: borrow_rate * utilization / UTIL_PRECISION.)
// ---------------------------------------------------------------------------
#[cfg(feature = "prop_interest_monotone")]
#[crucible_fuzz]
fn prop_interest_monotone(
    fixture: &mut SpotFixture,
    #[range(1..1_000_001u64)] opt_util: u64,   // (0, PERCENTAGE_PRECISION]
    #[range(0..1_000_001u64)] opt_rate: u64,   // <= max via extra below
    #[range(0..5_000_001u64)] max_extra: u64,  // max_rate = opt_rate + extra
    #[range(0..256u64)] min_rate: u64,         // SpotMarket.min_borrow_rate: u8
    #[range(0..1_000_001u64)] util_a: u64,
    #[range(0..1_000_001u64)] util_b: u64,
    #[range(0..1_000_001u64)] borrow_frac: u64,
    #[range(1..1_000_001u64)] t1: u64,
    #[range(0..1_000_001u64)] dt: u64,
) {
    let _ = &fixture.ctx;

    let rate_market = SpotMarket {
        optimal_utilization: opt_util as u32,
        optimal_borrow_rate: opt_rate as u32,
        max_borrow_rate: (opt_rate + max_extra) as u32,
        min_borrow_rate: min_rate as u8,
        ..Default::default()
    };

    let lo = (util_a.min(util_b)) as u128;
    let hi = (util_a.max(util_b)) as u128;

    if let (Ok(rate_lo), Ok(rate_hi)) = (
        calculate_borrow_rate(&rate_market, lo),
        calculate_borrow_rate(&rate_market, hi),
    ) {
        // Monotone non-decreasing in utilization.
        fuzz_assert_le!(rate_lo, rate_hi);

        // Implied deposit rate never exceeds the borrow rate.
        let deposit_rate_lo = rate_lo.saturating_mul(lo) / SPOT_UTILIZATION_PRECISION;
        fuzz_assert_le!(deposit_rate_lo, rate_lo);

        // Borrow rate respects the configured floor.
        if let Ok(min) = rate_market.get_min_borrow_rate() {
            fuzz_assert_ge!(rate_lo, min as u128);
        }
    }

    // Time monotonicity of accrued interest. Build a market with utilization > 0
    // (deposit_balance >= borrow_balance keeps utilization <= 100%).
    let cum = SPOT_CUMULATIVE_INTEREST_PRECISION;
    let deposit_balance = 1_000_000_000_000_000u128;
    let borrow_balance = deposit_balance.saturating_mul(borrow_frac as u128) / SPOT_UTILIZATION_PRECISION;
    let accrual_market = SpotMarket {
        decimals: 6,
        cumulative_deposit_interest: cum,
        cumulative_borrow_interest: cum,
        optimal_utilization: opt_util as u32,
        optimal_borrow_rate: opt_rate as u32,
        max_borrow_rate: (opt_rate + max_extra) as u32,
        min_borrow_rate: min_rate as u8,
        deposit_balance,
        borrow_balance,
        last_interest_ts: 0,
        ..Default::default()
    };

    let now1 = t1 as i64;
    let now2 = (t1 + dt) as i64;
    if let (Ok(i1), Ok(i2)) = (
        calculate_accumulated_interest(&accrual_market, now1),
        calculate_accumulated_interest(&accrual_market, now2),
    ) {
        // Accrued interest is non-decreasing in elapsed time.
        fuzz_assert_le!(i1.borrow_interest, i2.borrow_interest);
        fuzz_assert_le!(i1.deposit_interest, i2.deposit_interest);

        // Borrowers pay >= depositors earn, provided utilization <= 100% (else
        // the deposit-rate scaling factor util/UTIL exceeds 1 by construction).
        if let Ok(util) =
            velocity::math::spot_balance::calculate_spot_market_utilization(&accrual_market)
        {
            if util <= SPOT_UTILIZATION_PRECISION {
                fuzz_assert_ge!(i2.borrow_interest, i2.deposit_interest);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Property 3 — deposits >= borrows implies utilization <= cap; token amounts
// are non-negative in the signed representation (deposit >= 0, borrow <= 0).
// ---------------------------------------------------------------------------
#[cfg(feature = "prop_deposits_ge_borrows")]
#[crucible_fuzz]
fn prop_deposits_ge_borrows(
    fixture: &mut SpotFixture,
    #[range(0..1_000_000_000_000_000_000u64)] deposit: u64,
    #[range(0..1_000_000_000_000_000_000u64)] borrow: u64,
) {
    let _ = &fixture.ctx;
    let d = deposit as u128;
    let b = borrow as u128;

    if let Ok(util) = calculate_utilization(d, b) {
        // Deposits >= borrows in token terms => utilization within the cap.
        if b <= d {
            fuzz_assert_le!(util, SPOT_UTILIZATION_PRECISION);
        }
        // No borrows => zero utilization.
        if b == 0 {
            fuzz_assert_eq!(util, 0u128);
        }
    }

    // Signed token amounts: deposits are assets (>= 0), borrows liabilities (<= 0).
    if let Ok(signed_deposit) = get_signed_token_amount(d, &SpotBalanceType::Deposit) {
        fuzz_assert_ge!(signed_deposit, 0i128);
    }
    if let Ok(signed_borrow) = get_signed_token_amount(b, &SpotBalanceType::Borrow) {
        fuzz_assert_le!(signed_borrow, 0i128);
    }
}

// ---------------------------------------------------------------------------
// Property 4 — spot-swap price sanity + withdraw guard-rail math. The swap
// price is monotone (more `in` for the same `out` never raises the per-unit
// price), and the withdraw-limit helpers never permit exceeding the guard
// rails: min deposit after withdraw <= 75% of TWAP (pre-#185 hardcoded 25%
// breaker), max borrow <= the absolute borrow cap. `check_withdraw_limits`
// is exercised for totality (never panics) to get edge coverage.
// ---------------------------------------------------------------------------
#[cfg(feature = "prop_swap_and_withdraw_limits")]
#[crucible_fuzz]
fn prop_swap_and_withdraw_limits(
    fixture: &mut SpotFixture,
    #[range(1..1_000_000_000_000_000u64)] amount_in: u64,
    #[range(1..1_000_000_000_000_000u64)] amount_out: u64,
    #[range(0..13u64)] in_decimals: u64,
    #[range(0..13u64)] out_decimals: u64,
    #[range(0..1_000_000_000_000_000u64)] deposit_amount: u64,
    #[range(0..1_000_000_000_000_000u64)] deposit_twap: u64,
    #[range(0..1_000_000_000_000_000u64)] borrow_twap: u64,
    #[range(0..1_000_000_000_000_000u64)] guard: u64,
    #[range(0..1_000_000_000_000_000u64)] max_borrows: u64,
) {
    let _ = &fixture.ctx;

    // Swap price monotonicity: doubling amount_in never raises the price.
    if let Ok(price1) = calculate_swap_price(
        amount_out as u128,
        amount_in as u128,
        out_decimals as u32,
        in_decimals as u32,
    ) {
        if let Ok(price2) = calculate_swap_price(
            amount_out as u128,
            (amount_in as u128) * 2,
            out_decimals as u32,
            in_decimals as u32,
        ) {
            fuzz_assert_le!(price2, price1);
        }
    }

    let twap = deposit_twap as u128;
    let g = guard as u128;

    // Min deposit after withdraw: never requires more than the TWAP, and the
    // pre-#185 breaker always reserves at least 25% (subtract >= twap/4).
    if let Ok(min_deposit) = calculate_min_deposit_token_amount(twap, g) {
        fuzz_assert_le!(min_deposit, twap);
        fuzz_assert_le!(min_deposit, twap - twap / 4);
    }

    // Max borrow never exceeds the absolute borrow cap.
    let cap = max_borrows as u128;
    if let Ok(max_borrow) = calculate_max_borrow_token_amount(
        deposit_amount as u128,
        twap,
        borrow_twap as u128,
        g,
        cap,
        0, // pool_id 0 (main pool)
    ) {
        fuzz_assert_le!(max_borrow, cap);
    }

    // Totality (no-panic) check, deliberately result-discarding: over the whole
    // fuzzed input space `check_withdraw_limits` must never panic or hit an
    // unreachable; a panic is a crash the fuzzer reports. The Ok/Err verdict is
    // intentionally NOT asserted: there is no independent allow/deny oracle at
    // this tier (asserting `is_ok()` would false-positive on the legitimate
    // SafeMath overflow-rejection the fuzzer reaches at extreme balances). The
    // allow/deny logic itself is exercised end-to-end at the SVM tier.
    let market = SpotMarket {
        decimals: 6,
        cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
        cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
        deposit_balance: (deposit_amount as u128).saturating_mul(1_000_000),
        borrow_balance: (borrow_twap as u128).saturating_mul(1_000_000),
        deposit_token_twap: deposit_twap,
        borrow_token_twap: borrow_twap,
        withdraw_guard_threshold: guard,
        pool_id: 0,
        ..Default::default()
    };
    let _ = check_withdraw_limits(&market, None, None);
}

// ---------------------------------------------------------------------------
// REGRESSION — PENDING PR #185.
//
// #185 introduces a configurable withdraw circuit breaker and a daily deposit
// cap. On the current `master` branch NEITHER the new fields
// (`withdraw_circuit_breaker_pct`, `deposit_guard_threshold`,
// `max_deposit_pct_per_day`) NOR the new fns (`calculate_withdraw_limit`,
// `calculate_max_deposit_token_amount`, `check_deposit_limits`) NOR the new
// error (`DailyDepositLimit` 6357) exist — `SpotMarket::SIZE` is still 808, not
// the post-#185 824. These harnesses therefore build against the pre-existing
// math and reference implementations of the fixed formulas; see each header for
// its master-branch reproduction status and its un-gate plan.
// ---------------------------------------------------------------------------

/// PENDING PR #185: withdrawal bounded by `withdraw_circuit_breaker_pct` × 24h
/// deposit TWAP, with `0 ⇒ 25%` fallback.
///
/// Master hardcodes `deposit_token_twap / 4` (25%) inside
/// `calculate_min_deposit_token_amount` and has no `withdraw_circuit_breaker_pct`
/// field. This harness reimplements the FIXED configurable formula and asserts
/// the pre-#185 fn honors it: they agree only at the 25% fallback point, so for
/// any other `breaker_pct` the two DIVERGE and a violation is reported — i.e.
/// the harness reproduces the "breaker is not configurable" bug on master.
///
/// Layout dependency: un-gate to feed the pct through the real
/// `calculate_withdraw_limit(withdraw_circuit_breaker_pct)` once #185 lands
/// (SpotMarket::SIZE 808 → 824).
#[cfg(feature = "regr_185_withdraw_circuit_breaker")]
#[crucible_fuzz]
fn regr_185_withdraw_circuit_breaker(
    fixture: &mut SpotFixture,
    #[range(0..1_000_000_000_000_000u64)] deposit_twap: u64,
    #[range(0..1_000_000_000_000_000u64)] guard: u64,
    // PERCENTAGE_PRECISION-scaled breaker; 0 => 25% fallback.
    #[range(0..1_000_001u64)] breaker_pct: u64,
) {
    let _ = &fixture.ctx;
    let twap = deposit_twap as u128;
    let g = guard as u128;

    // FIXED (#185) reference: min deposit after withdraw
    //   = twap - max(twap * pct / PERCENTAGE_PRECISION, min(guard, twap))
    let pct = if breaker_pct == 0 {
        PERCENTAGE_PRECISION / 4
    } else {
        breaker_pct as u128
    };
    let breaker_amount = twap.saturating_mul(pct) / PERCENTAGE_PRECISION;
    let fixed_min_deposit = twap.saturating_sub(breaker_amount.max(g.min(twap)));

    if let Ok(master_min_deposit) = calculate_min_deposit_token_amount(twap, g) {
        // Master ignores `pct` (always 25%); the fix honors it. Equal only when
        // the effective breaker is 25% — otherwise this fires, reproducing #185.
        fuzz_assert_eq!(master_min_deposit, fixed_min_deposit);
    }
}

// DEFERRED: #185 daily deposit cap — no host surface on master; un-gate as a
// real harness calling check_deposit_limits when #185 lands (SpotMarket SIZE
// 808->824). The deposit-cap surface (`deposit_guard_threshold` /
// `max_deposit_pct_per_day` fields, `calculate_max_deposit_token_amount` /
// `check_deposit_limits` fns, `DailyDepositLimit` 6357) does not exist on
// current master, so there is no real velocity function to exercise or assert
// on today. Rather than ship a tautology over harness-reimplemented arithmetic,
// this regression is deferred until #185 introduces the surface.
