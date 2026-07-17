//! Package P1 — `margin-liq`: host-tier property/regression fuzzing of the
//! velocity margin & liquidation math (`velocity::math::margin`,
//! `velocity::math::liquidation`).
//!
//! Every harness runs against the velocity crate as a *host library* — pure
//! functions are called directly, and the fixtures for the full margin
//! calculation are built with `velocity::test_utils` account-info builders
//! (`create_anchor_account_info!`, `get_pyth_price`) exactly as the in-tree
//! unit tests do. No LiteSVM / `.so` is needed, so these are blackbox property
//! fuzzers over the `#[range]` input domains (Crucible shows `edges 0/0` for
//! host-tier harnesses — expected).
//!
//! Two sets:
//!  * **Invariant harnesses** (Family V of the campaign): margin totality,
//!    initial >= maintenance, meets-initial => meets-maintenance, liquidation
//!    sizing on healthy accounts, liquidation-fee bounds, asset/liability
//!    weight bounds + collateral monotonicity.
//!  * **Regression harnesses** (`regr_*`): encode the invariant a PENDING
//!    audit-fix PR restores, so they violate on current (pre-fix) master. The
//!    authoritative reproduction of the controller-level guards lives at the
//!    SVM tier (P8); see the per-fn notes.

use crucible_fuzzer::*;

use anchor_lang::prelude::Pubkey;

use velocity::create_anchor_account_info;
use velocity::math::constants::{
    AMM_RESERVE_PRECISION, BASE_PRECISION_U64, LIQUIDATION_FEE_PRECISION, MARGIN_PRECISION,
    MAX_POSITIVE_UPNL_FOR_INITIAL_MARGIN, PEG_PRECISION, PRICE_PRECISION_I64, QUOTE_PRECISION,
    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION, SPOT_WEIGHT_PRECISION_U128,
};
use velocity::math::liquidation::{
    calculate_asset_transfer_for_liability_transfer,
    calculate_base_asset_amount_to_cover_margin_shortage, calculate_liquidation_multiplier,
    calculate_perp_if_fee, calculate_spot_if_fee, get_liquidation_fee, LiquidationMultiplierType,
};
use velocity::math::margin::{
    calculate_margin_requirement_and_total_collateral_and_liability_info,
    calculate_perp_position_value_and_pnl, calculate_size_discount_asset_weight,
    calculate_size_premium_liability_weight, meets_initial_margin_requirement,
    meets_maintenance_margin_requirement, MarginRequirementType,
};
use velocity::state::margin_calculation::MarginContext;
use velocity::state::market_status::MarketStatus;
use velocity::state::oracle::{HistoricalOracleData, OraclePriceData, OracleSource, StrictOraclePrice};
use velocity::state::oracle_map::OracleMap;
use velocity::state::perp_market::{PerpMarket, AMM};
use velocity::state::perp_market_map::PerpMarketMap;
use velocity::state::pyth_lazer_oracle::PythLazerOracle;
use velocity::state::spot_market::{SpotBalanceType, SpotMarket};
use velocity::state::spot_market_map::SpotMarketMap;
use velocity::state::user::{PerpPosition, SpotPosition, User};
use velocity::test_utils::get_pyth_price;

// ---------------------------------------------------------------------------
// Fixture (host-tier: the ctx is unused, but #[fuzz_fixture] requires a
// TestContext field, a `setup`, and at least one `action_*`).
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MarginFixture {
    ctx: TestContext,
}

#[fuzz_fixture]
impl MarginFixture {
    pub fn setup() -> Self {
        MarginFixture {
            ctx: TestContext::new(),
        }
    }

    pub fn action_noop(&mut self) {
        let _ = &self.ctx;
    }
}

// ---------------------------------------------------------------------------
// Fixture builders — mirror programs/velocity/src/math/margin/tests.rs.
// ---------------------------------------------------------------------------

/// A single perp market backed by a PythLazer oracle + a USDC (QuoteAsset)
/// spot market. Constructs `perp_map`, `spot_map`, and a mutable `oracle_map`
/// in the caller's scope. `$price` is in *dollars* (fed to get_pyth_price with
/// expo 6), `$mr_init`/`$mr_maint` are MARGIN_PRECISION-based margin ratios.
macro_rules! build_maps {
    ($price:expr, $mr_init:expr, $mr_maint:expr, $perp_map:ident, $spot_map:ident, $oracle_map:ident) => {
        let oracle_key = Pubkey::new_from_array([7u8; 32]);
        let mut oracle_acct = get_pyth_price($price, 6);
        create_anchor_account_info!(oracle_acct, &oracle_key, PythLazerOracle, oracle_ai);
        let mut $oracle_map = OracleMap::load_one(&oracle_ai, 0u64, None).unwrap();

        let mut perp_market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: $mr_init,
            margin_ratio_maintenance: $mr_maint,
            status: MarketStatus::Initialized,
            order_step_size: 10_000_000,
            quote_spot_market_index: 0,
            oracle: oracle_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(perp_market, PerpMarket, perp_ai);
        let perp_ais = [perp_ai];
        let mut perp_iter = perp_ais.iter().peekable();
        let perp_writable = velocity::state::perp_market_map::get_market_set_from_list(vec![]);
        let $perp_map = PerpMarketMap::load(&perp_writable, &mut perp_iter).unwrap();

        let mut usdc_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            initial_liability_weight: SPOT_WEIGHT_PRECISION,
            maintenance_liability_weight: SPOT_WEIGHT_PRECISION,
            deposit_balance: 1_000_000_000_000_000_000,
            historical_oracle_data: HistoricalOracleData::default_quote_oracle(),
            ..SpotMarket::default()
        };
        create_anchor_account_info!(usdc_market, SpotMarket, usdc_ai);
        let spot_ais = [usdc_ai];
        let mut spot_iter = spot_ais.iter().peekable();
        let spot_writable =
            velocity::state::spot_market_map::get_writable_spot_market_set_from_many(vec![]);
        let $spot_map = SpotMarketMap::load(&spot_writable, &mut spot_iter).unwrap();
    };
}

/// A cross-margin user with a USDC deposit (collateral) and a single perp
/// position (liability) in market 0.
fn make_user(deposit_scaled: u64, base: i64) -> User {
    let mut user = User::default();
    user.pool_id = 0;
    user.spot_positions[0] = SpotPosition {
        market_index: 0,
        balance_type: SpotBalanceType::Deposit,
        scaled_balance: deposit_scaled,
        ..SpotPosition::default()
    };
    user.perp_positions[0] = PerpPosition {
        market_index: 0,
        base_asset_amount: base,
        ..PerpPosition::default()
    };
    user
}

/// A perp market for the pure `calculate_perp_position_value_and_pnl` path.
fn perp_market(mr_init: u32, mr_maint: u32, imf: u32) -> PerpMarket {
    PerpMarket {
        amm: AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,
            ..AMM::default()
        },
        margin_ratio_initial: mr_init,
        margin_ratio_maintenance: mr_maint,
        imf_factor: imf,
        status: MarketStatus::Initialized,
        order_step_size: 10_000_000,
        quote_spot_market_index: 0,
        ..PerpMarket::default()
    }
}

// ===========================================================================
// INVARIANT HARNESSES (Family V)
// ===========================================================================

/// V.1 — Margin valuation is total: `calculate_perp_position_value_and_pnl`
/// never panics/overflows over extreme positions/prices (SafeMath returns
/// `Err` rather than panicking; a raw overflow would be a genuine finding the
/// fuzzer flags). When it succeeds, the Initial-margin weighted uPnL respects
/// the `MAX_POSITIVE_UPNL_FOR_INITIAL_MARGIN` clamp.
#[cfg(feature = "prop_margin_value_totality")]
#[crucible_fuzz]
fn prop_margin_value_totality(
    fixture: &mut MarginFixture,
    #[range(1..1_000_000_000i64)] oracle_price: i64,
    #[range(-1_000_000_000_000i64..1_000_000_000_000i64)] base: i64,
    #[range(-1_000_000_000_000i64..1_000_000_000_000i64)] quote: i64,
    #[range(200..5000u32)] mr_init: u32,
    #[range(0..1_000_000u32)] imf: u32,
) {
    let _ = &fixture.ctx;
    let mr_maint = (mr_init / 2).max(1);
    let market = perp_market(mr_init, mr_maint, imf);
    let position = PerpPosition {
        market_index: 0,
        base_asset_amount: base,
        quote_asset_amount: quote,
        ..PerpPosition::default()
    };
    let opd = OraclePriceData {
        price: oracle_price,
        confidence: 0,
        delay: 0,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };
    // quote (USDC) valued at $1.
    let sqp = StrictOraclePrice::new(PRICE_PRECISION_I64, PRICE_PRECISION_I64, true);

    if let Ok((_margin_req, weighted_pnl, _liab_val, _base_val)) = calculate_perp_position_value_and_pnl(
        &position,
        &market,
        &opd,
        &sqp,
        MarginRequirementType::Initial,
        0,
    ) {
        // Safety clamp from margin.rs (line ~187) for the Initial calc.
        fuzz_assert_le!(weighted_pnl, MAX_POSITIVE_UPNL_FOR_INITIAL_MARGIN);
    }
}

/// V.2 — Initial margin requirement >= Maintenance requirement on identical
/// state. Evaluated on the same position/market with only the requirement
/// type flipped (market's initial ratio is >= maintenance by construction).
#[cfg(feature = "prop_initial_ge_maintenance")]
#[crucible_fuzz]
fn prop_initial_ge_maintenance(
    fixture: &mut MarginFixture,
    #[range(1..1_000_000_000i64)] oracle_price: i64,
    #[range(-500_000_000_000i64..500_000_000_000i64)] base: i64,
    #[range(-500_000_000_000i64..500_000_000_000i64)] quote: i64,
    #[range(200..5000u32)] mr_init: u32,
    #[range(0..1_000_000u32)] imf: u32,
) {
    let _ = &fixture.ctx;
    let mr_maint = (mr_init / 2).max(1);
    let market = perp_market(mr_init, mr_maint, imf);
    let position = PerpPosition {
        market_index: 0,
        base_asset_amount: base,
        quote_asset_amount: quote,
        ..PerpPosition::default()
    };
    let opd = OraclePriceData {
        price: oracle_price,
        confidence: 0,
        delay: 0,
        has_sufficient_number_of_data_points: true,
        sequence_id: None,
    };
    let sqp = StrictOraclePrice::new(PRICE_PRECISION_I64, PRICE_PRECISION_I64, true);

    let initial = calculate_perp_position_value_and_pnl(
        &position,
        &market,
        &opd,
        &sqp,
        MarginRequirementType::Initial,
        0,
    );
    let maintenance = calculate_perp_position_value_and_pnl(
        &position,
        &market,
        &opd,
        &sqp,
        MarginRequirementType::Maintenance,
        0,
    );

    if let (Ok(i), Ok(m)) = (initial, maintenance) {
        // .0 == margin_requirement
        fuzz_assert_ge!(i.0, m.0);
    }
}

/// V.3 — meets-initial => meets-maintenance. If an account clears the stricter
/// Initial gate it must clear the looser Maintenance gate; the reverse
/// implication is never violated. Uses the full cross-margin calculation.
#[cfg(feature = "prop_meets_initial_implies_maintenance")]
#[crucible_fuzz]
fn prop_meets_initial_implies_maintenance(
    fixture: &mut MarginFixture,
    #[range(1..1000i64)] price: i64,
    #[range(-100_000_000_000i64..100_000_000_000i64)] base: i64,
    #[range(0..1_000_000_000_000u64)] deposit: u64,
    #[range(200..2000u32)] mr_maint: u32,
) {
    let _ = &fixture.ctx;
    let mr_init = mr_maint.saturating_mul(2).min(MARGIN_PRECISION);
    build_maps!(price, mr_init, mr_maint, perp_map, spot_map, oracle_map);
    let user = make_user(deposit, base);

    let meets_init = meets_initial_margin_requirement(&user, &perp_map, &spot_map, &mut oracle_map);
    let meets_maint =
        meets_maintenance_margin_requirement(&user, &perp_map, &spot_map, &mut oracle_map);

    if let (Ok(mi), Ok(mm)) = (meets_init, meets_maint) {
        if mi {
            fuzz_assert!(mm);
        }
    }
}

/// V.4 — Liquidation sizing on a healthy account. At the pure-math tier this
/// pins: (a) a zero margin shortage sizes zero base to liquidate, and
/// (b) required base is monotone non-decreasing in the shortage. The full
/// "a healthy account is never touched, and transfer <= position size"
/// property is controller-level (the controller checks margin before sizing
/// and mins the transfer against the position) and belongs to the SVM tier
/// (P8).
#[cfg(feature = "prop_liquidation_sizing_healthy")]
#[crucible_fuzz]
fn prop_liquidation_sizing_healthy(
    fixture: &mut MarginFixture,
    #[range(0..1_000_000_000_000u128)] shortage: u128,
    #[range(1..2000u32)] margin_ratio: u32,
    #[range(0..50000u32)] liq_fee: u32,
    #[range(1..1_000_000_000i64)] oracle_price: i64,
    #[range(1..2_000_000i64)] quote_price: i64,
) {
    let _ = &fixture.ctx;

    // (a) zero shortage => nothing to liquidate (u64::MAX is the "refuse"
    // sentinel when margin_ratio <= liquidation_fee, not a real amount).
    if let Ok(b0) =
        calculate_base_asset_amount_to_cover_margin_shortage(0, margin_ratio, liq_fee, 0, oracle_price, quote_price)
    {
        if b0 != u64::MAX {
            fuzz_assert_eq!(b0, 0u64);
        }
    }

    // (b) monotone in shortage.
    let a = calculate_base_asset_amount_to_cover_margin_shortage(
        shortage, margin_ratio, liq_fee, 0, oracle_price, quote_price,
    );
    let b = calculate_base_asset_amount_to_cover_margin_shortage(
        shortage.saturating_add(1_000_000),
        margin_ratio,
        liq_fee,
        0,
        oracle_price,
        quote_price,
    );
    if let (Ok(a), Ok(b)) = (a, b) {
        if a != u64::MAX && b != u64::MAX {
            fuzz_assert_ge!(b, a);
        }
    }
}

/// V.5 — Liquidation fee is within its configured bounds for all inputs:
///  * `get_liquidation_fee` stays in `[base_fee, max_fee]`;
///  * `calculate_liquidation_multiplier` discount <= PRECISION <= premium;
///  * `calculate_perp_if_fee` / `calculate_spot_if_fee` <= the configured
///    `max_if_fee`.
#[cfg(feature = "prop_liquidation_fee_bounded")]
#[crucible_fuzz]
fn prop_liquidation_fee_bounded(
    fixture: &mut MarginFixture,
    #[range(0..1_000_001u32)] fee: u32,
    #[range(0..1_000_001u32)] base_fee: u32,
    #[range(0..1_000_001u32)] max_fee: u32,
    #[range(0..500_000u64)] cur_slot: u64,
    #[range(1..2000u32)] margin_ratio: u32,
    #[range(0..1_000_000_000_000u128)] shortage: u128,
    #[range(1..1_000_000_000i64)] price: i64,
    #[range(1..1_000_001u32)] max_if: u32,
) {
    let _ = &fixture.ctx;

    // get_liquidation_fee in [base, max] (max normalized to be >= base).
    let max = base_fee.max(max_fee);
    if let Ok(f) = get_liquidation_fee(base_fee, max, 0, cur_slot) {
        fuzz_assert_ge!(f, base_fee);
        fuzz_assert_le!(f, max);
    }

    // multiplier: discount <= PRECISION <= premium (fee <= PRECISION so the
    // discount subtraction does not underflow).
    if let (Ok(disc), Ok(prem)) = (
        calculate_liquidation_multiplier(fee, LiquidationMultiplierType::Discount),
        calculate_liquidation_multiplier(fee, LiquidationMultiplierType::Premium),
    ) {
        fuzz_assert_le!(disc, LIQUIDATION_FEE_PRECISION);
        fuzz_assert_ge!(prem, LIQUIDATION_FEE_PRECISION);
    }

    // perp IF fee capped by max_if.
    if let Ok(pf) = calculate_perp_if_fee(
        shortage,
        BASE_PRECISION_U64,
        margin_ratio,
        fee,
        price,
        PRICE_PRECISION_I64,
        max_if,
    ) {
        fuzz_assert_le!(pf, max_if);
    }

    // spot IF fee capped by max_if.
    let asset_mult = LIQUIDATION_FEE_PRECISION + fee;
    if let Ok(sf) = calculate_spot_if_fee(
        shortage,
        10u128.pow(9), // 1 token, 9 decimals
        SPOT_WEIGHT_PRECISION,
        asset_mult,
        11 * SPOT_WEIGHT_PRECISION / 10,
        LIQUIDATION_FEE_PRECISION,
        9,
        price,
        max_if,
    ) {
        fuzz_assert_le!(sf, max_if);
    }
}

/// V.6 — (a) size-discount asset weight <= its input asset weight (<= 1.0),
/// (b) size-premium liability weight >= its input liability weight (>= 1.0),
/// and (c) adding a positive-value deposit never lowers total collateral.
#[cfg(feature = "prop_asset_never_lowers_collateral")]
#[crucible_fuzz]
fn prop_asset_never_lowers_collateral(
    fixture: &mut MarginFixture,
    #[range(0..1_000_000_000_000_000u128)] size: u128,
    #[range(0..5_000_000u32)] imf: u32,
    #[range(0..10000u32)] weight: u32,
    #[range(1..1000i64)] price: i64,
    #[range(-100_000_000_000i64..100_000_000_000i64)] base: i64,
    #[range(0..1_000_000_000_000u64)] deposit: u64,
    #[range(1..1_000_000_000_000u64)] delta: u64,
    #[range(200..2000u32)] mr_maint: u32,
) {
    let _ = &fixture.ctx;

    // (a) asset weight bounded above by input weight (<= SPOT_WEIGHT_PRECISION).
    let asset_weight = weight.min(SPOT_WEIGHT_PRECISION);
    if let Ok(discounted) = calculate_size_discount_asset_weight(size, imf, asset_weight) {
        fuzz_assert_le!(discounted, asset_weight);
        fuzz_assert_le!(discounted, SPOT_WEIGHT_PRECISION);
    }

    // (b) liability weight bounded below by input weight (>= SPOT_WEIGHT_PRECISION).
    let liability_weight = SPOT_WEIGHT_PRECISION + weight;
    if let Ok(premium) =
        calculate_size_premium_liability_weight(size, imf, liability_weight, SPOT_WEIGHT_PRECISION_U128, true)
    {
        fuzz_assert_ge!(premium, liability_weight);
        fuzz_assert_ge!(premium, SPOT_WEIGHT_PRECISION);
    }

    // (c) collateral monotone in deposit: same maps, two users differing only
    // by a strictly larger deposit.
    let mr_init = mr_maint.saturating_mul(2).min(MARGIN_PRECISION);
    build_maps!(price, mr_init, mr_maint, perp_map, spot_map, oracle_map);
    let user_small = make_user(deposit, base);
    let user_large = make_user(deposit.saturating_add(delta), base);

    let c_small = calculate_margin_requirement_and_total_collateral_and_liability_info(
        &user_small,
        &perp_map,
        &spot_map,
        &mut oracle_map,
        MarginContext::standard(MarginRequirementType::Maintenance),
    );
    let c_large = calculate_margin_requirement_and_total_collateral_and_liability_info(
        &user_large,
        &perp_map,
        &spot_map,
        &mut oracle_map,
        MarginContext::standard(MarginRequirementType::Maintenance),
    );

    if let (Ok(cs), Ok(cl)) = (c_small, c_large) {
        fuzz_assert_ge!(cl.total_collateral, cs.total_collateral);
    }
}

// ===========================================================================
// REGRESSION HARNESSES (PENDING audit-fix PRs — violate on current master)
// ===========================================================================

// PENDING PR #267 (F5 liquidation math): liquidate_perp_pnl_for_deposit must
// not let the post-transfer buffered margin shortage exceed the prior shortage
// (the fix reverts with `LiquidationWorsensAccountHealth` 6360). This models
// the arithmetic core: seizing a quote deposit at the liquidator premium
// (asset_liquidation_multiplier) in exchange for pnl relief changes the
// buffered shortage by `seized_value - relieved_value*(1 + buffer)`. When the
// liquidator fee exceeds the liquidation buffer, that delta is positive — the
// transfer strips more collateral than the pnl relief plus buffer benefit, and
// master has no guard against it. Asserts the FIXED invariant (non-worsening,
// within a $1 tolerance for deposit dust rounding).
//
// NOTE: the authoritative reproduction is the controller revert
// (`LiquidationWorsensAccountHealth`), which is SVM-tier (P8). This host model
// reproduces the underlying worsening arithmetic so the fuzzer demonstrably
// catches the bug; its un-gate on merge tracks the P8 SVM harness.
#[cfg(feature = "regr_267_worsens_health")]
#[crucible_fuzz]
fn regr_267_worsens_health(
    fixture: &mut MarginFixture,
    #[range(1..1_000_000_000u128)] liability_transfer: u128,
    #[range(0..100_000u32)] liquidator_fee: u32,
    #[range(0..100_000u32)] buffer: u32,
) {
    let _ = &fixture.ctx;

    // pnl-for-deposit: seize a quote deposit (asset) at a liquidator premium in
    // exchange for relieving negative pnl (liability). Equal (6) decimals and
    // unit ($1) prices; a large asset_amount avoids the round-to-asset nudge.
    let asset_amount: u128 = 1_000_000_000_000_000; // 1e15
    let asset_liq_mult = LIQUIDATION_FEE_PRECISION + liquidator_fee; // premium on seized deposit
    let liab_liq_mult = LIQUIDATION_FEE_PRECISION; // pnl leg, no discount

    if let Ok(asset_transfer) = calculate_asset_transfer_for_liability_transfer(
        asset_amount,
        asset_liq_mult,
        6,
        PRICE_PRECISION_I64,
        liability_transfer,
        liab_liq_mult,
        6,
        PRICE_PRECISION_I64,
    ) {
        let seized = asset_transfer as i128; // quote value (price $1, 6 decimals)
        let relieved = liability_transfer as i128;
        // buffered collateral benefit from relieving negative pnl.
        let buffer_benefit = relieved.saturating_mul(buffer as i128) / (MARGIN_PRECISION as i128);
        let shortage_delta = seized - relieved - buffer_benefit;

        // FIXED invariant: the transfer must not worsen the buffered shortage
        // (with $1 dust tolerance). Violated on master when the premium exceeds
        // the buffer.
        fuzz_assert_le!(shortage_delta, QUOTE_PRECISION as i128);
    }
}

// PENDING PR #243 (spot-liq protective pricing): when a deposit/borrow oracle
// is margin-invalid, the transfer exchange rate must use the protective price
// so collateral seized <= what a valid price allows. `asset_transfer` is
// inversely proportional to the asset (collateral) price, so pricing the
// collateral leg at the raw (stale/depressed) oracle instead of the protective
// price `max(oracle, 5min twap, oracle + confidence)` seizes strictly more
// deposit than the protective price would. Asserts the FIXED invariant
// (seized_at_raw <= seized_at_protective); violated on master, which prices at
// the raw oracle.
//
// NOTE: the fix selects the protective price inside the liquidation
// controllers (and adds `calculate_user_protective_asset_price`, absent on
// master), so the authoritative reproduction is SVM-tier (P8). This host model
// compares the two exchange rates the pure sizing function produces.
#[cfg(feature = "regr_243_protective_pricing")]
#[crucible_fuzz]
fn regr_243_protective_pricing(
    fixture: &mut MarginFixture,
    #[range(1..1000i64)] oracle_dollar: i64,
    #[range(1..1000i64)] twap_dollar: i64,
    #[range(0..500i64)] conf_dollar: i64,
    #[range(1..1_000_000_000u128)] liability_transfer: u128,
) {
    let _ = &fixture.ctx;

    let oracle = oracle_dollar * PRICE_PRECISION_I64;
    let twap = twap_dollar * PRICE_PRECISION_I64;
    let conf = conf_dollar * PRICE_PRECISION_I64;
    // Collateral (deposit) leg protective price.
    let protective = oracle.max(twap).max(oracle + conf);

    let asset_amount: u128 = 1_000_000_000_000_000; // 1e15, avoids round-to-asset
    let liab_price = PRICE_PRECISION_I64; // borrow leg $1

    let seized_raw = calculate_asset_transfer_for_liability_transfer(
        asset_amount,
        LIQUIDATION_FEE_PRECISION,
        6,
        oracle, // master: raw (possibly depressed) oracle
        liability_transfer,
        LIQUIDATION_FEE_PRECISION,
        6,
        liab_price,
    );
    let seized_protective = calculate_asset_transfer_for_liability_transfer(
        asset_amount,
        LIQUIDATION_FEE_PRECISION,
        6,
        protective, // fix: protective price
        liability_transfer,
        LIQUIDATION_FEE_PRECISION,
        6,
        liab_price,
    );

    if let (Ok(raw), Ok(prot)) = (seized_raw, seized_protective) {
        // FIXED invariant: collateral seized never exceeds the protective-price
        // amount. Violated on master whenever protective > oracle.
        fuzz_assert_le!(raw, prot);
    }
}
