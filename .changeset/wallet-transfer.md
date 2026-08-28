---
'@velocity-exchange/admin-cli': minor
---

Add `wallet transfer` command: SPL token transfer from the signer wallet or a Squads vault PDA to a recipient's associated token account (created idempotently), using transferChecked against the mint decimals read on chain, with a source-balance preflight and the usual multisig proposal routing.
