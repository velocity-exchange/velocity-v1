---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

Add per-market configurable withdraw circuit breaker and a daily deposit rate
cap.

The previously-hardcoded 25% daily withdraw circuit breaker is now configurable
per spot market via `SpotMarketAccount.withdrawCircuitBreakerPct` (basis points,
10000 = 100%; `0` keeps the default 25% = 2500 bps). A new daily deposit rate
cap mirrors the withdraw side: `depositGuardThreshold` (no cap below it) and
`maxDepositPctPerDay` (basis points; `0` disables) bound how far resulting
deposits may exceed the 24h deposit TWAP. It is enforced on the direct `deposit`
instruction and on the shared spot-credit path, so `transfer_pools` and
`end_swap` deposit credits are bounded too; it reverts with the new
`DailyDepositLimit` (6364) program error.

All three fields are carved from the existing 13-byte alignment gap before
`protocol_fee_pool`, so `SpotMarket` stays 808 bytes with every other field
offset unchanged and no account migration. Existing markets read the repurposed
bytes as 0 (default 25% breaker, disabled deposit cap). The two percentage
fields are `u16` basis points rather than `u32` PERCENTAGE_PRECISION so the set
fits the gap; `depositGuardThreshold` stays `u64` (token amount).

SDK: `SpotMarketAccount` gains `withdrawCircuitBreakerPct`,
`depositGuardThreshold`, and `maxDepositPctPerDay`; new
`AdminClient.updateSpotMarketWithdrawCircuitBreaker` /
`updateSpotMarketDepositCap` (and their `getUpdate…Ix` builders); new math
helpers `calculateMaxDepositTokenAmount` / `checkDepositLimits`; the existing
`calculateWithdrawLimit` now honors the configurable breaker (all in basis
points).

Admin CLI: new `spot-market set-withdraw-breaker <market> <pct>` and
`spot-market set-deposit-cap <market> <threshold> <pctPerDay>` commands (pct in
basis points).
