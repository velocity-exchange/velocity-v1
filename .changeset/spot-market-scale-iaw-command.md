---
'@velocity-exchange/admin-cli': minor
---

Add `spot-market set-scale-initial-asset-weight-start` command: sets the deposit-notional threshold (QUOTE_PRECISION, 1e6) above which a spot market's initial asset weight scales down. `0` disables scaling. Warm/cold admin; maintenance weight is unaffected.
