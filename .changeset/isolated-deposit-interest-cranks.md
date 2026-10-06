---
'@velocity-exchange/sdk': patch
---

Paths that move collateral from cross into an isolated perp position now prepend
`updateSpotMarketCumulativeInterest` for every stale spot market the sub-account borrows from:
`transferIsolatedPerpPositionDeposit` with a positive amount, and the `isolatedPositionDepositAmount`
option of `prepareMarketOrderTxs`, `placePerpOrder`, `preparePlaceOrdersTx` and
`preparePlaceAndTakePerpOrderWithAdditionalOrders`. The program now values the sub-account's borrows
on that transfer and reverts with `SpotMarketInterestStaleForMargin` (6371) when one is stale.

`transferPerpPosition`, the vAMM-hedger transfer and the liquidator side of the four liquidation
instructions can also revert with 6371. Prepend `getStaleSpotInterestCrankIxs` for the accounts
involved; `transferPerpPosition` needs `getTransferPerpPositionIx` to do so.
