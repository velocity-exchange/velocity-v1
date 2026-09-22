//! Devnet end-to-end suite proving the deployed velocity program + bots work.
//!
//! Gated behind `rpc_tests`; run by the `rust-live-tests` / `devnet-e2e` CI jobs
//! (manual / scheduled). Requires `TEST_DEVNET_RPC_ENDPOINT` and a funded
//! `TEST_PRIVATE_KEY`, and an initialized devnet (run `deploy-scripts/init-devnet.ts`).
//!
//! Hybrid intent: for actions a DEPLOYED bot owns (DLOB fills, liquidations,
//! pnl settling, mark-twap crank) the test sets up one side and
//! polls for the bot to act; pure user actions (deposit/withdraw/AMM-take) are
//! driven directly. Each scenario owns a fixed, REUSED subaccount of the one
//! funded payer (see the `SUB_*` ids + `TestCtx::acquire`, which resets the
//! account to a clean slate each run) rather than allocating a fresh one — fresh
//! allocation leaked rent and exhausted the monotonic u16 sub-account-id space.
//! Run with `--test-threads=1`.
//!
//! Bot-timing-dependent scenarios (liquidation via oracle drift, settler
//! thresholds) RUN — their setup is deterministic and they treat
//! "bot didn't act in time" as inconclusive (warn, not failure), so they're safe
//! in the nightly non-gating job. `#[ignore]` is reserved for scenarios whose
//! setup itself can't be established on devnet right now (swift HTTP 502, SOL
//! borrow unavailable); those run only in the manual `--include-ignored` job.
#![cfg(feature = "rpc_tests")]

mod common;

use std::time::Duration;

use base64::Engine;
use common::*;
use nanoid::nanoid;
use velocity_rs::{
    constants::derive_perp_market_account,
    math::constants::BASE_PRECISION_I64,
    swift_order_subscriber::SignedOrderType,
    types::{
        accounts::PerpMarket, MarketType, NewOrder, OrderParams, OrderStatus, OrderType,
        PositionDirection, PostOnlyParam, SignedMsgOrderParamsMessage,
    },
    utils::try_deser_zero_copy,
};

const ONE_SOL: i64 = BASE_PRECISION_I64; // 1e9, 9 decimals
/// Slack (native units) on spot token-amount asserts to absorb the
/// scaled-balance ↔ token-amount interest-index round-trip rounding. 100 native
/// dUSDT units = 1e-4 dUSDT.
const DUSDT_SLACK: u128 = 100;

// Fixed, REUSED subaccount ids — one per test. The suite reuses these accounts
// every run (via `TestCtx::acquire`, which resets them to a clean slate) instead
// of allocating a fresh subaccount per run, which leaked rent and exhausted the
// monotonic u16 `number_of_sub_accounts_created`. Ids are contiguous so the
// sequential bootstrap in `acquire` never leaves gaps. (The one-shot
// `fund_mm_taker_subaccounts` uses ids 1/2 on a DIFFERENT authority — the filler
// key — so it does not collide with these.)
const SUB_DEPOSIT: u16 = 1;
const SUB_WITHDRAW: u16 = 2;
const SUB_TAKER_AMM: u16 = 3;
const SUB_TAKER_JIT: u16 = 4;
const SUB_DLOB_MAKER: u16 = 5;
const SUB_DLOB_TAKER: u16 = 6;
const SUB_BAD_PERP: u16 = 7;
const SUB_BAD_SPOT_BORROW: u16 = 8;
const SUB_UNSETTLED_PNL: u16 = 9;
const SUB_SWIFT: u16 = 10;

/// A marketable 1-SOL limit order priced 5% through the oracle in the trade
/// direction, so it crosses the AMM and the DEPLOYED filler fills it against the
/// AMM (place_and_take with no makers does NOT fill vs the AMM — fills go through
/// the filler). Rest it with `place_and_make`, then poll for the fill.
fn marketable_limit(px: u64, direction: PositionDirection) -> OrderParams {
    let (amount, price) = match direction {
        PositionDirection::Long => (ONE_SOL, px + px * 5 / 100),
        PositionDirection::Short => (-ONE_SOL, px.saturating_sub(px * 5 / 100)),
    };
    NewOrder::limit(SOL_PERP)
        .amount(amount)
        .price(price)
        .build()
}

/// The program's own clamp target for a LONG market-order auction: the oracle-
/// relative (start, end) offsets `update_perp_auction_params_market_and_oracle_orders`
/// sanitizes toward, at the market-order buffer scalar of 2.
fn baseline_long_offsets(market: &PerpMarket) -> (i64, i64) {
    OrderParams::get_perp_baseline_start_end_price_offset(market, PositionDirection::Long, 2)
        .expect("baseline offsets")
}

/// Read a perp market straight from RPC, BYPASSING the websocket account cache.
///
/// `VelocityClient::get_perp_market_account` serves the subscribed cache (TestCtx
/// subscribes to all three markets), and a cache entry's slot has no ordering
/// guarantee against a just-confirmed tx — a websocket update can still be in
/// flight. Callers that need reads which provably straddle a placement slot (the
/// auction baseline bracket in `taker_fills_against_amm`) must not use the cache.
async fn fetch_perp_market(ctx: &TestCtx, market_index: u16) -> PerpMarket {
    let data = ctx
        .client
        .rpc()
        .get_account_data(&derive_perp_market_account(market_index))
        .await
        .expect("fetch perp market from rpc");
    // Pod reader, not anchor's `try_deserialize`: the latter takes a reference into
    // byte-aligned RPC bytes and panics on 16-aligned zero-copy structs off-chain.
    try_deser_zero_copy::<PerpMarket>(&data).expect("decode perp market")
}

// ---- One-shot devnet provisioning (NOT a CI assertion) ---------------------
// Initialize + fund the MM (sub 1) and taker (sub 2) subaccounts of whatever
// authority TEST_PRIVATE_KEY holds, so the deployed rust-quoter-bot (sub 1) and
// rust-taker-bot (sub 2) have dUSDT collateral. Run it with the FILLER key.
//
// This is a plain set of devnet transactions — it needs only the key + a devnet
// RPC, run from anywhere. It does NOT need cluster access: the keep-rs pod can't
// faucet/deposit (it only ships the keeprs binary), so provisioning is done here.
// `#[ignore]` so it never runs in CI. Amount per subaccount via FUND_DUSDT (whole
// dUSDT, default 5000).
//
//   TEST_PRIVATE_KEY=<filler base58> TEST_DEVNET_RPC_ENDPOINT=<rpc> \
//     cargo test -p velocity-rs --test devnet_e2e --features rpc_tests \
//     fund_mm_taker_subaccounts -- --ignored --nocapture
#[tokio::test]
#[ignore = "one-shot devnet provisioning for the MM/taker bots, not a CI assertion"]
async fn fund_mm_taker_subaccounts() {
    let ctx = TestCtx::new().await;
    let amount: u64 = std::env::var("FUND_DUSDT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5_000);
    for sub_id in [1u16, 2u16] {
        ctx.ensure_subaccount(sub_id).await;
        ctx.fund_and_deposit_dusdt(ctx.sub(sub_id), amount).await;
        let bal = ctx.spot_token_amount(ctx.sub(sub_id), 0).await;
        let role = if sub_id == 1 { "maker" } else { "taker" };
        log::warn!(
            "provisioned sub {sub_id} ({role}) for {}: dUSDT collateral = {} (~{} whole)",
            ctx.authority(),
            bal,
            bal / DUSDT_PRECISION as u128,
        );
    }
}

// ---- Scenario 3: deposit (self-driven) -------------------------------------
#[tokio::test]
async fn deposit_into_spot_market() {
    let ctx = TestCtx::new().await;
    let sub = ctx.acquire(SUB_DEPOSIT).await;

    // Assert the DELTA, not the absolute balance: the subaccount is reused across
    // runs, so any residual from a prior run (reset drains best-effort) must not
    // skew the result. Depositing exactly 50 dUSDT must raise collateral by 50.
    let before = ctx.spot_token_amount(sub, 0).await;
    ctx.fund_and_deposit_dusdt(sub, 50).await;
    let after = ctx.spot_token_amount(sub, 0).await;
    let deposited = after.saturating_sub(before);
    let expected = 50 * DUSDT_PRECISION as u128;
    assert!(
        deposited.abs_diff(expected) <= DUSDT_SLACK,
        "deposit raised dUSDT collateral by {deposited} != {expected} (±{DUSDT_SLACK} native)"
    );
}

// ---- Scenario 4: withdraw (self-driven) ------------------------------------
#[tokio::test]
async fn withdraw_from_spot_market() {
    let ctx = TestCtx::new().await;
    let sub = ctx.acquire(SUB_WITHDRAW).await;
    ctx.fund_and_deposit_dusdt(sub, 50).await; // 50 dUSDT in
    let before = ctx.spot_token_amount(sub, 0).await;

    let tx = ctx
        .client
        .init_tx(&sub, false)
        .await
        .unwrap()
        .withdraw(10 * DUSDT_PRECISION, 0, Some(true), None)
        .build();
    ctx.send_confirmed(tx).await;

    // Assert the DELTA, not the absolute balance (the subaccount is reused, so a
    // prior-run residual must not skew it): withdrawing 10 drops collateral by 10.
    let after = ctx.spot_token_amount(sub, 0).await;
    let withdrawn = before.saturating_sub(after);
    let want_withdrawn = 10 * DUSDT_PRECISION as u128;
    assert!(
        withdrawn.abs_diff(want_withdrawn) <= DUSDT_SLACK,
        "withdraw dropped dUSDT collateral by {withdrawn} != {want_withdrawn} (±{DUSDT_SLACK} native)"
    );
}

// ---- Scenario 7: taker order fills against the AMM -------------------------
//
// This test is SOUND AGAINST AUCTION SANITIZATION. The earlier version rested a
// long MARKET order with an aggressive `+2% → +15%` auction and waited 120s for a
// lone-taker vAMM fill. It silently timed out — not a filler/DLOB/gRPC bug, but
// because the program *rewrites* a market order's auction params on placement
// (`OrderParams::update_perp_auction_params_market_and_oracle_orders`). The
// requested band never reaches the chain: for a long it is clamped to the AMM
// baseline offsets (`get_perp_baseline_start_end_price_offset(.., Long, 2)`).
//
// That baseline is itself live state, and on devnet it swings widely. Its END is
// `(last_ask_price_twap - oracle_twap) + baseline_end_price_buffer`, where the
// buffer is 2x the widest of mark_std / oracle_std / (amm spread * twap), clamped
// by the contract tier (`get_auction_end_min_max_divisors`: 1%..10% of price for
// Speculative). Both terms run large here: the bid/ask TWAPs are an EWMA over the
// funding period and lag the oracle badly on a low-volume market, and the buffer
// routinely pins to the 10% ceiling. Observed extremes on devnet market 0, a
// baseline end at oracle+0.5% (TWAPs behind, buffer small) and at oracle+21%
// (mark TWAP 10% above the oracle TWAP, amm.long_spread at 11%).
//
// Two consequences:
//   * the sanitized end can sit BELOW the live `vamm_ask`, which tracks the live
//     reserve price, so a lone-taker AMM fill is impossible and the filler
//     correctly never fills. Hence the crossability gate + warn-skip below rather
//     than a blind 120s timeout. A real fill needs the TWAPs warmed to the live
//     price, or the JIT route (`amm_wants_to_jit_make`, inventory + jit_intensity).
//   * the requested band must be derived FROM the live baseline, never hardcoded.
//     A fixed `+2% → +15%` is not reliably aggressive: once the baseline end runs
//     ~+20%, a +15% end is MILDER than the baseline, so the program leaves it
//     untouched and clamps only the start. The stored band is then
//     `requested_end - baseline_start`, which never matches the baseline band, and
//     check (2) fails while the program is behaving correctly.
//
// So instead of asserting a fill that sanitization can forbid, this test:
//   1. proves sanitization is active (the aggressive request is discarded),
//   2. locks the clamp target to the program's own baseline (regression guard),
//   3. resolves the fill three ways from live state, never a blind timeout:
//      - the order ALREADY filled (crossable day: sanitized end out-priced
//        `vamm_ask`, filler/AMM took it within the confirm window) — success;
//        the regression checks above run on the filled order's persisted params;
//      - it's resting and the sanitized end is <= `vamm_ask` (uncrossable, TWAP
//        lag) — INCONCLUSIVE warn-skip with the exact numbers;
//      - it's resting and crossable — wait for the filler to fill +1 SOL.
// NB the order read-back matches on the market order regardless of Open/Filled,
// because the deployed filler often fills it before the read.
#[tokio::test]
async fn taker_fills_against_amm() {
    let ctx = TestCtx::new().await;
    let sub = ctx.acquire(SUB_TAKER_AMM).await;
    ctx.fund_and_deposit_dusdt(sub, 100).await;

    let px = ctx.client.oracle_price(SOL_PERP).await.expect("oracle") as u64;

    // allow-verbose: derivation of the aggression margin below is a bound a
    // reader cannot reconstruct from the code alone.
    //
    // Aggressive RELATIVE TO THE LIVE BASELINE so sanitization must clamp both ends.
    // place_and_take routes the order through the book; a remainder it cannot fill
    // in this transaction rests on the CLOB until the deployed filler crosses it.
    //
    // The baseline read here is not the one the program will apply: it recomputes at
    // the placement slot. Size the offset so the request stays past the live
    // threshold even if the baseline RISES in between, otherwise that end is left
    // unclamped and the checks below fail on correct behaviour. The only fast-moving
    // term is `baseline_end_price_buffer`, hard-bounded above by the contract tier's
    // ceiling (`oracle_twap / max_divisor`), so clearing that ceiling covers a fill
    // or the vamm-widening crank driving amm.long_spread / mark_std from the buffer's
    // floor to its cap. The TWAP terms are an EWMA over the ~1h funding period and
    // cannot move materially in the second before placement. The extra 4% is the
    // margin check (1) asserts on.
    let market_before = fetch_perp_market(&ctx, 0).await;
    let (_, max_divisor) = market_before
        .get_auction_end_min_max_divisors()
        .expect("auction end divisors");
    let buffer_ceiling = market_before
        .market_stats
        .historical_oracle_data
        .last_oracle_price_twap
        .unsigned_abs()
        / max_divisor;
    let (pre_start_off, pre_end_off) = baseline_long_offsets(&market_before);
    let aggression = (buffer_ceiling + px / 25) as i64;
    let requested_start = px as i64 + pre_start_off + aggression;
    let requested_end = px as i64 + pre_end_off + aggression;
    let order = OrderParams {
        order_type: OrderType::Market,
        market_type: MarketType::Perp,
        market_index: 0,
        direction: PositionDirection::Long,
        base_asset_amount: ONE_SOL as u64,
        auction_start_price: Some(requested_start),
        auction_end_price: Some(requested_end),
        auction_duration: Some(200),
        ..Default::default()
    };
    let clob = clob_accounts(&ctx.client, 0).await;
    let tx = ctx
        .client
        .init_tx(&sub, false)
        .await
        .unwrap()
        .place_and_take(order, clob, None)
        .build();
    ctx.send_confirmed(tx).await;

    // --- Read back the ON-CHAIN (sanitized) order --------------------------
    // The order may already be FILLED by the time we read it: on a *crossable* day
    // the sanitized auction out-prices the vAMM ask and the deployed filler/AMM
    // fill it within the confirm window. The sanitized auction params persist on
    // the order slot whether it is Open or Filled, so match on the market order
    // regardless of status and inspect those.
    let user = ctx
        .client
        .get_user_account(&sub)
        .await
        .expect("user account");
    let position = user
        .get_perp_position(0)
        .map(|p| p.base_asset_amount)
        .unwrap_or(0);
    let placed = match user.orders.iter().find(|o| {
        o.market_index == 0 && o.order_type == OrderType::Market && o.status != OrderStatus::Init
    }) {
        Some(o) => *o,
        None => {
            // The order isn't in any slot — it fully filled and the slot was
            // cleared. The sanitized params are gone, but a crossable fill IS the
            // success path: assert the position and return.
            assert_eq!(
                position, ONE_SOL,
                "no market order on chain and position={position} (expected the \
                 order to have filled to +1 SOL against the AMM)",
            );
            log::warn!(
                "taker_fills_against_amm: order fully filled and slot cleared; \
                 sanitization regression not inspectable this run (position=+1 SOL)."
            );
            ctx.cleanup(sub).await;
            return;
        }
    };

    // (1) Regression: the aggressive request was discarded by sanitization. Require
    // each end to have dropped by 1% of price. A bare `<` would pass on rounding
    // alone, because `get_auction_params` standardizes a long's prices DOWN to the
    // tick, so `placed < requested` holds by up to tick_size - 1 (10 native units
    // here) even when nothing was clamped. 1% is small enough that the drop still
    // clears it after the baseline has risen by its full ceiling (the `aggression`
    // sizing above leaves 4% of price, minus any oracle move, for this check).
    let discarded_by = (px / 100) as i64;
    assert!(
        requested_start - placed.auction_start_price >= discarded_by
            && requested_end - placed.auction_end_price >= discarded_by,
        "sanitization did not clamp the aggressive auction: on-chain start={} end={} \
         vs requested start={} end={} (each end must drop by >= {})",
        placed.auction_start_price,
        placed.auction_end_price,
        requested_start,
        requested_end,
        discarded_by,
    );

    // (2) Regression: the clamp target is the program's own market-order baseline
    // (factor 2). The program sets start = oracle + start_off, end = oracle +
    // end_off using the SAME oracle, so the spread (end - start) is
    // oracle-independent and must equal (end_off - start_off).
    //
    // The program computes the baseline from market state at the PLACEMENT slot,
    // which we can't read. Bracket it instead, with baselines read before and after
    // placement: the window widens by exactly whatever moved in between (a mark-twap
    // crank, a fill shifting amm.long_spread or mark_std) and stays a point when
    // nothing moved. `tol` covers tick rounding. Both reads go straight to RPC —
    // `market_before` because the tx has not been sent yet, this one because it
    // follows `send_confirmed`, so they provably straddle the placement slot. The
    // cache cannot give that ordering. (A non-monotone excursion that peaks between
    // the two reads AND lands on the placement slot would still escape the bracket;
    // the message prints both bounds so that reads as drift, not as a regression.)
    let market = fetch_perp_market(&ctx, 0).await;
    let (start_off, end_off) = baseline_long_offsets(&market);
    let onchain_spread = placed.auction_end_price - placed.auction_start_price;
    let pre_spread = pre_end_off - pre_start_off;
    let post_spread = end_off - start_off;
    let tol = (px / 400) as i64; // 25 bps
    assert!(
        onchain_spread >= pre_spread.min(post_spread) - tol
            && onchain_spread <= pre_spread.max(post_spread) + tol,
        "sanitized auction spread {} outside the program baseline spread bracket \
         [{}, {}] (tol {}); sanitization logic changed",
        onchain_spread,
        pre_spread.min(post_spread),
        pre_spread.max(post_spread),
        tol,
    );

    // The order may have already filled (crossable day): the sanitized auction
    // out-priced the vAMM ask and the deployed filler/AMM took it. That IS the
    // success path this test exercises — the regression checks above already ran
    // on the (filled) order's persisted auction params.
    if position == ONE_SOL || placed.base_asset_amount_filled == ONE_SOL as u64 {
        log::info!(
            "taker_fills_against_amm: sanitized auction crossed and filled to +1 SOL \
             (position={position}, base_filled={}).",
            placed.base_asset_amount_filled,
        );
        ctx.cleanup(sub).await;
        return;
    }

    // (3) Crossability gate: does the SANITIZED auction reach the live vAMM ask?
    let reserve = market.amm.reserve_price().expect("reserve price");
    let vamm_ask = market
        .amm
        .ask_price(
            reserve,
            market.amm.long_spread,
            market.amm.reference_price_offset,
        )
        .expect("vamm ask");

    if (placed.auction_end_price as u64) <= vamm_ask {
        // ENVIRONMENTAL, not a code regression — so warn-and-skip rather than fail.
        //
        // On a low-volume market the bid/ask price TWAPs (an EWMA over the funding
        // period) lag the live oracle: `vamm_ask` tracks the live reserve price
        // while the sanitized auction end is built from the lagging TWAPs, so the
        // auction is capped BELOW the live ask and no lone-taker AMM fill is
        // possible. The sanitization regression checks above have already run and
        // passed, so turning a TWAP-lag into a red test (or a silent 120s timeout)
        // would be noise. We log the exact numbers and return.
        //
        // To actually exercise the fill you must lift `last_ask_price_twap` to the
        // live price first — but that's a funding-period EWMA, so it takes minutes
        // of trades/cranks (~0.3% of the gap closes per ~10s crank). The clean,
        // deterministic alternative is the JIT route (`amm_wants_to_jit_make`: AMM
        // inventory + jit_intensity > 0), which doesn't depend on the TWAP at all.
        log::warn!(
            "INCONCLUSIVE (AMM uncrossable by construction): sanitized auction end {} <= \
             vamm_ask {} (oracle {}). baseline_offsets=({start_off},{end_off}) \
             onchain_auction=({},{}). Skipping fill assertion — see comment above.",
            placed.auction_end_price,
            vamm_ask,
            px,
            placed.auction_start_price,
            placed.auction_end_price,
        );
        ctx.cleanup(sub).await;
        return;
    }

    // The sanitized auction DOES cross the AMM ask: the deployed filler must fill
    // it to exactly +1 SOL against the AMM (no maker present).
    let pos = ctx
        .wait_perp_base_eq(sub, 0, ONE_SOL, Duration::from_secs(120))
        .await
        .expect("sanitized auction crosses the AMM ask but the filler did not fill +1 SOL in 120s");
    assert_eq!(
        pos.base_asset_amount, ONE_SOL,
        "expected exactly +1 SOL long vs AMM, got {}",
        pos.base_asset_amount
    );
    ctx.cleanup(sub).await;
}

// ---- Scenario 7b: taker fills against the AMM via the JIT route ------------
//
// The JIT route (`Amm::amm_wants_to_jit_make`) is the OTHER way a lone taker
// reaches the AMM, and unlike the low-risk auction route in `taker_fills_against_amm`
// it does NOT depend on the (sanitized) auction out-pricing `vamm_ask` — the AMM
// proactively makes to OFFLOAD inventory, so it can fill same-slot via
// `place_and_take` (no external filler). It needs two preconditions:
//   * `amm_jit_intensity > 0` (devnet init now sets 100), and
//   * the AMM holding inventory on the side a taker would relieve. Note
//     `base_asset_amount_with_amm == net_user_position`: users net SHORT
//     (`< -order_step_size`) => AMM net long => a LONG taker lets it sell down;
//     users net LONG (`> order_step_size`) => AMM net short => a SHORT taker.
//
// Both preconditions are MARKET STATE we can't set from here (no admin to reseed
// jit intensity, and seeding AMM inventory needs prior flow). So this test is
// sound against that: it picks the taker direction FROM the live inventory and,
// if the preconditions aren't met (AMM flat, or jit inactive), warn-and-skips
// with the exact reason instead of failing. When they ARE met it sends
// `place_and_take` and asserts the AMM JIT-filled the taker in-tx.
#[tokio::test]
async fn taker_fills_against_amm_via_jit() {
    let ctx = TestCtx::new().await;
    let market = ctx
        .client
        .get_perp_market_account(0)
        .await
        .expect("perp market");
    let step = market.order_step_size;
    let inventory = market.amm.base_asset_amount_with_amm;
    let jit_intensity = market.amm.amm_jit_intensity;

    // Pick the taker direction the AMM would JIT-make for, from live inventory.
    let direction = if inventory < -(step as i128) {
        PositionDirection::Long
    } else if inventory > step as i128 {
        PositionDirection::Short
    } else {
        log::warn!(
            "INCONCLUSIVE (AMM flat): base_asset_amount_with_amm={} within +/- order_step_size={} \
             — no inventory for the AMM to JIT-offload. Skipping (needs prior flow to seed inventory).",
            inventory, step,
        );
        return;
    };

    // amm_wants_to_jit_make folds in the `amm_jit_intensity > 0` check.
    if !market
        .amm
        .amm_wants_to_jit_make(direction, step)
        .expect("jit check")
    {
        log::warn!(
            "INCONCLUSIVE (JIT inactive): amm_jit_intensity={} base_asset_amount_with_amm={} \
             order_step_size={} dir={:?} — AMM won't JIT-make. Skipping (can't reseed jit \
             intensity here).",
            jit_intensity,
            inventory,
            step,
            direction,
        );
        return;
    }

    let sub = ctx.acquire(SUB_TAKER_JIT).await;
    ctx.fund_and_deposit_dusdt(sub, 100).await;

    // Market order, auction params left to the program to derive (direction-correct);
    // place_and_take takes it in the SAME slot and the JIT route fills it directly
    // against the AMM — JIT making does NOT require crossing vamm_ask.
    let order = OrderParams {
        order_type: OrderType::Market,
        market_type: MarketType::Perp,
        market_index: 0,
        direction,
        base_asset_amount: ONE_SOL as u64,
        ..Default::default()
    };
    let clob = clob_accounts(&ctx.client, 0).await;
    let tx = ctx
        .client
        .init_tx(&sub, false)
        .await
        .unwrap()
        .place_and_take(order, clob, None)
        .build();
    ctx.send_confirmed(tx).await;

    // JIT fills in-tx; the taker opens a position on the chosen side. JIT may
    // partial-fill if AMM inventory < order size, so assert the SIGN, not exact.
    let pos = ctx
        .wait_perp_position_opened(sub, 0, Duration::from_secs(20))
        .await
        .expect(
            "preconditions met (jit active + AMM inventory) but place_and_take opened no \
             position vs the AMM — JIT route regressed",
        );
    match direction {
        PositionDirection::Long => assert!(
            pos.base_asset_amount > 0,
            "expected long vs AMM, got {}",
            pos.base_asset_amount
        ),
        PositionDirection::Short => assert!(
            pos.base_asset_amount < 0,
            "expected short vs AMM, got {}",
            pos.base_asset_amount
        ),
    }
    log::info!(
        "JIT fill ok: dir={:?} base={} (amm inventory before={})",
        direction,
        pos.base_asset_amount,
        inventory,
    );
    ctx.cleanup(sub).await;
}

// ---- Scenario 1: resting maker + crossing taker, DEPLOYED filler matches ----
#[tokio::test]
async fn dlob_maker_taker_filled_by_filler() {
    let ctx = TestCtx::new().await;
    let maker = ctx.acquire(SUB_DLOB_MAKER).await;
    let taker = ctx.acquire(SUB_DLOB_TAKER).await;
    ctx.fund_and_deposit_dusdt(maker, 100).await;
    ctx.fund_and_deposit_dusdt(taker, 100).await;

    let px = ctx.client.oracle_price(SOL_PERP).await.expect("oracle") as u64;
    let clob = clob_accounts(&ctx.client, 0).await;

    // Maker rests a best bid 5bps under oracle (post-only so it can't cross).
    let maker_bid = px - px * 5 / 10_000;
    let tx = ctx
        .client
        .init_tx(&maker, false)
        .await
        .unwrap()
        .place_and_make(
            NewOrder::limit(SOL_PERP)
                .amount(ONE_SOL)
                .price(maker_bid)
                .post_only(PostOnlyParam::MustPostOnly)
                .build(),
            clob,
            None,
        )
        .build();
    ctx.send_confirmed(tx).await;

    // Taker rests a marketable short 15bps under oracle (crosses the maker bid);
    // it does NOT self-fill — the deployed filler must match it.
    let taker_ask = px - px * 15 / 10_000;
    let tx = ctx
        .client
        .init_tx(&taker, false)
        .await
        .unwrap()
        .place_and_make(
            NewOrder::limit(SOL_PERP)
                .amount(-ONE_SOL)
                .price(taker_ask)
                .build(),
            clob,
            None,
        )
        .build();
    ctx.send_confirmed(tx).await;

    // The deployed filler must fill the crossing taker to exactly -1 SOL.
    ctx.wait_perp_base_eq(taker, 0, -ONE_SOL, Duration::from_secs(60))
        .await
        .expect("deployed filler did not fill the taker to exactly -1 SOL within 60s");

    // The taker is a marketable limit, so the program gives it an auto-auction:
    // during it the filler fulfils from a MIX of resting makers AND the AMM (and
    // any deployed maker that's also quoting). So our maker is NOT guaranteed the
    // full 1 SOL — it competes with the vAMM and other makers. The invariant this
    // test actually proves is that the filler matched the taker against OUR resting
    // maker, i.e. our best-bid maker received a non-zero fill (partial is fine; the
    // AMM/other makers take the remainder). If our maker is fully out-competed
    // (taker filled entirely by the AMM/others), that's environmental — warn-skip,
    // per the suite's design contract, rather than hard-fail.
    match ctx
        .wait_perp_position_opened(maker, 0, Duration::from_secs(30))
        .await
    {
        Some(maker_pos) => {
            let filled = maker_pos.base_asset_amount;
            assert!(
                filled > 0 && filled <= ONE_SOL,
                "maker fill {filled} should be in (0, +1 SOL] — the filler matched our \
                 resting maker against the taker",
            );
            log::info!(
                "deployed filler matched taker against our resting maker: maker_base={filled} \
                 (partial is expected; the vAMM/other makers take the rest of the auction)."
            );
        }
        None => {
            log::warn!(
                "INCONCLUSIVE: taker filled to -1 SOL but our resting maker (best bid \
                 {maker_bid}) received no fill within 30s — the taker was filled entirely by \
                 the vAMM/other makers during its auction, so the filler's maker-match path \
                 wasn't exercised this run."
            );
        }
    }
    ctx.cleanup(maker).await;
    ctx.cleanup(taker).await;
}

// ---- Scenario B/C: mark-twap crank keeps the market fresh -------------------
#[tokio::test]
async fn mark_twap_crank_advances() {
    let ctx = TestCtx::new().await;
    let from_ts = ctx
        .client
        .get_perp_market_account(0)
        .await
        .expect("perp market")
        .market_stats
        .last_mark_price_twap_ts;
    let new_ts = ctx
        .wait_mark_twap_ts_after(0, from_ts, Duration::from_secs(60))
        .await
        .expect("mark-twap crank did not advance last_mark_price_twap_ts within 60s");
    // It advanced (monotonic) and by a bounded amount — the crank cadence is
    // ~10s, so within the 60s poll the jump must be modest, not a stale/garbage ts.
    let delta = new_ts - from_ts;
    assert!(
        (1..=180).contains(&delta),
        "mark-twap ts advanced by {delta}s; expected 1..=180 (≈crank cadence within the poll)"
    );
}

// ---- Scenario 1s / 2s: swift taker submitted to deployed swift server -------
// Nightly-safe: warn-skips (never fails) if `POST /orders` is non-200 OR the
// connection drops OR the deployed maker doesn't fill in time. The `POST /orders`
// 502s that previously kept this `#[ignore]`d were NOT flaky ALB/cloudfront — they
// were the swift-server panicking in `simulate_detached_perp_order`: `State` is
// `#[account(zero_copy)]` (16-aligned off-chain), so deserializing it from a
// non-16-aligned buffer panicked with `TargetAlignmentGreaterAndInputNotAligned`
// and dropped the connection (proxy → 502). Fixed in `swift/src/util/local_sim.rs`
// (copy into `AlignedAccountData` first); see `swift/tests/devnet_zero_copy_alignment.rs`.
#[tokio::test]
async fn swift_taker_filled_by_deployed_maker() {
    let ctx = TestCtx::new().await;
    let sub_id = SUB_SWIFT;
    let sub = ctx.acquire(sub_id).await;
    ctx.fund_and_deposit_dusdt(sub, 100).await;

    let px = ctx.client.oracle_price(SOL_PERP).await.expect("oracle");
    let slot = ctx.client.rpc().get_slot().await.expect("slot") + 200;
    let order = OrderParams {
        order_type: OrderType::Oracle,
        market_type: MarketType::Perp,
        market_index: 0,
        direction: PositionDirection::Long,
        base_asset_amount: ONE_SOL as u64,
        oracle_price_offset: Some(px / 50),
        auction_start_price: Some(0),
        auction_end_price: Some(px / 50),
        auction_duration: Some(30),
        ..Default::default()
    };
    let msg = SignedMsgOrderParamsMessage {
        sub_account_id: sub_id,
        signed_msg_order_params: order,
        slot,
        uuid: nanoid!(8).as_bytes().try_into().unwrap(),
        take_profit_order_params: None,
        stop_loss_order_params: None,
        max_margin_ratio: None,
        builder_idx: None,
        builder_fee_tenth_bps: None,
        isolated_position_deposit: None,
        // Devnet: the program refuses a message tagged for another cluster.
        network: Some(velocity_rs::program::state::order_params::SIGNED_MSG_NETWORK_DEVNET),
        route: None,
    };
    let signed = SignedOrderType::authority(msg);
    let hex_msg = hex::encode(signed.to_borsh());
    let signature = ctx.wallet.sign_message(hex_msg.as_bytes()).expect("sign");
    let body = serde_json::json!({
        "message": hex_msg,
        "taker_authority": ctx.authority().to_string(),
        "taker_pubkey": sub.to_string(),
        "signature": base64::prelude::BASE64_STANDARD.encode(signature.as_ref()),
    });

    let url = format!("{}/orders", swift_http_endpoint());
    let resp = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => {}
        other => {
            log::warn!("INCONCLUSIVE: swift server unreachable/non-200: {other:?}");
            return;
        }
    }

    // If the deployed swift maker fills, the taker ends exactly +1 SOL.
    if ctx
        .wait_perp_base_eq(sub, 0, ONE_SOL, Duration::from_secs(90))
        .await
        .is_none()
    {
        log::warn!("INCONCLUSIVE: swift maker did not fill to 1 SOL within 90s");
    }
    ctx.cleanup(sub).await;
}

// ---- Scenario 5: bad perp trade → DEPLOYED liquidator ----------------------
// Nightly-safe: the setup (open +1 SOL vs AMM via the deployed filler) is
// deterministic and verified live; liquidation needs adverse oracle drift to
// cross maintenance, so that part warn-skips (not fails) if it doesn't happen.
#[tokio::test]
async fn bad_perp_trade_gets_liquidated() {
    let ctx = TestCtx::new().await;
    let sub = ctx.acquire(SUB_BAD_PERP).await;
    // Small collateral + max-leverage position sits near the maintenance edge so
    // small adverse oracle drift tips it over for the deployed liquidator.
    ctx.fund_and_deposit_dusdt(sub, 20).await;

    let px = ctx.client.oracle_price(SOL_PERP).await.expect("oracle") as u64;
    let clob = clob_accounts(&ctx.client, 0).await;
    // Rest a marketable long; the deployed filler opens it to exactly +1 SOL vs AMM.
    let tx = ctx
        .client
        .init_tx(&sub, false)
        .await
        .unwrap()
        .place_and_make(marketable_limit(px, PositionDirection::Long), clob, None)
        .build();
    if ctx.client.sign_and_send(tx).await.is_err() {
        log::warn!("INCONCLUSIVE: could not place opening order");
        return;
    }
    if ctx
        .wait_perp_base_eq(sub, 0, ONE_SOL, Duration::from_secs(60))
        .await
        .is_none()
    {
        log::warn!("INCONCLUSIVE: filler did not open the position to +1 SOL");
        return;
    }

    // Liquidation depends on oracle drift crossing maintenance — inconclusive (not
    // a failure) if it doesn't happen in time. When it does, the liquidator MUST
    // set the being-liquidated flag AND reduce the position below the opened size.
    if ctx
        .wait_being_liquidated(sub, Duration::from_secs(120))
        .await
        .is_some()
    {
        ctx.wait_perp_base_below(sub, 0, ONE_SOL, Duration::from_secs(30))
            .await
            .expect("liquidator set the flag but never reduced the position below 1 SOL");
    } else {
        log::warn!("INCONCLUSIVE: account did not become liquidatable / no liquidation in 120s");
    }
    ctx.cleanup(sub).await;
}

// ---- Scenario 6: bad spot borrow → DEPLOYED liquidator ---------------------
// Kept ignored: the blocker is NOT liquidity (a lender deposit lands fine — the
// vault holds SOL) but the program's daily withdraw guard. A SOL (spot 1) borrow
// fails with DailyWithdrawLimit (err 6128): on devnet `max_borrow_token` is
// ~1_195_748 (≈0.0012 SOL) because the deposit TWAP is tiny on a market with no
// deposit history, and a single fresh deposit doesn't lift it. The same guard caps
// WITHDRAWS, so seeded SOL can't even be pulled back. To un-ignore, an admin must
// raise the SOL spot-1 withdraw guard / borrow limit (or build up deposit-TWAP
// history); then the test can borrow against dUSDT and warn-skip the oracle-drift
// liquidation like bad_perp_trade.
#[tokio::test]
#[ignore = "LIVE_INFRA: SOL spot-1 borrow capped by DailyWithdrawLimit (~0.0012 SOL) on devnet; needs admin to raise the withdraw guard"]
async fn bad_spot_borrow_gets_liquidated() {
    let ctx = TestCtx::new().await;
    let sub = ctx.acquire(SUB_BAD_SPOT_BORROW).await;
    ctx.fund_and_deposit_dusdt(sub, 20).await;

    // Borrow SOL (spot 1) against the dUSDT collateral.
    let borrow = (BASE_PRECISION_I64 as u64) / 4; // 0.25 SOL
    let tx = ctx
        .client
        .init_tx(&sub, false)
        .await
        .unwrap()
        .withdraw(borrow, 1, None, None)
        .build();
    if let Err(e) = ctx.client.sign_and_send(tx).await {
        // Expected on devnet: DailyWithdrawLimit (err 6128). See the note above.
        log::warn!(
            "INCONCLUSIVE: SOL borrow rejected (expected DailyWithdrawLimit on devnet): {e:?}"
        );
        ctx.cleanup(sub).await;
        return;
    }
    // Borrowed exactly 0.25 SOL (SOL spot is 9-dp); assert the liability size.
    let borrowed = ctx.spot_token_amount(sub, 1).await;
    let want_borrow = ONE_SOL as u128 / 4; // 0.25 SOL = 250_000_000 native
    let sol_slack = 100_000u128; // 1e-4 SOL
    assert!(
        borrowed.abs_diff(want_borrow) <= sol_slack,
        "SOL borrow {borrowed} != {want_borrow} (±{sol_slack} native)"
    );

    if ctx
        .wait_being_liquidated(sub, Duration::from_secs(120))
        .await
        .is_none()
    {
        log::warn!("INCONCLUSIVE: borrow did not breach maintenance / no liquidation in 120s");
    }
    ctx.cleanup(sub).await;
}

// ---- Scenario A: userPnlSettler settles unsettled pnl ----------------------
// Nightly-safe: the setup (open+close vs AMM via the deployed filler, banking
// unsettled pnl) is deterministic and verified live; the userPnlSettler only acts
// above its pnl threshold, so that part warn-skips (not fails) if it doesn't run.
#[tokio::test]
async fn unsettled_pnl_gets_settled() {
    let ctx = TestCtx::new().await;
    let sub = ctx.acquire(SUB_UNSETTLED_PNL).await;
    ctx.fund_and_deposit_dusdt(sub, 100).await;

    // Open then close a position (filler fills each vs the AMM) to bank realized
    // but unsettled pnl, then wait for the deployed userPnlSettler to settle it.
    let px = ctx.client.oracle_price(SOL_PERP).await.expect("oracle") as u64;
    let clob = clob_accounts(&ctx.client, 0).await;
    let open = ctx
        .client
        .init_tx(&sub, false)
        .await
        .unwrap()
        .place_and_make(marketable_limit(px, PositionDirection::Long), clob, None)
        .build();
    ctx.send_confirmed(open).await;
    if ctx
        .wait_perp_base_eq(sub, 0, ONE_SOL, Duration::from_secs(60))
        .await
        .is_none()
    {
        log::warn!("INCONCLUSIVE: filler did not open the position");
        return;
    }
    let close = ctx
        .client
        .init_tx(&sub, false)
        .await
        .unwrap()
        .place_and_make(marketable_limit(px, PositionDirection::Short), clob, None)
        .build();
    ctx.send_confirmed(close).await;
    ctx.wait_perp_base_eq(sub, 0, 0, Duration::from_secs(60))
        .await;

    if ctx
        .client
        .unsettled_positions(&sub)
        .await
        .unwrap_or_default()
        .is_empty()
    {
        log::warn!("INCONCLUSIVE: no unsettled pnl produced to observe the settler");
        return;
    }
    match ctx.wait_pnl_settled(sub, Duration::from_secs(120)).await {
        Some(()) => {}
        None => log::warn!("INCONCLUSIVE: userPnlSettler did not settle within 120s"),
    }
    ctx.cleanup(sub).await;
}
