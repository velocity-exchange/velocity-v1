---
'@velocity-exchange/sdk': patch
---

Carry sub-unit lending-interest carveouts instead of delaying the accrual interval.

`PoolBalance` gains `pendingInterestSplitDust` and `pendingInterestDust`. Both come from the
former padding, so the size of the struct and every other field offset are unchanged. A carveout
too small to pay in whole units now accumulates on the carveout pools and does not hold up the
interval. `cumulativeDepositInterest` and `lastInterestTs` therefore advance on every interval
that is owed. `calculateInterestAccumulated` documents the change. A projection from
`lastInterestTs` no longer spans a window in which balances could have changed.
