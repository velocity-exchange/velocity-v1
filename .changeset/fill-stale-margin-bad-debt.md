---
'@velocity-exchange/sdk': patch
---

New error code `SpotMarketInterestStaleForMargin` (6371). A value-releasing path now reverts when
a spot market carrying one of the account's **borrows** has not accrued interest recently enough,
because margin would otherwise value that debt through a stale `cumulative_borrow_interest` and
understate it (OtterSec #135 / #148). The gated paths are withdraw, transfer deposit, transfer
pools, swap, isolated-position withdraw, and a perp fill — for the taker and for every maker
alike, whichever direction the fill moves each position. A borrow whose un-booked interest is
still under one token unit is exempt, so a dust-sized market that cannot book its interval does
not lock the account out. Liquidations are never gated.

Each market gets its own staleness window from its rate ceiling. The bound holds the omission
under one basis point of the debt, so a market that may charge more interest must be cranked more
often. `MAX_SPOT_INTEREST_STALENESS_FOR_MARGIN` (one hour) caps the window.

A perp fill also no longer credits a spot deposit whose oracle is invalid for margin
(OtterSec #143 / #144). The deposit contributes zero collateral, which is what the withdraw path
already does, so a fill that needed it now reverts with `InsufficientCollateral`. A fill by an
account holding a spot **borrow** whose oracle is invalid for margin reverts with `InvalidOracle`.

**Required change for anyone building fill or withdraw transactions.** No gated path cranks the
markets it does not itself touch, so a taker or maker holding a borrow in a quietly-traded spot
market becomes unfillable until that market is accrued. Recovery needs no privileges:
`update_spot_market_cumulative_interest` is permissionless and can ride in the same transaction.

New SDK helpers build exactly that:

- `MAX_SPOT_INTEREST_UNDERSTATEMENT_FOR_MARGIN` and `MAX_SPOT_INTEREST_STALENESS_FOR_MARGIN` —
  the program's bounds.
- `maxSpotInterestStalenessForMargin(spotMarket)` — one market's window.
- `VelocityClient.getStaleSpotInterestMarketIndexes(userAccounts, now?)` — the markets that need
  a crank for those accounts.
- `VelocityClient.getStaleSpotInterestCrankIxs(userAccounts, now?)` — one
  `updateSpotMarketCumulativeInterest` instruction per such market. Prepend them to the fill,
  withdraw, transfer, or swap.

Pass a fill's taker and every maker. The helpers do not model the sub-token exemption, so they
name a superset of what the program requires; cranking all of them always clears the check.
