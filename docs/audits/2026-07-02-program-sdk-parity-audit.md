Report goes to a decision-maker asking "can we re-open the SDK to the public." I'll synthesize the JSON into a decisive markdown report. Let me organize by severity, dedup, and build the four required sections.

Key dedup observations:
- The `MAX_POSITIVE_UPNL_FOR_INITIAL_MARGIN` $100 cap appears twice (math-margin critical + constants major) — same defect, merge.
- The OrderBitFlag missing HasBuilder/IsIsolatedPosition appears twice (events major + math-orders-dlob minor) — same root cause, merge (keep higher severity).
- The MarginCategory 'Fill' gap appears twice (errors-enums minor + math-margin minor) — merge.

Let me write the report as my final response.

# Velocity SDK ↔ Program Parity Audit — Synthesis & Go/No-Go

## 1. Executive summary

**Verdict: DO NOT re-open the SDK to the public yet.** The audit confirmed **5 critical** and **17 major** program↔SDK divergences (plus lower-severity items). Several criticals are not cosmetic — they cause the SDK to *mispredict on-chain outcomes* in ways that will silently mislead integrators, keepers, and any front-end that trusts SDK math. A public re-open in the current state ships a client that (a) crashes on real market decode, (b) over-reports buying power, (c) can miss a bankrupt user, and (d) predicts the wrong fill price.

**Must-fix before any public re-open (the blocking set):**

| # | Defect | Why it blocks |
|---|--------|--------------|
| C1 | `OracleSource.SWITCHBOARD*` uses pre-rename Borsh keys | `findAllMarketAndOracles` / account subscribers **throw** `Invalid oracle source` and abort market bootstrapping the instant any market with that discriminant is decoded; stale mainnet config would also fail instruction encoding. Latent today (devnet uses Pyth Lazer) but detonates on the first affected market. |
| C2 | Initial-margin uPnL never capped at $100 | SDK **overstates free collateral / buying power**; a client builds risk-increasing orders the SDK says pass and the program rejects with `InsufficientCollateral`. Direct user-facing mispredict. |
| C3 | Auction/oracle-offset limit price never standardized to tick size | DLOB predicts different crossing/fill prices than execute on-chain for every market with `tick_size > 1`. Core fill-simulation correctness. |
| C4 | `isUserBankrupt()` doesn't skip isolated perp positions | Can report a genuinely-bankrupt user as solvent; the keeper's bankruptcy-resolution gate is wired to this and would **skip resolution**. |
| C5 | `getMMOracleDataForPerpMarket` uses wrong MM-oracle gate | Picks the wrong price source vs the program; propagates into fill/quote pricing across DLOB and every keeper bot. |

Plus two doc-level criticals (C6, C7 below) that are migration-doc gaps, not code — lower re-open urgency but still required for an honest integrator migration story.

**Recommended gate:** ship the public re-open only after C1–C5 are fixed and the major-tier collateral/liquidation/fee items (M-group below) are triaged. The minors and info items can follow in a fast-follow release with a documented known-issues list.

---

## 2. Findings by severity (deduplicated)

### CRITICAL

**C1 — OracleSource Switchboard rename not mirrored** *(errors-enums)*
The program renamed `Switchboard → DeprecatedSwitchboard` and `SwitchboardOnDemand → DeprecatedSwitchboardOnDemand` (discriminants preserved). The SDK still defines the old Borsh keys and pattern-matches on them.
- Program: `programs/velocity/src/state/oracle.rs:161-185`; both deprecated sources hard-error `InvalidOracle` at `admin.rs:569-571`.
- SDK: `packages/sdk/src/types.ts:162-179`, `oracles/oracleId.ts:1-48`, `factory/oracleClient.ts:11-72`, and 13 stale `SWITCHBOARD_ON_DEMAND` uses in `constants/spotMarkets.ts`.
- **Two breakages:** decode path — `getOracleSourceNum` throws, killing `findAllMarketAndOracles` (`config.ts:199,208`) and subscribers (`webSocketVelocityClientAccountSubscriberV2.ts:639,664`); encode path — `initializeSpotMarket` would reject the stale key.
- **Fix:** rename SDK variant keys to `deprecatedSwitchboard` / `deprecatedSwitchboardOnDemand` to match the IDL, and purge/repoint the `MainnetSpotMarkets` entries still assigned a now-hard-erroring discriminant.
- **Note:** currently latent (deployed devnet markets use Pyth Lazer), but this is a crash-on-first-contact bug, not a slow drift.

**C2 — Initial-margin unrealized-PnL not capped at $100** *(math-margin + constants — merged, same defect)*
Program clamps weighted positive uPnL to `MAX_POSITIVE_UPNL_FOR_INITIAL_MARGIN` ($100) per perp position when margin type is Initial; SDK applies no cap anywhere.
- Program: `math/margin.rs:185-188`, constant at `math/constants.rs:194`.
- SDK: `user.ts:4463-4549` (`getMarginCalculation`) and `user.ts:854-928` (`getUnrealizedPNL`, feeds `getTotalCollateral`/`getFreeCollateral('Initial')`, the default category). No `MAX_POSITIVE_UPNL`-equivalent exists in the tree.
- **Effect:** any account with >$100 unrealized profit gets its Initial-margin free collateral overstated → SDK predicts a risk-increasing order passes that the program rejects.
- **Fix:** add `MAX_POSITIVE_UPNL_FOR_INITIAL_MARGIN = new BN(100).mul(QUOTE_PRECISION)` to `constants/numericConstants.ts`; apply `BN.min(...)` per position after asset-weight scaling, before summing, when category is `Initial`.

**C3 — Auction / oracle-offset limit price not standardized to tick size** *(math-orders-dlob)*
Program ends every auction-price and oracle-offset-limit path with `standardize_price(price, tick_size, direction)` and floors at `tick_size`; SDK returns the raw interpolated price floored at `1`.
- Program: `math/auction.rs:123-209`, `state/user.rs:1437-1473`, `standardize_price` at `math/orders.rs:267-288`.
- SDK: `math/auction.ts:116-191` (no `tickSize` param), `math/orders.ts:63-78` (`getLimitPrice`), `dlob/DLOBNode.ts:86-104` — no tick size threaded through any of DLOB's ~15 call sites.
- **Effect:** for `tick_size > 1` markets, crossing evaluation (`findCrossingRestingLimitOrders`, `findTakingNodesCrossingMakerNodes`) and any UI/keeper reading an in-auction price is up to one tick off from on-chain.
- **Fix:** thread `orderTickSize` through `getAuctionPrice*`/`getLimitPrice`/`DLOBNode.getPrice` and call the SDK's existing `standardizePrice()` on every branch, flooring oracle-offset at `tick_size` not `ONE`.

**C4 — `isUserBankrupt()` ignores isolated positions** *(liquidation)*
Program's `is_cross_margin_bankrupt` skips isolated perp positions (`bankruptcy.rs:22-26`); SDK loops all positions with no isolated check and has no mirror of `is_isolated_margin_bankrupt` (`bankruptcy.rs:43-53`).
- SDK: `math/bankruptcy.ts:20-32` (untouched since initial import, predates the isolated-margin feature).
- **Effect:** a user with a live isolated position + a genuinely bankrupt cross book reads solvent; `apps/keeper-bots-v2/src/bots/liquidator.ts:1536` gates `tryResolveBankruptUser` on this → resolution skipped. `user.isBankrupt()` (status bit) doesn't catch it either.
- **Fix:** skip isolated perp positions in `isUserBankrupt`; add `isIsolatedPositionBankrupt(user, marketIndex)` mirroring `is_isolated_margin_bankrupt`.

**C5 — MM-oracle fallback gate uses the wrong check** *(oracles)*
`getMMOracleDataForPerpMarket` gates MM-price fallback with `isOracleTooDivergent` (a %-vs-5min-TWAP band, 50% floor) instead of `is_oracle_valid_for_action(mm_oracle_validity, UseMMOraclePrice)` (blocks only on `NonPositive`/`TooVolatile` from `oracle_validity()`, using `last_oracle_price_twap` and a ratio guard-rail).
- Program: `state/oracle.rs:308-317`, `math/oracle.rs:230-233`, `state/perp_market.rs:1004-1045`.
- SDK: `velocityClient.ts:9394-9408`, `math/oracles.ts:179-194`. **The SDK already has a faithful ratio-based mirror** (`getOracleValidity`, `oracles.ts:57-135`) — it just isn't called here.
- **Effect:** wrong MM/exchange price source selection propagates into DLOB and all keeper bots (`filler`, `jitMaker`, `floatingMaker`, `uncrossArbBot`, …) and into `user.ts` margin/health/liq-price paths.
- **Fix:** replace the `isOracleTooDivergent` branch with `getOracleValidity` + `UseMMOraclePrice` semantics (fall back only on `NonPositive`/`TooVolatile`). Note `isOracleTooDivergent` mirrors an unrelated program check (`validate_fill_price_within_price_bands`) — don't conflate them.

**C6 (doc) — Bulk `place_orders` margin-check bypass fix (PR #135) unrecorded** *(git-recency)*
Security-relevant: previously an early risk-increasing order's risk wasn't accumulated into the batch's single margin check, and the check could be skipped if the final order was a no-op. Now enforced per risk-scope across the whole batch. No SDK logic mirror exists (not an SDK bug), but the behavior change (previously-accepted batches now rejected) is absent from `docs/DRIFT-TO-VELOCITY.md`.
- Program: `instructions/user.rs:2605` (place_orders), `controller/orders.rs:96-476` (`PlaceOrderResult`); test `tests/velocity/bulkOrdersMarginCheck.ts`.

**C7 (doc) — `liquidate_spot_with_swap` was fully bricked, now fixed (PR #134), unrecorded** *(git-recency)*
Stale fixed-account-count guard (13 vs actual 11) made every real call fail `InvalidLiquidateSpotWithSwap`. Fixed at `keeper.rs:1400-1450`. SDK builder (`velocityClient.ts:8751`) was never at fault, but keepers who found the ix always-failing have no doc that it's now usable.

---

### MAJOR

**M1 — `getSpecialTransferPerpPositionToVammIx` omits other positions from `remaining_accounts`** *(ix-user)*
Passes `userAccounts: []`, so only the single transferred market is in `remaining_accounts`, but the handler runs a full-account margin calc.
- SDK: `velocityClient.ts:4163-4185` (contrast `getTransferPerpPositionIx` at :4122-4129 which passes real `userAccounts`).
- Program: `user.rs:4088-4300`, unscoped margin calc `math/margin.rs`.
- **Effect:** if the VammHedger account holds a 2nd open perp position or a non-quote-market spot balance, the built tx fails on-chain with a missing-market error. **Fix:** pass `userAccounts: [userAccount]`.

**M2 — `update_pause_admin` has no SDK/CLI wrapper** *(ix-admin)*
Cold-only rotation of the emergency `pause_admin` key. In IDL and `types.ts:743`, but no `updatePauseAdmin`/`getUpdatePauseAdminIx` in `adminClient.ts` (sits right next to `updateWarmAdmin`/`updateHotAdmin` at :5906-5959), no `set-pause-admin` in `cli-admin/.../auth.ts`, and `show.ts` never prints `pauseAdmin`.
- **Fix:** add the wrapper (signer default `coldAdmin`), the CLI subcommand, and the show output.

**M3 — `UserStats.referrer_status` memcmp offset stale (188 vs 164)** *(accounts-types)*
`memcmp.ts:100-116` hardcodes offset 188; correct layout puts `referrer_status` at 164 (188 lands in the always-zero trailing padding). Filters always match zero accounts.
- **This is live, not dead code:** `userMap/referrerMap.ts` (`ReferrerMap.sync → syncReferrer`) uses both filters via `getProgramAccounts` → bulk referrer discovery silently returns empty. Same bug class PR #173 fixed for the User account but left these stale.
- **Fix:** set offset to 164 (named constant + layout comment).

**M4 — `LPPoolAccount.quoteConstituentIndex` spelled correctly in SDK, typo on-chain** *(accounts-types)*
On-chain field is `quote_consituent_index` (missing 't'); IDL carries the typo (`quoteConsituentIndex`); `types.ts:1696` declares the corrected spelling. `getLpPoolAccount()` force-casts (`as unknown as LPPoolAccount`), so decoded objects have `.quoteConsituentIndex` and any read of `.quoteConstituentIndex` gets `undefined`.
- **Fix:** rename the `types.ts` field to `quoteConsituentIndex` to match the actual decoded key (or fix the Rust field layout-safely and regenerate — but the mirror must match the on-chain name until then).

**M5 — Five emitted events unreachable via typed EventSubscriber** *(events)*
`PerpMarketFeeSweepRecord`, `ProtocolFeeWithdrawRecord`, `RevenueShareSettleRecord`, `TransferFeeAndPnlPoolRecord`, `LPBorrowLendDepositRecord` — all emitted on-chain, all have `*Record` types and IDL entries, but none are wired into `EventMap`/`EventType`/`DefaultEventSubscriptionOptions.eventTypes` (`events/types.ts`). `parseEventsFromLogs` drops any event not in `eventListMap`. (`LPBorrowLendDepositRecord` is even in the `VelocityEvent` union but omitted from `EventMap`.)
- Emit sites: `protocol_fees/withdraw_protocol_fees_{spot,perp}.rs`, `controller/perp_pools.rs:145`, `vlp/hedge/instructions.rs:1514,1592`, `controller/revenue_share.rs`, `vlp/amm/admin.rs`.
- **Fix:** import all five, add to `EventMap`, `VelocityEvent`, and default `eventTypes`.

**M6 — `OrderBitFlag` missing `HasBuilder`/`IsIsolatedPosition`** *(events + math-orders-dlob — merged)*
Program enum has 6 bits (`user.rs:1768-1775`); SDK `types.ts:221-226` has only the first 4 (missing `HasBuilder=16`, `IsIsolatedPosition=32`). Consumers reading `OrderRecord.order.bitFlags` / `OrderActionRecord.bitFlags` can't detect builder-fee or isolated-margin orders; `orders.ts:284` even redefines a duplicate local `FLAG_HAS_BUILDER = 0x10`.
- **Fix:** add both flags to `OrderBitFlag`; point `orders.ts` at the shared enum.

**M7 — `PerpOperation` bit-flags: missing `AmmImmediateFill`, wrong `SETTLE_REV_POOL`** *(errors-enums)*
Program has 8 bits with `AmmImmediateFill=64`, `SettleRevPool=128` (`paused_operations.rs:7-16`); SDK `types.ts:64-72` has 7, no `AMM_IMMEDIATE_FILL`, and `SETTLE_REV_POOL=64` (collides with the real AmmImmediateFill bit). Consumed by keeper bots. Any test of `SETTLE_REV_POOL` actually tests AmmImmediateFill; the real bit 128 is unreachable.
- **Fix:** add `AMM_IMMEDIATE_FILL = 64`, correct `SETTLE_REV_POOL = 128`.

**M8 — Isolated-asset-tier violation gate has no SDK mirror** *(math-margin)*
`num_perp_liabilities`/`num_spot_liabilities`/`with_*_isolated_liability` tracking + `validate_any_isolated_tier_requirements` (PR #165/#166) is entirely absent from the SDK's `MarginCalculation`/`getMarginCalculation`. Clients can't pre-flight this on-chain rejection.
- Program: `state/margin_calculation.rs:145-157`, `math/margin.rs:621-686`.
- **Fix:** port the counters + a `validateAnyIsolatedTierRequirements()` helper.

**M9 — Multi-pool segregation absent from SDK margin calc** *(math-margin)*
`user_pool_id == market.pool_id` checks and the `pool_id==1 && market_index==0 && !is_borrow` `skip_token_value` carve-out (`margin.rs:248-280,495-501`) aren't reflected in `getMarginCalculation` (no `poolId` read at all). A pool-1 user's quote deposit — reachable via cross-pool perp PnL settling into shared USDC — is valued at zero on-chain but full price by the SDK, overstating collateral.
- **Fix:** thread `user.poolId` / market `poolId`; skip quote-deposit value for the pool-1 case, assert pool match otherwise.

**M10 — Worst-case perp liability value not quote-converted before buffer math** *(math-margin)*
Program multiplies `worse_case_liability_value` by the strict quote price before feeding buffer/isolated bookkeeping (`margin.rs:136-142,570-613`); SDK passes the unconverted value into `addCrossMarginRequirement`/`addIsolatedMarginCalculation`/`addPerpLiabilityValue` (`user.ts:4534-4547`), while `perpMarginRequirement` *is* converted. Diverges whenever quote oracle ≠ 1.0 (stablecoin depeg). Feeds public `getMarginRequirement(category, liquidationBuffer)`.
- **Fix:** multiply `worstCaseLiabilityValue` by the strict (max) quote price before those calls.

**M11 — Referee discount never applied in fee prediction** *(math-fees)*
Program reduces the taker fee by `referee_fee_numerator/denominator` when `reward_referrer` is true (`fees.rs:139-143,336-340,251-272`). SDK `getUserFeeTier`/`calculateFeeForQuoteAmount` (`user.ts:3557-3614`) and `getMarketFees` (`velocityClient.ts:10114-10149`) never read the referee fields → overestimates fee for referred users.
- **Fix:** apply the referee discount when the user is a referee.

**M12 — No builder-fee amount computation** *(math-fees)*
Program charges `builder_fee = quote * builder_fee_bps / 100_000` on top of the tiered fee (`fees.rs:169-176,369-376`). `math/builder.ts` only has flag helpers; fee-estimation entry points don't accept/apply `builderFeeTenthBps` → underestimates cost for builder orders.
- **Fix:** add `calculateBuilderFee(quoteAssetAmount, builderFeeTenthBps)` and apply it when `hasBuilderParams`.

**M13 — `calculateReferencePriceOffset` drops a term** *(math-amm)*
Program subtracts an oracle-twap-scaled floor (`oracle_twap_slow.abs() / FUNDING_RATE_OFFSET_DENOMINATOR`) from the day-premium leg before clamping (`spread.rs:990-998`); SDK omits it (`amm.ts:491-496`). Diverges whenever `oracleTwapSlow` is non-trivial; propagates through reference-price-offset into bid/ask/spread reserves. (Rust term added in PR #1786; SDK never updated.)
- **Fix:** add `.sub(oracleTwapSlow.abs().div(FUNDING_RATE_OFFSET_DENOMINATOR))` before `clampBN`.

**M14 — `calculateUpdatedAMM` applies repeg unconditionally** *(math-amm)*
Program's `project_post_refresh` no-ops the curve update when the oracle is invalid for `UpdateAMMCurve` (non-positive price) or when the repeg cost fails the affordability floor (`check_lower_bound` + tfmd−cost<0) (`repeg.rs:551-616`). SDK applies the projection unconditionally and discards `checkLowerBound` as `_checkLowerBound` (`amm.ts:116-196`). Affordability rejection is the materially reachable case (budget-rounding); feeds all price/trade-sim functions.
- **Fix:** port the affordability gate + oracle-validity-for-curve-update check; return the original AMM unchanged when either fails.

**M15 — DLOB AMM-availability predicate missing MM-oracle volatility gate** *(math-amm)*
PR #182 split availability into `amm_fill_gates_ok` (pause + drawdown + MM-oracle volatility + oracle validity). The SDK's DLOB path *does* cover pause+drawdown (`ammPaused`) and oracle-validity-for-low-risk (`isFallbackAvailableLiquiditySource` via `getOracleValidity`) — **the one genuinely missing gate is `is_mm_exchange_diff_bps_high`** (MM-vs-exchange oracle volatility), left as an explicit TODO in `math/auction.ts:32-71`.
- **Effect:** DLOB treats AMM fallback/JIT as available when the MM oracle has diverged too far, where on-chain `amm_jit_allowed` is false. (Narrower than originally framed — one hard gate, not two.)
- **Fix:** add the MM-oracle-diff-bps check and use it alongside `ammPaused`.

**M16 — `calculateWithdrawLimit` wrong divisors + wrong base amount** *(math-spot)*
Two defects: main-pool divisors are `/3,/7,/8` but program is `/3,/5,/14` (`spot_withdraw.rs:39-60` vs `spotBalance.ts:589-611`); and both pool branches use raw `marketDepositTokenAmount` for the first term where the program uses `lesser_deposit_amount` (min of deposit and its twap). Confirmed via PR #1801 which updated the Rust divisors and the SDK isolated branch but missed the SDK main-pool branch. Feeds `User.getWithdrawalLimit()`.
- **Fix:** use `lesserDepositAmount` in the first term of both branches; restore main-pool divisors to `/5` and `/14`.

**M17 — `canBypassWithdrawLimits` omits the `cumulative_deposits >= 0` gate** *(math-spot)*
Program requires `net_deposits >= 0` AND `spot_position.cumulative_deposits >= 0` AND `balance_type == Deposit` (`spot_withdraw.rs:65-108`); SDK checks only the first two conditions (`user.ts:3711-3762`), never reading `position.cumulativeDeposits` (present on the type). Client-side misprediction of withdrawable amount (on-chain still rejects correctly).
- **Fix:** thread `cumulativeDeposits` and require `>= 0`.

**M18 — `calculateMaxPctToLiquidate` has no isolated-position override** *(liquidation)*
Program liquidates isolated positions 100% in one shot (`IsolatedMarginLiquidatePerpMode`, `liquidation_mode.rs:307-316`); SDK always applies the graduated cross-margin formula (`liquidation.ts:176-214`). External callers sizing an isolated liquidation compute far too small a percentage.
- **Fix:** add an isolated branch/param returning `LIQUIDATION_PCT_PRECISION`.

**M19 — Dynamic IF-fee cap not mirrored; SDK doc guidance is wrong** *(liquidation)*
Program runs `if_liquidation_fee + protocol_liquidation_fee` through `calculate_perp_if_fee`/`calculate_spot_if_fee` (margin-shortage-aware cap) before sizing margin-shortage coverage; the raw sum is only an upper bound. SDK has no port, and its doc comments (`liquidation.ts:14-19,51-56`) affirmatively tell callers to pass the raw sum "to match on-chain sizing" — false when the cap binds → overestimates base-amount/liability-transfer.
- **Fix:** port both functions; feed their output (not the raw sum) into the covering-amount helpers and correct the docs.

**M20 — PR #182 AMM-JIT gating not in migration doc** *(git-recency, doc)*
Match fills can now route entirely to the resting DLOB maker (smaller/DLOB-only) when a hard AMM gate is active. No §6 row.

**M21 — PR #174 bankruptcy-event semantics not in migration doc** *(git-recency, doc)*
`PerpBankruptcyRecord`/`SpotBankruptcyRecord.bankrupt` now reflects actual post-resolution state instead of always `true`. Wire type unchanged → invisible to type-checkers, breaking to indexers assuming `true`. No §6 row.

**M22 — PR #137 per-market Deposit pause not in migration doc** *(git-recency, doc)*
`handle_deposit` now respects the per-market `SpotOperation::Deposit` pause bit (`MarketActionPaused`) — previously only global pause + cap. No §6 row. (Note: original claim's "#139 implies it pre-existed" reasoning was refuted — the two doc entries describe unrelated checks; the missing #137 row is still real.)

---

### MINOR / INFO (fast-follow, non-blocking)

- **`transfer_fee_and_pnl_pool` has SDK wrapper but no CLI command** *(ix-admin)* — add `fees transfer-pool` to `cli-admin`.
- **`FeatureBitFlags.BUILDER_REFERRAL=8` is phantom** *(errors-enums)* — no such on-chain bit; remove it (currently unread).
- **`ContractType` stale `FUTURE` key + missing `DeprecatedPrediction`** *(errors-enums)* — rename to `deprecatedFuture`, add missing variant.
- **`MarginCategory 'Fill'` unusable** *(errors-enums + math-margin — merged)* — type declares `'Fill'` but `calculateMarketMarginRatio` throws on it; add the `(initial+maintenance)/2` case or drop the variant.
- **`getLastFundingBasis` hardcodes `3333`** *(math-funding)* — use `FUNDING_RATE_OFFSET_PERCENTAGE` constant.
- **Non-market-index taker-fee rounds down vs program ceil** *(math-fees)* — use a divCeil helper (`safe_div_ceil` parity).
- **`getTokenValue`/`getStrictTokenValue` truncate vs floor for negatives** *(math-spot)* — add a divFloor for the negative branch.
- **No `depositPaused`/`withdrawPaused` predicate** *(math-spot)* — add for parity with `fillPaused`/`ammPaused`.
- **`getBalance()` has no `round_up`/`is_leaving_velocity` override** *(math-spot, info)* — not exercised today; add optional param for future withdraw-preview callers.
- **`calculateMaxPctToLiquidate` gates slots-elapsed on `liquidationMarginFreed > 0`** *(liquidation)* — condition doesn't exist on-chain; compute `slotsElapsed` unconditionally.
- **Pyth stablecoin peg-snap uses `<` vs program `<=`** *(oracles)* — change `.lt` → `.lte` in `pythClient.ts` and `pythLazerClient.ts`.
- **No `isMarkOracleTooDivergent` (10% floor) helper** *(oracles, info)* — can't pre-flight the `UpdateFunding` divergence rejection.
- **FEES.md waterfall says "tiered by 30d volume + gov stake"** *(math-fees, info)* — gov-stake tiering removed; SDK already correct, only the doc is stale.
- **PR #158 mainnet `initialize` signer lock not in migration doc** *(git-recency, minor)* — genesis-only impact.

---

## 3. Migration-doc addendum (`docs/DRIFT-TO-VELOCITY.md`)

Add the following **§6 change-log rows** (verified PRs currently unrecorded), ranked by integrator impact:

| PR | Section | Row content to add |
|----|---------|--------------------|
| **#135** | §6 + note in §3 | Bulk `place_orders` now enforces initial margin **per risk-scope across the whole batch** (cross vs each isolated market), not just on the final order — batches that previously slipped a risk-increasing order past a weaker/absent margin check are now rejected. |
| **#134** | §6 | `liquidate_spot_with_swap_begin/end` was **non-functional prior to this fix** (stale account-index/count validation → always `InvalidLiquidateSpotWithSwap`); it is now operational — keepers depending on it should re-verify their integration. |
| **#182** | §6 (or extend the jit-proxy row) | AMM JIT **no longer participates in a DLOB match** when a hard AMM-fill gate (pause / drawdown / MM-oracle volatility / oracle invalidity) is active; match fills can now be smaller / DLOB-maker-only under those conditions. |
| **#174** | §6 (ABI/event-semantics note in §5) | `PerpBankruptcyRecord`/`SpotBankruptcyRecord.bankrupt` now reflects whether the user **still holds a bankrupting liability after** the resolve call, rather than always being `true`. Wire type unchanged; consumers assuming `bankrupt == true` must update. |
| **#137** | §6 | Direct `deposit()` now respects the **per-market `SpotOperation::Deposit` pause bit** (`MarketActionPaused`), independent of the pre-existing global deposit-pause and aggregate-cap checks. |
| **#158** | §5/§6 (one line) | Mainnet `initialize` (one-time global State creation) now requires a **fixed admin signer** (`state_init_authority` = `prpHJmuXnqdaz92tBVdwsqmqyhqPLuq5Km35a5QWco3`); relevant only to genesis/redeploy. |

Also add **§4 (SDK surface)** and **§5 (ABI/enum) notes** for the code-level enum/oracle changes that integrators inherit once fixed: the `OracleSource` deprecated-Switchboard rename (C1), the `PerpOperation` bit re-numbering (M7), and the added `OrderBitFlag` bits (M6).

---

## 4. Fix plan, ordered by risk

**Phase 0 — Re-open blockers (crash / mispredict; do first):**
1. **C1** OracleSource rename — one-line key rename in `types.ts` + purge stale `MainnetSpotMarkets` entries. Highest blast radius (aborts market bootstrapping), cheapest fix.
2. **C4** `isUserBankrupt` isolated skip — small, gates a live keeper decision path.
3. **C2** $100 uPnL cap — add constant + `BN.min`; directly overstated buying power.
4. **C5** MM-oracle gate — swap to the already-present `getOracleValidity`; low code cost, wide propagation.
5. **C3** tick-size standardization — largest change (thread `tickSize` through auction/limit/DLOB), but core fill correctness. Bundle with M6 (OrderBitFlag) since both touch the order/DLOB path.

**Phase 1 — Collateral / liquidation correctness majors (finance-critical mispredicts):**
M2 (pause-admin rotation — security surface), M8, M9, M10 (margin calc), M16, M17 (withdraw limits), M18, M19 (liquidation sizing). These change reported collateral/withdraw/liquidation numbers; verify each against `packages/sdk/.../marginCalculations.test.ts` with new non-1.0-quote and isolated-tier cases.

**Phase 2 — Data-integrity / decode majors:**
M3 (memcmp offset — breaks referrer sync), M4 (LPPool typo), M5 (unreachable events), M6/M7 (enum bits), M11/M12 (fee prediction), M13/M14/M15 (AMM math), M1 (special-transfer accounts).

**Phase 3 — Migration doc:**
All §6/§4/§5 rows from Section 3 (C6, C7, M20–M22, #158). Pair each code fix from Phases 0–2 that changes ABI/SDK surface with its doc row in the same PR, per the CLAUDE.md migration-doc policy.

**Phase 4 — Minors/info fast-follow:**
The full minor/info list, shippable in a point release with a published known-issues note.

**Cross-cutting:** every fix must (a) add/extend the pinning SDK unit test that would have caught it (several gaps exist precisely because tests only exercise the single-market / 1.0-quote / non-isolated case), and (b) where the fix is a re-implementation of program logic, port constants and edge-case handling exactly rather than approximating.