---
'@velocity-exchange/admin-cli': minor
---

Add `multisig set-rent-collector` command: propose a Squads config transaction setting the multisig's rent collector, the prerequisite for `close-accounts` rent reclamation. Refuses multisigs governed by a config authority, and warns that executing a config transaction marks still-Active vault proposals stale.
