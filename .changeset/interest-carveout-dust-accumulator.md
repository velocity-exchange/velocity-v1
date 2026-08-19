---
'@velocity-exchange/sdk': patch
---

Carry sub-unit lending-interest carveouts instead of delaying the accrual interval.

`PoolBalance` gains `pendingInterestSplitDust` and `pendingInterestDust`. Both come from the
former padding, so the size of the struct and every other field offset are unchanged. A carveout
too small to pay in whole units now accumulates on the carveout pools and does not hold up the
interval. `cumulativeDepositInterest` and `lastInterestTs` therefore advance on every interval
that reaches a whole index unit on both sides. An interval under that floor stays on the clock
and is retried on the next crank. `calculateInterestAccumulated` documents the change. A
projection from `lastInterestTs` spans a window in which balances could have changed only by
that sub-unit remainder.
