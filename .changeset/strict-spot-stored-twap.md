---
'@velocity-exchange/sdk': patch
---

Fix strict-mode spot margin pricing to use the stored 5-minute oracle TWAP, matching the program.

`User.getSpotMarketAssetAndLiabilityValue` and `User.getMarginCalculation` built their spot `StrictOraclePrice` from `calculateLiveOracleTwap`, which time-weights the stored TWAP back toward the live oracle price by the age of `lastOraclePriceTwapTs`. Spot markets only advance that timestamp when the market is touched, so it is routinely older than the 5-minute window — the projection then returned the live price exactly and strict mode became a no-op. `calculate_margin_requirement_and_total_collateral_and_liability_info` in the program uses `spot_market.historical_oracle_data.last_oracle_price_twap_5min` verbatim, so the SDK understated initial margin requirements (and overstated free collateral) for accounts holding spot borrows against a stale TWAP — a UI-computed "max withdrawal" could exceed what the program allows and fail with `InsufficientCollateral`.

Both call sites now read `historicalOracleData.lastOraclePriceTwap5Min` directly. Non-strict behavior is unchanged, and perp/AMM/funding uses of `calculateLiveOracleTwap` are untouched. The `now` parameter of `getSpotMarketAssetAndLiabilityValue` (and its wrappers) is retained for signature compatibility but no longer has any effect.
