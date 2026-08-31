---
'@velocity-exchange/admin-cli': minor
---

Add `multisig close-accounts` command: reclaim rent by closing the VaultTransaction + Proposal accounts of settled proposals (Executed/Rejected/Cancelled, plus stale non-approved ones; approved-but-unexecuted proposals are never touched). Requires the multisig's rent collector to be configured — rent is paid to it.
