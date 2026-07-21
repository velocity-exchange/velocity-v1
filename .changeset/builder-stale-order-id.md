---
'@velocity-exchange/sdk': patch
---

Builder revenue-share stale-order fix (High audit fix): a `RevenueShareOrder` written by `add_builder_order` before `place_perp_order` runs could be orphaned when placement soft-skipped on an expired `max_ts` (which returns before `next_order_id` is consumed), leaving the row keyed to an order id a later non-builder order reuses. Fill-time lookup matched only `(sub_account_id, order_id)` with no live `HasBuilder` check, letting a filler charge the stale builder fee on the reusing order. Fixed by (1) gating the fill-time builder-row lookup on the order's live `HasBuilder` flag and (2) clearing the builder-order row when placement bails before committing. Program-only change; no on-chain layout, IDL, or SDK API change (the SDK computes builder fees from explicit order params, not an order-id escrow lookup, so it never reproduced the issue).
