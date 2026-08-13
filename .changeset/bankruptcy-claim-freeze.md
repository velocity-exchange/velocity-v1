---
'@velocity-exchange/sdk': patch
'@velocity-exchange/admin-cli': patch
---

`PerpMarketAccount.pendingBankruptcyClaims` mirrors the new per-market counter of unresolved
bankrupt quote debts. While it is above zero the program's fee sweep withholds the whole
`feeLedger.pendingIfFee`, so a permissionless sweep cannot drain the bankruptcy first-loss tranche
between the latch and the resolution. `PositionFlag.BankruptcyClaim` marks the position whose debt
is counted.

`bankruptcyIfFloorPct` changes meaning: `0` now selects `DEFAULT_BANKRUPTCY_IF_FLOOR_PCT` (10 bps),
which is what a market written before the field existed reads, and the new
`BANKRUPTCY_IF_FLOOR_DISABLED` sentinel turns the standing floor off. Callers that passed `0` to
`AdminClient.updatePerpMarketBankruptcyIfFloorPct` to disable the floor must pass the sentinel
instead. The admin CLI accepts `perp-market set-bankruptcy-if-floor <market> disabled`.
