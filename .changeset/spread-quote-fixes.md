---
'@velocity-exchange/sdk': minor
---

Mirror the program's vAMM quoting fixes.

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
