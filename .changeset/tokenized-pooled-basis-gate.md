---
'@velocity-exchange/vaults-sdk': patch
---

`tokenizeShares` now fails with `InvalidTokenization` while the tokenized depositor's pooled value
is below its pooled cost basis.

A tokenized depositor carries one cost basis for every holder of its mint, and the profit-share fee
is collected by shrinking the pool's shares — so it dilutes every token equally regardless of who
accrued the loss. Minting into an under-water pool therefore handed the newcomer a slice of the
existing holders' loss shelter (OtterSec #140). Clients should surface the new failure and can
preflight it by comparing the depositor's value against `netDeposits + cumulativeProfitShareAmount`.

Redeeming is unaffected — existing holders can always exit — but a pool that has been under water
stays closed to *new* tokenizations until the vault recovers past the pooled high-water mark.

No SDK API change.
