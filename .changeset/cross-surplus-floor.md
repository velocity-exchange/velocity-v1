---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

Cross-match cranks now have a per-market profit floor. `update_perp_market_clob_quoter` takes a third argument, `min_cross_surplus` (QUOTE_PRECISION), and the crank reverts unless the protocol's quote surplus reaches it — cranking a cross pays the reservoir's keeper fee, so a cross that clears by a cent is one worth declining. Zero keeps the previous strictly-profitable rule.

SDK: `ClobCrankConditionsV0Account.minCrossSurplus`. Admin CLI: `--min-cross-surplus` on `clob-market init` and `quoter set-market-clob`.
