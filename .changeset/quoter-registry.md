---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

Quoter registry: `initialize_quoter` / `update_quoter_accounts` / `update_quoter_config` / `update_quoter_active` / `update_quoter_approved` / `update_quoter_priority` instructions and the `QuoterV0` account (incl. the admin-assigned `priority` routing field) added to the IDL, with `QuoterV0Account` / `QuoterType` / `QuoterCpiLeg` / `AmmAccountMeta` type mirrors. Admin CLI gains the `quoter` command group (`init`, `update-accounts`, `update-config`, `set-active`, `set-approved`, `set-priority`).
