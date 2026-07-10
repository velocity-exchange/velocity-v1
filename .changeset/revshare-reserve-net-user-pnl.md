---
'@velocity-exchange/sdk': patch
---

Revenue-share sweep PnL-pool reservation (High audit fix): the builder/referrer revenue-share sweep (`sweep_completed_revenue_share_for_market`, run as a side-effect of permissionless `settle_pnl`/`settle_multiple_pnls`) checked only the raw PnL-pool balance against `fees_accrued` and never reserved `max(net_user_pnl, 0)`, so a caller could pay revenue share out of tokens backing a third party's positive unsettled PnL. The sweep now reserves the aggregate positive user claim and draws only the pool's excess over it, mirroring the protocol fee sweep. Program-only change (no account layout, IDL, or SDK API change); the revenue-share sweep is not reimplemented client-side.
