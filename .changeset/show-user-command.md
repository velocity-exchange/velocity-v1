---
'@velocity-exchange/admin-cli': minor
---

Add `show user [authority]`. It prints the authority's UserStats and, for each sub-account, the delegate, status flags, collateral, health, leverage, spot balances and perp positions. With `--multisig`, `--vault-index` picks the vault PDA used as the authority.
