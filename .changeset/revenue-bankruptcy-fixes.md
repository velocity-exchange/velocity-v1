---
'@velocity-exchange/sdk': patch
---

Add the `InvalidRevenueShareRecipient` (6363) error to the IDL/types. Emitted by revenue-share settlement when a builder/referrer recipient `User` is not `sub_account_id == 0` — the canonical recipient of the stored authority — closing a hole where a permissionless settlement caller could redirect accrued rewards to any sibling subaccount. (Bundled with program-only accounting fixes that have no SDK type/IDL surface: two bankruptcy interest-refresh fixes; the `sweep_perp_market_fees` reserve now valued at the fixed `expiry_price` during market Settlement — `sweepPerpMarketFees`'s doc comment notes this; and the expiry-position closeout fee now accrues to the market fee ledger with the standard IF/protocol split.)
