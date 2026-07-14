---
'@velocity-exchange/sdk': patch
---

Add `calculateUserProtectiveAssetPrice` and `calculateUserProtectiveLiabilityPrice` (`math/liquidation`), mirroring the program's user-protective conversion pricing for spot and pnl-vs-spot liquidations when the deposit or borrow oracle is margin-invalid (stale for margin / too uncertain): the collateral leg is priced at `max(oracle, 5min twap, oracle + confidence)` and the borrow leg at `min(oracle, 5min twap, oracle - confidence)` (floored at 1) instead of the raw oracle price. Feed the result into `calculateAssetTransferForLiabilityTransfer` to predict on-chain transfer amounts in that case.
