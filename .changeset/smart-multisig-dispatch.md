---
'@velocity-exchange/admin-cli': patch
---

`--multisig` now only creates a Squads proposal when the multisig's vault 0 PDA is actually a required signer of the instructions being dispatched. When the vault does not need to sign (e.g. the wallet itself is the required authority), the CLI prints a notice and sends the transaction directly instead of creating a pointless proposal.
