---
'@velocity-exchange/sdk': patch
'@velocity-exchange/admin-cli': patch
---

`getUpdateHotAdminIx` accepts an optional `admin` authority override, and `auth set-hot-admin` passes the Squads vault PDA through it when `--multisig` is set. Previously the instruction always listed the local wallet as the admin signer, so proposing the rotation through a multisig failed (the vault was not a required signer of any instruction).
