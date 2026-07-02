---
"@velocity-exchange/sdk": minor
---

Add admin controls for the new runtime feature gates on isolated positions and the LP-pool hedge. `FeatureBitFlags` gains `ISOLATED_POSITIONS`, a new `LpPoolFeatureBitFlags` enum is exported (`SETTLE_LP_POOL`/`SWAP_LP_POOL`/`MINT_REDEEM_LP_POOL`/`HEDGE`), and `AdminClient` gains `updateFeatureBitFlagsIsolatedPositions` / `updateFeatureBitFlagsHedge` (+ their `getUpdate…Ix` builders). Both gates default off on-chain; enabling requires the cold admin, and any hot admin can disable them as a kill switch.
