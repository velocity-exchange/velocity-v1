---
'@velocity-exchange/sdk': patch
'@velocity-exchange/admin-cli': patch
---

Add `perp-market deposit-fee-pool` and `perp-market sync-amm-summary-stats` to the admin CLI, for
recovering a market whose `total_fee_minus_distributions` has gone negative. Both SDK instruction
builders (`getDepositIntoPerpMarketFeePoolIx`, `getUpdatePerpMarketAmmSummaryStatsIx`) now take an
optional `admin` override so the hot role that actually signs can be passed, as the other hot-role
builders already allow.
