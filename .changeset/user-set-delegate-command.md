---
'@velocity-exchange/admin-cli': minor
---

Add `user set-delegate` command: sets the delegate wallet on an authority's sub-accounts (optionally toggling the authority-wide `allowDelegateTransfer` flag), batched into a single Squads vault transaction proposal with `--multisig`. `sendOrPropose` now accepts a vault index so proposals can execute from vaults other than 0, and `user deposit`/`user withdraw` gain a `--vault-index` option.
