---
'@velocity-exchange/vaults-sdk': patch
---

Add the `DepositNotFullySettled` (6029) vaults error. `deposit`, `manager_deposit` and
`manager_repay` now revert with it when Velocity accepts less than the full amount, which
happens when the market being deposited into is `ReduceOnly` and the amount is larger than the
vault's outstanding borrow there. For `deposit` and `manager_deposit` that is the denomination
market; for `manager_repay` it is the repay market. Previously the excess stayed in the
vault's transit token account with no claim on it.
