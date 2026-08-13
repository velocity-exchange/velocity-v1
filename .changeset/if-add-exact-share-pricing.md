---
'@velocity-exchange/sdk': patch
'@velocity-exchange/vaults-sdk': patch
---

`addInsuranceFundStake`'s `amount` is now an upper bound rather than the staked amount. IF shares are
indivisible, so the program transfers only the portion of the request that prices to whole shares and
leaves the remainder — always less than one share price — in the token account.

This completes the fix for the zero-shares High finding. Rejecting only the zero-share case bounded
the loss instead of removing it: a request worth 1.5 shares minted 1 and donated the other half to
existing shareholders, and because the share price is set off a donation-inflatable vault balance, an
attacker could pick that fraction. Pricing the deposit exactly (shares floored, their cost ceiled, so
the fund never sells a share below price) caps the residual at one token unit and makes the donation
unprofitable.

`IFDepositMintsZeroShares` (6360) now means the request was below the price of a single share. Read
the staked amount from `InsuranceFundStakeRecord.amount` instead of assuming it equals the requested
amount; with `fromSubaccount`, any remainder lands in the wallet's token account rather than returning
to the sub-account.

`VaultClient.addToInsuranceFundStake` inherits the same rule with one difference: the vaults program
stakes the whole balance of the vault's IF token account, so a remainder from an earlier add is folded
in and the staked amount can exceed `amount`.
