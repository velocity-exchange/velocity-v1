---
'@velocity-exchange/sdk': minor
---

The program's price-band and divergence gates now measure against a settled oracle-TWAP anchor rather than the live TWAP, and the SDK mirrors it. `PerpMarketAccount` and `SpotMarketAccount` gain `settledOracleTwaps` (`SettledOracleTwaps`), and `math/oracles` exports `getSettledOraclePriceTwap5Min` / `getSettledOraclePriceTwap`, which fall back to the live TWAP while the anchor is unseeded. `isOracleTooDivergent` gained a second parameter, the market's `settledOracleTwaps` — a breaking signature change. Anything predicting whether a fill, swap, liquidation or funding crank will breach a band must read the anchor; valuation (`StrictOraclePrice`, margin, liquidation prices) still reads the live TWAPs. Regenerate any custom account decoder: both market accounts grew by 32 bytes at the tail.
