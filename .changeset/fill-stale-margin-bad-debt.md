---
'@velocity-exchange/sdk': patch
---

New error code `SpotMarketInterestStaleForMargin` (6368). A risk-increasing perp fill or a
withdrawal now reverts when a spot market carrying one of the account's **borrows** has not
accrued interest within the last hour, because margin would otherwise value that debt through a
stale `cumulative_borrow_interest` and understate it (OtterSec #135 / #148). Recoverable without
special privileges: `update_spot_market_cumulative_interest` is permissionless and can be bundled
into the same transaction. A risk-increasing fill can also now revert with `InvalidOracle` when one
of the filling account's spot collateral/borrow oracles is stale for margin (OtterSec #143 / #144).
