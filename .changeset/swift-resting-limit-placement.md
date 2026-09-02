---
'@velocity-exchange/sdk': patch
---

Add `signedMsgOrderPlaceable` and `isRestingSignedMsgLimitOrder` (with `SIGNED_MSG_MAX_ORDER_AGE_MS`), mirroring the program's `place_signed_msg_taker_order` slot gates: a limit order with no auction may now be placed ahead of its message slot, which is its placement deadline, within the ~200s window; auction orders still wait for their message slot (`signedMsgOrderSlotReached`).
