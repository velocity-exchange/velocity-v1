//! P4 "fees-if-bankruptcy" — host-tier property harnesses over the velocity
//! insurance-fund share math, bad-debt socialization, and fee-split math
//! (campaign Families III + VII).
//!
//! Each `#[crucible_fuzz]` fn is a single stateless property; it is gated behind
//! a feature named exactly like the fn (`crucible run fees-if-bankruptcy <fn>`
//! builds `--features <fn>`), so exactly one harness `main` is generated per
//! build. The shared fixture is ungated.
//!
//! Invariant harnesses are always-on properties. Regression harnesses
//! (`regr_<pr>_*`) reproduce the invariant a PENDING audit-fix PR restores; see
//! the per-fn `// PENDING PR #<n>` comments and the report for the
//! reproduced-as-math vs deferred-to-SVM categorization.

use crucible_fuzzer::*;
#[allow(unused_imports)]
use crucible_test_context::fuzz_assert_approx_eq;

#[derive(Clone)]
struct FeesFixture {
    // Host-tier harnesses don't drive instructions, but #[fuzz_fixture] requires
    // a TestContext field for its snapshot/clone wiring. Left unused.
    ctx: TestContext,
}

#[fuzz_fixture]
impl FeesFixture {
    pub fn setup() -> Self {
        FeesFixture {
            ctx: TestContext::new(),
        }
    }

    // #[fuzz_fixture] requires at least one discovered action.
    pub fn action_noop(&mut self) {
        let _ = &self.ctx;
    }
}

// ============================================================================
// Invariant harnesses (Families III + VII)
// ============================================================================

/// Family VII (prop 1): an IF stake→unstake round-trip never mints value.
/// Minting `amount` into a pool then immediately redeeming those shares from the
/// post-deposit pool yields <= the original amount — rounding always favors the
/// protocol. (The rebase interaction is covered by
/// `inv_rebase_preserves_redeemable`; a rebase introduces ±1 dust per user while
/// keeping the aggregate exact, so a strict per-user `<=` only holds here without
/// a rebase.)
#[cfg(feature = "inv_if_share_round_trip")]
#[crucible_fuzz]
fn inv_if_share_round_trip(
    fixture: &mut FeesFixture,
    #[range(1..1_000_000_000_000u64)] amount: u64,
    #[range(0..1_000_000_000_000u64)] total_shares: u64,
    #[range(0..1_000_000_000_000u64)] vault: u64,
) {
    let _ = &fixture.ctx;
    use velocity::math::insurance::{if_shares_to_vault_amount, vault_amount_to_if_shares};

    let total_shares = total_shares as u128;

    // vault==0 requires total_shares==0 (else the fn rejects); skip invalid combos.
    let new_shares = match vault_amount_to_if_shares(amount, total_shares, vault) {
        Ok(s) => s,
        Err(_) => return,
    };

    // Pool after the deposit.
    let total2 = match total_shares.checked_add(new_shares) {
        Some(t) => t,
        None => return,
    };
    let vault2 = match vault.checked_add(amount) {
        Some(v) => v,
        None => return,
    };

    let redeemed = match if_shares_to_vault_amount(new_shares, total2, vault2) {
        Ok(r) => r,
        Err(_) => return,
    };

    // The staking cycle never returns more than was put in.
    fuzz_assert_le!(redeemed, amount);
}

/// Family III (prop 2): the funding-rate delta that socializes a perp bad-debt
/// loss exactly covers the residual within one base-unit of rounding — never
/// under-socializes (leaving debt) nor over-socializes (over-charging traders).
#[cfg(feature = "inv_socialization_funding_delta")]
#[crucible_fuzz]
fn inv_socialization_funding_delta(
    fixture: &mut FeesFixture,
    #[range(1..1_000_000_000_000u64)] loss_mag: u64,
    #[range(1..100_000_000_000_000u64)] base_long: u64,
    #[range(0..100_000_000_000_000u64)] base_short: u64,
) {
    let _ = &fixture.ctx;
    use velocity::{
        math::{
            constants::{
                AMM_RESERVE_PRECISION_I128, FUNDING_RATE_TO_QUOTE_PRECISION_PRECISION_RATIO,
            },
            liquidation::calculate_funding_rate_deltas_to_resolve_bankruptcy,
        },
        state::perp_market::PerpMarket,
    };

    let mut market = PerpMarket::default();
    market.base_asset_amount_long = base_long as i128;
    market.base_asset_amount_short = -(base_short as i128);

    let loss: i128 = -(loss_mag as i128);
    let delta = match calculate_funding_rate_deltas_to_resolve_bankruptcy(loss, &market) {
        Ok(d) => d,
        Err(_) => return,
    };

    // delta = q * RATIO where q = ceil(|loss| * AMM_RESERVE / total_base).
    let ratio = FUNDING_RATE_TO_QUOTE_PRECISION_PRECISION_RATIO as i128;
    let q = delta / ratio;
    let total_base = base_long as i128 + base_short as i128;

    let lhs = q * total_base;
    let rhs = (loss_mag as i128) * AMM_RESERVE_PRECISION_I128;

    // Covers the loss (no under-socialization).
    fuzz_assert!(lhs >= rhs);
    // Minimal (no over-socialization beyond one base-unit of ceil rounding).
    fuzz_assert!(lhs < rhs + total_base);
}

/// Family III (prop 2): the cumulative-deposit-interest haircut that socializes
/// a spot bad-debt loss covers the borrow (when not clamped) and never drives
/// `cumulative_deposit_interest` below 1 (the divide-by-CDI floor).
#[cfg(feature = "inv_socialization_deposit_interest_delta")]
#[crucible_fuzz]
fn inv_socialization_deposit_interest_delta(
    fixture: &mut FeesFixture,
    #[range(1..1_000_000_000_000u64)] borrow: u64,
    #[range(1..1_000_000_000_000u64)] deposit_balance: u64,
    #[range(0..1_000_000_000_000u64)] cdi_extra: u64,
) {
    let _ = &fixture.ctx;
    use velocity::{
        math::{
            constants::SPOT_CUMULATIVE_INTEREST_PRECISION,
            liquidation::calculate_cumulative_deposit_interest_delta_to_resolve_bankruptcy,
            spot_balance::get_token_amount,
        },
        state::spot_market::{SpotBalanceType, SpotMarket},
    };

    let cdi = SPOT_CUMULATIVE_INTEREST_PRECISION + cdi_extra as u128;

    let mut spot_market = SpotMarket::default();
    spot_market.decimals = 6;
    spot_market.deposit_balance = deposit_balance as u128;
    spot_market.cumulative_deposit_interest = cdi;

    let total_deposits = match get_token_amount(
        spot_market.deposit_balance,
        &spot_market,
        &SpotBalanceType::Deposit,
    ) {
        Ok(t) => t,
        Err(_) => return,
    };

    let delta = match calculate_cumulative_deposit_interest_delta_to_resolve_bankruptcy(
        borrow as u128,
        &spot_market,
    ) {
        Ok(d) => d,
        Err(_) => return,
    };

    if total_deposits == 0 {
        // No depositors to haircut: nothing to socialize.
        fuzz_assert_eq!(delta, 0u128);
        return;
    }

    // Never underflow the interest index (balance conversions divide by it).
    fuzz_assert!(delta < cdi);

    // When not clamped (loss below total deposits), the haircut covers the
    // borrow: delta * total_deposits >= cdi * borrow (the ceil property).
    let clamped = cdi.saturating_sub(1);
    if delta < clamped {
        let lhs = delta.saturating_mul(total_deposits);
        let rhs = cdi.saturating_mul(borrow as u128);
        fuzz_assert!(lhs >= rhs);
        // Minimal within one `total_deposits` of ceil rounding.
        fuzz_assert!(lhs < rhs + total_deposits);
    }
}

/// Family III (prop 3): `split_fee_remainder` conserves the total — the AMM, IF,
/// and protocol cuts sum back to exactly the remainder, and rounding dust
/// accrues to the protocol residual (protocol_fee is the exact residual).
#[cfg(feature = "inv_split_fee_conserves")]
#[crucible_fuzz]
fn inv_split_fee_conserves(
    fixture: &mut FeesFixture,
    #[range(0..1_000_000_000_000_000u64)] remainder: u64,
    #[range(0..101u64)] amm_num: u64,
    #[range(0..101u64)] if_num: u64,
) {
    let _ = &fixture.ctx;
    use velocity::{math::fees::split_fee_remainder, state::state::FeeStructure};

    // amm + if <= FEE_PERCENTAGE_DENOMINATOR (100) is a fee-structure-update
    // invariant; skip invalid configs so the residual can't underflow.
    if amm_num + if_num > 100 {
        return;
    }

    let mut fs = FeeStructure::default();
    fs.amm_fee_numerator = amm_num as u32;
    fs.if_fee_numerator = if_num as u32;

    let (amm_fee, if_fee, protocol_fee) = match split_fee_remainder(remainder, &fs) {
        Ok(t) => t,
        Err(_) => return,
    };

    // Conservation: no fee unit created or destroyed.
    fuzz_assert_eq!(amm_fee + if_fee + protocol_fee, remainder);
    // Each explicit cut is floored, so it never exceeds its exact share; the
    // residual (protocol) therefore never underflows and absorbs the dust.
    fuzz_assert_le!(amm_fee, remainder);
    fuzz_assert_le!(if_fee, remainder);
}

/// Family VII (prop 3): `calculate_if_shares_lost` never forfeits more than the
/// staker's own pending-request shares.
#[cfg(feature = "inv_if_shares_lost_bounded")]
#[crucible_fuzz]
fn inv_if_shares_lost_bounded(
    fixture: &mut FeesFixture,
    #[range(0..1_000_000_000_000u64)] req_shares: u64,
    #[range(0..1_000_000_000_000u64)] req_value: u64,
    #[range(1..1_000_000_000_000u64)] total_shares: u64,
    #[range(1..1_000_000_000_000u64)] vault: u64,
) {
    let _ = &fixture.ctx;
    use {
        anchor_lang::prelude::Pubkey,
        velocity::{
            math::insurance::calculate_if_shares_lost,
            state::{insurance_fund_stake::InsuranceFundStake, spot_market::SpotMarket},
        },
    };

    let req_shares = req_shares as u128;
    let total_shares = total_shares as u128;

    // if_shares_to_vault_amount rejects n_shares > total; skip invalid combos.
    if req_shares > total_shares {
        return;
    }

    let mut spot_market = SpotMarket::default();
    spot_market.insurance_fund.total_shares = total_shares;

    let mut if_stake = InsuranceFundStake::new(Pubkey::default(), 0, 0);
    if_stake.last_withdraw_request_shares = req_shares;
    if_stake.last_withdraw_request_value = req_value;

    let lost = match calculate_if_shares_lost(&if_stake, &spot_market, vault) {
        Ok(l) => l,
        Err(_) => return,
    };

    // Can never lose more than the request held.
    fuzz_assert_le!(lost, req_shares);
}

/// Family VII (prop 3): a rebase preserves each staker's redeemable amount
/// within rounding. A rebase floors both the staker's shares and total shares by
/// the same divisor, so the redeemable amount changes by at most one rebased
/// share's granularity (`vault / total_r`) — the staker's proportional claim is
/// preserved and never stranded. A no-op rebase (divisor == 1) preserves it
/// exactly.
#[cfg(feature = "inv_rebase_preserves_redeemable")]
#[crucible_fuzz]
fn inv_rebase_preserves_redeemable(
    fixture: &mut FeesFixture,
    #[range(1..1_000_000_000_000u64)] total_shares: u64,
    #[range(1..1_000_000_000u64)] vault: u64,
    #[range(1..1_000_000_000_000u64)] user_shares: u64,
) {
    let _ = &fixture.ctx;
    use velocity::math::insurance::{calculate_rebase_info, if_shares_to_vault_amount};

    let total_shares = total_shares as u128;
    let user_shares = user_shares as u128;
    if user_shares > total_shares {
        return;
    }

    let before = match if_shares_to_vault_amount(user_shares, total_shares, vault) {
        Ok(b) => b,
        Err(_) => return,
    };

    let (_, divisor) = match calculate_rebase_info(total_shares, vault) {
        Ok(r) => r,
        Err(_) => return,
    };

    let user_r = user_shares / divisor;
    let total_r = total_shares / divisor;
    if total_r == 0 {
        return;
    }

    let after = match if_shares_to_vault_amount(user_r, total_r, vault) {
        Ok(a) => a,
        Err(_) => return,
    };

    if divisor == 1 {
        // A no-op rebase preserves the redeemable amount exactly.
        fuzz_assert_eq!(after, before);
    } else {
        // Preserved within one rebased-share's granularity: the floor on both
        // numerator (user shares) and denominator (total shares) perturbs the
        // ratio by < 1/total_r, i.e. the redeemable amount by < vault/total_r
        // (plus the two independent floor(<1) roundings of the redemptions).
        let tol = (vault / (total_r as u64)) + 4;
        fuzz_assert_approx_eq!(after, before, tol);
    }
}

/// Family VII (prop 4): the perp fee tier is monotone (total-ordered) in 30d
/// volume — more volume never yields a strictly higher taker fee rate.
#[cfg(feature = "inv_fee_tier_monotone")]
#[crucible_fuzz]
fn inv_fee_tier_monotone(
    fixture: &mut FeesFixture,
    #[range(0..2_000_000_000_000u64)] vol_a: u64,
    #[range(0..2_000_000_000_000u64)] vol_b: u64,
) {
    let _ = &fixture.ctx;
    use velocity::{
        math::fees::determine_user_fee_tier,
        state::{
            state::FeeStructure,
            user::{MarketType, UserStats},
        },
    };

    let (lo, hi) = if vol_a <= vol_b {
        (vol_a, vol_b)
    } else {
        (vol_b, vol_a)
    };

    let fs = FeeStructure::default();

    let mut stats_lo = UserStats::default();
    stats_lo.taker_volume_30d = lo;
    let mut stats_hi = UserStats::default();
    stats_hi.taker_volume_30d = hi;

    let tier_lo = match determine_user_fee_tier(&stats_lo, &fs, &MarketType::Perp) {
        Ok(t) => t,
        Err(_) => return,
    };
    let tier_hi = match determine_user_fee_tier(&stats_hi, &fs, &MarketType::Perp) {
        Ok(t) => t,
        Err(_) => return,
    };

    // Higher volume => fee rate is non-increasing. Compare rates by
    // cross-multiplying to be denominator-agnostic:
    // hi.num/hi.den <= lo.num/lo.den.
    let lhs = (tier_hi.fee_numerator as u128) * (tier_lo.fee_denominator as u128);
    let rhs = (tier_lo.fee_numerator as u128) * (tier_hi.fee_denominator as u128);
    fuzz_assert!(lhs <= rhs);
}

// ============================================================================
// Regression harnesses (PENDING audit-fix PRs)
// ============================================================================

// PENDING PR #253: IF add must reject a positive deposit that mints zero shares
// (IFDepositMintsZeroShares). The exploitable *math* condition is that
// `vault_amount_to_if_shares` returns 0 for a positive `amount` when the vault
// has been donation-inflated (amount * total_shares < vault). This harness
// asserts the FIXED invariant — a positive deposit mints > 0 shares — so it
// FAILS on master (the raw math floors to zero). NOTE: the guard itself lives in
// the `add_insurance_fund_stake` controller; full reject-path reproduction is
// SVM (P8). Categorized in the report as reproduced-as-math-condition.
#[cfg(feature = "regr_253_if_add_zero_shares")]
#[crucible_fuzz]
fn regr_253_if_add_zero_shares(
    fixture: &mut FeesFixture,
    #[range(1..1_000_000_000_000u64)] amount: u64,
    #[range(1..1_000_000_000_000u64)] total_shares: u64,
    #[range(1..1_000_000_000_000_000u64)] vault: u64,
) {
    let _ = &fixture.ctx;
    use velocity::math::insurance::vault_amount_to_if_shares;

    let shares = match vault_amount_to_if_shares(amount, total_shares as u128, vault) {
        Ok(s) => s,
        // A rejected deposit satisfies the fixed invariant ("mints >0 OR rejects").
        Err(_) => return,
    };

    // FIXED invariant: a positive deposit never mints zero shares.
    fuzz_assert!(shares > 0);
}

// PENDING PR #254: the revenue-settle APR cap must be sized off
// `min(live_if_vault, if_last_settle_vault_amount)` so a pre-settle SPL donation
// (which lifts only the live balance) can't inflate the cap.
//
// DEFERRED TO SVM (P8): #254 revenue-settle APR cap — needs
// `settle_revenue_to_insurance_fund` and the `if_last_settle_vault_amount` field
// added by the PR (absent on master). There is no master host fn to exercise, so
// there is no honest pure-math harness here (a model of the cap would only test
// Rust's `min`, not the program). Full reproduction belongs to the SVM tier.

// PENDING PR #266 (F9): after a market rebase floors a small
// `last_withdraw_request_shares` to 0, `calculate_if_shares_lost` returns 0 so
// the cancel path can clear the request and return the intact stake (stake not
// stranded). This is a math sub-property the controller fix relies on: a zeroed
// request forfeits nothing. It PASSES on master (the math is unchanged) —
// confirming the fix precondition. The bug itself is the controller's post-rebase
// `!= 0` re-check; full reproduction (cancel/remove/re-request lockout) is SVM
// (P7). Categorized in the report as fix-precondition (deferred-to-SVM).
#[cfg(feature = "regr_266_cancel_after_rebase")]
#[crucible_fuzz]
fn regr_266_cancel_after_rebase(
    fixture: &mut FeesFixture,
    #[range(1..1_000_000_000_000u64)] total_shares: u64,
    #[range(1..1_000_000_000u64)] vault: u64,
    #[range(0..1_000_000u64)] req_value: u64,
) {
    let _ = &fixture.ctx;
    use {
        anchor_lang::prelude::Pubkey,
        velocity::{
            math::insurance::{calculate_if_shares_lost, calculate_rebase_info},
            state::{insurance_fund_stake::InsuranceFundStake, spot_market::SpotMarket},
        },
    };

    let total_shares = total_shares as u128;

    let (_, divisor) = match calculate_rebase_info(total_shares, vault) {
        Ok(r) => r,
        Err(_) => return,
    };
    // Only meaningful when a rebase actually floors a small request to zero.
    if divisor <= 1 {
        return;
    }

    // A tiny request that floors to 0 under the rebase divisor.
    let small_request: u128 = divisor - 1;
    let req_shares_rebased = small_request / divisor; // == 0
    if req_shares_rebased != 0 {
        return;
    }

    let mut spot_market = SpotMarket::default();
    spot_market.insurance_fund.total_shares = total_shares / divisor;

    let mut if_stake = InsuranceFundStake::new(Pubkey::default(), 0, 0);
    if_stake.last_withdraw_request_shares = req_shares_rebased; // 0 after rebase
    if_stake.last_withdraw_request_value = req_value as u64;

    let lost = match calculate_if_shares_lost(&if_stake, &spot_market, vault) {
        Ok(l) => l,
        Err(_) => return,
    };

    // FIXED-invariant precondition: a rebase-zeroed request forfeits nothing, so
    // the cancel path can clear it and restore the intact stake.
    fuzz_assert_eq!(lost, 0u128);
}

// PENDING PR #255 (High): every permissionless fee/pool drain must reserve
// `min(pending_if_fee, get_bankruptcy_if_floor())` (the PR's new
// `PerpMarket::get_bankruptcy_if_tranche_reservation`) so a protocol-fee sweep
// can't unback the floored first-loss IF bankruptcy tranche. The reservation
// helper does not exist on master, but `get_bankruptcy_if_floor` — the value it
// caps against — does. This harness exercises genuine properties of the REAL
// `get_bankruptcy_if_floor()`: it is 0 only at the disable sentinel (a stored 0
// selects the default pct), monotone non-decreasing in the effective floor pct,
// and never exceeds the open-interest notional (equalling it exactly at
// pct == PERCENTAGE_PRECISION). NOTE: the full stateful
// sweep-vs-resolve reservation invariant (drain the pool below the reservation,
// then observe under-backed PnL at `resolve_perp_bankruptcy`) is SVM (P8).
#[cfg(feature = "regr_255_floored_if_tranche")]
#[crucible_fuzz]
fn regr_255_floored_if_tranche(
    fixture: &mut FeesFixture,
    #[range(0..100_000_000_000_000u64)] base_long: u64,
    #[range(0..100_000_000_000_000u64)] base_short: u64,
    #[range(0..1_000_000_000_000u64)] twap: u64,
    #[range(0..1_000_001u64)] pct_lo: u64,
    #[range(0..1_000_001u64)] pct_hi: u64,
) {
    let _ = &fixture.ctx;
    use velocity::{
        math::constants::{
            BANKRUPTCY_IF_FLOOR_DISABLED, BASE_PRECISION, DEFAULT_BANKRUPTCY_IF_FLOOR_PCT,
            PERCENTAGE_PRECISION,
        },
        state::perp_market::PerpMarket,
    };

    // A stored 0 selects the default pct, so order by the EFFECTIVE pct.
    let effective = |pct: u64| -> u64 {
        if pct == 0 {
            DEFAULT_BANKRUPTCY_IF_FLOOR_PCT as u64
        } else {
            pct
        }
    };
    let (pct_lo, pct_hi) = if effective(pct_lo) <= effective(pct_hi) {
        (pct_lo, pct_hi)
    } else {
        (pct_hi, pct_lo)
    };

    let mut market = PerpMarket::default();
    market.base_asset_amount_long = base_long as i128;
    market.base_asset_amount_short = -(base_short as i128);
    market
        .market_stats
        .historical_oracle_data
        .last_oracle_price_twap = twap as i64;

    // REAL fn: floor at the lower pct.
    market.bankruptcy_if_floor_pct = pct_lo as u32;
    let floor_lo = match market.get_bankruptcy_if_floor() {
        Ok(f) => f,
        Err(_) => return,
    };
    // REAL fn: floor at the higher pct.
    market.bankruptcy_if_floor_pct = pct_hi as u32;
    let floor_hi = match market.get_bankruptcy_if_floor() {
        Ok(f) => f,
        Err(_) => return,
    };

    // The open-interest notional at the twap — the reservation's natural ceiling.
    let oi = market.get_open_interest(); // REAL fn
    let notional = oi
        .saturating_mul(twap as u128)
        .saturating_div(BASE_PRECISION);

    // Only the sentinel removes the floor. A stored 0 is what every market
    // written before the field existed holds, so it must still reserve.
    market.bankruptcy_if_floor_pct = BANKRUPTCY_IF_FLOOR_DISABLED;
    let floor_disabled = match market.get_bankruptcy_if_floor() {
        Ok(f) => f,
        Err(_) => return,
    };
    fuzz_assert_eq!(floor_disabled, 0u128);
    // Monotone non-decreasing in the effective floor pct.
    fuzz_assert_le!(floor_lo, floor_hi);
    // Never reserves more than the full OI notional.
    fuzz_assert_le!(floor_hi, notional);
    // At 100% (PERCENTAGE_PRECISION) the floor is exactly the OI notional.
    if pct_hi as u128 == PERCENTAGE_PRECISION {
        fuzz_assert_eq!(floor_hi, notional);
    }
}

// Monotonicity property backing PR #273 (F8), NOT a reproduction of it.
// #273's actual bug is interest-refresh *ordering* in the `resolve_spot_bankruptcy`
// controller (it reads `get_token_amount` before refreshing interest, clearing the
// borrow at a stale-low index). That ordering is stateful and lives in the
// controller, so it is reproduced at the SVM tier (`e2e-svm-revshare`/`e2e-svm-liq`)
// and covered by the program's own tests; it CANNOT be caught here. This host
// harness only asserts the underlying math fact the fix relies on: valuing the same
// borrow at a fresher (larger) cumulative index never yields a smaller debt. That is
// a monotonicity of `get_token_amount` in the index and is always true, so it will
// never fail; it documents/pins the sub-property, it does not detect the bug. Named
// `prop_` (not `regr_`) so it is not mistaken for a bug regression.
#[cfg(feature = "prop_borrow_debt_monotonic_in_index")]
#[crucible_fuzz]
fn prop_borrow_debt_monotonic_in_index(
    fixture: &mut FeesFixture,
    #[range(1..1_000_000_000_000u64)] borrow_balance: u64,
    #[range(0..1_000_000_000_000u64)] interest_accrued: u64,
) {
    let _ = &fixture.ctx;
    use velocity::{
        math::{constants::SPOT_CUMULATIVE_INTEREST_PRECISION, spot_balance::get_token_amount},
        state::spot_market::{SpotBalanceType, SpotMarket},
    };

    let stale_index = SPOT_CUMULATIVE_INTEREST_PRECISION;
    let fresh_index = stale_index + interest_accrued as u128;

    let mut stale = SpotMarket::default();
    stale.decimals = 6;
    stale.cumulative_borrow_interest = stale_index;
    let mut fresh = SpotMarket::default();
    fresh.decimals = 6;
    fresh.cumulative_borrow_interest = fresh_index;

    let bal = borrow_balance as u128;
    let debt_stale = match get_token_amount(bal, &stale, &SpotBalanceType::Borrow) {
        Ok(d) => d,
        Err(_) => return,
    };
    let debt_fresh = match get_token_amount(bal, &fresh, &SpotBalanceType::Borrow) {
        Ok(d) => d,
        Err(_) => return,
    };

    // FIXED invariant: bankruptcy must clear the debt at the FRESH index, which
    // is >= the stale-index debt. Clearing at the stale index under-socializes
    // and forgives accrued interest whenever interest has accrued.
    fuzz_assert!(debt_fresh >= debt_stale);
}

// Monotonicity property backing PR #252, NOT a reproduction of it. #252's actual
// bug is settle-before-freeze *ordering* in `request_remove_insurance_fund_stake`
// (plus an accounts-struct ABI change), which is stateful controller logic
// reproduced at the SVM tier and covered by the program's own tests; it CANNOT be
// caught here. This host harness only asserts the math fact the fix relies on:
// freezing the exit value against a larger (post-settle) vault never shortchanges
// the staker vs a smaller (pre-settle) vault. That is monotonicity of
// `if_shares_to_vault_amount` in the vault balance and is always true, so it will
// never fail; it pins the sub-property, it does not detect the bug. Named `prop_`
// (not `regr_`) so it is not mistaken for a bug regression.
#[cfg(feature = "prop_if_exit_value_monotonic_in_vault")]
#[crucible_fuzz]
fn prop_if_exit_value_monotonic_in_vault(
    fixture: &mut FeesFixture,
    #[range(1..1_000_000_000_000u64)] shares: u64,
    #[range(1..1_000_000_000_000u64)] total_shares: u64,
    #[range(1..1_000_000_000_000u64)] vault_pre: u64,
    #[range(0..1_000_000_000_000u64)] due_revenue: u64,
) {
    let _ = &fixture.ctx;
    use velocity::math::insurance::if_shares_to_vault_amount;

    let shares = shares as u128;
    let total_shares = total_shares as u128;
    if shares > total_shares {
        return;
    }

    // Exit value frozen against the pre-settle vault (master bug) vs the
    // post-settle vault (the fix settles already-due revenue in first).
    let value_pre = match if_shares_to_vault_amount(shares, total_shares, vault_pre) {
        Ok(v) => v,
        Err(_) => return,
    };
    let vault_post = match vault_pre.checked_add(due_revenue) {
        Some(v) => v,
        None => return,
    };
    let value_post = match if_shares_to_vault_amount(shares, total_shares, vault_post) {
        Ok(v) => v,
        Err(_) => return,
    };

    // FIXED invariant: settling due revenue before freezing the exit value never
    // shortchanges the exiting staker — their frozen value is >= the value the
    // master path (pre-settle vault) would freeze.
    fuzz_assert!(value_post >= value_pre);
}
