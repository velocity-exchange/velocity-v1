---
'@velocity-exchange/admin-cli': patch
---

Add `velocity-admin show state`: dumps every field of the singleton State account, with the `exchangeStatus`, `featureBitFlags`, `lpPoolFeatureBitFlags` and `solvencyStatus` bitmasks decoded to their bit names. The `feature-flags` subcommands only write bits; there was no way to read the current ones back.
