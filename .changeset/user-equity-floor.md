---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

Per-user equity floor: new warm-admin instruction `update_user_equity_floor` sets `User.equityFloor` (QUOTE_PRECISION), a minimum cross-margin total collateral below which the program rejects risk-increasing order placement and fills, withdrawals, and transfers out of the account with `EquityBelowFloor` (6358); reduce-only activity stays allowed and 0 disables. `transferDepositByDelegate` gains an `equityFloorDelta` argument (instruction signature change) that atomically moves floor along with funds between same-authority subaccounts, preserving the sum of floors (`InvalidEquityFloorTransfer`, 6359). SDK adds `AdminClient.updateUserEquityFloor` / `getUpdateUserEquityFloorIx`, `UserAccount.equityFloor`, `User.isBelowEquityFloor` / `getEquityAboveFloor`, floor-aware `getWithdrawalLimit`, and an `'auto'` floor-delta mode on `transferDepositByDelegate` (quote market) that computes the minimal floor to carry. Admin CLI adds `velocity-admin user set-equity-floor <user> <floor>`.
