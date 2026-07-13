---
'@velocity-exchange/sdk': patch
---

Trigger orders now carry the order owner's `UserStats` account so the on-chain handler can enforce the authority-wide equity breaker: `getTriggerOrderIx` / `buildTriggerOrderInstruction` (and the `VelocityCore` wrapper) now derive and pass `userStats`. Mirrors the program fix that closes equity-floor/breaker enforcement gaps on `trigger_order`, `end_swap`, and `transfer_perp_position` (recipient side).
