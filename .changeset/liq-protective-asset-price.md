---
'@velocity-exchange/sdk': patch
---

Add `calculateUserProtectiveAssetPrice`, mirroring the program's user-protective pricing of collateral seized in spot liquidations when the deposit oracle is margin-invalid (stale for margin / too uncertain): transfers are sized at `max(oracle, 5min twap, oracle + confidence)` instead of the raw oracle price.
