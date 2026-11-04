---
'@velocity-exchange/admin-cli': minor
---

`spot-market set-withdraw-breaker` now takes one or more `<market>:<bps>` pairs and sets them all
in one transaction or proposal. The old `<market> <pct>` form is gone. Before it sends, the command
prints each market's withdraw headroom and Grafana breaker consumption under the current and the new
setting. `audit withdrawals` now computes breaker consumption from the TWAP the next transaction
will see, which matches market-stats-bot v0.1.37 and the Grafana alert.
