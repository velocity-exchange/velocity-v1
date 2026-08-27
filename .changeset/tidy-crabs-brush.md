---
'@velocity-exchange/admin-cli': patch
---

Fix the account-extension migration path against a live cluster: `auth set-hot-admin` no longer subscribes the client (it only needs the state PDA and the signer, and must work while zero-copy accounts are pre-extension size), and `extend-account` looks up coder account names in camelCase, matching anchor's Program-converted IDL (`perpMarket`, not `PerpMarket`).
