---
'@velocity-exchange/sdk': patch
'@velocity-exchange/vaults-sdk': patch
'@velocity-exchange/admin-cli': patch
---

Correct the slot-duration scaling in the off-chain mirrors and the staged-switch setter.

- `activeSlotDurationFromState` is now applied wherever the Rust SDK, the swift server, and
  the account-list builder previously read the raw `State.slotDurationMs` base field. That
  field lags a staged switch until the following gate is staged, so the mirrors sized oracle
  staleness windows, the signed-order age limit, and the auction band check off the
  pre-switch duration.
- `update_state_slot_duration_ms` commits an already-effective promotion when the gate
  schedule is exhausted instead of reverting it, so the base field never stays a step behind
  the live value.
- Reference-price-offset smoothing accrues its budget per elapsed millisecond instead of per
  whole 400ms period. Flooring to whole periods zeroed the budget for any crank gap under
  400ms, which pinned the step to the minimum and made convergence slower the more often a
  market was cranked.
- `math/time.ts` imports `BN` from the isomorphic entry point, keeping Anchor out of the
  browser bundle.
- The SDK's oracle staleness allowance is a wall-clock duration rather than a fixed five
  slots.
- Three velocity/jit-proxy instructions and three vaults instructions now take velocity's
  `State` so their oracle windows match the rest of the protocol. Hand-built transactions
  must add the account; the SDKs and CLI fill it in.
- `velocity-admin exchange set-slot-duration-ms` previews the live duration instead of the
  base field.
- `pythLazerCranker`'s post ceiling is a fixed wall-clock interval again, so the post rate
  does not double at each gate.
