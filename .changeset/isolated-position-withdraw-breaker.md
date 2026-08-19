---
'@velocity-exchange/sdk': patch
---

`withdrawFromIsolatedPerpPosition` now respects the spot market's withdraw circuit breaker onchain, at market level and without the small-depositor exception, so it can revert with `DailyWithdrawLimit` (6128) even when the isolated position holds the requested amount. `getWithdrawFromIsolatedPerpPositionIxsBundle` documents this; its clamp still bounds the request by the position's own balance only, so read `withdrawLimit` from `calculateWithdrawLimit` for the market's remaining room. The isolated-position instructions are behind the `isolated-position` program feature, which is not in the mainnet default feature set.
