---
'@velocity-exchange/sdk': minor
'@velocity-exchange/vaults-sdk': patch
---

Preserve insurance fund revenue as a spot vault receivable.

`SpotMarketAccount.spotFeePool` is renamed `insuranceFundRevenueReceivable`. It
keeps its `PoolBalance` type, width and offset, so account size and every other
field offset are unchanged and code only has to rename the field.

The pool holds revenue that is allocated to the insurance fund but still sits in
the spot vault, which happens while a withdraw pause blocks the transfer. Only
`scaledBalance` is used, and it is a scaled balance rather than a token amount,
so read its token value with the new
`getInsuranceFundRevenueReceivableTokenAmount`. Price insurance fund shares
with the new `getInsuranceFundNav`, which adds that value to the live vault
balance. Pricing off the vault balance alone understates the fund.

`calculateWithdrawLimit` reserves the receivable from the withdraw, exception
and borrow limits, so it no longer reports room the program refuses.

`removeInsuranceFundStake` takes the spot market vault account and settles
allocated revenue before it prices the payout. An unstake pays the cash share
of its claim while part of the fund is unsettled, and leaves the rest of the
request open, so one call no longer always closes a request.
