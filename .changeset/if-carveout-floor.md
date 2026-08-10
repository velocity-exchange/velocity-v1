---
'@velocity-exchange/sdk': patch
---

`calculateInterestAccumulated` now applies the program's conservation clamp: the returned
`depositInterest` is scaled down when the tokens it would credit to `depositBalance` exceed the
tokens `borrowInterest` charges `borrowBalance`. The two are equal by construction (the deposit rate
is the borrow rate scaled by utilization), but utilization is derived from rounded token amounts and
sampled once for the whole interval, so a long projection at a high rate previously overstated the
deposit side by whole tokens.

The doc comment also records that the program now **defers** an accrual interval whose configured
`insuranceFund.ifFeeFactor` / `protocolFeeFactor` carveout would convert to less than one token
(OtterSec #127), so a projection from `lastInterestTs` can legitimately span a long window on such a
market even though the accrual has been cranked repeatedly.
