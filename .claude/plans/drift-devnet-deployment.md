# Drift v2 Devnet Deployment Plan

## Context

Deploy the Drift Protocol v2 program to **Solana devnet** as a fresh, minimum-viable instance and then layer on LP pools, insurance-fund staking config, and admin governance (protected maker mode, IF rebalance). Quote asset will be **USDT** (not USDC), and all price oracles will use **Pyth Lazer**. Scope at launch: USDT spot (index 0) + one SOL‑PERP perp (index 0). Everything else (additional spot/perps, DEX fulfillment, referrer claims) is left for later — but the one‑time admin plumbing for IF, LP and governance is included so users can immediately stake IF, LP pools can accept constituents, and maker/rebalancer configs are in place.

Deliverable: a sequenced runbook + a deploy script (`deploy-scripts/init-devnet.ts`) that the admin wallet runs once after `anchor deploy`, producing a live, tradable devnet protocol.

## Prerequisites

- Toolchain: `rustup default stable-x86_64-apple-darwin` (Anchor 1.0 branch, never native aarch64 — zero-copy alignment).
- `bun` for SDK (`cd sdk && bun install && bun run build`).
- A funded devnet admin wallet (path via `$DEVNET_ADMIN`); this key becomes `State.admin` **immutably**.
- A devnet **USDT mint** (6 decimals, standard SPL). If no canonical devnet USDT exists, create one with `spl-token create-token --decimals 6` and mint a starting supply to the admin for seeding vaults/tests.
- Pyth Lazer SOL/USD feed ID (u32) — fetch from Pyth Lazer devnet feed registry at deploy time; not hardcoded in this repo.
- `Token Program` and `Token-2022` are already on devnet (nothing to do).

## Programs to deploy

| Program | Path | Required | Notes |
|---|---|---|---|
| `drift` | `programs/drift/` | yes | Core protocol. Program ID is declared: `dRiftyHA39MWEi3m9aunc5MzRF1JYuBsbn6VPcn33UH` (`programs/drift/src/lib.rs:70-73`). |
| `openbook_v2` | `programs/openbook_v2/` | skip | Only needed if enabling OpenBook V2 spot fulfillment (not in scope). |
| `token_faucet` | `programs/token_faucet/` | optional | Convenience for test USDT distribution on devnet. Skip unless needed. |

`programs/pyth-lazer/` is **not a deployable program** — it's a Rust library crate (`crate-type = ["lib"]`, no Anchor entrypoint) providing Lazer message types and signature-verification utilities that the drift program links against. The actual Pyth Lazer verifier program is deployed and maintained by Pyth Labs on devnet/mainnet; we consume it, we do not deploy it. No action needed for this crate at deploy time beyond it being compiled into the drift program.

Build: `bash deploy-scripts/build-devnet.sh` (runs `anchor build -- --no-default-features --features no-entrypoint`, which omits the `mainnet-beta` gate — see `programs/drift/Cargo.toml:12-23`).

Deploy (fresh, not upgrade): `anchor deploy --program-name drift --provider.cluster devnet --provider.wallet $DEVNET_ADMIN`. The existing `deploy-scripts/deploy-devnet.sh` uses `anchor upgrade` — only use that for subsequent upgrades.

After deploy: `anchor build -- --features anchor-test && cp target/idl/drift.json sdk/src/idl/drift.json` so the init script sees the latest IDL.

## Initialization sequence

Write the runbook as `deploy-scripts/init-devnet.ts` using `@drift-labs/sdk` `AdminClient`. All calls are admin-signed.

### Phase A — Global state (one‑time, must be first)

1. **`AdminClient.initialize(usdtMint, false)`** — `sdk/src/adminClient.ts:100`. Creates the `State` PDA `[b"drift_state"]` and derives `drift_signer`. Handler: `programs/drift/src/instructions/admin.rs:106` (`handle_initialize`). `quoteAssetMint` is parametric — passing the USDT mint is sufficient; no code hardcodes USDC (`admin.rs:121` just stores the mint). `State` starts with `number_of_markets = 0`, `number_of_spot_markets = 0`.

2. **`AdminClient.updatePerpAuctionDuration(10)`** — recommended default. Sets the min perp auction duration visible to keepers/fillers. `State.min_perp_auction_duration` is already 10 from `handle_initialize` (`admin.rs:120`), so this is optional but explicit.

3. **`AdminClient.initializeAmmCache()`** — `sdk/src/adminClient.ts:703`. Creates `AmmCache` PDA pre‑allocated for up to 16 perp markets. **Required before any `initializePerpMarket` call.**

### Phase B — Quote spot market (USDT @ index 0)

4. **`AdminClient.initializeSpotMarket(...)`** — `sdk/src/adminClient.ts:136`. Must be index 0. Required values:
   - `mint` = devnet USDT mint.
   - `oracle` = `PublicKey.default()`.
   - `oracleSource` = `OracleSource.QUOTE_ASSET`.
   - Weights: `initialAssetWeight=SPOT_WEIGHT_PRECISION`, `maintenanceAssetWeight=SPOT_WEIGHT_PRECISION`, `initialLiabilityWeight=SPOT_WEIGHT_PRECISION`, `maintenanceLiabilityWeight=SPOT_WEIGHT_PRECISION`.
   - Rates: `optimalUtilization=SPOT_MARKET_RATE_PRECISION/2`, `optimalRate=SPOT_MARKET_RATE_PRECISION`, `maxRate=SPOT_MARKET_RATE_PRECISION`.
   - `assetTier=COLLATERAL`, `name="USDT"`.

   Creates the `SpotMarket` PDA, the `spot_market_vault`, and the `insurance_fund_vault` (both token accounts owned by `drift_signer`). Template parameters lifted from `tests/testHelpers.ts:1145` (`initializeQuoteSpotMarket`).

### Phase C — Pyth Lazer SOL/USD oracle

5. **`AdminClient.initializePythLazerOracle(solFeedId)`** — `sdk/src/adminClient.ts:4737-4746`. Creates the `PythLazerOracle` PDA `[b"pyth_lazer", feed_id.to_le_bytes()]` (seed constant: `programs/drift/src/state/pyth_lazer_oracle.rs:5`; handler: `programs/drift/src/instructions/admin.rs:4541-4552`). Capture the returned PDA pubkey — that PDA is the `priceOracle` passed to `initializePerpMarket`.

### Phase D — SOL‑PERP (index 0)

6. **`AdminClient.initializePerpMarket(...)`** — `sdk/src/adminClient.ts:542`. Core args:
   - `marketIndex = 0`.
   - `priceOracle = <PythLazerOracle PDA from step 5>`.
   - `oracleSource = OracleSource.PYTH_LAZER`.
   - AMM seed: `baseAssetReserve = 1000 * AMM_RESERVE_PRECISION`, `quoteAssetReserve = 1000 * AMM_RESERVE_PRECISION`, `pegMultiplier = PEG_PRECISION` (tune at real deploy time against oracle price).
   - `periodicity = 3600` (funding update cadence).
   - `contractTier = SPECULATIVE`, `marginRatioInitial = 2000` (20%), `marginRatioMaintenance = 500` (5%).
   - `orderStepSize = BASE_PRECISION/10000`, `orderTickSize = PRICE_PRECISION/100000`, `minOrderSize = BASE_PRECISION/10000`.
   - `maxSpread = 142500`, `baseSpread = 0`, `curveUpdateIntensity = 0`, `ammJitIntensity = 0`.
   - `activeStatus = true`, `name = "SOL-PERP"`, `lpPoolId = 0` (default pool id — will be wired to the real LP pool in Phase F).

   Template lifted from `tests/admin.ts:99-108`.

### Phase E — Insurance fund admin config

7. **`AdminClient.initializeProtocolIfSharesTransferConfig()`** — `sdk/src/adminClient.ts:3996-4024`. Creates the global `ProtocolIfSharesTransferConfig` PDA (one‑time; governs IF share transfers). Without it, IF share transfer flows are blocked.

   *No per‑market or per‑user IF admin call is needed.* The per‑market IF vault was created in step 4 as part of `initializeSpotMarket`. `initializeInsuranceFundStake` is **user‑invoked** on first stake (`programs/drift/src/instructions/if_staker.rs:33-57`) — the deployer does not call it.

### Phase F — LP pool scaffolding

8. **`AdminClient.initializeLpPool(lpPoolId=1, minMintFee, maxAum, maxSettleQuoteAmountPerMarket, lpTokenMintKeypair)`** — `sdk/src/adminClient.ts:5262-5281`. Creates the `LPPool`, `AmmConstituentMapping`, and `ConstituentTargetBase` PDAs, plus a 6‑decimal LP token mint with authority = LP pool PDA. Handler: `programs/drift/src/instructions/lp_admin.rs:35-111`. Use `lpPoolId=1`; id `0` is the sentinel used by perp markets that are *not* in a pool.

9. **`AdminClient.initializeConstituent(lpPoolId=1, { spotMarketIndex: 0, ... })`** — `sdk/src/adminClient.ts:5355-5414`. Adds USDT (index 0) as the first constituent. Handler: `programs/drift/src/instructions/lp_admin.rs:114-211`. Set modest `swapFees`, `maxWeightDeviation`, and initial target weight 100% until more constituents are added. Creates the constituent's token vault owned by `drift_signer`.

   *Further constituents (SOL spot, etc.) are added later with the same call.* No perp‑constituent init instruction exists — perps participate via `lpPoolId` set on the perp market.

### Phase G — Governance / maker / rebalancer configs

10. **`AdminClient.initializeProtectedMakerModeConfig(maxUsers)`** — `sdk/src/adminClient.ts:4767-4803`. Admin one‑time, global. Creates `ProtectedMakerModeConfig` PDA `[b"protected_maker_mode_config"]` (struct at `programs/drift/src/instructions/admin.rs:6000-6020`). Pick a generous `maxUsers` (e.g., 200) for devnet.

11. **`AdminClient.initializeIfRebalanceConfig(params)`** — `sdk/src/adminClient.ts:4597-4627`. Per `(in_market_index, out_market_index)` pair. For devnet launch we only have USDT (index 0) so skip unless/until a second spot market is added; keep the helper stubbed and document in the script how to invoke once SOL spot lands.

*Referrer names are user‑invoked (`programs/drift/src/instructions/user.rs:292-325`) — no admin setup.*

## Necessary code changes for USDT quote asset support

Switching the deployment from the repo's current devnet **USDC @ spot market 0** assumptions to **USDT @ spot market 0** is not just an init-script concern. The on-chain program already accepts an arbitrary quote mint in `initialize`, so the core quote-asset switch does **not** require new protocol logic by itself, but the SDK and deployment config must be updated so clients/keepers resolve the correct mint, oracle PDAs, and market metadata.

### Program changes

- **No quote-asset-specific program logic change is required** for the base deployment. `handle_initialize` stores the quote mint passed by the admin, and `initializeSpotMarket(..., oracleSource = QUOTE_ASSET)` supports a USDT quote market at index 0.
- **Do not add any USDC-specific branching in program code.** If any helper script or downstream tool needs the quote mint, read it from `State` / market config rather than hardcoding USDC.
- **Separate non-quote caveat:** if the deployment still wants `ProtocolIfSharesTransferConfig` initialization in scope, that requires restoring the currently commented-out program entrypoints in `programs/drift/src/lib.rs`; this is not caused by USDT specifically, but it is a real code-level blocker for that phase.

### SDK / client config changes

1. **Introduce a deployment-specific SDK config instead of reusing the repo's current devnet defaults unchanged.**
   - Current devnet config in `sdk/src/config.ts` and `sdk/src/constants/{spotMarkets,perpMarkets}.ts` assumes the existing shared devnet deployment, including:
   - program id `dRiftyHA39MWEi3m9aunc5MzRF1JYuBsbn6VPcn33UH`
   - quote mint = canonical devnet USDC
   - pre-existing oracle PDA addresses
   - If this new deployment replaces the repo's canonical devnet environment, update those files directly.
   - If it is a parallel/custom devnet instance, create a dedicated config path (preferred) so existing devnet users do not silently switch to the new markets.

2. **Update quote spot market metadata for market index 0.**
   - In the deployment-specific spot-market config, change market 0 from `USDC` to `USDT`.
   - Set `mint` to the deployed USDT mint.
   - Set `symbol/name` to `USDT`.
   - Set the quote market's oracle metadata to match the actual on-chain initialization. If market 0 is initialized with `OracleSource.QUOTE_ASSET`, the SDK config should reflect that rather than continuing to point at the old devnet stablecoin oracle account.

3. **Update perp market oracle addresses to the newly derived Lazer PDAs.**
   - The perp market config cannot keep using the current hardcoded devnet oracle pubkeys.
   - For this deployment, `PerpMarket[0].oracle` should be the `PythLazerOracle` PDA derived from **this deployment's program id** and the configured SOL feed id.
   - The init script should write the resolved PDA(s) into an artifact that the SDK config can consume.

4. **Make the quote-mint config naming generic where practical.**
   - `sdk/src/config.ts` currently exposes `USDC_MINT_ADDRESS`, which becomes misleading once devnet quote collateral is USDT.
   - Preferred change: introduce `QUOTE_MINT_ADDRESS` (or equivalent) and migrate quote-aware consumers to use that field.
   - If renaming is too disruptive immediately, add a deployment-specific override and leave a compatibility alias, but document that the field is semantically "quote mint", not necessarily USDC.

5. **Update scripts/tests/helpers that assume "devnet quote == USDC".**
   - Any script that reads `getConfig().USDC_MINT_ADDRESS`, `DevnetSpotMarkets[0]`, or hardcodes canonical devnet USDC should be switched to the deployment-specific quote market config.
   - Operator-facing env vars in the new deployment scripts may remain `USDT_MINT`, but if this is expected to generalize later, prefer a neutral name like `QUOTE_MINT`.

6. **Emit a generated deployment artifact for downstream consumers.**
   - `deploy-scripts/init-devnet.ts` should not only write a receipt of tx signatures/PDAs; it should also write the exact market/oracle config needed by keepers, bots, and SDK consumers.
   - Minimum contents: `programId`, `quoteMint`, `spotMarket0`, `perpMarket0`, `pythLazerOraclePubkeys`, and any LP pool ids created during init.
   - This avoids hand-copying PDAs from logs into `sdk/src/constants/*.ts`.

## Files to create / modify

| Path | Action | Purpose |
|---|---|---|
| `deploy-scripts/init-devnet.ts` | **create** | TypeScript runbook executing Phases A–G above via `AdminClient`. Idempotency: wrap each step in a `try/catch` that checks for "already initialized" (`adminClient.ts:107-109` pattern) and skips. Writes a JSON receipt (`deploy-scripts/out/devnet-deployment.json`) with every created PDA + tx signature. |
| `deploy-scripts/init-devnet.sh` | **create** | Thin shell wrapper: `bun run deploy-scripts/init-devnet.ts`. |
| `deploy-scripts/README.md` | **create (short)** | Minimal operator runbook pointing at build → deploy → init scripts, env vars (`$DEVNET_ADMIN`, `$USDT_MINT`, `$SOL_LAZER_FEED_ID`), and the receipt path. Kept short per user preference. |
| `sdk/src/config.ts` | **modify** | Add a deployment-specific config/override path for the new devnet instance and stop relying on the existing devnet USDC assumptions. |
| `sdk/src/constants/spotMarkets.ts` | **modify** | Define the quote spot market for the new deployment as **USDT @ index 0** with the correct mint/oracle metadata. |
| `sdk/src/constants/perpMarkets.ts` | **modify** | Point `SOL-PERP` at the newly created `PythLazerOracle` PDA for this deployment instead of the current shared-devnet oracle pubkey. |

No quote-asset-specific program (Rust) changes should be necessary. SDK/config changes are required. If `ProtocolIfSharesTransferConfig` remains in scope, that specific instruction surface may require a program/IDL change before this runbook can execute end-to-end.

## Reusable references

- `tests/testHelpers.ts:1145` (`initializeQuoteSpotMarket`) — canonical spot-market-0 args.
- `tests/testHelpers.ts:1184` (`initializeSolSpotMarket`) — template if/when adding SOL spot.
- `tests/admin.ts:90-110` — full minimum init sequence (state → spot0 → amm cache → perp0), directly portable to `init-devnet.ts`.
- `sdk/src/addresses/pda.ts` — PDA derivation helpers (`getDriftStateAccountPublicKey`, `getSpotMarketPublicKey`, `getPerpMarketPublicKey`, `getPythLazerOraclePublicKey`, `getLpPoolPublicKey`, `getConstituentPublicKey`).
- `sdk/src/constants/spotMarkets.ts:30-50` — pattern for the per-cluster market config array; after init, add a "devnet" entry here (or a parallel file) so downstream SDK consumers resolve markets by index.

## Verification

1. **Programs on chain**: `solana program show dRiftyHA39MWEi3m9aunc5MzRF1JYuBsbn6VPcn33UH --url devnet` returns a valid program.
2. **State account**: `AdminClient.getStateAccount()` returns `admin == $DEVNET_ADMIN`, `number_of_spot_markets == 1`, `number_of_markets == 1`.
3. **Spot market**: fetch `SpotMarket[0]`, assert `mint == usdtMint`, `oracle_source == QUOTE_ASSET`, and the spot vault/IF vault are Token accounts owned by `drift_signer`.
4. **Perp market**: fetch `PerpMarket[0]`, assert `amm.oracle == <lazer PDA>` and `oracle_source == PYTH_LAZER`; `AmmCache` has a non‑zero slot at index 0.
5. **LP**: `LPPool` PDA for id `1` exists; `Constituent` for (pool=1, spot=0) exists; the LP token mint's authority is the LP pool PDA.
6. **Governance PDAs**: `ProtectedMakerModeConfig` and `ProtocolIfSharesTransferConfig` fetchable (non-null, correct owner = drift program).
7. **End-to-end smoke**:
   - Run `ts-mocha -t 300000 ./tests/admin.ts` against the devnet config (points at the deployed program) to exercise the same init sequence in a known-good way; skip if the test infra doesn't support a remote cluster, in which case run locally against the same built `drift.so`.
   - Have a second test wallet call `DriftClient.initializeUserAccount()`, `deposit(usdtAmount, 0)`, then `placePerpOrder({ marketIndex: 0, baseAssetAmount: ..., ... })` with a keeper loop to fill. Observing a filled order on devnet is the real green light.
8. **Rollback**: The initial program deploy retains the buffer; if Phase A..G errors, the program is still upgradable via `deploy-scripts/deploy-devnet.sh`. State/market accounts, once created, **cannot be cleanly deleted** — ensure USDT mint, admin wallet, and Lazer feed id are correct *before* running Phase A.
