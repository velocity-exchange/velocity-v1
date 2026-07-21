---
'@velocity-exchange/sdk': patch
---

Fix `getUpdateAMMsIx`/`updateAMMs` for `Prelaunch`-oracle-source perp markets: the crank loads each market as writable and the program refreshes a prelaunch oracle in place, so that oracle account must be passed writable. It was hard-coded read-only, making `updateAMMs` on a prelaunch market revert with "instruction modified data of a read-only account". Prelaunch oracles are now marked writable (matching `addPerpMarketToRemainingAccountMaps`); non-prelaunch oracles are unaffected.
