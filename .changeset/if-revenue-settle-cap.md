---
'@velocity-exchange/sdk': patch
---

IF revenue-settle APR cap donation-proofing (High audit fix): `settle_revenue_to_insurance_fund` sized its per-period APR cap from the live insurance-fund vault token balance, which anyone can inflate with a direct SPL donation to lift the cap toward the 10%-of-revenue-pool bound right before a settle. The cap is now sized off `min(live_if_vault, if_last_settle_vault_amount)`, using the new `SpotMarket.if_last_settle_vault_amount` field — a donation-proof accounted balance grown by stakes + settled revenue and shrunk by withdrawals (repurposed trailing padding — account size unchanged at 808 bytes; existing accounts read 0 and self-seed on the first add/settle after upgrade). A raw SPL donation never touches those paths so it can't lift the cap, while legitimate stakes still do. SDK `SpotMarketAccount` gains `ifLastSettleVaultAmount: BN`. The display-only `nextRevenuePoolSettleApr` estimate is unchanged.
