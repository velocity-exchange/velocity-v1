# Devnet E2E — CI reliability & orchestration handoff

Status for `tests/devnet_e2e.rs` (gated `--features rpc_tests`). This doc explains
**which scenarios are deterministic, which depend on uncontrollable market/infra
state, and exactly what it would take to make each of the latter deterministic.**

It supersedes the earlier "taker-vs-AMM" handoff: that investigation is resolved
(see "Root cause" below) — the blocker was **auction sanitization + TWAP lag**, NOT
the rust-filler. Two earlier hypotheses are now **refuted**:

- ~~(A) the rust-filler's DLOB doesn't update from gRPC streaming~~ — FALSE. Run
  locally in dry-run, the filler streams `User` accounts over Helius LaserStream
  gRPC and the DLOB sees resting limit makers AND a lone taker auction order
  (`taker_bids=1, kind=Market`) within a slot of placement.
- ~~(B) `find_crosses_for_auctions` needs resting makers to produce a lone-taker
  vAMM cross~~ — FALSE. `MakerCrosses::is_empty()` is `orders.is_empty() &&
  !has_vamm_cross`, so a lone taker with `has_vamm_cross` is kept.

## How to run

```bash
cd rust
set -a && . ./keep-rs/.env && set +a            # BOT_PRIVATE_KEY etc.
export TEST_PRIVATE_KEY="$BOT_PRIVATE_KEY" TEST_DEVNET_RPC_ENDPOINT="$RPC_URL"
# nightly path (non-ignored only):
cargo test -p velocity-rs --test devnet_e2e --features rpc_tests -- --test-threads=1 --nocapture
# full path (also the #[ignore]'d scenarios):
cargo test -p velocity-rs --test devnet_e2e --features rpc_tests -- --include-ignored --test-threads=1 --nocapture
```

Devnet: program `vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P`. Markets: SOL-PERP =
perp 0, dUSDT = spot 0 (6dp), SOL = spot 1 (9dp).

## Provisioning the deployed MM / taker bots

The `rust-quoter-bot` (MM) and `rust-taker-bot` run on subaccounts **1** and **2**
of the filler authority (sub 0 = filler). Before they can quote/trade they need
those subaccounts initialized and funded with dUSDT collateral. The one-shot
`fund_mm_taker_subaccounts` (`#[ignore]`, in `devnet_e2e.rs`) does init + faucet +
deposit for sub 1 and sub 2 via `ensure_subaccount` + `fund_and_deposit_dusdt`.

It is a plain set of devnet txs signed by `TEST_PRIVATE_KEY` — run it **with the
filler key**, from anywhere with a devnet RPC. It does **not** need cluster access
(the keep-rs pod only ships the `keeprs` binary, which can `--init-user` but cannot
faucet/deposit). The only real prerequisite is the **filler private key**
(in AWS Secrets Manager `velocity/non-prod/master-secret-store`, key
`FILLER_PRIVATE_KEY` — needs Secrets Manager read, not kube access):

```bash
cd rust
TEST_PRIVATE_KEY="$FILLER_PRIVATE_KEY" TEST_DEVNET_RPC_ENDPOINT="<devnet rpc>" \
FUND_DUSDT=5000 \
  cargo test -p velocity-rs --test devnet_e2e --features rpc_tests \
  fund_mm_taker_subaccounts -- --ignored --nocapture
```

## CI topology (why "unreliable" matters per job)

- **`rust-live-tests`** (`.github/workflows/main.yml`) — nightly cron + manual
  dispatch; runs `--features rpc_tests` **without** `--include-ignored`. So it runs
  every **non-`#[ignore]`** test. Non-gating: never blocks PRs.
- **`devnet-e2e`** — manual dispatch only; runs the devnet suite **with**
  `--include-ignored` (everything).
- PRs run neither. The suite only has a funded **`TEST_PRIVATE_KEY`** (a normal
  user, **not** the program admin and **not** an oracle authority), so a CI run
  **cannot set any market/admin/oracle state**. That single fact is the root of
  every "unreliable" scenario below.

## The design contract

Every scenario whose *outcome* depends on a deployed bot or on market state we
can't set is written to be **sound, not flaky**: it asserts the parts it CAN
control (the setup, and any program-level invariant) and treats "the bot/market
didn't cooperate in time" as **`log::warn!("INCONCLUSIVE…")` + pass**, never a hard
failure or a silent multi-minute timeout. So in CI these scenarios are *green but
may verify only their setup* on a given night — read the logs, don't trust a bare
"ok".

`#[ignore]` is reserved for the stricter case where the **setup itself cannot be
established on devnet right now** (so the test can't even reach its warn-skip).

## Reliable scenarios (deterministic — trust these)

| Test | Why deterministic |
|---|---|
| `deposit_into_spot_market` | A pure user action, with an exact balance assertion |
| `withdraw_from_spot_market` | A pure user action, with an exact balance assertion |
| `dlob_maker_taker_filled_by_filler` | A matched maker and taker. The deployed filler reliably matches crossing limit orders. The limit path is not sanitized out, unlike a market-order auction. See scenario 1 |
| `mark_twap_crank_advances` | The mark-twap crank runs about every 10 seconds. The test asserts a bounded timestamp advance |
| `taker_fills_against_amm` (the **assertions**) | The sanitization regression locks always run, and only the *fill* is gated. See the next table. They are deterministic **only because the requested band is built from the live baseline**. See the note below |

## Unreliable scenarios: what each depends on, and how to orchestrate it

All of these need market/admin/oracle/bot state a `TEST_PRIVATE_KEY`-only CI run
can't set. "Orchestrate" = what it would take to make the outcome deterministic.

### 1. `taker_fills_against_amm` — *the fill* (lone taker vs AMM, low-risk auction route)
- **Depends on:** the **sanitized** auction price out-pricing the live `vamm_ask`.
- **Why nobody can control it:** placement rewrites a market order's auction params in
  `update_perp_auction_params_market_and_oracle_orders` and clamps them to
  `get_perp_baseline_start_end_price_offset(market, dir, 2)`. The baseline end price comes
  from the **bid and ask price TWAPs**, an EWMA over the roughly one-hour funding period in
  `calculate_new_twap`. On a low-volume devnet those TWAPs **lag the live oracle**, while
  `vamm_ask` tracks the **live** reserve price. The sanitized end, around oracle plus 0.5%,
  then sits *below* `vamm_ask`, around oracle plus 0.73%, so the cross never happens. One
  observation: `start == end == oracle + 0.50%` against `vamm_ask = oracle + 0.73%`.
- **The baseline is not stable. Never hardcode a band against it.** The same formula also
  produces a baseline end around **oracle plus 21%**. The end is
  `(last_ask_price_twap - oracle_twap) + baseline_end_price_buffer`, and the buffer is
  `2 * max(mark_std, oracle_std, amm_spread * twap)`, clamped by the contract tier in
  `get_auction_end_min_max_divisors`, which is 1% to 10% of price for Speculative. Devnet
  market 0 has sat with its mark TWAP about 10% above the oracle TWAP and `amm.long_spread`
  around 11%, which pins the buffer to the 10% ceiling. An earlier version of the test
  hardcoded a band of +2% to +15% as "aggressive". With the baseline end at +19.6% the top
  of that band was *milder* than the baseline, so only the start was clamped and the spread
  assertion failed every night from 17 July, with the misleading message "sanitization logic
  changed". The test now reads the market first and offsets past the live baseline by the
  tier's buffer ceiling plus 4% on both ends, so the request still clears the threshold when
  the baseline rises before the placement slot.
- **A read that must be ordered against a transaction cannot come from the cache.**
  `TestCtx` subscribes to all three markets, so `get_perp_market_account` serves the
  websocket cache, and that cache's slot has no ordering guarantee against a just-confirmed
  transaction. The baseline bracket needs one read strictly before placement and one
  strictly after, so it uses `fetch_perp_market`, a raw `rpc().get_account_data` plus
  `try_deser_zero_copy`. It does not use anchor's `try_deserialize`, which panics on
  16-aligned zero-copy structs off-chain. A cache read is fine everywhere the ordering does
  not matter.
- **To orchestrate, do any one of:**
  1. **Warm the TWAPs** to the live price first. Loop matched maker and taker fills through
     the reliable `dlob_maker_taker_filled_by_filler` path at about the oracle price. This
     is slow. The EWMA closes only about 0.3% of the gap per 10-second crank, so it needs 5
     to 7 minutes of sustained flow, and it is not deterministic on a quiet night.
  2. **Reset** `last_bid_price_twap` and `last_ask_price_twap` to the oracle price. The
     admin path exists in `instructions/admin.rs`. It is instant and needs the **admin
     key**.
  3. **Shrink** `amm.long_spread` so `vamm_ask` drops below the sanitized end. This needs
     the admin key.
- Today the test gates on `sanitized_end > vamm_ask`. When that is false it warn-skips with
  the exact numbers, and it does not wait out a 120-second timeout.

### 2. `taker_fills_against_amm_via_jit`: a lone taker against the AMM, JIT route
- **Depends on:** `amm_jit_intensity > 0` **and** the AMM holding inventory on the side a
  taker would relieve, meaning `base_asset_amount_with_amm`, which is the net user position,
  beyond plus or minus `order_step_size`.
- **Why nobody can control it:** init-devnet sets `jit_intensity`, currently to 100, but the
  AMM is **flat**, with `base_asset_amount_with_amm == 0` and no flow, so there is nothing
  to offload through JIT. Seeding inventory needs prior taker flow against the AMM, which
  scenario 1 already gates. The two block each other.
- **To orchestrate, do either:**
  1. **Set** `base_asset_amount_with_amm` through the admin path, or run a quoter and
     sustained flow to build it, so the AMM is net long or net short past
     `order_step_size`.
  2. Have a **second-authority** account open a position the AMM must take. This needs a
     funded key other than `TEST_PRIVATE_KEY`, to avoid the duplicate `UserStats` problem.
- Today the test reads the live inventory, picks the direction the AMM would offload, and
  fills through `place_and_take` in the same transaction. It warn-skips when the AMM is flat
  or JIT is off.

### 3. `bad_perp_trade_gets_liquidated`: the deployed liquidator
- **Depends on:** adverse **oracle drift** moving SOL far enough to push the position past
  maintenance within the timeout, **and** a running liquidator.
- **Why nobody can control it:** the oracle is external, the program refuses to open an
  already-underwater position, and CI cannot move the price. The setup is fine, because the
  +1 SOL position opens reliably through the filler.
- **To orchestrate, do either:**
  1. Use a **controllable or mock oracle** on devnet and push an adverse price through the
     oracle authority.
  2. **Raise** the market's `margin_ratio_maintenance` so the existing leverage breaches
     immediately, then let the deployed liquidator act.

  Either one needs an admin or oracle authority and a running liquidator.

### 4. `bad_spot_borrow_gets_liquidated`: the deployed spot liquidator, `#[ignore]`
- **Depends on:** the daily withdraw guard **allowing** a meaningful SOL borrow on spot 1,
  then a maintenance breach. The maintenance breach hits the same oracle-drift problem as
  scenario 3.
- **Why nobody can control it:** the blocker is not liquidity. A live check seeded the vault
  from a lender, the deposit landed, and the vault held 4 SOL. The borrow still failed with
  **`DailyWithdrawLimit`, error 6128**, because `max_borrow_token` was about `1_195_748`,
  which is roughly 0.0012 SOL, against a 1 SOL attempt. The cap comes from the deposit TWAP,
  which is tiny on a market with no deposit history, and one fresh deposit does not lift it.
  That is the same TWAP-lag shape as scenario 1. The **same guard caps withdrawals**, so the
  seeded SOL cannot be pulled back either. Seeding liquidity is the wrong lever and only
  locks SOL.
- **To orchestrate:** an **admin must raise the SOL spot-1 withdraw guard and borrow
  limit**, or the deposit-TWAP history must build up over time. Then borrow against dUSDT
  collateral and warn-skip the oracle-drift liquidation as scenario 3 does. Cleanup also
  needs a repay step, because the borrowed SOL cannot be returned without acquiring SOL, and
  `cleanup` only cancels orders.
- **Note:** that check left about 4 SOL of the test authority's own SOL deposited in spot 1.
  It sits behind the same withdraw guard and is recoverable only once the guard is raised.

### 5. `unsettled_pnl_gets_settled`: the deployed userPnlSettler
- **Depends on:** the deployed **userPnlSettler being up** and the banked PnL exceeding
  **its settle threshold**.
- **Why nobody can control it:** external bot liveness, and an off-chain threshold nobody
  here controls. The setup is fine, because an open and close through the filler reliably
  banks unsettled PnL.
- **To orchestrate, do either:**
  1. Size the position so the realized PnL clears the settler's threshold. This requires
     knowing or controlling that threshold.
  2. Drive `settle_pnl` **from the test**, which is deterministic. It then verifies this
     crank rather than the deployed settler.

### 6. `swift_taker_filled_by_deployed_maker`: the swift server and a deployed maker, `#[ignore]`
- **Depends on:** the swift **HTTP order server being reachable**, and a deployed swift
  maker filling.
- **Why nobody can control it:** `swift.master.velocity.exchange/orders` returns an
  intermittent **502** from the ALB. One check saw a 422, meaning the server was up, and a
  502 minutes apart. This is server-side reliability. The host is correct, the swapped
  `master.swift.…` does not resolve, and the websocket feed on the right host works for the
  filler. Hence the `#[ignore]`.
- **To orchestrate:** stabilize the swift HTTP backend, which is an ops task, and run a
  swift maker. The alternative is to weaken the test to "POST accepted with a 2xx" without
  asserting a fill.

## What deterministic orchestration needs, and CI does not have

The CI run holds only a funded `TEST_PRIVATE_KEY`. Making the scenarios above deterministic
needs one or more of:

- **the program admin key**, for an oracle push, margin, spread, jit-intensity,
  borrow-enable or TWAP reset;
- **a controllable devnet oracle**;
- **test-pinned bots** for the maker, liquidator and settler roles, rather than the ambient
  production bots;
- **a second funded authority**, to avoid a duplicate `UserStats` when one side fills
  another;
- **TWAP warm-up time**.

Until some of those exist for the end-to-end environment, the warn-skip contract above is
the right design.

## Cleanup and loose ends
- `keep-rs/.env` runs the filler locally in dry-run for debugging, with `MAINNET=false`,
  `DRY_RUN=true`, Helius LaserStream gRPC, Triton RPC, and
  `SWIFT_WS_URL=wss://swift.master.velocity.exchange`. It is gitignored and not committed.
- After a devnet state wipe or reset, re-run keep-rs `--init-user` so the filler's `User`
  subaccount exists.
- Infra asks blocking the last two `#[ignore]`s: (a) stabilize the swift HTTP
  backend, (b) raise the SOL spot-1 daily withdraw guard / borrow limit (currently
  caps borrows to ~0.0012 SOL — liquidity is fine, the guard is the blocker).
