---
'@velocity-exchange/sdk': patch
---

Add the `InvalidRevenueShareRecipient` (6360) error to the IDL/types. Emitted by revenue-share settlement when a builder/referrer recipient `User` is not `sub_account_id == 0` — the canonical recipient of the stored authority — closing a hole where a permissionless settlement caller could redirect accrued rewards to any sibling subaccount. (Bundled with two program-only bankruptcy interest-refresh fixes that have no SDK surface.)
