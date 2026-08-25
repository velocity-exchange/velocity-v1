---
'@velocity-exchange/sdk': minor
---

Add the off-chain slot clock the TypeScript clients resolve the live slot length with.

- `currentSlotClock(source, currentSlot)` returns `{ slotDurationMs, isLive }`, and
  `currentSlotDuration(...)` returns just the duration. Both delegate the staged-flip decision
  to `activeSlotDurationFromState`, so a client's prediction matches the on-chain value across
  a gate boundary. `source` is duck-typed on `{ getStateAccount() }`, keeping `math/time` free
  of client imports.
- When state or the slot feed is unavailable, both helpers fall back to the hardcoded 400ms
  `SLOT_DURATION_BASELINE` (the longest scheduled slot, so risk ceilings tighten). Callers do
  not pass a fallback.
- A missing or `0` `currentSlot` resolves to the baseline rather than being read as slot zero.
  A failed slot subscription reports `0`, and slot zero precedes every effective slot, so it
  would otherwise return the pre-flip base while looking live.
- The keeper bots now import the shared resolver instead of keeping a local copy.

`SLOT_TIME_ESTIMATE_MS` remains exported and deprecated.
