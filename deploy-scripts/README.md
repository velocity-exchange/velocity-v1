# deploy-scripts

Devnet deployment scripts for the drift program.

## Runbook

1. **Build** (x86_64 toolchain; see root `CLAUDE.md`):
   ```
   bash deploy-scripts/build-devnet.sh
   ```
2. **Deploy** (first time — fresh program): use `anchor deploy` with the devnet cluster and your admin keypair; for subsequent upgrades use `bash deploy-scripts/deploy-devnet.sh` (which runs `anchor upgrade`).
3. **Sync IDL** into the SDK so the init script sees current instruction shapes:
   ```
   anchor build -- --features anchor-test && cp target/idl/drift.json sdk/src/idl/drift.json
   ```
4. **Initialize on-chain state** (phases A–G in one pass; idempotent):
   ```
   DEVNET_ADMIN=/path/to/admin.json \
   USDT_MINT=<usdt mint pubkey> \
   SOL_LAZER_FEED_ID=<u32 feed id> \
   bash deploy-scripts/init-devnet.sh
   ```
   Writes a receipt to `deploy-scripts/out/devnet-deployment.json` with every created PDA and tx signature.
5. **Patch SDK constants** with values from the receipt — these ship as `PublicKey.default` placeholders until the deployment exists:
   - `sdk/src/config.ts` → `configs.devnet.QUOTE_MINT_ADDRESS` ← `usdtMint`
   - `sdk/src/constants/spotMarkets.ts` → `DevnetSpotMarkets[0].mint` ← `usdtMint`
   - `sdk/src/constants/perpMarkets.ts` → `DevnetPerpMarkets[0].oracle` ← `pythLazerOracles[<feedId>].pubkey`

## Env vars

Required:
- `DEVNET_ADMIN` — path to admin keypair file; becomes `State.admin` **immutably**.
- `USDT_MINT` — devnet USDT SPL mint (6 decimals).
- `SOL_LAZER_FEED_ID` — Pyth Lazer u32 feed id for SOL/USD.

Optional:
- `RPC_URL` (default `https://api.devnet.solana.com`)
- `LP_POOL_ID` (default `1`; id `0` is the "not in a pool" sentinel)
- `LP_MAX_AUM` (default `1_000_000`, multiplied by `QUOTE_PRECISION`)
- `PROTECTED_MAKER_MAX_USERS` (default `200`)
- `RECEIPT_PATH` (default `deploy-scripts/out/devnet-deployment.json`)
- `NON_INTERACTIVE=1` (or `YES=1`) — skip every confirmation prompt; useful for CI

By default the script pauses before pre-flight and before each phase (A–G), printing the resolved inputs (mint, oracle, LP id, etc.) and waiting for `y` to continue. Pre-flight verifies the program is deployed/executable and the USDT mint is a real token mint before any state is touched.

## What gets initialized

See `.claude/plans/drift-devnet-deployment.md` for the authoritative plan. Summary:

- **A** global `State` + `AmmCache`
- **B** USDT spot market at index 0
- **C** Pyth Lazer SOL/USD oracle
- **D** SOL-PERP at index 0
- **E** `ProtocolIfSharesTransferConfig`
- **F** LP pool + USDT constituent
- **G** `ProtectedMakerModeConfig`

Skipped by design (left for later):
- Additional spot markets (SOL, BTC, …)
- `initializeIfRebalanceConfig` (needs ≥2 spot markets)
- Spot DEX fulfillment (OpenBook V2 / Phoenix / Serum)
- User-invoked flows: `initializeInsuranceFundStake`, `initializeReferrerName`

## Verification

After the script finishes, confirm:

```
solana program show dRiftyHA39MWEi3m9aunc5MzRF1JYuBsbn6VPcn33UH --url devnet
```

Then inspect the receipt for every PDA and cross-check with `solana account <pubkey> --url devnet`.

End-to-end smoke: use a second wallet to call `DriftClient.initializeUserAccount()` → `deposit(usdtAmount, 0)` → `placePerpOrder({ marketIndex: 0, ... })` and observe a keeper fill.
