# @velocity-exchange/sdk

## 0.26.0

### Minor Changes

- [#531](https://github.com/velocity-exchange/velocity-v1/pull/531) [`98c6416`](https://github.com/velocity-exchange/velocity-v1/commit/98c6416402f7cd3a8328a07589088aaba9d6be8e) Thanks [@0xahzam](https://github.com/0xahzam)! - Mirror the program's vAMM quoting fixes.

  - The vol spread discounts the 20bp Pyth Lazer confidence floor:
    `c = min(conf, conf / 20 + max(0, conf - 20bp))`, which is 1bp at the floor. The vol base uses
    `c` in place of the raw confidence. `SPREAD_CONF_FULL_WEIGHT_THRESHOLD` is removed;
    `LAZER_CONF_FLOOR_PCT` and `calculateSpreadConfComponent` are added.
  - `calculateReferencePriceOffset` sizes the offset by inventory alone:
    `sign(inventory) * maxOffset * min(1, liquidityFraction / 10%)`, with the premium used only as a
    sign gate. `REFERENCE_PRICE_OFFSET_FULL_INVENTORY_PCT` is added. The sign-flip smoothing is
    removed.
  - `calculateSpread` applies the oracle guard (`applyOracleGuard`) when `curveUpdateIntensity > 0`,
    so neither quote crosses the oracle, whether read as the marginal price at the spread reserves
    or through `calculateBidAskPrice`. `calculateReferencePriceOffsetForAmm` computes the offset
    from AMM state.
  - `calculateSpreadBN` now matches the program where they had drifted apart: the inventory
    adjustment floors at `max(baseSpread / 2, vol)`, the cap applies by safety priority, and the
    scales use the program's integer rounding. `calculateSpreadReserves` computes the reserve delta
    exactly.

  Breaking: the `latestSlot` and `slotDurationState` parameters, which only fed the removed
  smoothing, are dropped from `calculateSpreadReserves`, `calculateUpdatedAMMSpreadReserves`,
  `calculateBidAskPrice`, `calculateBidPrice`, `calculateAskPrice`, `calculateBaseAssetValue`,
  `calculateTradeAcquiredAmounts`, `calculateTradeSlippage`, `calculateTargetPriceTrade`,
  `calculateAllEstimatedFundingRate`, `calculateLongShortFundingRate`,
  `calculateLongShortFundingRateAndLiveTwaps`, `getVammL2Generator` and `DLOBSubscriber.getL2`.
  Callers passing them need to drop the arguments.

## 0.25.0

### Minor Changes

- [#523](https://github.com/velocity-exchange/velocity-v1/pull/523) [`f720e70`](https://github.com/velocity-exchange/velocity-v1/commit/f720e70641a87a3fed42a5164cca12a55c1d4bef) Thanks [@0xahzam](https://github.com/0xahzam)! - Add ZEC to the mainnet market registries.

  `MainnetSpotMarkets` gains ZEC at spot index 4 (mint
  `A7bdiYdS5GjqGFtxf17ppRHtDKPkkRqbKtR27dxvQXaS`, 8 decimals) and `MainnetPerpMarkets` gains
  ZEC-PERP at perp index 4. Both use Pyth Lazer feed 66 and so share one oracle PDA,
  `AqpaPcu6PYHrYNySrVptnQnwxVCNxWVFuCgCsr8R1eLQ`, the way wBTC and wETH share their perp feeds.

  Nothing updates these registries automatically. Someone edits them by hand to match on-chain
  state. dlob-server, keeper-bots-v2 and the relayer build their subscription lists from them, and
  the relayer takes its Lazer feed set from `PerpMarkets`, so none of them can see a market that is
  missing here. Ship this after the markets exist on chain and before redeploying those services.

## 0.24.0

### Minor Changes

- [#506](https://github.com/velocity-exchange/velocity-v1/pull/506) [`ab33ee9`](https://github.com/velocity-exchange/velocity-v1/commit/ab33ee907bd02907266853856718f8715a570f97) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Stop trusting JSON-RPC batch response order in `fetchLogs` and `BulkAccountLoader`.

  JSON-RPC lets a server return batch responses in any order and requires matching them by `id`. `connection._rpcBatchRequest` generates the ids internally and hands them back uncorrelated, so reading the array by position can attribute a response to the wrong request. Both batch call sites now go through a new `rpcBatchRequest` helper that sends the batch with ids it owns and returns the responses aligned to the requests, rejecting if any request goes unanswered.

  `fetchTransactionLogs` blamed the wrong signature for a failed `getTransaction` and let the resume cursor advance past the one that actually failed, dropping its events for good. `BulkAccountLoader.loadChunk` matched each `getMultipleAccounts` response to a chunk by position; those results carry no pubkeys, so a reordered batch wrote account data under the wrong keys silently. `loadChunk` also read results back against the unfiltered chunk while requesting only accounts with live callbacks, shifting every account after an unsubscribed one onto the wrong data.

  This is a minor rather than a patch because the batch transport moved from `Connection._rpcBatchRequest` to `Connection._rpcClient`: any test double or connection proxy that implements only the former needs updating.

### Patch Changes

- [#511](https://github.com/velocity-exchange/velocity-v1/pull/511) [`60a173f`](https://github.com/velocity-exchange/velocity-v1/commit/60a173fd58e70d68e7d670523129621df267e2c8) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `perp-market deposit-fee-pool` and `perp-market sync-amm-summary-stats` to the admin CLI, for
  recovering a market whose `total_fee_minus_distributions` has gone negative. Both SDK instruction
  builders (`getDepositIntoPerpMarketFeePoolIx`, `getUpdatePerpMarketAmmSummaryStatsIx`) now take an
  optional `admin` override so the hot role that actually signs can be passed, as the other hot-role
  builders already allow.

## 0.23.1

### Patch Changes

- [#502](https://github.com/velocity-exchange/velocity-v1/pull/502) [`a655327`](https://github.com/velocity-exchange/velocity-v1/commit/a655327291a5ef9238bae929f19d06158db512a4) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Stop `EventSubscriber.fetchPreviousTx` from dropping logs it already fetched.

  `fetchLogs` held its `earliestTx`/`mostRecentTx` resume cursors behind a failed `getTransaction` and returned `undefined` when neither end of the page was safe to resume from, which the backfill could not tell apart from "nothing to fetch". It now returns the fetched logs with only the unsafe cursor field left `undefined`, so `fetchPreviousTx` delivers that page's events before it stops and `PollingLogProvider` still skips the tick and retries with the cursor it already has.

## 0.23.0

### Minor Changes

- [#499](https://github.com/velocity-exchange/velocity-v1/pull/499) [`0afc72e`](https://github.com/velocity-exchange/velocity-v1/commit/0afc72e8c1506ce834c1f57b764a5b1a6cce6713) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Read Agave 4.2 / SIMD-0385 transaction v1 on `getTransaction` paths.

  Bump `@solana/web3.js` to 1.99.0 (read-only v1), `@triton-one/yellowstone-grpc` to 6.0.0, and `helius-laserstream` to 0.8.5. SDK `engines.node` is now `>=20.18.0`. For the packages in this release the change is limited to `maxSupportedTransactionVersion: 1` on RPC reads. The `solana-*` 4.2 crate bump (Rust wire decode / send) is a follow-up; until it lands the Rust event poller walks a transaction's logs when `decode()` cannot read the v1 wire format, and decodes payloads only while the Velocity program is the executing program.

  `fetchLogs` now logs `getTransaction` batch errors instead of discarding them, and holds its `earliestTx`/`mostRecentTx` resume cursors behind any signature it failed to fetch so those transactions are retried rather than skipped. It returns `undefined` when no signature in the batch is safe to resume from, so keep the current cursor and retry in that case. `EventSubscriber.fetchPreviousTx` counts only transactions it has not already decoded toward `maxTx`, so the page re-read after a failed fetch no longer shortens a backfill.

## 0.22.0

### Minor Changes

- [#491](https://github.com/velocity-exchange/velocity-v1/pull/491) [`d2ea4ff`](https://github.com/velocity-exchange/velocity-v1/commit/d2ea4ffd940d4498bb4d11a7983de650f0f4d886) Thanks [@0xahzam](https://github.com/0xahzam)! - Add the fourth perp fee tier, VIP 3, at $200M trailing-30d volume.

  The program's tier ladder is now Regular / VIP 1 / VIP 2 / VIP 3 (indices 0-3, breakpoints $5M / $80M / $200M). The SDK exports the new breakpoint as `VIP_FEE_TIER_THREE_VOLUME_QUOTE` and includes it in `PERP_FEE_TIER_VOLUME_THRESHOLDS`, so `getPerpFeeTierIndex`, `User.getUserFeeTier` and `VelocityClient.getMarketFees` select tier 3 above $200M and `PERP_FEE_TIER_MAX_INDEX` is 3. A `promoFeeTier` of 3 puts every account on the top tier.

  Admin CLI: `fees set-schedule` takes four tier fees (`<t0bp> <t1bp> <t2bp> <t3bp>`; tiers 4-9 mirror tier 3), `fees set-promo-tier` accepts 3, and `show fees` prints the VIP 3 row.

## 0.21.0

### Minor Changes

- [#481](https://github.com/velocity-exchange/velocity-v1/pull/481) [`033237b`](https://github.com/velocity-exchange/velocity-v1/commit/033237bb975692bcce5bd540b3b015aba29463f3) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Add `getMarketFeesForFeeTier` and a fee-tier override on `VelocityClient.getMarketFees`, so a caller can price a market at a tier the account is not on.

  `getMarketFees` computed the fee for whichever tier the account was on and applied the market surcharge, `feeAdjustment`, referee discount and builder fee on the way. There was no way to ask what the same market would charge at a different tier, which is what a UI needs to show the saving a fee promotion is making against someone's own volume tier. Deriving the second figure from the raw tier rate instead gives two numbers computed by different formulas, so the difference between them is not the saving.

  The modifier pipeline is now the exported `getMarketFeesForFeeTier(feeTier, marketType, marketAccount?, { isReferee, builderFeeTenthBps })`, and `getMarketFees` resolves the tier and the account-derived inputs and delegates to it. Passing `feeTierOverride` (the new fifth argument) prices that tier with everything else unchanged; the referee discount comes from the tier being priced, as it does on chain. No behaviour change for existing calls.

### Patch Changes

- [#482](https://github.com/velocity-exchange/velocity-v1/pull/482) [`eaa0664`](https://github.com/velocity-exchange/velocity-v1/commit/eaa06645a8ae137ce4e8ca606b2a65d3a24980cd) Thanks [@0xahzam](https://github.com/0xahzam)! - Export `LpPoolFeatureBitFlags`, the missing mirror of the onchain `State.lpPoolFeatureBitFlags` bits (`SETTLE_LP_POOL`, `SWAP_LP_POOL`, `MINT_REDEEM_LP_POOL`), alongside the existing `FeatureBitFlags`.

- [#484](https://github.com/velocity-exchange/velocity-v1/pull/484) [`e2b86d3`](https://github.com/velocity-exchange/velocity-v1/commit/e2b86d3ddba2c3e903ce70da835314a16ebed8e3) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Add `signedMsgOrderPlaceable` and `isRestingSignedMsgLimitOrder` (with `SIGNED_MSG_RESTING_LIMIT_MAX_LEAD_MS`), mirroring the program's `place_signed_msg_taker_order` slot gates: a limit order with no auction may now be placed ahead of its message slot, which is its placement deadline, within a 30s lead bound; auction orders still wait for their message slot (`signedMsgOrderSlotReached`).

## 0.20.0

### Minor Changes

- [#477](https://github.com/velocity-exchange/velocity-v1/pull/477) [`6c183e7`](https://github.com/velocity-exchange/velocity-v1/commit/6c183e7a9d45f4987055032efcd8267651a231a4) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Apply the `State.promoFeeTier` floor everywhere the SDK selects a perp fee tier, and export the selection rule.

  `VelocityClient.getMarketFees` read `feeTiers[0]` unconditionally when called without a `user`, so while a promo is active the generic schedule quoted the undiscounted entry tier and disagreed with the same call made with a user. Consumers that price a market before a wallet connects (a trade form's fee row, a market list) were showing a fee no account actually pays. `DLOB.getMakerRebate` had the same gap: it sizes its fallback-fill buffer on the lowest rebate any maker earns, which a promo floor raises.

  The volume ladder and the promo floor now have one definition, `getPerpFeeTierIndex` (`math/fees`), which `User.getUserFeeTier`, `getMarketFees` and `DLOB.getMakerRebate` all select through. `PERP_FEE_TIER_VOLUME_THRESHOLDS`, `PERP_FEE_TIER_MAX_INDEX` and `User.getUserPerpFeeTierIndex` are exported alongside it, so surfaces that rank the tier itself (highlighting the active row of a fee schedule, progress toward the next tier) can stop mirroring the program's ladder by hand.

## 0.19.0

### Minor Changes

- [#473](https://github.com/velocity-exchange/velocity-v1/pull/473) [`106aaeb`](https://github.com/velocity-exchange/velocity-v1/commit/106aaeb44eb4a3d0a6f1ad5f0c767b6f1e5adebe) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Retune the vAMM top-of-book quote breakpoints to $250/$750/$2000/$5000 on both the default and majors ladders, and add `isMajorPerpMarket` / `MAJOR_PERP_MARKET_INDEXES` as one definition of major-market tiering for SDK consumers.

  Consumers that read `DEFAULT_TOP_OF_BOOK_QUOTE_AMOUNTS` or `MAJORS_TOP_OF_BOOK_QUOTE_AMOUNTS` will see the retuned values on upgrade, which shifts near-touch level sizing on any locally derived vAMM book. `DLOBSubscriber.getL2` now selects between the two via `isMajorPerpMarket` instead of a `marketIndex < 3` literal, so market index 3 (HYPE) is no longer treated as a major.

### Patch Changes

- [#470](https://github.com/velocity-exchange/velocity-v1/pull/470) [`a720d5b`](https://github.com/velocity-exchange/velocity-v1/commit/a720d5b5abdc6258fd6a171282c7e46c5378be4e) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Expose the signed-message placement deadline helper so fillers and DLOB expiry use the same slot-duration-aware validity window.

- [#472](https://github.com/velocity-exchange/velocity-v1/pull/472) [`7e8ff7c`](https://github.com/velocity-exchange/velocity-v1/commit/7e8ff7ca876aad9e985d6fa0a1fd060614df4b8d) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Expose signedMsgOrderSlotReached, the placement-readiness half of the signed-message slot window, so every consumer gates place attempts with the same predicate the program enforces.

- [#467](https://github.com/velocity-exchange/velocity-v1/pull/467) [`be89e60`](https://github.com/velocity-exchange/velocity-v1/commit/be89e60ce61d33653817e35bcd2c640fc9204c6f) Thanks [@0xahzam](https://github.com/0xahzam)! - Authorize the `VammQuoteManagement` hot role for scoped vAMM quoting setters, enforce protocol wide safety bounds for every hot role value, and keep oracle, MM reset, and formulaic k controls on warm/cold admin. Adds a direct `perp-market set-spread-adjustment` admin CLI command, tightens every `perp-market` positional to a strict decimal-integer parse (previously `Number('')`/`parseInt('0x10', 10)` silently resolved to market 0, and `new BN(' ')` hung the process), and lets `getUpdatePerpMarketAmmSpreadAdjustmentIx` / `getUpdatePerpMarketFundingBiasSensitivityIx` take an explicit `admin` authority so the CLI can route these setters through the hot role Squads vault instead of defaulting to cold admin.

## 0.18.0

### Minor Changes

- [#461](https://github.com/velocity-exchange/velocity-v1/pull/461) [`4b55e4e`](https://github.com/velocity-exchange/velocity-v1/commit/4b55e4e6c7ae161b42d86f12a61da9d2c1003141) Thanks [@0xahzam](https://github.com/0xahzam)! - Remove the equity-floor breaker's `$100` invalid-oracle dust concession. Invalid-oracle assets and perp longs now make a trip unprovable at every size; liabilities and shorts retain their sound zero upper bound. The program and SDK now derive observed equity, strict oracle validity, and the trip upper bound from one shared position walk. This removes the exported `EQUITY_FLOOR_TRIP_DUST_ALLOWANCE` constant without changing instructions, accounts, IDL, error codes, or strict floor gates.

- [#463](https://github.com/velocity-exchange/velocity-v1/pull/463) [`560a198`](https://github.com/velocity-exchange/velocity-v1/commit/560a198fa8a0f22ba7f3dc7f926164f8ca91dff5) Thanks [@0xahzam](https://github.com/0xahzam)! - Add wBTC (spot market index 2) and wETH (index 3) to `MainnetSpotMarkets`. Both reuse the Pyth Lazer oracle accounts their perp counterparts already use (feed ids 1 and 2).

  Do not publish this version until the two markets are initialized on chain. `findAllMarketAndOracles` derives `spotMarketIndexes` and oracle subscriptions from this registry when a client passes no explicit market list, so a client on a version that lists markets the chain does not have will try to subscribe to accounts that do not exist.

### Patch Changes

- [#455](https://github.com/velocity-exchange/velocity-v1/pull/455) [`48b8529`](https://github.com/velocity-exchange/velocity-v1/commit/48b85296c60250316ae30e3f980237af97591ec4) Thanks [@0xahzam](https://github.com/0xahzam)! - `getUpdateHotAdminIx` accepts an optional `admin` authority override, and `auth set-hot-admin` passes the Squads vault PDA through it when `--multisig` is set. Previously the instruction always listed the local wallet as the admin signer, so proposing the rotation through a multisig failed (the vault was not a required signer of any instruction).

## 0.17.0

### Minor Changes

- [#440](https://github.com/velocity-exchange/velocity-v1/pull/440) [`079d579`](https://github.com/velocity-exchange/velocity-v1/commit/079d579d4fac0f3402a4a9f8fb1aeccf21ac32ba) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - `PythClient.getOraclePriceDataFromBuffer` checks the pyth v2 price account header before it
  decodes. The buffer must be at least 3312 bytes, the length of a price account, and must carry
  magic `0xa1b2c3d4`, version 2, and account type 3 (`AccountType::Price`). Otherwise the call
  throws.

  This mirrors the program, which now rejects the same accounts. Ownership by the pyth program does
  not make an account a price feed — that program also owns mapping accounts and product accounts,
  and the push-oracle decoder reinterprets whatever bytes it is given. A decoded number for such an
  account is worse than an error, because a caller cannot tell it apart from a price.

## 0.16.0

### Minor Changes

- [#447](https://github.com/velocity-exchange/velocity-v1/pull/447) [`ce01885`](https://github.com/velocity-exchange/velocity-v1/commit/ce0188563670520bfcddb689866e37c1fa19ed00) Thanks [@0xahzam](https://github.com/0xahzam)! - Slot-duration transition archive and permissionless sync. `StateAccount` gains `slotDurationTransitionSlots` (first slot of each IBRL regime); `activeSlotDurationFromState` consults the archive first, and new `elapsedMillis` / `elapsedMillisFromSlotDelta` integrate elapsed intervals per slot-duration regime, mirroring the program's `SlotClock`. Program mirrors that measure elapsed time (`getOracleValidity`, `isOracleValid`, `getSpotOracleValidity`, `blockOperation`, `getLiquidationFee`, `calculateMaxPctToLiquidate`, `User.canMakeIdle`) now take a trailing `SlotDurationState` (the decoded `State`) instead of a `SlotDurationMs`. Forward deadlines, MM-oracle gates, and vAMM spread smoothing also use the transition archive instead of one endpoint duration. The admin instruction `updateStateSlotDurationMs` was replaced by the permissionless `syncStateSlotDuration` (`AdminClient.syncStateSlotDuration` / `getSyncStateSlotDurationIx`; `IBRL_FEATURE_WARMUP_SLOTS` removed, the effective slot now derives onchain from the `EpochSchedule` sysvar). CLI: `exchange set-slot-duration-ms` is now `exchange sync-slot-duration`. Auction durations (`Order.auctionDuration`, `OrderParams.auctionDuration`) now mean wall-clock 400ms units instead of live slots (identical raw values at the 400ms baseline); auction mirrors (`isAuctionComplete`, `getAuctionPrice*`, `getLimitPrice`, `hasLimitPrice`, `hasAuctionPrice`, `isRestingLimitOrder`, `DLOBNode.getPrice`) take a trailing optional `SlotDurationState`, and `DLOB.slotDurationState` carries it for book math. When converting an auction duration from ms, divide by 400 (ceil), not by the live slot duration.

### Patch Changes

- [#446](https://github.com/velocity-exchange/velocity-v1/pull/446) [`823724e`](https://github.com/velocity-exchange/velocity-v1/commit/823724e4a8ea0d34b5a79883512eec9cb40b6123) Thanks [@0xahzam](https://github.com/0xahzam)! - Extend perp and spot market accounts with reserved padding and retire the unused spot fee pool field.

## 0.15.0

### Minor Changes

- [#442](https://github.com/velocity-exchange/velocity-v1/pull/442) [`d3824be`](https://github.com/velocity-exchange/velocity-v1/commit/d3824be0f2261e709477e8a1aceedcd11da842c5) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Add the off-chain slot clock the TypeScript clients resolve the live slot length with.

  - `currentSlotClock(source, currentSlot)` returns `{ slotDurationMs, isLive }`, and
    `currentSlotDuration(...)` returns just the duration. Both delegate the staged-flip decision
    to `activeSlotDurationFromState`, so a client's prediction matches the on-chain value across
    a gate boundary. `source` is duck-typed on `{ getStateAccount() }`, keeping `math/time` free
    of client imports.
  - When state or the slot feed is unavailable, both helpers fall back to the hardcoded 400ms
    `SLOT_DURATION_BASELINE`, the longest scheduled slot. Callers do not pass a fallback. That
    errs toward fewer slots when converting ms into slots (a threshold closes sooner) and toward
    up to 2x more ms when converting slots into ms (a countdown stays open longer), so a call
    site in the second direction should branch on `isLive` and use `SLOT_DURATION_FLOOR`.
  - A missing, `0`, negative or non-finite `currentSlot` resolves to the baseline rather than
    being read as a slot number. A failed slot subscription reports `0`, and slot zero precedes
    every effective slot, so it would otherwise return the pre-flip base while looking live.
  - `isLive` is only true for a fully decoded `State`: the staging fields are validated as
    non-negative integers and a real `BN` effective slot, so a partial or hand-built object
    falls back instead of reporting a `NaN` duration as a measurement.
  - New `SLOT_DURATION_SCHEDULE_MS` (the mirror of the program's `[400, 350, 300, 250, 200]`)
    and `SLOT_DURATION_FLOOR` (200ms), the value a user-protection window substitutes when the
    feed is dead.
  - The keeper bots now import the shared resolver instead of keeping a local copy.

  `SLOT_TIME_ESTIMATE_MS` remains exported and deprecated.

### Patch Changes

- [#441](https://github.com/velocity-exchange/velocity-v1/pull/441) [`fe0adbd`](https://github.com/velocity-exchange/velocity-v1/commit/fe0adbd72d77eefaada569292bca2f5baf1e1e58) Thanks [@ChesterSim](https://github.com/ChesterSim)! - `User`'s VIP fee tier calculation now reads its volume thresholds from the exported
  `VIP_FEE_TIER_ONE_VOLUME_QUOTE` and `VIP_FEE_TIER_TWO_VOLUME_QUOTE` constants instead of
  duplicating the `5,000,000` / `80,000,000` quote values inline, so SDK consumers can import
  and compare against the same thresholds the tier logic uses.

## 0.14.0

### Minor Changes

- [#429](https://github.com/velocity-exchange/velocity-v1/pull/429) [`4124e93`](https://github.com/velocity-exchange/velocity-v1/commit/4124e9313dd70610a705817570bd9e428c8dea85) Thanks [@0xahzam](https://github.com/0xahzam)! - Referrer rewards split into a Standard and an Accelerated rate. Standard stays per-fee-tier
  (`FeeTier.referrerRewardNumerator`, whose fresh default drops from 15% to 10%); Accelerated is
  the fixed `ACCELERATED_REFERRER_REWARD_PERCENT` constant, independent of the tier. The referee
  discount keeps reading the fee tier. `UserStatsAccount.acceleratedReferralStatus` mirrors the
  new onchain field, with the `AcceleratedReferralStatus` flags, the
  `AcceleratedReferralStatusChange` action enum, and the
  `AcceleratedReferralStatusChangedRecord` event (subscribed by default). `AdminClient` gains
  `updateUserAcceleratedReferralStatus`, wrapped by the admin CLI as
  `user set-accelerated-referral` alongside `fees set-referral-rate`. Automatic enrollment is
  gated by a beta-scoped program constant rather than a state field, so there is no client
  surface to toggle it.

  Fill instruction builders now append the referred taker's referrer `UserStats` (readonly) after
  the taker's `RevenueShareEscrow`, which is what selects the Accelerated rate. The account is
  optional onchain, so a client that omits it still fills at the Standard rate.
  `getFillPerpOrderIx` takes a new trailing `takerReferrer` argument and `ReferrerMap` exposes
  `getReferrerAuthority`; passing the referrer keeps the fill path free of an extra `UserStats`
  fetch.

- [#393](https://github.com/velocity-exchange/velocity-v1/pull/393) [`01a7131`](https://github.com/velocity-exchange/velocity-v1/commit/01a71316b0327acd32be6e90686edd296e592af6) Thanks [@0xahzam](https://github.com/0xahzam)! - Gate swap-backed spot liquidation on the authority-wide equity breaker. `liquidateSpotWithSwapBegin`/`...End` now require the liquidator's `UserStats` account and `begin` reverts with `EquityBelowFloor` while the liquidator authority's equity breaker is tripped, matching the other liquidator routes. `getLiquidateSpotWithSwapIx` and `getJupiterLiquidateSpotWithSwapIxV6` resolve the new account automatically; keepers building the instructions by hand must pass `liquidatorStats`.

- [#420](https://github.com/velocity-exchange/velocity-v1/pull/420) [`7ee2feb`](https://github.com/velocity-exchange/velocity-v1/commit/7ee2febf9c4bfe9cb0e7361828a1aad087216df7) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Perp fills charge a builder fee only while the taker meets initial margin. `User` gains
  `isBuilderFeeCharged()`, and `User.calculatePerpTakerFee` and `VelocityClient.getMarketFees` consult
  it, so a predicted fee for a taker below initial margin no longer includes the builder fee
  (OtterSec #83).

  A builder fee is an additive debit on the taker that the builder later claims into its own account,
  and the taker is the party that approves the builder. The fee is therefore a transfer out of the
  account, and it must clear the gate a withdrawal clears. A position-decreasing fill is otherwise
  checked against maintenance margin alone, which lets an under-margined taker reduce the position in
  slices and route out value the initial-margin gate holds in the account. The 1% cap on the fee rate
  bounds one fill, not the sequence.

  The program's gate reads the same oracle rules a withdrawal reads: strict (TWAP-bounded) prices,
  no collateral for a deposit with an invalid oracle, and every liability oracle valid.
  `isBuilderFeeCharged()` applies the strict prices but does not model oracle validity, so it is an
  estimate — it can report `true` where the program waives the fee.

  The program waives the fee, not the fill: the taker still closes the position and the builder is
  paid nothing for that fill. A client that shows a builder fee before a close must read
  `isBuilderFeeCharged()` to predict the charge for an under-margined account.

- [#355](https://github.com/velocity-exchange/velocity-v1/pull/355) [`b7b5ae8`](https://github.com/velocity-exchange/velocity-v1/commit/b7b5ae80040b66651e6553d16354cbd075113cbb) Thanks [@0xahzam](https://github.com/0xahzam)! - Harden the equity breaker recovery path. Cure transfers: `transferDepositByDelegate` with a zero floor delta into a subaccount below its buffered floor now passes onchain while the breaker is tripped, so a breach can be topped up from internal surplus instead of requiring fresh deposits; `EquityFloorManager` gains `planCureTransfers()` and `cureBreaches()` (plus the pure `planCureMoves`) to plan and submit those transfers, deepest breach first, without drawing any donor below its own buffered floor. Self-verifying reset: `resetEquityFloorBreaker` now carries every live subaccount of the authority (count pinned by `UserStats.numberOfSubAccounts`) plus their markets and oracles, and reverts with the new `InvalidEquityBreakerReset` (6368) unless every floored subaccount clears its floor + buffer at execution time, so a stale approval fails instead of unfreezing a breached authority; `AdminClient.resetEquityFloorBreaker`/`getResetEquityFloorBreakerIx` build the account set automatically, with an optional `userAccounts` override for connections without `getProgramAccounts`. Neither path clears the flag automatically; the admin reset remains the only unfreeze.

- [#386](https://github.com/velocity-exchange/velocity-v1/pull/386) [`4e29bc0`](https://github.com/velocity-exchange/velocity-v1/commit/4e29bc0f131ad278450042e2554fd64bac4315ee) Thanks [@0xahzam](https://github.com/0xahzam)! - Mirror the fail-closed equity-floor oracle handling: `User.getNetUsdValueBounds` is replaced by `User.getFloorNetEquity(slot)` returning `{ value, allOraclesValid }` (exact net equity plus the validity verdict, matching the program's floor metric); `boundPrices`, `NetUsdValueBounds`, `I128_MIN` and `I128_MAX` are removed. `isBelowBufferedEquityFloor(slot)` now predicts the fail-closed gates (any invalid oracle reads as gated), and `getEquityAboveFloor(slot)` / `getEquityAboveBufferedFloor(slot)` report zero headroom while any oracle is invalid.

- [#380](https://github.com/velocity-exchange/velocity-v1/pull/380) [`02078e6`](https://github.com/velocity-exchange/velocity-v1/commit/02078e625eb89c3fd5798af8d07693a21268a30e) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Mirror the equity floor's oracle-validity gating: `User.getNetUsdValueBounds(slot)` computes the two-sided net equity bounds the onchain floor gates now use (invalid-oracle positions priced at both live and 5-minute TWAP, non-positive candidates dropped, unpriceable positions saturating to `I128_MIN`/`I128_MAX`), with pure helpers `boundPrices`, `getSpotOracleValidity`, `getSpotMaxConfidenceIntervalMultiplier` and `isOracleValidForMarginCalc`. `isBelowBufferedEquityFloor(slot?)` predicts the gates on the lower bound when given a slot and is unchanged otherwise. The equity floor guard bot now alerts distinctly, once per outage, when a breaker trip is blocked by `InvalidOracle` instead of logging it as a generic simulation failure.

- [#388](https://github.com/velocity-exchange/velocity-v1/pull/388) [`77499bb`](https://github.com/velocity-exchange/velocity-v1/commit/77499bb3c0644730d5d48e6e3b331988cc5c2b02) Thanks [@0xahzam](https://github.com/0xahzam)! - Rework the perp fee schedule. Fee tiers cut from 6 to 3 (Regular / VIP 1 / VIP 2) with new 30d-volume thresholds ($5M / $80M) and new defaults (4/3/2bps taker, flat -0.25bp maker rebate); `getUserFeeTier` mirrors the new thresholds, projects the rolling-volume decay to now (demotion tracks the live trailing window), and applies the new promotional tier floor. New onchain knobs with SDK/CLI surface: per-market additive taker-fee surcharge (`PerpMarketAccount.takerFeeAddonTenthBps`, unsigned, applied by `getMarketFees` before `feeAdjustment`; `AdminClient.updatePerpMarketTakerFeeAddon`, `velocity-admin fees set-taker-addon`) and the promo fee-tier floor (`StateAccount.promoFeeTier`, effective tier = max(volume tier, promo tier), 0 = off; `AdminClient.updatePromoFeeTier`, `velocity-admin fees set-promo-tier`).

- [#404](https://github.com/velocity-exchange/velocity-v1/pull/404) [`15db231`](https://github.com/velocity-exchange/velocity-v1/commit/15db231101dd2ac6ed3a94d63d0b41e5acecceb3) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Mirror the program's mark-TWAP re-seed, so `calculateAllEstimatedFundingRate` does not predict a
  premium the next on-chain funding update will not charge.

  `calculate_new_twap` weights an incoming mark-TWAP sample by the time since the last write and floors
  the opposing weight at 1, so past one funding period a single fill-path sample replaces the TWAP
  outright (bid/ask-crank samples are weight-capped since the crank sample-weight fix). The program now
  discards the stored mark TWAPs and re-seeds them from the oracle TWAP when they went unwritten for
  more than `max(fundingPeriod * 3, 3600)` seconds. The first funding update after such a gap therefore
  sees a zero price spread and charges the baseline offset alone, and the real premium returns the
  following period.

  The gap is longest after a funding pause, because both funding cranks reject while the pause is set.
  It also opens on any multi-period keeper outage.

  `calculateLiveMarkTwap` applies the same threshold and re-seed, so the estimate tracks the program.
  New export `MARK_TWAP_RESEED_FUNDING_PERIODS`.

  New export `getMaxMarkTwapSampleElapsed`, the mirror of the program's
  `MarketStats::max_mark_twap_sample_elapsed` crank sample-weight cap, for predicting the TWAP a
  bid/ask crank write produces. `calculateLiveMarkTwap` deliberately does not apply it, because it
  predicts the funding update's own write, which is uncapped on-chain. `ONE_MINUTE` is also exported.

- [#383](https://github.com/velocity-exchange/velocity-v1/pull/383) [`ede187b`](https://github.com/velocity-exchange/velocity-v1/commit/ede187be1060f4790f03f459733e0485096aaf69) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `getUpdateMmOracleBatchNativeIx` / `updateMmOracleBatchNative`, builders for the program's new
  batched MM-oracle native instruction (dispatch opcode 2), plus the `MmOracleBatchUpdate` entry type
  and the `MM_ORACLE_BATCH_MAX_MARKETS` constant.

  One instruction writes the MM oracle for many perp markets, so a caller pays one transaction
  signature for the whole set instead of one per market, and the instruction's authentication prologue
  (which is most of its compute cost) is paid once rather than per market.

  Per-market rate-limit and sanity rejections skip only that market; the rest of the batch still
  lands. Each entry carries its own market index, which the program re-checks against the account it
  was paired with, so a misordered list fails loudly instead of writing one market's price onto
  another. Each entry also carries `oracleSourceSlot`, the slot the price was observed at; the program
  skips an entry landing more than `MM_ORACLE_MAX_SOURCE_AGE_SLOTS` from it in either direction, so
  a late-landing transaction cannot make an old observation read as fresh and a wrong-unit value
  cannot silently disable the check. The builder additionally rejects an empty
  list, more than `MM_ORACLE_BATCH_MAX_MARKETS` markets, duplicate market indexes, non-positive
  prices (`BN` little-endian serialization drops the sign, so a negative price would otherwise reach
  the program as its magnitude), and values that do not fit their on-chain width (`i64` price, `u64`
  sequence id and source slot).

- [#384](https://github.com/velocity-exchange/velocity-v1/pull/384) [`1b81121`](https://github.com/velocity-exchange/velocity-v1/commit/1b8112143db861aab3507df64028911285425827) Thanks [@0xahzam](https://github.com/0xahzam)! - MM oracle freshness fixes, mirroring the program:

  - `getOracleValidity` resolves an unset (`oracleSlotDelayOverride < 0`) immediate-fill staleness
    threshold by price source, via a new optional `isMmSourcedPrice` parameter: an MM-oracle-sourced
    price gets `MM_ORACLE_MIN_SLOT_GAP` (the program will not accept MM-oracle writes closer together
    than that, so a tighter threshold is unsatisfiable), while an exchange-sourced price keeps the
    strict zero threshold. `0` still means "no immediate AMM fills on this market", and explicit
    positive thresholds are unchanged. `MMOraclePriceData` gains an `isMMSourcedPrice` flag populated
    by `getMMOracleDataForPerpMarket`.
  - `updateMmOracleNative` / `getUpdateMmOracleNativeIx` take a new required `oracleSourceSlot`
    parameter (breaking): the slot the price was observed at, which the program now requires in the
    payload and checks against the landing slot symmetrically in both directions
    (`MM_ORACLE_MAX_SOURCE_AGE_SLOTS`, exported), so a late-landing update cannot make an old
    observation read as fresh and a wrong-unit value cannot silently disable the check. The builder
    also validates its inputs (positive price fitting `i64`, `u64` sequence id and source slot); the
    program now hard-errors on any non-positive price, not just exact zero.
  - The native MM-oracle instructions no longer take a clock sysvar account (the program reads the
    slot via syscall), so the builders emit one account fewer per transaction: `updateMmOracleNative`
    passes `[market, signer, state]` and the batch passes `[signer, state, ...markets]`.
  - Exports the `MM_ORACLE_MIN_SLOT_GAP` and `MM_ORACLE_MAX_SOURCE_AGE_SLOTS` constants.

- [#392](https://github.com/velocity-exchange/velocity-v1/pull/392) [`4d0946b`](https://github.com/velocity-exchange/velocity-v1/commit/4d0946b70b336cf71cfcdca202a47dc9d8c81e05) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Accrued builder/referrer revenue share can now be collected without the escrow owner's participation, and is paid out rather than written off when a market is delisted.

  `settleRevenueShare` / `getSettleRevenueShareIx` wrap the new permissionless `settle_revenue_share` instruction, which settles one escrow's rows for one perp market out of that market's pnl pool. Previously the only payer ran inside `settlePNL` and only when that settle actually moved PnL, so once an escrow owner flattened and stopped trading a market their beneficiaries' fees were stranded and the market's `pendingRevenueShare` kept reserving pnl-pool value against a claim nobody could settle.

  `forfeitRevenueShareOrder` / `getForfeitRevenueShareOrderIx` wrap `forfeit_revenue_share_order`, which writes off a row of a market in settlement or delisted that provably cannot be paid — the beneficiary has no payout account, the wound-down pool cannot cover it, or it names no reachable beneficiary. Anything still payable is rejected with `RevenueShareOrderNotForfeitable` (6373).

  Delisting a market now requires that revenue share to have been resolved: `settle_expired_market_pools_to_revenue_pool` rejects with `UnsettledRevenueShareOnDelist` (6372) while `pendingRevenueShare` is non-zero. There is no time-based escape, because between the two instructions above every row is terminally resolvable. A delisted market therefore always reports `pendingRevenueShare` as zero, and consumers must not treat a delisted market's counter as an outstanding liability.

  `RevenueShareEscrowMap.getEscrowsOwingRevenueShare(marketIndex)` returns the escrows still owed on a market — the work list to clear before delisting. `calculateRevenueShareSweepAvailable`, `calculateBankruptcyIfTrancheReservation` and `calculateBankruptcyIfFloor` (`math/market`) mirror the reservation the on-chain sweep applies, so a keeper can predict whether a call will pay before sending it.

  CLI: new `velocity-admin fees settle-revenue-share <market> [escrowAuthority]`, with `--all` to scan a market, settle every escrow still owed, and forfeit any stragglers that cannot be paid.

- [#425](https://github.com/velocity-exchange/velocity-v1/pull/425) [`193c357`](https://github.com/velocity-exchange/velocity-v1/commit/193c35720365eefac9bfe9fbf1b241cf809029ff) Thanks [@0xahzam](https://github.com/0xahzam)! - Slot-duration scaling for the Solana slot-time reduction (400 -> 350 -> 300 -> 250 -> 200ms feature gates). New `State` fields `slotDurationMs` (0 = unset = 400ms baseline), `pendingSlotDurationMs`, and `slotDurationEffectiveSlot`, plus the `updateStateSlotDurationMs` admin instruction. The instruction _stages_ the next value during the target IBRL gate's warmup: it accepts only the exact next value on the 400 -> 350 -> 300 -> 250 -> 200 schedule, reads the switch slot from the gate's feature account (passed as a remaining account; the account activation slot is exposed one epoch ahead), and records it as `pendingSlotDurationMs` + `slotDurationEffectiveSlot`. `State` then switches itself at that slot in lockstep with the chain, no second transaction. `updateStateSlotDurationMs`/`getUpdateStateSlotDurationMsIx` fill in the feature account automatically and take an optional explicit `admin` pubkey (default `warmAdmin` when set else `coldAdmin`). Resolve the live value with the new `activeSlotDurationFromState(state, currentSlot)` (the base-only `slotDurationFromState` still exists). Onchain duration arithmetic uses `Millis`; compact account fields use the new transparent `StoredSlotDuration<T, SLOT_MS>`, which preserves `T`'s wire width while recording the slot length assumed by that encoding (the IDL remains primitive-compatible). TypeScript exports branded `Millis`/`SlotDurationMs` with `slotDurationFromState`/`activeSlotDurationFromState`/`millisToSlots`/`millisToSlotsCeil`/`millisFromSlots`/`millisFromStoredUnits`/`divPeriods` (plus number-domain `msToSlotsNum`/`msToSlotsCeilNum`/`slotsToMsNum`), mirroring the onchain `math::time`. `getOracleValidity`, `isOracleValid`, `getSpotOracleValidity`, and `User.canMakeIdle` take an optional trailing `SlotDurationMs` (default the 400ms baseline); `getVammL2Generator` takes a required `slotDuration`; `calculateBidPrice`/`calculateAskPrice`/`calculateUpdatedAMMSpreadReserves`/`calculateTradeSlippage`/`calculateTradeAcquiredAmounts`/`calculateTargetPriceTrade`/`calculateBaseAssetValue` take an optional trailing `slotDuration` (default the 400ms baseline); `VelocityClient.getMMOracleDataForPerpMarket` takes an optional trailing `currentSlot` (pass a live slot for correct post-transition validity); `calculateMaxPctToLiquidate` takes its ramp length as `Millis` (decode the stored field with `millisFromStoredUnits`). New `getLiquidationFee` and `blockOperation` helpers mirror the program's duration-aware liquidation-fee and funding-block decisions. Force-close perp auctions now convert their legacy 32-second duration through the live slot length instead of hardcoding 80 slots. Renames: `IDLE_TIME_SLOTS` -> `IDLE_TIME` (Millis), `MM_ORACLE_MIN_SLOT_GAP` -> `MM_ORACLE_MIN_WRITE_GAP` (Millis), `MM_ORACLE_MAX_SOURCE_AGE_SLOTS` -> `MM_ORACLE_MAX_SOURCE_AGE` (Millis). `SLOT_TIME_ESTIMATE_MS` is deprecated. Admin CLI gains `exchange set-slot-duration-ms` (validates an exact integer, prints current -> new, previews the target IBRL gate's activation/effective slots from the on-chain feature account, and dispatches under the correct authority for direct or `--multisig` use). New SDK exports `getIbrlFeatureGate(slotDurationMs)` and `IBRL_FEATURE_WARMUP_SLOTS` support that preview.

  VLP constituent initialization and updates now reject oracle-staleness thresholds above 1,000,000 historical 400ms units, matching the defensive margin-oracle ceiling.

- [#415](https://github.com/velocity-exchange/velocity-v1/pull/415) [`dbea9aa`](https://github.com/velocity-exchange/velocity-v1/commit/dbea9aae45f27f8800cc80480443974ce68c031d) Thanks [@0xahzam](https://github.com/0xahzam)! - Mirror the equity-breaker trip's dust-tolerant proof. New `User.getTripNetEquity(slot?)` returns the trip's net-equity upper bound and provability: positions with valid oracles are valued live, an invalid-oracle position is conceded its most favorable value instead of vetoing the proof (a liability or short base leg counts as zero at any size, an asset or long base leg within `EQUITY_FLOOR_TRIP_DUST_ALLOWANCE` at its own last twap counts as the allowance), and a larger invalid asset or long, or one whose twap is not positive, keeps the breach unprovable. New `User.provesEquityFloorBreach(slot?)` mirrors the onchain trip predicate exactly. `isBelowEquityFloor` is unchanged and documents that it compares the point value only.

- [#370](https://github.com/velocity-exchange/velocity-v1/pull/370) [`64301e1`](https://github.com/velocity-exchange/velocity-v1/commit/64301e1f19257152bf3c51174f4549dfbbdc9009) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Mirror the program's clamp on a perp auction's baseline start offset, and document that
  `updatePerpBidAskTwap` / `getUpdatePerpBidAskTwapIx` only sample DLOB orders that have rested
  on-chain for at least 24 slots (~10s).

  `getTriggerAuctionStartPrice` (and so `getTriggerAuctionStartAndExecutionPrice`) now clamps the
  baseline start offset to ±(oracle TWAP / tier divisor) before applying the start buffer, matching
  `OrderParams::get_perp_baseline_start_price_offset`. The bound is 2% of the oracle TWAP on tier A, 5%
  on B and C, 10% on Speculative, 20% on HighlySpeculative and Isolated. Predicted start prices change
  only for markets whose mark TWAP sits outside that band. Two new exports expose the bound:
  `getAuctionEndMinMaxDivisors` and `getPerpBaselineMaxPriceOffset`.

  The program also now ignores quotes younger than 24 slots when estimating the market's bid/ask for
  the mark TWAP (OtterSec #146: previously a caller could place a self-crossed pair of post-only
  quotes, crank, and cancel in a single transaction, moving the TWAP that prices a third party's
  forced-close auction band without ever being exposed to a fill). Keeper operators should know that
  makers who cancel/replace faster than ~10s no longer contribute to the estimate, and that passing
  only freshly-placed makers yields no DLOB estimate at all — the crank falls back to the AMM's quote.

- [#387](https://github.com/velocity-exchange/velocity-v1/pull/387) [`6e34ce3`](https://github.com/velocity-exchange/velocity-v1/commit/6e34ce3a14292eb4f6ceecfc67cde2e15590bd35) Thanks [@0xahzam](https://github.com/0xahzam)! - Add the vAMM maker rebate feature flag. New onchain `FeatureBitFlags::VammMakerRebate` (bit 8, off by default): when enabled, the vAMM earns the maker rebate on fills it makes against a taker, carved off the taker-fee remainder before the protocol/IF/AMM split and folded into the AMM's fee provision. The taker's fee is unchanged; only the distribution shifts. SDK: `FeatureBitFlags.VAMM_MAKER_REBATE`, `AdminClient.updateFeatureBitFlagsVammMakerRebate` / `getUpdateFeatureBitFlagsVammMakerRebateIx`. Admin CLI: `velocity-admin feature-flags vamm-maker-rebate <true|false>`.

- [#379](https://github.com/velocity-exchange/velocity-v1/pull/379) [`98e787d`](https://github.com/velocity-exchange/velocity-v1/commit/98e787decb6153bacf6ec7f25e867cdcf217b413) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Bound the small-depositor exception to the spot withdraw circuit breaker at market level (OtterSec #150). Onchain, `check_withdraw_limits` now treats the per-account bypass predicate as an eligibility filter only: the whole eligible cohort shares one `withdrawGuardThreshold` of room below the breaker floor, so splitting a deposit across subaccounts no longer multiplies the bypass. `calculateWithdrawLimit` returns a new `exceptionWithdrawLimit` (that shared budget, always at least `withdrawLimit`), and `User.getWithdrawalLimit` caps the bypass by it. Previously `getWithdrawalLimit` raised the limit to the user's full deposit whenever `canBypassWithdrawLimits` returned true, which over-predicted a withdrawal that reverts onchain with `DailyWithdrawLimit`. `canBypassWithdrawLimits` is unchanged but is now documented as eligibility only, not a promise of a successful withdrawal. `AdminClient.initializeSpotMarket` and `updateWithdrawGuardThreshold` doc comments for `withdrawGuardThreshold` are corrected: it is the level _below_ which the withdraw guards stop binding, not a cap above which withdraws are blocked.

### Patch Changes

- [#364](https://github.com/velocity-exchange/velocity-v1/pull/364) [`74786b4`](https://github.com/velocity-exchange/velocity-v1/commit/74786b44c1009369c98d920b10d8f322a2214e26) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - `isUserBankrupt` now mirrors the program's two value-aware bankruptcy vetoes (OtterSec #151/#145), so a keeper stops reporting an account as solvent that the program will resolve. A spot deposit row vetoes only when it is worth at least one token — a fully socialized market floors `cumulativeDepositInterest` at 1 and leaves every wiped depositor a positive `scaledBalance` worth nothing, which cannot be seized and previously blocked admission forever. A positive perp `quoteAssetAmount` vetoes only while its market's PnL pool can pay part of it, plus a new net-quote gate that keeps a net solvent estate out of bankruptcy however unfundable its claims. The exported signature is unchanged, but the function now reads market state as well as the user account and throws if a market referenced by a nonzero position is not loaded on the client.

- [#397](https://github.com/velocity-exchange/velocity-v1/pull/397) [`1004b31`](https://github.com/velocity-exchange/velocity-v1/commit/1004b31a45f0da9cf8faed18c5c82f2351730c75) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - `PerpMarketAccount.pendingBankruptcyClaims` mirrors the new per-market counter of unresolved
  bankrupt quote debts. While it is above zero the program's fee sweep withholds the whole
  `feeLedger.pendingIfFee`, so a permissionless sweep cannot drain the bankruptcy first-loss tranche
  between the latch and the resolution. `PositionFlag.BankruptcyClaim` marks the position whose debt
  is counted. `AdminClient.settleExpiredMarketPoolsToRevenuePool` now fails while that counter is above
  zero, because the delist sweep bypasses the floor; resolve every bankruptcy in the market
  first.

  `bankruptcyIfFloorPct` changes meaning: `0` now selects `DEFAULT_BANKRUPTCY_IF_FLOOR_PCT` (10 bps),
  which is what a market written before the field existed reads, and the new
  `BANKRUPTCY_IF_FLOOR_DISABLED` sentinel turns the standing floor off. Callers that passed `0` to
  `AdminClient.updatePerpMarketBankruptcyIfFloorPct` to disable the floor must pass the sentinel
  instead. The admin CLI accepts `perp-market set-bankruptcy-if-floor <market> disabled`.

- [#422](https://github.com/velocity-exchange/velocity-v1/pull/422) [`48e9301`](https://github.com/velocity-exchange/velocity-v1/commit/48e930147f8110454ef83f13f27a8ce8b921791a) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Stop `isUserBankrupt` vetoing on a positive perp claim or its market's PnL pool, mirroring the
  program: the resolvers now recover what the pool can pay and forfeit the rest, so no pool state
  blocks admission. `getResolveSpotBankruptcyIx` also passes the quote spot market writable, which that
  instruction now requires.

- [#363](https://github.com/velocity-exchange/velocity-v1/pull/363) [`94bb6ce`](https://github.com/velocity-exchange/velocity-v1/commit/94bb6ce94aad1981e4ee7910a85ab9ffbfe1d7c3) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - `delete_user` and `force_delete_user` now take the authority's `RevenueShareEscrow` PDA as a required
  account, so each can settle that sub-account's builder revenue-share rows before the sub-account id is
  retired forever (OtterSec #128). Previously those rows became unreachable — the builder's accrued fee was
  stranded and the market's `pending_revenue_share` stayed inflated for the life of the market.

  **SDK callers need no change**: `getUserDeletionIx` / `deleteUser` and `getForceDeleteUserIx` /
  `forceDeleteUser` derive and pass the account for you. **Anyone building either instruction manually must
  add it**, including when the authority has never created an escrow — the address is pinned by seeds on
  chain, so an uninitialized account proves absence rather than signalling an omitted check.

  `getForceDeleteUserIx` no longer appends the escrow to `remaining_accounts` when the account holds
  builder orders. The named account replaces it, and the program never read the trailing copy.

- [#354](https://github.com/velocity-exchange/velocity-v1/pull/354) [`06fac9e`](https://github.com/velocity-exchange/velocity-v1/commit/06fac9ed1584d51a6599dfb673977c0a4626c943) Thanks [@0xahzam](https://github.com/0xahzam)! - triggerOrder's userStats account is writable in the regenerated IDL; onchain, the equity breaker now arms lazily when reducing fills, strictly reducing swaps, or trigger cancels observe a floored subaccount below its raw floor

- [#361](https://github.com/velocity-exchange/velocity-v1/pull/361) [`fccd4f6`](https://github.com/velocity-exchange/velocity-v1/commit/fccd4f63d7522eca86d79aa8ec93092af2b63b7f) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - New error code `SpotMarketInterestStaleForMargin` (6371). A value-releasing path now reverts when
  a spot market carrying one of the account's **borrows** has not accrued interest recently enough,
  because margin would otherwise value that debt through a stale `cumulative_borrow_interest` and
  understate it (OtterSec #135 / #148). The gated paths are withdraw, transfer deposit, transfer
  pools, swap, isolated-position withdraw, and a perp fill — for the taker and for every maker
  alike, whichever direction the fill moves each position. A borrow whose un-booked interest is
  still under one token unit is exempt, so a dust-sized market that cannot book its interval does
  not lock the account out. Liquidations are never gated.

  Each market gets its own staleness window from its rate ceiling. The bound holds the omission
  under one basis point of the debt, so a market that may charge more interest must be cranked more
  often. `MAX_SPOT_INTEREST_STALENESS_FOR_MARGIN` (one hour) caps the window.

  A perp fill also no longer credits a spot deposit whose oracle is invalid for margin
  (OtterSec #143 / #144). The deposit contributes zero collateral, which is what the withdraw path
  already does, so a fill that needed it now reverts with `InsufficientCollateral`. A fill by an
  account holding a spot **borrow** whose oracle is invalid for margin reverts with `InvalidOracle`.

  **Required change for anyone building fill or withdraw transactions.** No gated path cranks the
  markets it does not itself touch, so a taker or maker holding a borrow in a quietly-traded spot
  market becomes unfillable until that market is accrued. Recovery needs no privileges:
  `update_spot_market_cumulative_interest` is permissionless and can ride in the same transaction.

  New SDK helpers build exactly that:

  - `MAX_SPOT_INTEREST_UNDERSTATEMENT_FOR_MARGIN` and `MAX_SPOT_INTEREST_STALENESS_FOR_MARGIN` —
    the program's bounds.
  - `maxSpotInterestStalenessForMargin(spotMarket)` — one market's window.
  - `VelocityClient.getStaleSpotInterestMarketIndexes(userAccounts, now?)` — the markets that need
    a crank for those accounts.
  - `VelocityClient.getStaleSpotInterestCrankIxs(userAccounts, now?)` — one
    `updateSpotMarketCumulativeInterest` instruction per such market. Prepend them to the fill,
    withdraw, transfer, or swap.

  Pass a fill's taker and every maker. The helpers do not model the sub-token exemption, so they
  name a superset of what the program requires; cranking all of them always clears the check.

- [#390](https://github.com/velocity-exchange/velocity-v1/pull/390) [`a6bffcb`](https://github.com/velocity-exchange/velocity-v1/commit/a6bffcb20a909f98552ef8f3adee8b5665e4257a) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - `addInsuranceFundStake`'s `amount` is now an upper bound rather than the staked amount. IF shares are
  indivisible, so the program transfers only the portion of the request that prices to whole shares and
  leaves the remainder — always less than one share price — in the token account.

  This completes the fix for the zero-shares High finding. Rejecting only the zero-share case bounded
  the loss instead of removing it: a request worth 1.5 shares minted 1 and donated the other half to
  existing shareholders, and because the share price is set off a donation-inflatable vault balance, an
  attacker could pick that fraction. Pricing the deposit exactly (shares floored, their cost ceiled, so
  the fund never sells a share below price) caps the residual at one token unit and makes the donation
  unprofitable.

  `IFDepositMintsZeroShares` (6360) now means the request was below the price of a single share. Read
  the staked amount from `InsuranceFundStakeRecord.amount` instead of assuming it equals the requested
  amount; with `fromSubaccount`, any remainder lands in the wallet's token account rather than returning
  to the sub-account.

  `VaultClient.addToInsuranceFundStake` inherits the same rule with one difference: the vaults program
  stakes the whole balance of the vault's IF token account, so a remainder from an earlier add is folded
  in and the staked amount can exceed `amount`.

- [#366](https://github.com/velocity-exchange/velocity-v1/pull/366) [`4227e3e`](https://github.com/velocity-exchange/velocity-v1/commit/4227e3e6fe3805cd0986f81ca6cc4a7513c0c460) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - `cancel_request_remove_insurance_fund_stake` now settles any already-due revenue into the insurance
  fund vault before pricing the cancel's forfeiture (OtterSec #141). Previously a staker could order
  their signed cancel ahead of an already-due signerless settle, make the restake price against a stale
  vault, burn no shares, and keep revenue the anti-free-option rule assigns to the remaining stakers.

  **ABI change — the account list is reordered, not just appended.** The instruction now takes `state`
  (prepended), plus `spot_market_vault`, `velocity_signer` and `token_program`, matching
  `request_remove_insurance_fund_stake`. Both SDKs pass them for you
  (`VelocityClient.cancelRequestRemoveInsuranceFundStake`, `VaultClient.getCancelRequestRemoveInsuranceFundStakeIx`),
  so SDK callers need no change; anyone building the instruction manually must rebuild the account list.
  The `vaults` program's CPI wrapper gained the matching accounts.

- [#342](https://github.com/velocity-exchange/velocity-v1/pull/342) [`4872b4f`](https://github.com/velocity-exchange/velocity-v1/commit/4872b4f49942c0f2ef830d10214ac26f46464c38) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - `calculateInterestAccumulated` now applies the program's conservation clamp: the returned
  `depositInterest` is scaled down when the tokens it would credit to `depositBalance` exceed the
  tokens `borrowInterest` charges `borrowBalance`. The two are equal by construction (the deposit rate
  is the borrow rate scaled by utilization), but utilization is derived from rounded token amounts and
  sampled once for the whole interval, so a long projection at a high rate previously overstated the
  deposit side by whole tokens.

  The doc comment also records that the program now **defers** an accrual interval whose configured
  `insuranceFund.ifFeeFactor` / `protocolFeeFactor` carveout would convert to less than one token
  (OtterSec #127), so a projection from `lastInterestTs` can legitimately span a long window on such a
  market even though the accrual has been cranked repeatedly.

- [#407](https://github.com/velocity-exchange/velocity-v1/pull/407) [`aaec40f`](https://github.com/velocity-exchange/velocity-v1/commit/aaec40fe81268dcc5922f8bbdb1301ea635a6dfd) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - `SpotMarketAccount.ifLastSettleVaultAmount` now holds the lowest insurance-fund vault balance
  since the end of the last revenue settle, not an accounted shadow balance. The revenue-settle APR
  cap is sized off `min(live IF vault, this)`, so a donation must stay in the fund for a whole
  settle period to count. A `0` value means the market never settled revenue. The field name, type,
  and account offset are unchanged.

- [#395](https://github.com/velocity-exchange/velocity-v1/pull/395) [`b808fbb`](https://github.com/velocity-exchange/velocity-v1/commit/b808fbb90c4fea6bc597929203b25b6b9cf415d5) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Carry sub-unit lending-interest carveouts instead of delaying the accrual interval.

  `PoolBalance` gains `pendingInterestSplitDust` and `pendingInterestDust`. Both come from the
  former padding, so the size of the struct and every other field offset are unchanged. A carveout
  too small to pay in whole units now accumulates on the carveout pools and does not hold up the
  interval. `cumulativeDepositInterest` and `lastInterestTs` therefore advance on every interval
  that reaches a whole index unit on both sides. An interval under that floor stays on the clock
  and is retried on the next crank. `calculateInterestAccumulated` documents the change. A
  projection from `lastInterestTs` spans a window in which balances could have changed only by
  that sub-unit remainder.

- [#379](https://github.com/velocity-exchange/velocity-v1/pull/379) [`a6bd667`](https://github.com/velocity-exchange/velocity-v1/commit/a6bd667c28c3216ac213556d160aaea8e459191f) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - The isolated-position instructions now respect the per-market gates that the cross-margin paths already applied. `withdrawFromIsolatedPerpPosition` reverts with `MarketWithdrawPaused` (6149) unless the spot market status is `active`, `reduceOnly` or `settlement` and the `Withdraw` operation is unpaused, so a market closed for withdrawals is closed on every route. A market in `settlement` stays exitable, matching the cross-margin path, so no isolated collateral is trapped. `depositIntoIsolatedPerpPosition` reverts with `DailyDepositLimit` (6364) when the deposit takes the market above its daily deposit cap, so the cap can no longer be stepped around. JSDoc on both client methods records the new revert codes; use `calculateWithdrawLimit` and `checkDepositLimits` to test a market before building either instruction. The isolated-position instructions are behind the `isolated-position` program feature, which is not in the mainnet default feature set.

- [#379](https://github.com/velocity-exchange/velocity-v1/pull/379) [`d3ef5e5`](https://github.com/velocity-exchange/velocity-v1/commit/d3ef5e5ed17e0ac51e8b8eb2fd039c381e2cff30) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - `withdrawFromIsolatedPerpPosition` now respects the spot market's withdraw circuit breaker onchain, at market level and without the small-depositor exception, so it can revert with `DailyWithdrawLimit` (6128) even when the isolated position holds the requested amount. `getWithdrawFromIsolatedPerpPositionIxsBundle` documents this; its clamp still bounds the request by the position's own balance only, so read `withdrawLimit` from `calculateWithdrawLimit` for the market's remaining room. The isolated-position instructions are behind the `isolated-position` program feature, which is not in the mainnet default feature set.

- [#410](https://github.com/velocity-exchange/velocity-v1/pull/410) [`1a6af18`](https://github.com/velocity-exchange/velocity-v1/commit/1a6af1819be7822e56009e444d82c7a2fa84aed9) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Pin the `bn.js` runtime dependency to an exact version (`5.2.3`) instead of the `^5.2.0` range. `5.2.3` is the version the lockfile already resolved, so the SDK's own behaviour is unchanged; the effect is on consumers, who now resolve exactly `5.2.3` rather than any `5.2.x`. This makes `bn.js` consistent with every other runtime dependency in the package, all of which were already exact. Consumers that pull `bn.js` transitively at a higher patch may end up with a second nested copy, in which case `BN` instances will not share a constructor across the SDK boundary — add an `overrides`/`resolutions` entry to force a single copy if that matters for your tree.

- [#343](https://github.com/velocity-exchange/velocity-v1/pull/343) [`ae71278`](https://github.com/velocity-exchange/velocity-v1/commit/ae7127876ef98465ab53d611ee3447db73b224a2) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - `initializeRevenueShareEscrow` / `getInitializeRevenueShareEscrowIx`: the program now rejects
  `numOrders == 0`, so a zero-capacity escrow reverts with `DefaultError` instead of being created in a
  silently inert state where no builder or referral row can be held and all revenue share is suppressed
  (OtterSec #114). `numOrders` must be at least 1; the SDK signature is unchanged.

  Both doc comments also now record that `escrow.referrer` is snapshotted from `UserStats.referrer` once
  at creation and never re-read, so the escrow should be created _after_ the authority's first
  `initializeUser` — otherwise it permanently holds no referrer.

- `initializeRevenueShareEscrow` / `getInitializeRevenueShareEscrowIx` now revert with `UserNotFound`
  (6234 / `0x185A`) when the authority has not created a subaccount yet, so the escrow must be created
  after `initializeUserAccount` rather than after `initializeUserStats` alone (OtterSec #129).

  The escrow snapshots `escrow.referrer` from `UserStats.referrer` once at creation and never rewrites
  it, while `UserStats.referrer` is only ever set by the authority's first `initialize_user`. Because
  the escrow's `authority` does not sign (only the payer does), a third party could previously create
  an escrow in the window between those two calls and freeze a defaulted referrer into it, permanently
  suppressing that authority's referral rewards and referee discount with no way to repair the field.

  No SDK API change: signatures are unchanged and the standard onboarding order (subaccount 0 first,
  which every existing client already uses) is unaffected. Only a flow that creates the escrow before
  the first subaccount needs reordering.

- [#425](https://github.com/velocity-exchange/velocity-v1/pull/425) [`e8a894c`](https://github.com/velocity-exchange/velocity-v1/commit/e8a894c90edd03814330206b8f666591be72a774) Thanks [@0xahzam](https://github.com/0xahzam)! - Correct the slot-duration scaling in the off-chain mirrors and the staged-switch setter.

  - `activeSlotDurationFromState` is now applied wherever the Rust SDK, the swift server, and
    the account-list builder previously read the raw `State.slotDurationMs` base field. That
    field lags a staged switch until the following gate is staged, so the mirrors sized oracle
    staleness windows, the signed-order age limit, and the auction band check off the
    pre-switch duration.
  - `update_state_slot_duration_ms` commits an already-effective promotion when the gate
    schedule is exhausted instead of reverting it, so the base field never stays a step behind
    the live value. After the final 200ms switch, operators finalize the raw base field with one
    additional `set-slot-duration-ms 200` transaction.
  - Reference-price-offset smoothing accrues its budget per elapsed millisecond instead of per
    whole 400ms period. Flooring to whole periods zeroed the budget for any crank gap under
    400ms, which pinned the step to the minimum and made convergence slower the more often a
    market was cranked.
  - `math/time.ts` imports `BN` from the isomorphic entry point, keeping Anchor out of the
    browser bundle.
  - The SDK's oracle staleness allowance is a wall-clock duration rather than a fixed five
    slots.
  - Three velocity/jit-proxy instructions and three vaults instructions now take velocity's
    `State` so their oracle windows match the rest of the protocol. Hand-built transactions
    must add the account; the SDKs and CLI fill it in.
  - `velocity-admin exchange set-slot-duration-ms` previews the live duration instead of the
    base field.
  - `pythLazerCranker`'s post ceiling is a fixed wall-clock interval again, so the post rate
    does not double at each gate.

- [#405](https://github.com/velocity-exchange/velocity-v1/pull/405) [`fabc75c`](https://github.com/velocity-exchange/velocity-v1/commit/fabc75ce6daeb7faf75909ceacaac8ffac257bad) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - **`force_delete_user` could never succeed.** The handler bound `State` with a shared `load()` at the
  top and called `load_mut()` at the bottom. `Ref` implements `Drop`, so the first borrow lived to the
  end of the scope and the shadowing `let` did not end it. Every call reverted with
  `AccountBorrowFailed` — after the account's deposits had already moved to the keeper. Nothing
  covered the success path, so the revert went unnoticed. The borrow is now released explicitly, and
  `tests/velocity/equityFloorOracle.ts` covers the success path.

  Every vault instruction that snapshots NAV now books the lending interest of **every** spot market
  that prices the vault's equity, not just the denomination market.

  `Vault::calculate_equity` delegates to velocity's `calculate_user_equity`, which converts every held
  spot position through that position's own market's cumulative index. Refreshing one market left the
  rest priced off whatever index the last unrelated crank had written. For a borrow the sign flips: a
  stale `cumulative_borrow_interest` understates the liability, so NAV reads high and a withdrawer is
  overpaid out of the vault rather than out of another depositor.

  **New velocity instruction `refresh_spot_market_interest`.** It books up to sixteen spot markets in
  one call. Accounts: `state`, plus the markets as writable accounts in remaining accounts. Argument:
  `market_indexes: Vec<u16>`. Permissionless, like the single-market
  `update_spot_market_cumulative_interest` crank beside it, which is unchanged and stays the crank
  that keeps a spot market's oracle EMA fresh. SDK: `VelocityClient.refreshSpotMarketInterest` and
  `refreshSpotMarketInterestIx`.

  **The refresh passes no oracle.** `calculate_equity` gates the denomination oracle on
  `is_oracle_valid_for_action(MarginCalc)`, whose `TooVolatile` arm measures the live price against
  `last_oracle_price_twap`. The previous refresh advanced that TWAP toward the live price immediately
  before the check read it.

  **A delisted denomination market no longer blocks every vault instruction.** The refresh carries no
  `spot_market_valid` guard, so the paths that move no tokens keep working: `request_withdraw`,
  `cancel_withdraw_request`, `apply_rebase`, `apply_profit_share` and `liquidate`. Delisting is a
  terminal state, so the previous behavior had no recovery at all. The token-moving paths
  (`withdraw`, `force_withdraw`, `manager_withdraw`) still fail, because velocity's own withdraw
  admits only `Active`, `ReduceOnly` and `Settlement` — that gate is unchanged and out of scope here.
  Nothing about delisted markets changes: `deposit`, `force_delete_user` and `resolve_spot_bankruptcy`
  already book interest on one.

  **Isolated perp positions are covered too.** Such a position holds collateral that prices through
  its perp market's quote spot market, which the position itself does not name. The market list picks
  those up from the perp market accounts already present for the equity walk, and does that walk only
  when the user holds an isolated position, so an ordinary vault pays nothing for it.

  **ABI change — 20 instructions, accounts removed.** `velocity_spot_market` and `velocity_oracle` are
  removed from all of them, and `velocity_spot_market_vault` from the thirteen that do not need it for
  a deposit or withdraw CPI of their own. Each keeps `velocity_state` and `velocity_program`.
  `manager_update_fees` joins the list, because installing a matured fee update snapshots NAV.
  Affected: `deposit`, `manager_deposit`, `withdraw`, `manager_withdraw`, `protocol_withdraw`,
  `force_withdraw`, `request_withdraw`, `manager_request_withdraw`, `protocol_request_withdraw`,
  `cancel_withdraw_request`, `manager_cancel_withdraw_request`, `protocol_cancel_withdraw_request`,
  `apply_rebase`, `apply_rebase_tokenized_depositor`, `apply_profit_share`, `tokenize_shares`,
  `redeem_tokens`, `transfer_vault_depositor_shares`, `liquidate`, `manager_update_fees`.

  `VaultClient` builds every affected instruction, so SDK callers need no change. Anyone hand-rolling
  account lists must drop the removed accounts and must mark every spot market in the remaining
  accounts writable — velocity fails the load with `SpotMarketWrongMutability` when it is asked to
  refresh a market it was handed read-only.

## 0.13.0

### Minor Changes

- [#356](https://github.com/velocity-exchange/velocity-v1/pull/356) [`872edd6`](https://github.com/velocity-exchange/velocity-v1/commit/872edd66c5d94a01d6c694a06b03c4ed20c2054c) Thanks [@ChesterSim](https://github.com/ChesterSim)! - JupiterClient: opt-in support for Jupiter Swap API v2 (`GET /swap/v2/build`) via `apiVersion: 'v2'` — one HTTP round trip for quote + instructions instead of `/quote` then `POST /swap` plus a transaction deserialization. v2 quotes are wallet-bound, so `getQuote` requires `userPublicKey` and rejects the quote if it is later swapped by a different wallet; `autoSlippage` is not supported on v2 (the API silently ignores it and returns zero slippage tolerance) and throws, directing callers to `apiVersion: 'v1'`. `swapMode: 'ExactOut'` likewise throws — `/swap/v2/build` is ExactIn-only and reinterprets an ExactOut amount as the input. `onlyDirectRoutes: true` also throws: `/swap/v2/build` has no direct-only routing control and silently ignores the parameter, still returning multi-hop routes. `getSwapTransaction` gains an optional `computeUnitLimit`, since v2 returns a compute unit price but no limit. `UnifiedSwapClient` accepts a matching `jupiterApiVersion` option. The former v1-only `JupiterClient.getSwap` is removed — use `getSwapTransaction` (standalone tx) or `getRouteInstructions` (route for velocity's begin/end swap bracket); under v1, `getSwapTransaction` still posts to `/swap`. The default remains `'v1'`, so nothing changes unless you opt in or were calling `getSwap` directly.

## 0.12.0

### Minor Changes

- [#350](https://github.com/velocity-exchange/velocity-v1/pull/350) [`c08211e`](https://github.com/velocity-exchange/velocity-v1/commit/c08211e4c106f57a1092238adb5c6d14895734f5) Thanks [@ChesterSim](https://github.com/ChesterSim)! - `PriorityFeeSubscriber` accepts an optional `fetchSolanaPriorityFee` override in its config, defaulting to the built-in `getRecentPrioritizationFees` RPC call. Lets an upstream route SOLANA fee sampling through its own cache/proxy instead of hitting the RPC directly.

## 0.11.0

### Minor Changes

- [#325](https://github.com/velocity-exchange/velocity-v1/pull/325) [`2a73aa7`](https://github.com/velocity-exchange/velocity-v1/commit/2a73aa716aed4f5910893c0ad9012bd598f72844) Thanks [@0xahzam](https://github.com/0xahzam)! - Account extension: new `extend_account` instruction grows a zero-copy account (`User`, `UserStats`, `PerpMarket`, `SpotMarket`, `State`, ...) to the size the deployed program compiles in for its discriminator's type, as the migration crank for a future upgrade that appends fields to an account struct (see `docs/ACCOUNT-EXTENSION.md`). Gated on the new `HotRole.AccountExtension` (cold, warm, or the configured hot key; `StateAccount` gains `hotAccountExtension`, carved from tail padding with the account size unchanged). Grow-only with the target compiled in, payer covers the rent-exempt shortfall, tail zero-filled by the runtime, no-op when already at size; borsh accounts and unknown discriminators are rejected with the new `InvalidAccountExtension` error (6367). A devnet/test-only `extend_account_devnet(new_len)` (compiled out of production mainnet builds, kept by `anchor-test`; same role gate) grows to an arbitrary larger size so the flow can be exercised before a real extension exists. SDK: `VelocityClient.extendAccount`/`getExtendAccountIx` and the devnet variants. Admin CLI: new `extend-account` command, single-account mode plus a `--type <t>` batch crank that scans by discriminator, skips at-size accounts, and supports `--dry-run` and `--batch-size`; assign the role with `auth set-hot-admin accountExtension <pubkey>`.

- [#336](https://github.com/velocity-exchange/velocity-v1/pull/336) [`6e29daf`](https://github.com/velocity-exchange/velocity-v1/commit/6e29daf0ef781986dbe3bbf79f1c9bd2e25eb646) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Stop discarding the AMM's dynamic spread when `base_spread` is 0, so quoted vAMM prices match what the program will fill at.

  `calculateSpread` short-circuited on `amm.baseSpread == 0 || amm.curveUpdateIntensity == 0` and returned `[baseSpread / 2, baseSpread / 2]`. On chain, `update_spreads` (`vlp/amm/math/spread.rs`) branches on `curve_update_intensity > 0` alone: `base_spread` is only the floor that the vol spread is maxed against (`long_spread = max(half_base_spread, long_vol_spread)`), so a `base_spread` of 0 does not disable the dynamic spread. Any market configured with `base_spread = 0` therefore had its entire inventory/volatility spread thrown away off chain while the program kept applying it.

  This was not a rounding difference. On devnet SOL-PERP (`base_spread: 0`, `curve_update_intensity: 100`, `max_spread: 142500`) the AMM was short 108.6 base — 10.9% of `sqrt_k` — which drove an inventory scale of ~8.4x and `long_spread` to 111985 (~11.2%) on chain, against a `short_spread` of 440. The SDK reported `[0, 0]` for that same state, so `calculateSpreadReserves` returned the unadjusted reserves and `calculateAskPrice`, `calculateBidPrice` and `calculateReservePrice` all collapsed onto the same number. `getVammL2Generator` then published a best vAMM ask of ~$80.03 — pure curve price impact with no spread — where the program's ask was ~$82.12 against a ~$73.96 oracle.

  Everything downstream of that book inherited the error. `dlob-server`'s `/auctionParams` priced a 1 SOL long at `entryPrice`/`worstPrice` $80.034 with `priceImpact: 0` and set the oracle-offset auction end to +$6.149, i.e. a maximum price of $80.114 — about $2.00 short of the real ask, so the order could not cross at any point in its auction and expired unfilled with `taker does not cross amm` / `no fulfillment methods found` in the program logs. The on-chain helper for the same job, `OrderParams::get_perp_baseline_start_end_price_offset`, folds `amm_spread_side_pct * oracle_twap` into its end-price buffer and would have produced a crossable order; only the off-chain path was blind to the spread.

  Two smaller divergences in the same function are fixed alongside it, both verified against `update_spreads`:

  - **`amm_spread_adjustment` was skipped on the frozen-curve branch.** On chain it is applied after the `curve_update_intensity` branch, so it affects both the dynamic and the `[base_spread / 2, base_spread / 2]` result. The early return meant a market with `curve_update_intensity == 0` ignored its manual adjustment entirely. The adjustment is now factored into `applyAmmSpreadAdjustment` and applied to both branches, rounding as the program does — ceil when growing, floor when shrinking — rather than leaving a fractional spread.
  - **`base_spread / 2` was float division.** `base_spread.safe_div(2)` truncates on chain, so a `base_spread` of 175 (the value BTC-PERP and ETH-PERP carry) yielded 87 on chain and 87.5 in the SDK.

  `AdminClient.getMoveAmmToPriceIx` now passes `getMMOracleDataForPerpMarket` into `calculateTargetPriceTrade` rather than `undefined`. It was already sizing the move against a zero-width spread on markets with a nonzero `baseSpread` and `curveUpdateIntensity`, and with the change above would have started throwing on `baseSpread == 0` markets as well.

  **Behavioural note.** `calculateSpread` and `calculateSpreadReserves` now throw when `oraclePriceData` is omitted and `curveUpdateIntensity` is nonzero, where a `baseSpread` of 0 previously returned `[0, 0]` without needing an oracle. This is the same requirement markets with a nonzero `baseSpread` already had; callers on affected markets were silently receiving a zero-width spread instead. `getVammL2Generator` already requires `mmOraclePriceData`, so the in-repo L2 path is unaffected.

  `calculateSpreadBN`, which does the actual work, was already faithful to the Rust — no spread math changed here, only whether it gets called. Driven through `calculateSpread`, the captured snapshot above now yields a `long_spread` of 111990 against the 111985 the market carried on chain, and a `short_spread` of 441 against 440. The small drift is expected: the SDK derives `liveOracleStd` and the confidence percentage from `now` rather than reading back the values the on-chain crank used.

## 0.10.0

### Minor Changes

- [#328](https://github.com/velocity-exchange/velocity-v1/pull/328) [`63a580e`](https://github.com/velocity-exchange/velocity-v1/commit/63a580ea3c31a21fb8820fe75075d799cc8dc3da) Thanks [@0xahzam](https://github.com/0xahzam)! - Equity floor measured as net equity; strictly reducing swaps allowed under the breaker (audit fixes #119/#120).

  - **#120** — every equity-floor check (breaker trip, withdrawals, transfers, risk-increasing order placement/fills/triggers, force-cancel eligibility) now measures **net equity**: unweighted asset value plus funding-inclusive perp PnL minus unweighted spot liability value, at live oracle prices (`calculate_user_equity`). Previously the checks compared the margin numerator (`total_collateral`), which never subtracts spot borrow value (a user could hollow out equity via borrow-withdrawals without tripping) and understates healthy accounts via asset weights, strict pricing, and the positive-PnL clamp (a permissionless keeper could freeze a healthy authority). `trip_equity_floor_breaker` additionally rejects with `InvalidOracle` when any of the subaccount's oracles is invalid.
  - **#119** — `end_swap` no longer blanket-rejects while the breaker is tripped. A strictly reducing swap (consumes an existing deposit to repay an existing borrow, creating no new liability or deposit exposure) is allowed under the breaker and below the floor, so a frozen account can deleverage instead of being forced into a liquidation loss. While under floor protection the exempted swap's execution value is bounded against oracle (`EQUITY_FLOOR_SWAP_MAX_VALUE_LOSS_BPS`, 1%), so a "reducing" swap routed at a bad price cannot leak value out of a frozen account.
  - **Trigger reward ordering** — `trigger_order` evaluates the risk-increasing floor/breaker/margin cancel condition before paying the keeper's flat trigger reward: a trigger that resolves to a cancel pays no reward, so a keeper can no longer farm the reward out of a frozen or below-floor account (the residual loss path of OtterSec #54).
  - **Buffer travels with floor** — `transfer_equity_floor` carries a proportional share of the debited side's `equity_floor_buffer` along with the floor delta (rounded up on the debited side; sums of floors and buffers both conserved), so shedding the whole floor also sheds the whole buffer and no orphan buffer is left behind on a floorless, check-disabled subaccount.

  SDK (breaking): `User.isBelowEquityFloor` / `isBelowBufferedEquityFloor` / `getEquityAboveFloor` / `getEquityAboveBufferedFloor` now use `getNetUsdValue()` and drop their `strict` parameter; `calculateEquityFloorAutoDelta` / `getEquityFloorLevel` take `netEquity` instead of `totalCollateral`; `EquityFloorManager` and `transferDepositByDelegate`'s `'auto'` floor delta size against net equity. No account-layout or instruction-signature changes.

- [#331](https://github.com/velocity-exchange/velocity-v1/pull/331) [`2219857`](https://github.com/velocity-exchange/velocity-v1/commit/2219857615aeb4cd11b5ace8a203279797f41c88) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Put Jupiter and Titan behind one `SwapProvider` interface, and carry a quote's route on the quote itself.

  **Breaking.** `TitanClient.getSwap` and `TitanClient.getTitanInstructions` are removed; both providers now expose `getRouteInstructions({ quote, userPublicKey })`. `getQuote` returns a `SwapQuote` (the previous fields plus a `providerRoute` payload, typed `SwapProviderRoute`), so any parameter typed `QuoteResponse` or `UnifiedQuoteResponse` becomes `JupiterSwapQuote` or `SwapQuote` — this affects `swap`, the `AdminClient` swap helper, and the `superStake` `jupiterQuote` parameters. `UnifiedSwapClient.getSwap`, whose result was a union of optionals the caller had to discriminate, is removed, and the types it existed to describe — `SwapTransactionParams` and `SwapTransactionResult` — are removed with it; its two use cases are now separate methods, `getRouteInstructions` (route only, for velocity's `beginSwap`/`endSwap` bracket) and `getSwapTransaction` (a complete standalone transaction, setup and teardown included, for swaps the caller signs and sends itself). `getRouteInstructions` takes no `slippageBps`: the quote's own `slippageBps` is authoritative, since that is the price the caller was shown. `JupiterClient.getJupiterInstructions` still works but is deprecated.

  The two providers had drifted because nothing forced them to agree. `UnifiedSwapClient.getSwapInstructions` branched on client type into two unrelated implementations: the Jupiter branch built from the `quote` argument, while the Titan branch ignored every argument except `userPublicKey` and replayed a route held in private client state (`lastQuoteData`), populated by whichever `getQuote` had run last on that instance and cleared after a single use. A caller passing a quote got the route it named on Jupiter and an unrelated cached route on Titan, with nothing at the call site to distinguish the two. When the cached route was for a different pair, the swap executed and deposited the output into a token account `end_swap` wasn't watching, failing on-chain with `InvalidSwap: amount_out must be greater than 0` after the route had already run. `lastQuoteParams` was recorded for a match check that was never implemented.

  Both clients now implement `SwapProvider`, and `UnifiedSwapClient` forwards through that interface rather than the concrete client union, so a provider that only has half the contract fails to compile. `getQuote` attaches the provider payload to the quote it returns and `getRouteInstructions` reads it from there, making route building a pure function of its arguments: no client state, no ordering requirement between quote and swap, no single-use cache, and a clear error when a quote from one provider is handed to another. `UnifiedSwapClient` has no per-provider branches, and provider-specific request fields are mapped by the provider that understands them rather than being stripped in the unified layer.

  Four further ways a route could fail to match the swap it was built for are now rejected rather than executed:

  - **Wrong pair.** `getProviderSwapIx` and the liquidation swap helper throw unless the quote's mints are the mints of the spot markets `beginSwap`/`endSwap` are being built for. Previously a mismatched quote routed normally and reverted on-chain with `amount_out must be greater than 0` only after funds had moved.
  - **Wrong wallet.** Titan resolves the user's token accounts at quote time, so its routes are wallet-bound; the quoting wallet is recorded on the route and enforced when the route is built. Jupiter builds per-wallet at swap time and is unaffected. **Breaking:** `expectProviderRoute`, the shared check both providers build this on, now requires `userPublicKey` rather than treating it as optional — a caller that couldn't supply the executing wallet previously skipped the check silently instead of being unable to call it.
  - **Wrong slippage.** Jupiter's `/swap` defaulted to 50bps, so a quote priced at any other slippage silently executed at 50 while Titan honoured what it had baked in. Both now build at the quote's slippage.
  - **Wrong size.** A quote used by `getProviderSwapIx` must be for the amount being swapped (its `inAmount` under `ExactIn`, its `outAmount` under `ExactOut`), whether it was passed in or fetched by `getProviderSwapIx` itself. `beginSwap` releases funds sized off the quote, so a quote for a different size moved the wrong amount out of the user's deposits. The effective mode is likewise read from the quote on both paths — previously a fetched quote was sized against the mode that had been _requested_, which is the wrong side if the provider answers in the other one. The liquidation swap helper applies the same size check.
  - **Wrong route for the quote's own fields.** A `SwapQuote`'s normalized fields are what callers and velocity's guards read, but the opaque `providerRoute` beside them is what executes. A quote now records what its route was quoted with, and `expectProviderRoute` rejects one whose pair, size, mode or slippage no longer matches — so a modified copy of a returned quote (`{ ...quote, inAmount }`, a mint rewritten in place) is rejected instead of passing every guard and executing the swap it was originally quoted for. Treat a returned quote as immutable and re-quote to change it. Providers get this by returning `buildSwapQuote(normalizedQuote, providerRoute)` from `getQuote` instead of assembling the object themselves; the recorded fields (`SwapRouteFields`) are never written by hand. Titan additionally checks the pair in its own response against the pair requested, instead of assuming the request was honoured.

  **Breaking.** The three near-identical per-provider builders on `VelocityClient` — `getSwapIxV2`, `getJupiterSwapIxV6` and `getTitanSwapIx` — are replaced by one `getProviderSwapIx({ swapProvider, ... })` taking any `SwapProvider`, and `swap()` no longer branches on client type. `swap`'s `swapClient` parameter is typed `SwapProvider`, and its `v6` parameter (a deprecated `{ quote?: QuoteResponse }` wrapper predating the unified `quote` parameter) is removed. The three differed in ways that were bugs rather than provider requirements: only two forwarded a pre-fetched `quote` (the Titan path discarded it and re-quoted), only two checked the quote's pair, and only one sized `beginSwap` off the quote's own input. Under `ExactOut`, `getSwapIxV2` buffered the caller's `amount` — an _output_ amount — and released that as the input.

  `TitanClient` decodes its MessagePack quote with `useBigInt64`. The default decoder converts 64-bit integers to JavaScript numbers, so any u64 above 2^53 was silently rounded to the nearest double — including the route `inAmount` that funds `beginSwap`. Amount fields on the decoded route are `bigint | number` and are converted to decimal strings without passing through `Number`.

  `TitanClient.getQuote` now reports the route's own `inAmount` rather than the requested amount. The two coincide under `ExactIn`, but under `ExactOut` the request is the _output_, so the quote claimed an input equal to the desired output and `beginSwap` was sized off it. Jupiter already reported the route's input; this makes the two agree.

  **Breaking.** `JupiterClient.getSwap({ quote, userPublicKey, slippageBps? })` drops the `slippageBps` override; it now always executes at the quote's own `slippageBps`, matching Titan (which never took one) and closing the wrong-slippage gap above at the single-provider level too. `JupiterClient.lookupTableCahce` is renamed to `lookupTableCache`.

  The instruction filter that strips a route's compute-budget, token, system, and input/output ATA instructions is now a single shared `filterRouteInstructions`, replacing near-identical copies in each client — the Jupiter copy indexed `keys[3]` unguarded and threw on a short ATA instruction where the Titan copy kept it. It takes the route's `instructions: TransactionInstruction[]` directly rather than a `TransactionMessage` — a route no longer has to be compiled into a message just to be filtered. The five hand-rolled `getSwap` → `getTransactionMessageAndLookupTables` → `getXInstructions` sequences across `velocityClient`, `adminClient`, and the keeper bots collapse to one `getRouteInstructions` call each.

## 0.9.0

### Minor Changes

- [#322](https://github.com/velocity-exchange/velocity-v1/pull/322) [`c16315e`](https://github.com/velocity-exchange/velocity-v1/commit/c16315e594120afdeb10f832c64914da01cbddcb) Thanks [@0xahzam](https://github.com/0xahzam)! - Equity floor buffer: `User.equity_floor_buffer` (carved from the last 8 tail-padding bytes, account size unchanged) adds admin-set headroom above the equity floor. Every risk-increasing gate (order placement/fills, withdrawals, swaps, deposit/position transfers out, trigger activation, floor-transfer to-side) now enforces `total_collateral >= equity_floor + equity_floor_buffer`, while the permissionless breaker still trips at the raw floor — so no permitted action can leave a subaccount trippable; only a passive drawdown through the whole buffer can arm the breaker. `updateUserEquityFloor(user, equityFloor, equityFloorBuffer)` sets both (breaking signature change, on-chain and in `AdminClient`). SDK: `UserAccount.equityFloorBuffer`, `User.isBelowBufferedEquityFloor`/`getBufferedEquityFloor`/`getEquityAboveBufferedFloor`, pure `calculateEquityFloorAutoDelta` and `getEquityFloorLevel` helpers, and a new `EquityFloorManager` that abstracts the per-subaccount mechanics for delegates: aggregate status + levels, haircut-padded `transferQuote`/`planQuoteTransfer`, `getMaxWithdrawable`/`getMaxQuoteTransferable`, and proportional-to-equity `rebalanceFloors` via zero-amount floor moves. `transferDepositByDelegate` `'auto'` now targets `floor + buffer`. Admin CLI: `user set-equity-floor <user> <floor> <buffer>` (breaking), new `user equity-floor-status <authority>` and `user close-positions` closure sweep.

### Patch Changes

- [#306](https://github.com/velocity-exchange/velocity-v1/pull/306) [`d142320`](https://github.com/velocity-exchange/velocity-v1/commit/d14232017da7b09be1a71af8c5f6ee889ccac745) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Perp bankruptcy no longer double-credits the AMM with phantom funding (High audit fix #89). `resolve_perp_bankruptcy` socializes residual bad debt by bumping `cumulative_funding_rate_long`/`_short` so surviving longs and shorts both owe funding covering the loss, but it never resynced the AMM's own `amm.last_cumulative_funding_rate_long`/`_short`. On the next `update_funding_rate`, `calculate_amm_funding_payment` derives the AMM's payment from `(cumulative_rate − amm.last_cumulative_rate)` — which now carried the bankruptcy bump on both legs — so for a balanced book the AMM "received" phantom revenue into `total_fee_minus_distributions` (≈ `D·G1/G0`, able to exceed the socialized loss as gross OI grows), spendable as funding payouts to survivors or as curve budget. The socialization block now first fully settles the AMM's own funding through the current (pre-socialization) cum rates against its net position (via `record_amm_pnl`, the same payment the `FundingUpdated` handler applies), then resyncs the AMM stamp to the bumped cumulative rates — so no genuine accrued AMM funding is dropped and only the socialization delta is excluded from the AMM's next funding payment. Program-internal only; no layout/IDL/error/SDK-API change (the SDK does not predict AMM funding across a bankruptcy).

- [#309](https://github.com/velocity-exchange/velocity-v1/pull/309) [`25da8e1`](https://github.com/velocity-exchange/velocity-v1/commit/25da8e1e39ccbb8310de32dd0da29041f2a93a0c) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Builder-code fee integrity (two Medium audit fixes). Adds one error variant to the IDL; no account-layout change.

  - **#82** — builder fees could be silently dropped. `add_builder_order` now propagates `RevenueShareEscrowOrdersAccountFull` instead of returning `Ok(None)` (which placed the order with no `HasBuilder` bit and charged no fee) when the escrow is full; `revoke_completed_orders` now decides whether an order is still open by matching `(sub_account_id, order_id)` across the whole order list rather than trusting the row's stored `user_order_index` (which can go stale and clear a still-open fee-bearing row early); and `modify_order` now rejects a builder-coded order (`CannotModifyBuilderOrder`, error 6366) rather than silently stripping attribution on the cancel-and-replace — cancel and re-place with builder params to change a builder order.
  - **#83** — a self-approved builder could route up to ~65.5% of notional (a `u16::MAX` fee, with no global ceiling) out through a maintenance-margin-gated position-decreasing fill, moving value the taker couldn't withdraw under initial margin. A new global cap `MAX_BUILDER_FEE_TENTH_BPS` (1000 tenth-bps = 1%, tunable) bounds the actual builder fee charged, independent of the builder's own configured `max_fee_tenth_bps`.

  Adds error `CannotModifyBuilderOrder` (6366) to the IDL. No instruction/account-layout change; error codes are read from the IDL (no manual `types.ts` mirror).

- [#308](https://github.com/velocity-exchange/velocity-v1/pull/308) [`e34c623`](https://github.com/velocity-exchange/velocity-v1/commit/e34c6233afa1e04c7a4ff4a3f088405508f85790) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Order placement & settlement gating (four Medium audit fixes). Program-internal behavior changes; no instruction, account-layout, IDL, or error-code change (reuses existing error variants).

  - **#84** — signed-message bundles are now atomic around the main order: `place_signed_msg_taker_order` pre-checks the main taker order's `max_ts` before placing anything and skips the whole bundle if it has already expired, so the reduce-only TP/SL sidecars (which are exempt from `max_ts` expiry) are no longer installed as standalone triggers when the main entry would soft-skip. The sidecars keep their existing order ids (the main keeps the trailing id clients rely on).
  - **#85** — the signed-message taker path now rejects immediate-or-cancel orders (`InvalidOrderIOC`), matching the direct/batch place paths. A signed IOC limit order can no longer be stored as an indefinitely-resting order (limit orders default `max_ts` to 0).
  - **#86** — `trigger_order` now enforces the same `is_in_settlement` gate the place/fill paths use, so a keeper can't trigger a dormant order (and collect the flat reward) on an expired/settling market.
  - **#87** — `transfer_perp_position` now rejects transfers once the market is expired / in settlement, so an authority can't split a live-oracle gain from the matching fixed-`expiry_price` loss across two of its own subaccounts.

- [#305](https://github.com/velocity-exchange/velocity-v1/pull/305) [`edfc846`](https://github.com/velocity-exchange/velocity-v1/commit/edfc8469b5b4058f8767f1e48075b384fb809b4f) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Revenue-share accounting correctness (two related High audit fixes). Both change observable on-chain behavior but touch no SDK code — the SDK does not reimplement either path.

  - **#88** — the fill-time builder-order lookup matched an escrow row on `(sub_account_id, order_id)` only, ignoring the market. Order ids are per-subaccount and reused across markets, and a builder row can outlive its order, so a stale market-A row could attach to a same-id fill in market B: the market-B taker was charged a builder fee that accrued to — and was later swept from — market A's PnL pool. The fill path now uses `find_builder_order_index`, which additionally requires the row's `market_index`/`market_type` to equal the fill's, that it still be `Open` (not a `Completed` row whose id is stale), and that it not be a referral row.
  - **#90** — `calculate_perp_market_amm_summary_stats` (the balance-sheet recompute an `AmmCrank` commits into `amm.total_fee_minus_distributions`) subtracted `net_user_pnl` and the pending protocol/IF counters but not accrued-but-unswept builder/referrer revenue share, so it counted that PnL-pool liability as retained AMM equity and inflated the funding/curve budget by the owed amount. It now also subtracts `PerpMarket.pending_revenue_share`, matching the reservation the fee sweep already applies.

  No instruction, account-layout, IDL, or error-code change.

- [#330](https://github.com/velocity-exchange/velocity-v1/pull/330) [`5fac99b`](https://github.com/velocity-exchange/velocity-v1/commit/5fac99bb93343d93c2a58e776fdba888588f89a2) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Fix strict-mode spot margin pricing to use the stored 5-minute oracle TWAP, matching the program.

  `User.getSpotMarketAssetAndLiabilityValue` and `User.getMarginCalculation` built their spot `StrictOraclePrice` from `calculateLiveOracleTwap`, which time-weights the stored TWAP back toward the live oracle price by the age of `lastOraclePriceTwapTs`. Spot markets only advance that timestamp when the market is touched, so it is routinely older than the 5-minute window — the projection then returned the live price exactly and strict mode became a no-op. `calculate_margin_requirement_and_total_collateral_and_liability_info` in the program uses `spot_market.historical_oracle_data.last_oracle_price_twap_5min` verbatim, so the SDK understated initial margin requirements (and overstated free collateral) for accounts holding spot borrows against a stale TWAP — a UI-computed "max withdrawal" could exceed what the program allows and fail with `InsufficientCollateral`.

  Both call sites now read `historicalOracleData.lastOraclePriceTwap5Min` directly. Non-strict behavior is unchanged, and perp/AMM/funding uses of `calculateLiveOracleTwap` are untouched. The `now` parameter of `getSpotMarketAssetAndLiabilityValue` (and its wrappers) is retained for signature compatibility but no longer has any effect.

- [#330](https://github.com/velocity-exchange/velocity-v1/pull/330) [`5fac99b`](https://github.com/velocity-exchange/velocity-v1/commit/5fac99bb93343d93c2a58e776fdba888588f89a2) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Apply strict (TWAP-bounded) spot pricing to swap sizing, matching the program.

  `User.getMaxSwapAmount` built its in/out `StrictOraclePrice` with no TWAP argument, so both legs of a swap were valued at the live oracle price and strict mode was unconditionally off. `handle_end_swap` in the program builds both strict prices from each market's `historical_oracle_data.last_oracle_price_twap_5min` with `strict = true` before calling `select_margin_type_for_swap`, and the resulting margin check runs with `strict = true` whenever the swap worsens free collateral (`meets_withdraw_margin_requirement_swap`). The SDK therefore over-valued the bought asset (deposits should price at `min(oracle, twap5min)`) and under-valued any borrow the swap opens (liabilities should price at `max(oracle, twap5min)`), so a UI-computed max swap could exceed what the program allows. The base `getFreeCollateral()` the search starts from is already strict, so the mismatch also made the search's free-collateral delta inconsistent with its own starting point.

  The free-collateral search now passes the relevant spot market's stored `lastOraclePriceTwap5Min` for both the in and out sides, unconditionally, as the handler does.

  Strict pricing is deliberately confined to that sizing path. The `leverage` field `getMaxSwapAmount` returns, and everything `accountLeverageAfterSwap` returns, are deltas applied to the non-strict live-oracle baseline from `getLeverageComponents()`; pricing those deltas strictly would blend two price bases inside one figure and yield a valuation matching neither the oracle nor the TWAP. Both leverage readouts therefore stay on the live oracle price — unchanged from before this release, and comparable to the account leverage `getLeverage()` reports.

- [#307](https://github.com/velocity-exchange/velocity-v1/pull/307) [`0ac1f73`](https://github.com/velocity-exchange/velocity-v1/commit/0ac1f730d0bdc5420ae0efd0ec12a1eb017fa542) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Vault share-pricing hardening (four High audit fixes, #91/#92/#93/#94).

  Builder/referral rewards owed to a vault PDA accrue in arbitrary third-party escrows and only enter the vault-owned Velocity User's equity via a permissionless revenue-share sweep — at a time an attacker controls. Because the vault can't see or enumerate those rewards (and no legitimate flow has a vault earn revenue share — a third party can name a vault PDA as their builder with no signature), the reward is blocked at the source instead:

  - **#91/#92/#93** — new `UserStatus::VaultOwned` bit (`UserStatus.VAULT_OWNED = 32`) marks a vault-owned User; the vaults program sets it at `initialize_vault` via a new CPI to the new velocity instruction `update_user_vault_owned` (authority-gated, set-only). `sweep_completed_revenue_share_for_market` now skips crediting a builder/referral reward to a vault-owned User — draining the liability counter and clearing the row without transferring, so the reward stays in the market's PnL pool and can never enter vault NAV. This closes late-entrant dilution (#91), the stranded pending-withdrawer (#92), and the reward-donation-burns-a-canceller's-claim vector (#93) at the root.
  - **#93 (defense-in-depth)** — `VaultDepositor::deposit` now rejects a positive deposit that mints zero shares (mirrors `request_withdraw`'s guard and the IF `IFDepositMintsZeroShares` path); `WithdrawRequest::calculate_shares_lost` rejects a cancel that would floor a positive claim's retained shares to zero purely because equity rose.
  - **#94** — `Vault::calculate_equity` now fetches the denomination-market oracle with `get_price_data_and_validity` and gates it with `VelocityAction::MarginCalc` (rejecting NonPositive/TooVolatile/TooUncertain/StaleForMargin), instead of a raw unchecked `get_price_data`. Previously a stale-high denomination oracle could shrink NAV and overmint shares whenever the vault held no denomination position (so the margin walk never validated that oracle).

  SDK: adds `UserStatus.VAULT_OWNED` and the `updateUserVaultOwned` instruction to the IDL. No account-layout change (`VaultOwned` reuses a spare `status` bit; existing accounts read 0). `update_user_vault_owned` is CPI-only (called by the vaults program at vault init), not a client-facing builder.

## 0.8.0

### Minor Changes

- [#185](https://github.com/velocity-exchange/velocity-v1/pull/185) [`1f866f1`](https://github.com/velocity-exchange/velocity-v1/commit/1f866f1c44def526aeef8927fe14d563f3a8ed7b) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Add per-market configurable withdraw circuit breaker and a daily deposit rate
  cap.

  The previously-hardcoded 25% daily withdraw circuit breaker is now configurable
  per spot market via `SpotMarketAccount.withdrawCircuitBreakerBps` (basis points,
  10000 = 100%; `0` keeps the default 25% = 2500 bps). A new daily deposit rate
  cap mirrors the withdraw side: `depositGuardThreshold` (no cap below it) and
  `maxDepositBpsPerDay` (basis points; `0` disables) bound how far resulting
  deposits may exceed the 24h deposit TWAP. It is enforced on the direct `deposit`
  instruction and on the shared spot-credit path, so `transfer_pools` and
  `end_swap` deposit credits are bounded too; it reverts with the new
  `DailyDepositLimit` (6364) program error.

  All three fields are carved from the existing 13-byte alignment gap before
  `protocol_fee_pool`, so `SpotMarket` stays 808 bytes with every other field
  offset unchanged and no account migration. Existing markets read the repurposed
  bytes as 0 (default 25% breaker, disabled deposit cap). The two percentage
  fields are `u16` basis points rather than `u32` PERCENTAGE_PRECISION so the set
  fits the gap; `depositGuardThreshold` stays `u64` (token amount).

  SDK: `SpotMarketAccount` gains `withdrawCircuitBreakerBps`,
  `depositGuardThreshold`, and `maxDepositBpsPerDay`; new
  `AdminClient.updateSpotMarketWithdrawCircuitBreaker` /
  `updateSpotMarketDepositCap` (and their `getUpdate…Ix` builders); new math
  helpers `calculateMaxDepositTokenAmount` / `checkDepositLimits`; the existing
  `calculateWithdrawLimit` now honors the configurable breaker (all in basis
  points).

  Admin CLI: new `spot-market set-withdraw-breaker <market> <pct>` and
  `spot-market set-deposit-cap <market> <threshold> <pctPerDay>` commands (pct in
  basis points).

- [#271](https://github.com/velocity-exchange/velocity-v1/pull/271) [`943095b`](https://github.com/velocity-exchange/velocity-v1/commit/943095b975dff10791b0e287df462d2ecf176aea) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Signed-message ("swift") taker-order hardening (OtterSec Medium findings). `placeSignedMsgTakerOrder` is now rejected while the exchange is fully paused (`ExchangePaused`), matching normal order placement. `resizeSignedMsgUserOrders` may only be shrunk by the account's `authority` — a per-sub-account delegate can no longer shrink the authority-scoped replay account and evict other sub-accounts' replay protection. The redundant `user` account was removed from the on-chain `resizeSignedMsgUserOrders` instruction, so `VelocityClient.resizeSignedMsgUserOrders` and `getResizeSignedMsgUserOrdersInstruction` drop their trailing `userSubaccountId` parameter.

### Patch Changes

- [#269](https://github.com/velocity-exchange/velocity-v1/pull/269) [`ac29aa1`](https://github.com/velocity-exchange/velocity-v1/commit/ac29aa129c2cac14099a92b4a484bf20d2974863) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - AMM-quoter audit fixes (protocol-side behavior; the SDK's off-chain prediction now matches on-chain again). The vAMM limit-fill cap now always sizes off the spread-adjusted ask/bid reserves — including when `baseSpread == 0` but the vol/inventory spread is non-zero — so a take can no longer execute past the taker's limit price; this aligns the program with `calculateMaxBaseAssetAmountToTrade`, which already sized off `calculateSpreadReserves` unconditionally (no SDK code change was needed). The fill-triggered funding update now evaluates the mark/oracle divergence gate (and the oracle-TWAP sanitization sharing that reserve) against the post-fill reserve rather than the pre-fill one; a zero-fill quote step no longer stamps the mark-TWAP ahead of a real same-timestamp maker trade; and AMM JIT participation in a permissionless DLOB match is now throttled by the per-fill `maxFillReserveFraction` available-liquidity bound (a deliberate divergence from upstream Drift's JIT sizing).

- [#256](https://github.com/velocity-exchange/velocity-v1/pull/256) [`88642ad`](https://github.com/velocity-exchange/velocity-v1/commit/88642ad1784af54c9ca0df271535b32a69cbe517) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Builder revenue-share stale-order fix (High audit fix): a `RevenueShareOrder` written by `add_builder_order` before `place_perp_order` runs could be orphaned when placement soft-skipped on an expired `max_ts` (which returns before `next_order_id` is consumed), leaving the row keyed to an order id a later non-builder order reuses. Fill-time lookup matched only `(sub_account_id, order_id)` with no live `HasBuilder` check, letting a filler charge the stale builder fee on the reusing order. Fixed by (1) gating the fill-time builder-row lookup on the order's live `HasBuilder` flag and (2) clearing the builder-order row when placement bails before committing. Program-only change; no on-chain layout, IDL, or SDK API change (the SDK computes builder fees from explicit order params, not an order-id escrow lookup, so it never reproduced the issue).

- [#275](https://github.com/velocity-exchange/velocity-v1/pull/275) [`ce18d22`](https://github.com/velocity-exchange/velocity-v1/commit/ce18d22926ae6a18b98df8a60bbd2696e0d10dbc) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Trigger orders now carry the order owner's `UserStats` account so the on-chain handler can enforce the authority-wide equity breaker: `getTriggerOrderIx` / `buildTriggerOrderInstruction` (and the `VelocityCore` wrapper) now derive and pass `userStats`. Mirrors the program fix that closes equity-floor/breaker enforcement gaps on `trigger_order`, `end_swap`, and `transfer_perp_position` (recipient side).

  Two further equity-floor/breaker gaps are closed: (1) `transfer_deposit_by_delegate` now rejects (`InvalidEquityFloorTransfer`) any floor-delta that would reduce a sub-account's floor while that sub-account is already below the floor being reduced, so an owner can't shed floor off a breached sub-account with a zero-amount transfer to defuse a pending breaker trip; and (2) a tripped authority is now barred (`EquityBelowFloor`) from every balance-acquiring liquidation (`liquidatePerp`, `liquidateSpot`, `liquidateBorrowForPerpPnl`, `liquidatePerpPnlForDeposit`; `liquidatePerpWithFill` stays allowed — its liquidator routes the position to the book and never acquires a balance). `liquidateSpot` gains a required read-only `liquidatorStats` account — `getLiquidateSpotIx` already supplies it.

- [#276](https://github.com/velocity-exchange/velocity-v1/pull/276) [`4179772`](https://github.com/velocity-exchange/velocity-v1/commit/417977294e10ffc152a0e5230019671000d2185b) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Enforce the funding pause on paths that previously bypassed it (OtterSec Medium findings). On-chain program hardening (no SDK API change): spot interest accrual now honors the exchange-wide `FundingPaused` bit on every call path (not just the dedicated crank), and `update_perp_bid_ask_twap` now no-ops when a market's `UpdateFunding` operation is paused so its funding-input mark/bid/ask TWAP stops advancing. Clients that predict spot interest or funding-input TWAP during a pause should account for the freeze.

- [#253](https://github.com/velocity-exchange/velocity-v1/pull/253) [`88a3c64`](https://github.com/velocity-exchange/velocity-v1/commit/88a3c647f609a5fe357e414ee8b4b630fd2dc68c) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - IF add zero-shares guard (High audit fix): `add_insurance_fund_stake` now rejects a positive deposit that would mint zero insurance-fund shares (new `IFDepositMintsZeroShares` error, 6360). Previously shares were computed off the pre-transfer, donation-inflatable vault balance with no nonzero-share check, so an attacker could donate into the vault before a victim's add to force `floor(amount * total_shares / vault) == 0` and capture the victim's full deposit as share-price appreciation. Mirrors the `n_shares > 0` guard the request-remove path already enforced. No account layout change; the regenerated IDL gains one error entry.

- [#266](https://github.com/velocity-exchange/velocity-v1/pull/266) [`e1f45a3`](https://github.com/velocity-exchange/velocity-v1/commit/e1f45a3d9e9e54e42987ed71bff9f6eaa6eac623) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Fix (#34): a market-level insurance-fund rebase could floor a small pending IF-stake unstake request to zero, after which cancel reverted (`InvalidIFUnstakeCancel`) and the stake was stranded — `remove` also rejected the zeroed request and `add`/re-`request` were blocked by the in-progress request. `cancel_request_remove_insurance_fund_stake` no longer re-checks the post-rebase share count, so a zeroed request cancels successfully, returning the intact rebased stake to active and abandoning only the dust request value.

- [#252](https://github.com/velocity-exchange/velocity-v1/pull/252) [`fc86321`](https://github.com/velocity-exchange/velocity-v1/commit/fc86321e8b8323e95e2d8385a82fdc69ca00075f) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - IF request-remove settle (High audit fix): `requestRemoveInsuranceFundStake` now settles already-due protocol revenue into the insurance-fund vault before freezing the staker's withdraw value, mirroring `addInsuranceFundStake`. Previously the exit value was frozen against the pre-settle vault, so a public revenue settle between request and remove shifted the exiting staker's rightful share of already-due revenue to the remaining stakers. The `requestRemoveInsuranceFundStake` instruction gains `state`, `spotMarketVault`, `velocitySigner`, and `tokenProgram` accounts (plus transfer-hook remaining accounts); the SDK builder supplies them automatically, but manual instruction construction must include them. No on-chain account layout change. Because the pre-freeze settle is skipped while withdraws are paused, `requestRemoveInsuranceFundStake` now rejects during a withdraw pause (`ExchangePaused` for the exchange-wide status, `MarketWithdrawPaused` for the market-scoped `SpotOperation::Withdraw` bit) so an accepted request always freezes a post-settle exit value; request again once the pause lifts. Canceling a pending request stays allowed during a pause.

  The vaults program mirrors this: its `request_remove_insurance_fund_stake` instruction gains the same velocity CPI accounts (`velocity_state`, `velocity_spot_market_vault`, `velocity_signer`, `token_program`) and `cancel_request_remove_insurance_fund_stake` is split onto its own unchanged accounts struct. `VaultClient.getRequestRemoveInsuranceFundStakeIx` supplies the new accounts automatically (the spot-market vault auto-resolves from its PDA seeds).

- [#254](https://github.com/velocity-exchange/velocity-v1/pull/254) [`8f8b1ef`](https://github.com/velocity-exchange/velocity-v1/commit/8f8b1efcd9323369e13b0166ea158d78bd49b2ad) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - IF revenue-settle APR cap donation-proofing (High audit fix): `settle_revenue_to_insurance_fund` sized its per-period APR cap from the live insurance-fund vault token balance, which anyone can inflate with a direct SPL donation to lift the cap toward the 10%-of-revenue-pool bound right before a settle. The cap is now sized off `min(live_if_vault, if_last_settle_vault_amount)`, using the new `SpotMarket.if_last_settle_vault_amount` field — a donation-proof accounted balance that shadows every real IF-vault movement: grown by stakes + settled revenue, shrunk (saturating at 0) by withdrawals and by every insurance-fund loss-draw (`resolve_perp_pnl_deficit`, `resolve_perp_bankruptcy`, `resolve_spot_bankruptcy`) so it never drifts after a bankruptcy/deficit (repurposed trailing padding — account size unchanged at 808 bytes; existing accounts read 0 and self-seed on the first add/settle after upgrade). A raw SPL donation never touches those paths so it can't lift the cap, while legitimate stakes still do. SDK `SpotMarketAccount` gains `ifLastSettleVaultAmount: BN`. The display-only `nextRevenuePoolSettleApr` estimate is unchanged.

  Same fix family, a second High audit finding: the unstake-cancel share forfeiture (`cancel_request_remove_insurance_fund_stake` / `calculate_if_shares_lost`) valued a canceling staker's requested shares off the live IF-vault balance, so an attacker holding a residual IF share could sandwich a victim's signed cancel with an SPL donation, manufacture "appreciation", and burn the victim's pending shares into their own. A cancel is now modeled as withdraw-and-restake at the current active share price: it completes the withdrawal of the requested shares (paying out the value frozen at request time) and immediately re-stakes the resulting tokens at the live price, forfeiting genuine escrow-window appreciation (anti-free-option) while staying donation-immune — the withdraw leg is bounded by the request-time snapshot `last_withdraw_request_value`, so a raw donation cannot manufacture forfeiture an attacker could profitably capture. This path does not read the accounted `if_last_settle_vault_amount`. No SDK logic mirror change (the SDK does not reimplement the forfeiture), and the cancel instruction's accounts/token-flows are unchanged so the vaults-program CPI is unaffected.

- [#257](https://github.com/velocity-exchange/velocity-v1/pull/257) [`70ec53e`](https://github.com/velocity-exchange/velocity-v1/commit/70ec53e9390f8da2dd5aeee752c2bf3d285a2697) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Pyth Lazer max-staleness check (High audit fix): `post_pyth_lazer_oracle_update` validated a signed Lazer message only for signer trust and a monotonic feed timestamp versus the cached account — never against `Clock::unix_timestamp` — while always stamping `posted_slot` to the current slot, from which all downstream staleness is derived. An authentic-but-stale or replayed message was thus treated as slot-fresh (and a strict-`<` monotonic check let the same message be re-posted each slot to peg a stale price as fresh). The handler now skips any feed whose message timestamp lags the wall clock by more than `PYTH_LAZER_MAX_STALENESS_SECONDS` (15s). Keepers must post Lazer updates promptly (legit updates are sub-second, so this is a no-op for them). No on-chain layout or IDL change.

- [#243](https://github.com/velocity-exchange/velocity-v1/pull/243) [`3b9a07b`](https://github.com/velocity-exchange/velocity-v1/commit/3b9a07bbe8f72145006ab45837a1fc21858d8c73) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Add `calculateUserProtectiveAssetPrice` and `calculateUserProtectiveLiabilityPrice`, mirroring the program's user-protective pricing of spot-liquidation transfers when the deposit or borrow oracle is margin-invalid (stale for margin / too uncertain): the collateral leg is priced at `max(oracle, 5min twap, oracle + confidence)` and the borrow leg at `min(oracle, 5min twap, oracle - confidence)` instead of the raw oracle price.

- [#267](https://github.com/velocity-exchange/velocity-v1/pull/267) [`2994a81`](https://github.com/velocity-exchange/velocity-v1/commit/2994a813ab3de23c44d11157f040f690e3ddf8d6) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Program liquidation-safety fixes (OtterSec audit) surfaced through the bundled IDL. `liquidate_perp_pnl_for_deposit` now reverts with the new `LiquidationWorsensAccountHealth` (6361) error instead of seizing a deposit when doing so would grow the account's margin shortage (fees above the liquidation buffer). `liquidate_spot_with_swap_end` now caps its insurance-side fee by the margin shortage like the direct spot path, delivering equivalent borrow relief. `resolve_spot_bankruptcy` now enforces a deterministic perp-before-spot precedence (matching the keeper bots) and reverts with the new `PerpBankruptcyMustPrecedeSpot` (6362) error while a cross-margin perp bankruptcy is still pending, so the shared insurance-fund draw order can't be gamed to shift socialized loss.

- [#273](https://github.com/velocity-exchange/velocity-v1/pull/273) [`a0e111a`](https://github.com/velocity-exchange/velocity-v1/commit/a0e111a237245d4011b33ff263a2cc9237a66265) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Add the `InvalidRevenueShareRecipient` (6363) error to the IDL/types. Emitted by revenue-share settlement when a builder/referrer recipient `User` is not `sub_account_id == 0` — the canonical recipient of the stored authority — closing a hole where a permissionless settlement caller could redirect accrued rewards to any sibling subaccount. (Bundled with program-only accounting fixes that have no SDK type/IDL surface: two bankruptcy interest-refresh fixes; the `sweep_perp_market_fees` reserve now valued at the fixed `expiry_price` during market Settlement — `sweepPerpMarketFees`'s doc comment notes this; and the expiry-position closeout fee now accrues to the market fee ledger with the standard IF/protocol split.)

- [#255](https://github.com/velocity-exchange/velocity-v1/pull/255) [`0e0654c`](https://github.com/velocity-exchange/velocity-v1/commit/0e0654cafc5a95855caccd1f7741e74089cb6007) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Pnl-pool fee-sweep reservations (three related High audit fixes). The permissionless sweeps that drain a perp market's PnL pool now reserve every token another claimant is owed before draining:

  - **#48** — the builder/referrer revenue-share sweep (`sweep_completed_revenue_share_for_market`, a side-effect of permissionless `settle_pnl`/`settle_multiple_pnls`) checked only the raw PnL-pool balance against `fees_accrued` and never reserved `max(net_user_pnl, 0)`, so a caller could pay revenue share out of tokens backing a third party's positive unsettled PnL. It now reserves the aggregate positive user claim (`net_user_pnl` valued at the market's oracle price, validity-gated in-slot by the preceding settle).
  - **#53** — the protocol fee sweep (`sweep_market_fees`) let its buffer-exempt protocol-fee drain move the tokens backing the floored `pending_if_fee` bankruptcy tranche into `protocol_fee_pool` (outside the insurance backstop) without touching the counter, so a later bankruptcy resolution cancelled the loss counter-only against an unbacked tranche and left surviving-trader PnL short. Every drain (protocol included, and the revenue-share sweep) now reserves `min(pending_if_fee, get_bankruptcy_if_floor())` on top of user PnL.
  - **#73** — `sweep_market_fees` drained protocol fees without reserving already-accrued builder/referrer revenue share, briefly leaving those claims unpayable. A new per-market counter `PerpMarket.pending_revenue_share` (`PerpMarketAccount.pendingRevenueShare`, QUOTE_PRECISION) tracks accrued-but-unpaid revenue share and is reserved by the sweep. It reuses the alignment padding before `amm`, so `PerpMarket` stays 1304 bytes with all offsets unchanged (existing accounts read 0).

  Adds `PerpMarketAccount.pendingRevenueShare` to `types.ts` + the IDL. No instruction or SDK-API change; the sweeps are program-internal and not reimplemented client-side.

- [#297](https://github.com/velocity-exchange/velocity-v1/pull/297) [`2d4a32f`](https://github.com/velocity-exchange/velocity-v1/commit/2d4a32f1c74b28b123ad4bd47f734c2843b090e4) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Fix the IDL's `SpotMarket` layout: the borsh-packed offset of `protocol_fee_pool` was 5 bytes short of the on-chain `#[repr(C)]` offset (implicit alignment padding the IDL didn't model), so `protocolFeePool`, `protocolLiquidationFee`, and `protocolFeeFactor` decoded garbage/zeros. On-chain layout is unchanged; only the IDL (and generated types) are corrected.

- [#268](https://github.com/velocity-exchange/velocity-v1/pull/268) [`8761596`](https://github.com/velocity-exchange/velocity-v1/commit/87615967d775c435fa777a5fa9396a80bbb0a62f) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Fix `getUpdateAMMsIx`/`updateAMMs` for `Prelaunch`-oracle-source perp markets: the crank loads each market as writable and the program refreshes a prelaunch oracle in place, so that oracle account must be passed writable. It was hard-coded read-only, making `updateAMMs` on a prelaunch market revert with "instruction modified data of a read-only account". Prelaunch oracles are now marked writable (matching `addPerpMarketToRemainingAccountMaps`); non-prelaunch oracles are unaffected.

- [#292](https://github.com/velocity-exchange/velocity-v1/pull/292) [`633c5f1`](https://github.com/velocity-exchange/velocity-v1/commit/633c5f17c56b186e505a3a7bfad2a450a1e9a82e) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Fix warm-gated spot-market admin commands failing when proposed through the warm-admin Squads multisig. `getUpdateSpotMarketStatusIx`, `getUpdateWithdrawGuardThresholdIx`, `getUpdateSpotMarketIfFactorIx`, and `getUpdateSpotMarketScaleInitialAssetWeightStartIx` now accept an optional `admin` override, and `velocity-admin spot-market` commands resolve the admin signer to the executing authority (the multisig's vault 0 PDA with `--multisig`, else the local keypair) instead of always embedding `state.coldAdmin`.

## 0.7.0

### Minor Changes

- [#258](https://github.com/velocity-exchange/velocity-v1/pull/258) [`c85d802`](https://github.com/velocity-exchange/velocity-v1/commit/c85d80284fb61dac7a08e47fe7b78340bf1213cc) Thanks [@0xahzam](https://github.com/0xahzam)! - `getTriggerPrice` now mirrors the program's last-fill staleness guard: the last-fill leg of the median trigger price is ignored (oracle price substitutes) when the market's last fill is older than the new `TRIGGER_PRICE_LAST_FILL_MAX_AGE` export (5 minutes, read from `marketStats.lastTradeTs`).

### Patch Changes

- [#259](https://github.com/velocity-exchange/velocity-v1/pull/259) [`cec4fcb`](https://github.com/velocity-exchange/velocity-v1/commit/cec4fcbf440645ad55dd41ec8410a250ca96fdef) Thanks [@jt-lumen](https://github.com/jt-lumen)! - TxHandler now caches recent blockhashes by default (2s TTL), collapsing the per-build `getLatestBlockhash` RPC call into at most one fetch per window. Consumers building many transactions in quick succession (e.g. crankers) no longer hit RPC on every build. Set `txHandlerConfig.blockhashCachingEnabled: false` to restore the previous fetch-fresh-every-build behavior.

- [#260](https://github.com/velocity-exchange/velocity-v1/pull/260) [`f03beee`](https://github.com/velocity-exchange/velocity-v1/commit/f03beeecea6f3c9cc6c0ad7e828e9fab639e9a1b) Thanks [@jt-lumen](https://github.com/jt-lumen)! - BlockhashSubscriber: derive the current block height from `getLatestBlockhashAndContext().value.lastValidBlockHeight - 150` instead of a paired `getBlockHeight` RPC call. This halves the per-poll RPC load of the subscriber (removing one `getBlockHeight` request per interval) with no change to `getLatestBlockHeight()` semantics, matching the Rust subscriber which never issued the extra call.

## 0.6.1

### Patch Changes

- [#245](https://github.com/velocity-exchange/velocity-v1/pull/245) [`35f1480`](https://github.com/velocity-exchange/velocity-v1/commit/35f1480f2a60bad00a96f2e254cb5e9b210067b1) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Bankruptcy IF-fee floor (High audit fix): the permissionless fee sweep now leaves `bankruptcyIfFloorPct` of open-interest notional behind in `feeLedger.pendingIfFee`, so a sweep front-running a `resolvePerpBankruptcy` can no longer strip the first-loss tranche up to the floor. `PerpMarketAccount` gains `bankruptcyIfFloorPct` (repurposed padding — layout size unchanged; existing markets read 0 = disabled, new markets default to 10 bps), `AdminClient` gains `updatePerpMarketBankruptcyIfFloorPct`, and the admin CLI gains `perp-market set-bankruptcy-if-floor <market> <pct>`.

- [#244](https://github.com/velocity-exchange/velocity-v1/pull/244) [`00ebcd2`](https://github.com/velocity-exchange/velocity-v1/commit/00ebcd2068b03db30652d352e2995417e08d9b35) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - `User.getSafestTiers()` no longer counts a zero-base perp position whose only exposure is positive unsettled pnl as the user's safest perp liability, mirroring the program fix in `calculate_user_safest_position_tiers`: a positive pnl claim is a claim on the market's pnl pool, not a liability, and counting it made liquidator pre-flight tier checks skip `liquidatePerpPnlForDeposit` liquidations the program now accepts.

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
