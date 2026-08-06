---
'@velocity-exchange/sdk': minor
---

Mirror the program's clamp on a perp auction's baseline start offset, and document that
`updatePerpBidAskTwap` / `getUpdatePerpBidAskTwapIx` only sample DLOB orders that have rested
on-chain for at least 24 slots (~10s).

`getTriggerAuctionStartPrice` (and so `getTriggerAuctionStartAndExecutionPrice`) now clamps the
baseline start offset to ±(oracle TWAP / tier divisor) before applying the start buffer, matching
`OrderParams::get_perp_baseline_start_price_offset`. The bound is 2% of the oracle TWAP on tier A, 5%
on B and C, 10% on Speculative, 20% on HighlySpeculative and Isolated. Predicted start prices change
only for markets whose mark TWAP sits outside that band. Two new exports expose the bound:
`getAuctionEndMinMaxDivisors` and `getPerpBaselineMaxPriceOffset`.

The program also now ignores quotes younger than 24 slots when estimating the market's bid/ask for
the mark TWAP (OtterSec #146: previously a caller could place a self-crossed pair of post-only
quotes, crank, and cancel in a single transaction, moving the TWAP that prices a third party's
forced-close auction band without ever being exposed to a fill). Keeper operators should know that
makers who cancel/replace faster than ~10s no longer contribute to the estimate, and that passing
only freshly-placed makers yields no DLOB estimate at all — the crank falls back to the AMM's quote.
