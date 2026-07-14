---
'@velocity-exchange/admin-cli': minor
---

Add `user init` command: initializes UserStats (if missing) and sequential named sub-accounts for a given authority (or a Squads vault PDA via `--multisig`/`--vault-index`). On mainnet the program requires the authority to sign creation, so with `--multisig` the instructions are batched into one vault transaction proposal with the vault as rent payer; otherwise the signer pays and the transaction is sent directly. Idempotent across reruns. Prints the planned accounts and expected rent up front; `--dry-run` stops there.
