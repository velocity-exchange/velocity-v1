---
'@velocity-exchange/sdk': patch
---

`initializeRevenueShareEscrow` / `getInitializeRevenueShareEscrowIx` now revert with `UserNotFound`
(6234 / `0x185A`) when the authority has not created a subaccount yet, so the escrow must be created
after `initializeUserAccount` rather than after `initializeUserStats` alone (OtterSec #129).

The escrow snapshots `escrow.referrer` from `UserStats.referrer` once at creation and never rewrites
it, while `UserStats.referrer` is only ever set by the authority's first `initialize_user`. Because
the escrow's `authority` does not sign (only the payer does), a third party could previously create
an escrow in the window between those two calls and freeze a defaulted referrer into it, permanently
suppressing that authority's referral rewards and referee discount with no way to repair the field.

No SDK API change: signatures are unchanged and the standard onboarding order (subaccount 0 first,
which every existing client already uses) is unaffected. Only a flow that creates the escrow before
the first subaccount needs reordering.
