---
'@velocity-exchange/sdk': patch
---

Fix `SwiftOrderSubscriber` crashing the process instead of reconnecting.

- The `close` and `error` handlers were registered inside the socket's `open` callback, so a
  socket that failed *during* the handshake — connection refused, reset, the server pod being
  evicted — emitted `'error'` with no listener attached. Node throws on an unhandled `'error'`
  event, so the process died rather than retrying. Both handlers, plus `unexpected-response`, are
  now registered on the socket immediately.
- Reconnects use jittered exponential backoff (500ms base, 30s cap, reset once the auth handshake
  completes) in place of a flat 1s retry, so a fleet of subscribers knocked off the same server
  does not retry in lockstep.
- A single disconnect normally emits both `close` and `error`, and the heartbeat timer could fire
  on top of them. Each of the three previously scheduled its own reconnect, leaving duplicate
  sockets; reconnects are now idempotent per disconnect.
- A reconnect re-entered `subscribe(onOrder)` with no further arguments, silently dropping the
  caller's `acceptSanitized` and `acceptDepositTrade` options. Both are now retained.
- `unsubscribe()` no longer no-ops when called before the subscription is established, and it
  cancels any pending reconnect so a queued timer cannot resurrect the socket.

`IndicativeQuotesSender` had the same nested-handler crash path and gets the same treatment:
handlers registered on the socket immediately, jittered exponential backoff in place of a
plain doubling delay, reconnects idempotent per disconnect, and `connected` reset on
disconnect rather than staying `true` until the next successful auth.
