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
  `SLOT_DURATION_BASELINE`, the longest scheduled slot. Callers do not pass a fallback. That
  errs toward fewer slots when converting ms into slots (a threshold closes sooner) and toward
  up to 2x more ms when converting slots into ms (a countdown stays open longer), so a call
  site in the second direction should branch on `isLive` and use `SLOT_DURATION_FLOOR`.
- A missing, `0`, negative or non-finite `currentSlot` resolves to the baseline rather than
  being read as a slot number. A failed slot subscription reports `0`, and slot zero precedes
  every effective slot, so it would otherwise return the pre-flip base while looking live.
- `isLive` is only true for a fully decoded `State`: the staging fields are validated as
  non-negative integers and a real `BN` effective slot, so a partial or hand-built object
  falls back instead of reporting a `NaN` duration as a measurement.
- New `SLOT_DURATION_SCHEDULE_MS` (the mirror of the program's `[400, 350, 300, 250, 200]`)
  and `SLOT_DURATION_FLOOR` (200ms), the value a user-protection window substitutes when the
  feed is dead.
- The keeper bots now import the shared resolver instead of keeping a local copy.

`SLOT_TIME_ESTIMATE_MS` remains exported and deprecated.
