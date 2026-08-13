---
'@velocity-exchange/admin-cli': patch
---

`user equity-floor-status` reads the fail-closed floor metric (`getFloorNetEquity`) and reports "invalid oracle: floor gates blocked" instead of describing the removed lower bound.
