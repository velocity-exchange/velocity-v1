---
'@velocity-exchange/admin-cli': minor
---

Add `perp-market withdraw-fee-pool-batch <market>:<amount>... --recipient <pubkey>`. It proposes
fee pool withdrawals from several perp markets as one Squads batch, with one approval round and one
timelock, and one inner transaction per market. `--dry-run` sizes and simulates each inner
transaction as the vault.
