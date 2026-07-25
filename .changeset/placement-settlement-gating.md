---
'@velocity-exchange/sdk': patch
---

Order placement & settlement gating (four Medium audit fixes). Program-internal behavior changes; no instruction, account-layout, IDL, or error-code change (reuses existing error variants).

- **#84** — signed-message bundles are now atomic around the main order: `place_signed_msg_taker_order` pre-checks the main taker order's `max_ts` before placing anything and skips the whole bundle if it has already expired, so the reduce-only TP/SL sidecars (which are exempt from `max_ts` expiry) are no longer installed as standalone triggers when the main entry would soft-skip. The sidecars keep their existing order ids (the main keeps the trailing id clients rely on).
- **#85** — the signed-message taker path now rejects immediate-or-cancel orders (`InvalidOrderIOC`), matching the direct/batch place paths. A signed IOC limit order can no longer be stored as an indefinitely-resting order (limit orders default `max_ts` to 0).
- **#86** — `trigger_order` now enforces the same `is_in_settlement` gate the place/fill paths use, so a keeper can't trigger a dormant order (and collect the flat reward) on an expired/settling market.
- **#87** — `transfer_perp_position` now rejects transfers once the market is expired / in settlement, so an authority can't split a live-oracle gain from the matching fixed-`expiry_price` loss across two of its own subaccounts.
