# @velocity-exchange/sdk

## 0.6.0

### Minor Changes

- [#216](https://github.com/velocity-exchange/velocity-v1/pull/216) [`7b44bb0`](https://github.com/velocity-exchange/velocity-v1/commit/7b44bb0832e3eb72b2cd31d01d3c7d60c9794dab) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Point jit-proxy at Velocity's own program deployment `J1TPRoXCtGuMcWiWFE6RB9eZU8U35PBMETCwNQLCNPhQ` (devnet & mainnet), replacing Drift's upstream `J1TnP8zvVxbtF5KFp5xRmWuvG9McnhzmBd9XGfCyuxFP`: the jit-proxy IDL/types are regenerated with the new address and the SDK config presets' `JIT_PROXY_PROGRAM_ID` now resolve to it. Also fixes `JitProxyClient` deriving the builder-order `REV_ESCROW` PDA under the jit-proxy program id instead of the velocity program id (the account passed for `hasBuilder` orders was wrong), and stops passing `velocityProgram` explicitly now that the IDL pins its address.

- [#220](https://github.com/velocity-exchange/velocity-v1/pull/220) [`155ed06`](https://github.com/velocity-exchange/velocity-v1/commit/155ed0618d7012b9305f4c84786c4d4e96d86be8) Thanks [@0xahzam](https://github.com/0xahzam)! - Per-user equity floor: new warm-admin instruction `update_user_equity_floor` sets `User.equityFloor` (QUOTE_PRECISION), a minimum cross-margin total collateral below which the program rejects risk-increasing order placement and fills, withdrawals, and transfers out of the account with `EquityBelowFloor` (6358); reduce-only activity stays allowed and 0 disables. `transferDepositByDelegate` gains an `equityFloorDelta` argument (instruction signature change) that atomically moves floor along with funds between same-authority subaccounts, preserving the sum of floors (`InvalidEquityFloorTransfer`, 6359). An authority-wide breaker escalates the freeze: the permissionless `trip_equity_floor_breaker` proves one subaccount below its floor and sets `UserStats.equityBreakerTripped`, freezing all of the authority's subaccounts until the warm-admin `reset_equity_floor_breaker` clears it. SDK adds `AdminClient.updateUserEquityFloor` / `getUpdateUserEquityFloorIx` / `resetEquityFloorBreaker`, `VelocityClient.tripEquityFloorBreaker`, `UserAccount.equityFloor`, `UserStatsAccount.equityBreakerTripped`, `User.isBelowEquityFloor` / `getEquityAboveFloor`, floor-aware `getWithdrawalLimit`, and an `'auto'` floor-delta mode on `transferDepositByDelegate` (quote market) that computes the minimal floor to carry. Admin CLI adds `velocity-admin user reset-equity-breaker <userStats>`. Admin CLI adds `velocity-admin user set-equity-floor <user> <floor>`.

### Patch Changes

- [#213](https://github.com/velocity-exchange/velocity-v1/pull/213) [`00decfd`](https://github.com/velocity-exchange/velocity-v1/commit/00decfd93fff5668779255288a0f61242be99d07) Thanks [@0xahzam](https://github.com/0xahzam)! - `updatePerpBidAskTwap` no longer updates the funding rate as a side effect.

  The `update_perp_bid_ask_twap` program instruction previously refreshed the mark-price TWAP from caller-supplied DLOB depth and then applied the funding rate in the same instruction, letting the just-written TWAP feed funding at zero elapsed time. Funding is now decoupled: it runs only via the dedicated `update_funding_rate` crank (and on fills). Callers of `velocityClient.updatePerpBidAskTwap` / `getUpdatePerpBidAskTwapIx` that relied on the funding side effect must call `getUpdateFundingRateIx` separately.

  Alongside this, two program-side hardening changes affect callers: the oracle-divergence filter used by the crank is now symmetric (DLOB levels are kept only within ±15% of the oracle on both sides), and `keeper_stats` must belong to the signing authority (`has_one`), so a caller can no longer pass a third party's staked `UserStats` to satisfy the insurance-fund stake gate. The SDK already passes the caller's own stats, so the normal happy path is unaffected.

- [#206](https://github.com/velocity-exchange/velocity-v1/pull/206) [`c288314`](https://github.com/velocity-exchange/velocity-v1/commit/c2883143d813a5c608354d64c3d1a5b825c4f398) Thanks [@jordy25519](https://github.com/jordy25519)! - Fix two `PythLazerSubscriber` reliability bugs:

  - **Register the message listener once per client, not once per feed chunk.** The SDK's `addMessageListener` is global to the client (it fires for every message, not scoped to a subscription), so registering it inside the per-chunk subscribe loop meant every incoming message was processed once per chunk — K× redundant map writes and K× resubscribe-timer churn for K subscription chunks. The stored prices were already idempotent so there is no behavior change to reported prices; this removes the wasted per-message work.
  - **Subscribe via `subscribe()` instead of `send()`.** `send()` fires the subscription frame once and is never replayed, so after the first heartbeat-timeout socket reconnect the connection streamed nothing and only recovered via the coarse watchdog (which tears down the whole client and reopens all connections). `subscribe()` registers the request in the pool so `ResilientWebSocket` replays it on every reconnect, recovering in place with no connection churn.

  Affects every consumer of `PythLazerSubscriber` (filler, pyth-lazer cranker; the maker bid/ask TWAP crank and multithreaded filler use a separate copy in keeper-bots-v2 that already used `subscribe()` and got the same single-listener fix).

- [#210](https://github.com/velocity-exchange/velocity-v1/pull/210) [`61cbeb2`](https://github.com/velocity-exchange/velocity-v1/commit/61cbeb2f70dcd3f6d4fab1209962132dea9d60fe) Thanks [@0xahzam](https://github.com/0xahzam)! - Point mainnet-beta `MARKET_LOOKUP_TABLE(S)` at the relaunch lookup table `4E971nER9Jn4JjT8mKEX1nvkfg8Qycp7zNEcCq2nT8ZY` (state, signer, spot markets 0-1 with oracles/mints/vaults/IF vaults, perp markets 0-3 with oracles, token/ATA/system programs). Removes the two stale pre-relaunch tables, whose addresses no longer match any deployed account.

- [#216](https://github.com/velocity-exchange/velocity-v1/pull/216) [`7b44bb0`](https://github.com/velocity-exchange/velocity-v1/commit/7b44bb0832e3eb72b2cd31d01d3c7d60c9794dab) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Publish only the built `lib/` output (adds a `files` field, shrinking the npm tarball from ~14 MB unpacked to the compiled artifacts), widen the `engines` constraint from `^24.0.0` to `>=20` so Node 20/22 LTS consumers install without engine errors, and point the `repository` field at the public https URL.

## 0.5.0

### Minor Changes

- [#205](https://github.com/velocity-exchange/velocity-v1/pull/205) [`9854dfa`](https://github.com/velocity-exchange/velocity-v1/commit/9854dfa1c915938fe08495568f262285ffb6c933) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `user deposit`, `user withdraw`, and `if stake` commands to the admin CLI, usable directly or through a Squads V4 multisig (`--multisig` defaults the authority to the vault 0 PDA so the proposal executes with the vault as signer). SDK: `getWithdrawIx`, `getInitializeInsuranceFundStakeIx`, and `getAddInsuranceFundStakeIx` now accept an optional `overrides.authority`, matching `getDepositInstruction`, so instructions can be built for an authority other than the wallet (e.g. a multisig vault PDA).

### Patch Changes

- [`6d58632`](https://github.com/velocity-exchange/velocity-v1/commit/6d58632540814c739fc3848e4110c2a24547722b) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Fix `OrderSubscriber.fetch()` leaving `mostRecentSlot` (and therefore `getSlot()`) stuck at `0` when a `getProgramAccounts` snapshot matches zero accounts (e.g. no users currently have open orders). The RPC response's slot is now stamped unconditionally, not only inside the per-account loop.

- [#201](https://github.com/velocity-exchange/velocity-v1/pull/201) [`3042967`](https://github.com/velocity-exchange/velocity-v1/commit/304296799e8d5b8a6525cb18cfd8398637c00521) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `IsolatedPositionDisabled` (6357) error to the IDL. Mainnet program builds now compile out the isolated-position and VLP hedge instruction surface pending audit (`isolated-position` / `vlp-hedge` cargo features); devnet and test builds keep both enabled.

## 0.4.0

### Minor Changes

- [#197](https://github.com/velocity-exchange/velocity-v1/pull/197) [`900c07d`](https://github.com/velocity-exchange/velocity-v1/commit/900c07d9da7e106c82fbe65b3d92226d090bdee9) Thanks [@jordy25519](https://github.com/jordy25519)! - Attach the taker's `RevenueShareEscrow` on perp fills for referred takers. `getFillPerpOrderIx` gains an optional `takerIsReferred` flag; when set (or when the order carries a builder code) the deterministic escrow PDA is added to the fill. This mirrors the on-chain fill gate, which reads `UserStats.referrerStatus` (the `BuilderReferral` bit), so referred takers' fills no longer revert with `UnableToLoadRevenueShareAccount`. `ReferrerMap` gains `isBuilderReferral(authority)` and `mustGetIsBuilderReferral(authority)`, sourcing the bit from the UserStats fetch it already performs.

- [#189](https://github.com/velocity-exchange/velocity-v1/pull/189) [`8df28ac`](https://github.com/velocity-exchange/velocity-v1/commit/8df28ac6d113760ae4a8cdff4ad438cb25efce2c) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Program↔SDK parity fixes from the 2026-07-02 audit: renamed deprecated Switchboard
  OracleSource keys to match the IDL (fixes a decode crash on affected markets), applied
  the $100 initial-margin unrealized-PnL cap, standardized auction/limit prices to order
  tick size across the DLOB, isolated-position handling in bankruptcy/liquidation math,
  corrected MM-oracle validity gating, referrer_status memcmp offset, PerpOperation and
  OrderBitFlag bit values, wired five missing event records into EventSubscriber, fixed
  withdraw-limit divisors, multi-pool margin segregation, referee/builder fee estimation,
  and added AdminClient.updatePauseAdmin plus admin CLI commands for pause-admin rotation
  and fee-pool transfers. Also fixed withdrawFromIsolatedPerpPosition's withdraw-all path:
  it substituted the MIN_I64 sentinel into the instruction's unsigned u64 amount (serializing
  as 2^63, so full withdrawals always failed on-chain with InsufficientCollateral); it now
  clamps the request to the position's deposit plus claimable PnL.

  Follow-up completeness fixes: DLOBSubscriber.getL2/getL3 (and the dlob-server publisher) now
  thread orderTickSize so the public book view is tick-standardized like on-chain; added
  hasIsolatedMarginBankrupt and wired isolated-only bankruptcy detection into keeper resolution
  (isIsolatedPositionBankrupt now guards against non-isolated indices); getMarketFees applies the
  referee discount and calculateFeeForQuoteAmount accepts builder params so both public fee-prediction
  entry points match on-chain; and isFallbackAvailableLiquiditySource now fully mirrors
  amm_fill_gates_ok, adding the market-drawdown and MM-vs-exchange oracle volatility gates.

  Low-risk parity follow-ups: MarginCategory now includes 'Fill' as a single shared type, handled
  across perp margin ratio / unrealized-asset-weight and spot asset/liability weights (the
  integer-averaged midpoint of initial and maintenance, mirroring get_margin_ratio /
  get_asset_weight / get_liability_weight) instead of throwing or returning undefined; the
  worst-tier taker-fee estimate in calculateEntriesEffectOnFreeCollateral now ceil-divides to match
  calculate_taker_fee; MM-oracle validity is computed with the raw exchange confidence (matching
  get_mm_oracle_price_data) while the returned MM price keeps its diff-adjusted confidence; and
  corrected the OracleSourceNum doc (it is an SDK-internal oracle-id encoding, not the on-chain
  Borsh discriminant).

  Visible/breaking API changes in this release: `OracleSource.SWITCHBOARD` /
  `OracleSource.SWITCHBOARD_ON_DEMAND` (and the corresponding `OracleSourceNum` entries) are renamed
  to `DEPRECATED_SWITCHBOARD` / `DEPRECATED_SWITCHBOARD_ON_DEMAND` with no aliases kept for the old
  names; `ContractType.FUTURE` is renamed to `DEPRECATED_FUTURE`; and `FeatureBitFlags.BUILDER_REFERRAL`
  is removed outright, since no such on-chain flag exists. Separately, `getLimitPrice` gained a new
  optional trailing `tickSize` parameter — the existing `fallbackPrice` parameter stays in its original
  4th position, so old 4-argument call sites keep working unchanged.

### Patch Changes

- [#199](https://github.com/velocity-exchange/velocity-v1/pull/199) [`dff8a47`](https://github.com/velocity-exchange/velocity-v1/commit/dff8a4754f6b736fed330b2ab4fa5685db4f8159) Thanks [@jordy25519](https://github.com/jordy25519)! - Add BTC-PERP (index 1) and ETH-PERP (index 2) to `DevnetPerpMarkets`, reflecting the two Pyth-Lazer perp markets re-created on devnet. ETH-PERP uses lazer feed 2 (real ETH price feed).

- [#193](https://github.com/velocity-exchange/velocity-v1/pull/193) [`bafd699`](https://github.com/velocity-exchange/velocity-v1/commit/bafd6990f8322f232d2f0d17042beb0e9c567164) Thanks [@jordy25519](https://github.com/jordy25519)! - Remove the stale `FettyRIP` (marketIndex 2) entry from `DevnetPerpMarkets`. That perp market was deleted on-chain, but the hand-maintained devnet config still listed it, so consumers that enumerate all perp markets (keeper-bots-v2, dlob-server) crashed with `Perp market config for 2 not found` when resolving the nonexistent market. The devnet config now mirrors on-chain state (SOL-PERP at index 0 only).

## 0.3.0

### Minor Changes

- [#172](https://github.com/velocity-exchange/velocity-v1/pull/172) [`b7d15b9`](https://github.com/velocity-exchange/velocity-v1/commit/b7d15b970a74d267aeaf20bb644d5344b9aadc61) Thanks [@0xahzam](https://github.com/0xahzam)! - Decouple solvency-repair from the withdraw pause. The `resolve_perp_pnl_deficit`,
  `resolve_perp_bankruptcy`, and `resolve_spot_bankruptcy` instructions are now gated by a
  new `State.solvencyStatus` bitfield instead of `WithdrawPaused`, so user withdrawals can
  be halted while solvency repair keeps running (or repair can be frozen on its own). Adds
  the `SolvencyStatus` enum, `StateAccount.solvencyStatus`, a `solvencyRepairPaused()`
  helper, `AdminClient.updateSolvencyStatus`, and the `exchange set-solvency-status` admin
  CLI command.

- [#141](https://github.com/velocity-exchange/velocity-v1/pull/141) [`3f148f8`](https://github.com/velocity-exchange/velocity-v1/commit/3f148f8b477e4176e11e0660adb0e67dd5163d3b) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Harden the native fast-path admin handlers. The
  `update_amm_spread_adjustment_native` instruction now requires the program
  `State` account: `getUpdateAmmSpreadAdjustmentNativeIx` is now **async** and
  returns a `Promise<TransactionInstruction>` (it derives and appends the state
  account), and its compute-unit budget was raised to cover the on-chain account
  validation. Direct callers must `await` the builder. Two new program error
  codes are surfaced in the IDL: `InvalidNativeStateAccount` (6355) and
  `InvalidNativePerpMarketAccount` (6356).

### Patch Changes

- [#173](https://github.com/velocity-exchange/velocity-v1/pull/173) [`2f6c64d`](https://github.com/velocity-exchange/velocity-v1/commit/2f6c64d54f1146d8e7f9ee4ab556929c6bf8b920) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Fix stale `User` account byte offsets in `memcmp` filters and `OrderSubscriber`.

  The Velocity `User` account is 4496 bytes, but the memcmp filters and the
  `OrderSubscriber` staleness check still used offsets from the older 4376-byte
  layout. As a result `getUserWithOrderFilter()` matched zero accounts, so any
  consumer that bulk-loads users-with-orders (e.g. the DLOB server's
  `OrderSubscriber.fetch()`) loaded no orders and produced an empty order book
  (vAMM-only L2, empty L3). Offsets for `idle`, `hasOpenOrder`, `hasOpenAuction`,
  `poolId`, and `lastActiveSlot` are corrected to match the on-chain layout.

## 0.2.6

### Patch Changes

- [#156](https://github.com/velocity-exchange/velocity-v1/pull/156) [`d3b58ab`](https://github.com/velocity-exchange/velocity-v1/commit/d3b58ab7e3ad33f0e6634ff87e9b150915b3aa13) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Fix account decoder to pass account names as-is instead of capitalizing them. The
  Anchor v1 IDL program constructor already camelCases account names, so the extra
  `capitalize()` call was incorrect and caused decoding failures in the gRPC and
  WebSocket subscribers.

## 0.2.5

### Patch Changes

- [#155](https://github.com/velocity-exchange/velocity-v1/pull/155) [`15073bc`](https://github.com/velocity-exchange/velocity-v1/commit/15073bc0b740b2d1cad471126a00368e72655bd5) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Make the `UserAccountSubscriber` "not subscribed" contract consistent and fix a
  misleading error message. `getUserAccountAndSlot()` now throws `NotSubscribedError`
  when called before `subscribe()` on the gRPC-multi and WebSocket-program subscribers
  too (the WebSocket and polling subscribers already did) — so `User.getUserAccount()`
  uniformly throws when not subscribed and returns `undefined` only when subscribed but
  the account was not found on chain. `getUserAccountOrThrow()` /
  `getUserAccountAndSlotOrThrow()` now throw `User account not found: <pubkey>` (was
  `User account not loaded`), since after `subscribe()` resolves a missing account means
  "not found", not "still loading".

## 0.2.4

### Patch Changes

- [#127](https://github.com/velocity-exchange/velocity-v1/pull/127) [`4f8e7aa`](https://github.com/velocity-exchange/velocity-v1/commit/4f8e7aaef0e35b190fc0b91cd29314d902d1ccab) Thanks [@ChesterSim](https://github.com/ChesterSim)! - reflect Typescript types on IDL changes

## 0.2.3

### Patch Changes

- [#100](https://github.com/velocity-exchange/velocity-v1/pull/100) [`ae78769`](https://github.com/velocity-exchange/velocity-v1/commit/ae78769ef58355202c030435c2796ef045fe30a0) Thanks [@ChesterSim](https://github.com/ChesterSim)! - add back ForwardOnlyTxSender and calculateMaxRemainingDeposit

## 0.2.2

### Patch Changes

- [#97](https://github.com/velocity-exchange/velocity-v1/pull/97) [`022a949`](https://github.com/velocity-exchange/velocity-v1/commit/022a949cb1802171ca57a61260f86c8908f94f34) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Re-export `PriceUpdateAccount` from the package root and declare `@types/node` as a devDependency (fixes the SDK build under isolated installs). Enables downstream apps (dlob-server, keeper-bots-v2) to consume the velocity SDK without reaching into subpaths.

## 0.2.1

### Patch Changes

- [`4fd7462`](https://github.com/velocity-exchange/velocity-v1/commit/4fd7462bfa3c55e31e3457b1b65f519cf052a6fa) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Testing new changelog based package publishing flow
