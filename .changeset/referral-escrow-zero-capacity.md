---
'@velocity-exchange/sdk': patch
---

`initializeRevenueShareEscrow` / `getInitializeRevenueShareEscrowIx`: the program now rejects
`numOrders == 0`, so a zero-capacity escrow reverts with `DefaultError` instead of being created in a
silently inert state where no builder or referral row can be held and all revenue share is suppressed
(OtterSec #114). `numOrders` must be at least 1; the SDK signature is unchanged.

Both doc comments also now record that `escrow.referrer` is snapshotted from `UserStats.referrer` once
at creation and never re-read, so the escrow should be created _after_ the authority's first
`initializeUser` — otherwise it permanently holds no referrer.
