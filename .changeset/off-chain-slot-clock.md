---
'@velocity-exchange/sdk': minor
---

Add the off-chain slot clock the TypeScript clients resolve the live slot length with.

- `currentSlotClock(source, currentSlot, fallback)` returns `{ slotDurationMs, isLive }`, and
  `currentSlotDuration(...)` returns just the duration. Both delegate the staged-flip decision
  to `activeSlotDurationFromState`, so a client's prediction matches the on-chain value across
  a gate boundary. `source` is duck-typed on `{ getStateAccount() }`, keeping `math/time` free
  of client imports.
- `fallback` is required and has no default. The safe direction differs per call site: a
  user-protection window (signing budget, expiry countdown) passes the shortest scheduled slot
  so it under-promises, while a risk ceiling (staleness, rate limit) passes the longest so it
  tightens. A single shared default would be wrong for one of the two classes at every gate.
- A missing or `0` `currentSlot` resolves to the fallback rather than being read as slot zero.
  A failed slot subscription reports `0`, and slot zero precedes every effective slot, so it
  would otherwise return the pre-flip base while looking live.
- The keeper bots now import the shared resolver instead of keeping a local copy.

`SLOT_TIME_ESTIMATE_MS` remains exported and deprecated.
