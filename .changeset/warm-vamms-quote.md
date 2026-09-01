---
'@velocity-exchange/sdk': patch
'@velocity-exchange/admin-cli': patch
---

Authorize the `VammQuoteManagement` hot role for scoped vAMM quoting setters, enforce protocol wide safety bounds for every hot role value, and keep oracle, MM reset, and formulaic k controls on warm/cold admin. Adds a direct `perp-market set-spread-adjustment` admin CLI command, tightens every `perp-market` positional to a strict decimal-integer parse (previously `Number('')`/`parseInt('0x10', 10)` silently resolved to market 0, and `new BN(' ')` hung the process), and lets `getUpdatePerpMarketAmmSpreadAdjustmentIx` / `getUpdatePerpMarketFundingBiasSensitivityIx` take an explicit `admin` authority so the CLI can route these setters through the hot role Squads vault instead of defaulting to cold admin.
