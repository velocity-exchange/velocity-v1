---
'@velocity-exchange/sdk': patch
---

`SpotMarketAccount.ifLastSettleVaultAmount` now holds the lowest insurance-fund vault balance
since the end of the last revenue settle, not an accounted shadow balance. The revenue-settle APR
cap is sized off `min(live IF vault, this)`, so a donation must stay in the fund for a whole
settle period to count. A `0` value means the market never settled revenue. The field name, type,
and account offset are unchanged.
