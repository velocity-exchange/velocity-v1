---
'@velocity-exchange/sdk': patch
---

Stop the websocket resubscribe watchdogs wedging on a hung teardown.

`SlotSubscriber`, `SlothashSubscriber`, `ClockSubscriber`, `WebSocketProgramAccountSubscriber` and `WebSocketLogProvider` all re-arm their resub timer only at the end of a successful `subscribe()`. On a half-open websocket `removeSlotChangeListener` / `removeAccountChangeListener` / `removeOnLogsListener` never resolves, so the `await` inside the timer callback pended forever and the timer chain ended: no throw, no log, no further resubscribe. The subscriber then reported a frozen slot (or stale accounts) for as long as the process lived.

Each teardown is now bounded by a 10s `promiseTimeout` with forced cleanup of `subscriptionId`/`listenerId` and `isUnsubscribing`, matching what `WebSocketAccountSubscriber` already did, and the resub callback re-arms in a `finally` so a failed resubscribe retries instead of ending the chain.

`unsubscribe()` therefore now always settles within ~10s instead of potentially never, and a rejection from the underlying `remove*Listener` is logged and swallowed rather than propagated.
