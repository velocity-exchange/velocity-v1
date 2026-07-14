---
'@velocity-exchange/sdk': patch
---

Trigger orders now carry the order owner's `UserStats` account so the on-chain handler can enforce the authority-wide equity breaker: `getTriggerOrderIx` / `buildTriggerOrderInstruction` (and the `VelocityCore` wrapper) now derive and pass `userStats`. Mirrors the program fix that closes equity-floor/breaker enforcement gaps on `trigger_order`, `end_swap`, and `transfer_perp_position` (recipient side).

Two further equity-floor/breaker gaps are closed: (1) `transfer_deposit_by_delegate` now rejects (`InvalidEquityFloorTransfer`) any floor-delta that would reduce a sub-account's floor while that sub-account is already below the floor being reduced, so an owner can't shed floor off a breached sub-account with a zero-amount transfer to defuse a pending breaker trip; and (2) a tripped authority is now barred (`EquityBelowFloor`) from position-acquiring liquidations (`liquidatePerp`, `liquidateSpot`). `liquidateSpot` gains a required `liquidatorStats` account — `getLiquidateSpotIx` already supplies it.
