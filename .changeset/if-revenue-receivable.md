---
'@velocity-exchange/sdk': minor
'@velocity-exchange/vaults-sdk': patch
---

Preserve insurance fund revenue as a spot vault receivable.

`SpotMarketAccount.spotFeePool` is removed. The retired slot now holds
`paddingFormerSpotFeePool: number[]` and
`insuranceFundRevenueReceivableScaled: BN`. Code that constructs a
`SpotMarketAccount` literal must replace the field; account size and every
other field offset are unchanged, so decoders keep working.

The receivable holds revenue that is allocated to the insurance fund but still
sits in the spot vault, which happens while a withdraw pause blocks the
transfer. It is a scaled balance, so read its token value with the new
`getInsuranceFundRevenueReceivableTokenAmount`. Price insurance fund shares
with the new `getInsuranceFundNav`, which adds that value to the live vault
balance. Pricing off the vault balance alone understates the fund.

`calculateWithdrawLimit` reserves the receivable from the withdraw, exception
and borrow limits, so it no longer reports room the program refuses.

`removeInsuranceFundStake` takes the spot market vault account and settles
allocated revenue before it prices the payout. An unstake pays the cash share
of its claim while part of the fund is unsettled, and leaves the rest of the
request open, so one call no longer always closes a request.
