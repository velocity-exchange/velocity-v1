---
'@velocity-exchange/sdk': patch
---

Builder-code fee integrity (two Medium audit fixes). Adds one error variant to the IDL; no account-layout change.

- **#82** — builder fees could be silently dropped. `add_builder_order` now propagates `RevenueShareEscrowOrdersAccountFull` instead of returning `Ok(None)` (which placed the order with no `HasBuilder` bit and charged no fee) when the escrow is full; `revoke_completed_orders` now decides whether an order is still open by matching `(sub_account_id, order_id)` across the whole order list rather than trusting the row's stored `user_order_index` (which can go stale and clear a still-open fee-bearing row early); and `modify_order` now rejects a builder-coded order (`CannotModifyBuilderOrder`, error 6366) rather than silently stripping attribution on the cancel-and-replace — cancel and re-place with builder params to change a builder order.
- **#83** — a self-approved builder could route up to ~65.5% of notional (a `u16::MAX` fee, with no global ceiling) out through a maintenance-margin-gated position-decreasing fill, moving value the taker couldn't withdraw under initial margin. A new global cap `MAX_BUILDER_FEE_TENTH_BPS` (1000 tenth-bps = 1%, tunable) bounds the actual builder fee charged, independent of the builder's own configured `max_fee_tenth_bps`.

Adds error `CannotModifyBuilderOrder` (6366) to the IDL. No instruction/account-layout change; error codes are read from the IDL (no manual `types.ts` mirror).
