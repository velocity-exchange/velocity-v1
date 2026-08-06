---
'@velocity-exchange/sdk': patch
---

New error code `SpotMarketInterestStaleForMargin` (6369). A risk-increasing perp fill or a
withdrawal now reverts when a spot market carrying one of the account's **borrows** has not
accrued interest within the last hour, because margin would otherwise value that debt through a
stale `cumulative_borrow_interest` and understate it (OtterSec #135 / #148). A risk-increasing fill
can also now revert with `InvalidOracle` when one of the filling account's spot collateral/borrow
oracles is stale for margin (OtterSec #143 / #144).

**Required change for anyone building fill transactions.** The staleness check reads the market's
`lastInterestTs`, and no fill path cranks it, so a taker or maker holding a borrow in a
quietly-traded spot market becomes unfillable until that market is accrued. Recovery needs no
privileges: build `updateSpotMarketCumulativeInterestIx(marketIndex)` for every spot market the
taker and each maker hold a borrow in whose `lastInterestTs` is more than an hour old, and prepend
those instructions to the fill. Fillers that do not bundle them will see `SpotMarketInterestStaleForMargin`
on accounts that previously filled. The same applies to withdrawals for the account's markets other
than the one being withdrawn, which the withdraw handler already cranks itself.
