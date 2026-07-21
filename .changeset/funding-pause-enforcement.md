---
'@velocity-exchange/sdk': patch
---

Enforce the funding pause on paths that previously bypassed it (OtterSec Medium findings). On-chain program hardening (no SDK API change): spot interest accrual now honors the exchange-wide `FundingPaused` bit on every call path (not just the dedicated crank), and `update_perp_bid_ask_twap` now no-ops when a market's `UpdateFunding` operation is paused so its funding-input mark/bid/ask TWAP stops advancing. Clients that predict spot interest or funding-input TWAP during a pause should account for the freeze.
