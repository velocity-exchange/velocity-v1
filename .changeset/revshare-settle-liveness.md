---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

Accrued builder/referrer revenue share can now be collected without the escrow owner's participation, and is paid out rather than written off when a market is delisted.

`settleRevenueShare` / `getSettleRevenueShareIx` wrap the new permissionless `settle_revenue_share` instruction, which settles one escrow's rows for one perp market out of that market's pnl pool. Previously the only payer ran inside `settlePNL` and only when that settle actually moved PnL, so once an escrow owner flattened and stopped trading a market their beneficiaries' fees were stranded and the market's `pendingRevenueShare` kept reserving pnl-pool value against a claim nobody could settle.

`forfeitRevenueShareOrder` / `getForfeitRevenueShareOrderIx` wrap `forfeit_revenue_share_order`, which writes off a row of a market in settlement or delisted that provably cannot be paid — the beneficiary has no payout account, the wound-down pool cannot cover it, or it names no reachable beneficiary. Anything still payable is rejected with `RevenueShareOrderNotForfeitable` (6372).

Delisting a market now requires that revenue share to have been resolved: `settle_expired_market_pools_to_revenue_pool` rejects with `UnsettledRevenueShareOnDelist` (6371) while `pendingRevenueShare` is non-zero. There is no time-based escape, because between the two instructions above every row is terminally resolvable. A delisted market therefore always reports `pendingRevenueShare` as zero, and consumers must not treat a delisted market's counter as an outstanding liability.

`RevenueShareEscrowMap.getEscrowsOwingRevenueShare(marketIndex)` returns the escrows still owed on a market — the work list to clear before delisting. `calculateRevenueShareSweepAvailable`, `calculateBankruptcyIfTrancheReservation` and `calculateBankruptcyIfFloor` (`math/market`) mirror the reservation the on-chain sweep applies, so a keeper can predict whether a call will pay before sending it.

CLI: new `velocity-admin fees settle-revenue-share <market> [escrowAuthority]`, with `--all` to scan a market, settle every escrow still owed, and forfeit any stragglers that cannot be paid.
