---
'@velocity-exchange/admin-cli': patch
---

`show perp-markets` now prints the AMM spread and inventory spread adjustments and the live long/short spreads, and uses the shared section layout.

`perp-market set-spread-adjustment` takes a market list (`0,1,4`) or `all` and writes every market in one transaction or proposal.
