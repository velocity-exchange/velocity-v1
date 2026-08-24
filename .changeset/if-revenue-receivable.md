---
'@velocity-exchange/sdk': minor
'@velocity-exchange/vaults-sdk': patch
---

Preserve insurance fund revenue as a spot vault receivable.

`SpotMarketAccount.spotFeePool` is removed. Its slot now holds
`paddingFormerSpotFeePool: number[]` and
`insuranceFundRevenueReceivableScaled: BN`. Code that builds a
`SpotMarketAccount` literal must replace the field. Account size and every other
field offset are unchanged, so decoders keep working.

The receivable holds revenue that is allocated to the insurance fund but still
sits in the spot vault, which happens while a withdraw pause blocks the
transfer. It is a scaled balance rather than a token amount, so read its token
value with the new `getInsuranceFundRevenueReceivableTokenAmount`. Price insurance fund shares
with the new `getInsuranceFundNav`, which adds that value to the live vault
balance. Pricing off the vault balance alone understates the fund.

`calculateWithdrawLimit` reserves the receivable from the withdraw, exception
and borrow limits, so it no longer reports room the program refuses.

`removeInsuranceFundStake` takes the spot market vault account and settles
allocated revenue before it prices the payout, so the receivable is normally
empty by then. While a pause holds that revenue in the spot vault, an unstake
whose frozen value exceeds the cash on hand reverts with `InvalidIFUnstakeSize`
rather than paying part of it.
