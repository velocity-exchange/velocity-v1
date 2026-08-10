---
'@velocity-exchange/sdk': patch
'@velocity-exchange/vaults-sdk': patch
---

`cancel_request_remove_insurance_fund_stake` now settles any already-due revenue into the insurance
fund vault before pricing the cancel's forfeiture (OtterSec #141). Previously a staker could order
their signed cancel ahead of an already-due signerless settle, make the restake price against a stale
vault, burn no shares, and keep revenue the anti-free-option rule assigns to the remaining stakers.

**ABI change — the account list is reordered, not just appended.** The instruction now takes `state`
(prepended), plus `spot_market_vault`, `velocity_signer` and `token_program`, matching
`request_remove_insurance_fund_stake`. Both SDKs pass them for you
(`VelocityClient.cancelRequestRemoveInsuranceFundStake`, `VaultClient.getCancelRequestRemoveInsuranceFundStakeIx`),
so SDK callers need no change; anyone building the instruction manually must rebuild the account list.
The `vaults` program's CPI wrapper gained the matching accounts.
