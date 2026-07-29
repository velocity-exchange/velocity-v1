---
"@velocity-exchange/sdk": minor
---

Stop discarding the AMM's dynamic spread when `base_spread` is 0, so quoted vAMM prices match what the program will fill at.

`calculateSpread` short-circuited on `amm.baseSpread == 0 || amm.curveUpdateIntensity == 0` and returned `[baseSpread / 2, baseSpread / 2]`. On chain, `update_spreads` (`vlp/amm/math/spread.rs`) branches on `curve_update_intensity > 0` alone: `base_spread` is only the floor that the vol spread is maxed against (`long_spread = max(half_base_spread, long_vol_spread)`), so a `base_spread` of 0 does not disable the dynamic spread. Any market configured with `base_spread = 0` therefore had its entire inventory/volatility spread thrown away off chain while the program kept applying it.

This was not a rounding difference. On devnet SOL-PERP (`base_spread: 0`, `curve_update_intensity: 100`, `max_spread: 142500`) the AMM was short 108.6 base — 10.9% of `sqrt_k` — which drove an inventory scale of ~8.4x and `long_spread` to 111985 (~11.2%) on chain, against a `short_spread` of 440. The SDK reported `[0, 0]` for that same state, so `calculateSpreadReserves` returned the unadjusted reserves and `calculateAskPrice`, `calculateBidPrice` and `calculateReservePrice` all collapsed onto the same number. `getVammL2Generator` then published a best vAMM ask of ~$80.03 — pure curve price impact with no spread — where the program's ask was ~$82.12 against a ~$73.96 oracle.

Everything downstream of that book inherited the error. `dlob-server`'s `/auctionParams` priced a 1 SOL long at `entryPrice`/`worstPrice` $80.034 with `priceImpact: 0` and set the oracle-offset auction end to +$6.149, i.e. a maximum price of $80.114 — about $2.00 short of the real ask, so the order could not cross at any point in its auction and expired unfilled with `taker does not cross amm` / `no fulfillment methods found` in the program logs. The on-chain helper for the same job, `OrderParams::get_perp_baseline_start_end_price_offset`, folds `amm_spread_side_pct * oracle_twap` into its end-price buffer and would have produced a crossable order; only the off-chain path was blind to the spread.

Two smaller divergences in the same function are fixed alongside it, both verified against `update_spreads`:

- **`amm_spread_adjustment` was skipped on the frozen-curve branch.** On chain it is applied after the `curve_update_intensity` branch, so it affects both the dynamic and the `[base_spread / 2, base_spread / 2]` result. The early return meant a market with `curve_update_intensity == 0` ignored its manual adjustment entirely. The adjustment is now factored into `applyAmmSpreadAdjustment` and applied to both branches, rounding as the program does — ceil when growing, floor when shrinking — rather than leaving a fractional spread.
- **`base_spread / 2` was float division.** `base_spread.safe_div(2)` truncates on chain, so a `base_spread` of 175 (the value BTC-PERP and ETH-PERP carry) yielded 87 on chain and 87.5 in the SDK.

`AdminClient.getMoveAmmToPriceIx` now passes `getMMOracleDataForPerpMarket` into `calculateTargetPriceTrade` rather than `undefined`. It was already sizing the move against a zero-width spread on markets with a nonzero `baseSpread` and `curveUpdateIntensity`, and with the change above would have started throwing on `baseSpread == 0` markets as well.

**Behavioural note.** `calculateSpread` and `calculateSpreadReserves` now throw when `oraclePriceData` is omitted and `curveUpdateIntensity` is nonzero, where a `baseSpread` of 0 previously returned `[0, 0]` without needing an oracle. This is the same requirement markets with a nonzero `baseSpread` already had; callers on affected markets were silently receiving a zero-width spread instead. `getVammL2Generator` already requires `mmOraclePriceData`, so the in-repo L2 path is unaffected.

`calculateSpreadBN`, which does the actual work, was already faithful to the Rust — no spread math changed here, only whether it gets called. Driven through `calculateSpread`, the captured snapshot above now yields a `long_spread` of 111990 against the 111985 the market carried on chain, and a `short_spread` of 441 against 440. The small drift is expected: the SDK derives `liveOracleStd` and the confidence percentage from `now` rather than reading back the values the on-chain crank used.
