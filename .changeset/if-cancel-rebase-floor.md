---
'@velocity-exchange/sdk': patch
---

Fix (#34): a market-level insurance-fund rebase could floor a small pending IF-stake unstake request to zero, after which cancel reverted (`InvalidIFUnstakeCancel`) and the stake was stranded — `remove` also rejected the zeroed request and `add`/re-`request` were blocked by the in-progress request. `cancel_request_remove_insurance_fund_stake` no longer re-checks the post-rebase share count, so a zeroed request cancels successfully, returning the intact rebased stake to active and abandoning only the dust request value.
