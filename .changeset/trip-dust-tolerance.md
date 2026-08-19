---
'@velocity-exchange/sdk': minor
---

Mirror the equity-breaker trip's dust-tolerant proof. New `User.getTripNetEquity(slot?)` returns the trip's net-equity upper bound and provability: positions with valid oracles are valued live, an invalid-oracle position is conceded its most favorable value instead of vetoing the proof (a liability or short base leg counts as zero at any size, an asset or long base leg within `EQUITY_FLOOR_TRIP_DUST_ALLOWANCE` at its own last twap counts as the allowance), and a larger invalid asset or long, or one whose twap is not positive, keeps the breach unprovable. New `User.provesEquityFloorBreach(slot?)` mirrors the onchain trip predicate exactly. `isBelowEquityFloor` is unchanged and documents that it compares the point value only.
