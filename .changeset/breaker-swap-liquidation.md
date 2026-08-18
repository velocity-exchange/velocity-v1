---
'@velocity-exchange/sdk': minor
---

Gate swap-backed spot liquidation on the authority-wide equity breaker. `liquidateSpotWithSwapBegin`/`...End` now require the liquidator's `UserStats` account and `begin` reverts with `EquityBelowFloor` while the liquidator authority's equity breaker is tripped, matching the other liquidator routes. `getLiquidateSpotWithSwapIx` and `getJupiterLiquidateSpotWithSwapIxV6` resolve the new account automatically; keepers building the instructions by hand must pass `liquidatorStats`.
