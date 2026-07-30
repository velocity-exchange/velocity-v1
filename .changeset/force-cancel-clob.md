---
'@velocity-exchange/sdk': minor
---

New keeper instruction `force_cancel_clob_orders` in the IDL: the CLOB arm of the force-cancel flow — reclaims a failing account's risk-increasing book orders (and their placed-trigger shadows) via keeper-passed `OrderRef`s, with the same margin/equity-floor gates and flat fee as `force_cancel_orders`.
