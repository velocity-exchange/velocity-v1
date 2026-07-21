---
'@velocity-exchange/sdk': patch
---

Pnl-pool fee-sweep reservations (three related High audit fixes). The permissionless sweeps that drain a perp market's PnL pool now reserve every token another claimant is owed before draining:

- **#48** — the builder/referrer revenue-share sweep (`sweep_completed_revenue_share_for_market`, a side-effect of permissionless `settle_pnl`/`settle_multiple_pnls`) checked only the raw PnL-pool balance against `fees_accrued` and never reserved `max(net_user_pnl, 0)`, so a caller could pay revenue share out of tokens backing a third party's positive unsettled PnL. It now reserves the aggregate positive user claim (`net_user_pnl` valued at the market's oracle price, validity-gated in-slot by the preceding settle).
- **#53** — the protocol fee sweep (`sweep_market_fees`) let its buffer-exempt protocol-fee drain move the tokens backing the floored `pending_if_fee` bankruptcy tranche into `protocol_fee_pool` (outside the insurance backstop) without touching the counter, so a later bankruptcy resolution cancelled the loss counter-only against an unbacked tranche and left surviving-trader PnL short. Every drain (protocol included, and the revenue-share sweep) now reserves `min(pending_if_fee, get_bankruptcy_if_floor())` on top of user PnL.
- **#73** — `sweep_market_fees` drained protocol fees without reserving already-accrued builder/referrer revenue share, briefly leaving those claims unpayable. A new per-market counter `PerpMarket.pending_revenue_share` (`PerpMarketAccount.pendingRevenueShare`, QUOTE_PRECISION) tracks accrued-but-unpaid revenue share and is reserved by the sweep. It reuses the alignment padding before `amm`, so `PerpMarket` stays 1304 bytes with all offsets unchanged (existing accounts read 0).

Adds `PerpMarketAccount.pendingRevenueShare` to `types.ts` + the IDL. No instruction or SDK-API change; the sweeps are program-internal and not reimplemented client-side.
