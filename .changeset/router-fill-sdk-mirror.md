---
'@velocity-exchange/sdk': minor
---

Add the router fill math mirror: `splitAcrossQuoters` (priority-tier split with
step quantization) and `vammQuoteLevels` (the vAMM's quote ladder, priced from
the same swap math the program executes, with last-look shading against rival
books). Perp fills now route through this split on-chain, so predicting a fill
requires reproducing it — `calculateTradeSlippage` and
`calculateBaseAssetAmountForAmmToFulfill` still use the legacy AMM-only path and
will be rewired onto the mirror next.
