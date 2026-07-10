---
'@velocity-exchange/sdk': patch
'@velocity-exchange/admin-cli': patch
---

Bankruptcy IF-fee floor (High audit fix): the permissionless fee sweep now leaves `bankruptcyIfFloorPct` of open-interest notional behind in `feeLedger.pendingIfFee`, so a sweep front-running a `resolvePerpBankruptcy` can no longer strip the first-loss tranche up to the floor. `PerpMarketAccount` gains `bankruptcyIfFloorPct` (repurposed padding — layout size unchanged; existing markets read 0 = disabled, new markets default to 10 bps), `AdminClient` gains `updatePerpMarketBankruptcyIfFloorPct`, and the admin CLI gains `perp-market set-bankruptcy-if-floor <market> <pct>`.
