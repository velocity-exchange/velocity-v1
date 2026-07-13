---
'@velocity-exchange/sdk': minor
---

`getTriggerPrice` now mirrors the program's last-fill staleness guard: the last-fill leg of the median trigger price is ignored (oracle price substitutes) when the market's last fill is older than the new `TRIGGER_PRICE_LAST_FILL_MAX_AGE` export (5 minutes, read from `marketStats.lastTradeTs`).
