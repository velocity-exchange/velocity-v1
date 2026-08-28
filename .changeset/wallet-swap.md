---
'@velocity-exchange/admin-cli': minor
---

Add `wallet swap` command: Jupiter swap from the signer wallet or a Squads vault PDA at any `--vault-index`, with `--slippage-bps`, `--only-direct-routes`, and `--dry-run`. Compute-budget instructions are stripped from the inner message (not CPI-able from the vault executor) and lookup tables are carried through proposal creation; direct sends with lookup tables go out as v0 transactions.
