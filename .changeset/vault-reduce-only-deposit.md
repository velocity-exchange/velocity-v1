---
'@velocity-exchange/vaults-sdk': patch
---

Add the `DepositNotFullySettled` (6029) vaults error. `deposit`, `manager_deposit` and
`manager_repay` now revert with it when Velocity accepts less than the full amount, which
happens when the denomination market is `ReduceOnly` and the amount is larger than the vault's
outstanding borrow. Previously the excess stayed in the vault's transit token account with no
claim on it.
