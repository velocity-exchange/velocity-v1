---
'@velocity-exchange/sdk': minor
---

Mirror the equity-breaker trip's dust-tolerant proof. New `User.getTripNetEquity(slot?)` returns the trip's net-equity upper bound and provability: positions with valid oracles are valued live, an invalid-oracle position within `EQUITY_FLOOR_TRIP_DUST_ALLOWANCE` at its own last twap is conceded its most favorable value instead of vetoing the proof, and a larger invalid position keeps the breach unprovable. New `User.provesEquityFloorBreach(slot?)` mirrors the onchain trip predicate exactly. `isBelowEquityFloor` is unchanged and documents that it compares the point value only.
