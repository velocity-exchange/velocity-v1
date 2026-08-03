---
'@velocity-exchange/sdk': patch
---

`delete_user` now takes the authority's `RevenueShareEscrow` PDA as a required fifth account, so it can
settle that sub-account's builder revenue-share rows before the sub-account id is retired forever
(OtterSec #128). Previously those rows became unreachable — the builder's accrued fee was stranded and
the market's `pending_revenue_share` stayed inflated for the life of the market.

**SDK callers need no change**: `getUserDeletionIx` / `deleteUser` derive and pass the account for you.
**Anyone building the instruction manually must add it**, including when the authority has never created
an escrow — the address is pinned by seeds on chain, so an uninitialized account proves absence rather
than signalling an omitted check.
