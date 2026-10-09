---
'@velocity-exchange/sdk': patch
---

`calculateNewAmm` and `calculateUpdatedAMM` now run a port of the program's `adjust_amm`, and
shared fixtures check it against the program. The k decrease uses the
program's arithmetic and lower bound, and its gain counts toward the cost, so the projected
update stays within the AMM's fees instead of being rejected. The SDK lowers k only where the
program does: curve update intensity of at least 100, the AMM able to lower k, and
`DisableFormulaicKUpdate` unset. Pass the market's fields through the new
`getKUpdateGate(market)` for an exact mirror. Without k, the peg moves as far as the plain budget
pays. `calculateNewAmm` returns the exact new curve as a sixth element.

`calculateLongShortFundingRate` compares long and short open interest by magnitude, so the larger
side gets the capped rate. It used to give longs the capped rate almost always, because
`baseAssetAmountShort` is negative.

Trade and position pricing (`calculateTradeSlippage`, `calculateTradeAcquiredAmounts`,
`calculateTargetPriceTrade`, `calculateBaseAssetValue`) go through the spread reserves when
`useSpread` is set, including on markets with `baseSpread` 0, as program fills do.

`calculateClaimablePnl` computes the pnl pool excess as `settle_pnl` does. The AMM fee pool no
longer counts, and net user pnl is floored at zero, so the result no longer exceeds what the
program settles.
