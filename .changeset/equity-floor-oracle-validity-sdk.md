---
'@velocity-exchange/sdk': minor
---

Mirror the equity floor's oracle-validity gating: `User.getNetUsdValueBounds(slot)` computes the two-sided net equity bounds the onchain floor gates now use (invalid-oracle positions priced at both live and 5-minute TWAP, non-positive candidates dropped, unpriceable positions saturating to `I128_MIN`/`I128_MAX`), with pure helpers `boundPrices`, `getSpotOracleValidity`, `getSpotMaxConfidenceIntervalMultiplier` and `isOracleValidForMarginCalc`. `isBelowBufferedEquityFloor(slot?)` predicts the gates on the lower bound when given a slot and is unchanged otherwise. The equity floor guard bot now alerts distinctly, once per outage, when a breaker trip is blocked by `InvalidOracle` instead of logging it as a generic simulation failure.
