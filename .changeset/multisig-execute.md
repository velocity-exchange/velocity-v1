---
'@velocity-exchange/admin-cli': minor
---

Add `multisig execute` command: execute an approved vault transaction as a member with an explicit compute-unit limit (default 1.4M) and optional priority fee. The Squads UI executes at the 200k CU default, which CPI-heavy inner transactions such as Jupiter swaps exceed.
