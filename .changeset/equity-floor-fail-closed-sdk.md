---
'@velocity-exchange/sdk': minor
---

Mirror the fail-closed equity-floor oracle handling: `User.getNetUsdValueBounds` is replaced by `User.getFloorNetEquity(slot)` returning `{ value, allOraclesValid }` (exact net equity plus the validity verdict, matching the program's floor metric); `boundPrices`, `NetUsdValueBounds`, `I128_MIN` and `I128_MAX` are removed. `isBelowBufferedEquityFloor(slot)` now predicts the fail-closed gates (any invalid oracle reads as gated), and `getEquityAboveFloor(slot)` / `getEquityAboveBufferedFloor(slot)` report zero headroom while any oracle is invalid.
