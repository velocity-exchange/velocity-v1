---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

Per-user equity floor: new warm-admin instruction `update_user_equity_floor` sets `User.equityFloor` (QUOTE_PRECISION), a minimum cross-margin total collateral below which the program rejects risk-increasing order placement and fills, withdrawals, and transfers out of the account with `EquityBelowFloor` (6358); reduce-only activity stays allowed and 0 disables. SDK adds `AdminClient.updateUserEquityFloor` / `getUpdateUserEquityFloorIx`, `UserAccount.equityFloor`, `User.isBelowEquityFloor` / `getEquityAboveFloor`, and floor-aware `getWithdrawalLimit`. Admin CLI adds `velocity-admin user set-equity-floor <user> <floor>`.
