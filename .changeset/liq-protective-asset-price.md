---
'@velocity-exchange/sdk': patch
---

Add `calculateUserProtectiveAssetPrice` and `calculateUserProtectiveLiabilityPrice`, mirroring the program's user-protective pricing of spot-liquidation transfers when the deposit or borrow oracle is margin-invalid (stale for margin / too uncertain): the collateral leg is priced at `max(oracle, 5min twap, oracle + confidence)` and the borrow leg at `min(oracle, 5min twap, oracle - confidence)` instead of the raw oracle price.
