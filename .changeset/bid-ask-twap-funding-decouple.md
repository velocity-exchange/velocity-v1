---
'@velocity-exchange/sdk': patch
---

`updatePerpBidAskTwap` no longer updates the funding rate as a side effect.

The `update_perp_bid_ask_twap` program instruction previously refreshed the mark-price TWAP from caller-supplied DLOB depth and then applied the funding rate in the same instruction, letting the just-written TWAP feed funding at zero elapsed time. Funding is now decoupled: it runs only via the dedicated `update_funding_rate` crank (and on fills). Callers of `velocityClient.updatePerpBidAskTwap` / `getUpdatePerpBidAskTwapIx` that relied on the funding side effect must call `getUpdateFundingRateIx` separately.

Alongside this, two program-side hardening changes affect callers: the oracle-divergence filter used by the crank is now symmetric (DLOB levels are kept only within ±15% of the oracle on both sides), and `keeper_stats` must belong to the signing authority (`has_one`), so a caller can no longer pass a third party's staked `UserStats` to satisfy the insurance-fund stake gate. The SDK already passes the caller's own stats, so the normal happy path is unaffected.
