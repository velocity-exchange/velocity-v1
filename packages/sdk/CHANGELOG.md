# @velocity-exchange/sdk

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
