---
'@velocity-exchange/admin-cli': patch
---

`user equity-floor-status` now reports net equity, the quantity every onchain floor gate compares, instead of the initial-margin total collateral (which never subtracts spot borrows and applies asset weights). It reads the lower equity bound at the current slot and appends a stale-oracle note when any oracle is invalid, so the printed figure is not mistaken for exact.
