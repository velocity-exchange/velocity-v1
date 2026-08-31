---
'@velocity-exchange/admin-cli': minor
---

Add `lut show` and `lut extend`: inspect and extend the market address lookup table. The market account set is derived from `State`'s spot and perp market counts rather than a hardcoded list, so it cannot go stale when a market is added, and addresses already present are skipped. Defaults to the environment's configured table, refuses a frozen table, and checks the resulting size against the 256 entry limit.
