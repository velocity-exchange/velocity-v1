---
'@velocity-exchange/sdk': minor
---

Mirror the program's mark-TWAP re-seed, so `calculateAllEstimatedFundingRate` does not predict a
premium the next on-chain funding update will not charge.

`calculate_new_twap` weights an incoming mark-TWAP sample by the time since the last write and floors
the opposing weight at 1, so past one funding period a single fill-path sample replaces the TWAP
outright (bid/ask-crank samples are weight-capped since the crank sample-weight fix). The program now
discards the stored mark TWAPs and re-seeds them from the oracle TWAP when they went unwritten for
more than `max(fundingPeriod * 3, 3600)` seconds. The first funding update after such a gap therefore
sees a zero price spread and charges the baseline offset alone, and the real premium returns the
following period.

The gap is longest after a funding pause, because both funding cranks reject while the pause is set.
It also opens on any multi-period keeper outage.

`calculateLiveMarkTwap` applies the same threshold and re-seed, so the estimate tracks the program.
New export `MARK_TWAP_RESEED_FUNDING_PERIODS`.

New export `getMaxMarkTwapSampleElapsed`, the mirror of the program's
`MarketStats::max_mark_twap_sample_elapsed` crank sample-weight cap, for predicting the TWAP a
bid/ask crank write produces. `calculateLiveMarkTwap` deliberately does not apply it, because it
predicts the funding update's own write, which is uncapped on-chain. `ONE_MINUTE` is also exported.
