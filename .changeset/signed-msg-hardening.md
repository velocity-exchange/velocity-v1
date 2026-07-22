---
'@velocity-exchange/sdk': minor
---

Signed-message ("swift") taker-order hardening (OtterSec Medium findings). `placeSignedMsgTakerOrder` is now rejected while the exchange is fully paused (`ExchangePaused`), matching normal order placement. `resizeSignedMsgUserOrders` may only be shrunk by the account's `authority` — a per-sub-account delegate can no longer shrink the authority-scoped replay account and evict other sub-accounts' replay protection. The redundant `user` account was removed from the on-chain `resizeSignedMsgUserOrders` instruction, so `VelocityClient.resizeSignedMsgUserOrders` and `getResizeSignedMsgUserOrdersInstruction` drop their trailing `userSubaccountId` parameter.
