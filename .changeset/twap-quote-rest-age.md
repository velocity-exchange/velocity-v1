---
'@velocity-exchange/sdk': patch
---

Document that `updatePerpBidAskTwap` / `getUpdatePerpBidAskTwapIx` only sample DLOB orders that
have rested on-chain for at least 24 slots (~10s).

The program now ignores quotes younger than that when estimating the market's bid/ask for the mark
TWAP (OtterSec #146: previously a caller could place a self-crossed pair of post-only quotes, crank,
and cancel in a single transaction, moving the TWAP that prices a third party's forced-close auction
band without ever being exposed to a fill). Keeper operators should know that makers who
cancel/replace faster than ~10s no longer contribute to the estimate, and that passing only
freshly-placed makers yields no DLOB estimate at all — the crank falls back to the AMM's quote.

No SDK API change.
