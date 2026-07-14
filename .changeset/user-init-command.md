---
'@velocity-exchange/admin-cli': minor
---

Add `user init` command: initializes UserStats (if missing) and sequential named sub-accounts for a given authority (or a Squads vault PDA via `--multisig`/`--vault-index`). Permissionless, signer pays rent, idempotent across reruns.
