---
'@velocity-exchange/sdk': patch
---

`SpotMarketAccount.ifLastSettleVaultAmount` now holds the insurance-fund vault balance at the end
of the last revenue settle, not an accounted shadow balance. The revenue-settle APR cap is sized
off `min(live IF vault, this)`, so a donation must be present at both endpoints of a settle period
to count. A `0` value means the market never settled revenue. The field name, type, and account
offset are unchanged.
