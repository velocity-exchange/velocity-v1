---
'@velocity-exchange/admin-cli': minor
---

Add `multisig inspect` command: decode a vault transaction's inner instructions with account keys resolved through its lookup tables, and simulate its execution with a full compute budget, reporting the error or the compute units consumed. Before approval the simulation reports InvalidProposalStatus, which is the proposal gate rather than a broken transaction.
