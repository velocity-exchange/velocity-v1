---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

Add per-market configurable withdraw circuit breaker and a daily deposit rate
cap.

The previously-hardcoded 25% daily withdraw circuit breaker is now configurable
per spot market via `SpotMarketAccount.withdrawCircuitBreakerPct`
(PERCENTAGE_PRECISION; `0` keeps the default 25%). A new daily deposit rate cap
mirrors the withdraw side: `depositGuardThreshold` (no cap below it) and
`maxDepositPctPerDay` (`0` disables) bound how far resulting deposits may exceed
the 24h deposit TWAP, enforced on the user `deposit` instruction with the new
`DailyDepositLimit` (6357) program error.

SDK: `SpotMarketAccount` gains `withdrawCircuitBreakerPct`,
`depositGuardThreshold`, and `maxDepositPctPerDay`; new
`AdminClient.updateSpotMarketWithdrawCircuitBreaker` /
`updateSpotMarketDepositCap` (and their `getUpdate…Ix` builders); new math
helpers `calculateMaxDepositTokenAmount` / `checkDepositLimits`; the existing
`calculateWithdrawLimit` now honors the configurable breaker.

Admin CLI: new `spot-market set-withdraw-breaker <market> <pct>` and
`spot-market set-deposit-cap <market> <threshold> <pctPerDay>` commands.

Also fixes a latent `SpotMarket` decode bug: `_padding_align_pfp` was widened
from `[u8; 8]` to `[u8; 13]` so the IDL models the repr(C) alignment pad before
`protocol_fee_pool` explicitly. No on-chain byte offset changed, but decoding of
`protocolFeePool` / `protocolLiquidationFee` / `protocolFeeFactor` (added in the
fee redesign) was previously reading 5 bytes early and returned garbage for any
non-zero value.
