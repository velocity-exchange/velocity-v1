---
'@velocity-exchange/admin-cli': minor
---

Add `user withdraw-batch <market>:<amount|max>...`. It proposes reduce-only withdrawals from several
spot markets as one Squads batch, with one approval round and one timelock, and one inner
transaction per market. `max` withdraws the whole deposit. The command checks that the vault's
destination ATAs exist before it proposes, and `--dry-run` sizes and simulates each inner
transaction as the vault.
