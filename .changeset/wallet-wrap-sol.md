---
'@velocity-exchange/admin-cli': minor
---

Add `wallet wrap-sol` command: wraps native SOL from the signer wallet or a Squads vault PDA into its wSOL ATA (ATA created idempotently, syncNative in the same transaction), with the usual `--multisig` proposal routing, `--dry-run`, and a `--min-remaining` guard so the wallet keeps SOL for rent and fees.
